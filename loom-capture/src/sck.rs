//! ScreenCaptureKit capture (M2.1) — the macOS twin of the Linux portal path.
//!
//! `SCStream` is configured for `420v` (biplanar NV12), so SCK's GPU path does
//! the RGB→YUV conversion and [`crate::nv12`] only splits the chroma plane
//! (ARCHITECTURE §5.2). SCK also scales the display to the configured size, so
//! unlike the portal path there is no size-mismatch failure — a non-16:9 display
//! is letterboxed rather than rejected.
//!
//! Delivery is damage-driven exactly like the portal path: the newest frame
//! lives in a shared slot and `loomd`'s media loop does the pacing and
//! last-frame repeat (§5.6).
//!
//! Screen Recording (TCC) permission is resolved during [`ScreenCapture::start`]:
//! SCK answers `SCError::UserDeclined` (-3801), which surfaces as
//! [`CaptureError::PermissionDenied`] with an actionable message — never a
//! silent stream of black frames.

// The objc2 bindings are unsafe by construction; this module is the only place
// in loom-capture that may call them (see the crate-level deny).
#![allow(unsafe_code)]

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, Message};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamErrorCode, SCStreamOutput, SCStreamOutputType,
};

use crate::frame::I420Buffer;
use crate::nv12;
use crate::CaptureError;

/// How long to wait for SCK's async replies before calling it a failure. These
/// resolve in milliseconds in practice; the bound only exists so a wedged
/// callback fails loudly instead of hanging `loomd` forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared latest-frame slot: the SCK callback publishes, the media thread takes.
type FrameSlot = Arc<Mutex<Option<I420Buffer>>>;

/// State owned by the stream-output delegate.
struct OutputIvars {
    size: (u32, u32),
    slot: FrameSlot,
    /// The buffer displaced from `slot` on the last publish, reused as the next
    /// scratch so steady-state capture does not allocate.
    spare: Mutex<Option<I420Buffer>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class does not
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "LoomStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }

            self.publish(sample_buffer);
        }
    }
);

impl StreamOutput {
    fn new(size: (u32, u32), slot: FrameSlot) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars {
            size,
            slot,
            spare: Mutex::new(None),
        });

        unsafe { msg_send![super(this), init] }
    }

    /// Convert one delivered sample buffer into the shared slot. Runs on SCK's
    /// sample-handler queue, not the media thread.
    fn publish(&self, sample_buffer: &CMSampleBuffer) {
        // SCK signals "nothing changed" (idle) and blank frames by handing over a
        // sample buffer with no image buffer; there is nothing to publish then and
        // the media loop repeats its held frame.
        // SAFETY: SCK guarantees the sample buffer is valid for this call.
        let Some(pixels) = (unsafe { sample_buffer.image_buffer() }) else {
            return;
        };

        let ivars = self.ivars();
        let mut frame = ivars
            .spare
            .lock()
            .expect("spare poisoned")
            .take()
            .unwrap_or_else(|| I420Buffer::new(ivars.size.0, ivars.size.1));

        // SAFETY: the planes stay valid until the matching unlock below, and the
        // frame is sized from the same (width, height) SCK was configured with.
        unsafe {
            let format = CVPixelBufferGetPixelFormatType(&pixels);
            if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
                tracing::error!(target: "loom::capture", format, "unexpected pixel format");
                return;
            }
            if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) != 0 {
                tracing::error!(target: "loom::capture", "locking the pixel buffer failed");
                return;
            }

            let y = CVPixelBufferGetBaseAddressOfPlane(&pixels, 0).cast::<u8>();
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 0);
            let y_rows = CVPixelBufferGetHeightOfPlane(&pixels, 0);
            let uv = CVPixelBufferGetBaseAddressOfPlane(&pixels, 1).cast::<u8>();
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixels, 1);
            let uv_rows = CVPixelBufferGetHeightOfPlane(&pixels, 1);

            if !y.is_null() && !uv.is_null() {
                let y_plane = std::slice::from_raw_parts(y, y_stride * y_rows);
                let uv_plane = std::slice::from_raw_parts(uv, uv_stride * uv_rows);
                nv12::to_i420(y_plane, y_stride, uv_plane, uv_stride, &mut frame);
            }

            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
        }

        let displaced = ivars
            .slot
            .lock()
            .expect("frame slot poisoned")
            .replace(frame);
        *ivars.spare.lock().expect("spare poisoned") = displaced;
    }
}

/// A running ScreenCaptureKit capture. Frames flow into a shared slot; poll the
/// newest with [`Self::fill`]. Dropping it stops the stream.
pub struct ScreenCapture {
    stream: Retained<SCStream>,
    slot: FrameSlot,
    /// Kept alive for the stream's lifetime: SCK holds only a weak reference to
    /// its stream output, so dropping this would silence the callback.
    _output: Retained<StreamOutput>,
}

