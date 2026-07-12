//! Host clock domain — PROTOCOL.md §1.2.
//!
//! All media timestamps and CLOCK_PONG replies are microseconds since an
//! arbitrary, monotonic epoch fixed for the process. Using one process-global
//! epoch (rather than per-connection) is a valid superset of the spec's
//! "fixed for the connection" and guarantees the media path's `capture_ts` and
//! the control task's CLOCK_PONG timestamps share exactly one clock — which is
//! what makes the client's end-to-end latency computation (§7 + capture_ts)
//! correct.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the process's monotonic epoch (fixed on first call).
pub fn host_now_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}
