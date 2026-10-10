//! Every message type survives an encode/decode round trip, and the framing
//! reassembles messages regardless of how the byte stream was chopped up.

mod common;

use common::{all_messages, channel, pubkey, signature};
use tollgate_protocol::*;

#[test]
fn every_message_round_trips() {
    for msg in all_messages() {
        let mut buf = Vec::new();
        encode(&msg, &mut buf).expect("encode");
        let back = decode(&buf).unwrap_or_else(|e| panic!("decode {:?}: {e}", msg.msg_type()));
        assert_eq!(msg, back, "round trip changed {:?}", msg.msg_type());
    }
}

#[test]
fn reject_with_no_text_round_trips() {
    // The null branch of field 3 is a separate path from the string branch.
    let msg = Message::Reject(Reject {
        rejected_type: MsgType::Offer as u8,
        reason: ReasonCode::MintNotAccepted,
        text: None,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf).expect("decode"), msg);
}

#[test]
fn messages_stay_within_their_size_estimates() {
    // The design doc quotes per-message sizes; a large regression here means
    // the encoding drifted from what constrained devices were promised.
    let announce = Message::Announce(Announce {
        version: PROTOCOL_VERSION,
        pubkey: pubkey(1),
        unit: "byte".into(),
        capabilities: 0,
    });
    let mut buf = Vec::new();
    encode(&announce, &mut buf).expect("encode");
    assert!(buf.len() <= 60, "announce grew to {} bytes", buf.len());

    let topup = Message::TopUp(TopUp {
        updates: vec![ChannelUpdate {
            channel_id: channel(3),
            cumulative: u64::MAX,
            signature: signature(4),
        }],
        window_ms: 5_000,
        reserved_rate: 625_000,
    });
    buf.clear();
    encode(&topup, &mut buf).expect("encode");
    assert!(buf.len() <= 140, "topup grew to {} bytes", buf.len());

    // ~25 bytes; a month to go and a large budget is the long end.
    let balance = Message::Balance(Balance {
        remaining: 1_000_000_000,
        expires_in_ms: 2_592_000_000,
        reserved_rate: 625_000,
    });
    buf.clear();
    encode(&balance, &mut buf).expect("encode");
    assert!(buf.len() <= 30, "balance grew to {} bytes", buf.len());
}

#[test]
fn an_offer_without_a_weight_charges_both_directions_alike() {
    // Key 4 is always written, but a peer that leaves it out means 1, not 0:
    // what the payer sends is never free unless the provider says so.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(4).unwrap();
    e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
    e.u8(1).unwrap().array(1).unwrap();
    e.str("https://hub.example/mint").unwrap();
    e.u8(2).unwrap().str("byte").unwrap();
    e.u8(3)
        .unwrap()
        .array(2)
        .unwrap()
        .u64(200)
        .unwrap()
        .u64(30_000)
        .unwrap();
    let Message::Offer(offer) = decode(&buf).expect("decode") else {
        panic!("not an offer");
    };
    assert_eq!(offer.from_payer_weight, 1);
}

#[test]
fn a_window_of_a_year_fits() {
    let year = 31_536_000_000;
    let mut o = offer(false);
    o.max_window_ms = year;
    let mut buf = Vec::new();
    encode(&Message::Offer(o.clone()), &mut buf).expect("encode");
    assert_eq!(decode(&buf).expect("decode"), Message::Offer(o));
}

#[test]
fn frame_reader_reassembles_across_arbitrary_chunk_boundaries() {
    let msgs = all_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        encode_frame(m, &mut stream).expect("encode_frame");
    }

    // One byte at a time is the worst case a TCP reader can hand us.
    let mut reader = FrameReader::new();
    let mut out = Vec::new();
    for byte in &stream {
        reader.push(&[*byte]);
        while let Some(m) = reader.next_message() {
            out.push(m.expect("decode"));
        }
    }

    assert_eq!(out, msgs);
    assert_eq!(
        reader.pending(),
        0,
        "reader held bytes after the last frame"
    );
}

#[test]
fn frame_reader_yields_several_messages_from_one_push() {
    let msgs = all_messages();
    let mut stream = Vec::new();
    for m in &msgs {
        encode_frame(m, &mut stream).expect("encode_frame");
    }

    let mut reader = FrameReader::new();
    reader.push(&stream);
    let mut out = Vec::new();
    while let Some(m) = reader.next_message() {
        out.push(m.expect("decode"));
    }
    assert_eq!(out, msgs);
}

