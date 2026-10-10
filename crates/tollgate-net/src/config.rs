//! The YAML an operator writes, and how it becomes a [`NodeConfig`].
//!
//! Every parameter has a default, so a minimal file only says what differs.
//! Delivery has no price to configure: one voucher buys one unit, and what a
//! unit costs in money is decided where vouchers are sold.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tollgate_core::buyer::BuyerPolicy;
use tollgate_core::config::{BurstPolicy, GrantPolicy, NodePolicy, PeerPolicy};
use tollgate_protocol::{DEFAULT_PORT, PubKey};
use tracing::warn;

use crate::channel::Settle;
use crate::identity::Identity;
use crate::node::{NodeConfig, PeerConfig};
use crate::wire::PeerIdentity;

/// The whole configuration file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct File {
    /// This instance's name: letters, digits and `-`. Unset is
    /// [`crate::instance::DEFAULT`]. `--instance` wins over it. See
    /// [`crate::instance`].
    pub instance: Option<String>,
    /// Where `tolltop` and trusted local clients reach this instance. Empty
    /// means `control.sock` in the instance's runtime directory,
    /// `/run/tollgate-<instance>/`; set it only to put the socket somewhere
    /// else.
    pub control_socket: String,
    /// Node identity.
    pub identity: IdentitySection,
    /// This node's own mint, and the unit it denominates in.
    pub mint: MintSection,
    /// Which mints this node takes payment in.
    pub vouchers: VouchersSection,
    /// Where `merchantd` is: what funds this node's channels.
    pub merchant: MerchantSection,
    /// The minimum flow allowance.
    pub access: AccessSection,
    /// Channel parameters.
    pub channels: ChannelsSection,
    /// What a payer may buy: the from-payer weight, windows, reserved rates
    /// and how often.
    pub grants: GrantsSection,
    /// How fast a payer is carried above its reserved rate.
    pub burst: BurstSection,
    /// How this node buys from its peers.
    pub buying: BuyingSection,
    /// Where to listen.
    pub network: NetworkSection,
    /// What enforces delivery: a built-in enforcer, or an external one.
    pub enforcer: EnforcerSection,
    /// A byte source clients can measure this node against.
    pub speedtest: SpeedtestSection,
    /// Per-peer overrides, keyed by hex-encoded compressed pubkey.
    pub peers: BTreeMap<String, PeerSection>,
}

/// Node identity.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentitySection {
    /// 32-byte secp256k1 secret key in hex. Generated if absent.
    pub secret_key: Option<String>,
}

/// Where this node's own mint is.
///
/// The mint is `mintd`, a separate daemon with its own `mint.yaml`
/// ([`crate::mintd`]). `tollgated` holds none of its keys: it advertises the
/// mint to peers and settles at it like any other Cashu client.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MintSection {
    /// Mint URL advertised to peers, and the one they fund channels against.
    ///
    /// It has to be reachable *by peers*, which it always is — it is the node
    /// they are already talking to.
    pub url: String,
    /// Where `tollgated` reaches `mintd` itself, when that differs from what
    /// peers are told. Empty means [`Self::url`].
    pub local: String,
    /// Quantity unit. Fixed by the resource and identical across every node
    /// selling it.
    ///
    /// `"byte"` for network forwarding: a proof is then a claim on one byte of
    /// this node's capacity rather than on money.
    pub unit: String,
}

impl Default for MintSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3338".into(),
            local: String::new(),
            unit: "byte".into(),
        }
    }
}

impl MintSection {
    /// Where to reach the mint from this machine.
    pub fn local_url(&self) -> &str {
        if self.local.is_empty() {
            &self.url
        } else {
            &self.local
        }
    }
}

/// Which mints this node will take payment in.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct VouchersSection {
    /// Most preferred first, at least one. Defaults to this node's own mint.
    pub accepted_mints: Vec<AcceptedMint>,
}

/// A mint this node takes payment in, and what becomes of its vouchers once a
/// channel funded in them has settled.
///
/// Written as a bare URL, which keeps them, or as `{ url, settle }`. `settle`
/// is ignored for this node's own mint, whose vouchers are always burned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "AcceptedMintEntry")]
pub struct AcceptedMint {
    /// The mint.
    pub url: String,
    /// Keep or burn what a channel funded in it pays.
    pub settle: Settle,
}

/// The two ways an accepted mint may be written.
#[derive(Deserialize)]
#[serde(untagged)]
enum AcceptedMintEntry {
    Url(String),
    Full(FullEntry),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FullEntry {
    url: String,
    #[serde(default)]
    settle: Settle,
}

impl From<AcceptedMintEntry> for AcceptedMint {
    fn from(entry: AcceptedMintEntry) -> Self {
        match entry {
            AcceptedMintEntry::Url(url) => Self {
                url,
                settle: Settle::Keep,
            },
            AcceptedMintEntry::Full(FullEntry { url, settle }) => Self { url, settle },
        }
    }
}

impl VouchersSection {
    /// The accepted mints set to burn: what a channel funded in one of them
    /// pays is melted at it rather than kept.
    pub fn burned(&self) -> Vec<String> {
        self.accepted_mints
            .iter()
            .filter(|m| m.settle == Settle::Burn)
            .map(|m| m.url.clone())
            .collect()
    }

    /// Whether anything is kept: an accepted mint other than `own` set to
    /// keep, whose proceeds go to `merchantd`.
    pub fn keeps_any(&self, own: &str) -> bool {
        let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
        self.accepted_mints
            .iter()
            .any(|m| m.settle == Settle::Keep && !same(&m.url, own))
    }
}

/// Where `merchantd` is.
///
/// `tollgated` holds no stock of vouchers and no money: when a channel to a
/// peer opens or rolls over, it asks `merchantd` for the vouchers to fund it
/// with, and hands it anything of value a settlement brings in
/// (`docs/design/core/tollgate-daemons.md`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MerchantSection {
    /// `merchantd`'s funding socket. Empty picks its default.
    pub socket: String,
}

impl MerchantSection {
    /// Where to reach `merchantd`.
    pub fn socket_path(&self) -> PathBuf {
        if self.socket.is_empty() {
            crate::merchant::default_socket_path()
        } else {
            self.socket.clone().into()
        }
    }
}

/// The minimum flow allowance.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccessSection {
    /// Traffic every peer gets without paying.
    pub minimum_flow: MinimumFlow,
}

/// Traffic given away.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MinimumFlow {
    /// Off by default: it is traffic given away, and it can be farmed.
    pub enabled: bool,
    /// Keep it small. Being a rate, it cannot be accumulated.
    pub bytes_per_second: u64,
}

