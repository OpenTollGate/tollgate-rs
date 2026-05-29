//! Peer identity.

use tollgate_protocol::PublicKey;

/// A peer, identified by its raw secp256k1 compressed public key.
///
/// `npub` is only the human-readable form of the same key; core works with the
/// raw bytes and never needs Nostr/Bech32 machinery.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PeerId(pub PublicKey);
