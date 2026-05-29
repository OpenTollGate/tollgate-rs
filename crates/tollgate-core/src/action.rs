//! Outputs from the state machine. The host executes these (and may do async
//! I/O to do so); any results flow back in as [`crate::Event`]s.

use alloc::vec::Vec;

use crate::access::AccessLevel;
use crate::peer::PeerId;

/// An effect the core asks the host to carry out.
#[derive(Clone, Debug)]
pub enum Action {
    /// Send an already-encoded protocol frame to a peer.
    Send { peer: PeerId, bytes: Vec<u8> },
    /// Apply an access decision — install/remove the firewall or forwarding rule.
    SetAccess { peer: PeerId, level: AccessLevel },
    /// Verify/redeem a bootstrap Cashu token with its mint before granting
    /// access. The host replies with [`crate::Event::BootstrapVerified`].
    VerifyBootstrapToken { peer: PeerId, token: Vec<u8> },
    /// Begin sampling the resource meter for a peer.
    StartMetering { peer: PeerId },
    /// Stop sampling the resource meter for a peer.
    StopMetering { peer: PeerId },
    /// Drop the peer link.
    Disconnect { peer: PeerId },
}
