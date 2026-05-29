//! Inputs to the state machine. The host produces these from real I/O.

use alloc::vec::Vec;

use crate::metering::Counters;
use crate::peer::PeerId;

/// Something that happened in the outside world, handed to [`crate::Session::handle`].
#[derive(Clone, Debug)]
pub enum Event {
    /// A peer link came up and was authenticated at the transport layer.
    PeerConnected { peer: PeerId },
    /// A peer link went away.
    PeerDisconnected { peer: PeerId },
    /// A decoded protocol frame arrived (still opaque bytes here; decoding into
    /// typed messages is a later step).
    MessageReceived { peer: PeerId, bytes: Vec<u8> },
    /// The latest cumulative meter reading the host observed for a peer.
    MeterSample { peer: PeerId, counters: Counters },
    /// The result of a bootstrap-token verification the host performed against a
    /// mint. `amount` is in scaled milli-units.
    BootstrapVerified { peer: PeerId, amount: u64, ok: bool },
    /// A periodic timer tick (metering intervals, expiry checks).
    Tick,
}