#[test]
fn a_corrupt_frame_does_not_desync_the_ones_behind_it() {
    let mut stream = Vec::new();
    // A frame whose body is not valid CBOR at all.
    stream.extend_from_slice(&3u16.to_le_bytes());
    stream.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    let good = Message::Disconnect(Disconnect {
        reason: ReasonCode::Other,
    });
    encode_frame(&good, &mut stream).expect("encode_frame");

    let mut reader = FrameReader::new();
    reader.push(&stream);
    assert!(reader.next_message().expect("a frame").is_err());
    assert_eq!(
        reader.next_message().expect("a frame").expect("decode"),
        good
    );
}

#[test]
fn an_offer_with_no_mints_is_malformed() {
    // "A node that will take no payment has nothing to offer" — the encoder
    // will happily write it, so the decoder is where this has to be caught.
    let msg = Message::Offer(Offer {
        accepted_mints: vec![],
        unit: "byte".into(),
        min_window_ms: 200,
        max_window_ms: 30_000,
        from_payer_weight: 1,
        no_charge: false,
        min_reserved_rate: 0,
        min_topup_gap_ms: 200,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::EmptyMintList));
}

fn offer(no_charge: bool) -> Offer {
    Offer {
        accepted_mints: vec!["https://hub.example/mint".into()],
        unit: "byte".into(),
        min_window_ms: 200,
        max_window_ms: 30_000,
        from_payer_weight: 0,
        no_charge,
        // What a peer that writes neither key 6 nor 7 is read as.
        min_reserved_rate: 0,
        min_topup_gap_ms: 0,
    }
}

#[test]
fn an_offer_that_charges_does_not_carry_the_no_charge_key() {
    // Absent is the default, so an ordinary Offer encodes exactly as it did
    // without key 5 — seven pairs, and no `5` among them.
    let mut buf = Vec::new();
    encode(&Message::Offer(offer(false)), &mut buf).expect("encode");
    let mut d = minicbor::Decoder::new(&buf);
    assert_eq!(d.map().unwrap(), Some(7));

    buf.clear();
    encode(&Message::Offer(offer(true)), &mut buf).expect("encode");
    let mut d = minicbor::Decoder::new(&buf);
    assert_eq!(d.map().unwrap(), Some(8));
}

#[test]
fn an_offer_without_key_5_decodes_as_charging() {
    // Built by hand: the Offer of a peer that predates the field.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(5).unwrap();
    e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
    e.u8(1).unwrap().array(1).unwrap();
    e.str("https://hub.example/mint").unwrap();
    e.u8(2).unwrap().str("byte").unwrap();
    e.u8(3)
        .unwrap()
        .array(2)
        .unwrap()
        .u32(200)
        .unwrap()
        .u32(30_000)
        .unwrap();
    e.u8(4).unwrap().u16(0).unwrap();

    assert_eq!(decode(&buf).expect("decode"), Message::Offer(offer(false)));
}

#[test]
fn an_offer_with_key_5_false_decodes_as_charging() {
    // Nothing writes it, but a peer that does means the same as leaving it out.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(6).unwrap();
    e.u8(0).unwrap().u8(MsgType::Offer as u8).unwrap();
    e.u8(1).unwrap().array(1).unwrap();
    e.str("https://hub.example/mint").unwrap();
    e.u8(2).unwrap().str("byte").unwrap();
    e.u8(3)
        .unwrap()
        .array(2)
        .unwrap()
        .u32(200)
        .unwrap()
        .u32(30_000)
        .unwrap();
    e.u8(4).unwrap().u16(0).unwrap();
    e.u8(5).unwrap().bool(false).unwrap();

    assert_eq!(decode(&buf).expect("decode"), Message::Offer(offer(false)));
}

#[test]
fn a_no_charge_offer_still_needs_a_mint() {
    // Not charging this peer says nothing about what the node takes from
    // anyone else, so the list stays mandatory.
    let mut msg = offer(true);
    msg.accepted_mints.clear();
    let mut buf = Vec::new();
    encode(&Message::Offer(msg), &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::EmptyMintList));
}