/// Channel parameters.
///
/// Capacities are in the mint's unit — bytes, for forwarding — and default to
/// powers of two, since a channel is funded with one proof per set bit.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChannelsSection {
    /// Units of capacity the first outgoing channel to a peer opens with.
    /// Small, because a new peering may not last.
    pub initial_capacity: u64,
    /// No outgoing channel is opened smaller than this.
    pub min_capacity: u64,
    /// No outgoing channel is opened larger than this, however long the
    /// peering. Also the most this node's market sells in one swap, since a
    /// peer paying us funds a channel of up to this much.
    pub max_capacity: u64,
    /// Multiply the capacity by this each time a channel fills up and is
    /// replaced. `1.0` never grows.
    pub capacity_growth_factor: f64,
    /// How long a channel this node funds lives before the refund path opens.
    pub ttl_seconds: u64,
    /// Percentage of capacity at which the funder starts a rollover.
    pub rollover_threshold_pct: u8,
    /// The safety margin before a channel's expiry, in which the funder rolls
    /// it over and the receiver settles it. It does not depend on the window:
    /// a budget is kept apart from channels. Both ends of a channel must use
    /// the same value.
    pub safety_margin_seconds: u64,
    /// Drop a peer that has sent nothing at all for this long. Zero disables it.
    pub stale_timeout_seconds: u64,
}

impl Default for ChannelsSection {
    fn default() -> Self {
        let d = NodePolicy::default();
        Self {
            initial_capacity: d.initial_channel_capacity,
            min_capacity: d.min_channel_capacity,
            max_capacity: d.max_channel_capacity,
            capacity_growth_factor: d.capacity_growth_pct as f64 / 100.0,
            ttl_seconds: 3_600,
            rollover_threshold_pct: d.rollover_threshold_pct,
            safety_margin_seconds: d.safety_margin_ms / 1_000,
            stale_timeout_seconds: 60,
        }
    }
}

impl ChannelsSection {
    /// Check the channel parameters hang together.
    ///
    /// Returns the growth factor as the percentage core works in.
    fn validate(&self) -> Result<u32> {
        let c = self;
        if c.min_capacity == 0 || c.min_capacity > c.max_capacity {
            bail!(
                "channels.min_capacity ({}) must be above zero and no more than \
                 channels.max_capacity ({})",
                c.min_capacity,
                c.max_capacity
            );
        }
        if !(c.min_capacity..=c.max_capacity).contains(&c.initial_capacity) {
            bail!(
                "channels.initial_capacity ({}) must lie between channels.min_capacity \
                 ({}) and channels.max_capacity ({})",
                c.initial_capacity,
                c.min_capacity,
                c.max_capacity
            );
        }
        // A factor below one would shrink a channel for filling up, and a
        // non-finite one is a typo rather than a policy.
        if !c.capacity_growth_factor.is_finite() || c.capacity_growth_factor < 1.0 {
            bail!(
                "channels.capacity_growth_factor ({}) must be at least 1.0",
                c.capacity_growth_factor
            );
        }
        // A channel that is born inside its own safety margin is rolled over
        // the moment it opens, and again for its replacement: a funding loop,
        // not a short TTL. Twice the margin leaves the channel at least as long
        // in use as in retirement. No window comes into it: what a payer buys
        // is kept in its budget, which outlives any channel.
        if c.ttl_seconds < c.safety_margin_seconds.saturating_mul(2) {
            bail!(
                "channels.ttl_seconds ({}) must be at least twice \
                 channels.safety_margin_seconds ({})",
                c.ttl_seconds,
                c.safety_margin_seconds
            );
        }
        Ok((c.capacity_growth_factor * 100.0)
            .round()
            .min(u32::MAX as f64) as u32)
    }
}

/// What this node will accept when a peer buys from it.
///
/// Each payer has a budget, a deadline and a reserved rate; a purchase adds to
/// the budget, and every second the node draws `max(units moved, reserved rate
/// × 1 s)` from it. The first four are advertised in the Offer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrantsSection {
    /// What a unit from the payer draws from its budget, against one for a
    /// unit to it: `moved = to_payer + from_payer × from_payer_weight`. `1`
    /// charges both directions alike, `10` suits a 100/10 line, `0` makes
    /// what the payer sends free. Fixed for a session; per-peer overrides in
    /// `peers`.
    pub from_payer_weight: u16,
    /// `[min, max]` window in milliseconds. The payer picks any window in this
    /// range, per purchase; the upper end is how long a budget can be kept
    /// without buying again. The lower end may not be below
    /// [`Self::min_topup_gap_ms`].
    pub window_range_ms: [u64; 2],
    /// Smallest reserved rate a payer may choose, units per second. `0` lets a
    /// payer reserve nothing and pay for what it uses; above `0` this node
    /// sells only time at a speed.
    pub min_reserved_rate: u64,
    /// A TopUp sooner than this after a payer's last is refused as too soon,
    /// before any signature on it is checked.
    pub min_topup_gap_ms: u64,
    /// Units per second this node will reserve across all its payers together
    /// — a node-wide ceiling, not a per-peer one. A TopUp whose reserved rate
    /// would take the sum past it is refused, with the rate still free
    /// attached. Absent means the link is the only limit.
    pub max_rate: Option<u64>,
}

impl Default for GrantsSection {
    fn default() -> Self {
        let d = NodePolicy::default();
        Self {
            from_payer_weight: d.from_payer_weight,
            window_range_ms: [d.grants.min_window_ms, d.grants.max_window_ms],
            min_reserved_rate: d.grants.min_reserved_rate,
            min_topup_gap_ms: d.grants.min_topup_gap_ms,
            max_rate: d.grants.max_rate,
        }
    }
}

/// How fast a payer is carried above what it reserved. The node's own policy;
/// it never reaches the protocol, and `grants.max_rate` does not count it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BurstSection {
    /// Carry a payer that reserved a rate at up to this, units per second, if
    /// that is more. `0`, the default, carries it at exactly its rate.
    pub rate: Option<u64>,
    /// Carry a payer that reserved nothing at this, units per second. Absent
    /// is as fast as the link allows; `0` gives it only the minimum flow
    /// allowance until it reserves.
    pub unreserved_rate: Option<u64>,
}

