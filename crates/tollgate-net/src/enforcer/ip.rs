//! Gating and shaping the kernel's own forwarding path.
//!
//! This is the enforcer that carries somebody else's packets. Two mechanisms,
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
//! # A customer's IPv6
//!
//! A peer is registered by the IPv4 address its session comes from, but a
//! customer on the LAN is a device, and a dual-stack device sends much of its
//! traffic over IPv6 from addresses it picks itself — several at once, and
//! privacy addresses that change by the day. Keyed by the IPv4 address alone,
//! all of that would be forwarded unshaped and uncounted, whatever the peer
//! had paid.
//!
//! So a customer that is on the link is also tied to its **MAC**, read from
//! the IPv4 neighbour table, and through the MAC to the IPv6 addresses the
//! IPv6 neighbour table lists for it (a [`Link`]). Its IPv6 traffic is then
//! charged to the same grant as its IPv4:
//!
//! - **gated** by source MAC on the way out, and by destination address on
//!   the way in, through `known_mac` / `allowed_mac` and `known6` /
//!   `allowed6`, which follow the IPv4 `allowed` set;
//! - **shaped** by marking what is forwarded to its IPv6 addresses with its
//!   class (the `mark6` map);
//! - **counted** into the same two counters: delivered by destination IPv6
//!   (`down6_tx`), received by source MAC, IPv6 only (`down6_rx`) — its IPv4
//!   is already counted by address.
//!
//! No new interface is needed for it: the host registers the IPv4 address as
//! before, and the enforcer learns the rest from the kernel on the same
//! refresh that tells customers from upstreams. An address seen once stays
//! the peer's until another MAC claims it, so an idle privacy address that
//! drops out of the neighbour table is not left ungated.
//!
//! A MAC with more addresses than [`MAX_V6_PER_PEER`] fails closed: it stays
//! known, with the addresses it had, but none of its IPv6 is forwarded until
//! the count drops back. Keeping only some of them would leave the rest
//! ungated, unshaped and uncounted, and the device picks which exist.
//!
//! # Requirements
//!
//! Linux, `CAP_NET_ADMIN`, and `net.ipv4.ip_forward=1`. Without the capability
//! every command fails and the node would gate nothing while believing it had,
//! so construction probes for it and refuses to start rather than pretending.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::Command;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;
use tracing::{debug, info, warn};

use super::Enforcer;

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

/// High bits of the packet mark this enforcer sets, with the peer's class in the
/// low bits.
///
/// The mark is a field the whole box shares, so a bare small integer would be
/// asking to collide with whatever else marks packets here. This is not a
/// reservation — nothing enforces one — but it is distinctive enough that a
/// collision is a deliberate choice rather than an accident.
const MARK_BASE: u32 = 0x7011_0000;

/// The most IPv6 addresses one customer is tied to.
///
/// A device picks its own addresses, so one that invented them by the
/// thousand would otherwise grow the sets without bound. A phone uses a
/// handful: a stable one, a privacy one or two, per prefix.
///
/// Past it, the peer's IPv6 is withheld rather than truncated to some of its
/// addresses: see [`customer_link`].
const MAX_V6_PER_PEER: usize = 16;

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
    /// Customer, by destination IPv6: delivered to it.
    pub const DOWN6_TX: &str = "down6_tx";
    /// Customer, by the frame's source MAC, IPv6 only: received from it.
    pub const DOWN6_RX: &str = "down6_rx";
    /// Customer, by destination IPv6: the mark that selects its class.
    pub const MARK6: &str = "mark6";
    /// Customers' MACs and IPv6 addresses, and the subsets forwarded for.
    pub const KNOWN_MAC: &str = "known_mac";
    pub const ALLOWED_MAC: &str = "allowed_mac";
    pub const KNOWN6: &str = "known6";
    pub const ALLOWED6: &str = "allowed6";
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
    /// A customer on the link: its MAC and IPv6 addresses. `None` until the
    /// neighbour table has one, and always for an upstream.
    link: Option<Link>,
}

/// What ties a customer's IPv6 traffic to it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    mac: String,
    /// Global and unique-local only: link-local traffic is never forwarded.
    v6: BTreeSet<Ipv6Addr>,
    /// The neighbour table shows more than [`MAX_V6_PER_PEER`] addresses for
    /// the MAC. Its IPv6 is then gated shut whatever the peer has paid.
    overflowed: bool,
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
pub struct Ip {
    /// Interface facing the peers, where their classes live.
    interface: String,
    peers: Mutex<HashMap<PubKey, Peer>>,
    /// Hands out class minor numbers. Starts at 2 because 1 is the qdisc root.
    next_classid: Mutex<u16>,
    /// When routes and neighbours were last read. `None` until the first time.
    refreshed: Mutex<Option<Instant>>,
}

