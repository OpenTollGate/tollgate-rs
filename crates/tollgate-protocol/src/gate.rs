//! The gate protocol: how `tollgated` drives an enforcement program it does
//! not contain.
//!
//! Specified in `docs/design/core/tollgate-gate-protocol.md`, with `gate.cddl`
//! beside `tollgate.cddl` as the normative schema. The **gate** owns a data
//! plane — a firewall, a proxy, a tunnel — and listens on a Unix socket;
//! `tollgated` connects, says which payer holds which subjects and at what
//! rate each payer is carried, and the gate reports what it carried.
//!
//! The encoding is the wire protocol's: a definite-length CBOR map, key `0` the
//! message type, integer keys, unknown keys skipped, and the same 2-byte
//! little-endian length prefix ([`encode_frame`] and
//! [`FrameReader::next_gate_message`](crate::FrameReader::next_gate_message)).
//! The type tags start at `0x20`, so a frame sent to the wrong socket fails to
//! decode rather than meaning something else.
//!
//! This module is all a gate written in Rust needs of TollGate, and it is
//! `no_std` + `alloc` like the rest of the crate.
//!
//! **`peer` is always the payer**: the TollGate key a session runs under. What
//! the gate's data plane matches is a [`Subject`], and every subject a payer
//! holds arrives in a [`Bind`]. Nothing is ever inferred from the payer's key.

use alloc::vec::Vec;
use core::fmt;

use minicbor::{Decoder, Encoder};

use crate::types::PubKey;

/// Gate protocol version, carried in [`Hello`].
pub const GATE_PROTOCOL_VERSION: u8 = 1;

/// Most subjects one [`Bind`] may carry.
///
/// Keeps the largest message under 1 KiB. The subjects a gate derives for
/// itself — a device's many IPv6 addresses — are the gate's, and never cross
/// the socket.
pub const MAX_BINDINGS: usize = 8;

/// Longest value an [`Subject::Opaque`] may carry, in bytes.
pub const MAX_OPAQUE_LEN: usize = 64;

/// Gate message type tags, `0x20..=0x25`: disjoint from the wire protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GateMsgType {
    /// Gate to `tollgated`: first message on every connection.
    Hello = 0x20,
    /// `tollgated` to gate: every subject a payer holds.
    Bind = 0x21,
    /// `tollgated` to gate: a payer's whole state.
    Set = 0x22,
    /// `tollgated` to gate: forget a payer.
    Remove = 0x23,
    /// Gate to `tollgated`: what was carried for a payer.
    Counters = 0x24,
    /// Gate to `tollgated`: a binding refused because another payer holds it.
    Conflict = 0x25,
}

impl GateMsgType {
    /// Map a tag onto a type, or `None` if it is not a gate message.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x20 => Self::Hello,
            0x21 => Self::Bind,
            0x22 => Self::Set,
            0x23 => Self::Remove,
            0x24 => Self::Counters,
            0x25 => Self::Conflict,
            _ => return None,
        })
    }
}

/// The kinds of subject a gate can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SubjectKind {
    /// An IPv4 address.
    Ipv4 = 0,
    /// An IPv6 address. One in `fd00::/8` is just an address, never a FIPS
    /// identity.
    Ipv6 = 1,
    /// A MAC address.
    Mac = 2,
    /// A 32-byte x-only secp256k1 key: the key itself, not its npub.
    Pubkey = 3,
    /// Anything else, under a `u32` kind the delegating client and the gate
    /// agree on.
    Opaque = 4,
}

impl SubjectKind {
    /// Map a wire value onto a kind, or `None` if it is not one.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Ipv4,
            1 => Self::Ipv6,
            2 => Self::Mac,
            3 => Self::Pubkey,
            4 => Self::Opaque,
            _ => return None,
        })
    }
}

/// What a gate's data plane matches to open or close.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    /// An IPv4 address.
    Ipv4([u8; 4]),
    /// An IPv6 address.
    Ipv6([u8; 16]),
    /// A MAC address.
    Mac([u8; 6]),
    /// An x-only secp256k1 key: the compressed key without its parity byte.
    Pubkey([u8; 32]),
    /// A subject this protocol has no name for.
    Opaque {
        /// Agreed between whoever delegates the subject and the gate.
        kind: u32,
        /// At most [`MAX_OPAQUE_LEN`] bytes.
        value: Vec<u8>,
    },
}