/// How this node buys.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BuyingSection {
    /// Units per second to want from every peer, whether or not anything is
    /// asking for them.
    ///
    /// Zero — the default — means this node buys only what something observes
    /// demand for, which for a node that forwards is what its own customers
    /// pull through it. A node at the edge has no such signal: nothing measures
    /// how much of its own traffic it would like to be able to send, so an
    /// operator who wants it to keep a link paid for says how much here.
    ///
    /// It is a standing order, and it spends money. `--demand` overrides it for
    /// one run.
    pub demand: u64,
    /// Reserve a rate that follows demand — time at a speed. `false` reserves
    /// nothing, or the peer's smallest reserved rate, and pays per use.
    pub reserve: bool,
    /// Reserve this percentage of observed demand.
    pub headroom_pct: u32,
    /// Never reserve below this; raised to the peer's smallest reserved rate.
    pub min_rate: u64,
    /// Never reserve above this — the operator's spending ceiling. Absent is
    /// no ceiling.
    pub max_rate: Option<u64>,
    /// Refuse a peer whose from-payer weight is above this: fund nothing, buy
    /// nothing, pay nothing. Absent takes any.
    pub max_from_payer_weight: Option<u16>,
    /// Window to ask for, clamped to the peer's range. With `reserve` it also
    /// sets the budget: the reserved rate times the window.
    pub window_ms: u64,
    /// Without `reserve`, the units to hold with each peer. Must be set for a
    /// buyer that pays per use.
    pub budget: u64,
    /// Buy again this long before the budget or the deadline would run out.
    pub renew_lead_ms: u64,
    /// How long to keep to a reserved rate a peer named in a TopUpReject
    /// before trying higher again.
    pub cap_hold_ms: u64,
}

impl Default for BuyingSection {
    fn default() -> Self {
        let d = BuyerPolicy::default();
        Self {
            demand: d.demand,
            reserve: d.reserve,
            headroom_pct: d.headroom_pct,
            min_rate: d.min_rate,
            max_rate: None,
            max_from_payer_weight: d.max_from_payer_weight,
            window_ms: d.window_ms,
            budget: d.budget,
            renew_lead_ms: d.renew_lead_ms,
            cap_hold_ms: d.cap_hold_ms,
        }
    }
}

/// Where to listen.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// Control-plane listen address. The data plane is the next port up.
    ///
    /// Under `enforcer.identity: pubkey` this has to be somewhere mesh peers
    /// reach — the node's own `fips0` address, or `[::]` — because a
    /// connection from anywhere else cannot prove whose key it is announcing;
    /// `tollgated` refuses to start otherwise.
    pub listen: String,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            listen: format!("0.0.0.0:{DEFAULT_PORT}"),
        }
    }
}

/// What enforces delivery: lets a paying peer's traffic through, shapes it to
/// the rate it bought, and counts what it carried.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnforcerSection {
    /// `loopback`, `ip`, `fips` or `external`.
    ///
    /// `loopback` shapes and meters a socket of its own and forwards nobody's
    /// traffic — right for a demo or a test, and it runs anywhere. `ip` gates
    /// and shapes the kernel's forwarding path with nftables and `tc`, which is
    /// what actually sells transit, and needs Linux with `CAP_NET_ADMIN`.
    /// `fips` sells transit across a FIPS mesh instead, leaving the enforcement
    /// to the FIPS node and reaching it over its control socket. `external`
    /// hands the enforcement to a separate program listening on
    /// [`Self::socket`], over the enforcer protocol
    /// (`docs/design/core/tollgate-enforcer-protocol.md`).
    pub kind: EnforcerKind,
    /// Interface facing the peers, where their `tc` classes live.
    ///
    /// Only `ip` uses this.
    pub interface: String,
    /// FIPS control socket to drive. Only `fips` uses this; empty means the
    /// same default path the FIPS daemon itself resolves.
    pub fips_socket: String,
    /// The external enforcer's Unix socket. Only `external` uses this; empty
    /// means `enforcer.sock` in the instance's runtime directory,
    /// `/run/tollgate-<instance>/`. Its permissions should admit `tollgated`
    /// alone: reaching it is the power to open the traffic.
    pub socket: String,
    /// Who a connecting peer is: `pubkey` or `address`. Unset takes the
    /// kind's default — see [`Self::identity`].
    #[serde(with = "identity_name")]
    pub identity: Option<PeerIdentity>,
}

impl Default for EnforcerSection {
    fn default() -> Self {
        Self {
            // The default has to run everywhere and gate nothing it does not
            // own: a node that silently installed firewall rules because of a
            // missing config line would be a nasty surprise.
            kind: EnforcerKind::Loopback,
            interface: "eth0".into(),
            fips_socket: String::new(),
            socket: String::new(),
            identity: None,
        }
    }
}

impl EnforcerSection {
    /// Who a connecting peer is: what `identity` says, or the kind's default
    /// — `address` for `ip` and `loopback`, `pubkey` for `fips`.
    ///
    /// `external` has no default: `tollgated` cannot tell what an external
    /// enforcer matches, or which network its peers arrive over, so any
    /// default would be wrong for some enforcer.
    pub fn identity(&self) -> Result<PeerIdentity> {
        if let Some(identity) = self.identity {
            return Ok(identity);
        }
        Ok(match self.kind {
            EnforcerKind::Loopback | EnforcerKind::Ip => PeerIdentity::Address,
            EnforcerKind::Fips => PeerIdentity::Pubkey,
            EnforcerKind::External => bail!(
                "enforcer.kind: external needs enforcer.identity, pubkey or address: \
                 tollgated cannot tell what an external enforcer matches"
            ),
        })
    }

    /// The external enforcer's socket, given the instance's runtime directory:
    /// `socket` if set, else `enforcer.sock` in it.
    pub fn socket_path(&self, runtime_dir: &Path) -> PathBuf {
        if self.socket.is_empty() {
            runtime_dir.join(crate::instance::ENFORCER_SOCKET)
        } else {
            PathBuf::from(&self.socket)
        }
    }
}

/// `enforcer.identity` as its words, `pubkey` and `address`.
mod identity_name {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::wire::PeerIdentity;

    pub fn serialize<S: Serializer>(
        identity: &Option<PeerIdentity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match identity {
            Some(identity) => serializer.serialize_str(identity.as_str()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PeerIdentity>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|name| {
                PeerIdentity::from_name(&name).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "enforcer.identity {name:?} is neither pubkey nor address"
                    ))
                })
            })
            .transpose()
    }
}

/// Which enforcer applies access and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnforcerKind {
    /// A shaper and meter over a dedicated socket.
    Loopback,
    /// nftables and `tc` on the kernel forwarding path.
    Ip,
    /// Per-peer transit policy on a FIPS node, over its control socket.
    Fips,
    /// A separate program, over the enforcer protocol on a Unix socket.
    External,
}

