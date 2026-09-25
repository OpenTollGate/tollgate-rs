//! Gating and shaping the kernel's own forwarding path.
//!
//! This is the adapter that carries somebody else's packets. Two mechanisms,
//! because access and rate are different questions:
//!
//! - **nftables** decides *whether* a packet is forwarded, and counts what was.
//!   Membership of a named set is the gate, so changing a peer's access is one
//!   set element rather than a rule rewrite. What goes in the set is core's
//!   [`AccessLevel::carried`], so a peer that is not paying is still forwarded
//!   at the minimum flow allowance, and dropped only when there is none.
//! - **`tc`** decides *how fast*. One HTB class per peer, whose rate is
//!   replaced as grants arrive.
//!
//! Both are driven by shelling out to `nft` and `tc`. That is deliberate: the
//! netlink crates would pull a large dependency and a lot of unsafe for
//! something invoked a handful of times per peer per second, and `nft -j`
//! already speaks JSON.
//!
//! # What is shaped, and what is not
//!
//! Only the peer's **download** — bytes we forward toward it. Its upload is
//! charged through the `received_multiplier`, which drains that peer's own
//! grant faster rather than capping its ingress, so a peer that pushes harder
//! exhausts its grant sooner and falls to the allowance. No ingress policer is
//! involved, and that is a design choice rather than an omission.
//!
//! And only traffic we **forward**, never traffic that terminates here. The
//! distinction is load-bearing rather than tidy. A peer whose grant has lapsed
//! falls to the minimum flow allowance, and the whole point of that allowance is
//! to leave it able to reach us and buy its way back up. Shaping by destination
//! address would put the control plane and the mint in the same class as the
//! bulk transit that just exhausted the grant, so a peer saturating its link
//! would starve the very messages that would have renewed it — a deadlock that
//! ends with the session dropped as stale.
//!
//! So the forward hook marks what it forwards and `tc` classifies on that mark.
//! Locally generated traffic never traverses that hook, is never marked, and
//! falls through to the qdisc's default: unshaped.
//!
//! # What is counted, and by what
//!
//! Depends on which side of the relationship the peer is on. A **customer** —
//! a peer whose traffic we forward — is the source or destination of every
//! packet it is owed, so it is counted by its IP. An **upstream** — a peer
//! that forwards our traffic onward — is neither: what it hands us carries
//! the far end's source address, and what we hand it carries the far end's
//! destination. Counted by its IP it would appear to carry almost nothing, and
//! the buyer, sizing purchases against what it observes, would under-buy.
//!
//! So an upstream is counted by the link, not the packet: what arrives from
//! its MAC (resolved from the neighbour table) is what it delivered to us, and
//! what the kernel routes via it as next hop is what we delivered to it.
//!
//! Which side a peer is on is read from the kernel rather than configured:
//! a peer is an upstream exactly when some route uses it as a gateway, which
//! is what "forwards our traffic onward" means to the kernel. Routes and
//! neighbour entries change — a MAC is not known until the first ARP exchange,
//! and an operator may reroute — so both are re-read periodically, and a peer
//! moves between the two ways of counting without its totals resetting: the
//! counters are named objects, and only which map points at them changes.
//!
//! # Requirements
//!
//! Linux, `CAP_NET_ADMIN`, and `net.ipv4.ip_forward=1`. Without the capability
//! every command fails and the node would gate nothing while believing it had,
//! so construction probes for it and refuses to start rather than pretending.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;
use tracing::{debug, info, warn};

use super::ResourceAdapter;

/// The nftables table everything lives in, so teardown is one command.
const TABLE: &str = "tollgate";

/// Burst allowed by a peer's class, in milliseconds of its rate.
///
/// Deliberately tiny, because a grant is a quantity as well as a rate. Burst is
/// permission to run ahead of the rate, and a peer that runs ahead spends the
/// quantity before the window that sized it has elapsed — so the grant lapses
/// early, the class collapses to the allowance with a full window's worth of
/// packets in flight, and the transfer it was carrying stalls for seconds
/// recovering. A quarter-second of burst was enough to do that on every few
/// windows.
///
/// It cannot be zero: HTB needs enough to pass one full-sized packet per timer
/// tick, and a class that cannot is one that never sends. Ten milliseconds
/// clears that on any plausible tick rate while staying far inside the lead a
/// buyer renews on.
const BURST_MS: u64 = 10;

/// High bits of the packet mark this adapter sets, with the peer's class in the
/// low bits.
///
/// The mark is a field the whole box shares, so a bare small integer would be
/// asking to collide with whatever else marks packets here. This is not a
/// reservation — nothing enforces one — but it is distinctive enough that a
/// collision is a deliberate choice rather than an accident.
const MARK_BASE: u32 = 0x7011_0000;

