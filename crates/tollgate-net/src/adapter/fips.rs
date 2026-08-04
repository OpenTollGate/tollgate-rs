//! Gating and shaping a FIPS mesh, through its control socket.
//!
//! FIPS carries the traffic and enforces the policy; this sets the policy. Two
//! commands do the work — `set_transit_policy` says whether a peer's traffic is
//! carried and how fast, and `show_transit_policy` reports what has moved under
//! it — so nothing here has to know how a mesh forwards anything.
//!
//! # Why this one is safe where the kernel adapter is not
//!
//! [`Nftables`](super::Nftables) binds a peer's public key to whatever address
//! it announced itself from, on the peer's own say-so. On an unwrapped IP
//! network that is a real hole: claim a paying peer's key and its grant gates
//! your address.
//!
//! FIPS names peers by npub and has already authenticated the link with a Noise
//! IK handshake before this node hears about it, so a policy set here lands on
//! the key that actually completed that handshake. There is no address to spoof
//! and no binding step to subvert — which is why the [`register`] address
//! argument is ignored rather than merely unused.
//!
//! [`register`]: ResourceAdapter::register
//!
//! # Identity
//!
//! TollGate carries a 33-byte compressed secp256k1 key; FIPS names the same key
//! two ways, and this derives both locally rather than asking:
//!
//! - the **npub** (NIP-19 bech32 of the 32-byte x-only key) is how a policy is
//!   addressed,
//! - the **node address** (`SHA-256(x-only)[..16]`) is how the reply is keyed,
//!   because a node address is a hash and cannot be turned back into an npub.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tollgate_core::access::AccessLevel;
use tollgate_core::meter::Counters;
use tollgate_protocol::PubKey;
use tracing::{debug, warn};

use super::ResourceAdapter;

/// How long a counter reading is reused before the socket is asked again.
///
/// The node reads counters once per peer per tick, but one `show_transit_policy`
/// already returns every peer. Coalescing inside a window turns a round trip per
/// peer into a round trip per tick, and the readings are a tick old at worst —
/// which is what they would have been anyway.
const COUNTER_TTL: Duration = Duration::from_millis(200);

/// How long to wait on the control socket before giving up.
///
/// A grant that fails to apply is worth a log line and a retry on the next
/// tick; it is not worth stalling the event loop that would do the retrying.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// One peer, as this adapter knows it.
#[derive(Debug, Clone)]
struct Peer {
    /// How a policy for this peer is addressed.
    npub: String,
    /// How FIPS keys this peer when reporting, hex-encoded.
    node_addr: String,
    access: AccessLevel,
    rate: u64,
    counters: Counters,
}

/// Sets per-peer transit policy on a FIPS node.
#[derive(Debug)]
pub struct Fips {
    socket: PathBuf,
    peers: Mutex<HashMap<PubKey, Peer>>,
    /// When the counters were last refreshed, for [`COUNTER_TTL`].
    last_refresh: Mutex<Option<Instant>>,
}

impl Fips {
    /// Connect to a FIPS control socket and declare the floor, failing if
    /// nothing answers.
    ///
    /// Probes rather than trusting the path: an adapter that cannot reach FIPS
    /// would report policies as applied while the mesh carried everything
    /// unshaped, which is worse than not starting.
    ///
    /// `minimum_flow` becomes the default policy for peers this node has not
    /// named — which is every peer, for the moment between its link coming up
    /// and the first session reaching the point of setting a policy for it.
    /// Without it that peer would be carried unshaped through exactly the
    /// window in which it has bought nothing. Setting it once here, rather than
    /// per peer as they arrive, is what makes the floor hold from the first
    /// packet instead of the first tick.
    pub fn new(socket: impl Into<PathBuf>, minimum_flow: u64) -> Result<Self> {
        let adapter = Self {
            socket: socket.into(),
            peers: Mutex::new(HashMap::new()),
            last_refresh: Mutex::new(None),
        };
        adapter
            .request("show_transit_policy", serde_json::json!({}))
            .with_context(|| {
                format!(
                    "reach the FIPS control socket at {}",
                    adapter.socket.display()
                )
            })?;

        // A floor of zero is a node that gives unpaid peers nothing at all,
        // which is still a floor worth stating: without the default they would
        // get everything.
        adapter
            .request(
                "set_default_transit_policy",
                serde_json::json!({
                    "admitted": true,
                    "rate_bytes_per_sec": minimum_flow,
                }),
            )
            .context("declare the minimum flow allowance as the FIPS default")?;
        debug!(minimum_flow, "unnamed peers held at the allowance");

        Ok(adapter)
    }

