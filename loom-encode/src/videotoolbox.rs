//! Hardware HEVC encode via VideoToolbox (M2.2, macOS/Apple Silicon).
//!
//! Same seam as [`crate::HevcEncoder`] and [`crate::NvencEncoder`] — an
//! [`EncoderConfig`] in, an [`AccessUnit`] out — so `loomd` swaps between them
//! with no other change.
//!
//! Configured per Apple's own "ultra-low-latency conferencing and cloud gaming"
//! recipe in `VTCompressionProperties.h`: low-latency rate control, RealTime,
//! ExpectedFrameRate, and speed over quality. §5's stream shape then comes from
//! AllowFrameReordering=false (no B-frames), ReferenceBufferCount=1 (single
//! reference) and a huge MaxKeyFrameInterval (infinite GOP).
//!
//! The recipe's *hints* and §5's *constraints* fail differently, which is why
//! there are two setters: this machine's encoder does not implement
//! PrioritizeEncodingSpeedOverQuality, and tolerating that is fine — it only
//! tunes how a conformant stream is reached. A rejected §5 knob is fatal.
//!
//! **§5.4 has no API guarantee.** VideoToolbox exposes no scene-cut control (the
//! x265 path has `scenecutThreshold=0`), and MaxKeyFrameInterval is documented as
//! a *ceiling*: "Video encoders are allowed to generate key frames more
//! frequently if this would result in more efficient compression." Conformance is
//! therefore established empirically by the bitstream test, which forces a hard
//! scene cut and asserts no unrequested IDR appears — see
//! `loomd/tests/bitstream_conformance.rs`.
//!
//! Input is a CPU I420 buffer, copied into an **NV12** pixel buffer from the
//! session's own pool. NV12 is what the media engine consumes natively; handing
//! it planar I420 instead costs 1.4 ms/frame at 1440p (10.2 → 8.8) in a
//! conversion VideoToolbox performs *inside* the encode.
//!
//! The remaining ~8.4 ms/frame at 1440p is the engine's latency under §5.3, and
//! is not recoverable here: single-reference with no reordering makes frame N
//! depend on N-1, so the encoder is a serial chain — measured latency and
//! throughput are equal (8.4 ms, 118 fps), and neither QoS, ExpectedFrameRate,
//! nor RequireHardwareAcceleratedVideoEncoder moves it. See `reviews/M2.3`.

// The VideoToolbox/CoreFoundation bindings are unsafe by construction.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};
use objc2_core_media::{
    kCMVideoCodecType_HEVC, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AllowTemporalCompression, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_ReferenceBufferCount, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
};

use crate::annexb;
use crate::{AccessUnit, EncodeError, EncoderConfig};

/// `kVTPropertyNotSupportedErr` from `VTErrors.h`: this encoder does not
/// implement the key at all (as opposed to rejecting the value).
const K_VT_PROPERTY_NOT_SUPPORTED: i32 = -12900;

/// Where the output callback leaves the encoded frame for `encode_i420` to pick
/// up. VideoToolbox may call back on its own thread, hence the mutex.
#[derive(Default)]
struct Sink {
    au: Option<AccessUnit>,
    /// First failing OSStatus seen, reported by the next `encode_i420`.
    error: Option<i32>,
    /// Whether to prepend VPS/SPS/PPS to IDRs (§5.2; `repeat_headers`).
    repeat_headers: bool,
}

/// A hardware HEVC encoder honoring the §5 constraints in [`EncoderConfig`].
pub struct VideoToolboxEncoder {
    // Declared first so Drop invalidates the session — and therefore stops the
    // callback — before `sink`, which the callback dereferences, is released.
    session: CFRetained<VTCompressionSession>,
    sink: Arc<Mutex<Sink>>,
    fps_num: u32,
    fps_den: u32,
}

