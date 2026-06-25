//! Wire messages and the identifiers that appear in them.

use alloc::string::String;
use alloc::vec::Vec;

use minicbor::bytes::{ByteArray, ByteVec};
use minicbor::{Decode, Encode};

use crate::product::{MintPrice, option_id, product_id};

/// A peer's identity: the raw secp256k1 *compressed* public key (33 bytes).
///
/// Everything in the protocol and core works with these raw bytes. `npub` is
/// only the human-readable Bech32 rendering of the same key and lives at the
/// edges (config, logs, UI) — never on the wire.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PublicKey([u8; 33]);

impl PublicKey {
    pub const LEN: usize = 33;

    pub const fn from_bytes(bytes: [u8; 33]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 33] {
        &self.0
    }
}

/// The 15 TollGate message types (see `docs/design/core/tollgate-protocol.md`).
/// The discriminant is the value stored at CBOR field key `0`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MessageType {
    Announce = 0x00,
    PriceSheet = 0x01,
    Accept = 0x02,
    ChannelReady = 0x03,
    MeteringReport = 0x04,
    BalanceUpdate = 0x05,
    BalanceAck = 0x06,
    BootstrapToken = 0x07,
    BootstrapAck = 0x08,
    RolloverInit = 0x09,
    RolloverReady = 0x0A,
    ChannelClose = 0x0B,
    CloseAck = 0x0C,
    Reject = 0x0D,
    Disconnect = 0x0E,
}

impl MessageType {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Announce,
            0x01 => Self::PriceSheet,
            0x02 => Self::Accept,
            0x03 => Self::ChannelReady,
            0x04 => Self::MeteringReport,
            0x05 => Self::BalanceUpdate,
            0x06 => Self::BalanceAck,
            0x07 => Self::BootstrapToken,
            0x08 => Self::BootstrapAck,
            0x09 => Self::RolloverInit,
            0x0A => Self::RolloverReady,
            0x0B => Self::ChannelClose,
            0x0C => Self::CloseAck,
            0x0D => Self::Reject,
            0x0E => Self::Disconnect,
            _ => return None,
        })
    }
}

/// [`MessageType::MeteringReport`] (0x04): cumulative, **unsigned** resource
/// stats exchanged each interval so both sides compute the same cost. Counters
/// are cumulative since the session baseline; no sequence number is needed (the
/// protocol is self-healing — a lost report is corrected by the next one's
/// totals).
///
/// Key 5 (`new_pricing`, the updated pricing array used for price renegotiation)
/// is reserved until the PriceSheet pricing types exist; only `new_product_id`
/// (key 4) is carried for now.
#[derive(Clone, PartialEq, Eq, Debug, Encode, Decode)]
#[cbor(map)]
pub struct MeteringReport {
    #[n(0)]
    pub type_tag: u8,
    /// Milliseconds since session start (cumulative).
    #[n(1)]
    pub elapsed_ms: u64,
    /// Cumulative units delivered TO the peer since session start.
    #[n(2)]
    pub delivered: u64,
    /// Cumulative units received FROM the peer since session start.
    #[n(3)]
    pub received: u64,
    /// Updated product id for the next interval, if the provider is changing
    /// price (renegotiation); `None` otherwise.
    #[n(4)]
    pub new_product_id: Option<ByteArray<32>>,
}