    /// Where FIPS puts its control socket unless told otherwise.
    ///
    /// Mirrors the daemon's own resolution order, so the common case needs no
    /// configuration on either side.
    pub fn default_socket_path() -> PathBuf {
        if Path::new("/run/fips").is_dir() {
            return PathBuf::from("/run/fips/control.sock");
        }
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR")
            && Path::new(&xdg).is_dir()
        {
            return PathBuf::from(format!("{xdg}/fips/control.sock"));
        }
        PathBuf::from("/tmp/fips-control.sock")
    }

    /// Send one command and return its `data`.
    ///
    /// One connection per request. The protocol is a line of JSON each way, so
    /// a pooled connection would buy one `connect` on a local socket at the
    /// price of owning its lifecycle.
    fn request(&self, command: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to {}", self.socket.display()))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        let mut request = serde_json::to_vec(&serde_json::json!({
            "command": command,
            "params": params,
        }))?;
        request.push(b'\n');
        (&stream).write_all(&request).context("write the request")?;

        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .context("read the response")?;

        let response: serde_json::Value =
            serde_json::from_str(&line).context("parse the response")?;
        if response["status"] != "ok" {
            let message = response["message"].as_str().unwrap_or("no reason given");
            bail!("{command} was refused: {message}");
        }
        Ok(response["data"].clone())
    }

    /// Push a peer's current access and rate to FIPS.
    ///
    /// Both knobs go in one message. They are set together here because core
    /// decides them together, and sending one at a time would leave a window
    /// where a peer was admitted at a rate it had not been granted.
    fn apply(&self, peer: &Peer) {
        let result = self.request(
            "set_transit_policy",
            serde_json::json!({
                "npub": peer.npub,
                "admitted": peer.access.delivery_allowed(),
                "rate_bytes_per_sec": peer.rate,
            }),
        );
        if let Err(e) = result {
            warn!(npub = %peer.npub, error = format!("{e:#}"), "could not set a peer's transit policy");
        }
    }

    /// Refresh cached counters unless a recent reading will do.
    fn refresh_counters(&self) {
        {
            let last = self.last_refresh.lock().expect("not poisoned");
            if last.is_some_and(|t| t.elapsed() < COUNTER_TTL) {
                return;
            }
        }

        let data = match self.request("show_transit_policy", serde_json::json!({})) {
            Ok(data) => data,
            Err(e) => {
                warn!(error = format!("{e:#}"), "could not read transit counters");
                return;
            }
        };

        // Keyed by node address, so index the reply once rather than scanning
        // it per peer.
        let mut by_addr: HashMap<&str, (u64, u64)> = HashMap::new();
        if let Some(entries) = data["peers"].as_array() {
            for entry in entries {
                let Some(addr) = entry["node_addr"].as_str() else {
                    continue;
                };
                by_addr.insert(
                    addr,
                    (
                        entry["forwarded_bytes"].as_u64().unwrap_or(0),
                        entry["received_bytes"].as_u64().unwrap_or(0),
                    ),
                );
            }
        }

        let mut peers = self.peers.lock().expect("not poisoned");
        for peer in peers.values_mut() {
            // A peer FIPS has not reported keeps the reading it had. Zeroing it
            // would look like a settled channel rather than a missing answer,
            // and core draws grants down from these.
            if let Some(&(delivered, received)) = by_addr.get(peer.node_addr.as_str()) {
                peer.counters = Counters {
                    delivered,
                    received,
                };
            }
        }
        *self.last_refresh.lock().expect("not poisoned") = Some(Instant::now());
    }
}

