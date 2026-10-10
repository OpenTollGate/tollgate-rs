//! The enforcer protocol: how `tollgated` drives an enforcer it does not
//! contain.
//!
//! Specified in `docs/design/core/tollgate-enforcer-protocol.md`, with
//! `enforcer.cddl` beside `tollgate.cddl` as the normative schema. An
//! **external enforcer** owns the traffic it controls — a firewall, a proxy, a
//! tunnel — and listens on a Unix socket; `tollgated` connects, says which
//! payer holds which subjects and at what rate each payer is carried, and the
//! enforcer reports what it carried.
//!
//! The encoding is the wire protocol's: a definite-length CBOR map, key `0` the
//! message type, integer keys, unknown keys skipped, and the same 2-byte
//! little-endian length prefix ([`encode_frame`] and
//! [`FrameReader::next_enforcer_message`](crate::FrameReader::next_enforcer_message)).
//! The type tags start at `0x20`, so a frame sent to the wrong socket fails to
//! decode rather than meaning something else.
//!
//! This module is all an enforcer written in Rust needs of TollGate, and it is
//! `no_std` + `alloc` like the rest of the crate.
//!
//! **`peer` is always the payer**: the TollGate key a session runs under. What
//! the enforcer recognizes in traffic is a [`Subject`]: plain bytes, whose form
//! the instance's [`Identity`] fixes. Every subject a payer holds arrives in a
//! [`Bind`]; nothing is ever inferred from the payer's key.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use minicbor::{Decoder, Encoder};

use crate::types::PubKey;

/// Enforcer protocol version, carried in [`Hello`].
pub const PROTOCOL_VERSION: u8 = 1;

/// Most subjects one [`Bind`] may carry.
///
/// Keeps the largest message under 1 KiB. The subjects an enforcer derives for
/// itself — a device's many IPv6 addresses — are its own, and never cross the
/// socket.
pub const MAX_BINDINGS: usize = 8;

/// Longest a [`Subject`] may be, in bytes. The shortest is one.
pub const MAX_SUBJECT_LEN: usize = 64;

/// Enforcer message type tags, `0x20..=0x25`: disjoint from the wire
/// protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EnforcerMsgType {
    /// Enforcer to `tollgated`: first message on every connection.
    Hello = 0x20,
    /// `tollgated` to enforcer: every subject a payer holds.
    Bind = 0x21,
    /// `tollgated` to enforcer: a payer's whole state.
    Set = 0x22,
    /// `tollgated` to enforcer: forget a payer.
    Remove = 0x23,
    /// Enforcer to `tollgated`: what was carried for a payer.
    Counters = 0x24,
    /// Enforcer to `tollgated`: a binding refused because another payer holds
    /// the subject.
    Conflict = 0x25,
}

impl EnforcerMsgType {
    /// Map a tag onto a type, or `None` if it is not an enforcer message.
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

/// Who a connecting peer is — `enforcer.identity` in `tollgated`'s config —
/// and so what form a subject that is not delegated takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Identity {
    /// The peer is its public key, proven by the network it came over. A
    /// subject is the key's 32-byte x-only form.
    Pubkey,
    /// The peer is the address its connection came from. A subject is that
    /// address in 16 bytes, an IPv4 address written IPv4-mapped.
    Address,
}

impl Identity {
    /// The word for it, on the wire and in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pubkey => "pubkey",
            Self::Address => "address",
        }
    }

    /// The identity a word names, or `None` if it names none.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "pubkey" => Some(Self::Pubkey),
            "address" => Some(Self::Address),
            _ => None,
        }
    }

    /// How long a subject that is not delegated is under this identity.
    pub fn subject_len(self) -> usize {
        match self {
            Self::Pubkey => 32,
            Self::Address => 16,
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an enforcer recognizes in traffic: plain bytes, one to
/// [`MAX_SUBJECT_LEN`] of them.
///
/// The wire says nothing about what kind of thing a subject is. Under
/// [`Identity::Pubkey`] it is a 32-byte x-only key, under
/// [`Identity::Address`] a 16-byte address, and a delegated one is whatever
/// the delegating client and the enforcer agreed on. Every address has exactly
/// one encoding, so two subjects are the same subject exactly when their bytes
/// are equal.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Subject(Vec<u8>);

impl Subject {
    /// A subject of these bytes, refused unless there are one to
    /// [`MAX_SUBJECT_LEN`] of them.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_SUBJECT_LEN {
            return Err(Error::SubjectLength(bytes.len()));
        }
        Ok(Self(bytes))
    }

    /// The subject a key is under [`Identity::Pubkey`]: its x-only form, the
    /// compressed key without its parity byte.
    pub fn pubkey(key: &PubKey) -> Self {
        Self(key.0[1..].to_vec())
    }

    /// The subject an address is under [`Identity::Address`]: 16 bytes, an
    /// IPv4 address written as `::ffff:a.b.c.d`. An address that is already
    /// IPv4-mapped stays as it is, so each address has one encoding.
    pub fn address(addr: core::net::IpAddr) -> Self {
        let v6 = match addr {
            core::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
            core::net::IpAddr::V6(v6) => v6,
        };
        Self(v6.octets().to_vec())
    }

    /// The bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Hex, since the bytes say nothing of what they are.
impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A subject a payer holds, and whether a local trusted client delegated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// What the enforcer recognizes.
    pub subject: Subject,
    /// A local trusted client's word rather than what `tollgated` itself saw.
    /// The only trust distinction on the wire.
    pub delegated: bool,
}