impl Subject {
    /// Which kind of subject this is.
    pub fn kind(&self) -> SubjectKind {
        match self {
            Self::Ipv4(_) => SubjectKind::Ipv4,
            Self::Ipv6(_) => SubjectKind::Ipv6,
            Self::Mac(_) => SubjectKind::Mac,
            Self::Pubkey(_) => SubjectKind::Pubkey,
            Self::Opaque { .. } => SubjectKind::Opaque,
        }
    }

    /// The subject a payer's key names: its x-only form.
    pub fn pubkey_of(key: &PubKey) -> Self {
        let mut x_only = [0u8; 32];
        x_only.copy_from_slice(&key.0[1..]);
        Self::Pubkey(x_only)
    }
}

impl From<core::net::IpAddr> for Subject {
    /// An IPv4-mapped IPv6 address is the IPv4 address it maps: that is the
    /// address a gate sees the traffic from.
    fn from(addr: core::net::IpAddr) -> Self {
        match addr.to_canonical() {
            core::net::IpAddr::V4(v4) => Self::Ipv4(v4.octets()),
            core::net::IpAddr::V6(v6) => Self::Ipv6(v6.octets()),
        }
    }
}

impl fmt::Debug for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4(a) => write!(f, "ipv4 {}", core::net::Ipv4Addr::from(*a)),
            Self::Ipv6(a) => write!(f, "ipv6 {}", core::net::Ipv6Addr::from(*a)),
            Self::Mac(m) => write!(
                f,
                "mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                m[0], m[1], m[2], m[3], m[4], m[5]
            ),
            Self::Pubkey(k) => {
                write!(f, "pubkey ")?;
                for b in k {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
            Self::Opaque { kind, value } => {
                write!(f, "opaque {kind}:")?;
                for b in value {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// A subject a payer holds, and whether a local trusted client delegated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// What the gate matches.
    pub subject: Subject,
    /// A local trusted client's word rather than what `tollgated` itself saw.
    /// The only trust distinction on the wire.
    pub delegated: bool,
}

/// The Identify mode a gate requires `tollgated` to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Identify {
    /// A peer's announced key is taken at its word; `tollgated` binds the
    /// address it connected from.
    Claimed = 0,
    /// A peer must connect from the FIPS address of the key it announces;
    /// `tollgated` binds the key itself.
    Fips = 1,
}

/// `0x20` — gate to `tollgated`, first on every connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Gate protocol version, [`GATE_PROTOCOL_VERSION`].
    pub version: u8,
    /// The subject kinds this gate matches. Never empty.
    pub kinds: Vec<SubjectKind>,
    /// The Identify mode `tollgated` must run.
    pub identify: Identify,
    /// Whether this gate accepts delegated bindings.
    pub delegated: bool,
    /// The [`Subject::Opaque`] kinds this gate matches. Empty for none, which
    /// is also what an absent key means.
    pub opaque_kinds: Vec<u32>,
}

impl Hello {
    /// Whether this gate matches subjects of `kind`.
    pub fn matches(&self, kind: SubjectKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// Whether this gate may be sent `binding`: its kind is one the gate
    /// listed — for an opaque subject, its opaque kind too — and it is not a
    /// delegated binding sent to a gate that refuses them.
    ///
    /// Sending anything else is a protocol error, on which the gate closes the
    /// connection.
    pub fn accepts(&self, binding: &Binding) -> bool {
        if binding.delegated && !self.delegated {
            return false;
        }
        match &binding.subject {
            Subject::Opaque { kind, .. } => {
                self.matches(SubjectKind::Opaque) && self.opaque_kinds.contains(kind)
            }
            subject => self.matches(subject.kind()),
        }
    }
}

/// `0x21` — `tollgated` to gate. The complete set of subjects a payer holds,
/// replacing any earlier set. Empty unbinds them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// The payer.
    pub peer: PubKey,
    /// At most [`MAX_BINDINGS`].
    pub bindings: Vec<Binding>,
}

/// `0x22` — `tollgated` to gate. The payer's whole state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Set {
    /// The payer.
    pub peer: PubKey,
    /// `Some(0)` closed, `Some(n)` open and shaped to `n` bytes per second,
    /// `None` open and unshaped.
    pub rate: Option<u64>,
}

