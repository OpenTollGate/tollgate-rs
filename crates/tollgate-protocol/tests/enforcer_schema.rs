//! The enforcer codec against `enforcer.cddl`, its normative schema, and round
//! trips through the framing.
//!
//! Every message the encoder writes must validate and decode back to itself,
//! and so must the variants the decoder accepts but the encoder never writes.
//! Malformed messages must fail both.

use minicbor::Encoder;
use tollgate_protocol::enforcer::*;
use tollgate_protocol::{FrameReader, PubKey};

const SCHEMA: &str = include_str!("../enforcer.cddl");

fn payer(seed: u8) -> PubKey {
    let mut b = [seed; 33];
    b[0] = 0x02;
    PubKey(b)
}

fn subject(bytes: &[u8]) -> Subject {
    Subject::new(bytes).expect("a subject")
}

/// One of each form: a key, an address, the shortest and the longest
/// delegated subject.
fn every_subject() -> Vec<Subject> {
    vec![
        Subject::pubkey(&payer(0x3b)),
        Subject::address("192.168.1.23".parse().unwrap()),
        subject(b"tap-3"),
        subject(&[7; MAX_SUBJECT_LEN]),
        subject(&[1]),
    ]
}

/// Every tag the enforcer protocol defines, in order.
fn msg_types() -> Vec<EnforcerMsgType> {
    (0..=u8::MAX).filter_map(EnforcerMsgType::from_u8).collect()
}

/// A fully populated and a minimal instance of each message type. Exhaustive
/// on purpose: a new tag does not compile until it has vectors, and then fails
/// the schema test until `enforcer.cddl` describes it.
fn vectors(ty: EnforcerMsgType) -> Vec<EnforcerMessage> {
    match ty {
        EnforcerMsgType::Hello => vec![
            EnforcerMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                identity: Identity::Pubkey,
                delegated: true,
                unit: "byte".into(),
            }),
            EnforcerMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                identity: Identity::Address,
                delegated: false,
                unit: "ml".into(),
            }),
            EnforcerMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                identity: Identity::Address,
                delegated: false,
                unit: String::new(),
            }),
        ],
        EnforcerMsgType::Bind => vec![
            EnforcerMessage::Bind(Bind {
                peer: payer(1),
                bindings: every_subject()
                    .into_iter()
                    .chain([subject(b"tap-4"), subject(&[2; 16]), subject(&[3; 32])])
                    .enumerate()
                    .map(|(i, subject)| Binding {
                        subject,
                        delegated: i % 2 == 1,
                    })
                    .collect(),
            }),
            EnforcerMessage::Bind(Bind {
                peer: payer(2),
                bindings: vec![],
            }),
        ],
        EnforcerMsgType::Set => vec![
            EnforcerMessage::Set(Set {
                peer: payer(1),
                rate: Some(u64::MAX - 1),
            }),
            EnforcerMessage::Set(Set {
                peer: payer(1),
                rate: Some(0),
            }),
            EnforcerMessage::Set(Set {
                peer: payer(2),
                rate: None,
            }),
        ],
        EnforcerMsgType::Remove => vec![EnforcerMessage::Remove(Remove { peer: payer(3) })],
        EnforcerMsgType::Counters => vec![
            EnforcerMessage::Counters(Counters {
                peer: payer(1),
                to_payer: u64::MAX,
                from_payer: u64::MAX,
            }),
            EnforcerMessage::Counters(Counters {
                peer: payer(2),
                to_payer: 0,
                from_payer: 0,
            }),
        ],
        EnforcerMsgType::Conflict => every_subject()
            .into_iter()
            .map(|subject| {
                EnforcerMessage::Conflict(Conflict {
                    peer: payer(4),
                    subject,
                })
            })
            .collect(),
    }
}

fn encoded(msg: &EnforcerMessage) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(msg, &mut buf).expect("encode");
    buf
}

fn validate(cbor: &[u8]) -> Result<(), String> {
    // The `cddl` crate panics, rather than failing, when it checks a bstr
    // against a text literal: a malformed conflict reaches hello's identity.
    // That is a message that does not validate.
    std::panic::catch_unwind(|| cddl::validate_cbor_from_slice(SCHEMA, cbor, None))
        .map_err(|_| "the validator panicked".to_string())?
        .map_err(|e| e.to_string())
}