/// `0x20` — enforcer to `tollgated`, first on every connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Enforcer protocol version, [`PROTOCOL_VERSION`].
    pub version: u8,
    /// The identity the enforcer was built for. Only a check: it must equal
    /// `tollgated`'s `enforcer.identity`.
    pub identity: Identity,
    /// Whether this enforcer accepts delegated bindings.
    pub delegated: bool,
    /// The unit the enforcer counts in, such as `byte` or `ml`. Only a check:
    /// it must equal `tollgated`'s own unit, compared as plain text.
    pub unit: String,
}

impl Hello {
    /// Whether this enforcer may be sent `binding`, and why not.
    ///
    /// A delegated binding needs an enforcer that accepts them; any other
    /// must be exactly as long as the identity says. Sending anything else is
    /// a protocol error, on which the enforcer closes the connection.
    pub fn check(&self, binding: &Binding) -> Result<(), Error> {
        if binding.delegated {
            if !self.delegated {
                return Err(Error::DelegatedRefused);
            }
            return Ok(());
        }
        let got = binding.subject.as_bytes().len();
        if got != self.identity.subject_len() {
            return Err(Error::WrongSubjectLength {
                identity: self.identity,
                got,
            });
        }
        Ok(())
    }

    /// Whether [`Self::check`] passes.
    pub fn accepts(&self, binding: &Binding) -> bool {
        self.check(binding).is_ok()
    }
}

/// `0x21` — `tollgated` to enforcer. The complete set of subjects a payer
/// holds, replacing any earlier set. Empty unbinds them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    /// The payer.
    pub peer: PubKey,
    /// At most [`MAX_BINDINGS`].
    pub bindings: Vec<Binding>,
}

/// `0x22` — `tollgated` to enforcer. The payer's whole state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Set {
    /// The payer.
    pub peer: PubKey,
    /// `Some(0)` closed, `Some(n)` open and shaped to `n` units per second,
    /// `None` open and unshaped.
    pub rate: Option<u64>,
}

/// `0x23` — `tollgated` to enforcer. Forget the payer: its subjects return to
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remove {
    /// The payer.
    pub peer: PubKey,
}

/// `0x24` — enforcer to `tollgated`. Units carried to and from the payer's
/// subjects, cumulative from its first [`Bind`] on this connection and summed
/// over all its subjects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// The payer.
    pub peer: PubKey,
    /// `units_to_payer`: what went to its subjects.
    pub to_payer: u64,
    /// `units_from_payer`: what came from them.
    pub from_payer: u64,
}

/// `0x25` — enforcer to `tollgated`. The enforcer refused to bind `subject` to
/// `peer`, because another payer holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The payer refused.
    pub peer: PubKey,
    /// The subject in question.
    pub subject: Subject,
}

/// Any enforcer message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforcerMessage {
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

impl EnforcerMessage {
    /// The tag this message encodes under.
    pub fn msg_type(&self) -> EnforcerMsgType {
        match self {
            Self::Hello(_) => EnforcerMsgType::Hello,
            Self::Bind(_) => EnforcerMsgType::Bind,
            Self::Set(_) => EnforcerMsgType::Set,
            Self::Remove(_) => EnforcerMsgType::Remove,
            Self::Counters(_) => EnforcerMsgType::Counters,
            Self::Conflict(_) => EnforcerMsgType::Conflict,
        }
    }
}