impl ScreenCapture {
    /// Start capturing a display, scaled to `width`×`height`. `target` is the
    /// `CGDirectDisplayID` to capture; `None` selects the main display (the M2.1
    /// behavior). Blocks through display enumeration and stream start, so a
    /// missing Screen Recording permission — or a target that no longer exists —
    /// surfaces here rather than as black frames.
    pub fn start(
        target: Option<u32>,
        width: u32,
        height: u32,
        refresh: u32,
    ) -> Result<Self, CaptureError> {
        let display = pick_display(target)?;
        let display_id = unsafe { display.displayID() };

        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };
        let config = configuration(width, height, refresh);

        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let output = StreamOutput::new((width, height), slot.clone());
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };

        let queue = DispatchQueue::new("dev.loom.capture", None);
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| CaptureError::ScreenCaptureKit(format!("addStreamOutput: {e}")))?;

        start_capture(&stream)?;
        tracing::info!(target: "loom::capture", width, height, refresh, display_id,
            "ScreenCaptureKit capture started (420v)");

        Ok(Self {
            stream,
            slot,
            _output: output,
        })
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

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// The `SCStreamConfiguration` for §5-shaped capture: `420v` frames at the
/// negotiated size, paced no faster than the refresh rate, with a shallow queue
/// (freshness over completeness — a backlog of stale frames is worthless).
fn configuration(width: u32, height: u32, refresh: u32) -> Retained<SCStreamConfiguration> {
    let config = unsafe { SCStreamConfiguration::new() };

    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: refresh.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        config.setQueueDepth(3);
        config.setShowsCursor(true);
    }

    config
}

/// One capturable display, as reported by ScreenCaptureKit. `id` is the
/// `CGDirectDisplayID` to pass back to [`ScreenCapture::start`] as its `target`;
/// `width`/`height` are the display's native pixel size (before Loom's scaling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// The `CGDirectDisplayID`, stable while the display stays connected.
    pub id: u32,
    /// Native width in pixels.
    pub width: u32,
    /// Native height in pixels.
    pub height: u32,
}

/// Enumerate the displays ScreenCaptureKit can capture, in SCK's order (index 0
/// is the main display). This is how the multi-display fan-out (M6.2) discovers
/// which `CGDirectDisplayID`s exist to stream. Blocks on SCK's async enumeration,
/// so a missing Screen Recording permission surfaces here.
pub fn displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    let content = shareable_content()?;
    let list = unsafe { content.displays() };

    let mut out = Vec::with_capacity(list.count());
    for d in list.iter() {
        // SAFETY: the accessors are read-only and `d` is a live SCDisplay.
        unsafe {
            out.push(DisplayInfo {
                id: d.displayID(),
                width: d.width() as u32,
                height: d.height() as u32,
            });
        }
    }
    Ok(out)
}

/// Resolve a `target` `CGDirectDisplayID` to its [`SCDisplay`]. `None` selects the
/// main display (SCK's first, the M2.1 behavior); an id that no longer exists is
/// a [`CaptureError::DisplayNotFound`] rather than a silent fall-back to main.
fn pick_display(target: Option<u32>) -> Result<Retained<SCDisplay>, CaptureError> {
    let content = shareable_content()?;
    let list = unsafe { content.displays() };

    match target {
        None => list
            .firstObject()
            .ok_or_else(|| CaptureError::ScreenCaptureKit("no displays available to capture".into())),
        Some(id) => list
            .iter()
            .find(|d| unsafe { d.displayID() } == id)
            .ok_or(CaptureError::DisplayNotFound(id)),
    }
}

/// Fetch the current [`SCShareableContent`], blocking on SCK's async enumeration
/// (bounded by [`REPLY_TIMEOUT`]). Shared by [`displays`] and [`pick_display`].
fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: SCK hands back exactly one of content/error, both autoreleased.
            let reply = unsafe {
                if let Some(error) = error.as_ref() {
                    Err(map_error(error))
                } else if let Some(content) = content.as_ref() {
                    Ok(content.retain())
                } else {
                    Err(CaptureError::ScreenCaptureKit(
                        "no shareable content and no error".into(),
                    ))
                }
            };
            let _ = tx.send(reply);
        },
    );

    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    rx.recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| CaptureError::ScreenCaptureKit("timed out enumerating displays".into()))?
}

/// Start the stream, blocking until SCK confirms (or refuses).
fn start_capture(stream: &SCStream) -> Result<(), CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: a null error means the stream started.
        let reply = unsafe {
            match error.as_ref() {
                Some(error) => Err(map_error(error)),
                None => Ok(()),
            }
        };
        let _ = tx.send(reply);
    });

    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    rx.recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| CaptureError::ScreenCaptureKit("timed out starting capture".into()))?
}

/// Turn an `NSError` from SCK into a [`CaptureError`], singling out the one
/// failure an operator can actually fix.
fn map_error(error: &NSError) -> CaptureError {
    if error.code() == SCStreamErrorCode::UserDeclined.0 {
        return CaptureError::PermissionDenied;
    }

    CaptureError::ScreenCaptureKit(error.localizedDescription().to_string())
}
