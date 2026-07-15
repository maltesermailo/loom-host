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

#[cfg(target_os = "linux")]
use loom_capture::{I420Buffer, PortalCapture};
#[cfg(target_os = "macos")]
use loom_capture::{I420Buffer, ScreenCapture};
#[cfg(feature = "nvenc")]
use loom_encode::NvencEncoder;
use loom_encode::{AccessUnit, EncodeError, EncoderConfig, HevcEncoder};
use loom_proto::datagram;

use crate::session::MediaParams;
use testpattern::TestPattern;

/// Which frame source the media thread encodes. Selected by loomd config
/// (`--source`), not a protocol concern: the client sees identical §4/§5 wire
/// output either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CaptureSource {
    /// The synthetic test pattern (M1.2) — cross-platform, the default so the
    /// conformance/recovery tests and the Mac loopback are unaffected.
    Synthetic,
    /// Real desktop capture via the Linux portal (M1.4). Exists only in a Linux
    /// build; pops a portal picker dialog when a session starts.
    #[cfg(target_os = "linux")]
    Portal,
    /// Real desktop capture via ScreenCaptureKit (M2.1). Exists only in a macOS
    /// build; needs Screen Recording permission, which it demands loudly.
    #[cfg(target_os = "macos")]
    Sck,
}

/// The live frame source, resolved once when the media thread starts. Both arms
/// yield tightly-packed I420 planes/strides for [`HevcEncoder::encode_i420`].
enum Source {
    Synthetic(TestPattern),
    #[cfg(target_os = "linux")]
    Portal {
        capture: PortalCapture,
        frame: I420Buffer,
        have: bool,
    },
    #[cfg(target_os = "macos")]
    Sck {
        capture: ScreenCapture,
        frame: I420Buffer,
        have: bool,
    },
}

/// Which HEVC encoder loomd uses (`--encoder`). Not a protocol concern — the §5
/// output is identical either way. `Nvenc` exists only in a build with the
/// `nvenc` feature, so the CLI offers it exactly when it can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EncoderKind {
    /// Software HEVC via libx265 (M1.2) — all platforms, the default.
    X265,
    /// Hardware HEVC via NVENC (M1.5) — Linux/NVIDIA only.
    #[cfg(feature = "nvenc")]
    Nvenc,
}

/// The live encoder, resolved once when the media thread starts. Both arms take
/// the same I420 planes/strides and yield the same [`AccessUnit`].
enum VideoEncoder {
    X265(HevcEncoder),
    #[cfg(feature = "nvenc")]
    Nvenc(NvencEncoder),
}

impl VideoEncoder {
    fn encode_i420(
        &mut self,
        planes: [&[u8]; 3],
        strides: [i32; 3],
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        match self {
            VideoEncoder::X265(e) => e.encode_i420(planes, strides, pts, force_idr),
            #[cfg(feature = "nvenc")]
            VideoEncoder::Nvenc(e) => e.encode_i420(planes, strides, pts, force_idr),
        }
    }
}

fn open_encoder(kind: EncoderKind, cfg: EncoderConfig) -> Result<VideoEncoder, EncodeError> {
    match kind {
        EncoderKind::X265 => Ok(VideoEncoder::X265(HevcEncoder::new(cfg)?)),
        #[cfg(feature = "nvenc")]
        EncoderKind::Nvenc => Ok(VideoEncoder::Nvenc(NvencEncoder::new(cfg)?)),
    }
}

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

/// Spawn the media thread for a session. `source` selects the frame source;
/// `drop_percent` injects deterministic datagram loss for testing (0 = none).
pub fn spawn(
    connection: Connection,
    params: MediaParams,
    source: CaptureSource,
    encoder: EncoderKind,
    drop_percent: u32,
) -> MediaHandle {
    let (idr_tx, idr_rx) = mpsc::channel();
    let join =
        std::thread::spawn(move || run(connection, params, source, encoder, drop_percent, idr_rx));
    MediaHandle {
        idr_tx,
        join: Some(join),
    }
}