/// Something went wrong encoding, decoding or checking an enforcer message.
///
/// Every one of these met on the socket is a protocol error: the receiver
/// closes the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not well-formed CBOR, or a value had the wrong type.
    Cbor,
    /// Key `0` held a tag that is not an enforcer message.
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
    /// A payer had the wrong length.
    BadLength {
        /// The key that carried it.
        key: u8,
        /// How many bytes it should have held.
        expected: usize,
        /// How many it held.
        got: usize,
    },
    /// A map or array used an indefinite length.
    IndefiniteLength,
    /// An identity that is neither `pubkey` nor `address`.
    UnknownIdentity(String),
    /// A subject of no bytes, or of more than [`MAX_SUBJECT_LEN`].
    SubjectLength(usize),
    /// A subject that is not delegated, of the wrong length for the identity:
    /// 32 bytes under `pubkey`, 16 under `address`.
    WrongSubjectLength {
        /// The identity in force.
        identity: Identity,
        /// How many bytes the subject held.
        got: usize,
    },
    /// A delegated binding, for an enforcer that refuses them.
    DelegatedRefused,
    /// A [`Bind`] carried more than [`MAX_BINDINGS`] subjects.
    TooManyBindings(usize),
    /// A message longer than a frame can carry, [`crate::MAX_FRAME_LEN`].
    FrameTooLong(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cbor => write!(f, "malformed CBOR"),
            Self::UnknownType(t) => write!(f, "unknown enforcer message type 0x{t:02x}"),
            Self::MissingType => write!(f, "message has no type field"),
            Self::MissingField { msg, key } => {
                write!(
                    f,
                    "enforcer message 0x{msg:02x} is missing required key {key}"
                )
            }
            Self::BadLength { key, expected, got } => {
                write!(f, "key {key}: expected {expected} bytes, got {got}")
            }
            Self::IndefiniteLength => write!(f, "indefinite length not accepted"),
            Self::UnknownIdentity(name) => {
                write!(f, "unknown identity {name:?}; pubkey or address")
            }
            Self::SubjectLength(n) => write!(
                f,
                "a subject of {n} bytes; one to {MAX_SUBJECT_LEN} are allowed"
            ),
            Self::WrongSubjectLength { identity, got } => write!(
                f,
                "a subject of {got} bytes under identity {identity}, which takes {}",
                identity.subject_len()
            ),
            Self::DelegatedRefused => {
                write!(f, "a delegated binding, to an enforcer that refuses them")
            }
            Self::TooManyBindings(n) => write!(
                f,
                "bind carried {n} subjects, more than the {MAX_BINDINGS} allowed"
            ),
            Self::FrameTooLong(n) => write!(
                f,
                "a message of {n} bytes, more than a frame's {}",
                crate::MAX_FRAME_LEN
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

/// Encode an enforcer message as CBOR, appending to `out`.
///
/// Refuses a [`Bind`] with more than [`MAX_BINDINGS`] subjects, which the
/// receiver would have to close the connection over. A [`Subject`] cannot be
/// built at a length the schema refuses.
pub fn encode(msg: &EnforcerMessage, out: &mut Vec<u8>) -> Result<(), Error> {
    let mut e = Encoder::new(out);
    let tag = msg.msg_type() as u8;

    match msg {
        EnforcerMessage::Hello(m) => {
            e.map(5)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.u8(m.version)?;
            e.u8(2)?.str(m.identity.as_str())?;
            e.u8(3)?.bool(m.delegated)?;
            e.u8(4)?.str(&m.unit)?;
        }
        EnforcerMessage::Bind(m) => {
            if m.bindings.len() > MAX_BINDINGS {
                return Err(Error::TooManyBindings(m.bindings.len()));
            }
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?.array(m.bindings.len() as u64)?;
            for b in &m.bindings {
                e.array(2)?.bytes(b.subject.as_bytes())?.bool(b.delegated)?;
            }
        }
        EnforcerMessage::Set(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?;
            match m.rate {
                Some(rate) => e.u64(rate)?,
                None => e.null()?,
            };
        }
        EnforcerMessage::Remove(m) => {
            e.map(2)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
        }
        EnforcerMessage::Counters(m) => {
            e.map(4)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?.u64(m.to_payer)?;
            e.u8(3)?.u64(m.from_payer)?;
        }
        EnforcerMessage::Conflict(m) => {
            e.map(3)?;
            e.u8(0)?.u8(tag)?;
            e.u8(1)?.bytes(&m.peer.0)?;
            e.u8(2)?.bytes(m.subject.as_bytes())?;
        }
    }
    Ok(())
}

/// Encode an enforcer message and append it to `out` with its 2-byte
/// little-endian length prefix: the framing the enforcer socket carries.
pub fn encode_frame(msg: &EnforcerMessage, out: &mut Vec<u8>) -> Result<(), Error> {
    let prefix_at = out.len();
    out.extend_from_slice(&[0, 0]);
    if let Err(e) = encode(msg, out) {
        out.truncate(prefix_at);
        return Err(e);
    }

    let len = out.len() - prefix_at - 2;
    // Only a hello with an enormous unit could get here.
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

/// Decode one CBOR enforcer message.
///
/// Checks what the schema says. Whether a subject has the length the identity
/// in force needs, or is delegated to an enforcer that refuses that, is a
/// matter for [`Hello::check`]: the bytes alone do not say.
pub fn decode(input: &[u8]) -> Result<EnforcerMessage, Error> {
    let tag = scan_type(input)?;
    let ty = EnforcerMsgType::from_u8(tag).ok_or(Error::UnknownType(tag))?;

    let mut d = Decoder::new(input);
    let pairs = map_len(&mut d)?;

    match ty {
        EnforcerMsgType::Hello => decode_hello(&mut d, pairs).map(EnforcerMessage::Hello),
        EnforcerMsgType::Bind => decode_bind(&mut d, pairs).map(EnforcerMessage::Bind),
        EnforcerMsgType::Set => decode_set(&mut d, pairs).map(EnforcerMessage::Set),
        EnforcerMsgType::Remove => decode_remove(&mut d, pairs).map(EnforcerMessage::Remove),
        EnforcerMsgType::Counters => decode_counters(&mut d, pairs).map(EnforcerMessage::Counters),
        EnforcerMsgType::Conflict => decode_conflict(&mut d, pairs).map(EnforcerMessage::Conflict),
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

fn peer(d: &mut Decoder<'_>) -> Result<PubKey, Error> {
    let bytes = d.bytes()?;
    let key: [u8; 33] = bytes.try_into().map_err(|_| Error::BadLength {
        key: 1,
        expected: 33,
        got: bytes.len(),
    })?;
    Ok(PubKey(key))
}

fn subject(d: &mut Decoder<'_>) -> Result<Subject, Error> {
    Subject::new(d.bytes()?)
}

fn required<T>(value: Option<T>, msg: EnforcerMsgType, key: u8) -> Result<T, Error> {
    value.ok_or(Error::MissingField {
        msg: msg as u8,
        key,
    })
}

fn decode_hello(d: &mut Decoder<'_>, pairs: u64) -> Result<Hello, Error> {
    let (mut version, mut identity, mut delegated, mut unit) = (None, None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => version = Some(d.u8()?),
            2 => {
                let name = d.str()?;
                identity = Some(
                    Identity::from_name(name).ok_or_else(|| Error::UnknownIdentity(name.into()))?,
                );
            }
            3 => delegated = Some(d.bool()?),
            4 => unit = Some(String::from(d.str()?)),
            _ => d.skip()?,
        }
    }
    Ok(Hello {
        version: required(version, EnforcerMsgType::Hello, 1)?,
        identity: required(identity, EnforcerMsgType::Hello, 2)?,
        delegated: required(delegated, EnforcerMsgType::Hello, 3)?,
        unit: required(unit, EnforcerMsgType::Hello, 4)?,
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
                if n > MAX_BINDINGS as u64 {
                    return Err(Error::TooManyBindings(
                        usize::try_from(n).unwrap_or(usize::MAX),
                    ));
                }
                let mut list = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    if array_len(d)? != 2 {
                        return Err(Error::Cbor);
                    }
                    list.push(Binding {
                        subject: subject(d)?,
                        delegated: d.bool()?,
                    });
                }
                bindings = Some(list);
            }
            _ => d.skip()?,
        }
    }
    Ok(Bind {
        peer: required(who, EnforcerMsgType::Bind, 1)?,
        bindings: required(bindings, EnforcerMsgType::Bind, 2)?,
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
        peer: required(who, EnforcerMsgType::Set, 1)?,
        rate: required(rate, EnforcerMsgType::Set, 2)?,
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
        peer: required(who, EnforcerMsgType::Remove, 1)?,
    })
}

fn decode_counters(d: &mut Decoder<'_>, pairs: u64) -> Result<Counters, Error> {
    let (mut who, mut to_payer, mut from_payer) = (None, None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => to_payer = Some(d.u64()?),
            3 => from_payer = Some(d.u64()?),
            _ => d.skip()?,
        }
    }
    Ok(Counters {
        peer: required(who, EnforcerMsgType::Counters, 1)?,
        to_payer: required(to_payer, EnforcerMsgType::Counters, 2)?,
        from_payer: required(from_payer, EnforcerMsgType::Counters, 3)?,
    })
}

fn decode_conflict(d: &mut Decoder<'_>, pairs: u64) -> Result<Conflict, Error> {
    let (mut who, mut refused) = (None, None);
    for _ in 0..pairs {
        match d.u8()? {
            0 => d.skip()?,
            1 => who = Some(peer(d)?),
            2 => refused = Some(subject(d)?),
            _ => d.skip()?,
        }
    }
    Ok(Conflict {
        peer: required(who, EnforcerMsgType::Conflict, 1)?,
        subject: required(refused, EnforcerMsgType::Conflict, 2)?,
    })
}