/// How often routes and neighbour entries are re-read.
///
/// Counters are sampled every tick, but which side a peer is on and what its
/// MAC is change on the scale of ARP and operator action, not packets. Two
/// processes every few seconds is cheap; two per tick would not be.
const REFRESH: Duration = Duration::from_secs(5);

/// The four maps a peer's counters are reached through.
///
/// Each maps a key the kernel sees on a packet to the name of a peer's
/// counter, so moving a peer between counting by IP and counting by link is
/// a change of map elements rather than of rules — and the counter itself,
/// and its total, is untouched by the move.
mod maps {
    /// Customer, by destination IP: delivered to it.
    pub const DOWN_TX: &str = "down_tx";
    /// Customer, by source IP: received from it.
    pub const DOWN_RX: &str = "down_rx";
    /// Upstream, by the route's next hop: delivered to it.
    pub const UP_TX: &str = "up_tx";
    /// Upstream, by the frame's source MAC: received from it.
    pub const UP_RX: &str = "up_rx";
}

/// One peer, as the kernel knows it.
#[derive(Debug, Clone)]
struct Peer {
    addr: IpAddr,
    /// The tc class minor number. Small, stable, and unique per peer.
    classid: u16,
    access: AccessLevel,
    rate: u64,
    /// Whether the address is in the `allowed` set right now.
    carried: bool,
    /// How its traffic is currently being counted.
    metering: Metering,
}

/// How a peer's traffic is matched to its counters.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Metering {
    /// A customer, or anything we cannot tell is an upstream: by its IP.
    Addr,
    /// An upstream: delivered by next hop, received by MAC if one is known.
    ///
    /// Without a MAC the receive side stays on the IP until the neighbour
    /// table has one, which undercounts exactly as before but no worse.
    Upstream { mac: Option<String> },
}

/// Gates with nftables, shapes with `tc`.
#[derive(Debug)]
pub struct Nftables {
    /// Interface facing the peers, where their classes live.
    interface: String,
    peers: Mutex<HashMap<PubKey, Peer>>,
    /// Hands out class minor numbers. Starts at 2 because 1 is the qdisc root.
    next_classid: Mutex<u16>,
    /// When routes and neighbours were last read. `None` until the first time.
    refreshed: Mutex<Option<Instant>>,
}

impl Nftables {
    /// Set up the table, the base chain and the root qdisc.
    ///
    /// Fails rather than degrading: an adapter that cannot install rules would
    /// forward everything while reporting that it was gating, which is worse
    /// than not starting.
    pub fn new(interface: &str) -> Result<Self> {
        let adapter = Self {
            interface: interface.to_string(),
            peers: Mutex::new(HashMap::new()),
            next_classid: Mutex::new(2),
            refreshed: Mutex::new(None),
        };
        adapter.install()?;
        Ok(adapter)
    }

    fn install(&self) -> Result<()> {
        // Start from a clean table so a restart does not inherit rules whose
        // peers are gone. Deleting a table that is not there is not an error
        // worth caring about.
        let _ = nft(&["delete", "table", "inet", TABLE]);

        nft(&["add", "table", "inet", TABLE]).context("create the nftables table")?;

        // Peers allowed to have traffic forwarded. Access control is membership
        // of this set, so a change is one element rather than a rule rewrite.
        // Two sets. `known` is every peer we have a session with; `allowed` is
        // the subset we will forward for. Splitting them is what lets one rule
        // gate every peer: traffic for an address we have never heard of is
        // none of our business, since this node may be forwarding other things
        // too, but a peer we know and have not allowed must be dropped.
        for set in ["known", "allowed"] {
            nft(&["add", "set", "inet", TABLE, set, "{ type ipv4_addr; }"])
                .with_context(|| format!("create the {set} set"))?;
        }

        nft(&[
            "add",
            "chain",
            "inet",
            TABLE,
            "forward",
            "{ type filter hook forward priority 0; policy accept; }",
        ])
        .context("create the forward chain")?;

        // One rule per direction, covering every peer for the life of the node.
        nft(&[
            "add", "rule", "inet", TABLE, "forward", "ip", "saddr", "@known", "ip", "saddr", "!=",
            "@allowed", "drop",
        ])
        .context("gate traffic from known peers")?;
        nft(&[
            "add", "rule", "inet", TABLE, "forward", "ip", "daddr", "@known", "ip", "daddr", "!=",
            "@allowed", "drop",
        ])
        .context("gate traffic toward known peers")?;

        // Counting, after the gate so a dropped packet is not counted as
        // delivered. One rule per map, covering every peer for the life of the
        // node; which peer a packet is counted against is a map lookup, and a
        // packet whose key is in no map is counted against nobody.
        for (name, key) in [
            (maps::DOWN_TX, "ipv4_addr"),
            (maps::DOWN_RX, "ipv4_addr"),
            (maps::UP_TX, "ipv4_addr"),
            (maps::UP_RX, "ether_addr"),
        ] {
            nft(&[
                "add",
                "map",
                "inet",
                TABLE,
                name,
                &format!("{{ type {key} : counter; }}"),
            ])
            .with_context(|| format!("create the {name} map"))?;
        }
        for rule in counting_rules() {
            let mut args = vec!["add", "rule", "inet", TABLE, "forward"];
            args.extend(rule.iter().copied());
            nft(&args).with_context(|| format!("add the rule {}", rule.join(" ")))?;
        }

        // The root qdisc every peer's class hangs off. `replace` so a restart
        // does not fail on one that already exists.
        //
        // The default class is deliberately one that is never created: HTB sends
        // traffic it cannot classify straight to the device, unshaped. That is
        // what carries this node's own packets — the control plane, the mint —
        // and anything else the box is doing, none of which any peer bought.
        tc(&[
            "qdisc",
            "replace",
            "dev",
            &self.interface,
            "root",
            "handle",
            "1:",
            "htb",
            "default",
            "9999",
        ])
        .context("install the root qdisc")?;

        Ok(())
    }

