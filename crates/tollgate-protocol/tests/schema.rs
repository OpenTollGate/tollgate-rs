//! The codec against `tollgate.cddl`, the normative schema.
//!
//! Every message the encoder writes must validate, and so must the variants
//! the decoder accepts but the encoder never writes (optional keys left out,
//! unknown keys added). Malformed messages must fail both.

mod common;

use common::{channel, msg_types, pubkey, signature, vectors};
use minicbor::Encoder;
use tollgate_protocol::*;

const SCHEMA: &str = include_str!("../tollgate.cddl");

fn encoded(msg: &Message) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(msg, &mut buf).expect("encode");
    buf
}

fn validate(cbor: &[u8]) -> Result<(), String> {
    cddl::validate_cbor_from_slice(SCHEMA, cbor, None).map_err(|e| e.to_string())
}

#[track_caller]
fn assert_valid(cbor: &[u8], what: &str) {
    if let Err(e) = validate(cbor) {
        panic!("{what} does not validate against tollgate.cddl: {e}");
    }
}

#[track_caller]
fn assert_invalid(cbor: &[u8], what: &str) {
    assert!(
        validate(cbor).is_err(),
        "{what} validated against tollgate.cddl but should not have"
    );
    assert!(decode(cbor).is_err(), "{what} decoded but should not have");
}

/// As [`assert_invalid`], and the validator's complaint names `why` — so the
/// message fails for the reason the test is about, not an incidental one.
#[track_caller]
fn assert_invalid_because(cbor: &[u8], what: &str, why: &str) {
    assert_invalid(cbor, what);
    let err = validate(cbor).unwrap_err();
    assert!(
        err.contains(why),
        "{what} failed validation, but not with {why:?}:\n{err}"
    );
}

/// Build a message map by hand, for shapes our own encoder never writes.
fn map(pairs: u64, fill: impl FnOnce(&mut Encoder<&mut Vec<u8>>)) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut e = Encoder::new(&mut buf);
    e.map(pairs).expect("encode");
    fill(&mut e);
    buf
}

#[test]
fn every_encoded_message_validates() {
    for ty in msg_types() {
        for (i, msg) in vectors(ty).iter().enumerate() {
            let what = format!("{ty:?} vector {i}");
            let buf = encoded(msg);
            assert_valid(&buf, &what);
            assert_eq!(&decode(&buf).expect("decode"), msg, "{what}");
        }
    }
}

#[test]
fn the_schema_covers_every_message_type() {
    // Each known tag appears as a type tag in the schema, and the schema names
    // no tag the codec does not know. Together with `vectors` being an
    // exhaustive match, a new message type cannot land without a schema entry.
    let tags_in_schema: Vec<u8> = SCHEMA
        .lines()
        .map(|l| l.split(';').next().unwrap_or("").trim())
        .filter_map(|l| l.strip_prefix("0: 0x"))
        .map(|hex| u8::from_str_radix(hex.trim_end_matches(','), 16).expect("hex tag"))
        .collect();

    for ty in msg_types() {
        assert!(
            tags_in_schema.contains(&(ty as u8)),
            "{ty:?} (0x{:02X}) has no entry in tollgate.cddl",
            ty as u8
        );
    }
    for tag in &tags_in_schema {
        assert!(
            MsgType::from_u8(*tag).is_some(),
            "tollgate.cddl describes tag 0x{tag:02X}, which the codec does not know"
        );
    }
    assert_eq!(
        tags_in_schema.len(),
        msg_types().len(),
        "a tag appears more than once in tollgate.cddl"
    );
}

#[test]
fn every_reason_code_validates() {
    for code in [
        ReasonCode::MultiplierUnacceptable,
        ReasonCode::MintNotAccepted,
        ReasonCode::UnitNotAccepted,
        ReasonCode::WindowOutOfRange,
        ReasonCode::FundingInvalid,
        ReasonCode::GrantInvalid,
        ReasonCode::RateExceedsCapacity,
        ReasonCode::GrantExceedsChannel,
        ReasonCode::VersionUnsupported,
        ReasonCode::Other,
    ] {
        let msg = Message::Disconnect(Disconnect { reason: code });
        assert_valid(&encoded(&msg), &format!("{code:?}"));
    }
    for reason in [
        CloseReason::Normal,
        CloseReason::PriceRejected,
        CloseReason::PeerLeaving,
    ] {
        let msg = Message::ChannelClose(ChannelClose {
            channel_id: channel(1),
            final_balance: 1,
            final_signature: signature(1),
            reason,
        });
        assert_valid(&encoded(&msg), &format!("{reason:?}"));
    }
}