#[track_caller]
fn assert_valid(cbor: &[u8], what: &str) {
    if let Err(e) = validate(cbor) {
        panic!("{what} does not validate against enforcer.cddl: {e}");
    }
}

#[track_caller]
fn assert_invalid(cbor: &[u8], what: &str) {
    assert!(
        validate(cbor).is_err(),
        "{what} validated against enforcer.cddl but should not have"
    );
    assert!(decode(cbor).is_err(), "{what} decoded but should not have");
}

fn map(pairs: u64, fill: impl FnOnce(&mut Encoder<&mut Vec<u8>>)) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut e = Encoder::new(&mut buf);
    e.map(pairs).expect("encode");
    fill(&mut e);
    buf
}

fn hello_map(identity: &str, unit: Option<&str>) -> Vec<u8> {
    map(if unit.is_some() { 5 } else { 4 }, |e| {
        e.u8(0).unwrap().u8(0x20).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(2).unwrap().str(identity).unwrap();
        e.u8(3).unwrap().bool(false).unwrap();
        if let Some(unit) = unit {
            e.u8(4).unwrap().str(unit).unwrap();
        }
    })
}

#[test]
fn every_encoded_message_validates_and_round_trips() {
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
    let tags_in_schema: Vec<u8> = SCHEMA
        .lines()
        .map(|l| l.split(';').next().unwrap_or("").trim())
        .filter_map(|l| l.strip_prefix("0: 0x"))
        .map(|hex| u8::from_str_radix(hex.trim_end_matches(','), 16).expect("hex tag"))
        .collect();

    for ty in msg_types() {
        assert!(
            tags_in_schema.contains(&(ty as u8)),
            "{ty:?} (0x{:02X}) has no entry in enforcer.cddl",
            ty as u8
        );
    }
    for tag in &tags_in_schema {
        assert!(
            EnforcerMsgType::from_u8(*tag).is_some(),
            "enforcer.cddl describes tag 0x{tag:02X}, which the codec does not know"
        );
    }
    assert_eq!(tags_in_schema.len(), msg_types().len());
}

#[test]
fn enforcer_tags_are_disjoint_from_the_wire_protocols() {
    // A frame on the wrong socket fails to decode instead of being misread.
    for ty in msg_types() {
        assert!(tollgate_protocol::MsgType::from_u8(ty as u8).is_none());
        let buf = encoded(&vectors(ty)[0]);
        assert!(tollgate_protocol::decode(&buf).is_err(), "{ty:?}");
    }
    let wire = tollgate_protocol::Message::Disconnect(tollgate_protocol::Disconnect {
        reason: tollgate_protocol::ReasonCode::Other,
    });
    let mut buf = Vec::new();
    tollgate_protocol::encode(&wire, &mut buf).expect("encode");
    assert_eq!(decode(&buf), Err(Error::UnknownType(0x0B)));
    assert!(validate(&buf).is_err());
}

#[test]
fn frames_carry_enforcer_messages_back_to_back() {
    let messages: Vec<EnforcerMessage> = msg_types().into_iter().flat_map(vectors).collect();
    let mut stream = Vec::new();
    for msg in &messages {
        encode_frame(msg, &mut stream).expect("frame");
    }
    // Delivered a byte at a time, as a socket might.
    let mut reader = FrameReader::new();
    let mut out = Vec::new();
    for byte in stream {
        reader.push(&[byte]);
        while let Some(msg) = reader.next_enforcer_message() {
            out.push(msg.expect("decode"));
        }
    }
    assert_eq!(out, messages);
    assert_eq!(reader.pending(), 0);
}

// ---------------------------------------------------------------------------
// What the decoder accepts but the encoder never writes
// ---------------------------------------------------------------------------

#[test]
fn unknown_keys_and_any_key_order_validate() {
    let buf = map(6, |e| {
        e.u8(9).unwrap().str("from the future").unwrap();
        e.u8(4).unwrap().str("wh").unwrap();
        e.u8(3).unwrap().bool(true).unwrap();
        e.u8(2).unwrap().str("pubkey").unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(0).unwrap().u8(0x20).unwrap();
    });
    assert_valid(&buf, "hello with an unknown key, reordered");
    assert_eq!(
        decode(&buf).expect("decode"),
        EnforcerMessage::Hello(Hello {
            version: 1,
            identity: Identity::Pubkey,
            delegated: true,
            unit: "wh".into(),
        })
    );

    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x23).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(255).unwrap().array(0).unwrap();
    });
    assert_valid(&buf, "remove with key 255");
    decode(&buf).expect("decode");
}

