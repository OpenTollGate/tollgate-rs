//! The gate codec against `gate.cddl`, its normative schema, and round trips
//! through the framing.
//!
//! Every message the encoder writes must validate and decode back to itself,
//! and so must the variants the decoder accepts but the encoder never writes.
//! Malformed messages must fail both.

use minicbor::Encoder;
use tollgate_protocol::gate::*;
use tollgate_protocol::{FrameReader, PubKey};

const SCHEMA: &str = include_str!("../gate.cddl");

fn payer(seed: u8) -> PubKey {
    let mut b = [seed; 33];
    b[0] = 0x02;
    PubKey(b)
}

fn every_subject() -> Vec<Subject> {
    vec![
        Subject::Ipv4([192, 168, 1, 23]),
        Subject::Ipv6([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5]),
        Subject::Mac([0xaa, 0xbb, 0xcc, 0x00, 0x11, 0x22]),
        Subject::Pubkey([0x3b; 32]),
        Subject::Opaque {
            kind: u32::MAX,
            value: vec![7; MAX_OPAQUE_LEN],
        },
        Subject::Opaque {
            kind: 0,
            value: vec![],
        },
    ]
}

/// Every tag the gate protocol defines, in order.
fn msg_types() -> Vec<GateMsgType> {
    (0..=u8::MAX).filter_map(GateMsgType::from_u8).collect()
}

/// A fully populated and a minimal instance of each message type. Exhaustive
/// on purpose: a new tag does not compile until it has vectors, and then fails
/// the schema test until `gate.cddl` describes it.
fn vectors(ty: GateMsgType) -> Vec<GateMessage> {
    match ty {
        GateMsgType::Hello => vec![
            GateMessage::Hello(Hello {
                version: GATE_PROTOCOL_VERSION,
                kinds: vec![
                    SubjectKind::Ipv4,
                    SubjectKind::Ipv6,
                    SubjectKind::Mac,
                    SubjectKind::Pubkey,
                    SubjectKind::Opaque,
                ],
                identify: Identify::Fips,
                delegated: true,
                opaque_kinds: vec![0, 1, u32::MAX],
            }),
            GateMessage::Hello(Hello {
                version: GATE_PROTOCOL_VERSION,
                kinds: vec![SubjectKind::Ipv4],
                identify: Identify::Claimed,
                delegated: false,
                opaque_kinds: vec![],
            }),
        ],
        GateMsgType::Bind => vec![
            GateMessage::Bind(Bind {
                peer: payer(1),
                bindings: every_subject()
                    .into_iter()
                    .chain([Subject::Ipv4([10, 0, 0, 1]), Subject::Mac([1; 6])])
                    .enumerate()
                    .map(|(i, subject)| Binding {
                        subject,
                        delegated: i % 2 == 1,
                    })
                    .collect(),
            }),
            GateMessage::Bind(Bind {
                peer: payer(2),
                bindings: vec![],
            }),
        ],
        GateMsgType::Set => vec![
            GateMessage::Set(Set {
                peer: payer(1),
                rate: Some(u64::MAX - 1),
            }),
            GateMessage::Set(Set {
                peer: payer(1),
                rate: Some(0),
            }),
            GateMessage::Set(Set {
                peer: payer(2),
                rate: None,
            }),
        ],
        GateMsgType::Remove => vec![GateMessage::Remove(Remove { peer: payer(3) })],
        GateMsgType::Counters => vec![
            GateMessage::Counters(Counters {
                peer: payer(1),
                delivered: u64::MAX,
                received: u64::MAX,
            }),
            GateMessage::Counters(Counters {
                peer: payer(2),
                delivered: 0,
                received: 0,
            }),
        ],
        GateMsgType::Conflict => every_subject()
            .into_iter()
            .map(|subject| {
                GateMessage::Conflict(Conflict {
                    peer: payer(4),
                    subject,
                })
            })
            .collect(),
    }
}

fn encoded(msg: &GateMessage) -> Vec<u8> {
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
        panic!("{what} does not validate against gate.cddl: {e}");
    }
}

