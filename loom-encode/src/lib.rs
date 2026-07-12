//! loom-encode — video encode. M1.2 ships the software HEVC backend (libx265);
//! NVENC / VideoToolbox arrive in M1.5 / M2.2.
//!
//! [`HevcEncoder`] is a thin, safe wrapper over the libx265 C API. It is pure
//! *mechanism*: the §5 constraint values (no B-frames, single ref, infinite GOP,
//! repeat headers, no scenecut) are passed in via [`EncoderConfig`] — their one
//! authoritative source lives in `loomd` (see its media constraints module), so
//! the encoder config and the conformance test agree by construction.

mod ffi;

use std::ffi::{c_void, CString};

/// libx265 NAL type for IDR access units (§5.2), from `x265.h`.
const NAL_IDR_W_RADL: u32 = ffi::NalUnitType_NAL_UNIT_CODED_SLICE_IDR_W_RADL;
const NAL_IDR_N_LP: u32 = ffi::NalUnitType_NAL_UNIT_CODED_SLICE_IDR_N_LP;

extern "C" {
    // Provided by src/x265_shim.c (the version-macro'd open).
    fn loom_x265_encoder_open(param: *mut ffi::x265_param) -> *mut ffi::x265_encoder;
}

/// Errors from the encoder.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// x265 parameter allocation/setup failed.
    #[error("x265 parameter allocation/setup failed")]
    Param,
    /// The requested preset/tune was rejected.
    #[error("x265 preset '{0}' rejected")]
    Preset(String),
    /// `x265_encoder_open` returned null.
    #[error("x265_encoder_open failed")]
    Open,
    /// `x265_encoder_encode` returned a negative status.
    #[error("x265_encoder_encode returned {0}")]
    Encode(i32),
}

/// Encoder configuration. The §5 knobs are explicit so the *policy* lives in the
/// caller (loomd), not buried here.
#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frame-rate numerator (e.g. 72).
    pub fps_num: u32,
    /// Frame-rate denominator (e.g. 1).
    pub fps_den: u32,
    /// Target bitrate, kbit/s.
    pub bitrate_kbps: u32,
    /// Max consecutive B-frames (§5.3 requires 0).
    pub bframes: u32,
    /// Max reference frames (§5.3 requires 1).
    pub max_ref: u32,
    /// Max intra period; a huge value gives the §5 infinite GOP.
    pub keyint_max: i32,
    /// Emit VPS/SPS/PPS with every IDR (§5.2).
    pub repeat_headers: bool,
    /// Allow scene-cut-triggered I-frames. §5.4 requires this OFF so IDRs appear
    /// only at start and on request.
    pub scenecut: bool,
}

/// One encoded HEVC access unit (Annex-B, start codes included; §4.1).
#[derive(Clone, Debug)]
pub struct AccessUnit {
    /// True if this AU is an IDR (carries VPS/SPS/PPS and resets the chain).
    pub keyframe: bool,
    /// The Annex-B byte stream for exactly one frame.
    pub data: Vec<u8>,
}

/// A software HEVC encoder honoring the §5 constraints in [`EncoderConfig`].
pub struct HevcEncoder {
    param: *mut ffi::x265_param,
    enc: *mut ffi::x265_encoder,
    started: bool,
}

impl HevcEncoder {
    /// Open an encoder for `cfg`. Uses the `ultrafast`/`zerolatency` preset (no
    /// lookahead, no frame reordering) as the low-latency base, then applies the
    /// explicit §5 constraints.
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncodeError> {
        unsafe {
            let param = ffi::x265_param_alloc();
            if param.is_null() {
                return Err(EncodeError::Param);
            }
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();
            if ffi::x265_param_default_preset(param, preset.as_ptr(), tune.as_ptr()) != 0 {
                ffi::x265_param_free(param);
                return Err(EncodeError::Preset("ultrafast/zerolatency".into()));
            }

            // Silence libx265's own stderr chatter so it doesn't pollute the
            // daemon's structured JSON logs (also on stderr).
            (*param).logLevel = ffi::X265_LOG_NONE;
            (*param).sourceWidth = cfg.width as i32;
            (*param).sourceHeight = cfg.height as i32;
            (*param).fpsNum = cfg.fps_num;
            (*param).fpsDenom = cfg.fps_den;
            (*param).internalCsp = ffi::X265_CSP_I420 as i32;
            // §5.3 no B-frames, single reference.
            (*param).bframes = cfg.bframes as i32;
            (*param).maxNumReferences = cfg.max_ref as i32;
            // §5: infinite GOP, closed, no scenecut → IDR only at start / on request.
            (*param).keyframeMax = cfg.keyint_max;
            (*param).keyframeMin = cfg.keyint_max;
            (*param).bOpenGOP = 0;
            (*param).scenecutThreshold = if cfg.scenecut { 40 } else { 0 };
            // §5.2 repeat VPS/SPS/PPS with every IDR; Annex-B byte stream.
            (*param).bRepeatHeaders = i32::from(cfg.repeat_headers);
            (*param).bAnnexB = 1;
            // CBR-like rate control with a ~1-frame VBV (ARCHITECTURE §9).
            (*param).rc.rateControlMode = ffi::X265_RC_METHODS_X265_RC_ABR as i32;
            (*param).rc.bitrate = cfg.bitrate_kbps as i32;
            (*param).rc.vbvMaxBitrate = cfg.bitrate_kbps as i32;
            let per_frame_kbit = (cfg.bitrate_kbps as i32) * (cfg.fps_den.max(1) as i32)
                / (cfg.fps_num.max(1) as i32);
            (*param).rc.vbvBufferSize = per_frame_kbit.max(1);

            let enc = loom_x265_encoder_open(param);
            if enc.is_null() {
                ffi::x265_param_free(param);
                return Err(EncodeError::Open);
            }
            Ok(Self {
                param,
                enc,
                started: false,
            })
        }
    }