/// `0x23` — `tollgated` to gate. Forget the payer: its subjects return to
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remove {
    /// The payer.
    pub peer: PubKey,
}

/// `0x24` — gate to `tollgated`. Bytes carried to and from the payer's
/// subjects, cumulative from its first [`Bind`] on this connection and summed
/// over all its subjects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// The payer.
    pub peer: PubKey,
    /// What went to its subjects.
    pub delivered: u64,
    /// What came from them.
    pub received: u64,
}

/// `0x25` — gate to `tollgated`. The gate refused to bind `subject` to `peer`,
/// because another payer holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The payer refused.
    pub peer: PubKey,
    /// The subject in question.
    pub subject: Subject,
}

/// Any gate message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateMessage {
    /// See [`Hello`].
    Hello(Hello),
    /// See [`Bind`].
    Bind(Bind),
    /// See [`Set`].
    Set(Set),
    /// See [`Remove`].
    Remove(Remove),
    /// See [`Counters`].
    Counters(Counters),
    /// See [`Conflict`].
    Conflict(Conflict),
}

impl GateMessage {
    /// The tag this message encodes under.
    pub fn msg_type(&self) -> GateMsgType {
        match self {
            Self::Hello(_) => GateMsgType::Hello,
            Self::Bind(_) => GateMsgType::Bind,
            Self::Set(_) => GateMsgType::Set,
            Self::Remove(_) => GateMsgType::Remove,
            Self::Counters(_) => GateMsgType::Counters,
            Self::Conflict(_) => GateMsgType::Conflict,
        }
    }
}

/// Something went wrong encoding or decoding a gate message.
///
/// Every decoding error is a protocol error: the receiver closes the
/// connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not well-formed CBOR, or a value had the wrong type.
    Cbor,
    /// Key `0` held a tag that is not a gate message.
    UnknownType(u8),
    /// Key `0` was absent.
    MissingType,
    /// A field the message type requires was absent.
    MissingField {
        /// The message's tag.
        msg: u8,
        /// The key that should have been present.
        key: u8,
    },
    /// A fixed-width field had the wrong length.
    BadLength {
        /// The key that carried it.
        key: u8,
        /// How many bytes or items it should have held.
        expected: usize,
        /// How many it held.
        got: usize,
    },
    /// A map or array used an indefinite length.
    IndefiniteLength,
    /// A subject kind that is not one of [`SubjectKind`].
    UnknownSubjectKind(u64),
    /// An Identify mode that is not one of [`Identify`].
    UnknownIdentify(u64),
    /// A [`Hello`] listed no subject kinds.
    NoKinds,
    /// A [`Bind`] carried more than [`MAX_BINDINGS`] subjects.
    TooManyBindings(usize),
    /// An opaque subject longer than [`MAX_OPAQUE_LEN`].
    OpaqueTooLong(usize),
    /// A message longer than a frame can carry, [`crate::MAX_FRAME_LEN`].
    FrameTooLong(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cbor => write!(f, "malformed CBOR"),
            Self::UnknownType(t) => write!(f, "unknown gate message type 0x{t:02x}"),
            Self::MissingType => write!(f, "message has no type field"),
            Self::MissingField { msg, key } => {
                write!(f, "gate message 0x{msg:02x} is missing required key {key}")
            }
            Self::BadLength { key, expected, got } => {
                write!(f, "key {key}: expected {expected}, got {got}")
            }
            Self::IndefiniteLength => write!(f, "indefinite length not accepted"),
            Self::UnknownSubjectKind(k) => write!(f, "unknown subject kind {k}"),
            Self::UnknownIdentify(m) => write!(f, "unknown identify mode {m}"),
            Self::NoKinds => write!(f, "hello listed no subject kinds"),
            Self::TooManyBindings(n) => write!(
                f,
                "bind carried {n} subjects, more than the {MAX_BINDINGS} allowed"
            ),
            Self::FrameTooLong(n) => write!(
                f,
                "a message of {n} bytes, more than a frame's {}",
                crate::MAX_FRAME_LEN
            ),
            Self::OpaqueTooLong(n) => write!(
                f,
                "opaque subject of {n} bytes, more than the {MAX_OPAQUE_LEN} allowed"
            ),
        }
    }
}

