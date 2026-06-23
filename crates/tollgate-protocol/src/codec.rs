//! Transport framing and message-type peeking.
//!
//! HTTP polling frames each CBOR message with a 2-byte little-endian length
//! prefix (see `docs/design/core/tollgate-protocol.md`). WebSocket puts one
//! message per binary frame and needs no prefix — there the raw message bytes
//! are used directly.

use alloc::vec::Vec;

use minicbor::Decode;

use crate::message::MessageType;

/// Maximum size of a single framed message — the 2-byte length prefix limit.
pub const MAX_FRAME_LEN: usize = u16::MAX as usize;

/// Errors from frame (de)serialization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameError {
    /// A message body exceeds [`MAX_FRAME_LEN`].
    TooLong,
    /// The buffer ended in the middle of a frame.
    Truncated,
}

/// Append `body` to `out` as a length-prefixed frame (2-byte little-endian
/// length + body).
pub fn encode_frame(body: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    if body.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLong);
    }
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(body);
    Ok(())
}

/// Encode a single message body into a fresh one-frame buffer.
pub fn frame(body: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::with_capacity(body.len() + 2);
    encode_frame(body, &mut out)?;
    Ok(out)
}

/// Split a buffer of length-prefixed frames into the contained message slices.
/// An empty buffer yields an empty list.
pub fn decode_frames(buf: &[u8]) -> Result<Vec<&[u8]>, FrameError> {
    let mut frames = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err(FrameError::Truncated);
        }
        let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        let end = 2 + len;
        if rest.len() < end {
            return Err(FrameError::Truncated);
        }
        frames.push(&rest[2..end]);
        rest = &rest[end..];
    }
    Ok(frames)
}

/// A minimal view of any message — just field key 0, the message type.
/// minicbor skips the other map keys during decode.
#[derive(Decode)]
#[cbor(map)]
struct Header {
    #[n(0)]
    type_tag: u8,
}

/// Read the [`MessageType`] of an encoded message without decoding the body.
/// Returns `None` if the bytes aren't a valid message map or the type is unknown.
pub fn peek_type(message: &[u8]) -> Option<MessageType> {
    let header: Header = minicbor::decode(message).ok()?;
    MessageType::from_u8(header.type_tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn frame_round_trips_multiple_messages() {
        let mut buf = Vec::new();
        encode_frame(b"hello", &mut buf).unwrap();
        encode_frame(b"", &mut buf).unwrap();
        encode_frame(b"world!", &mut buf).unwrap();

        let frames = decode_frames(&buf).unwrap();
        assert_eq!(frames, vec![&b"hello"[..], &b""[..], &b"world!"[..]]);
    }

    #[test]
    fn empty_buffer_yields_no_frames() {
        assert_eq!(decode_frames(&[]).unwrap(), Vec::<&[u8]>::new());
    }

    #[test]
    fn truncated_length_prefix_errors() {
        assert_eq!(decode_frames(&[0x05]), Err(FrameError::Truncated));
    }

    #[test]
    fn truncated_body_errors() {
        // Says 5 bytes follow, but only 2 are present.
        assert_eq!(
            decode_frames(&[0x05, 0x00, 0xAA, 0xBB]),
            Err(FrameError::Truncated)
        );
    }

    #[test]
    fn encode_frame_rejects_oversize_body_and_accepts_the_limit() {
        let too_big = vec![0u8; MAX_FRAME_LEN + 1];
        let mut out = Vec::new();
        assert_eq!(encode_frame(&too_big, &mut out), Err(FrameError::TooLong));
        assert!(
            out.is_empty(),
            "nothing is written when the body is rejected"
        );

        // Exactly at the limit is allowed (2-byte prefix + body).
        let at_limit = vec![0u8; MAX_FRAME_LEN];
        let mut out = Vec::new();
        assert!(encode_frame(&at_limit, &mut out).is_ok());
        assert_eq!(out.len(), MAX_FRAME_LEN + 2);
    }

    #[test]
    fn peek_type_handles_garbage_unknown_and_known() {
        // Not a CBOR message map at all.
        assert!(peek_type(&[0xff, 0xff]).is_none());
        // A valid one-entry map {0: 0x0F} whose type tag is an unknown type.
        assert!(peek_type(&[0xA1, 0x00, 0x0F]).is_none());
        // Sanity: {0: 0x07} is recognised as BootstrapToken.
        assert_eq!(
            peek_type(&[0xA1, 0x00, 0x07]),
            Some(MessageType::BootstrapToken)
        );
    }
}