#[test]
fn unknown_fields_are_skipped_rather_than_rejected() {
    // A later minor version adding key 9 must not break a v1 peer. Built by
    // hand because our own encoder never emits an unknown key.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(3).unwrap();
    e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();
    e.u8(1)
        .unwrap()
        .u8(ReasonCode::MintNotAccepted as u8)
        .unwrap();
    e.u8(9).unwrap().str("from the future").unwrap();

    assert_eq!(
        decode(&buf).expect("decode"),
        Message::Disconnect(Disconnect {
            reason: ReasonCode::MintNotAccepted
        })
    );
}

#[test]
fn the_type_field_may_arrive_last() {
    // Field order is not guaranteed across implementations, so the decoder
    // scans for key 0 rather than assuming it comes first.
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(2).unwrap();
    e.u8(1)
        .unwrap()
        .u8(ReasonCode::VersionUnsupported as u8)
        .unwrap();
    e.u8(0).unwrap().u8(MsgType::Disconnect as u8).unwrap();

    assert_eq!(
        decode(&buf).expect("decode"),
        Message::Disconnect(Disconnect {
            reason: ReasonCode::VersionUnsupported
        })
    );
}

#[test]
fn a_wrong_length_channel_id_is_rejected() {
    let mut buf = Vec::new();
    let mut e = minicbor::Encoder::new(&mut buf);
    e.map(2).unwrap();
    e.u8(0).unwrap().u8(MsgType::ChannelReady as u8).unwrap();
    e.u8(1).unwrap().bytes(&[0u8; 16]).unwrap();

    assert_eq!(
        decode(&buf),
        Err(Error::BadLength {
            key: 1,
            expected: 32,
            got: 16
        })
    );
}

#[test]
fn only_capacity_and_timing_refusals_are_unavoidable_from_the_offer() {
    // The Offer advertises mints, unit, window range and the smallest reserved
    // rate, so a peer that read it has no excuse for tripping any of those. It
    // carries no rate ceiling — there is no honest static number, since what is
    // free depends on what is promised to every other payer — so being refused
    // on capacity is how a payer discovers the limit rather than a fault. And
    // two TopUps sent a gap apart can still arrive closer than that.
    assert!(!ReasonCode::RateExceedsCapacity.avoidable_from_offer());
    assert!(!ReasonCode::TooSoon.avoidable_from_offer());

    for reason in [
        ReasonCode::FromPayerWeightUnacceptable,
        ReasonCode::MintNotAccepted,
        ReasonCode::UnitNotAccepted,
        ReasonCode::OutOfRange,
        ReasonCode::FundingInvalid,
        ReasonCode::GrantInvalid,
        ReasonCode::GrantExceedsChannel,
        ReasonCode::VersionUnsupported,
        ReasonCode::Other,
    ] {
        assert!(
            reason.avoidable_from_offer(),
            "{reason:?} should be worth an operator's attention"
        );
    }
}

#[test]
fn a_topup_with_no_updates_is_malformed() {
    // A purchase that ratchets nothing buys nothing.
    let msg = Message::TopUp(TopUp {
        updates: vec![],
        window_ms: 1_000,
        reserved_rate: 0,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::NoChannelUpdates));
}

#[test]
fn a_topup_with_too_many_updates_is_refused_before_allocating() {
    // Each update is a signature verification and `min_topup_gap_ms` only bounds
    // how often a TopUp arrives, so an unbounded array would multiply straight
    // through that budget.
    let over = MAX_CHANNEL_UPDATES + 1;
    let msg = Message::TopUp(TopUp {
        updates: (0..over)
            .map(|i| ChannelUpdate {
                channel_id: channel(i as u8),
                cumulative: 1_000 + i as u64,
                signature: signature(1),
            })
            .collect(),
        window_ms: 1_000,
        reserved_rate: 0,
    });
    let mut buf = Vec::new();
    encode(&msg, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::TooManyChannelUpdates(over)));
}

#[test]
fn a_full_length_topup_still_fits_a_frame_comfortably() {
    // The cap has to be affordable on the wire as well as in verification time.
    let msg = Message::TopUp(TopUp {
        updates: (0..MAX_CHANNEL_UPDATES)
            .map(|i| ChannelUpdate {
                channel_id: channel(i as u8),
                cumulative: u64::MAX,
                signature: signature(1),
            })
            .collect(),
        window_ms: 30_000,
        reserved_rate: u64::MAX,
    });
    let mut buf = Vec::new();
    encode_frame(&msg, &mut buf).expect("encode");
    assert!(
        buf.len() < 1_024,
        "a full top-up reached {} bytes",
        buf.len()
    );
}
