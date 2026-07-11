//! The §5 encoder constraints, in exactly one place.
//!
//! The ROADMAP mandates a single constants module that both the encoder config
//! and its validation test import, so "what we told the encoder" and "what the
//! conformance test asserts" cannot drift. This is that module. The media path
//! builds its [`loom_encode::EncoderConfig`] here; the bitstream-conformance
//! test (M1.2) reads these same values to know what to check.

use loom_encode::EncoderConfig;

/// Max consecutive B-frames (§5.3: none).
pub const BFRAMES: u32 = 0;
/// Max reference frames (§5.3: single ref).
pub const MAX_REF: u32 = 1;
/// Intra period. `i32::MAX` gives the §5 infinite GOP — IDRs only at start and
/// on request, never periodically.
pub const KEYINT_MAX: i32 = i32::MAX;
/// Repeat VPS/SPS/PPS with every IDR (§5.2).
pub const REPEAT_HEADERS: bool = true;
/// Scene-cut-triggered I-frames. §5.4 requires this OFF.
pub const SCENECUT: bool = false;

/// Build the §5-conforming encoder config for the given stream parameters.
pub fn encoder_config(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps_num: fps,
        fps_den: 1,
        bitrate_kbps,
        bframes: BFRAMES,
        max_ref: MAX_REF,
        keyint_max: KEYINT_MAX,
        repeat_headers: REPEAT_HEADERS,
        scenecut: SCENECUT,
    }
}
