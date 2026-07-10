//! vector-adapter — the `loom-proto` conformance adapter (VECTORS.md §2/§3).
//!
//! Invoked as `vector-adapter <category>` with a vector file's JSON on stdin; it
//! runs each case's `op` against the `loom-proto` library and prints
//! `{"results": [ <one per case, in order> ]}` on stdout. This is the only place
//! in `loom-proto` that does I/O; the library itself stays pure.
//!
//! Keymap cases require the spec's `keymaps/*.csv` at runtime (VECTORS.md §3:
//! "Adapters MUST derive their tables from `keymaps/*.csv` … never from an
//! independent copy"). We locate that directory relative to this executable and
//! the working directory (see [`find_keymaps_dir`]).

use std::io::Read;
use std::path::PathBuf;

use serde_json::{json, Value as J};

use loom_proto::cbor::Value as C;
use loom_proto::{clocksync, control, datagram, keymap, reassembly};

fn main() {
    let category = std::env::args()
        .nth(1)
        .expect("usage: vector-adapter <category>");

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read stdin");
    let doc: J = serde_json::from_str(&input).expect("parse vector JSON");
    let cases = doc
        .get("cases")
        .and_then(J::as_array)
        .expect("vector file has a cases array");

    // Load keymap tables once if this is the keymap category.
    let keymaps = (category == "keymap").then(load_keymaps);

    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let op = case.get("op").and_then(J::as_str).expect("case has op");
        let input = case.get("input").unwrap_or(&J::Null);
        let result = match category.as_str() {
            "datagram" => match op {
                "encode" => datagram_encode(input),
                "decode" => datagram_decode(input),
                other => panic!("unknown datagram op {other}"),
            },
            "control" => match op {
                "encode" => control_encode(input),
                "decode" => control_decode(input),
                other => panic!("unknown control op {other}"),
            },
            "reassembly" => reassembly_trace(input),
            "clocksync" => clocksync_series(input),
            "keymap" => {
                let (ak2ev, ev2cg) = keymaps.as_ref().expect("keymaps loaded");
                match op {
                    "akeycode_to_evdev" => keymap_lookup(input, ak2ev),
                    "evdev_to_cgkeycode" => keymap_lookup(input, ev2cg),
                    other => panic!("unknown keymap op {other}"),
                }
            }
            other => panic!("unknown category {other}"),
        };
        results.push(result);
    }

    println!("{}", json!({ "results": results }));
}

// ---------------------------------------------------------------- datagram

fn datagram_encode(input: &J) -> J {
    let keyframe = input["flags_keyframe"].as_bool().unwrap();
    let stream_id = input["stream_id"].as_u64().unwrap() as u16;
    let frame_seq = input["frame_seq"].as_u64().unwrap() as u32;
    let frag_index = input["frag_index"].as_u64().unwrap() as u16;
    let frag_count = input["frag_count"].as_u64().unwrap() as u16;
    let payload = hex::decode(input["payload"].as_str().unwrap()).unwrap();

    let header = datagram::DatagramHeader::new(keyframe, stream_id, frame_seq, frag_index, frag_count);
    json!({ "hex": hex::encode(header.encode(&payload)) })
}

fn datagram_decode(input: &J) -> J {
    let bytes = hex::decode(input["hex"].as_str().unwrap()).unwrap();
    match datagram::decode(&bytes) {
        Ok(d) => {
            let h = d.header;
            json!({
                "ok": true,
                "header": {
                    "flags_keyframe": h.keyframe,
                    "flags_last": h.last_fragment,
                    "stream_id": h.stream_id,
                    "frame_seq": h.frame_seq,
                    "frag_index": h.frag_index,
                    "frag_count": h.frag_count,
                    "payload_len": d.payload_len,
                }
            })
        }
        Err(reason) => json!({ "ok": false, "reason": reason.as_str() }),
    }
}

// ---------------------------------------------------------------- control

fn control_encode(input: &J) -> J {
    let msg_type = input["msg_type"].as_u64().unwrap();
    let body = match json_to_cbor(&input["body"]) {
        C::Map(pairs) => pairs,
        other => panic!("control encode body must be a map, got {other:?}"),
    };
    json!({ "hex": hex::encode(control::encode_frame(msg_type, &body)) })
}

fn control_decode(input: &J) -> J {
    let bytes = hex::decode(input["hex"].as_str().unwrap()).unwrap();
    match control::decode_frame(&bytes) {
        Ok(control::Decoded::Message { msg_type, body }) => json!({
            "ok": true,
            "msg_type": msg_type,
            "body": cbor_to_json(&C::Map(body)),
        }),
        Ok(control::Decoded::Ignored) => json!({ "ok": true, "ignored": true }),
        Err(_) => json!({ "ok": false, "error": "PROTOCOL_VIOLATION" }),
    }
}

// ---------------------------------------------------------------- reassembly

fn reassembly_trace(input: &J) -> J {
    let mut r = reassembly::Reassembler::new();
    for d in input["trace"].as_array().unwrap() {
        let t_ms = d["t_ms"].as_i64().unwrap();
        let frag = reassembly::Fragment {
            frame_seq: d["frame_seq"].as_u64().unwrap() as u32,
            frag_index: d["frag_index"].as_u64().unwrap() as u16,
            frag_count: d["frag_count"].as_u64().unwrap() as u16,
            keyframe: d["keyframe"].as_bool().unwrap(),
        };
        r.push(t_ms, frag);
    }

    let events: Vec<J> = r
        .events()
        .iter()
        .map(|e| match e {
            reassembly::Event::Deliver {
                t_ms,
                frame_seq,
                keyframe,
            } => json!({
                "t_ms": t_ms,
                "ev": "deliver",
                "frame_seq": frame_seq,
                "keyframe": keyframe,
            }),
            reassembly::Event::IdrRequest { t_ms, last_good } => json!({
                "t_ms": t_ms,
                "ev": "idr_request",
                "last_good": last_good,
            }),
        })
        .collect();

    let c = r.counters();
    json!({
        "events": events,
        "counters": {
            "dropped_incomplete": c.dropped_incomplete,
            "discarded_gap": c.discarded_gap,
            "stale_fragments": c.stale_fragments,
        }
    })
}

