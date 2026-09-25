//! Test vectors shared by the round-trip and schema tests.
//!
//! Each test binary compiles its own copy of this module and uses only part of
//! it, hence the blanket `dead_code` allowance.
#![allow(dead_code)]

use tollgate_protocol::*;

pub fn pubkey(seed: u8) -> PubKey {
    let mut b = [seed; 33];
    b[0] = 0x02;
    PubKey(b)
}

pub fn channel(seed: u8) -> ChannelId {
    ChannelId([seed; 32])
}

pub fn signature(seed: u8) -> Signature {
    Signature([seed; 64])
}

/// Every message type the protocol defines, in tag order.
pub fn msg_types() -> Vec<MsgType> {
    (0..=u8::MAX).filter_map(MsgType::from_u8).collect()
}

/// A fully populated and a minimal instance of each message type.
///
/// "Fully populated" sets every field the encoder can vary — the optional
/// ones present, arrays at their longest. "Minimal" is the least a sender can
/// write: empty blobs and arrays where they are allowed, no text, defaults.
///
/// The match is exhaustive on purpose: a new [`MsgType`] does not compile
/// until it has vectors here, and once it does, the schema test fails until
/// `tollgate.cddl` describes it.
pub fn vectors(ty: MsgType) -> [Message; 2] {
    match ty {
        MsgType::Announce => [
            Message::Announce(Announce {
                version: PROTOCOL_VERSION,
                pubkey: pubkey(1),
                unit: "byte".into(),
                capabilities: u32::MAX,
            }),
            Message::Announce(Announce {
                version: PROTOCOL_VERSION,
                pubkey: pubkey(2),
                unit: "wh".into(),
                capabilities: 0,
            }),
        ],
        MsgType::Offer => [
            Message::Offer(Offer {
                accepted_mints: vec![
                    "https://upstream.example/mint".into(),
                    "https://hub.example/mint".into(),
                ],
                unit: "byte".into(),
                min_window_ms: 200,
                max_window_ms: 30_000,
                received_multiplier: u16::MAX,
                // Free peering: the one case key 5 is written.
                no_charge: true,
            }),
            Message::Offer(Offer {
                accepted_mints: vec!["https://hub.example/mint".into()],
                unit: "byte".into(),
                min_window_ms: 0,
                max_window_ms: 0,
                received_multiplier: 0,
                no_charge: false,
            }),
        ],
        MsgType::Accept => [
            Message::Accept(Accept {
                funding: vec![0xAA; 64],
            }),
            // No funding, which is what an uncharged payer sends.
            Message::Accept(Accept { funding: vec![] }),
        ],
        MsgType::ChannelReady => [
            Message::ChannelReady(ChannelReady {
                channel_id: channel(3),
            }),
            Message::ChannelReady(ChannelReady {
                channel_id: channel(0),
            }),
        ],
        MsgType::TopUp => [
            // As many channels as one purchase may span.
            Message::TopUp(TopUp {
                updates: (0..MAX_CHANNEL_UPDATES as u8)
                    .map(|i| ChannelUpdate {
                        channel_id: channel(i),
                        cumulative: u64::MAX - u64::from(i),
                        signature: signature(i),
                    })
                    .collect(),
                window_ms: u32::MAX,
            }),
            Message::TopUp(TopUp {
                updates: vec![ChannelUpdate {
                    channel_id: channel(3),
                    cumulative: 1,
                    signature: signature(4),
                }],
                window_ms: 0,
            }),
        ],
        MsgType::TopUpReject => [
            Message::TopUpReject(TopUpReject {
                refused: vec![
                    RefusedUpdate {
                        channel_id: channel(3),
                        cumulative: 9_000_000,
                    },
                    RefusedUpdate {
                        channel_id: channel(5),
                        cumulative: 1_500_000,
                    },
                ],
                max_rate_available: 1_000_000,
                reason: ReasonCode::RateExceedsCapacity,
            }),
            Message::TopUpReject(TopUpReject {
                refused: vec![],
                max_rate_available: 0,
                reason: ReasonCode::Other,
            }),
        ],
        MsgType::RolloverInit => [
            Message::RolloverInit(RolloverInit {
                old_channel_id: channel(3),
                funding: vec![0xBB; 96],
            }),
            Message::RolloverInit(RolloverInit {
                old_channel_id: channel(3),
                funding: vec![],
            }),
        ],
        MsgType::RolloverReady => [
            Message::RolloverReady(RolloverReady {
                old_channel_id: channel(3),
                new_channel_id: channel(5),
            }),
            Message::RolloverReady(RolloverReady {
                old_channel_id: channel(0),
                new_channel_id: channel(0),
            }),
        ],
        MsgType::ChannelClose => [
            Message::ChannelClose(ChannelClose {
                channel_id: channel(3),
                final_balance: u64::MAX,
                final_signature: signature(6),
                reason: CloseReason::PeerLeaving,
            }),
            Message::ChannelClose(ChannelClose {
                channel_id: channel(3),
                final_balance: 0,
                final_signature: signature(6),
                reason: CloseReason::Normal,
            }),
        ],
        MsgType::CloseAck => [
            Message::CloseAck(CloseAck {
                channel_id: channel(3),
                accepted_balance: u64::MAX,
            }),
            Message::CloseAck(CloseAck {
                channel_id: channel(3),
                accepted_balance: 0,
            }),
        ],
        MsgType::Reject => [
            Message::Reject(Reject {
                rejected_type: MsgType::TopUp as u8,
                reason: ReasonCode::GrantInvalid,
                text: Some("cumulative did not increase".into()),
            }),
            // The null branch of key 3.
            Message::Reject(Reject {
                rejected_type: MsgType::Offer as u8,
                reason: ReasonCode::Other,
                text: None,
            }),
        ],
        MsgType::Disconnect => [
            Message::Disconnect(Disconnect {
                reason: ReasonCode::VersionUnsupported,
            }),
            Message::Disconnect(Disconnect {
                reason: ReasonCode::Other,
            }),
        ],
    }
}

/// Both vectors of every type, so a test over this covers the whole set.
pub fn all_messages() -> Vec<Message> {
    msg_types().into_iter().flat_map(vectors).collect()
}