    /// Rule and class names derived from the peer, so they can be found again
    /// without keeping kernel handles around.
    fn counter_names(peer: PubKey) -> (String, String) {
        let tag = hex::encode(&peer.0[..6]);
        (format!("d{tag}"), format!("r{tag}"))
    }

    /// Re-read routes and neighbours if [`REFRESH`] has passed.
    fn refresh_if_due(&self) {
        {
            let mut last = self.refreshed.lock().expect("not poisoned");
            if last.is_some_and(|t| t.elapsed() < REFRESH) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.refresh();
    }

    /// Decide afresh how each peer is counted, and move the ones that changed.
    ///
    /// A failure to read or parse either table leaves every peer as it was:
    /// guessing would move an upstream back to counting by IP, which is the
    /// very undercount this exists to prevent. So does a failure to install a
    /// peer's new elements, so the next refresh tries the move again rather
    /// than believing it happened.
    fn refresh(&self) {
        let routes = ["-4", "-j", "route", "show", "table", "all"];
        let Some(gateways) = read_table("route", &routes, parse_gateways) else {
            return;
        };
        let neigh = ["-4", "-j", "neigh", "show"];
        let Some(neighbours) = read_table("neighbour", &neigh, parse_neighbours) else {
            return;
        };

        let mut peers = self.peers.lock().expect("not poisoned");
        for (peer, entry) in peers.iter_mut() {
            let next = classify(entry.addr, &gateways, &neighbours);
            if next == entry.metering {
                continue;
            }
            let (delivered, received) = Self::counter_names(*peer);
            let moved = apply_elements(
                &map_elements(entry.addr, &entry.metering, &delivered, &received),
                &map_elements(entry.addr, &next, &delivered, &received),
            );
            if !moved {
                continue;
            }
            info!(%peer, addr = %entry.addr, from = ?entry.metering, to = ?next, "counting the peer differently");
            entry.metering = next;
        }
    }
}

/// Read one of the kernel's tables with `ip`, or `None` if it could not be
/// read or did not parse.
fn read_table<T>(what: &str, args: &[&str], parse: fn(&str) -> Option<T>) -> Option<T> {
    match run("ip", args) {
        Ok(json) => {
            let parsed = parse(&json);
            if parsed.is_none() {
                warn!(table = what, "could not parse the kernel's table");
            }
            parsed
        }
        Err(e) => {
            warn!(table = what, error = %e, "could not read the kernel's table");
            None
        }
    }
}

/// The counting rules, one per map, in the forward chain.
///
/// All four live in the forward hook, so only traffic we forward is counted —
/// the same boundary the shaper draws. The link layer is still visible there
/// for a packet that arrived over Ethernet, and, unlike early prerouting, the
/// hook sits after connection tracking has undone any NAT: before that, a
/// reply to a masqueraded customer is still addressed to this node.
fn counting_rules() -> [Vec<&'static str>; 4] {
    [
        vec!["counter", "name", "ip", "daddr", "map", "@down_tx"],
        vec!["counter", "name", "ip", "saddr", "map", "@down_rx"],
        vec!["counter", "name", "rt", "ip", "nexthop", "map", "@up_tx"],
        vec!["counter", "name", "ether", "saddr", "map", "@up_rx"],
    ]
}

/// Which way a peer should be counted, given the kernel's tables.
///
/// An upstream is a peer some route uses as its gateway. Anything else —
/// including an IPv6 peer, which the maps do not cover — is counted by IP.
fn classify(
    addr: IpAddr,
    gateways: &HashSet<Ipv4Addr>,
    neighbours: &HashMap<Ipv4Addr, String>,
) -> Metering {
    let IpAddr::V4(v4) = addr else {
        return Metering::Addr;
    };
    if !gateways.contains(&v4) {
        return Metering::Addr;
    }
    Metering::Upstream {
        mac: neighbours.get(&v4).cloned(),
    }
}

/// One entry in a counting map: which map, the key, and the counter it names.
type Element = (&'static str, String, String);

/// The map elements that count a peer one way.
///
/// Comparable, so a change of metering is the difference between two lists.
fn map_elements(
    addr: IpAddr,
    metering: &Metering,
    delivered: &str,
    received: &str,
) -> Vec<Element> {
    let ip = addr.to_string();
    match metering {
        Metering::Addr => vec![
            (maps::DOWN_TX, ip.clone(), delivered.to_owned()),
            (maps::DOWN_RX, ip, received.to_owned()),
        ],
        Metering::Upstream { mac } => {
            // The next hop covers traffic addressed to the upstream itself as
            // well: for a destination on the link, the next hop is the
            // destination. So nothing to it is left to the per-IP map.
            let rx = match mac {
                Some(mac) => (maps::UP_RX, mac.clone(), received.to_owned()),
                None => (maps::DOWN_RX, ip.clone(), received.to_owned()),
            };
            vec![(maps::UP_TX, ip, delivered.to_owned()), rx]
        }
    }
}

/// Remove the elements in `old` but not `new`, then add those in `new` but not
/// `old`.
///
/// Removal first, because a counter briefly reached through neither map loses
/// a few packets, where one reached through both would count them twice.
///
/// Whether every new element went in. A failed removal does not count against
/// it: the usual cause is an element that is already gone, which is where it
/// was headed anyway.
fn apply_elements(old: &[Element], new: &[Element]) -> bool {
    for (map, key, _) in old.iter().filter(|e| !new.contains(e)) {
        if let Err(e) = nft(&[
            "delete",
            "element",
            "inet",
            TABLE,
            map,
            &format!("{{ {key} }}"),
        ]) {
            warn!(map, key, error = %e, "could not remove a counting element");
        }
    }
    let mut added = true;
    for (map, key, counter) in new.iter().filter(|e| !old.contains(e)) {
        if let Err(e) = nft(&["add", "element", "inet", TABLE, map, &element(key, counter)]) {
            warn!(map, key, error = %e, "could not add a counting element");
            added = false;
        }
    }
    added
}

/// A map element pointing a key at a named counter.
fn element(key: &str, counter: &str) -> String {
    format!("{{ {key} : \"{counter}\" }}")
}

/// Every IPv4 address some route uses as its gateway, from `ip -j route`.
///
/// A multipath route names its gateways under `nexthops` rather than at the
/// top level, and a multi-homed node is exactly the one likely to have one.
///
/// `None` if the output is not a JSON array: that is an `ip` that did not
/// understand the question, not a routing table with no gateways in it.
fn parse_gateways(json: &str) -> Option<HashSet<Ipv4Addr>> {
    let Ok(serde_json::Value::Array(routes)) = serde_json::from_str(json) else {
        return None;
    };
    let gateway = |v: &serde_json::Value| v.get("gateway")?.as_str()?.parse::<Ipv4Addr>().ok();
    let gateways = routes
        .iter()
        .flat_map(|route| {
            let hops = route
                .get("nexthops")
                .and_then(|n| n.as_array())
                .into_iter()
                .flatten()
                .filter_map(gateway);
            gateway(route).into_iter().chain(hops)
        })
        .collect();
    Some(gateways)
}

/// IPv4 neighbours with a usable link address, from `ip -j neigh`.
///
/// An entry that failed or is still being resolved has no address worth
/// counting by. A stale one does: stale means unconfirmed lately, not wrong,
/// and an upstream we mostly receive from goes stale while still sending.
///
/// `None` if the output is not a JSON array, as for [`parse_gateways`].
fn parse_neighbours(json: &str) -> Option<HashMap<Ipv4Addr, String>> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(json) else {
        return None;
    };
    let neighbours = entries
        .iter()
        .filter_map(|entry| {
            let failed = entry
                .get("state")
                .and_then(|s| s.as_array())
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str())
                .any(|s| matches!(s, "FAILED" | "INCOMPLETE"));
            if failed {
                return None;
            }
            let ip = entry.get("dst")?.as_str()?.parse::<Ipv4Addr>().ok()?;
            let mac = normalize_mac(entry.get("lladdr")?.as_str()?)?;
            Some((ip, mac))
        })
        .collect();
    Some(neighbours)
}

