//! loom-proto — the Loom wire protocol.
//!
//! An independent, self-contained implementation of the contract in
//! `spec/PROTOCOL.md` and `spec/PAIRING.md`: video/audio datagram framing (§4),
//! the length-prefixed CBOR control stream (§3), the client receive model —
//! reassembly, decode-gating and IDR-request logic (§6 + §3.6), and the
//! clock-sync min-filter (§7). Keymap tables (§3.5) are parsed from the spec's
//! `keymaps/*.csv` at runtime.
//!
//! By design this crate has **no I/O, no async, and no QUIC types** — it is
//! pure encode/decode/state-machine logic that both the host and (independently)
//! the C++ client must agree with, byte-for-byte, via the conformance vectors.
#![forbid(unsafe_code)]

pub mod cbor;
pub mod clocksync;
pub mod control;
pub mod datagram;
pub mod error;
pub mod errors;
pub mod keymap;
pub mod reassembly;

pub use error::{Error, Result};

/// The protocol version this crate speaks (PROTOCOL.md §3.4, HELLO key 0).
pub const PROTOCOL_VERSION: u64 = 1;
