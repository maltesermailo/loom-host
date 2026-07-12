//! §5 bitstream-conformance test (M1.2 accept).
//!
//! Encodes the synthetic test pattern with the media path's real encoder config,
//! then validates the Annex-B output two ways:
//!   * a **reference decoder** (ffprobe → libavcodec) reports per-frame picture
//!     type and keyframe flag — asserting no B-frames and that IDRs appear only
//!     at start and on request; and
//!   * a lightweight NAL-type census confirms VPS/SPS/PPS ride with every IDR
//!     (§5.2) and no unexpected IDRs exist.
//!
//! The expected §5 values come from `loomd`'s one constraints module, the same
//! source the encoder config uses — so the assertion can't drift from the policy.

use std::io::Write;
use std::process::Command;

use loom_encode::HevcEncoder;
use loomd::media::{constraints, testpattern::TestPattern};

const W: usize = 320;
const H: usize = 240;
const FRAMES: u32 = 12;
const FORCE_IDR_AT: u32 = 6;

/// Encode `FRAMES` frames, forcing an IDR at `FORCE_IDR_AT`, into one Annex-B blob.
fn encode_stream() -> Vec<u8> {
    let cfg = constraints::encoder_config(W as u32, H as u32, 72, 2000);
    let mut enc = HevcEncoder::new(cfg).expect("open encoder");
    let mut pat = TestPattern::new(W, H);
    let mut out = Vec::new();
    for n in 0..FRAMES {
        pat.render(n);
        let force = n == FORCE_IDR_AT;
        if let Some(au) = enc
            .encode_i420(pat.planes(), pat.strides(), n as i64, force)
            .expect("encode")
        {
            out.extend_from_slice(&au.data);
        }
    }
    out
}

/// Iterate (nal_type) over an Annex-B byte stream by splitting on start codes.
fn nal_types(stream: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        // Match a 3- or 4-byte start code.
        let sc3 = stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1;
        let sc4 = stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 0 && stream[i + 3] == 1;
        if sc3 || sc4 {
            let nal_start = i + if sc4 { 4 } else { 3 };
            if nal_start < stream.len() {
                // HEVC NAL header: forbidden_zero(1) | type(6) | ... → (byte>>1)&0x3f.
                types.push((stream[nal_start] >> 1) & 0x3f);
            }
            i = nal_start;
        } else {
            i += 1;
        }
    }
    types
}

/// `(pict_type, key_frame)` per frame via ffprobe (the libavcodec reference).
fn ffprobe_frames(stream: &[u8]) -> Vec<(String, bool)> {
    let mut path = std::env::temp_dir();
    path.push(format!("loom_conf_{}.hevc", std::process::id()));
    std::fs::File::create(&path)
        .and_then(|mut f| f.write_all(stream))
        .expect("write temp bitstream");

    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pict_type,key_frame",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("run ffprobe (is ffmpeg installed?)");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            // Fields are pict_type and key_frame in some order across versions;
            // normalise: the letter is the pict_type, the digit the key flag.
            let mut pict = String::new();
            let mut key = false;
            for field in l.split(',') {
                let f = field.trim();
                match f {
                    "0" => key = false,
                    "1" => key = true,
                    other => pict = other.to_string(),
                }
            }
            (pict, key)
        })
        .collect()
}

#[test]
#[allow(clippy::assertions_on_constants)] // intentional: pin the DRY constants to §5
fn constraints_are_section5() {
    // The DRY constants themselves encode §5.3.
    assert_eq!(constraints::BFRAMES, 0, "§5.3: no B-frames");
    assert_eq!(constraints::MAX_REF, 1, "§5.3: single reference");
    assert!(constraints::REPEAT_HEADERS, "§5.2: repeat VPS/SPS/PPS");
    assert!(!constraints::SCENECUT, "§5.4: no scenecut IDRs");
}

#[test]
fn no_b_frames_and_idr_only_at_start_and_on_request() {
    let stream = encode_stream();
    let frames = ffprobe_frames(&stream);
    assert!(!frames.is_empty(), "decoder saw no frames");

    // §5.3: no B-frames anywhere.
    assert!(
        frames.iter().all(|(t, _)| t != "B"),
        "found a B-frame: {frames:?}"
    );

    // §5.4/§5 IDR placement: keyframes only at index 0 (start) and FORCE_IDR_AT.
    let keyframe_indices: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, (_, key))| *key)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        keyframe_indices,
        vec![0, FORCE_IDR_AT as usize],
        "IDRs must appear only at start and on request; got {keyframe_indices:?}"
    );
}

#[test]
fn every_idr_carries_parameter_sets() {
    let stream = encode_stream();
    let types = nal_types(&stream);
    // HEVC NAL types: VPS=32, SPS=33, PPS=34, IDR_W_RADL=19, IDR_N_LP=20.
    let count = |t: u8| types.iter().filter(|&&x| x == t).count();
    let idrs = count(19) + count(20);
    assert_eq!(idrs, 2, "expected exactly 2 IDR NALs (start + forced)");
    // §5.2: repeat-headers → one VPS/SPS/PPS trio per IDR.
    assert_eq!(count(32), idrs, "a VPS per IDR");
    assert_eq!(count(33), idrs, "an SPS per IDR");
    assert_eq!(count(34), idrs, "a PPS per IDR");
}