/// A MAC in the form nftables takes, or `None` if it is not one.
///
/// Checked rather than passed through, because it ends up in an `nft` command
/// and the kernel's idea of a link address includes things that are not six
/// octets — a tunnel's, for one.
fn normalize_mac(mac: &str) -> Option<String> {
    let octets: Vec<&str> = mac.split(':').collect();
    let valid = octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()));
    valid.then(|| mac.to_ascii_lowercase())
}

impl ResourceAdapter for Nftables {
    fn register(&self, peer: PubKey, addr: IpAddr) {
        let mut peers = self.peers.lock().expect("not poisoned");
        if peers.contains_key(&peer) {
            return;
        }

        let classid = {
            let mut next = self.next_classid.lock().expect("not poisoned");
            let id = *next;
            *next = next.saturating_add(1);
            id
        };

        let (delivered, received) = Self::counter_names(peer);
        let ip = addr.to_string();

        // Known from now on, so the two gate rules apply to it.
        let _ = nft(&[
            "add",
            "element",
            "inet",
            TABLE,
            "known",
            &format!("{{ {ip} }}"),
        ]);

        // Named counters, so reading them back is one JSON dump rather than a
        // walk over rule handles.
        for name in [&delivered, &received] {
            let _ = nft(&["add", "counter", "inet", TABLE, name]);
        }
        // Counted by IP until a refresh shows it is an upstream.
        let metering = Metering::Addr;
        apply_elements(&[], &map_elements(addr, &metering, &delivered, &received));

        // Marking is by the peer's own address whichever way it is counted: a
        // packet headed for this peer is exactly the packet its class should
        // shape. For an upstream that is traffic that terminates at it, not
        // transit we route through it — shaping that to what the upstream has
        // bought from us would cap our own customers' uploads at its grant.
        let mark = MARK_BASE | classid as u32;
        let _ = nft(&[
            "add",
            "rule",
            "inet",
            TABLE,
            "forward",
            "ip",
            "daddr",
            &ip,
            "meta",
            "mark",
            "set",
            &format!("{mark:#x}"),
        ]);

        // A class, and a filter that selects it by the mark set above. Created
        // once; only the rate is replaced afterwards.
        let class = format!("1:{classid}");
        let _ = tc(&[
            "class",
            "replace",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "classid",
            &class,
            "htb",
            "rate",
            "8bit",
        ]);
        let _ = tc(&[
            "filter",
            "replace",
            "dev",
            &self.interface,
            "protocol",
            "ip",
            "parent",
            "1:",
            "prio",
            "1",
            "handle",
            &format!("{mark:#x}"),
            "fw",
            "flowid",
            &class,
        ]);

        peers.insert(
            peer,
            Peer {
                addr,
                classid,
                access: AccessLevel::None,
                rate: 0,
                carried: false,
                metering,
            },
        );
        debug!(%peer, %addr, classid, "peer registered with the kernel");

        // A new peer may be an upstream already, so it should not wait out a
        // whole refresh interval being counted by the wrong key.
        drop(peers);
        *self.refreshed.lock().expect("not poisoned") = None;
    }