// ---------------------------------------------------------------- clocksync

fn clocksync_series(input: &J) -> J {
    let mut f = clocksync::ClockFilter::new();
    let estimates: Vec<J> = input["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let s = s.as_array().unwrap();
            let g = |i: usize| s[i].as_i64().unwrap();
            let e = f.push(g(0), g(1), g(2), g(3));
            json!({ "rtt": e.rtt, "offset": e.offset })
        })
        .collect();
    json!({ "estimates": estimates })
}

// ---------------------------------------------------------------- keymap

fn keymap_lookup(input: &J, table: &keymap::Keymap) -> J {
    let code = input["code"].as_i64().unwrap();
    match table.get(code) {
        Some(v) => json!({ "code": v }),
        None => json!({ "code": J::Null }),
    }
}

/// Load both keymap tables from the spec's `keymaps/*.csv`.
fn load_keymaps() -> (keymap::Keymap, keymap::Keymap) {
    let dir = find_keymaps_dir().unwrap_or_else(|| {
        panic!(
            "could not locate keymaps/ directory (set LOOM_KEYMAPS_DIR or run from the host repo root)"
        )
    });
    let read = |name: &str| {
        let path = dir.join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        keymap::Keymap::from_csv(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    };
    (
        read("akeycode_to_evdev.csv"),
        read("evdev_to_cgkeycode.csv"),
    )
}

/// Locate the spec `keymaps/` directory. Tries, in order: `$LOOM_KEYMAPS_DIR`,
/// paths relative to this executable (`target/<profile>/vector-adapter` →
/// `../../spec/keymaps` etc.), and paths relative to the current directory.
/// The first candidate that actually contains `akeycode_to_evdev.csv` wins, so
/// the adapter works regardless of where the harness launches it from.
fn find_keymaps_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(dir) = std::env::var("LOOM_KEYMAPS_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // target/<profile>/vector-adapter -> repo root is two levels up;
            // spec is a submodule there, and a sibling of the repo one more up.
            candidates.push(dir.join("../../spec/keymaps"));
            candidates.push(dir.join("../../../spec/keymaps"));
        }
    }
    candidates.push(PathBuf::from("spec/keymaps")); // cwd = host repo root (submodule)
    candidates.push(PathBuf::from("../spec/keymaps")); // cwd = host repo root (sibling spec)

    candidates
        .into_iter()
        .find(|c| c.join("akeycode_to_evdev.csv").is_file())
}

// ---------------------------------------------------------------- JSON <-> CBOR

/// Convert a vector-JSON body value into a CBOR [`C`] value. Byte strings appear
/// as `{"$hex": "..."}`; CBOR integer map keys appear as JSON string keys.
fn json_to_cbor(j: &J) -> C {
    match j {
        J::Null => C::Null,
        J::Bool(b) => C::Bool(*b),
        J::Number(n) => {
            if let Some(u) = n.as_u64() {
                C::Int(i128::from(u))
            } else if let Some(i) = n.as_i64() {
                C::Int(i128::from(i))
            } else {
                C::Float(n.as_f64().unwrap())
            }
        }
        J::String(s) => C::Text(s.clone()),
        J::Array(a) => C::Array(a.iter().map(json_to_cbor).collect()),
        J::Object(m) => {
            if m.len() == 1 {
                if let Some(J::String(h)) = m.get("$hex") {
                    return C::Bytes(hex::decode(h).expect("valid $hex"));
                }
            }
            let pairs = m
                .iter()
                .map(|(k, v)| {
                    let key = match k.parse::<i128>() {
                        Ok(i) => C::Int(i),
                        Err(_) => C::Text(k.clone()),
                    };
                    (key, json_to_cbor(v))
                })
                .collect();
            C::Map(pairs)
        }
    }
}

/// Convert a CBOR [`C`] value back to vector-JSON (inverse of [`json_to_cbor`]).
fn cbor_to_json(c: &C) -> J {
    match c {
        C::Int(i) => int_to_json(*i),
        C::Bytes(b) => json!({ "$hex": hex::encode(b) }),
        C::Text(s) => J::String(s.clone()),
        C::Array(a) => J::Array(a.iter().map(cbor_to_json).collect()),
        C::Map(pairs) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in pairs {
                let key = match k {
                    C::Int(i) => i.to_string(),
                    C::Text(s) => s.clone(),
                    other => panic!("non-string/int map key: {other:?}"),
                };
                obj.insert(key, cbor_to_json(v));
            }
            J::Object(obj)
        }
        C::Bool(b) => J::Bool(*b),
        C::Float(f) => J::Number(serde_json::Number::from_f64(*f).expect("finite float")),
        C::Null => J::Null,
    }
}

/// Represent an `i128` CBOR integer as a JSON number (covers the full u64 and
/// i64 ranges used by the protocol).
fn int_to_json(i: i128) -> J {
    if i >= 0 && i <= i128::from(u64::MAX) {
        json!(i as u64)
    } else if i >= i128::from(i64::MIN) && i <= i128::from(i64::MAX) {
        json!(i as i64)
    } else {
        // Outside both ranges: not used by the protocol, but stay lossless-ish.
        json!(i.to_string())
    }
}
