//! loomd — the Loom host daemon (quinn server + session management).
//!
//! This library crate holds the daemon's building blocks so they can be
//! integration-tested (`tests/handshake.rs` drives a quinn client against the
//! same [`conn::handle`] the binary uses). The `loomd` binary in `main.rs` is a
//! thin CLI over [`endpoint::accept_loop`].
//!
//! Layering (M1.1):
//! - [`session`] — the sans-io state machine (pure; all protocol decisions).
//! - [`conn`]    — per-connection async driver (transport I/O only).
//! - [`endpoint`]— quinn endpoint construction + the accept loop.
//! - [`tls`]     — dev self-signed certs; peer verification skipped (TODO M7).

pub mod conn;
pub mod endpoint;
pub mod session;
pub mod tls;

/// Boxed error alias for the daemon's fallible setup paths.
pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;