    fn set_access(&self, peer: PubKey, access: AccessLevel) {
        let mut peers = self.peers.lock().expect("not poisoned");
        let Some(entry) = peers.get_mut(&peer) else {
            return;
        };
        entry.access = access;
        gate(peer, entry);
    }

    fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let mut peers = self.peers.lock().expect("not poisoned");
        let Some(entry) = peers.get_mut(&peer) else {
            return;
        };
        entry.rate = rate;
        // The rate opens and closes the gate as much as the level does: an
        // unpaid peer is forwarded exactly when it has an allowance.
        gate(peer, entry);

        // tc wants bits per second, and refuses a rate of zero. The floor is
        // the minimum flow allowance, which core has already applied, so zero
        // here means there is no allowance and the gate above has already shut
        // the peer out — one byte per second is the closest honest thing to
        // "effectively nothing" for a class nothing reaches.
        let bits = rate.saturating_mul(8).max(8);
        let burst = rate.saturating_mul(BURST_MS) / 1_000;

        let class = format!("1:{}", entry.classid);
        if let Err(e) = tc(&[
            "class",
            "replace",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "classid",
            &class,
            "htb",
            "rate",
            &format!("{bits}bit"),
            "burst",
            &format!("{}b", burst.max(1_600)),
        ]) {
            warn!(%peer, rate, error = %e, "could not set the peer's rate");
        }
    }

    fn counters(&self, peer: PubKey) -> Counters {
        self.refresh_if_due();
        let (delivered, received) = Self::counter_names(peer);
        // One dump for both directions: the table is small, and two processes
        // per peer per tick would be the expensive part of metering.
        let all = read_counters();
        Counters {
            delivered: all.get(delivered.as_str()).copied().unwrap_or(0),
            received: all.get(received.as_str()).copied().unwrap_or(0),
        }
    }

    fn demand(&self, _peer: PubKey) -> u64 {
        // A forwarding node does not want traffic of its own: what it buys
        // upstream is driven by what its customers pull through it, which the
        // node computes from the meters rather than from an intention.
        0
    }

    fn set_demand(&self, _peer: PubKey, _rate: u64) {}

    fn shaping_rate(&self, peer: PubKey) -> u64 {
        self.peers
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|p| p.rate)
            .unwrap_or(0)
    }

    fn peers(&self) -> Vec<PubKey> {
        self.peers
            .lock()
            .expect("not poisoned")
            .keys()
            .copied()
            .collect()
    }

    fn remove(&self, peer: PubKey) {
        let Some(entry) = self.peers.lock().expect("not poisoned").remove(&peer) else {
            return;
        };
        // Out of the maps, so its counters stop counting and a MAC or address
        // reused by someone else is not counted against it.
        let (delivered, received) = Self::counter_names(peer);
        apply_elements(
            &map_elements(entry.addr, &entry.metering, &delivered, &received),
            &[],
        );
        let ip = entry.addr.to_string();
        for set in ["allowed", "known"] {
            let _ = nft(&[
                "delete",
                "element",
                "inet",
                TABLE,
                set,
                &format!("{{ {ip} }}"),
            ]);
        }
        // The filter first, then the class it points at: a class still selected
        // by a filter is in use, and the kernel refuses to delete it. Both go,
        // because a peering that ends and starts again gets a fresh classid —
        // so a filter left behind is a permanent one, and enough churn fills
        // the qdisc with filters selecting classes that no longer exist.
        let mark = MARK_BASE | entry.classid as u32;
        let _ = tc(&[
            "filter",
            "delete",
            "dev",
            &self.interface,
            "parent",
            "1:",
            "protocol",
            "ip",
            "prio",
            "1",
            "handle",
            &format!("{mark:#x}"),
            "fw",
        ]);
        let _ = tc(&[
            "class",
            "delete",
            "dev",
            &self.interface,
            "classid",
            &format!("1:{}", entry.classid),
        ]);
    }
}

