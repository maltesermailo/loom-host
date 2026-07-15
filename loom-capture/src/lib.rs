//! loom-capture — screen capture for the Loom host.
//!
//! M1.4 ships the Linux path: xdg-desktop-portal **ScreenCast** of a physical
//! monitor → PipeWire → I420 frames the encoder consumes exactly as it consumed
//! the synthetic test pattern (spec/ARCHITECTURE.md §5.1). Per the R2 spike
//! verdict (`spikes/r2-dmabuf/VERDICT.md`) this is the **SHM path**: portal
//! delivers packed BGRx/BGRA, which [`convert`] turns into I420 at a ~2–3 ms
//! cost. dmabuf→CUDA zero-copy is a deferred M1.5+ optimization — TODO(R2).
//!
//! [`PortalCapture`] owns the capture thread. Frame delivery is damage-driven
//! (not 72 fps): the newest frame lives in a shared slot, and `loomd`'s media
//! loop does the pacing and last-frame repeat. A portal picker dialog appears
//! each time capture starts; restore-token persistence is deferred (M1.4+).
//!
//! M2.1 ships the macOS twin: ScreenCaptureKit → `420v` → I420, in [`sck`].
//! [`PortalCapture`] and [`ScreenCapture`] are platform-exclusive — each exists
//! only in its own target's build — and share [`I420Buffer`] and
//! [`CaptureError`]. They deliberately expose the same `start`/`fill` shape, so
//! `loomd`'s media loop treats them identically.
//!
//! `deny` rather than `forbid`: the objc2 bindings are unsafe by construction, so
//! [`sck`] alone opts back in. Every other module — including the whole Linux
//! path — stays unsafe-free.
#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod convert;
mod frame;
#[cfg(target_os = "macos")]
mod nv12;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "macos")]
mod sck;
#[cfg(target_os = "linux")]
mod stream;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::mpsc;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::thread::JoinHandle;

pub use frame::I420Buffer;
#[cfg(target_os = "macos")]
pub use sck::ScreenCapture;

/// Errors from the capture pipeline.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The portal ScreenCast handshake failed (or the picker was cancelled).
    #[error("portal ScreenCast: {0}")]
    Portal(String),
    /// PipeWire stream setup or negotiation failed.
    #[error("pipewire: {0}")]
    Pipewire(String),
    /// The portal negotiated a resolution other than the configured one. M1.4
    /// has no scaler; set `--width/--height` to the monitor's native size.
    #[error("capture size {got:?} != configured {want:?}; set --width/--height to match the monitor (M1.4 has no scaler)")]
    SizeMismatch {
        /// The resolution the portal delivered.
        got: (u32, u32),
        /// The resolution `loomd` was configured for.
        want: (u32, u32),
    },
    /// The capture thread ended before reporting readiness.
    #[error("capture thread exited during startup")]
    StartupAborted,
    /// Screen Recording permission (TCC) is not granted. macOS attributes the
    /// permission to the *launching* application, so when `loomd` is started from
    /// a shell it is the terminal that must be listed — hence the hint.
    #[error(
        "Screen Recording permission denied.\n       \
         Grant it in System Settings → Privacy & Security → Screen & System Audio Recording,\n       \
         then restart the app you launched loomd from (the permission follows the terminal,\n       \
         not the loomd binary)."
    )]
    PermissionDenied,
    /// ScreenCaptureKit setup or streaming failed.
    #[error("ScreenCaptureKit: {0}")]
    ScreenCaptureKit(String),
}

#[cfg(target_os = "linux")]
impl CaptureError {
    fn portal(e: impl std::fmt::Display) -> Self {
        CaptureError::Portal(e.to_string())
    }
}

/// A running portal ScreenCast capture. Frames flow into a shared slot; poll the
/// newest with [`Self::fill`]. Dropping it stops the capture thread.
#[cfg(target_os = "linux")]
pub struct PortalCapture {
    slot: stream::FrameSlot,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PortalCapture {
    /// Start capturing a user-selected monitor at `width`×`height`. Blocks
    /// through the portal handshake (picker dialog) and the first format
    /// negotiation, so a size mismatch or cancellation surfaces here.
    pub fn start(width: u32, height: u32, refresh: u32) -> Result<Self, CaptureError> {
        let expected = (width, height);

        // The portal handshake is async; run it to completion on a throwaway
        // current-thread runtime, then hand the fd to the sync PipeWire thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CaptureError::Portal(format!("tokio runtime: {e}")))?;
        let (fd, node_id) = runtime.block_on(portal::open_screencast())?;
        drop(runtime);

        let slot: stream::FrameSlot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();

        let thread = {
            let (slot, stop) = (slot.clone(), stop.clone());
            std::thread::Builder::new()
                .name("loom-capture".into())
                .spawn(move || stream::run(fd, node_id, expected, refresh, slot, stop, ready_tx))
                .map_err(|e| CaptureError::Pipewire(format!("spawn: {e}")))?
        };

        // Wait for the first format event (Ok) or a startup failure (Err).
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                slot,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(CaptureError::StartupAborted)
            }
        }
    }

    /// Copy the newest captured frame into `dst`, returning `true` if a frame was
    /// available. When it returns `false` (before the first frame), the caller
    /// keeps whatever it had — the damage-driven repeat that satisfies §5.6.
    pub fn fill(&self, dst: &mut I420Buffer) -> bool {
        match self.slot.lock().expect("frame slot poisoned").as_ref() {
            Some(frame) => {
                frame.copy_into(dst);
                true
            }
            None => false,
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
