//! Synthetic media path (M1.2): test pattern → HEVC encode → §4 fragmentation →
//! QUIC datagrams, on a dedicated per-session OS thread (libx265 encode is
//! blocking, so it stays off the tokio runtime).
//!
//! The thread owns a cloned `quinn::Connection` and sends datagrams directly. It
//! stops when the connection closes (checked each frame). An IDR request from
//! the control task arrives over an mpsc channel and forces the next frame to an
//! IDR. Structured `tracing` events (`target: "loom::media"`) record every sent
//! frame and forced IDR so the M1.2 recovery test can parse both sides' logs.

pub mod constraints;
pub mod testpattern;

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::Connection;

use loom_encode::HevcEncoder;
use loom_proto::datagram;

use crate::session::MediaParams;
use testpattern::TestPattern;

/// Handle to a running media thread. Dropping it detaches; [`Self::join`] waits.
pub struct MediaHandle {
    idr_tx: Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl MediaHandle {
    /// Ask the encoder to emit an IDR on its next frame (§3.6).
    pub fn request_idr(&self) {
        let _ = self.idr_tx.send(());
    }

    /// Block until the media thread has stopped.
    pub fn join(mut self) {
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the media thread for a session. `drop_percent` injects deterministic
/// datagram loss for testing (0 = none).
pub fn spawn(connection: Connection, params: MediaParams, drop_percent: u32) -> MediaHandle {
    let (idr_tx, idr_rx) = mpsc::channel();
    let join = std::thread::spawn(move || run(connection, params, drop_percent, idr_rx));
    MediaHandle { idr_tx, join: Some(join) }
}

fn run(connection: Connection, params: MediaParams, drop_percent: u32, idr_rx: Receiver<()>) {
    let (w, h) = (params.width as usize, params.height as usize);
    let cfg = constraints::encoder_config(
        params.width as u32,
        params.height as u32,
        params.refresh as u32,
        params.bitrate_kbps as u32,
    );
    let mut encoder = match HevcEncoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(target: "loom::media", error = %e, "encoder open failed");
            return;
        }
    };

    let mut pattern = TestPattern::new(w, h);
    let mut drop_gen = DropInjector::new(drop_percent);
    let mut frame_seq: u32 = 0;
    let epoch = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / params.refresh.max(1) as f64);
    let mut next = Instant::now();

    tracing::info!(target: "loom::media", event = "media_start", width = w, height = h,
        refresh = params.refresh, drop_percent, "media thread started");

    loop {
        if connection.close_reason().is_some() {
            break;
        }
        // Coalesce any pending IDR requests into one forced IDR.
        let force_idr = idr_rx.try_iter().count() > 0;

        pattern.render(frame_seq);
        let capture_ts = epoch.elapsed().as_micros() as u64;
        match encoder.encode_i420(pattern.planes(), pattern.strides(), frame_seq as i64, force_idr) {
            Ok(Some(au)) => {
                if force_idr {
                    tracing::info!(target: "loom::media", event = "idr_forced", frame_seq);
                }
                // §4.1 frame body: capture_ts (u64 BE) ‖ Annex-B access unit.
                let mut body = Vec::with_capacity(8 + au.data.len());
                body.extend_from_slice(&capture_ts.to_be_bytes());
                body.extend_from_slice(&au.data);

                let frags = datagram::fragment(0, frame_seq, au.keyframe, &body);
                let total = frags.len();
                let mut sent = 0usize;
                for dg in frags {
                    if drop_gen.should_drop() {
                        continue;
                    }
                    if connection.send_datagram(Bytes::from(dg)).is_err() {
                        break;
                    }
                    sent += 1;
                }
                tracing::info!(target: "loom::media", event = "frame_sent", frame_seq,
                    keyframe = au.keyframe, frags = total, sent, capture_ts);
                frame_seq = frame_seq.wrapping_add(1);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(target: "loom::media", error = %e, "encode failed");
                break;
            }
        }

        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't accumulate debt
        }
    }
    tracing::info!(target: "loom::media", event = "media_stop", frames = frame_seq);
}

/// Deterministic per-datagram loss injector (`--drop-percent`). A fixed-seed
/// xorshift64 keeps loss reproducible across runs so the recovery test is stable.
struct DropInjector {
    state: u64,
    percent: u32,
}

impl DropInjector {
    fn new(percent: u32) -> Self {
        Self { state: 0x9E37_79B9_7F4A_7C15, percent }
    }

    fn should_drop(&mut self) -> bool {
        if self.percent == 0 {
            return false;
        }
        // xorshift64
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x % 100) < self.percent as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_injector_off_never_drops() {
        let mut d = DropInjector::new(0);
        assert!((0..1000).all(|_| !d.should_drop()));
    }

    #[test]
    fn drop_injector_rate_is_in_the_ballpark() {
        let mut d = DropInjector::new(10);
        let dropped = (0..10_000).filter(|_| d.should_drop()).count();
        // Deterministic seed; just assert it's roughly 10% (5%..15%).
        assert!((500..1500).contains(&dropped), "dropped {dropped}/10000");
    }
}