impl VideoToolboxEncoder {
    /// Open a VideoToolbox HEVC session for `cfg`.
    pub fn new(cfg: EncoderConfig) -> Result<Self, EncodeError> {
        let sink = Arc::new(Mutex::new(Sink {
            repeat_headers: cfg.repeat_headers,
            ..Sink::default()
        }));

        let specification: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl }],
            &[cf_bool(true).as_ref()],
        );
        let source_attributes = source_image_attributes(cfg.width, cfg.height);

        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                cfg.width as i32,
                cfg.height as i32,
                kCMVideoCodecType_HEVC,
                Some(specification.as_opaque()),
                Some(source_attributes.as_opaque()),
                None,
                Some(output_callback),
                Arc::as_ptr(&sink) as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(EncodeError::VideoToolbox(status));
        }
        // SAFETY: create() returned success, so `raw` is a +1 session reference.
        let session = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };

        let encoder = Self {
            session,
            sink,
            fps_num: cfg.fps_num,
            fps_den: cfg.fps_den,
        };
        encoder.configure(&cfg)?;

        Ok(encoder)
    }

    /// Apply the §5 constraints and the low-latency recipe. Every key here was
    /// verified against this machine's `VTCompressionProperties.h`.
    fn configure(&self, cfg: &EncoderConfig) -> Result<(), EncodeError> {
        let fps = cfg.fps_num as f64 / cfg.fps_den.max(1) as f64;

        // Apple's ultra-low-latency recipe (VTCompressionProperties.h §"Ultra-low-
        // latency conferencing and cloud gaming").
        self.set_hint(
            "RealTime",
            unsafe { kVTCompressionPropertyKey_RealTime },
            cf_bool(true),
        )?;
        self.set_hint(
            "PrioritizeEncodingSpeedOverQuality",
            unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
            cf_bool(true),
        )?;
        self.set_hint(
            "ExpectedFrameRate",
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            CFNumber::new_f64(fps),
        )?;

        // §5.1 HEVC Main, 8-bit 4:2:0.
        self.set(
            "ProfileLevel",
            unsafe { kVTCompressionPropertyKey_ProfileLevel },
            unsafe { kVTProfileLevel_HEVC_Main_AutoLevel }.retain(),
        )?;

        // §5.3: no B-frames, and only the immediately previous frame as reference.
        // ReferenceBufferCount is a hard limit — the encoder errors rather than
        // silently exceeding it.
        self.set(
            "AllowFrameReordering",
            unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
            cf_bool(cfg.bframes > 0),
        )?;
        self.set(
            "ReferenceBufferCount",
            unsafe { kVTCompressionPropertyKey_ReferenceBufferCount },
            CFNumber::new_i32(cfg.max_ref as i32),
        )?;
        // P-frames must stay enabled; false would make every frame intra.
        self.set(
            "AllowTemporalCompression",
            unsafe { kVTCompressionPropertyKey_AllowTemporalCompression },
            cf_bool(true),
        )?;

        // §5.4/§5: infinite closed GOP — IDRs only at start and on request. Both
        // the count and the duration limit must be lifted, or whichever is smaller
        // wins and reintroduces periodic IDRs.
        self.set(
            "MaxKeyFrameInterval",
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
            CFNumber::new_i32(cfg.keyint_max),
        )?;
        self.set(
            "MaxKeyFrameIntervalDuration",
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
            CFNumber::new_f64(0.0),
        )?;
        // No AllowOpenGOP here (this encoder rejects it anyway): open vs closed GOP
        // only decides whether *leading* pictures may reference across an IDR, and
        // AllowFrameReordering=false with a single reference means no leading
        // pictures exist. §5 does not ask for it; x265's bOpenGOP=0 is that
        // encoder's own belt-and-braces.

        // CBR-ish with a ~1-frame VBV (ARCHITECTURE §9): an average target plus a
        // hard cap of one frame's worth of bytes per frame interval.
        let bits_per_second = cfg.bitrate_kbps as f64 * 1000.0;
        self.set(
            "AverageBitRate",
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            CFNumber::new_i32(bits_per_second as i32),
        )?;
        self.set_data_rate_limit(bits_per_second / 8.0 / fps.max(1.0), 1.0 / fps.max(1.0))?;

        Ok(())
    }

    /// Set a property §5 depends on. A rejection here is fatal: it would mean the
    /// bitstream cannot be made conformant, which is a spec question, not
    /// something to paper over.
    fn set(
        &self,
        name: &'static str,
        key: &CFString,
        value: CFRetained<impl AsRef<CFType>>,
    ) -> Result<(), EncodeError> {
        match self.try_set(key, value) {
            0 => Ok(()),
            status => Err(EncodeError::VideoToolboxProperty { key: name, status }),
        }
    }

    /// Set a latency *hint* from Apple's recipe. These tune how the encoder gets
    /// to a conformant stream, they don't define one, so an encoder that doesn't
    /// implement one is fine — the M4 Max's HEVC encoder rejects
    /// PrioritizeEncodingSpeedOverQuality, for instance. Any other failure is
    /// still an error.
    fn set_hint(
        &self,
        name: &'static str,
        key: &CFString,
        value: CFRetained<impl AsRef<CFType>>,
    ) -> Result<(), EncodeError> {
        match self.try_set(key, value) {
            0 | K_VT_PROPERTY_NOT_SUPPORTED => Ok(()),
            status => Err(EncodeError::VideoToolboxProperty { key: name, status }),
        }
    }

    fn try_set(&self, key: &CFString, value: CFRetained<impl AsRef<CFType>>) -> i32 {
        unsafe { VTSessionSetProperty(&self.session, key, Some(value.as_ref())) }
    }

    /// `DataRateLimits` is the one property that takes an array: [bytes, seconds].
    fn set_data_rate_limit(&self, bytes: f64, seconds: f64) -> Result<(), EncodeError> {
        let (bytes, seconds) = (CFNumber::new_f64(bytes), CFNumber::new_f64(seconds));
        let limits: CFRetained<CFArray<CFType>> =
            CFArray::from_objects(&[bytes.as_ref(), seconds.as_ref()]);
        let status = unsafe {
            VTSessionSetProperty(
                &self.session,
                objc2_video_toolbox::kVTCompressionPropertyKey_DataRateLimits,
                Some(limits.as_opaque().as_ref()),
            )
        };

        match status {
            0 => Ok(()),
            _ => Err(EncodeError::VideoToolbox(status)),
        }
    }

    /// Encode one I420 frame. The planes are copied into a pool pixel buffer, the
    /// frame is submitted, and the session is flushed so the access unit for
    /// *this* frame is returned — one-in-one-out, matching §5.6's "never queue
    /// more than one frame at the encoder input".
    pub fn encode_i420(
        &mut self,
        planes: [&[u8]; 3],
        strides: [i32; 3],
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<AccessUnit>, EncodeError> {
        let pixels = self.fill_pixel_buffer(planes, strides)?;

        let timestamp = CMTime {
            value: pts,
            timescale: self.fps_num as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let duration = CMTime {
            value: self.fps_den as i64,
            timescale: self.fps_num as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let properties: Option<CFRetained<CFDictionary<CFString, CFType>>> = force_idr.then(|| {
            CFDictionary::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[cf_bool(true).as_ref()],
            )
        });

        let status = unsafe {
            self.session.encode_frame(
                &pixels,
                timestamp,
                duration,
                properties.as_deref().map(CFDictionary::as_opaque),
                std::ptr::null_mut(),
                std::ptr::null_mut::<VTEncodeInfoFlags>(),
            )
        };
        if status != 0 {
            return Err(EncodeError::VideoToolbox(status));
        }

        // Flush so the callback has run by the time we take the result: an invalid
        // CMTime (flags empty, as kCMTimeInvalid is defined) means "complete every
        // pending frame".
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags::empty(),
            epoch: 0,
        };
        let status = unsafe { self.session.complete_frames(invalid) };
        if status != 0 {
            return Err(EncodeError::VideoToolbox(status));
        }

        let mut sink = self.sink.lock().expect("encoder sink poisoned");
        if let Some(status) = sink.error.take() {
            return Err(EncodeError::VideoToolbox(status));
        }

        Ok(sink.au.take())
    }

    /// Copy the I420 planes into an NV12 pixel buffer from the session's pool,
    /// interleaving the chroma on the way in.
    ///
    /// NV12 rather than the planar `y420` this first shipped with: measured at
    /// 1440p72, feeding the encoder its native layout cuts encode from 10.2 ms to
    /// 8.8 ms, because VideoToolbox otherwise converts internally — inside the
    /// encode, where the cost is invisible to a caller timing its own memcpy.
    fn fill_pixel_buffer(
        &self,
        planes: [&[u8]; 3],
        strides: [i32; 3],
    ) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let pool = unsafe { self.session.pixel_buffer_pool() }
            .ok_or(EncodeError::Bitstream("no source pixel-buffer pool"))?;

        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        if status != 0 || raw.is_null() {
            return Err(EncodeError::VideoToolbox(status));
        }
        // SAFETY: the create call succeeded, so `raw` is a +1 pixel-buffer reference.
        let pixels = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };

        // SAFETY: planes are valid until the matching unlock; each source row is
        // copied into the destination's own (possibly wider) stride.
        unsafe {
            let status = CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags(0));
            if status != 0 {
                return Err(EncodeError::VideoToolbox(status));
            }

            let (luma_stride, chroma_stride) = (strides[0] as usize, strides[1] as usize);

            let luma = CVPixelBufferGetBaseAddressOfPlane(&pixels, 0).cast::<u8>();
            let dst_luma_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 0);
            for row in 0..planes[0].len() / luma_stride.max(1) {
                std::ptr::copy_nonoverlapping(
                    planes[0][row * luma_stride..].as_ptr(),
                    luma.add(row * dst_luma_stride),
                    luma_stride,
                );
            }

            let chroma = CVPixelBufferGetBaseAddressOfPlane(&pixels, 1).cast::<u8>();
            let dst_chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 1);
            for row in 0..planes[1].len() / chroma_stride.max(1) {
                let (u_row, v_row) = (
                    &planes[1][row * chroma_stride..],
                    &planes[2][row * chroma_stride..],
                );
                let dst_row = chroma.add(row * dst_chroma_stride);

                for i in 0..chroma_stride {
                    *dst_row.add(2 * i) = u_row[i];
                    *dst_row.add(2 * i + 1) = v_row[i];
                }
            }

            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags(0));
        }

        Ok(pixels)
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