/// A byte source clients can measure this node against.
///
/// Off unless an operator turns it on. The endpoints are unauthenticated, and
/// on a node that is not gating transit they are a free firehose — see
/// [`crate::speedtest`] for why that is the right default even though the
/// intended deployment, behind a FIPS transit policy, is not one.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpeedtestSection {
    /// Whether to serve it at all.
    pub enabled: bool,
    /// Where to serve it.
    ///
    /// Defaults to every interface on both families, because the client this is
    /// for arrives over `fips0` on an IPv6 address and `0.0.0.0` would not hear
    /// it.
    pub listen: String,
    /// Ceiling on a single request, in either direction.
    pub max_bytes: u64,
    /// How many flows the page opens at once.
    pub streams: u8,
    /// How long the page discards before it starts counting.
    pub warmup_ms: u32,
    /// How long it counts for, after the warm-up.
    pub duration_ms: u32,
}

impl Default for SpeedtestSection {
    fn default() -> Self {
        let defaults = crate::speedtest::Config::default();
        Self {
            enabled: false,
            listen: "[::]:3339".into(),
            max_bytes: defaults.max_bytes,
            streams: defaults.streams,
            warmup_ms: defaults.warmup_ms,
            duration_ms: defaults.duration_ms,
        }
    }
}

impl SpeedtestSection {
    /// The address to serve on, and how.
    pub fn resolve(&self) -> Result<(SocketAddr, crate::speedtest::Config)> {
        let listen: SocketAddr = self
            .listen
            .parse()
            .with_context(|| format!("speedtest.listen {:?} is not an address", self.listen))?;
        // A test with no flows measures nothing, and one with no window divides
        // by zero on the page rather than here.
        if self.streams == 0 {
            bail!("speedtest.streams must be at least one");
        }
        if self.duration_ms == 0 {
            bail!("speedtest.duration_ms must be above zero");
        }
        Ok((
            listen,
            crate::speedtest::Config {
                max_bytes: self.max_bytes,
                streams: self.streams,
                warmup_ms: self.warmup_ms,
                duration_ms: self.duration_ms,
            },
        ))
    }
}

/// Per-peer overrides.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeerSection {
    /// Do not charge this peer. One-sided, and not transitive.
    pub no_charge: bool,
    /// Refuse this peer entirely.
    pub blocked: bool,
    /// What a unit this peer sends us draws from its budget, against one for a
    /// unit we send it. Usually `0` for a peering partner. Absent takes
    /// `grants.from_payer_weight`.
    pub from_payer_weight: Option<u16>,
    /// How fast this peer is carried above what it reserved, or when it
    /// reserved nothing. Each key absent takes the `burst` block's.
    pub burst: BurstSection,
    /// Static endpoint to dial. Peers without one have to dial us, and
    /// everything else written here still applies to them when they do.
    pub endpoint: Option<String>,
}

/// Who chose a configuration's renewal lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lead {
    /// An operator, in a file: a thin one is probably a mistake.
    Operator,
    /// The code that wrote the configuration, deliberately.
    Chosen,
}

impl File {
    /// This instance's name: `cli`, which is `--instance`, if given; else
    /// `instance` in the file; else [`crate::instance::DEFAULT`]. Refused
    /// unless it is letters, digits and `-`.
    pub fn instance(&self, cli: Option<&str>) -> Result<String> {
        let name = cli
            .or(self.instance.as_deref())
            .unwrap_or(crate::instance::DEFAULT);
        crate::instance::validate(name)?;
        Ok(name.to_owned())
    }

    /// Where to serve the control socket, given the instance's runtime
    /// directory: `control_socket` if set, else `control.sock` in it.
    pub fn control_socket_path(&self, runtime_dir: &Path) -> PathBuf {
        if self.control_socket.is_empty() {
            runtime_dir.join(crate::instance::CONTROL_SOCKET)
        } else {
            PathBuf::from(&self.control_socket)
        }
    }

    /// Read a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Resolve into what the node actually runs on.
    pub fn resolve(&self) -> Result<NodeConfig> {
        self.resolve_with(Lead::Operator)
    }

    /// Resolve a configuration whose renewal lead was chosen in code, knowing
    /// it is thin, rather than written by an operator.
    ///
    /// The same checks, without the warning about a thin lead: it is there to
    /// catch an operator's mistake, and [`crate::client::run`] picks one below
    /// the safe minimum on purpose. Logged for every session it starts, it
    /// would only teach operators to ignore it.
    pub(crate) fn resolve_with_chosen_lead(&self) -> Result<NodeConfig> {
        self.resolve_with(Lead::Chosen)
    }