#[track_caller]
fn assert_invalid(cbor: &[u8], what: &str) {
    assert!(
        validate(cbor).is_err(),
        "{what} validated against gate.cddl but should not have"
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
            "{ty:?} (0x{:02X}) has no entry in gate.cddl",
            ty as u8
        );
    }
    for tag in &tags_in_schema {
        assert!(
            GateMsgType::from_u8(*tag).is_some(),
            "gate.cddl describes tag 0x{tag:02X}, which the codec does not know"
        );
    }
    assert_eq!(tags_in_schema.len(), msg_types().len());
}

#[test]
fn gate_tags_are_disjoint_from_the_wire_protocols() {
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
fn frames_carry_gate_messages_back_to_back() {
    let messages: Vec<GateMessage> = msg_types().into_iter().flat_map(vectors).collect();
    let mut stream = Vec::new();
    for msg in &messages {
        encode_frame(msg, &mut stream).expect("frame");
    }
    // Delivered a byte at a time, as a socket might.
    let mut reader = FrameReader::new();
    let mut out = Vec::new();
    for byte in stream {
        reader.push(&[byte]);
        while let Some(msg) = reader.next_gate_message() {
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
fn unknown_keys_any_key_order_and_empty_opaque_kinds_validate() {
    let buf = map(7, |e| {
        e.u8(9).unwrap().str("from the future").unwrap();
        e.u8(5).unwrap().array(0).unwrap();
        e.u8(4).unwrap().bool(false).unwrap();
        e.u8(3).unwrap().u8(0).unwrap();
        e.u8(2).unwrap().array(1).unwrap().u8(0).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(0).unwrap().u8(0x20).unwrap();
    });
    assert_valid(&buf, "hello with an unknown key, reordered");
    let GateMessage::Hello(hello) = decode(&buf).expect("decode") else {
        panic!("not a hello");
    };
    assert!(hello.opaque_kinds.is_empty());

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
fn a_hello_missing_a_required_key_is_invalid() {
    // No delegated flag: a gate has to say whether it takes a third party's
    // word, not leave it to a default.
    let buf = map(4, |e| {
        e.u8(0).unwrap().u8(0x20).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(2).unwrap().array(1).unwrap().u8(0).unwrap();
        e.u8(3).unwrap().u8(0).unwrap();
    });
    assert_invalid(&buf, "hello without key 4");
}

#[test]
fn a_hello_with_no_kinds_or_an_unknown_one_is_invalid() {
    for kinds in [vec![], vec![5u8]] {
        let buf = map(5, |e| {
            e.u8(0).unwrap().u8(0x20).unwrap();
            e.u8(1).unwrap().u8(1).unwrap();
            e.u8(2).unwrap().array(kinds.len() as u64).unwrap();
            for k in &kinds {
                e.u8(*k).unwrap();
            }
            e.u8(3).unwrap().u8(0).unwrap();
            e.u8(4).unwrap().bool(false).unwrap();
        });
        assert_invalid(&buf, &format!("hello with kinds {kinds:?}"));
    }
}

#[test]
fn a_hello_with_an_unknown_identify_mode_is_invalid() {
    let buf = map(5, |e| {
        e.u8(0).unwrap().u8(0x20).unwrap();
        e.u8(1).unwrap().u8(1).unwrap();
        e.u8(2).unwrap().array(1).unwrap().u8(3).unwrap();
        e.u8(3).unwrap().u8(2).unwrap();
        e.u8(4).unwrap().bool(false).unwrap();
    });
    assert_invalid(&buf, "hello requiring identify mode 2");
}

#[test]
fn a_bind_with_too_many_subjects_is_invalid_and_is_never_written() {
    let bind = Bind {
        peer: payer(1),
        bindings: (0..=MAX_BINDINGS as u8)
            .map(|i| Binding {
                subject: Subject::Ipv4([10, 0, 0, i]),
                delegated: false,
            })
            .collect(),
    };
    let mut buf = Vec::new();
    assert_eq!(
        encode(&GateMessage::Bind(bind.clone()), &mut buf),
        Err(Error::TooManyBindings(MAX_BINDINGS + 1))
    );

    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x21).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(2).unwrap().array(bind.bindings.len() as u64).unwrap();
        for (i, _) in bind.bindings.iter().enumerate() {
            e.array(2).unwrap().array(2).unwrap().u8(0).unwrap();
            e.bytes(&[10, 0, 0, i as u8]).unwrap().bool(false).unwrap();
        }
    });
    assert_invalid(&buf, "bind with nine subjects");
}

#[test]
fn a_subject_of_the_wrong_length_is_invalid() {
    for (kind, len) in [(0u8, 16usize), (1, 4), (2, 8), (3, 33)] {
        let buf = map(3, |e| {
            e.u8(0).unwrap().u8(0x25).unwrap();
            e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
            e.u8(2).unwrap().array(2).unwrap().u8(kind).unwrap();
            e.bytes(&vec![1; len]).unwrap();
        });
        assert_invalid(&buf, &format!("subject kind {kind} of {len} bytes"));
    }

    let buf = map(3, |e| {
        e.u8(0).unwrap().u8(0x25).unwrap();
        e.u8(1).unwrap().bytes(&payer(1).0).unwrap();
        e.u8(2)
            .unwrap()
            .array(3)
            .unwrap()
            .u8(4)
            .unwrap()
            .u32(1)
            .unwrap();
        e.bytes(&[1; MAX_OPAQUE_LEN + 1]).unwrap();
    });
    assert_invalid(&buf, "an opaque subject of 65 bytes");

    let mut buf = Vec::new();
    let too_long = GateMessage::Conflict(Conflict {
        peer: payer(1),
        subject: Subject::Opaque {
            kind: 1,
            value: vec![1; MAX_OPAQUE_LEN + 1],
        },
    });
    assert_eq!(
        encode(&too_long, &mut buf),
        Err(Error::OpaqueTooLong(MAX_OPAQUE_LEN + 1))
    );
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

#[test]
fn a_payers_key_is_bound_by_its_x_only_form() {
    let key = payer(9);
    assert_eq!(Subject::pubkey_of(&key), Subject::Pubkey([9; 32]));
}

#[test]
fn an_ipv4_mapped_address_binds_as_ipv4() {
    use core::net::IpAddr;
    let mapped: IpAddr = "::ffff:192.168.1.23".parse().unwrap();
    assert_eq!(Subject::from(mapped), Subject::Ipv4([192, 168, 1, 23]));
    let v6: IpAddr = "fd10:93b2::1".parse().unwrap();
    assert_eq!(Subject::from(v6).kind(), SubjectKind::Ipv6);
}

#[test]
fn a_hello_accepts_only_the_kinds_it_listed() {
    let hello = Hello {
        version: 1,
        kinds: vec![SubjectKind::Ipv4, SubjectKind::Opaque],
        identify: Identify::Claimed,
        delegated: false,
        opaque_kinds: vec![7],
    };
    let direct = |subject| Binding {
        subject,
        delegated: false,
    };
    assert!(hello.accepts(&direct(Subject::Ipv4([10, 0, 0, 1]))));
    assert!(!hello.accepts(&direct(Subject::Mac([1; 6]))));
    assert!(hello.accepts(&direct(Subject::Opaque {
        kind: 7,
        value: vec![1]
    })));
    assert!(!hello.accepts(&direct(Subject::Opaque {
        kind: 8,
        value: vec![1]
    })));
    // This gate takes no third party's word.
    assert!(!hello.accepts(&Binding {
        subject: Subject::Ipv4([10, 0, 0, 1]),
        delegated: true,
    }));
}

#[test]
fn a_message_too_long_for_a_frame_is_refused_rather_than_truncated() {
    let hello = GateMessage::Hello(Hello {
        version: GATE_PROTOCOL_VERSION,
        kinds: vec![SubjectKind::Opaque],
        identify: Identify::Claimed,
        delegated: true,
        opaque_kinds: vec![u32::MAX; 20_000],
    });
    let mut out = vec![1, 2, 3];
    assert!(matches!(
        encode_frame(&hello, &mut out),
        Err(Error::FrameTooLong(_))
    ));
    assert_eq!(out, [1, 2, 3], "nothing half-written is left behind");
}