impl Drop for Nftables {
    fn drop(&mut self) {
        // Leaving rules behind would silently gate traffic for a node that is
        // no longer running.
        let _ = nft(&["delete", "table", "inet", TABLE]);
        let _ = tc(&["qdisc", "delete", "dev", &self.interface, "root"]);
    }
}

/// Put a peer in or take it out of the `allowed` set, to match what core says
/// about its level and rate together.
///
/// Only acts on a change. nftables refuses to delete an element that is not
/// there, so re-applying the same verdict would log an error for having done
/// nothing.
fn gate(peer: PubKey, entry: &mut Peer) {
    let carried = entry.access.carried(entry.rate);
    if carried == entry.carried {
        return;
    }

    let element = format!("{{ {} }}", entry.addr);
    let result = if carried {
        nft(&["add", "element", "inet", TABLE, "allowed", &element])
    } else {
        nft(&["delete", "element", "inet", TABLE, "allowed", &element])
    };
    match result {
        Ok(_) => entry.carried = carried,
        Err(e) => {
            warn!(%peer, access = ?entry.access, rate = entry.rate, error = %e, "could not update the allowed set")
        }
    }
}

fn nft(args: &[&str]) -> Result<String> {
    run("nft", args)
}

fn tc(args: &[&str]) -> Result<String> {
    run("tc", args)
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run {program}"))?;

    if !output.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every named counter in the table, by name, with its byte total.
///
/// `list counters table` rather than a lookup per name: one process, and the
/// per-name form is not accepted by every nft build.
fn read_counters() -> HashMap<String, u64> {
    let Ok(json) = nft(&["-j", "list", "counters", "table", "inet", TABLE]) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json) else {
        return HashMap::new();
    };

    parsed["nftables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let counter = entry.get("counter")?;
            Some((
                counter["name"].as_str()?.to_owned(),
                counter["bytes"].as_u64()?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seed: u8) -> PubKey {
        let mut b = [seed; 33];
        b[0] = 0x02;
        PubKey(b)
    }

    #[test]
    fn counter_names_are_distinct_per_peer_and_per_direction() {
        // They are how a counter is found again without keeping kernel handles,
        // so a collision would silently merge two peers' traffic.
        let (a_in, a_out) = Nftables::counter_names(peer(1));
        let (b_in, b_out) = Nftables::counter_names(peer(2));

        assert_ne!(a_in, a_out, "the two directions must not share a counter");
        assert_ne!(a_in, b_in, "two peers must not share a counter");
        assert_ne!(a_out, b_out);
    }

    #[test]
    fn counter_names_are_valid_nftables_identifiers() {
        // nftables will not accept a name starting with a digit, which a raw
        // hex key often would.
        let (delivered, received) = Nftables::counter_names(peer(0xAB));
        for name in [delivered, received] {
            assert!(name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()));
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    // Shaped like `ip -4 -j route show table all` from iproute2: a default
    // route, an ECMP route across two gateways, and on-link routes that name
    // no gateway at all.
    const ROUTES: &str = r#"[
        {"type":"unicast","dst":"default","gateway":"192.168.1.1","dev":"eth0","protocol":"dhcp","flags":[]},
        {"dst":"10.9.0.0/16","protocol":"static","flags":[],"nexthops":[
            {"gateway":"192.168.1.2","dev":"eth0","weight":1,"flags":[]},
            {"gateway":"192.168.1.3","dev":"eth0","weight":1,"flags":[]}]},
        {"dst":"192.168.1.0/24","dev":"eth0","protocol":"kernel","scope":"link","prefsrc":"192.168.1.50","flags":[]},
        {"type":"local","dst":"192.168.1.50","table":"local","dev":"eth0","protocol":"kernel","scope":"host","prefsrc":"192.168.1.50","flags":[]}
    ]"#;

    // Shaped like `ip -4 -j neigh show`.
    const NEIGHBOURS: &str = r#"[
        {"dst":"192.168.1.1","dev":"eth0","lladdr":"52:54:00:AA:BB:CC","state":["REACHABLE"]},
        {"dst":"192.168.1.2","dev":"eth0","lladdr":"52:54:00:aa:bb:dd","state":["STALE"]},
        {"dst":"192.168.1.3","dev":"eth0","state":["FAILED"]},
        {"dst":"192.168.1.4","dev":"eth0","state":["INCOMPLETE"]},
        {"dst":"10.0.0.42","dev":"gre1","lladdr":"0.0.0.0","state":["PERMANENT"]}
    ]"#;

    #[test]
    fn gateways_include_every_hop_of_a_multipath_route() {
        // A multi-homed node is the one with several upstreams, and it is also
        // the one whose routes spread across them — missing the nexthops would
        // miss exactly the case per-MAC counting is for.
        let gateways = parse_gateways(ROUTES).unwrap();
        assert_eq!(
            gateways,
            HashSet::from([v4("192.168.1.1"), v4("192.168.1.2"), v4("192.168.1.3")])
        );
    }

    #[test]
    fn neighbours_keep_usable_macs_only() {
        let neighbours = parse_neighbours(NEIGHBOURS).unwrap();
        // Normalized, since it is compared against what was applied before.
        assert_eq!(neighbours[&v4("192.168.1.1")], "52:54:00:aa:bb:cc");
        // Stale is unconfirmed, not wrong.
        assert_eq!(neighbours[&v4("192.168.1.2")], "52:54:00:aa:bb:dd");
        // Failed, unresolved, and a tunnel's non-MAC link address are dropped.
        assert_eq!(neighbours.len(), 2);
    }

    #[test]
    fn garbage_from_ip_is_not_an_empty_table() {
        // An `ip` that did not understand `-j` must not read as "no gateways",
        // or every upstream would be moved back to counting by IP.
        assert_eq!(parse_gateways(""), None);
        assert_eq!(parse_gateways("{}"), None);
        assert_eq!(parse_neighbours("not json"), None);
        // An empty table is still a table.
        assert_eq!(parse_gateways("[]"), Some(HashSet::new()));
        assert_eq!(parse_neighbours("[]"), Some(HashMap::new()));
    }

    #[test]
    fn macs_are_checked_before_they_reach_nft() {
        assert_eq!(
            normalize_mac("52:54:00:AA:bb:cc").as_deref(),
            Some("52:54:00:aa:bb:cc")
        );
        for bad in [
            "",
            "0.0.0.0",
            "52:54:00:aa:bb",
            "52:54:00:aa:bb:cc:dd",
            "52:54:00:aa:bb:zz",
            "5:54:00:aa:bb:cc0",
        ] {
            assert_eq!(normalize_mac(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_peer_is_an_upstream_when_a_route_goes_through_it() {
        let gateways = parse_gateways(ROUTES).unwrap();
        let neighbours = parse_neighbours(NEIGHBOURS).unwrap();

        // A customer on the link: nothing is routed via it.
        assert_eq!(
            classify("192.168.1.77".parse().unwrap(), &gateways, &neighbours),
            Metering::Addr
        );
        // The default gateway, MAC known.
        assert_eq!(
            classify("192.168.1.1".parse().unwrap(), &gateways, &neighbours),
            Metering::Upstream {
                mac: Some("52:54:00:aa:bb:cc".into())
            }
        );
        // A gateway whose neighbour entry failed is still an upstream; it just
        // has no MAC to count by yet.
        assert_eq!(
            classify("192.168.1.3".parse().unwrap(), &gateways, &neighbours),
            Metering::Upstream { mac: None }
        );
        // IPv6 is not covered by the maps, so it stays on its address.
        assert_eq!(
            classify("fe80::1".parse().unwrap(), &gateways, &neighbours),
            Metering::Addr
        );
    }

    #[test]
    fn a_customer_is_counted_by_its_address_both_ways() {
        let addr: IpAddr = "10.0.0.42".parse().unwrap();
        assert_eq!(
            map_elements(addr, &Metering::Addr, "d1", "r1"),
            vec![
                (maps::DOWN_TX, "10.0.0.42".to_owned(), "d1".to_owned()),
                (maps::DOWN_RX, "10.0.0.42".to_owned(), "r1".to_owned()),
            ]
        );
    }

    #[test]
    fn an_upstream_is_counted_by_next_hop_and_mac() {
        let addr: IpAddr = "192.168.1.1".parse().unwrap();
        let upstream = Metering::Upstream {
            mac: Some("52:54:00:aa:bb:cc".into()),
        };
        let elements = map_elements(addr, &upstream, "d1", "r1");
        assert_eq!(
            elements,
            vec![
                (maps::UP_TX, "192.168.1.1".to_owned(), "d1".to_owned()),
                (maps::UP_RX, "52:54:00:aa:bb:cc".to_owned(), "r1".to_owned()),
            ]
        );
        // No per-IP element survives, or traffic addressed to the upstream
        // itself would be counted twice.
        assert!(
            elements
                .iter()
                .all(|(m, _, _)| *m != maps::DOWN_TX && *m != maps::DOWN_RX)
        );
    }

    #[test]
    fn an_upstream_without_a_mac_falls_back_to_its_address_for_receive() {
        let addr: IpAddr = "192.168.1.1".parse().unwrap();
        assert_eq!(
            map_elements(addr, &Metering::Upstream { mac: None }, "d1", "r1"),
            vec![
                (maps::UP_TX, "192.168.1.1".to_owned(), "d1".to_owned()),
                (maps::DOWN_RX, "192.168.1.1".to_owned(), "r1".to_owned()),
            ]
        );
    }

    #[test]
    fn a_mac_change_moves_only_the_receive_element() {
        // What `apply_elements` computes: the delivered side is keyed by the
        // IP, which did not change, so it must not be touched — deleting and
        // re-adding it would drop packets for nothing.
        let addr: IpAddr = "192.168.1.1".parse().unwrap();
        let old = map_elements(
            addr,
            &Metering::Upstream {
                mac: Some("52:54:00:aa:bb:cc".into()),
            },
            "d1",
            "r1",
        );
        let new = map_elements(
            addr,
            &Metering::Upstream {
                mac: Some("52:54:00:aa:bb:dd".into()),
            },
            "d1",
            "r1",
        );
        let removed: Vec<_> = old.iter().filter(|e| !new.contains(e)).collect();
        let added: Vec<_> = new.iter().filter(|e| !old.contains(e)).collect();
        assert_eq!(
            removed,
            vec![&(maps::UP_RX, "52:54:00:aa:bb:cc".to_owned(), "r1".to_owned())]
        );
        assert_eq!(
            added,
            vec![&(maps::UP_RX, "52:54:00:aa:bb:dd".to_owned(), "r1".to_owned())]
        );
    }

    #[test]
    fn rule_and_element_text_is_what_nft_takes() {
        assert_eq!(
            element("52:54:00:aa:bb:cc", "rabc"),
            r#"{ 52:54:00:aa:bb:cc : "rabc" }"#
        );
        let rules: Vec<String> = counting_rules().iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            rules,
            [
                "counter name ip daddr map @down_tx",
                "counter name ip saddr map @down_rx",
                "counter name rt ip nexthop map @up_tx",
                "counter name ether saddr map @up_rx",
            ]
        );
        // Every map a rule names is one an element can be put in.
        for map in [maps::DOWN_TX, maps::DOWN_RX, maps::UP_TX, maps::UP_RX] {
            assert!(rules.iter().any(|r| r.ends_with(&format!("@{map}"))));
        }
    }
}