    fn resolve_with(&self, lead: Lead) -> Result<NodeConfig> {
        let identity = match &self.identity.secret_key {
            Some(hex) => Identity::from_hex(hex)?,
            None => Identity::generate(),
        };

        // An Offer with no mints is malformed, and a node that names none has
        // simply not said which of its own it means.
        let accepted_mints = if self.vouchers.accepted_mints.is_empty() {
            vec![self.mint.url.clone()]
        } else {
            self.vouchers
                .accepted_mints
                .iter()
                .map(|m| m.url.clone())
                .collect()
        };

        let [min_window_ms, max_window_ms] = self.grants.window_range_ms;
        let grants = GrantPolicy {
            min_window_ms,
            max_window_ms,
            min_reserved_rate: self.grants.min_reserved_rate,
            min_topup_gap_ms: self.grants.min_topup_gap_ms,
            max_rate: self.grants.max_rate,
        };
        if min_window_ms == 0 || !grants.window_range_valid() {
            bail!(
                "grants.window_range_ms [{min_window_ms}, {max_window_ms}] must be a range \
                 starting above zero and no shorter than grants.min_topup_gap_ms ({}): \
                 a shorter window would let a budget expire before its payer may renew it",
                grants.min_topup_gap_ms
            );
        }

        let capacity_growth_pct = self.channels.validate()?;

        let node_burst = BurstPolicy::default();
        let policy = NodePolicy {
            unit: self.mint.unit.clone(),
            accepted_mints,
            from_payer_weight: self.grants.from_payer_weight,
            minimum_flow: if self.access.minimum_flow.enabled {
                self.access.minimum_flow.bytes_per_second
            } else {
                0
            },
            grants,
            burst: BurstPolicy {
                rate: self.burst.rate.unwrap_or(node_burst.rate),
                unreserved_rate: self
                    .burst
                    .unreserved_rate
                    .unwrap_or(node_burst.unreserved_rate),
            },
            tick_ms: crate::node::TICK.as_millis() as u64,
            initial_channel_capacity: self.channels.initial_capacity,
            min_channel_capacity: self.channels.min_capacity,
            max_channel_capacity: self.channels.max_capacity,
            capacity_growth_pct,
            safety_margin_ms: self.channels.safety_margin_seconds.saturating_mul(1_000),
            stale_timeout_ms: self.channels.stale_timeout_seconds.saturating_mul(1_000),
            rollover_threshold_pct: self.channels.rollover_threshold_pct,
        };

        let buyer = BuyerPolicy {
            demand: self.buying.demand,
            reserve: self.buying.reserve,
            headroom_pct: self.buying.headroom_pct,
            min_rate: self.buying.min_rate,
            max_rate: self.buying.max_rate.unwrap_or(u64::MAX),
            max_from_payer_weight: self.buying.max_from_payer_weight,
            window_ms: self.buying.window_ms,
            budget: self.buying.budget,
            renew_lead_ms: self.buying.renew_lead_ms,
            cap_hold_ms: self.buying.cap_hold_ms,
        };

        // A lead at least as long as the window it renews inside is not a
        // conservative setting, it is a contradiction: every purchase starts
        // already inside its own lead. Core clamps it rather than looping, but
        // an operator who wrote this meant something else.
        if self.buying.renew_lead_ms >= self.buying.window_ms {
            bail!(
                "buying.renew_lead_ms ({}) must be shorter than buying.window_ms ({})",
                self.buying.renew_lead_ms,
                self.buying.window_ms
            );
        }
        // A buyer that pays per use holds a budget of `budget` units: with none
        // it would buy nothing at all, which is not what turning reserve off
        // means.
        if !self.buying.reserve && self.buying.budget == 0 {
            bail!("buying.reserve is false, so buying.budget must say how much to hold");
        }
        // Not an error: a node carrying nothing but small requests can live
        // with a short lead, and on an idle link it is free. It is a trap for
        // anything carrying TCP, and the failure — a flow that stalls for
        // seconds after a gap of a tenth of one — does not look like its cause.
        if lead == Lead::Operator && buyer.lead_is_thin() {
            warn!(
                renew_lead_ms = self.buying.renew_lead_ms,
                suggested_lead_ms = BuyerPolicy::MIN_SAFE_LEAD_MS,
                "a purchase this late runs the budget out under load; buying \
                 earlier costs nothing, since nothing is forfeit"
            );
        }

        let listen: SocketAddr = self.network.listen.parse().with_context(|| {
            format!("network.listen {:?} is not an address", self.network.listen)
        })?;

        let mut peers = Vec::new();
        for (key, section) in &self.peers {
            let pubkey = parse_pubkey(key)?;
            let policy = PeerPolicy {
                no_charge: section.no_charge,
                blocked: section.blocked,
                from_payer_weight: section.from_payer_weight,
                burst_rate: section.burst.rate,
                unreserved_rate: section.burst.unreserved_rate,
            };
            // A peer with no endpoint is one that dials us. It is still carried
            // here, because the policy is the point: `blocked` on a peer that
            // calls in is exactly the case that matters, and dropping the entry
            // for want of an address would quietly admit it.
            peers.push(PeerConfig {
                pubkey,
                endpoint: section.endpoint.clone(),
                policy,
            });
        }

        // Who a peer is decides what an enforcer is told. `pubkey` means the
        // network proved the key, which only a mesh peers reach can do; a
        // node listening anywhere else would be believing announced keys,
        // which `pubkey` never means.
        let peer_identity = self.enforcer.identity()?;
        if peer_identity == PeerIdentity::Pubkey && !reached_over_a_mesh(listen) {
            bail!(
                "enforcer.identity is pubkey, but network.listen ({listen}) is not an \
                 address mesh peers reach: listen on the node's fips0 address, or on \
                 [::], or use identity: address"
            );
        }

        Ok(NodeConfig {
            identity,
            policy,
            buyer,
            listen,
            peer_identity,
            mint_url: self.mint.url.clone(),
            mint_local: self.mint.local_url().to_owned(),
            connector: None,
            budget_file: None,
            channel_ttl_seconds: self.channels.ttl_seconds,
            peers,
        })
    }
}

/// Whether `listen` is somewhere peers on a FIPS mesh reach: every IPv6
/// address (`[::]`), or one in `fd00::/8`, where FIPS addresses live.
///
/// `fd00::/8` is FIPS's range but not only FIPS's, so this refuses what cannot
/// be a mesh address rather than proving one is; a connection that does not
/// come from the FIPS address of the key it announces is still refused, one by
/// one, on the wire.
pub fn reached_over_a_mesh(listen: SocketAddr) -> bool {
    match listen.ip() {
        std::net::IpAddr::V6(v6) => v6.is_unspecified() || v6.octets()[0] == 0xfd,
        std::net::IpAddr::V4(_) => false,
    }
}

/// Where a node keeps one of its state files unless told otherwise.
///
/// The first of the directories the packages own that exists or can be made,
/// so that the wallet and the mint land beside each other and whatever keeps
/// one across an upgrade keeps the other.
pub(crate) fn state_file(name: &str) -> PathBuf {
    for candidate in ["/var/lib/tollgate", "/usr/local/var/lib/tollgate"] {
        let dir = Path::new(candidate);
        if dir.is_dir() || std::fs::create_dir_all(dir).is_ok() {
            return dir.join(name);
        }
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(format!("{xdg}/tollgate/{name}"));
    }
    PathBuf::from(format!("/tmp/tollgate-{name}"))
}

/// Where an instance keeps its payers' budgets: in the state directory, beside
/// the wallet and the mint, one file per instance so two instances on one
/// machine never share one.
pub fn budget_file(instance: &str) -> PathBuf {
    state_file(&format!("budgets-{instance}.json"))
}