impl MeteringReport {
    pub fn new(elapsed_ms: u64, delivered: u64, received: u64) -> Self {
        Self {
            type_tag: MessageType::MeteringReport.as_u8(),
            elapsed_ms,
            delivered,
            received,
            new_product_id: None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("MeteringReport encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// Current TollGate protocol version, carried in [`Announce`] field 1.
pub const PROTOCOL_VERSION: u8 = 1;

/// Capability bit: peer can fund and sign Spilman channels. If unset in an
/// [`Announce`], the peer is bootstrap-only.
pub const CAP_SPILMAN: u32 = 0x01;

/// [`MessageType::Announce`] (0x00): the first message each peer sends. It
/// establishes the sender's identity (pubkey) and declares protocol version,
/// resource unit, and capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Announce {
    #[n(0)]
    pub type_tag: u8,
    #[n(1)]
    pub version: u8,
    #[n(2)]
    pub pubkey: ByteArray<33>,
    #[n(3)]
    pub unit: String,
    #[n(4)]
    pub capabilities: u32,
}

impl Announce {
    pub fn new(version: u8, pubkey: PublicKey, unit: impl Into<String>, capabilities: u32) -> Self {
        Self {
            type_tag: MessageType::Announce.as_u8(),
            version,
            pubkey: ByteArray::from(*pubkey.as_bytes()),
            unit: unit.into(),
            capabilities,
        }
    }

    /// The sender's public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey::from_bytes(*self.pubkey)
    }

    /// Whether the peer advertises Spilman capability.
    pub fn supports_spilman(&self) -> bool {
        self.capabilities & CAP_SPILMAN != 0
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("Announce encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// [`MessageType::BootstrapToken`] (0x07): a raw Cashu token, sent when a peer
/// cannot reach a mint and pays to get online.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct BootstrapToken {
    #[n(0)]
    pub type_tag: u8,
    #[n(1)]
    pub token: ByteVec,
}

impl BootstrapToken {
    pub fn new(token: Vec<u8>) -> Self {
        Self {
            type_tag: MessageType::BootstrapToken.as_u8(),
            token: ByteVec::from(token),
        }
    }

    /// The raw token bytes (typically the UTF-8 of a `cashuB…` string).
    pub fn token_bytes(&self) -> Vec<u8> {
        self.token.to_vec()
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("BootstrapToken encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// [`MessageType::BootstrapAck`] (0x08): the provider's response to a
/// [`BootstrapToken`], sent only after verifying it with the mint.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct BootstrapAck {
    #[n(0)]
    pub type_tag: u8,
    #[n(1)]
    pub status: u8,
    #[n(2)]
    pub reason: Option<String>,
}

impl BootstrapAck {
    pub const STATUS_ACCEPTED: u8 = 0;
    pub const STATUS_REJECTED: u8 = 1;

    pub fn accepted() -> Self {
        Self {
            type_tag: MessageType::BootstrapAck.as_u8(),
            status: Self::STATUS_ACCEPTED,
            reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            type_tag: MessageType::BootstrapAck.as_u8(),
            status: Self::STATUS_REJECTED,
            reason: Some(reason.into()),
        }
    }

    pub fn is_accepted(&self) -> bool {
        self.status == Self::STATUS_ACCEPTED
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("BootstrapAck encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// [`MessageType::Reject`] (0x0D): the provider tells a peer why service was
/// refused or stopped. The most common cause in bootstrap-only mode is the
/// balance running out ([`Reject::CODE_BALANCE_EXHAUSTED`]); the peer reacts by
/// sending another [`BootstrapToken`] to top up.
///
/// In the HTTP-polling transport this is queued and delivered on the peer's next
/// exchange; over WebSocket it is pushed immediately.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct Reject {
    #[n(0)]
    pub type_tag: u8,
    /// Machine-readable reason code (see the `CODE_*` constants).
    #[n(1)]
    pub code: u8,
    /// Optional human-readable detail.
    #[n(2)]
    pub reason: Option<String>,
}

impl Reject {
    /// Metering measurements diverged beyond the configured tolerance (transit
    /// loss). Reason code per `docs/design/core/tollgate-protocol.md`.
    pub const CODE_TRANSIT_LOSS: u8 = 0x07;
    /// Balance exhausted — the peer must top up (or upgrade to Spilman) to
    /// resume. See `docs/design/core/tollgate-bootstrap.md`.
    pub const CODE_BALANCE_EXHAUSTED: u8 = 0x09;

    pub fn new(code: u8, reason: Option<String>) -> Self {
        Self {
            type_tag: MessageType::Reject.as_u8(),
            code,
            reason,
        }
    }

    /// A balance-exhausted rejection.
    pub fn balance_exhausted() -> Self {
        Self::new(Self::CODE_BALANCE_EXHAUSTED, None)
    }

    /// A transit-loss-tolerance-exceeded warning: our metering and the peer's
    /// disagree by more than the tolerance. Billing still proceeds (on the higher
    /// value); this tells the peer to investigate before the relationship is cut.
    pub fn transit_loss() -> Self {
        Self::new(Self::CODE_TRANSIT_LOSS, None)
    }

    /// Whether this rejection is the balance-exhausted signal.
    pub fn is_balance_exhausted(&self) -> bool {
        self.code == Self::CODE_BALANCE_EXHAUSTED
    }

    /// Whether this rejection is the transit-loss warning.
    pub fn is_transit_loss(&self) -> bool {
        self.code == Self::CODE_TRANSIT_LOSS
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("Reject encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

/// One mint option inside a [`ProductOffer`]: a mint URL and the price charged
/// when paying through it. `option_id` is the canonical reference an [`Accept`]
/// uses to name the chosen option unambiguously. Nested object — no type tag.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct MintOption {
    #[n(1)]
    pub option_id: ByteArray<32>,
    #[n(2)]
    pub mint_url: String,
    #[n(3)]
    pub price_per_second: i64,
    #[n(4)]
    pub price_per_unit: i64,
    #[n(5)]
    pub mint_unit: String,
}

impl MintOption {
    /// Build a wire option from a [`MintPrice`], computing its `option_id`.
    pub fn from_price(price: &MintPrice) -> Self {
        Self {
            option_id: ByteArray::from(option_id(&price.mint_url, &price.mint_unit)),
            mint_url: price.mint_url.clone(),
            price_per_second: price.price_per_second,
            price_per_unit: price.price_per_unit,
            mint_unit: price.mint_unit.clone(),
        }
    }
}

/// One product offering inside a [`PriceSheet`]: a priced bundle across one or
/// more mints, identified by its canonical [`product_id`]. Nested object — the
/// map starts at field 1 and carries no type tag.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct ProductOffer {
    #[n(1)]
    pub product_id: ByteArray<32>,
    /// Opaque, implementation-defined extension bytes, hashed into the id.
    #[n(2)]
    pub extensions: ByteVec,
    #[n(3)]
    pub pricing_scale: u32,
    #[n(4)]
    pub mints: Vec<MintOption>,
}

impl ProductOffer {
    /// Build an offer from per-mint prices, computing the canonical `product_id`
    /// and each mint's `option_id` so the offer is self-describing on the wire.
    pub fn new(pricing_scale: u32, prices: &[MintPrice], extensions: Vec<u8>) -> Self {
        let pid = product_id(pricing_scale, prices, &extensions);
        Self {
            product_id: ByteArray::from(pid.0),
            extensions: ByteVec::from(extensions),
            pricing_scale,
            mints: prices.iter().map(MintOption::from_price).collect(),
        }
    }
}

/// [`MessageType::PriceSheet`] (0x01): a peer's "take it or leave it" offer, sent
/// after [`Announce`]. Carries product offerings and the metering interval range
/// this peer will accept; the other side picks one product + mint option (and,
/// on the Spilman path, replies with an Accept). In bootstrap-only mode the
/// client just reads it to learn the price and which mints are accepted, then
/// sends a [`BootstrapToken`]. See `docs/design/core/tollgate-protocol.md`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[cbor(map)]
pub struct PriceSheet {
    #[n(0)]
    pub type_tag: u8,
    #[n(1)]
    pub products: Vec<ProductOffer>,
    /// `(min_interval_ms, max_interval_ms)` — the acceptable metering interval
    /// range (CBOR array `[min, max]`).
    #[n(2)]
    pub interval_ms: (u32, u32),
}

impl PriceSheet {
    pub fn new(products: Vec<ProductOffer>, min_interval_ms: u32, max_interval_ms: u32) -> Self {
        Self {
            type_tag: MessageType::PriceSheet.as_u8(),
            products,
            interval_ms: (min_interval_ms, max_interval_ms),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        minicbor::to_vec(self).expect("PriceSheet encodes infallibly")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, minicbor::decode::Error> {
        minicbor::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_round_trips() {
        for raw in 0x00u8..=0x0E {
            let ty = MessageType::from_u8(raw).expect("known type");
            assert_eq!(ty.as_u8(), raw);
        }
        assert!(MessageType::from_u8(0x0F).is_none());
        assert!(MessageType::from_u8(0xFF).is_none());
    }

    #[test]
    fn metering_report_cbor_round_trips() {
        let report = MeteringReport::new(5000, 100, 40);
        let back = MeteringReport::decode(&report.encode()).expect("decode");
        assert_eq!(report, back);
        assert_eq!(back.type_tag, MessageType::MeteringReport.as_u8());
        assert_eq!(back.elapsed_ms, 5000);
        assert_eq!(back.new_product_id, None);
    }

    #[test]
    fn metering_report_carries_renegotiated_product_id() {
        let mut report = MeteringReport::new(1000, 1, 2);
        report.new_product_id = Some(ByteArray::from([9u8; 32]));
        let back = MeteringReport::decode(&report.encode()).expect("decode");
        assert_eq!(back.new_product_id, Some(ByteArray::from([9u8; 32])));
    }

    #[test]
    fn announce_round_trips_and_exposes_pubkey() {
        let pk = PublicKey::from_bytes([7u8; 33]);
        let announce = Announce::new(1, pk, "bytes", CAP_SPILMAN);
        let bytes = announce.encode();
        let back = Announce::decode(&bytes).expect("decode");
        assert_eq!(announce, back);
        assert_eq!(back.public_key(), pk);
        assert_eq!(back.type_tag, MessageType::Announce.as_u8());
        assert!(back.supports_spilman());
    }

    #[test]
    fn announce_without_spilman_capability() {
        let announce = Announce::new(1, PublicKey::from_bytes([1u8; 33]), "wh", 0);
        assert!(!announce.supports_spilman());
    }

    #[test]
    fn bootstrap_token_round_trips() {
        let token = BootstrapToken::new(b"cashuBsometoken".to_vec());
        let bytes = token.encode();
        let back = BootstrapToken::decode(&bytes).expect("decode");
        assert_eq!(back.token_bytes(), b"cashuBsometoken");
        assert_eq!(back.type_tag, MessageType::BootstrapToken.as_u8());
    }

    #[test]
    fn price_sheet_round_trips_with_products_and_mints() {
        use crate::product::MintPrice;
        use alloc::string::ToString;

        let prices = alloc::vec![
            MintPrice {
                mint_url: "https://mint-a.example".to_string(),
                price_per_second: 0,
                price_per_unit: 1,
                mint_unit: "sat".to_string(),
            },
            MintPrice {
                mint_url: "https://mint-b.example".to_string(),
                price_per_second: 2,
                price_per_unit: 3,
                mint_unit: "sat".to_string(),
            },
        ];
        let offer = ProductOffer::new(1000, &prices, alloc::vec![]);
        let sheet = PriceSheet::new(alloc::vec![offer], 5000, 60000);

        let back = PriceSheet::decode(&sheet.encode()).expect("decode PriceSheet");
        assert_eq!(sheet, back);
        assert_eq!(back.type_tag, MessageType::PriceSheet.as_u8());
        assert_eq!(back.interval_ms, (5000, 60000));
        assert_eq!(back.products.len(), 1);
        assert_eq!(back.products[0].mints.len(), 2);

        // The wire option_id matches the canonical helper, and product_id is the
        // declaration-order-independent fingerprint of the same prices.
        assert_eq!(
            back.products[0].mints[0].option_id.as_slice(),
            &option_id("https://mint-a.example", "sat")
        );
        assert_eq!(
            back.products[0].product_id.as_slice(),
            &product_id(1000, &prices, b"").0
        );

        // peek_type identifies it without a full decode.
        assert_eq!(
            crate::peek_type(&sheet.encode()),
            Some(MessageType::PriceSheet)
        );
    }

    #[test]
    fn price_sheet_round_trips_when_empty_or_mintless() {
        // No products at all (e.g. a pure client that sells nothing).
        let empty = PriceSheet::new(alloc::vec![], 1000, 2000);
        assert_eq!(PriceSheet::decode(&empty.encode()).expect("decode"), empty);
        assert!(empty.products.is_empty());

        // A product with zero mint options (a gateway with no accepted mints).
        let mintless = PriceSheet::new(
            alloc::vec![ProductOffer::new(1000, &[], alloc::vec![])],
            5000,
            60000,
        );
        let back = PriceSheet::decode(&mintless.encode()).expect("decode");
        assert_eq!(back, mintless);
        assert_eq!(back.products.len(), 1);
        assert!(back.products[0].mints.is_empty());
    }

    #[test]
    fn reject_round_trips_and_flags_balance_exhausted() {
        let reject = Reject::balance_exhausted();
        assert_eq!(reject.type_tag, MessageType::Reject.as_u8());
        assert!(reject.is_balance_exhausted());

        let back = Reject::decode(&reject.encode()).expect("decode");
        assert_eq!(reject, back);
        assert_eq!(back.code, Reject::CODE_BALANCE_EXHAUSTED);
        assert_eq!(back.reason, None);

        let other = Reject::new(0x01, Some("mint unreachable".into()));
        assert!(!other.is_balance_exhausted());
        let back = Reject::decode(&other.encode()).expect("decode");
        assert_eq!(back.reason.as_deref(), Some("mint unreachable"));
    }

    #[test]
    fn reject_round_trips_and_flags_transit_loss() {
        let reject = Reject::transit_loss();
        assert_eq!(reject.code, Reject::CODE_TRANSIT_LOSS);
        assert!(reject.is_transit_loss());
        assert!(!reject.is_balance_exhausted());
        let back = Reject::decode(&reject.encode()).expect("decode");
        assert_eq!(reject, back);
    }

    #[test]
    fn bootstrap_ack_round_trips() {
        let accepted = BootstrapAck::accepted();
        let back = BootstrapAck::decode(&accepted.encode()).expect("decode");
        assert!(back.is_accepted());
        assert_eq!(back.reason, None);

        let rejected = BootstrapAck::rejected("mint unreachable").encode();
        let back = BootstrapAck::decode(&rejected).expect("decode");
        assert!(!back.is_accepted());
        assert_eq!(back.reason.as_deref(), Some("mint unreachable"));
    }
}