impl Ip {
    /// Set up the table, the base chain and the root qdisc.
    ///
    /// Fails rather than degrading: an enforcer that cannot install rules would
    /// forward everything while reporting that it was gating, which is worse
    /// than not starting.
    pub fn new(interface: &str) -> Result<Self> {
        let enforcer = Self {
            interface: interface.to_string(),
            peers: Mutex::new(HashMap::new()),
            next_classid: Mutex::new(2),
            refreshed: Mutex::new(None),
        };
        enforcer.install()?;
        Ok(enforcer)
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

        // The same gate for a customer's IPv6: out by its MAC, in by its
        // addresses. The MAC rule is IPv6 only, since IPv4 is gated above.
        for (set, key) in [
            (maps::KNOWN_MAC, "ether_addr"),
            (maps::ALLOWED_MAC, "ether_addr"),
            (maps::KNOWN6, "ipv6_addr"),
            (maps::ALLOWED6, "ipv6_addr"),
        ] {
            nft(&[
                "add",
                "set",
                "inet",
                TABLE,
                set,
                &format!("{{ type {key}; }}"),
            ])
            .with_context(|| format!("create the {set} set"))?;
        }
        for rule in ipv6_gate_rules() {
            let mut args = vec!["add", "rule", "inet", TABLE, "forward"];
            args.extend(rule.iter().copied());
            nft(&args).with_context(|| format!("add the rule {}", rule.join(" ")))?;
        }

        // Counting, after the gate so a dropped packet is not counted as
        // delivered. One rule per map, covering every peer for the life of the
        // node; which peer a packet is counted against is a map lookup, and a
        // packet whose key is in no map is counted against nobody.
        for (name, key) in [
            (maps::DOWN_TX, "ipv4_addr"),
            (maps::DOWN_RX, "ipv4_addr"),
            (maps::UP_TX, "ipv4_addr"),
            (maps::UP_RX, "ether_addr"),
            (maps::DOWN6_TX, "ipv6_addr"),
            (maps::DOWN6_RX, "ether_addr"),
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

        // Marking a customer's IPv6 for its class. A map rather than a rule
        // per peer, because a peer's addresses come and go; a packet to an
        // address in no peer's entry is left unmarked.
        nft(&[
            "add",
            "map",
            "inet",
            TABLE,
            maps::MARK6,
            "{ type ipv6_addr : mark; }",
        ])
        .context("create the mark6 map")?;
        nft(&[
            "add", "rule", "inet", TABLE, "forward", "meta", "mark", "set", "ip6", "daddr", "map",
            "@mark6",
        ])
        .context("mark customers' IPv6")?;

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
        let Some(neighbours) = read_table("neighbour", &neigh, parse_neighbours::<Ipv4Addr>) else {
            return;
        };
        // A node without IPv6 has no IPv6 neighbours; that is not a reason
        // to stop telling customers from upstreams.
        let neigh6 = ["-6", "-j", "neigh", "show"];
        let neighbours6 =
            read_table("neighbour", &neigh6, parse_neighbours::<Ipv6Addr>).unwrap_or_default();

        let mut peers = self.peers.lock().expect("not poisoned");
        // Every peer's new state first, then every removal, then every
        // addition. The maps are shared, so an address moving from one peer
        // to another is one peer's removal and another's addition: applied
        // peer by peer, the addition could run first, collide with the
        // element still there, and fail — and the removal then delete it.
        let next: Vec<(PubKey, Metering, Option<Link>, Move)> = peers
            .iter()
            .map(|(peer, entry)| {
                let metering = classify(entry.addr, &gateways, &neighbours);
                let link = customer_link(
                    entry.addr,
                    &metering,
                    &neighbours,
                    &neighbours6,
                    entry.link.as_ref(),
                );
                let (delivered, received) = Self::counter_names(*peer);
                let mark = MARK_BASE | entry.classid as u32;
                let elements = |metering: &Metering, link: Option<&Link>| {
                    [
                        map_elements(entry.addr, metering, &delivered, &received),
                        link_elements(link, entry.carried, &delivered, &received, mark),
                    ]
                    .concat()
                };
                let change = Move {
                    before: elements(&entry.metering, entry.link.as_ref()),
                    after: elements(&metering, link.as_ref()),
                };
                (*peer, metering, link, change)
            })
            .collect();
        let moves: Vec<Move> = next.iter().map(|(.., m)| m.clone()).collect();
        let (removals, additions) = plan_moves(&moves);
        remove_elements(&removals);

        for ((peer, metering, link, _), added) in next.into_iter().zip(additions) {
            let entry = peers.get_mut(&peer).expect("planned from this map");
            if metering == entry.metering && link == entry.link {
                continue;
            }
            // A failed addition leaves the peer as it was, so the next
            // refresh tries the move again.
            if !add_elements(&added) {
                continue;
            }
            if metering != entry.metering {
                info!(%peer, addr = %entry.addr, from = ?entry.metering, to = ?metering, "counting the peer differently");
            }
            let was_over = entry.link.as_ref().is_some_and(|l| l.overflowed);
            let is_over = link.as_ref().is_some_and(|l| l.overflowed);
            if is_over && !was_over {
                warn!(
                    %peer,
                    addr = %entry.addr,
                    mac = link.as_ref().map(|l| l.mac.as_str()),
                    max = MAX_V6_PER_PEER,
                    "the peer's MAC has more IPv6 addresses than it may; its IPv6 is not forwarded until that drops"
                );
            } else if was_over && !is_over {
                info!(%peer, addr = %entry.addr, "the peer's MAC is back within its IPv6 addresses");
            }
            if link != entry.link {
                info!(%peer, addr = %entry.addr, link = ?link, "the peer's IPv6 follows its grant");
            }
            entry.metering = metering;
            entry.link = link;
        }
    }
}

/// One peer's elements before and after a refresh.
#[derive(Debug, Clone)]
struct Move {
    before: Vec<Element>,
    after: Vec<Element>,
}

/// What a refresh removes, across every peer, and what each peer then adds.
///
/// All removals go before any addition, so a key moving between peers is
/// free by the time its new owner adds it. An element some peer's new state
/// holds exactly — a MAC or address in one of the `known` sets, say — is not
/// removed at all: that would open a gap in the gate for nothing, and adding
/// an element that is already there is not an error.
fn plan_moves(moves: &[Move]) -> (Vec<Element>, Vec<Vec<Element>>) {
    let claimed: HashSet<&Element> = moves.iter().flat_map(|m| &m.after).collect();
    let mut removals: Vec<Element> = Vec::new();
    for m in moves {
        for e in &m.before {
            if !m.after.contains(e) && !claimed.contains(e) && !removals.contains(e) {
                removals.push(e.clone());
            }
        }
    }
    let additions = moves
        .iter()
        .map(|m| {
            m.after
                .iter()
                .filter(|e| !m.before.contains(e))
                .cloned()
                .collect()
        })
        .collect();
    (removals, additions)
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

/// The IPv6 gate: a customer's traffic out by its MAC, in by its addresses.
///
/// The MAC rule is limited to IPv6 because the peer's IPv4 is gated by
/// address already, and an upstream's frames carry a MAC too.
fn ipv6_gate_rules() -> [Vec<&'static str>; 2] {
    [
        vec![
            "meta",
            "nfproto",
            "ipv6",
            "ether",
            "saddr",
            "@known_mac",
            "ether",
            "saddr",
            "!=",
            "@allowed_mac",
            "drop",
        ],
        vec![
            "ip6",
            "daddr",
            "@known6",
            "ip6",
            "daddr",
            "!=",
            "@allowed6",
            "drop",
        ],
    ]
}

/// The counting rules, one per map, in the forward chain.
///
/// All four live in the forward hook, so only traffic we forward is counted —
/// the same boundary the shaper draws. The link layer is still visible there
/// for a packet that arrived over Ethernet, and, unlike early prerouting, the
/// hook sits after connection tracking has undone any NAT: before that, a
/// reply to a masqueraded customer is still addressed to this node.
///
/// The two IPv6 rules count a customer's IPv6 into the same counters: by
/// destination address, and by source MAC for IPv6 alone, since its IPv4 is
/// counted by address.
fn counting_rules() -> [Vec<&'static str>; 6] {
    [
        vec!["counter", "name", "ip", "daddr", "map", "@down_tx"],
        vec!["counter", "name", "ip", "saddr", "map", "@down_rx"],
        vec!["counter", "name", "rt", "ip", "nexthop", "map", "@up_tx"],
        vec!["counter", "name", "ether", "saddr", "map", "@up_rx"],
        vec!["counter", "name", "ip6", "daddr", "map", "@down6_tx"],
        vec![
            "meta",
            "nfproto",
            "ipv6",
            "counter",
            "name",
            "ether",
            "saddr",
            "map",
            "@down6_rx",
        ],
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

/// A customer's MAC and IPv6 addresses, from the neighbour tables.
///
/// Only for a customer whose IPv4 address has a MAC: an upstream's MAC is the
/// source of everything it forwards to us, and a peer that is not on the link
/// has no MAC we could see. An address the peer had stays its own until
/// another MAC claims it, so an idle privacy address is not dropped just
/// because its neighbour entry was collected. A MAC that is gone from the
/// table keeps the link it had; a different MAC starts a new one.
///
/// More than [`MAX_V6_PER_PEER`] addresses in the table for the MAC fails
/// closed: the link is marked overflowed, which keeps the MAC and its
/// addresses known but none of them allowed. Which addresses it keeps are the
/// ones it had, before any new one, so a flood of fresh addresses cannot push
/// the real ones out. It clears once the table shows few enough again.
///
/// The table is the device's to fill — any host can create a neighbour entry
/// with a Neighbour Solicitation — and truncating instead would let it pick
/// which of its addresses fall outside the gate, class and counters. That
/// includes another customer's: the MAC an entry records is the one in the
/// solicitation's link-layer option, not the frame's. Failing closed turns
/// that from free, unmetered IPv6 into a denial of the victim's IPv6 while
/// the flood lasts; its IPv4, gated by address, is untouched.
fn customer_link(
    addr: IpAddr,
    metering: &Metering,
    neighbours: &HashMap<Ipv4Addr, String>,
    neighbours6: &HashMap<Ipv6Addr, String>,
    previous: Option<&Link>,
) -> Option<Link> {
    let IpAddr::V4(v4) = addr else {
        return None;
    };
    if *metering != Metering::Addr {
        return None;
    }
    let Some(mac) = neighbours.get(&v4) else {
        return previous.cloned();
    };
    let kept: Vec<Ipv6Addr> = previous
        .filter(|link| link.mac == *mac)
        .into_iter()
        .flat_map(|link| link.v6.iter())
        .filter(|a| neighbours6.get(a).is_none_or(|m| m == mac))
        .copied()
        .collect();
    let seen: BTreeSet<Ipv6Addr> = neighbours6
        .iter()
        .filter(|(a, m)| *m == mac && forwardable(a))
        .map(|(a, _)| *a)
        .collect();
    let overflowed = seen.len() > MAX_V6_PER_PEER;
    // Within the cap, everything the table shows now, then what the peer had.
    // Over it, what the peer had, then new ones: nothing is allowed either
    // way, and the addresses it was using stay gated.
    let (first, then): (Vec<Ipv6Addr>, Vec<Ipv6Addr>) = if overflowed {
        (kept, seen.into_iter().collect())
    } else {
        (seen.into_iter().collect(), kept)
    };
    let mut v6 = BTreeSet::new();
    for a in first.into_iter().chain(then) {
        if v6.len() >= MAX_V6_PER_PEER {
            break;
        }
        v6.insert(a);
    }
    Some(Link {
        mac: mac.clone(),
        v6,
        overflowed,
    })
}

/// An IPv6 address traffic could be forwarded to or from: not link-local,
/// multicast, loopback or unspecified.
fn forwardable(a: &Ipv6Addr) -> bool {
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_multicast()
        || (a.segments()[0] & 0xffc0) == 0xfe80)
}

/// What a map or set element holds besides its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Value {
    /// A set member: the key alone.
    Member,
    /// A named counter.
    Counter(String),
    /// A packet mark.
    Mark(u32),
}

/// One entry in a map or set: which one, the key, and what it holds.
type Element = (&'static str, String, Value);

/// The elements that tie a customer's IPv6 to its gate, class and counters.
///
/// `carried` is whether the peer is forwarded for, which the `allowed` sets
/// follow exactly as the IPv4 one does — unless the link has overflowed, when
/// they hold none of it.
fn link_elements(
    link: Option<&Link>,
    carried: bool,
    delivered: &str,
    received: &str,
    mark: u32,
) -> Vec<Element> {
    let Some(link) = link else {
        return Vec::new();
    };
    let carried = carried && !link.overflowed;
    let mut out = vec![
        (maps::KNOWN_MAC, link.mac.clone(), Value::Member),
        (
            maps::DOWN6_RX,
            link.mac.clone(),
            Value::Counter(received.to_owned()),
        ),
    ];
    if carried {
        out.push((maps::ALLOWED_MAC, link.mac.clone(), Value::Member));
    }
    for a in &link.v6 {
        let key = a.to_string();
        out.push((maps::KNOWN6, key.clone(), Value::Member));
        if carried {
            out.push((maps::ALLOWED6, key.clone(), Value::Member));
        }
        out.push((
            maps::DOWN6_TX,
            key.clone(),
            Value::Counter(delivered.to_owned()),
        ));
        out.push((maps::MARK6, key, Value::Mark(mark)));
    }
    out
}

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
    let delivered = Value::Counter(delivered.to_owned());
    let received = Value::Counter(received.to_owned());
    match metering {
        Metering::Addr => vec![
            (maps::DOWN_TX, ip.clone(), delivered),
            (maps::DOWN_RX, ip, received),
        ],
        Metering::Upstream { mac } => {
            // The next hop covers traffic addressed to the upstream itself as
            // well: for a destination on the link, the next hop is the
            // destination. So nothing to it is left to the per-IP map.
            let rx = match mac {
                Some(mac) => (maps::UP_RX, mac.clone(), received),
                None => (maps::DOWN_RX, ip.clone(), received),
            };
            vec![(maps::UP_TX, ip, delivered), rx]
        }
    }
}

/// Remove the elements in `old` but not `new`, then add those in `new` but not
/// `old`.
///
/// Removal first, because a counter briefly reached through neither map loses
/// a few packets, where one reached through both would count them twice.
///
/// Whether every new element went in, and every old one came out. Callers
/// that only move counters look at `added` alone: a failed removal's usual
/// cause is an element that is already gone, which is where it was headed.
fn apply_elements(old: &[Element], new: &[Element]) -> Applied {
    let gone: Vec<Element> = old.iter().filter(|e| !new.contains(e)).cloned().collect();
    let fresh: Vec<Element> = new.iter().filter(|e| !old.contains(e)).cloned().collect();
    let removed = remove_elements(&gone);
    let added = add_elements(&fresh);
    Applied { added, removed }
}

/// Delete each element, logging any that would not go. Whether all did.
fn remove_elements(elements: &[Element]) -> bool {
    let mut all = true;
    for (map, key, _) in elements {
        if let Err(e) = nft(&[
            "delete",
            "element",
            "inet",
            TABLE,
            map,
            &format!("{{ {key} }}"),
        ]) {
            warn!(map, key, error = %e, "could not remove an element");
            all = false;
        }
    }
    all
}

/// Add each element, logging any that would not go in. Whether all did.
fn add_elements(elements: &[Element]) -> bool {
    let mut all = true;
    for (map, key, value) in elements {
        if let Err(e) = nft(&["add", "element", "inet", TABLE, map, &element(key, value)]) {
            warn!(map, key, error = %e, "could not add an element");
            all = false;
        }
    }
    all
}

/// What [`apply_elements`] managed.
#[derive(Debug, Clone, Copy)]
struct Applied {
    added: bool,
    removed: bool,
}

/// An element as `nft add element` takes it.
fn element(key: &str, value: &Value) -> String {
    match value {
        Value::Member => format!("{{ {key} }}"),
        Value::Counter(counter) => format!("{{ {key} : \"{counter}\" }}"),
        Value::Mark(mark) => format!("{{ {key} : {mark:#x} }}"),
    }
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

/// Neighbours with a usable link address, from `ip -4 -j neigh` or
/// `ip -6 -j neigh` — whichever family `A` is; entries of the other are
/// skipped.
///
/// An entry that failed or is still being resolved has no address worth
/// counting by. A stale one does: stale means unconfirmed lately, not wrong,
/// and an upstream we mostly receive from goes stale while still sending.
///
/// `None` if the output is not a JSON array, as for [`parse_gateways`].
fn parse_neighbours<A: FromStr + Eq + Hash>(json: &str) -> Option<HashMap<A, String>> {
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
            let ip = entry.get("dst")?.as_str()?.parse::<A>().ok()?;
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

impl Enforcer for Ip {
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
        // The same class for its IPv6, marked through `mark6`. A filter is
        // per protocol, so this one has a priority of its own.
        let _ = tc(&[
            "filter",
            "replace",
            "dev",
            &self.interface,
            "protocol",
            "ipv6",
            "parent",
            "1:",
            "prio",
            "2",
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
                link: None,
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
        let mark = MARK_BASE | entry.classid as u32;
        apply_elements(
            &[
                map_elements(entry.addr, &entry.metering, &delivered, &received),
                link_elements(
                    entry.link.as_ref(),
                    entry.carried,
                    &delivered,
                    &received,
                    mark,
                ),
            ]
            .concat(),
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
        for (protocol, prio) in [("ip", "1"), ("ipv6", "2")] {
            let _ = tc(&[
                "filter",
                "delete",
                "dev",
                &self.interface,
                "parent",
                "1:",
                "protocol",
                protocol,
                "prio",
                prio,
                "handle",
                &format!("{mark:#x}"),
                "fw",
            ]);
        }
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

impl Drop for Ip {
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
        Ok(_) => {
            // Its IPv6, if it has a link, follows. A failure there is logged
            // and not retried off this verdict: the IPv4 set is what the
            // verdict is recorded against, and the next change moves both.
            let (delivered, received) = Ip::counter_names(peer);
            let mark = MARK_BASE | entry.classid as u32;
            let link = entry.link.as_ref();
            let applied = apply_elements(
                &link_elements(link, entry.carried, &delivered, &received, mark),
                &link_elements(link, carried, &delivered, &received, mark),
            );
            if !(applied.added && applied.removed) {
                warn!(%peer, carried, "could not update the peer's IPv6 gate");
            }
            entry.carried = carried;
        }
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
        let (a_in, a_out) = Ip::counter_names(peer(1));
        let (b_in, b_out) = Ip::counter_names(peer(2));

        assert_ne!(a_in, a_out, "the two directions must not share a counter");
        assert_ne!(a_in, b_in, "two peers must not share a counter");
        assert_ne!(a_out, b_out);
    }

    #[test]
    fn counter_names_are_valid_nftables_identifiers() {
        // nftables will not accept a name starting with a digit, which a raw
        // hex key often would.
        let (delivered, received) = Ip::counter_names(peer(0xAB));
        for name in [delivered, received] {
            assert!(name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()));
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn v6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    /// A named counter, as an element holds it.
    fn c(name: &str) -> Value {
        Value::Counter(name.to_owned())
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
        let neighbours = parse_neighbours::<Ipv4Addr>(NEIGHBOURS).unwrap();
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
        assert_eq!(parse_neighbours::<Ipv4Addr>("not json"), None);
        // An empty table is still a table.
        assert_eq!(parse_gateways("[]"), Some(HashSet::new()));
        assert_eq!(parse_neighbours::<Ipv4Addr>("[]"), Some(HashMap::new()));
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
        let neighbours = parse_neighbours::<Ipv4Addr>(NEIGHBOURS).unwrap();

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
                (maps::DOWN_TX, "10.0.0.42".to_owned(), c("d1")),
                (maps::DOWN_RX, "10.0.0.42".to_owned(), c("r1")),
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
                (maps::UP_TX, "192.168.1.1".to_owned(), c("d1")),
                (maps::UP_RX, "52:54:00:aa:bb:cc".to_owned(), c("r1")),
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
                (maps::UP_TX, "192.168.1.1".to_owned(), c("d1")),
                (maps::DOWN_RX, "192.168.1.1".to_owned(), c("r1")),
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
            vec![&(maps::UP_RX, "52:54:00:aa:bb:cc".to_owned(), c("r1"))]
        );
        assert_eq!(
            added,
            vec![&(maps::UP_RX, "52:54:00:aa:bb:dd".to_owned(), c("r1"))]
        );
    }

    #[test]
    fn rule_and_element_text_is_what_nft_takes() {
        assert_eq!(
            element("52:54:00:aa:bb:cc", &c("rabc")),
            r#"{ 52:54:00:aa:bb:cc : "rabc" }"#
        );
        assert_eq!(
            element("2001:db8::23", &Value::Mark(0x7011_0002)),
            "{ 2001:db8::23 : 0x70110002 }"
        );
        assert_eq!(
            element("52:54:00:aa:bb:cc", &Value::Member),
            "{ 52:54:00:aa:bb:cc }"
        );
        let rules: Vec<String> = counting_rules().iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            rules,
            [
                "counter name ip daddr map @down_tx",
                "counter name ip saddr map @down_rx",
                "counter name rt ip nexthop map @up_tx",
                "counter name ether saddr map @up_rx",
                "counter name ip6 daddr map @down6_tx",
                "meta nfproto ipv6 counter name ether saddr map @down6_rx",
            ]
        );
        // Every map a rule names is one an element can be put in.
        for map in [
            maps::DOWN_TX,
            maps::DOWN_RX,
            maps::UP_TX,
            maps::UP_RX,
            maps::DOWN6_TX,
            maps::DOWN6_RX,
        ] {
            assert!(rules.iter().any(|r| r.ends_with(&format!("@{map}"))));
        }
    }

    #[test]
    fn the_ipv6_gate_is_by_mac_out_and_by_address_in() {
        let rules: Vec<String> = ipv6_gate_rules().iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            rules,
            [
                // IPv6 only: the peer's IPv4 is gated by address already.
                "meta nfproto ipv6 ether saddr @known_mac ether saddr != @allowed_mac drop",
                "ip6 daddr @known6 ip6 daddr != @allowed6 drop",
            ]
        );
    }

    // Shaped like `ip -6 -j neigh show`: a phone's stable and privacy global
    // addresses, a ULA, its link-local, another device, and a failed entry.
    const NEIGHBOURS6: &str = r#"[
        {"dst":"2001:db8:1::a","dev":"br-lan","lladdr":"AA:BB:CC:00:00:23","state":["REACHABLE"]},
        {"dst":"2001:db8:1::5eed","dev":"br-lan","lladdr":"aa:bb:cc:00:00:23","state":["STALE"]},
        {"dst":"fd00:1::23","dev":"br-lan","lladdr":"aa:bb:cc:00:00:23","state":["DELAY"]},
        {"dst":"fe80::a8bb:ccff:fe00:23","dev":"br-lan","lladdr":"aa:bb:cc:00:00:23","state":["STALE"]},
        {"dst":"2001:db8:1::77","dev":"br-lan","lladdr":"aa:bb:cc:00:00:77","state":["REACHABLE"]},
        {"dst":"2001:db8:1::99","dev":"br-lan","state":["FAILED"]}
    ]"#;

    const PHONE_MAC: &str = "aa:bb:cc:00:00:23";

    fn phone_neighbours() -> HashMap<Ipv4Addr, String> {
        HashMap::from([(v4("192.168.1.23"), PHONE_MAC.to_owned())])
    }

    #[test]
    fn ipv6_neighbours_parse_with_the_same_rules() {
        let n = parse_neighbours::<Ipv6Addr>(NEIGHBOURS6).unwrap();
        assert_eq!(n[&v6("2001:db8:1::a")], PHONE_MAC, "normalized");
        assert_eq!(n.len(), 5, "the failed entry is dropped");
        // A table of the other family parses to nothing, not to garbage.
        assert!(parse_neighbours::<Ipv6Addr>(NEIGHBOURS).unwrap().is_empty());
    }

    #[test]
    fn a_customer_on_the_link_is_tied_to_its_mac_and_forwardable_ipv6() {
        let n6 = parse_neighbours::<Ipv6Addr>(NEIGHBOURS6).unwrap();
        let link = customer_link(
            "192.168.1.23".parse().unwrap(),
            &Metering::Addr,
            &phone_neighbours(),
            &n6,
            None,
        )
        .unwrap();
        assert_eq!(link.mac, PHONE_MAC);
        // Link-local is never forwarded; the other device's address is not
        // the phone's.
        assert_eq!(
            link.v6,
            BTreeSet::from([
                v6("2001:db8:1::5eed"),
                v6("2001:db8:1::a"),
                v6("fd00:1::23")
            ])
        );
    }

    #[test]
    fn upstreams_and_strangers_get_no_link() {
        let n6 = parse_neighbours::<Ipv6Addr>(NEIGHBOURS6).unwrap();
        let addr: IpAddr = "192.168.1.23".parse().unwrap();
        // An upstream's MAC is the source of everything it forwards.
        let upstream = Metering::Upstream {
            mac: Some(PHONE_MAC.into()),
        };
        assert_eq!(
            customer_link(addr, &upstream, &phone_neighbours(), &n6, None),
            None
        );
        // Not on the link: no MAC to tie anything to.
        assert_eq!(
            customer_link(addr, &Metering::Addr, &HashMap::new(), &n6, None),
            None
        );
        // An IPv6 peer is its own address already.
        assert_eq!(
            customer_link(
                "2001:db8::1".parse().unwrap(),
                &Metering::Addr,
                &phone_neighbours(),
                &n6,
                None
            ),
            None
        );
    }

    #[test]
    fn an_idle_privacy_address_stays_until_another_mac_claims_it() {
        let addr: IpAddr = "192.168.1.23".parse().unwrap();
        let before = Link {
            mac: PHONE_MAC.into(),
            v6: BTreeSet::from([v6("2001:db8:1::aa01"), v6("2001:db8:1::77")]),
            overflowed: false,
        };
        // `::aa01` fell out of the table; `::77` now belongs to another MAC;
        // `::bb02` is the day's fresh privacy address.
        let n6 = HashMap::from([
            (v6("2001:db8:1::77"), "aa:bb:cc:00:00:77".to_owned()),
            (v6("2001:db8:1::bb02"), PHONE_MAC.to_owned()),
        ]);
        let after = customer_link(
            addr,
            &Metering::Addr,
            &phone_neighbours(),
            &n6,
            Some(&before),
        )
        .unwrap();
        assert_eq!(
            after.v6,
            BTreeSet::from([v6("2001:db8:1::bb02"), v6("2001:db8:1::aa01")])
        );
        // The ARP entry expired: the link is kept as it was.
        assert_eq!(
            customer_link(addr, &Metering::Addr, &HashMap::new(), &n6, Some(&before)),
            Some(before.clone())
        );
        // The address went to a different device: a new link, nothing kept.
        let other = HashMap::from([(v4("192.168.1.23"), "aa:bb:cc:00:00:99".to_owned())]);
        let replaced = customer_link(addr, &Metering::Addr, &other, &n6, Some(&before)).unwrap();
        assert_eq!(replaced.mac, "aa:bb:cc:00:00:99");
        assert!(replaced.v6.is_empty());
    }

    /// `count` addresses in the phone's prefix, all on its MAC.
    fn invented(count: u16) -> HashMap<Ipv6Addr, String> {
        (0..count)
            .map(|i| {
                (
                    Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, i),
                    PHONE_MAC.to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn a_device_inventing_addresses_is_capped() {
        let link = customer_link(
            "192.168.1.23".parse().unwrap(),
            &Metering::Addr,
            &phone_neighbours(),
            &invented(100),
            None,
        )
        .unwrap();
        assert_eq!(link.v6.len(), MAX_V6_PER_PEER);
        assert!(link.overflowed);
        // Exactly at the cap is not over it.
        let at = customer_link(
            "192.168.1.23".parse().unwrap(),
            &Metering::Addr,
            &phone_neighbours(),
            &invented(MAX_V6_PER_PEER as u16),
            None,
        )
        .unwrap();
        assert_eq!(at.v6.len(), MAX_V6_PER_PEER);
        assert!(!at.overflowed);
    }

    #[test]
    fn over_the_cap_the_link_is_known_but_not_allowed_until_it_drops() {
        // A paid phone using one real address. Then sixteen lower ones appear
        // on its MAC — invented by it, or by a neighbour's solicitations — and
        // truncating by address would push the real one out of the gate, the
        // class and the counters, while its MAC was still allowed out.
        let addr: IpAddr = "192.168.1.23".parse().unwrap();
        let real = v6("2001:db8:1::5eed");
        let mut n6 = HashMap::from([(real, PHONE_MAC.to_owned())]);
        let before = customer_link(addr, &Metering::Addr, &phone_neighbours(), &n6, None).unwrap();
        assert!(!before.overflowed);
        n6.extend(invented(MAX_V6_PER_PEER as u16));

        let over = customer_link(
            addr,
            &Metering::Addr,
            &phone_neighbours(),
            &n6,
            Some(&before),
        )
        .unwrap();
        assert!(over.overflowed);
        assert!(over.v6.contains(&real), "the address it had is kept first");
        assert_eq!(over.v6.len(), MAX_V6_PER_PEER);

        // Paid, and still nothing of its IPv6 is allowed: the MAC and every
        // address it holds stay known, so all of it is dropped.
        let mark = MARK_BASE | 2;
        let elements = link_elements(Some(&over), true, "d1", "r1", mark);
        assert!(
            elements
                .iter()
                .all(|(m, _, _)| *m != maps::ALLOWED_MAC && *m != maps::ALLOWED6)
        );
        assert!(elements.contains(&(maps::KNOWN_MAC, PHONE_MAC.to_owned(), Value::Member)));
        assert!(elements.contains(&(maps::KNOWN6, real.to_string(), Value::Member)));

        // The invented entries age out: allowed again, real address and all.
        n6.retain(|a, _| *a == real);
        let recovered =
            customer_link(addr, &Metering::Addr, &phone_neighbours(), &n6, Some(&over)).unwrap();
        assert!(!recovered.overflowed);
        assert!(recovered.v6.contains(&real));
        let elements = link_elements(Some(&recovered), true, "d1", "r1", mark);
        assert!(elements.contains(&(maps::ALLOWED_MAC, PHONE_MAC.to_owned(), Value::Member)));
        assert!(elements.contains(&(maps::ALLOWED6, real.to_string(), Value::Member)));
    }

    #[test]
    fn an_address_moving_between_peers_is_removed_before_it_is_added() {
        // `::77` moves from P1 to P2. Whatever order the peers come in, P1's
        // elements for it go before P2's go in, and neither peer's removal
        // takes out what the other now holds.
        let a = "2001:db8:1::77".to_owned();
        let p1_before = vec![
            (maps::KNOWN6, a.clone(), Value::Member),
            (maps::DOWN6_TX, a.clone(), c("d1")),
            (maps::MARK6, a.clone(), Value::Mark(MARK_BASE | 2)),
        ];
        let p2_after = vec![
            (maps::KNOWN6, a.clone(), Value::Member),
            (maps::DOWN6_TX, a.clone(), c("d2")),
            (maps::MARK6, a.clone(), Value::Mark(MARK_BASE | 3)),
        ];
        let p1 = Move {
            before: p1_before,
            after: vec![],
        };
        let p2 = Move {
            before: vec![],
            after: p2_after.clone(),
        };
        for moves in [vec![p1.clone(), p2.clone()], vec![p2.clone(), p1.clone()]] {
            let (removals, additions) = plan_moves(&moves);
            // P1's counter and mark for the address come out; its `known6`
            // member does not, since P2 holds the identical one — removing it
            // would open the gate for the address until P2's went in.
            assert_eq!(
                removals,
                vec![
                    (maps::DOWN6_TX, a.clone(), c("d1")),
                    (maps::MARK6, a.clone(), Value::Mark(MARK_BASE | 2)),
                ]
            );
            let p2_added = if moves[0].after.is_empty() {
                &additions[1]
            } else {
                &additions[0]
            };
            assert_eq!(*p2_added, p2_after);
        }
        // An unchanged peer contributes nothing to either side.
        let still = Move {
            before: vec![(maps::KNOWN6, a.clone(), Value::Member)],
            after: vec![(maps::KNOWN6, a.clone(), Value::Member)],
        };
        let (removals, additions) = plan_moves(&[still]);
        assert!(removals.is_empty());
        assert!(additions[0].is_empty());
    }

    #[test]
    fn a_customers_ipv6_is_gated_shaped_and_counted_with_its_ipv4() {
        let link = Link {
            mac: PHONE_MAC.into(),
            v6: BTreeSet::from([v6("2001:db8:1::a")]),
            overflowed: false,
        };
        let mark = MARK_BASE | 2;
        let unpaid = link_elements(Some(&link), false, "d1", "r1", mark);
        let a = "2001:db8:1::a".to_owned();
        assert_eq!(
            unpaid,
            vec![
                (maps::KNOWN_MAC, PHONE_MAC.to_owned(), Value::Member),
                (maps::DOWN6_RX, PHONE_MAC.to_owned(), c("r1")),
                (maps::KNOWN6, a.clone(), Value::Member),
                (maps::DOWN6_TX, a.clone(), c("d1")),
                (maps::MARK6, a.clone(), Value::Mark(0x7011_0002)),
            ]
        );
        // Carrying the peer adds exactly the two `allowed` members: that is
        // the whole diff `gate` applies.
        let paid = link_elements(Some(&link), true, "d1", "r1", mark);
        let added: Vec<_> = paid.iter().filter(|e| !unpaid.contains(e)).collect();
        assert_eq!(
            added,
            vec![
                &(maps::ALLOWED_MAC, PHONE_MAC.to_owned(), Value::Member),
                &(maps::ALLOWED6, a, Value::Member),
            ]
        );
        assert!(unpaid.iter().all(|e| paid.contains(e)));
        assert!(link_elements(None, true, "d1", "r1", mark).is_empty());
    }

    #[test]
    fn only_forwardable_ipv6_is_tied_to_a_peer() {
        for a in ["2001:db8::1", "fd00::1"] {
            assert!(forwardable(&v6(a)), "{a}");
        }
        for a in ["fe80::1", "febf::1", "ff02::1", "::1", "::"] {
            assert!(!forwardable(&v6(a)), "{a}");
        }
    }
}
