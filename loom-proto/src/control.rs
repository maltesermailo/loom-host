//! Control-stream framing and messages — PROTOCOL.md §3 (+ pairing types
//! 0x50–0x53 from PAIRING.md §5).
//!
//! Each frame is a big-endian `u32` body length (MUST be ≤ 65536) followed by a
//! CBOR-encoded message. Every message is a 2-element array `[msg_type, body]`
//! where `body` is a map with integer keys. On decode, unknown body keys are
//! stripped and unknown `msg_type`s are ignored (§3.2 forward compatibility);
//! envelope/framing violations are `PROTOCOL_VIOLATION`.

use crate::cbor::{self, Value};
use crate::error::{Error, Result};

/// Maximum control frame body length (§3.1).
pub const MAX_FRAME_BODY: usize = 65536;

// --- message type registry (§3.3, PAIRING.md §5) ---
/// HELLO — client → host, setup.
pub const HELLO: u64 = 0x01;
/// WELCOME — host → client, setup.
pub const WELCOME: u64 = 0x02;
/// CONFIG — host → client, setup/any.
pub const CONFIG: u64 = 0x03;
/// CONFIG_ACK — client → host, setup/any.
pub const CONFIG_ACK: u64 = 0x04;
/// START — host → client, setup.
pub const START: u64 = 0x05;
/// INPUT — client → host, streaming.
pub const INPUT: u64 = 0x10;
/// IDR_REQUEST — client → host, streaming.
pub const IDR_REQUEST: u64 = 0x20;
/// STATS — client → host, streaming.
pub const STATS: u64 = 0x21;
/// VIEWPORT — client → host, streaming (best-effort resolution request, §3.10).
pub const VIEWPORT: u64 = 0x22;
/// CLOCK_PING — client → host, any.
pub const CLOCK_PING: u64 = 0x30;
/// CLOCK_PONG — host → client, any.
pub const CLOCK_PONG: u64 = 0x31;
/// ERROR — either direction, any.
pub const ERROR: u64 = 0x40;
/// BYE — either direction, any.
pub const BYE: u64 = 0x41;
/// PAIR_A — client → host, pairing.
pub const PAIR_A: u64 = 0x50;
/// PAIR_B — host → client, pairing.
pub const PAIR_B: u64 = 0x51;
/// PAIR_C — client → host, pairing.
pub const PAIR_C: u64 = 0x52;
/// PAIR_RESULT — host → client, pairing.
pub const PAIR_RESULT: u64 = 0x53;

/// The set of body-map keys defined for a message type, or `None` if the type
/// is unregistered (and hence must be ignored on receipt). Keys not in this set
/// are dropped on decode.
pub fn known_keys(msg_type: u64) -> Option<&'static [i128]> {
    Some(match msg_type {
        HELLO => &[0, 1, 2, 3, 4, 5],
        WELCOME => &[0, 1, 2],
        CONFIG => &[0, 1, 2, 3, 4, 5],
        CONFIG_ACK => &[0],
        START => &[],
        INPUT => &[0],
        IDR_REQUEST => &[0],
        STATS => &[0, 1, 2, 3, 4, 5, 6],
        VIEWPORT => &[0],
        CLOCK_PING => &[0],
        CLOCK_PONG => &[0, 1, 2],
        ERROR => &[0, 1],
        BYE => &[0],
        PAIR_A => &[0],
        PAIR_B => &[0, 1],
        PAIR_C => &[0],
        PAIR_RESULT => &[0, 1],
        _ => return None,
    })
}

/// The outcome of decoding one control frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Decoded {
    /// A registered message; `body` has unknown keys stripped, order unspecified.
    Message {
        /// The message type (§3.3).
        msg_type: u64,
        /// The filtered body entries.
        body: Vec<(Value, Value)>,
    },
    /// A well-formed frame carrying an unregistered `msg_type` (§3.2: ignore it).
    Ignored,
}

/// Encode a control frame: canonical-CBOR `[msg_type, body]` prefixed with its
/// big-endian `u32` body length.
pub fn encode_frame(msg_type: u64, body: &[(Value, Value)]) -> Vec<u8> {
    let envelope = Value::Array(vec![
        Value::Int(i128::from(msg_type)),
        Value::Map(body.to_vec()),
    ]);
    let cbor = envelope.to_canonical();
    let mut out = Vec::with_capacity(4 + cbor.len());
    out.extend_from_slice(&(cbor.len() as u32).to_be_bytes());
    out.extend_from_slice(&cbor);
    out
}