// ---------------------------------------------------------------------------
// Malformed messages fail validation as well as decoding
// ---------------------------------------------------------------------------

#[test]
fn a_hello_without_a_unit_is_invalid() {
    // The unit is the check that the two ends count the same thing; leaving it
    // out is not "byte by default".
    assert_invalid(&hello_map("address", None), "hello without key 4");
}

#[test]
fn a_hello_missing_its_delegated_flag_is_invalid() {
    // An enforcer has to say whether it takes a third party's word, not leave
    // it to a default.
    let buf = map(4, |e| {
        e.u8(0).unwrap().u8(0x20).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(2).unwrap().str("address").unwrap();
        e.u8(4).unwrap().str("byte").unwrap();
    });
    assert_invalid(&buf, "hello without key 3");
}

#[test]
fn a_hello_with_an_identity_that_is_not_one_is_invalid() {
    for identity in ["fips", "claimed", "Pubkey", "", "pubkeys", "an address"] {
        let buf = hello_map(identity, Some("byte"));
        assert_invalid(&buf, &format!("hello with identity {identity:?}"));
        assert_eq!(decode(&buf), Err(Error::UnknownIdentity(identity.into())));
    }
}

#[test]
fn a_hello_with_kinds_in_the_old_shape_is_invalid() {
    // Subject kinds are gone from the wire: an array at key 2 is not an
    // identity.
    let buf = map(5, |e| {
        e.u8(0).unwrap().u8(0x20).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(2).unwrap().array(1).unwrap().u8(0).unwrap();
        e.u8(3).unwrap().bool(false).unwrap();
        e.u8(4).unwrap().str("byte").unwrap();
    });
    assert_invalid(&buf, "hello with a list of kinds");
}

#[test]
fn a_bind_with_too_many_subjects_is_invalid_and_is_never_written() {
    let bind = Bind {
        peer: payer(1),
        bindings: (0..=MAX_BINDINGS as u8)
            .map(|i| Binding {
                subject: subject(&[i; 16]),
                delegated: false,
            })
            .collect(),
    };
    let mut buf = Vec::new();
    assert_eq!(
        encode(&EnforcerMessage::Bind(bind.clone()), &mut buf),
        Err(Error::TooManyBindings(MAX_BINDINGS + 1))
    );

    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x21).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(2).unwrap().array(bind.bindings.len() as u64).unwrap();
        for b in &bind.bindings {
            e.array(2).unwrap();
            e.bytes(b.subject.as_bytes()).unwrap().bool(false).unwrap();
        }
    });
    assert_invalid(&buf, "bind with nine subjects");
}

#[test]
fn a_subject_of_no_bytes_or_more_than_64_is_invalid() {
    for len in [0usize, MAX_SUBJECT_LEN + 1] {
        assert_eq!(Subject::new(vec![1; len]), Err(Error::SubjectLength(len)));

        let buf = map(3, |e| {
            e.u8(0).unwrap().u8(0x25).unwrap();
            e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
            e.u8(2).unwrap().bytes(&vec![1; len]).unwrap();
        });
        assert_invalid(&buf, &format!("a conflict over a subject of {len} bytes"));

        let buf = map(3, |e| {
            e.u8(0).unwrap().u8(0x21).unwrap();
            e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
            e.u8(2).unwrap().array(1).unwrap().array(2).unwrap();
            e.bytes(&vec![1; len]).unwrap().bool(true).unwrap();
        });
        assert_invalid(&buf, &format!("a bind of a subject of {len} bytes"));
    }
}

#[test]
fn a_subject_in_the_old_kinded_shape_is_invalid() {
    // `[kind, bytes]` was a subject once; now a subject is the bytes alone.
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x25).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(2).unwrap().array(2).unwrap().u8(0).unwrap();
        e.bytes(&[192, 168, 1, 23]).unwrap();
    });
    assert_invalid(&buf, "a subject with a kind");
}

