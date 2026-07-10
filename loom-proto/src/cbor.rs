//! A minimal CBOR value model with a **canonical** encoder and a permissive
//! decoder, exactly as PROTOCOL.md §3.2 requires:
//!
//! > Senders MUST emit canonical CBOR (RFC 8949 §4.2.1: definite lengths,
//! > shortest-form integers, bytewise-sorted map keys); receivers MUST accept
//! > any valid CBOR.
//!
//! Encoding is hand-rolled so we control every byte (including shortest-form
//! floats per RFC 8949 §4.2.2 — the STATS jitter value 2.5 must serialize as the
//! half-precision `f9 4100`). Decoding delegates to `ciborium`, which accepts any
//! valid CBOR, and the result is lowered into [`Value`].

use crate::error::{Error, Result};

/// A CBOR value restricted to the shapes the Loom control protocol uses.
///
/// Integers are held as `i128` so both the full unsigned (`u64`) and negative
/// (`-1 - u64`) CBOR ranges are representable in one variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Major types 0 (unsigned) and 1 (negative) unified.
    Int(i128),
    /// Major type 2, a byte string.
    Bytes(Vec<u8>),
    /// Major type 3, a UTF-8 text string.
    Text(String),
    /// Major type 4, an array.
    Array(Vec<Value>),
    /// Major type 5, a map. Order here is irrelevant; the encoder sorts keys.
    Map(Vec<(Value, Value)>),
    /// A boolean (major 7, simple 20/21).
    Bool(bool),
    /// A floating-point number (major 7). Encoded in shortest lossless form.
    Float(f64),
    /// Null (major 7, simple 22).
    Null,
}

impl Value {
    /// If this is an [`Value::Int`], return it.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// If this is a [`Value::Map`], return its entries.
    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// If this is a [`Value::Array`], return its items.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Encode to canonical CBOR (RFC 8949 §4.2.1 + §4.2.2 preferred floats).
    pub fn to_canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Int(i) => encode_int(*i, out),
            Value::Bytes(b) => {
                encode_head(2, b.len() as u64, out);
                out.extend_from_slice(b);
            }
            Value::Text(s) => {
                encode_head(3, s.len() as u64, out);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Array(a) => {
                encode_head(4, a.len() as u64, out);
                for v in a {
                    v.encode_into(out);
                }
            }
            Value::Map(pairs) => encode_map(pairs, out),
            Value::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
            Value::Float(f) => encode_float(*f, out),
            Value::Null => out.push(0xf6),
        }
    }
}

/// Write a CBOR head: major type in the top 3 bits, argument in shortest form.
fn encode_head(major: u8, arg: u64, out: &mut Vec<u8>) {
    let mt = major << 5;
    if arg < 24 {
        out.push(mt | arg as u8);
    } else if arg <= u64::from(u8::MAX) {
        out.push(mt | 24);
        out.push(arg as u8);
    } else if arg <= u64::from(u16::MAX) {
        out.push(mt | 25);
        out.extend_from_slice(&(arg as u16).to_be_bytes());
    } else if arg <= u64::from(u32::MAX) {
        out.push(mt | 26);
        out.extend_from_slice(&(arg as u32).to_be_bytes());
    } else {
        out.push(mt | 27);
        out.extend_from_slice(&arg.to_be_bytes());
    }
}

fn encode_int(i: i128, out: &mut Vec<u8>) {
    if i >= 0 {
        encode_head(0, i as u64, out);
    } else {
        // Negative n encodes with major type 1 and argument (-1 - n).
        let arg = (-1 - i) as u64;
        encode_head(1, arg, out);
    }
}

