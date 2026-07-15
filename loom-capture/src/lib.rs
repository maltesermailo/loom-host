//! loom-capture — screen capture for the Loom host.
//!
//! M1.4 target: xdg-desktop-portal ScreenCast of a physical monitor → PipeWire →
//! I420 frames the encoder consumes exactly as it consumed the synthetic test
//! pattern (spec/ARCHITECTURE.md §5.1). Per the R2 spike verdict
//! (`spikes/r2-dmabuf/VERDICT.md`) this is the **SHM path**: the portal delivers
//! packed BGRx/BGRA, which [`convert`] turns into I420 at a ~2–3 ms cost.
//! dmabuf→CUDA zero-copy is a deferred M1.5+ optimization — TODO(R2).
//!
//! This commit lands the pure, dependency-light core (the I420 frame buffer and
//! the color converter, both unit-tested). The portal + PipeWire capture thread
//! that drives them lands next. The macOS ScreenCaptureKit backend arrives in
//! M2.1.
#![forbid(unsafe_code)]

pub mod convert;
mod frame;

pub use frame::I420Buffer;