// ---------------------------------------------------------------------------
// What the decoder accepts but the encoder never writes
// ---------------------------------------------------------------------------

#[test]
fn optional_keys_may_be_left_out() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "Announce without capabilities",
            map(4, |e| {
                e.u8(0).unwrap().u8(MsgType::Announce as u8).unwrap();
                e.u8(1).unwrap().u8(PROTOCOL_VERSION).unwrap();
                e.u8(2).unwrap().bytes(&pubkey(1).0).unwrap();
                e.u8(3).unwrap().str("byte").unwrap();
            }),
        ),
        (
            "Offer without multiplier or no-charge",
            map(4, |e| {
                e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
                e.u8(1).unwrap().array(1).unwrap().str("https://m").unwrap();
                e.u8(2).unwrap().str("byte").unwrap();
                e.u8(3)
                    .unwrap()
                    .array(2)
                    .unwrap()
                    .u32(1)
                    .unwrap()
                    .u32(2)
                    .unwrap();
            }),
        ),
        (
            "Accept without funding",
            map(1, |e| {
                e.u8(0).unwrap().u8(MsgType::Accept as u8).unwrap();
            }),
        ),
        (
            "TopUpReject without a reason",
            map(3, |e| {
                e.u8(0).unwrap().u8(MsgType::TopUpReject as u8).unwrap();
                e.u8(1).unwrap().array(0).unwrap();
                e.u8(2).unwrap().u64(0).unwrap();
            }),
        ),
        (
            "ChannelClose without a reason",
            map(4, |e| {
                e.u8(0).unwrap().u8(MsgType::ChannelClose as u8).unwrap();
                e.u8(1).unwrap().bytes(&channel(1).0).unwrap();
                e.u8(2).unwrap().u64(1).unwrap();
                e.u8(3).unwrap().bytes(&signature(1).0).unwrap();
            }),
        ),
        (
            "Reject without reason or text",
            map(2, |e| {
                e.u8(0).unwrap().u8(MsgType::Reject as u8).unwrap();
                e.u8(1).unwrap().u8(MsgType::Offer as u8).unwrap();
            }),
        ),
        (
            "Disconnect without a reason",
            map(1, |e| {
                e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();
            }),
        ),
    ];
    for (what, buf) in cases {
        assert_valid(&buf, what);
        decode(&buf).unwrap_or_else(|e| panic!("{what} does not decode: {e}"));
    }
}

#[test]
fn an_offer_with_no_charge_false_validates() {
    // Nothing writes it, but it is accepted, and means charging.
    let buf = map(6, |e| {
        e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
        e.u8(1).unwrap().array(1).unwrap().str("https://m").unwrap();
        e.u8(2).unwrap().str("byte").unwrap();
        e.u8(3)
            .unwrap()
            .array(2)
            .unwrap()
            .u32(1)
            .unwrap()
            .u32(2)
            .unwrap();
        e.u8(4).unwrap().u16(0).unwrap();
        e.u8(5).unwrap().bool(false).unwrap();
    });
    assert_valid(&buf, "Offer with key 5 false");
    let Message::Offer(offer) = decode(&buf).expect("decode") else {
        panic!("not an offer");
    };
    assert!(!offer.no_charge);
}

#[test]
fn unknown_keys_and_any_key_order_validate() {
    // Key 9 is from a later minor version; the type tag arrives last.
    let buf = map(3, |e| {
        e.u8(9).unwrap().str("from the future").unwrap();
        e.u8(1).unwrap().bytes(&channel(1).0).unwrap();
        e.u8(0).unwrap().u8(MsgType::ChannelReady as u8).unwrap();
    });
    assert_valid(&buf, "ChannelReady with an unknown key");
    decode(&buf).expect("decode");

    // The highest key the decoder can skip.
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();
        e.u8(1).unwrap().u8(ReasonCode::Other as u8).unwrap();
        e.u8(255).unwrap().array(0).unwrap();
    });
    assert_valid(&buf, "Disconnect with key 255");
    decode(&buf).expect("decode");
}