/// Decode one length-prefixed control frame. Returns [`Decoded`], or
/// [`Error::ProtocolViolation`] on any framing/envelope violation (§3, §6.6).
pub fn decode_frame(bytes: &[u8]) -> Result<Decoded> {
    if bytes.len() < 4 {
        return Err(Error::ProtocolViolation);
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    // The length field alone is sufficient to reject an over-limit frame (§3.1).
    if len > MAX_FRAME_BODY {
        return Err(Error::ProtocolViolation);
    }
    let body = bytes.get(4..4 + len).ok_or(Error::ProtocolViolation)?;

    let value = cbor::decode(body)?;
    let items = value.as_array().ok_or(Error::ProtocolViolation)?;
    if items.len() != 2 {
        return Err(Error::ProtocolViolation);
    }
    let msg_type = match items[0].as_int() {
        Some(i) if i >= 0 => i as u64,
        _ => return Err(Error::ProtocolViolation),
    };
    let map = items[1].as_map().ok_or(Error::ProtocolViolation)?;

    let allowed = match known_keys(msg_type) {
        Some(k) => k,
        None => return Ok(Decoded::Ignored),
    };

    let mut filtered = Vec::new();
    for (k, v) in map {
        if let Value::Int(ki) = k {
            if allowed.contains(ki) {
                filtered.push((k.clone(), v.clone()));
            }
        }
    }
    Ok(Decoded::Message {
        msg_type,
        body: filtered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(pairs: Vec<(i128, Value)>) -> Vec<(Value, Value)> {
        pairs.into_iter().map(|(k, v)| (Value::Int(k), v)).collect()
    }

    #[test]
    fn encode_config_ack_frame() {
        // [4, {0: 1}] canonical, length-prefixed.
        let f = encode_frame(CONFIG_ACK, &body(vec![(0, Value::Int(1))]));
        assert_eq!(hex::encode(f), "000000058204a10001");
    }

    #[test]
    fn encode_empty_body_start() {
        let f = encode_frame(START, &[]);
        assert_eq!(hex::encode(f), "000000038205a0");
    }

    #[test]
    fn roundtrip_message() {
        let b = body(vec![(0, Value::Int(2)), (1, Value::Text("busy".into()))]);
        let f = encode_frame(ERROR, &b);
        match decode_frame(&f).unwrap() {
            Decoded::Message { msg_type, body } => {
                assert_eq!(msg_type, ERROR);
                assert_eq!(body.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decode_strips_unknown_keys() {
        // HELLO with a bogus key 99 present; it must be dropped.
        let b = body(vec![(0, Value::Int(1)), (99, Value::Text("future".into()))]);
        let f = encode_frame(HELLO, &b);
        match decode_frame(&f).unwrap() {
            Decoded::Message { msg_type, body } => {
                assert_eq!(msg_type, HELLO);
                assert_eq!(body, body_only_key0());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn body_only_key0() -> Vec<(Value, Value)> {
        vec![(Value::Int(0), Value::Int(1))]
    }

    #[test]
    fn decode_ignores_unknown_type() {
        let f = encode_frame(0x7F, &body(vec![(0, Value::Int(123))]));
        assert_eq!(decode_frame(&f).unwrap(), Decoded::Ignored);
    }

    #[test]
    fn decode_rejects_over_limit_length_field() {
        // Only the 4-byte length is present; 65537 > 65536 => violation.
        let bytes = 65537u32.to_be_bytes();
        assert!(matches!(
            decode_frame(&bytes),
            Err(Error::ProtocolViolation)
        ));
    }

    #[test]
    fn decode_rejects_envelope_violations() {
        // body not a map: [5, [1,2]]
        let not_map =
            Value::Array(vec![Value::Int(5), Value::Array(vec![Value::Int(1)])]).to_canonical();
        let framed = frame_with(&not_map);
        assert!(matches!(
            decode_frame(&framed),
            Err(Error::ProtocolViolation)
        ));

        // wrong arity: [1, {}, 3]
        let arity =
            Value::Array(vec![Value::Int(1), Value::Map(vec![]), Value::Int(3)]).to_canonical();
        assert!(matches!(
            decode_frame(&frame_with(&arity)),
            Err(Error::ProtocolViolation)
        ));

        // not an array at all
        let map = Value::Map(vec![(Value::Text("a".into()), Value::Int(1))]).to_canonical();
        assert!(matches!(
            decode_frame(&frame_with(&map)),
            Err(Error::ProtocolViolation)
        ));
    }

    fn frame_with(cbor_body: &[u8]) -> Vec<u8> {
        let mut out = (cbor_body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(cbor_body);
        out
    }

    #[test]
    fn decode_rejects_truncated_cbor() {
        // Frame claims 2 bytes of body "8205" = array(2) with only 1 item present.
        let framed = frame_with(&[0x82, 0x05]);
        assert!(matches!(
            decode_frame(&framed),
            Err(Error::ProtocolViolation)
        ));
    }
}