/// The npub and node address FIPS knows a TollGate key by.
///
/// Both come from the 32-byte x-only key, which is the compressed key without
/// its parity byte.
fn fips_names(peer: PubKey) -> (String, String) {
    let x_only = &peer.0[1..];

    let npub = bech32::encode::<bech32::Bech32>(bech32::Hrp::parse_unchecked("npub"), x_only)
        .expect("an x-only key is always encodable as bech32");

    let node_addr = hex::encode(&Sha256::digest(x_only)[..16]);
    (npub, node_addr)
}

impl ResourceAdapter for Fips {
    /// Note a peer. The address is ignored: FIPS gates by authenticated
    /// identity, and there is nothing here for an address to add.
    fn register(&self, peer: PubKey, _addr: IpAddr) {
        let mut peers = self.peers.lock().expect("not poisoned");
        if peers.contains_key(&peer) {
            return;
        }
        let (npub, node_addr) = fips_names(peer);
        debug!(%peer, %npub, "peer registered with FIPS");
        peers.insert(
            peer,
            Peer {
                npub,
                node_addr,
                access: AccessLevel::None,
                rate: 0,
                counters: Counters::default(),
            },
        );
    }

    fn set_access(&self, peer: PubKey, access: AccessLevel) {
        let updated = {
            let mut peers = self.peers.lock().expect("not poisoned");
            let Some(entry) = peers.get_mut(&peer) else {
                return;
            };
            if entry.access.delivery_allowed() == access.delivery_allowed() {
                entry.access = access;
                return;
            }
            entry.access = access;
            entry.clone()
        };
        self.apply(&updated);
    }

    fn set_shaping_rate(&self, peer: PubKey, rate: u64) {
        let updated = {
            let mut peers = self.peers.lock().expect("not poisoned");
            let Some(entry) = peers.get_mut(&peer) else {
                return;
            };
            if entry.rate == rate {
                return;
            }
            entry.rate = rate;
            entry.clone()
        };
        self.apply(&updated);
    }

    fn counters(&self, peer: PubKey) -> Counters {
        self.refresh_counters();
        self.peers
            .lock()
            .expect("not poisoned")
            .get(&peer)
            .map(|p| p.counters)
            .unwrap_or_default()
    }

    fn demand(&self, _peer: PubKey) -> u64 {
        // A forwarding node wants no traffic of its own: what it buys upstream
        // follows from what its customers pull through it, which the node works
        // out from the meters.
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
        // Drop the policy rather than leaving the peer admitted at whatever it
        // last bought: a session that has gone away has no grant behind it.
        if let Err(e) = self.request(
            "clear_transit_policy",
            serde_json::json!({ "npub": entry.npub }),
        ) {
            warn!(npub = %entry.npub, error = format!("{e:#}"), "could not clear a peer's transit policy");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key whose npub and node address are known, so the derivation is
    /// checked against FIPS's own encoding rather than against itself.
    ///
    /// Taken from `PeerIdentity::from_npub` in the FIPS tree: the npub below
    /// decodes to this x-only key, and FIPS derives the node address as
    /// `SHA-256(x-only)[..16]`.
    const NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
    const X_ONLY_HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    fn key() -> PubKey {
        let mut bytes = [0u8; 33];
        // The parity byte is not part of the identity FIPS names, so either
        // prefix must produce the same npub.
        bytes[0] = 0x02;
        bytes[1..].copy_from_slice(&hex::decode(X_ONLY_HEX).unwrap());
        PubKey(bytes)
    }

    #[test]
    fn a_key_maps_to_the_npub_fips_names_it_by() {
        let (npub, _) = fips_names(key());
        assert_eq!(npub, NPUB);
    }

    #[test]
    fn the_parity_byte_does_not_change_the_identity() {
        let mut odd = key();
        odd.0[0] = 0x03;
        assert_eq!(fips_names(key()), fips_names(odd));
    }

    #[test]
    fn a_key_maps_to_the_node_address_fips_reports_it_by() {
        let (_, node_addr) = fips_names(key());
        let expected = hex::encode(&Sha256::digest(hex::decode(X_ONLY_HEX).unwrap())[..16]);
        assert_eq!(node_addr, expected);
        assert_eq!(node_addr.len(), 32, "16 bytes, hex-encoded");
    }
}
