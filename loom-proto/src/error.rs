//! Library error type (`thiserror`).
//!
//! Note the deliberate split: **datagram** decode failures are *silent drops*
//! in production (PROTOCOL.md §6.6), so [`crate::datagram::decode`] returns a
//! [`crate::datagram::DropReason`] rather than an [`Error`]. Only the control
//! stream and the runtime-parsed keymaps surface real errors here.

/// Errors produced by the control-stream and keymap paths.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A control frame violated the framing or envelope rules (PROTOCOL.md §3,
    /// §6.6). This maps to the QUIC/`ERROR` code `PROTOCOL_VIOLATION` (0x04).
    #[error("protocol violation")]
    ProtocolViolation,

    /// A keymap CSV row could not be parsed.
    #[error("keymap parse error: {0}")]
    Keymap(String),
}

/// Convenience alias for fallible control/keymap operations.
pub type Result<T> = std::result::Result<T, Error>;