impl core::error::Error for Error {}

impl From<minicbor::decode::Error> for Error {
    fn from(_: minicbor::decode::Error) -> Self {
        Self::Cbor
    }
}

impl<E> From<minicbor::encode::Error<E>> for Error {
    fn from(_: minicbor::encode::Error<E>) -> Self {
        Self::Cbor
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode a gate message as CBOR, appending to `out`.
///
/// Refuses what the receiver would have to close the connection over: a
/// [`Bind`] with more than [`MAX_BINDINGS`] subjects, an opaque subject longer
/// than [`MAX_OPAQUE_LEN`], a [`Hello`] with no kinds.
pub fn encode(msg: &GateMessage, out: &mut Vec<u8>) -> Result<(), Error> {
    let mut e = Encoder::new(out);
    let tag = msg.msg_type() as u8;

    match msg {
        GateMessage::Hello(m) => {
            if m.kinds.is_empty() {
                return Err(Error::NoKinds);
            }
            // Key 5 only when there is something in it: absent means none.
            let opaque = !m.opaque_kinds.is_empty();
            e.map(if opaque { 6 } else { 5 })?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.u8(m.version)?;
            e.u8(2)?.array(m.kinds.len() as u64)?;
            for kind in &m.kinds {
                e.u8(*kind as u8)?;
            }
            e.u8(3)?.u8(m.identify as u8)?;
            e.u8(4)?.bool(m.delegated)?;
            if opaque {
                e.u8(5)?.array(m.opaque_kinds.len() as u64)?;
                for kind in &m.opaque_kinds {
                    e.u32(*kind)?;
                }
            }
        }
        GateMessage::Bind(m) => {
            if m.bindings.len() > MAX_BINDINGS {
                return Err(Error::TooManyBindings(m.bindings.len()));
            }
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?.array(m.bindings.len() as u64)?;
            for b in &m.bindings {
                e.array(2)?;
                encode_subject(&mut e, &b.subject)?;
                e.bool(b.delegated)?;
            }
        }
        GateMessage::Set(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?;
            match m.rate {
                Some(rate) => e.u64(rate)?,
                None => e.null()?,
            };
        }
        GateMessage::Remove(m) => {
            e.map(2)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
        }
        GateMessage::Counters(m) => {
            e.map(4)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?.u64(m.delivered)?;
            e.u8(3)?.u64(m.received)?;
        }
        GateMessage::Conflict(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?;
            encode_subject(&mut e, &m.subject)?;
        }
    }
    Ok(())
}

fn encode_subject(e: &mut Encoder<&mut Vec<u8>>, subject: &Subject) -> Result<(), Error> {
    let kind = subject.kind() as u8;
    match subject {
        Subject::Ipv4(a) => e.array(2)?.u8(kind)?.bytes(a)?,
        Subject::Ipv6(a) => e.array(2)?.u8(kind)?.bytes(a)?,
        Subject::Mac(a) => e.array(2)?.u8(kind)?.bytes(a)?,
        Subject::Pubkey(a) => e.array(2)?.u8(kind)?.bytes(a)?,
        Subject::Opaque { kind: k, value } => {
            if value.len() > MAX_OPAQUE_LEN {
                return Err(Error::OpaqueTooLong(value.len()));
            }
            e.array(3)?.u8(kind)?.u32(*k)?.bytes(value)?
        }
    };
    Ok(())
}

/// Encode a gate message and append it to `out` with its 2-byte
/// little-endian length prefix: the framing the gate socket carries.
pub fn encode_frame(msg: &GateMessage, out: &mut Vec<u8>) -> Result<(), Error> {
    let prefix_at = out.len();
    out.extend_from_slice(&[0, 0]);
    encode(msg, out)?;

    let len = out.len() - prefix_at - 2;
    // Only a hello with an enormous list of opaque kinds could get here.
    if len > crate::MAX_FRAME_LEN {
        out.truncate(prefix_at);
        return Err(Error::FrameTooLong(len));
    }
    out[prefix_at..prefix_at + 2].copy_from_slice(&(len as u16).to_le_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode one CBOR gate message.
pub fn decode(input: &[u8]) -> Result<GateMessage, Error> {
    let tag = scan_type(input)?;
    let ty = GateMsgType::from_u8(tag).ok_or(Error::UnknownType(tag))?;

    let mut d = Decoder::new(input);
    let pairs = map_len(&mut d)?;

    match ty {
        GateMsgType::Hello => decode_hello(&mut d, pairs).map(GateMessage::Hello),
        GateMsgType::Bind => decode_bind(&mut d, pairs).map(GateMessage::Bind),
        GateMsgType::Set => decode_set(&mut d, pairs).map(GateMessage::Set),
        GateMsgType::Remove => decode_remove(&mut d, pairs).map(GateMessage::Remove),
        GateMsgType::Counters => decode_counters(&mut d, pairs).map(GateMessage::Counters),
        GateMsgType::Conflict => decode_conflict(&mut d, pairs).map(GateMessage::Conflict),
    }
}

fn map_len(d: &mut Decoder<'_>) -> Result<u64, Error> {
    d.map()?.ok_or(Error::IndefiniteLength)
}

fn array_len(d: &mut Decoder<'_>) -> Result<u64, Error> {
    d.array()?.ok_or(Error::IndefiniteLength)
}

fn scan_type(input: &[u8]) -> Result<u8, Error> {
    let mut d = Decoder::new(input);
    let pairs = map_len(&mut d)?;
    for _ in 0..pairs {
        let key = d.u8()?;
        if key == 0 {
            return Ok(d.u8()?);
        }
        d.skip()?;
    }
    Err(Error::MissingType)
}

fn fixed<const N: usize>(d: &mut Decoder<'_>, key: u8) -> Result<[u8; N], Error> {
    let bytes = d.bytes()?;
    bytes.try_into().map_err(|_| Error::BadLength {
        key,
        expected: N,
        got: bytes.len(),
    })
}

fn peer(d: &mut Decoder<'_>) -> Result<PubKey, Error> {
    Ok(PubKey(fixed::<33>(d, 1)?))
}

fn required<T>(value: Option<T>, msg: GateMsgType, key: u8) -> Result<T, Error> {
    value.ok_or(Error::MissingField {
        msg: msg as u8,
        key,
    })
}

fn subject_kind(d: &mut Decoder<'_>) -> Result<SubjectKind, Error> {
    let v = d.u64()?;
    u8::try_from(v)
        .ok()
        .and_then(SubjectKind::from_u8)
        .ok_or(Error::UnknownSubjectKind(v))
}

fn decode_subject(d: &mut Decoder<'_>, key: u8) -> Result<Subject, Error> {
    let fields = array_len(d)?;
    let kind = subject_kind(d)?;
    let expected: usize = if kind == SubjectKind::Opaque { 3 } else { 2 };
    if fields != expected as u64 {
        return Err(Error::BadLength {
            key,
            expected,
            got: fields as usize,
        });
    }
    Ok(match kind {
        SubjectKind::Ipv4 => Subject::Ipv4(fixed(d, key)?),
        SubjectKind::Ipv6 => Subject::Ipv6(fixed(d, key)?),
        SubjectKind::Mac => Subject::Mac(fixed(d, key)?),
        SubjectKind::Pubkey => Subject::Pubkey(fixed(d, key)?),
        SubjectKind::Opaque => {
            let kind = d.u32()?;
            let value = d.bytes()?;
            if value.len() > MAX_OPAQUE_LEN {
                return Err(Error::OpaqueTooLong(value.len()));
            }
            Subject::Opaque {
                kind,
                value: value.to_vec(),
            }
        }
    })
}

fn decode_hello(d: &mut Decoder<'_>, pairs: u64) -> Result<Hello, Error> {
    let (mut version, mut kinds, mut identify, mut delegated) = (None, None, None, None);
    let mut opaque_kinds = Vec::new();
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => version = Some(d.u8()?),
            2 => {
                let n = array_len(d)?;
                // Bounded by the number of kinds there are; a longer list is
                // not a list of kinds.
                let mut list = Vec::new();
                for _ in 0..n {
                    let kind = subject_kind(d)?;
                    if !list.contains(&kind) {
                        list.push(kind);
                    }
                }
                if list.is_empty() {
                    return Err(Error::NoKinds);
                }
                kinds = Some(list);
            }
            3 => {
                let v = d.u64()?;
                identify = Some(match v {
                    0 => Identify::Claimed,
                    1 => Identify::Fips,
                    _ => return Err(Error::UnknownIdentify(v)),
                });
            }
            4 => delegated = Some(d.bool()?),
            5 => {
                let n = array_len(d)?;
                let mut list = Vec::new();
                for _ in 0..n {
                    list.push(d.u32()?);
                }
                opaque_kinds = list;
            }
            _ => d.skip()?,
        }
    }
    Ok(Hello {
        version: required(version, GateMsgType::Hello, 1)?,
        kinds: required(kinds, GateMsgType::Hello, 2)?,
        identify: required(identify, GateMsgType::Hello, 3)?,
        delegated: required(delegated, GateMsgType::Hello, 4)?,
        opaque_kinds,
    })
}

fn decode_bind(d: &mut Decoder<'_>, pairs: u64) -> Result<Bind, Error> {
    let (mut who, mut bindings) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => {
                let n = array_len(d)?;
                // Checked before allocating.
                if n as usize > MAX_BINDINGS {
                    return Err(Error::TooManyBindings(n as usize));
                }
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let fields = array_len(d)?;
                    if fields != 2 {
                        return Err(Error::BadLength {
                            key: 2,
                            expected: 2,
                            got: fields as usize,
                        });
                    }
                    list.push(Binding {
                        subject: decode_subject(d, 2)?,
                        delegated: d.bool()?,
                    });
                }
                bindings = Some(list);
            }
            _ => d.skip()?,
        }
    }
    Ok(Bind {
        peer: required(who, GateMsgType::Bind, 1)?,
        bindings: required(bindings, GateMsgType::Bind, 2)?,
    })
}