// Owns a VideoToolbox session; loomd runs it on a dedicated per-session thread
// (safe to move, not to share) — same contract as the other two encoders.
unsafe impl Send for VideoToolboxEncoder {}

/// The compression session's output callback. Converts VideoToolbox's
/// length-prefixed NALs to Annex-B and, for IDRs, prepends the parameter sets
/// that VideoToolbox otherwise keeps out of band (§5.2).
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the Arc<Mutex<Sink>> kept alive by the encoder, which
    // invalidates the session before releasing it.
    let sink = unsafe { &*(refcon as *const Mutex<Sink>) };
    let mut sink = sink.lock().expect("encoder sink poisoned");

    if status != 0 {
        sink.error.get_or_insert(status);
        return;
    }
    let Some(sample_buffer) = (unsafe { sample_buffer.as_ref() }) else {
        return; // a dropped frame, not an error
    };

    match convert(sample_buffer, sink.repeat_headers) {
        Ok(au) => sink.au = Some(au),
        Err(EncodeError::VideoToolbox(status)) => {
            sink.error.get_or_insert(status);
        }
        Err(_) => {
            sink.error.get_or_insert(-1);
        }
    }
}

/// Turn one encoded sample buffer into an Annex-B [`AccessUnit`].
fn convert(
    sample_buffer: &CMSampleBuffer,
    repeat_headers: bool,
) -> Result<AccessUnit, EncodeError> {
    let format = unsafe { sample_buffer.format_description() }.ok_or(EncodeError::Bitstream(
        "sample buffer has no format description",
    ))?;
    let block = unsafe { sample_buffer.data_buffer() }
        .ok_or(EncodeError::Bitstream("sample buffer has no data buffer"))?;

    // SAFETY: the block buffer's bytes stay valid for this call's duration.
    let mut total: usize = 0;
    let mut pointer: *mut std::ffi::c_char = std::ptr::null_mut();
    let status = unsafe { block.data_pointer(0, std::ptr::null_mut(), &mut total, &mut pointer) };
    if status != 0 || pointer.is_null() {
        return Err(EncodeError::VideoToolbox(status));
    }
    let src = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), total) };

    // The parameter sets are needed both for the NAL length size and, on IDRs, to
    // be injected in-band; index 0 reports the count and the length size.
    let mut nal_length_size: i32 = 0;
    let mut count: usize = 0;
    let status = unsafe {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
            &format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut nal_length_size,
        )
    };
    if status != 0 {
        return Err(EncodeError::VideoToolbox(status));
    }

    let mut body = Vec::with_capacity(total + 128);
    let keyframe = annexb::append(src, nal_length_size as usize, &mut body)?;

    if !(keyframe && repeat_headers) {
        return Ok(AccessUnit {
            keyframe,
            data: body,
        });
    }

    // §5.2: VPS/SPS/PPS lead every IDR, so a client can join or recover mid-stream.
    let mut data = Vec::with_capacity(body.len() + 128);
    for index in 0..count {
        let mut set: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let status = unsafe {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                &format,
                index,
                &mut set,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || set.is_null() {
            return Err(EncodeError::VideoToolbox(status));
        }

        data.extend_from_slice(&annexb::START_CODE);
        data.extend_from_slice(unsafe { std::slice::from_raw_parts(set, size) });
    }
    data.extend_from_slice(&body);

    Ok(AccessUnit { keyframe, data })
}

