//! Protocol error codes — PROTOCOL.md §10.
//!
//! These integers are used both as the `code` in an ERROR (0x40) message body
//! (§3.9) and as QUIC application close codes. They live here, next to the rest
//! of the wire vocabulary, so neither `loomd` nor the C++ client hand-rolls
//! them (the DRY rule: wire constants have exactly one home per implementation).

/// Clean close (after BYE).
pub const NONE: u64 = 0x00;
/// HELLO `protocol_version` not acceptable.
pub const VERSION_UNSUPPORTED: u64 = 0x01;
/// Host already has an active session.
pub const BUSY: u64 = 0x02;
/// Client offered no codec the host can encode.
pub const NO_COMMON_CODEC: u64 = 0x03;
/// Framing / state-machine violation.
pub const PROTOCOL_VIOLATION: u64 = 0x04;
/// Peer lacks QUIC datagram support.
pub const DATAGRAM_UNSUPPORTED: u64 = 0x05;
/// Certificate not pinned / pairing required (PAIRING.md).
pub const AUTH_FAILED: u64 = 0x06;
/// Unrecoverable local error (encoder death, capture loss > 5 s, …).
pub const INTERNAL: u64 = 0x07;

/// The stable name for a code, for logs/UI. Unknown codes read as `INTERNAL`
/// per §10 ("Unknown codes MUST be treated as INTERNAL").
pub fn name(code: u64) -> &'static str {
    match code {
        NONE => "NONE",
        VERSION_UNSUPPORTED => "VERSION_UNSUPPORTED",
        BUSY => "BUSY",
        NO_COMMON_CODEC => "NO_COMMON_CODEC",
        PROTOCOL_VIOLATION => "PROTOCOL_VIOLATION",
        DATAGRAM_UNSUPPORTED => "DATAGRAM_UNSUPPORTED",
        AUTH_FAILED => "AUTH_FAILED",
        _ => "INTERNAL",
    }
}