fn decode_set(d: &mut Decoder<'_>, pairs: u64) -> Result<Set, Error> {
    let (mut who, mut rate) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => {
                rate = Some(if d.datatype()? == minicbor::data::Type::Null {
                    d.null()?;
                    None
                } else {
                    Some(d.u64()?)
                });
            }
            _ => d.skip()?,
        }
    }
    Ok(Set {
        peer: required(who, GateMsgType::Set, 1)?,
        rate: required(rate, GateMsgType::Set, 2)?,
    })
}

fn decode_remove(d: &mut Decoder<'_>, pairs: u64) -> Result<Remove, Error> {
    let mut who = None;
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            _ => d.skip()?,
        }
    }
    Ok(Remove {
        peer: required(who, GateMsgType::Remove, 1)?,
    })
}

fn decode_counters(d: &mut Decoder<'_>, pairs: u64) -> Result<Counters, Error> {
    let (mut who, mut delivered, mut received) = (None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => delivered = Some(d.u64()?),
            3 => received = Some(d.u64()?),
            _ => d.skip()?,
        }
    }
    Ok(Counters {
        peer: required(who, GateMsgType::Counters, 1)?,
        delivered: required(delivered, GateMsgType::Counters, 2)?,
        received: required(received, GateMsgType::Counters, 3)?,
    })
}

fn decode_conflict(d: &mut Decoder<'_>, pairs: u64) -> Result<Conflict, Error> {
    let (mut who, mut subject) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => subject = Some(decode_subject(d, 2)?),
            _ => d.skip()?,
        }
    }
    Ok(Conflict {
        peer: required(who, GateMsgType::Conflict, 1)?,
        subject: required(subject, GateMsgType::Conflict, 2)?,
    })
}
