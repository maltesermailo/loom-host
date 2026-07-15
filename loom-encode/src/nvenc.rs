//! Hardware HEVC encode via libavcodec's `hevc_nvenc` (M1.5, Linux/NVIDIA).
//!
//! Feature-gated (`nvenc`): it links libavcodec/libavutil + NVENC, which only
//! exist on the Linux host, so it is absent from the default build. Same seam as
//! the software [`crate::HevcEncoder`] — an [`EncoderConfig`] in, an
//! [`AccessUnit`] out — so `loomd` swaps between them with no other change.
//!
//! Input is a CPU I420 buffer: capture is SHM this milestone, so the frame is
//! already in host memory and NVENC does one host→device upload during
//! `send_frame`. dmabuf→CUDA zero-copy that would remove that copy is deferred —
//! TODO(R2), and it needs the dmabuf capture path (also deferred) first.

use std::ffi::{c_int, c_void, CString};

use crate::av_ffi as ff;
use crate::{AccessUnit, EncodeError, EncoderConfig};

/// `AVERROR(EAGAIN)`: the encoder accepted the frame but has no packet yet.
/// `AVERROR(e) == -e` and `EAGAIN == 11` on Linux; this backend is Linux-only.
const AVERROR_EAGAIN: c_int = -11;

/// A hardware HEVC encoder honoring the §5 constraints in [`EncoderConfig`].
pub struct NvencEncoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
}

impl NvencEncoder {
    /// Open `hevc_nvenc` for `cfg`. The §5 stream shape is set through public
    /// `AVCodecContext` fields; the low-latency / rate-control / force-IDR knobs
    /// through the encoder's private options.
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncodeError> {
        unsafe {
            let name = CString::new("hevc_nvenc").unwrap();
            let codec = ff::avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                return Err(EncodeError::EncoderUnavailable);
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(EncodeError::Open);
            }

            (*ctx).width = cfg.width as c_int;
            (*ctx).height = cfg.height as c_int;
            (*ctx).pix_fmt = ff::AVPixelFormat_AV_PIX_FMT_YUV420P;
            (*ctx).time_base = ff::AVRational {
                num: cfg.fps_den as c_int,
                den: cfg.fps_num as c_int,
            };
            (*ctx).framerate = ff::AVRational {
                num: cfg.fps_num as c_int,
                den: cfg.fps_den as c_int,
            };
            // §5: huge GOP → no periodic IDR; no B-frames; single reference.
            (*ctx).gop_size = cfg.keyint_max;
            (*ctx).max_b_frames = cfg.bframes as c_int;
            (*ctx).refs = cfg.max_ref as c_int;
            // CBR with a 1-frame VBV (§5 / ARCHITECTURE §9).
            let bitrate = cfg.bitrate_kbps as i64 * 1000;
            let per_frame = bitrate * cfg.fps_den.max(1) as i64 / cfg.fps_num.max(1) as i64;
            (*ctx).bit_rate = bitrate;
            (*ctx).rc_max_rate = bitrate;
            (*ctx).rc_min_rate = bitrate;
            (*ctx).rc_buffer_size = per_frame as c_int;

            // Private hevc_nvenc options (set before open). NOT setting
            // AV_CODEC_FLAG_GLOBAL_HEADER makes hevc_nvenc repeat VPS/SPS/PPS
            // in-band with every IDR (§5.2).
            let opts = (*ctx).priv_data;
            set_opt(opts, "preset", "p4")?;
            set_opt(opts, "tune", "ull")?; // ultra low latency
            set_opt(opts, "rc", "cbr")?;
            set_opt(opts, "profile", "main")?; // §5.1 HEVC Main
            set_opt_int(opts, "forced-idr", 1)?; // pict_type=I ⇒ IDR (§3.6)
            set_opt_int(opts, "delay", 0)?; // emit each frame immediately, no reorder

            let ret = ff::avcodec_open2(ctx, codec, std::ptr::null_mut());
            if ret < 0 {
                let mut c = ctx;
                ff::avcodec_free_context(&mut c);
                return Err(EncodeError::Av(ret));
            }

            let frame = ff::av_frame_alloc();
            let packet = ff::av_packet_alloc();
            if frame.is_null() || packet.is_null() {
                let mut c = ctx;
                ff::avcodec_free_context(&mut c);
                return Err(EncodeError::Open);
            }

            Ok(Self { ctx, frame, packet })
        }
    }

    /// Encode one I420 frame. The frame points directly at the caller's planes
    /// (no copy here; NVENC uploads during `send_frame`). `force_idr` codes it as
    /// an IDR; the first frame is an IDR regardless (encoder start). With
    /// `delay=0` and no B-frames, output order equals input order, so this is
    /// one-in-one-out.
    pub fn encode_i420(
        &mut self,
        planes: [&[u8]; 3],
        strides: [i32; 3],
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        unsafe {
            let f = self.frame;
            (*f).width = (*self.ctx).width;
            (*f).height = (*self.ctx).height;
            (*f).format = ff::AVPixelFormat_AV_PIX_FMT_YUV420P as c_int;
            for i in 0..3 {
                (*f).data[i] = planes[i].as_ptr() as *mut u8;
                (*f).linesize[i] = strides[i];
            }
            (*f).pts = pts;
            (*f).pict_type = if force_idr {
                ff::AVPictureType_AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType_AV_PICTURE_TYPE_NONE
            };

            let ret = ff::avcodec_send_frame(self.ctx, f);
            if ret < 0 {
                return Err(EncodeError::Av(ret));
            }

            let ret = ff::avcodec_receive_packet(self.ctx, self.packet);
            if ret == AVERROR_EAGAIN {
                return Ok(None);
            }
            if ret < 0 {
                return Err(EncodeError::Av(ret));
            }

            let pkt = self.packet;
            let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize).to_vec();
            let keyframe = (*pkt).flags & ff::AV_PKT_FLAG_KEY as c_int != 0;
            ff::av_packet_unref(pkt);

            Ok(Some(AccessUnit { keyframe, data }))
        }
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