/// Parse a hex-encoded compressed public key.
fn parse_pubkey(s: &str) -> Result<PubKey> {
    let bytes = hex::decode(s).with_context(|| format!("peer key {s:?} is not hex"))?;
    let bytes: [u8; 33] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("peer key {s:?} is not 33 bytes"))?;
    Ok(PubKey(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_resolves_to_working_defaults() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let config = file.resolve().expect("resolve");

        assert_eq!(config.policy.unit, "byte");
        assert_eq!(config.policy.accepted_mints.len(), 1, "own mint by default");
        assert_eq!(
            config.policy.minimum_flow, 0,
            "the allowance is off by default"
        );
        assert_eq!(config.listen.port(), DEFAULT_PORT);
    }

    #[test]
    fn the_allowance_is_zero_unless_it_is_switched_on() {
        // A rate configured but not enabled is a trap worth closing: it reads as
        // "4 KiB/s free" but must mean nothing until `enabled` is set.
        let file: File = serde_yaml::from_str(
            "access:\n  minimum_flow:\n    enabled: false\n    bytes_per_second: 4096\n",
        )
        .expect("parse");
        assert_eq!(file.resolve().expect("resolve").policy.minimum_flow, 0);
    }

    fn resolved(yaml: &str) -> Result<NodeConfig> {
        serde_yaml::from_str::<File>(yaml).expect("parse").resolve()
    }

    #[test]
    fn each_kind_has_its_default_identity() {
        // A firewall knows peers by address, and plain IP proves no keys.
        for kind in ["loopback", "ip"] {
            let config = resolved(&format!("enforcer:\n  kind: {kind}\n")).expect("resolve");
            assert_eq!(config.peer_identity, PeerIdentity::Address, "{kind}");
        }
        // FIPS proves the key behind every connection.
        let config = resolved("enforcer:\n  kind: fips\nnetwork:\n  listen: \"[::]:4747\"\n")
            .expect("resolve");
        assert_eq!(config.peer_identity, PeerIdentity::Pubkey);
    }

    #[test]
    fn an_external_enforcer_must_be_told_the_identity() {
        let e = resolved("enforcer:\n  kind: external\n").expect_err("no default");
        assert!(format!("{e:#}").contains("enforcer.identity"), "{e:#}");

        let config =
            resolved("enforcer:\n  kind: external\n  identity: address\n").expect("resolve");
        assert_eq!(config.peer_identity, PeerIdentity::Address);
    }

    #[test]
    fn an_identity_is_pubkey_or_address() {
        for word in ["fips", "claimed", "Pubkey", "mac"] {
            let yaml = format!("enforcer:\n  kind: external\n  identity: {word}\n");
            assert!(serde_yaml::from_str::<File>(&yaml).is_err(), "{word}");
        }
        // The old section is gone, not an alias.
        assert!(serde_yaml::from_str::<File>("forwarding:\n  mode: fips\n").is_err());
        assert!(serde_yaml::from_str::<File>("enforcer:\n  kind: nftables\n").is_err());
    }

    #[test]
    fn pubkey_is_refused_where_no_mesh_peer_can_prove_its_key() {
        // The default listen is IPv4: nothing arriving there proves a key.
        for yaml in [
            "enforcer:\n  kind: fips\n",
            "enforcer:\n  kind: external\n  identity: pubkey\n",
            "enforcer:\n  kind: ip\n  identity: pubkey\nnetwork:\n  listen: \"10.0.0.1:4747\"\n",
            "enforcer:\n  kind: external\n  identity: pubkey\nnetwork:\n  listen: \"[2001:db8::1]:4747\"\n",
        ] {
            let e = resolved(yaml).expect_err(yaml);
            assert!(format!("{e:#}").contains("network.listen"), "{yaml}: {e:#}");
        }
        // Every address, or one in the FIPS range, is where mesh peers reach.
        for listen in [
            "[::]:4747",
            "[fd10:93b2:8586:6046:e42d:c089:3228:ccff]:4747",
        ] {
            let yaml = format!(
                "enforcer:\n  kind: external\n  identity: pubkey\nnetwork:\n  listen: \"{listen}\"\n"
            );
            assert_eq!(
                resolved(&yaml).expect("resolve").peer_identity,
                PeerIdentity::Pubkey,
                "{listen}"
            );
        }
        // Address believes nothing it needs proven, so it runs anywhere.
        let config = resolved("enforcer:\n  kind: fips\n  identity: address\n").expect("resolve");
        assert_eq!(config.peer_identity, PeerIdentity::Address);
    }

    #[test]
    fn the_enforcer_socket_follows_from_the_instance_name() {
        let file: File = serde_yaml::from_str(
            "instance: fips-exit\nenforcer:\n  kind: external\n  identity: address\n",
        )
        .expect("parse");
        let dir = crate::instance::dir_in(Path::new("/run"), &file.instance(None).expect("name"));
        assert_eq!(
            file.enforcer.socket_path(&dir),
            PathBuf::from("/run/tollgate-fips-exit/enforcer.sock")
        );
        // The flag names the instance, and so the socket.
        let dir =
            crate::instance::dir_in(Path::new("/run"), &file.instance(Some("ip")).expect("name"));
        assert_eq!(
            file.enforcer.socket_path(&dir),
            PathBuf::from("/run/tollgate-ip/enforcer.sock")
        );

        let moved: File = serde_yaml::from_str(
            "enforcer:\n  kind: external\n  identity: address\n  socket: /srv/enf.sock\n",
        )
        .expect("parse");
        assert_eq!(
            moved.enforcer.socket_path(&dir),
            PathBuf::from("/srv/enf.sock")
        );
    }

    #[test]
    fn an_unnamed_instance_is_called_default() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        assert_eq!(file.instance(None).expect("name"), "default");
    }

    #[test]
    fn the_instance_flag_wins_over_the_file() {
        let file: File = serde_yaml::from_str("instance: ip\n").expect("parse");
        assert_eq!(file.instance(None).expect("name"), "ip");
        assert_eq!(file.instance(Some("fips-exit")).expect("name"), "fips-exit");

        // Checked wherever it came from: it becomes part of a path.
        assert!(file.instance(Some("../etc")).is_err());
        let bad: File = serde_yaml::from_str("instance: fips_exit\n").expect("parse");
        assert!(bad.instance(None).is_err());
        assert_eq!(bad.instance(Some("fips-exit")).expect("name"), "fips-exit");
    }

    #[test]
    fn the_control_socket_follows_from_the_instance_name() {
        let file: File = serde_yaml::from_str("instance: fips-exit\n").expect("parse");
        let name = file.instance(None).expect("name");
        let dir = crate::instance::dir_in(Path::new("/run"), &name);
        assert_eq!(
            file.control_socket_path(&dir),
            PathBuf::from("/run/tollgate-fips-exit/control.sock")
        );

        let unnamed: File = serde_yaml::from_str("{}").expect("parse");
        let dir =
            crate::instance::dir_in(Path::new("/run"), &unnamed.instance(None).expect("name"));
        assert_eq!(
            unnamed.control_socket_path(&dir),
            PathBuf::from("/run/tollgate-default/control.sock")
        );

        let moved: File =
            serde_yaml::from_str("control_socket: /srv/tg/control.sock\n").expect("parse");
        assert_eq!(
            moved.control_socket_path(&dir),
            PathBuf::from("/srv/tg/control.sock")
        );
    }

    #[test]
    fn a_lead_at_least_as_long_as_the_window_is_rejected() {
        // Not conservatism: every grant would start inside its own renewal
        // lead, so the operator meant something else.
        let file: File =
            serde_yaml::from_str("buying:\n  window_ms: 1000\n  renew_lead_ms: 1000\n")
                .expect("parse");
        assert!(file.resolve().is_err());
    }

    #[test]
    fn the_default_pair_is_accepted_and_is_not_thin() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let buyer = file.resolve().expect("resolve").buyer;
        assert!(!buyer.lead_is_thin());
        assert_eq!((buyer.window_ms, buyer.renew_lead_ms), (10_000, 1_200));
        assert!(buyer.reserve);
        assert_eq!(buyer.max_rate, u64::MAX, "no ceiling");
    }

    #[test]
    fn an_inverted_window_range_is_rejected() {
        let file: File =
            serde_yaml::from_str("grants:\n  window_range_ms: [30000, 2000]\n").expect("parse");
        assert!(file.resolve().is_err());
    }

    #[test]
    fn the_grants_default_to_the_documented_terms() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let p = file.resolve().expect("resolve").policy;
        assert_eq!(p.from_payer_weight, 1);
        assert_eq!(
            (p.grants.min_window_ms, p.grants.max_window_ms),
            (1_000, 2_592_000_000),
            "one second to thirty days"
        );
        assert_eq!(p.grants.min_reserved_rate, 0);
        assert_eq!(p.grants.min_topup_gap_ms, 1_000);
        assert_eq!(p.grants.max_rate, None);
        assert_eq!(
            p.burst,
            BurstPolicy::default(),
            "no burst; unreserved at the link"
        );
    }

    #[test]
    fn a_window_shorter_than_the_gap_refuses_to_start() {
        // A budget could expire before its payer is allowed to renew it.
        let e = resolved("grants:\n  window_range_ms: [500, 30000]\n  min_topup_gap_ms: 1000\n")
            .expect_err("refused");
        assert!(format!("{e:#}").contains("min_topup_gap_ms"), "{e:#}");
        assert!(
            resolved("grants:\n  window_range_ms: [200, 30000]\n  min_topup_gap_ms: 200\n").is_ok()
        );
    }

    #[test]
    fn a_year_long_window_fits() {
        let config =
            resolved("grants:\n  window_range_ms: [1000, 31536000000]\n").expect("resolve");
        assert_eq!(config.policy.grants.max_window_ms, 31_536_000_000);
    }

    #[test]
    fn the_from_payer_weight_is_set_per_node_and_per_peer() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!(
            "grants:\n  from_payer_weight: 10\npeers:\n  \"{key}\":\n    from_payer_weight: 0\n"
        );
        let config = resolved(&yaml).expect("resolve");
        assert_eq!(config.policy.from_payer_weight, 10);
        assert_eq!(config.peers[0].policy.weight(&config.policy), 0);
        // Unsigned: there is no way to write a negative one.
        assert!(serde_yaml::from_str::<File>("grants:\n  from_payer_weight: -1\n").is_err());
        // The old key is gone, not an alias.
        assert!(serde_yaml::from_str::<File>("vouchers:\n  received_multiplier: 2\n").is_err());
    }

    #[test]
    fn burst_is_set_per_node_and_per_peer() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!(
            "burst:\n  unreserved_rate: 2500000\npeers:\n  \"{key}\":\n    burst:\n      rate: 2500000\n"
        );
        let config = resolved(&yaml).expect("resolve");
        assert_eq!(
            config.policy.burst,
            BurstPolicy {
                rate: 0,
                unreserved_rate: 2_500_000
            }
        );
        assert_eq!(
            config.peers[0].policy.burst(&config.policy),
            BurstPolicy {
                rate: 2_500_000,
                unreserved_rate: 2_500_000
            }
        );
    }

    #[test]
    fn the_buying_keys_reach_the_buyer() {
        let config = resolved(
            "buying:\n  reserve: false\n  budget: 1000000000\n  max_rate: 5000000\n  \
             max_from_payer_weight: 2\n  demand: 100000\n  window_ms: 2592000000\n",
        )
        .expect("resolve");
        let b = config.buyer;
        assert!(!b.reserve);
        assert_eq!(b.budget, 1_000_000_000);
        assert_eq!(b.max_rate, 5_000_000);
        assert_eq!(b.max_from_payer_weight, Some(2));
        assert_eq!(b.demand, 100_000);
        assert_eq!(b.window_ms, 2_592_000_000);
    }

    #[test]
    fn a_buyer_that_pays_per_use_has_to_say_how_much_to_hold() {
        let e = resolved("buying:\n  reserve: false\n").expect_err("refused");
        assert!(format!("{e:#}").contains("buying.budget"), "{e:#}");
    }

    #[test]
    fn a_peer_without_an_endpoint_is_not_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    no_charge: true\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert!(peers[0].endpoint.is_none(), "nothing to dial");
    }

    #[test]
    fn a_peer_that_dials_us_still_gets_the_policy_written_for_it() {
        // The case this exists for: a blocked peer has no endpoint, because a
        // node does not dial one it refuses to talk to. Dropping the entry for
        // want of an address would admit exactly the peer being turned away.
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    blocked: true\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert!(peers[0].policy.blocked);
    }

    #[test]
    fn a_peer_with_an_endpoint_is_dialled() {
        let key = "02".to_string() + &"11".repeat(32);
        let yaml = format!("peers:\n  \"{key}\":\n    endpoint: \"10.0.0.1:4747\"\n");
        let file: File = serde_yaml::from_str(&yaml).expect("parse");
        let peers = file.resolve().expect("resolve").peers;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].endpoint.as_deref(), Some("10.0.0.1:4747"));
    }

    /// The configs the packages install have to parse, and they are not
    /// exercised by anything else — a typo in one is a router that will not
    /// start, found after it has shipped rather than before.
    #[test]
    fn the_configs_the_packages_install_are_valid() {
        for (name, text) in [
            (
                "openwrt",
                include_str!("../../../packaging/openwrt-ipk/files/etc/tollgate/tollgate.yaml"),
            ),
            (
                "macos",
                include_str!("../../../packaging/macos/tollgate.yaml"),
            ),
        ] {
            let file: File = serde_yaml::from_str(text)
                .unwrap_or_else(|e| panic!("the {name} package config does not parse: {e}"));
            let config = file
                .resolve()
                .unwrap_or_else(|e| panic!("the {name} package config does not resolve: {e:#}"));

            // Both ship without an identity, because one is generated at
            // install time. Anything else in them has to be usable as it is.
            assert_eq!(config.policy.unit, "byte", "{name}");
            assert!(config.policy.minimum_flow > 0, "{name}: no allowance");
        }
    }

    #[test]
    fn the_mint_is_reached_where_peers_are_told_unless_said_otherwise() {
        let file: File = serde_yaml::from_str("mint:\n  url: http://gw:3338\n").expect("parse");
        assert_eq!(file.mint.local_url(), "http://gw:3338");

        let file: File =
            serde_yaml::from_str("mint:\n  url: http://gw:3338\n  local: http://127.0.0.1:3338\n")
                .expect("parse");
        assert_eq!(file.mint.local_url(), "http://127.0.0.1:3338");
    }

    #[test]
    fn channels_default_to_one_gib_growing_to_sixteen_over_an_hour_ttl() {
        let file: File = serde_yaml::from_str("{}").expect("parse");
        let config = file.resolve().expect("resolve");
        let p = &config.policy;

        assert_eq!(p.initial_channel_capacity, 1 << 30, "1 GiB, one proof");
        assert_eq!(p.min_channel_capacity, 1 << 27, "128 MiB");
        assert_eq!(p.max_channel_capacity, 1 << 34, "16 GiB");
        assert_eq!(p.capacity_growth_pct, 200);
        assert_eq!(p.rollover_threshold_pct, 80);
        assert_eq!(p.safety_margin_ms, 60_000);
        assert_eq!(p.stale_timeout_ms, 60_000, "silence only");
        assert_eq!(config.channel_ttl_seconds, 3_600);
    }

    #[test]
    fn the_channel_ttl_comes_from_the_config() {
        let file: File = serde_yaml::from_str("channels:\n  ttl_seconds: 7200\n").expect("parse");
        assert_eq!(file.resolve().expect("resolve").channel_ttl_seconds, 7_200);
    }

    #[test]
    fn a_ttl_inside_its_own_safety_margin_is_rejected() {
        // Two minutes against a one-minute margin is the shortest that works;
        // anything less would roll every channel over as soon as it opened.
        let ok: File = serde_yaml::from_str("channels:\n  ttl_seconds: 120\n").expect("parse");
        assert!(ok.resolve().is_ok());
        let short: File = serde_yaml::from_str("channels:\n  ttl_seconds: 119\n").expect("parse");
        assert!(short.resolve().is_err());

        // And no window comes into it: a month-long window needs no
        // month-long channel.
        let long: File = serde_yaml::from_str(
            "channels:\n  ttl_seconds: 120\ngrants:\n  window_range_ms: [1000, 2592000000]\n",
        )
        .expect("parse");
        assert!(long.resolve().is_ok());
    }

    #[test]
    fn the_growth_factor_becomes_a_percentage() {
        let file: File =
            serde_yaml::from_str("channels:\n  capacity_growth_factor: 1.5\n").expect("parse");
        assert_eq!(
            file.resolve().expect("resolve").policy.capacity_growth_pct,
            150
        );
        let shrinking: File =
            serde_yaml::from_str("channels:\n  capacity_growth_factor: 0.5\n").expect("parse");
        assert!(shrinking.resolve().is_err());
    }

    #[test]
    fn capacities_out_of_order_are_rejected() {
        for yaml in [
            "channels:\n  initial_capacity: 1000\n",
            "channels:\n  initial_capacity: 34359738368\n",
            "channels:\n  min_capacity: 0\n",
            "channels:\n  min_capacity: 34359738368\n",
        ] {
            let file: File = serde_yaml::from_str(yaml).expect("parse");
            assert!(file.resolve().is_err(), "{yaml}");
        }

        // A deliberately small channel is fine once the floor is lowered with it.
        let small: File = serde_yaml::from_str(
            "channels:\n  initial_capacity: 5000000\n  min_capacity: 1000000\n",
        )
        .expect("parse");
        assert_eq!(
            small
                .resolve()
                .expect("resolve")
                .policy
                .first_channel_capacity(),
            5_000_000
        );
    }

    #[test]
    fn an_accepted_mint_is_a_bare_url_that_keeps_or_says_what_to_do() {
        let file: File = serde_yaml::from_str(
            "vouchers:\n  accepted_mints:\n\
             \x20   - http://ours:3338\n\
             \x20   - url: http://upstream:3338\n\
             \x20   - url: http://upstream2:3338\n      settle: keep\n\
             \x20   - url: http://neighbour:3338\n      settle: burn\n",
        )
        .expect("parse");
        let settles: Vec<_> = file
            .vouchers
            .accepted_mints
            .iter()
            .map(|m| (m.url.as_str(), m.settle))
            .collect();
        assert_eq!(
            settles,
            [
                ("http://ours:3338", Settle::Keep),
                ("http://upstream:3338", Settle::Keep),
                ("http://upstream2:3338", Settle::Keep),
                ("http://neighbour:3338", Settle::Burn),
            ]
        );
        assert_eq!(file.vouchers.burned(), ["http://neighbour:3338"]);

        // Core sees the URLs, in order.
        let config = file.resolve().expect("resolve");
        assert_eq!(config.policy.accepted_mints.len(), 4);
        assert_eq!(config.policy.accepted_mints[3], "http://neighbour:3338");
    }

    #[test]
    fn only_another_mint_kept_needs_somewhere_to_keep_it() {
        let own = "http://ours:3338";
        let keeps = |yaml: &str| {
            serde_yaml::from_str::<File>(yaml)
                .expect("parse")
                .vouchers
                .keeps_any(own)
        };
        // The own mint is burned whatever it says, so it keeps nothing.
        assert!(!keeps(
            "vouchers:\n  accepted_mints: [\"http://ours:3338/\"]\n"
        ));
        assert!(!keeps(
            "vouchers:\n  accepted_mints:\n    - url: http://theirs:3338\n      settle: burn\n"
        ));
        assert!(keeps(
            "vouchers:\n  accepted_mints: [\"http://theirs:3338\"]\n"
        ));
    }

    #[test]
    fn a_settle_that_is_neither_keep_nor_burn_is_an_error() {
        for yaml in [
            "vouchers:\n  accepted_mints:\n    - url: http://a\n      settle: sell\n",
            "vouchers:\n  accepted_mints:\n    - url: http://a\n      setle: burn\n",
            "vouchers:\n  accepted_mints:\n    - settle: burn\n",
        ] {
            assert!(serde_yaml::from_str::<File>(yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn a_misspelt_key_is_an_error_rather_than_silently_ignored() {
        // `deny_unknown_fields` is what stops `from_payer_wieght` from
        // quietly meaning "default".
        assert!(serde_yaml::from_str::<File>("grants:\n  from_payer_wieght: 2\n").is_err());
    }
}