#[test]
fn a_payer_of_the_wrong_length_is_invalid() {
    // A payer is the compressed key; its x-only form is a subject.
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x22).unwrap();
        e.u8(1).unwrap().bytes(&[2; 32]).unwrap();
        e.u8(2).unwrap().u64(0).unwrap();
    });
    assert_invalid(&buf, "set with a 32-byte payer");
}

#[test]
fn a_set_without_a_rate_is_invalid() {
    // Closed is `0`, not an absent rate.
    let buf = map(2, |e| {
        e.u8(0).unwrap().u8(0x22).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
    });
    assert_invalid(&buf, "set without key 2");
}

#[test]
fn a_known_key_with_the_wrong_type_is_invalid() {
    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x22).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(2).unwrap().str("fast").unwrap();
    });
    assert_invalid(&buf, "set with a text rate");
}

// ---------------------------------------------------------------------------
// Subject forms, and what an enforcer may be sent
// ---------------------------------------------------------------------------

#[test]
fn a_payers_key_is_bound_by_its_x_only_form() {
    let key = payer(9);
    let subject = Subject::pubkey(&key);
    assert_eq!(subject.as_bytes(), &[9; 32]);
    assert_eq!(subject.as_bytes().len(), Identity::Pubkey.subject_len());
}

#[test]
fn every_address_has_one_sixteen_byte_encoding() {
    use core::net::IpAddr;
    let v4: IpAddr = "192.168.1.23".parse().unwrap();
    let mapped: IpAddr = "::ffff:192.168.1.23".parse().unwrap();
    let expected = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 23];
    // Four bytes or sixteen on the socket, the subject is the same.
    assert_eq!(Subject::address(v4).as_bytes(), &expected);
    assert_eq!(Subject::address(mapped), Subject::address(v4));

    let v6: IpAddr = "fd10:93b2:8586:6046:e42d:c089:3228:ccff".parse().unwrap();
    assert_eq!(
        Subject::address(v6).as_bytes().len(),
        Identity::Address.subject_len()
    );
}

#[test]
fn a_subject_that_is_not_delegated_must_fit_the_identity() {
    let key = Binding {
        subject: Subject::pubkey(&payer(1)),
        delegated: false,
    };
    let addr = Binding {
        subject: Subject::address("10.0.0.1".parse().unwrap()),
        delegated: false,
    };
    let hello = |identity| Hello {
        version: PROTOCOL_VERSION,
        identity,
        delegated: false,
        unit: "byte".into(),
    };

    assert_eq!(hello(Identity::Pubkey).check(&key), Ok(()));
    assert_eq!(
        hello(Identity::Pubkey).check(&addr),
        Err(Error::WrongSubjectLength {
            identity: Identity::Pubkey,
            got: 16
        })
    );
    assert_eq!(hello(Identity::Address).check(&addr), Ok(()));
    assert_eq!(
        hello(Identity::Address).check(&key),
        Err(Error::WrongSubjectLength {
            identity: Identity::Address,
            got: 32
        })
    );
}

#[test]
fn a_delegated_subject_is_any_length_but_only_to_an_enforcer_that_takes_them() {
    let tap = Binding {
        subject: subject(b"tap-3"),
        delegated: true,
    };
    let mut hello = Hello {
        version: PROTOCOL_VERSION,
        identity: Identity::Address,
        delegated: false,
        unit: "ml".into(),
    };
    assert_eq!(hello.check(&tap), Err(Error::DelegatedRefused));
    hello.delegated = true;
    assert_eq!(hello.check(&tap), Ok(()));
    // Five bytes would be the wrong length for an address it saw itself.
    assert!(!hello.accepts(&Binding {
        delegated: false,
        ..tap
    }));
}

#[test]
fn a_message_too_long_for_a_frame_is_refused_rather_than_truncated() {
    let hello = EnforcerMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        identity: Identity::Address,
        delegated: true,
        unit: "u".repeat(70_000),
    });
    let mut out = vec![1, 2, 3];
    assert!(matches!(
        encode_frame(&hello, &mut out),
        Err(Error::FrameTooLong(_))
    ));
    assert_eq!(out, [1, 2, 3], "nothing half-written is left behind");
}