// Owns raw libav pointers; loomd runs it on a dedicated per-session thread
// (safe to move, not to share) — same contract as the x265 encoder.
unsafe impl Send for NvencEncoder {}

unsafe fn set_opt(obj: *mut c_void, key: &str, val: &str) -> Result<(), EncodeError> {
    let key = CString::new(key).unwrap();
    let val = CString::new(val).unwrap();
    let ret = ff::av_opt_set(obj, key.as_ptr(), val.as_ptr(), 0);
    if ret < 0 {
        return Err(EncodeError::Av(ret));
    }
    Ok(())
}

unsafe fn set_opt_int(obj: *mut c_void, key: &str, val: i64) -> Result<(), EncodeError> {
    let key = CString::new(key).unwrap();
    let ret = ff::av_opt_set_int(obj, key.as_ptr(), val, 0);
    if ret < 0 {
        return Err(EncodeError::Av(ret));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Requires a real NVENC GPU, so it only runs under `--features nvenc`.
    #[test]
    fn first_frame_is_keyframe_then_p_frames() {
        let cfg = EncoderConfig {
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
        };
        let mut enc = NvencEncoder::new(cfg).expect("open hevc_nvenc");

        let (w, h) = (320usize, 240usize);
        let (y, u, v) = (
            vec![128u8; w * h],
            vec![128u8; w * h / 4],
            vec![128u8; w * h / 4],
        );
        let strides = [w as i32, (w / 2) as i32, (w / 2) as i32];

        let first = enc
            .encode_i420([&y, &u, &v], strides, 0, false)
            .expect("encode")
            .expect("first frame emits with delay=0");
        assert!(first.keyframe, "first frame must be an IDR");

        let mut saw_p = false;
        for pts in 1..6 {
            if let Some(au) = enc
                .encode_i420([&y, &u, &v], strides, pts, false)
                .expect("encode")
            {
                saw_p |= !au.keyframe;
            }
        }
        assert!(saw_p, "subsequent frames must be non-IDR");
    }
}