fn run(
    connection: Connection,
    params: MediaParams,
    source_kind: CaptureSource,
    encoder_kind: EncoderKind,
    drop_percent: u32,
    idr_rx: Receiver<()>,
) {
    let (w, h) = (params.width as usize, params.height as usize);
    let cfg = constraints::encoder_config(
        params.width as u32,
        params.height as u32,
        params.refresh as u32,
        params.bitrate_kbps as u32,
    );
    let mut encoder = match open_encoder(encoder_kind, cfg) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(target: "loom::media", error = %e, "encoder open failed");
            return;
        }
    };

    let mut source = match open_source(source_kind, &params) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "loom::media", error = %e, "capture open failed");
            return;
        }
    };
    let mut drop_gen = DropInjector::new(drop_percent);
    let mut frame_seq: u32 = 0;
    let interval = Duration::from_secs_f64(1.0 / params.refresh.max(1) as f64);
    let mut next = Instant::now();

    // Group this session's media events under one span (safe to enter: the media
    // thread is synchronous, no await points).
    let span = tracing::info_span!(
        "media_session",
        width = w,
        height = h,
        refresh = params.refresh
    );
    let _guard = span.enter();
    tracing::info!(target: "loom::media", event = "media_start", drop_percent,
        "media thread started");

    loop {
        if connection.close_reason().is_some() {
            break;
        }
        // Coalesce any pending IDR requests into one forced IDR.
        let force_idr = idr_rx.try_iter().count() > 0;

        // Produce this tick's I420 frame. Portal capture is damage-driven, so
        // before its first frame we simply wait a tick; once it has delivered,
        // the held frame repeats when nothing new arrived (§5.6 freshness).
        let (planes, strides);
        match &mut source {
            Source::Synthetic(pattern) => {
                pattern.render(frame_seq);
                planes = pattern.planes();
                strides = pattern.strides();
            }
            #[cfg(target_os = "linux")]
            Source::Portal {
                capture,
                frame,
                have,
            } => {
                if capture.fill(frame) {
                    *have = true;
                }
                if !*have {
                    pace(&mut next, interval);
                    continue;
                }
                planes = frame.planes();
                strides = frame.strides();
            }
            #[cfg(target_os = "macos")]
            Source::Sck {
                capture,
                frame,
                have,
            } => {
                if capture.fill(frame) {
                    *have = true;
                }
                if !*have {
                    pace(&mut next, interval);
                    continue;
                }
                planes = frame.planes();
                strides = frame.strides();
            }
        }

        let capture_ts = crate::clock::host_now_us();
        match encoder.encode_i420(planes, strides, frame_seq as i64, force_idr) {
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

        pace(&mut next, interval);
    }
    tracing::info!(target: "loom::media", event = "media_stop", frames = frame_seq);
}

/// Resolve the configured [`CaptureSource`] into a live [`Source`]. Portal
/// capture blocks here through the picker dialog + first format negotiation, so
/// a size mismatch or cancellation surfaces before the encode loop starts.
fn open_source(
    kind: CaptureSource,
    params: &MediaParams,
) -> Result<Source, Box<dyn std::error::Error>> {
    let (w, h) = (params.width as u32, params.height as u32);

    match kind {
        CaptureSource::Synthetic => Ok(Source::Synthetic(TestPattern::new(w as usize, h as usize))),
        #[cfg(target_os = "linux")]
        CaptureSource::Portal => {
            let capture = PortalCapture::start(w, h, params.refresh as u32)?;
            Ok(Source::Portal {
                capture,
                frame: I420Buffer::new(w, h),
                have: false,
            })
        }
        #[cfg(target_os = "macos")]
        CaptureSource::Sck => {
            let capture = ScreenCapture::start(w, h, params.refresh as u32)?;
            Ok(Source::Sck {
                capture,
                frame: I420Buffer::new(w, h),
                have: false,
            })
        }
    }
}

/// Sleep until the next frame deadline, dropping accumulated debt if we fell
/// behind (the encoder paces itself at the configured refresh, §5.6).
fn pace(next: &mut Instant, interval: Duration) {
    *next += interval;

    let now = Instant::now();
    if *next > now {
        std::thread::sleep(*next - now);
    } else {
        *next = now;
    }
}

/// Deterministic per-datagram loss injector (`--drop-percent`). A fixed-seed
/// xorshift64 keeps loss reproducible across runs so the recovery test is stable.
struct DropInjector {
    state: u64,
    percent: u32,
}

impl DropInjector {
    fn new(percent: u32) -> Self {
        Self {
            state: 0x9E37_79B9_7F4A_7C15,
            percent,
        }
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