/// Canonical map: sort entries by the *bytewise* encoding of their keys
/// (RFC 8949 §4.2.1), then emit key/value pairs in that order.
fn encode_map(pairs: &[(Value, Value)], out: &mut Vec<u8>) {
    let mut items: Vec<(Vec<u8>, Vec<u8>)> = pairs
        .iter()
        .map(|(k, v)| {
            let mut kb = Vec::new();
            k.encode_into(&mut kb);
            let mut vb = Vec::new();
            v.encode_into(&mut vb);
            (kb, vb)
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    encode_head(5, items.len() as u64, out);
    for (kb, vb) in items {
        out.extend_from_slice(&kb);
        out.extend_from_slice(&vb);
    }
}

/// Preferred (shortest) float serialization, RFC 8949 §4.2.2: use f16 if it
/// round-trips exactly, else f32 if it does, else f64.
#[allow(clippy::float_cmp)] // Exact-representability checks require bitwise-equal float comparisons.
fn encode_float(v: f64, out: &mut Vec<u8>) {
    if v.is_nan() {
        // Canonical quiet NaN.
        out.push(0xf9);
        out.extend_from_slice(&[0x7e, 0x00]);
        return;
    }
    let as_f32 = v as f32;
    if f64::from(as_f32) == v {
        let h = half::f16::from_f32(as_f32);
        if f32::from(h) == as_f32 {
            out.push(0xf9);
            out.extend_from_slice(&h.to_be_bytes());
            return;
        }
        out.push(0xfa);
        out.extend_from_slice(&as_f32.to_be_bytes());
        return;
    }
    out.push(0xfb);
    out.extend_from_slice(&v.to_be_bytes());
}

/// Decode any valid CBOR into a [`Value`]. Returns [`Error::ProtocolViolation`]
/// on malformed input (the control stream treats that as a framing violation).
pub fn decode(bytes: &[u8]) -> Result<Value> {
    let val: ciborium::value::Value =
        ciborium::de::from_reader(bytes).map_err(|_| Error::ProtocolViolation)?;
    Ok(from_ciborium(val))
}

fn from_ciborium(v: ciborium::value::Value) -> Value {
    use ciborium::value::Value as C;
    match v {
        C::Integer(i) => Value::Int(i.into()),
        C::Bytes(b) => Value::Bytes(b),
        C::Float(f) => Value::Float(f),
        C::Text(s) => Value::Text(s),
        C::Bool(b) => Value::Bool(b),
        C::Null => Value::Null,
        C::Array(a) => Value::Array(a.into_iter().map(from_ciborium).collect()),
        C::Map(m) => Value::Map(
            m.into_iter()
                .map(|(k, v)| (from_ciborium(k), from_ciborium(v)))
                .collect(),
        ),
        // Tags are unused by the protocol; unwrap to the tagged value so a
        // stray tag can never crash decode.
        C::Tag(_, inner) => from_ciborium(*inner),
        // `undefined` and any future simple value have no protocol meaning.
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(v: &Value) -> String {
        hex::encode(v.to_canonical())
    }

    #[test]
    fn uint_shortest_forms() {
        assert_eq!(hex(&Value::Int(0)), "00");
        assert_eq!(hex(&Value::Int(23)), "17");
        assert_eq!(hex(&Value::Int(24)), "1818");
        assert_eq!(hex(&Value::Int(90)), "185a");
        assert_eq!(hex(&Value::Int(255)), "18ff");
        assert_eq!(hex(&Value::Int(256)), "190100");
        assert_eq!(hex(&Value::Int(3072)), "190c00");
        assert_eq!(hex(&Value::Int(60000)), "19ea60");
        assert_eq!(hex(&Value::Int(1_000_000)), "1a000f4240");
        assert_eq!(hex(&Value::Int(i128::from(u32::MAX))), "1affffffff");
    }

    #[test]
    fn negative_ints() {
        assert_eq!(hex(&Value::Int(-1)), "20");
        assert_eq!(hex(&Value::Int(-240)), "38ef");
        assert_eq!(hex(&Value::Int(-256)), "38ff");
        assert_eq!(hex(&Value::Int(-257)), "390100");
    }

    #[test]
    fn strings_and_bytes() {
        assert_eq!(hex(&Value::Text("Quest 3".into())), "6751756573742033");
        assert_eq!(hex(&Value::Text(String::new())), "60");
        assert_eq!(hex(&Value::Bytes(vec![0x00, 0x01, 0x02])), "43000102");
        // 32-byte string uses a 1-byte length prefix (0x58 0x20).
        let b = Value::Bytes(vec![0xAB; 32]);
        assert!(hex(&b).starts_with("5820"));
    }

    #[test]
    fn simple_values() {
        assert_eq!(hex(&Value::Bool(true)), "f5");
        assert_eq!(hex(&Value::Bool(false)), "f4");
        assert_eq!(hex(&Value::Null), "f6");
    }

    #[test]
    fn float_prefers_half() {
        // 2.5 is exactly representable as f16.
        assert_eq!(hex(&Value::Float(2.5)), "f94100");
        assert_eq!(hex(&Value::Float(0.0)), "f90000");
        assert_eq!(hex(&Value::Float(1.0)), "f93c00");
        // 0.1 is not exactly representable in f16 or f32 -> full f64.
        assert_eq!(hex(&Value::Float(0.1)), "fb3fb999999999999a");
    }

    #[test]
    fn map_keys_are_bytewise_sorted() {
        // Insert out of order; canonical output must sort by encoded key.
        let m = Value::Map(vec![
            (Value::Int(2), Value::Int(0)),
            (Value::Int(0), Value::Int(0)),
            (Value::Int(1), Value::Int(0)),
        ]);
        assert_eq!(hex(&m), "a3000001000200");
        // A large key (encoded as 0x18 0x63 = 99) sorts after single-byte keys.
        let m2 = Value::Map(vec![
            (Value::Int(99), Value::Int(1)),
            (Value::Int(1), Value::Int(2)),
        ]);
        assert_eq!(hex(&m2), "a20102186301");
    }

    #[test]
    fn array_nesting() {
        let a = Value::Array(vec![
            Value::Int(1),
            Value::Array(vec![Value::Int(2), Value::Int(3)]),
        ]);
        assert_eq!(hex(&a), "8201820203");
    }

    #[test]
    fn decode_accepts_noncanonical() {
        // 1 encoded the long way (0x1b + 8 bytes) must still decode to Int(1).
        let non_canonical = hex::decode("1b0000000000000001").unwrap();
        assert_eq!(decode(&non_canonical).unwrap(), Value::Int(1));
    }

    #[test]
    fn decode_roundtrips_structures() {
        let v = Value::Map(vec![
            (Value::Int(0), Value::Int(1)),
            (Value::Int(1), Value::Text("hi".into())),
            (Value::Int(2), Value::Array(vec![Value::Bool(true), Value::Null])),
            (Value::Int(3), Value::Bytes(vec![0xDE, 0xAD])),
        ]);
        let bytes = v.to_canonical();
        assert_eq!(decode(&bytes).unwrap(), v);
    }

    #[test]
    fn decode_rejects_garbage() {
        // A bare "start of 2-array" with no elements is truncated/invalid.
        assert!(matches!(decode(&[0x82]), Err(Error::ProtocolViolation)));
    }
}