// ---------------------------------------------------------------------------
// Malformed messages fail validation as well as decoding
// ---------------------------------------------------------------------------

#[test]
fn a_missing_required_key_is_invalid() {
    // Announce without its pubkey.
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(MsgType::Announce as u8).unwrap();
        e.u8(1).unwrap().u8(PROTOCOL_VERSION).unwrap();
        e.u8(3).unwrap().str("byte").unwrap();
    });
    assert_invalid_because(&buf, "Announce without key 2", "missing key: 2");

    // RolloverInit without its funding, which unlike Accept's is required.
    let buf = map(2, |e| {
        e.u8(0).unwrap().u8(MsgType::RolloverInit as u8).unwrap();
        e.u8(1).unwrap().bytes(&channel(1).0).unwrap();
    });
    assert_invalid_because(&buf, "RolloverInit without key 2", "missing key: 2");
}

#[test]
fn a_wrong_length_signature_is_invalid() {
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(MsgType::TopUp as u8).unwrap();
        e.u8(1).unwrap().array(1).unwrap().array(3).unwrap();
        e.bytes(&channel(1).0).unwrap().u64(1).unwrap();
        e.bytes(&[7; 63]).unwrap();
        e.u8(2).unwrap().u32(1_000).unwrap();
    });
    assert_invalid_because(&buf, "TopUp with a 63-byte signature", ".size 64, got 63");
}

#[test]
fn a_wrong_length_pubkey_is_invalid() {
    let buf = map(5, |e| {
        e.u8(0).unwrap().u8(MsgType::Announce as u8).unwrap();
        e.u8(1).unwrap().u8(PROTOCOL_VERSION).unwrap();
        e.u8(2).unwrap().bytes(&[2; 32]).unwrap();
        e.u8(3).unwrap().str("byte").unwrap();
        e.u8(4).unwrap().u32(0).unwrap();
    });
    assert_invalid_because(&buf, "Announce with a 32-byte pubkey", ".size 33, got 32");
}

#[test]
fn an_unknown_type_tag_is_invalid() {
    let buf = map(2, |e| {
        e.u8(0).unwrap().u8(0x0C).unwrap();
        e.u8(1).unwrap().u8(ReasonCode::Other as u8).unwrap();
    });
    assert_invalid_because(&buf, "a message tagged 0x0C", "got Integer(12)");
}

#[test]
fn array_bounds_are_enforced() {
    // An empty mint list, and a TopUp with no updates or too many.
    let mut empty_offer = vectors(MsgType::Offer)[1].clone();
    if let Message::Offer(o) = &mut empty_offer {
        o.accepted_mints.clear();
    }
    assert_invalid(&encoded(&empty_offer), "Offer with no mints");

    let empty_topup = Message::TopUp(TopUp {
        updates: vec![],
        window_ms: 1_000,
    });
    assert_invalid(&encoded(&empty_topup), "TopUp with no updates");

    let long_topup = Message::TopUp(TopUp {
        updates: (0..=MAX_CHANNEL_UPDATES as u8)
            .map(|i| ChannelUpdate {
                channel_id: channel(i),
                cumulative: 1,
                signature: signature(1),
            })
            .collect(),
        window_ms: 1_000,
    });
    assert_invalid(&encoded(&long_topup), "TopUp with too many updates");
}

#[test]
fn a_known_key_with_the_wrong_type_is_not_an_unknown_key() {
    // The catch-all must not absorb a known key: `5: bool` is a cut, so a text
    // value there is an error rather than an extension.
    let buf = map(6, |e| {
        e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
        e.u8(1).unwrap().array(1).unwrap().str("https://m").unwrap();
        e.u8(2).unwrap().str("byte").unwrap();
        e.u8(3)
            .unwrap()
            .array(2)
            .unwrap()
            .u32(1)
            .unwrap()
            .u32(2)
            .unwrap();
        e.u8(4).unwrap().u16(0).unwrap();
        e.u8(5).unwrap().str("yes").unwrap();
    });
    assert_invalid_because(&buf, "Offer with key 5 as text", "expected type bool");
}
