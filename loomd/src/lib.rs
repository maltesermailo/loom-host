//! loomd — the Loom host daemon (quinn server + session management).
//!
//! Library crate holding the daemon's building blocks. M1.1 lands them in two
//! commits: first the pure session state machine, then the quinn runtime that
//! drives it. See spec/ROADMAP.md.

pub mod session;