fn cf_bool(value: bool) -> CFRetained<CFBoolean> {
    CFBoolean::new(value).retain()
}

/// Source pixel-buffer attributes: NV12 at the encode size — the layout Apple's
/// media engine consumes natively, so VideoToolbox does no conversion of its own.
/// Video range, matching what ScreenCaptureKit delivers and what the Linux
/// converter produces.
fn source_image_attributes(width: u32, height: u32) -> CFRetained<CFDictionary<CFString, CFType>> {
    let (format, width, height) = (
        CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32),
        CFNumber::new_i32(width as i32),
        CFNumber::new_i32(height as i32),
    );

    CFDictionary::from_slices(
        &[
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            unsafe { kCVPixelBufferWidthKey },
            unsafe { kCVPixelBufferHeightKey },
        ],
        &[format.as_ref(), width.as_ref(), height.as_ref()],
    )
}

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
        let mut enc = VideoToolboxEncoder::new(cfg()).expect("open VideoToolbox");
        let (y, u, v) = gray(320, 240);
        let strides = [320, 160, 160];

        let first = enc
            .encode_i420([&y, &u, &v], strides, 0, false)
            .expect("encode")
            .expect("a flushed session yields output");
        assert!(first.keyframe, "first frame must be an IDR");
        assert_eq!(
            first.data[..4],
            annexb::START_CODE,
            "output must be Annex-B, not length-prefixed"
        );

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

    #[test]
    fn force_idr_produces_keyframe() {
        let mut enc = VideoToolboxEncoder::new(cfg()).expect("open VideoToolbox");
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