    /// Encode one I420 frame. `planes`/`strides` are the Y, U, V planes. `pts` is
    /// a presentation timestamp (ticks; informative to the encoder). The first
    /// frame and any `force_idr` frame is coded as an IDR; all others as P
    /// (single-ref, no B), so the chain matches §5. With `zerolatency` the AU for
    /// this frame is returned immediately.
    pub fn encode_i420(
        &mut self,
        planes: [&[u8]; 3],
        strides: [i32; 3],
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        unsafe {
            let mut pic_in: ffi::x265_picture = std::mem::zeroed();
            ffi::x265_picture_init(self.param, &mut pic_in);
            pic_in.planes[0] = planes[0].as_ptr() as *mut c_void;
            pic_in.planes[1] = planes[1].as_ptr() as *mut c_void;
            pic_in.planes[2] = planes[2].as_ptr() as *mut c_void;
            pic_in.stride[0] = strides[0];
            pic_in.stride[1] = strides[1];
            pic_in.stride[2] = strides[2];
            pic_in.pts = pts;
            pic_in.sliceType = if force_idr || !self.started {
                ffi::X265_TYPE_IDR as i32
            } else {
                ffi::X265_TYPE_P as i32
            };
            self.started = true;

            let mut nal: *mut ffi::x265_nal = std::ptr::null_mut();
            let mut num_nal: u32 = 0;
            let mut pic_out: ffi::x265_picture = std::mem::zeroed();
            let ret = ffi::x265_encoder_encode(
                self.enc,
                &mut nal,
                &mut num_nal,
                &mut pic_in,
                &mut pic_out,
            );
            if ret < 0 {
                return Err(EncodeError::Encode(ret));
            }
            if ret == 0 || num_nal == 0 {
                return Ok(None); // no output this call (encoder warming up)
            }

            let nals = std::slice::from_raw_parts(nal, num_nal as usize);
            let mut data = Vec::new();
            let mut keyframe = false;
            for n in nals {
                if n.type_ == NAL_IDR_W_RADL || n.type_ == NAL_IDR_N_LP {
                    keyframe = true;
                }
                let payload = std::slice::from_raw_parts(n.payload, n.sizeBytes as usize);
                data.extend_from_slice(payload);
            }
            Ok(Some(AccessUnit { keyframe, data }))
        }
    }
}

impl Drop for HevcEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.enc.is_null() {
                ffi::x265_encoder_close(self.enc);
            }
            if !self.param.is_null() {
                ffi::x265_param_free(self.param);
            }
            ffi::x265_cleanup();
        }
    }
}

// The encoder owns raw pointers into libx265; loomd runs it on a dedicated
// per-session thread (safe to move, not to share).
unsafe impl Send for HevcEncoder {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EncoderConfig {
        EncoderConfig {
            width: 320,
            height: 240,
            fps_num: 72,
            fps_den: 1,
            bitrate_kbps: 2000,
            bframes: 0,
            max_ref: 1,
            keyint_max: i32::MAX,
            repeat_headers: true,
            scenecut: false,
        }
    }

    fn gray(w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            vec![128u8; w * h],
            vec![128u8; (w / 2) * (h / 2)],
            vec![128u8; (w / 2) * (h / 2)],
        )
    }

    #[test]
    fn first_frame_is_keyframe_then_p_frames() {
        let mut enc = HevcEncoder::new(cfg()).expect("open");
        let (y, u, v) = gray(320, 240);
        let strides = [320, 160, 160];

        let first = enc
            .encode_i420([&y, &u, &v], strides, 0, false)
            .expect("encode")
            .expect("zerolatency yields output");
        assert!(first.keyframe, "first frame must be an IDR");
        assert!(!first.data.is_empty());

        let mut saw_p = false;
        for pts in 1..6 {
            if let Some(au) = enc
                .encode_i420([&y, &u, &v], strides, pts, false)
                .expect("encode")
            {
                if !au.keyframe {
                    saw_p = true;
                }
            }
        }
        assert!(saw_p, "subsequent frames must be non-IDR");
    }

    #[test]
    fn force_idr_produces_keyframe() {
        let mut enc = HevcEncoder::new(cfg()).expect("open");
        let (y, u, v) = gray(320, 240);
        let strides = [320, 160, 160];
        enc.encode_i420([&y, &u, &v], strides, 0, false)
            .expect("encode");
        let forced = enc
            .encode_i420([&y, &u, &v], strides, 1, true)
            .expect("encode")
            .expect("output");
        assert!(forced.keyframe, "forced frame must be an IDR");
    }
}
