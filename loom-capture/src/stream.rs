//! The synchronous PipeWire capture stream (SHM path).
//!
//! Runs the PipeWire main loop on a dedicated thread: offers an SHM `video/raw`
//! format (no dmabuf modifier — R2 verdict defers zero-copy to M1.5+), and on
//! each delivered frame converts the packed buffer to I420 and publishes it into
//! a shared latest-frame slot the media thread polls. Delivery is damage-driven,
//! not clock-paced (R2 verdict), so the slot simply always holds the newest
//! frame; `loomd` does the 72 fps pacing and last-frame repeat.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use crate::convert::{self, PixelFormat};
use crate::frame::I420Buffer;
use crate::CaptureError;

/// Shared latest-frame slot: capture publishes, the media thread takes.
pub type FrameSlot = Arc<Mutex<Option<I420Buffer>>>;

/// State owned by the PipeWire thread and its callbacks.
struct StreamData {
    expected: (u32, u32),
    fmt: Option<PixelFormat>,
    slot: FrameSlot,
    spare: Option<I420Buffer>,
    ready: Option<Sender<Result<(), CaptureError>>>,
}

/// Run the capture loop until `stop` is set. Reports first-format success (or
/// failure) exactly once over `ready`, so `PortalCapture::start` can surface a
/// size mismatch or negotiation error before returning.
pub fn run(
    fd: OwnedFd,
    node_id: u32,
    expected: (u32, u32),
    refresh: u32,
    slot: FrameSlot,
    stop: Arc<AtomicBool>,
    ready: Sender<Result<(), CaptureError>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoop::new(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Pipewire(format!("main loop: {e}"))));
            return;
        }
    };
    let context = match pw::context::Context::new(&mainloop) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Pipewire(format!("context: {e}"))));
            return;
        }
    };
    let core = match context.connect_fd(fd, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Pipewire(format!("connect_fd: {e}"))));
            return;
        }
    };

    let stream = match pw::stream::Stream::new(
        &core,
        "loom-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(CaptureError::Pipewire(format!("stream: {e}"))));
            return;
        }
    };

    let state = Rc::new(RefCell::new(StreamData {
        expected,
        fmt: None,
        slot,
        spare: None,
        ready: Some(ready),
    }));

    let ml_fmt = mainloop.clone();
    let ml_err = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state.clone())
        .state_changed(move |_s, ud, _old, new| {
            if let pw::stream::StreamState::Error(msg) = new {
                report(
                    ud,
                    Err(CaptureError::Pipewire(format!("stream error: {msg}"))),
                );
                ml_err.quit();
            }
        })
        .param_changed(move |_stream, ud, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };

            if let Err(e) = on_format(ud, param) {
                report(ud, Err(e));
                ml_fmt.quit();
            }
        })
        .process(|stream, ud| {
            on_process(stream, ud);
        })
        .register();

    let _listener = match _listener {
        Ok(l) => l,
        Err(e) => {
            report(
                &state,
                Err(CaptureError::Pipewire(format!("listener: {e}"))),
            );
            return;
        }
    };

    // SHM offer only (no VideoModifier property → no dmabuf). TODO(R2): a fixated
    // modifier list + EGL/GBM import would enable dmabuf→CUDA zero-copy (M1.5+).
    let pod = match build_shm_format(expected, refresh) {
        Ok(p) => p,
        Err(e) => {
            report(&state, Err(e));
            return;
        }
    };
    let mut params = [Pod::from_bytes(&pod).expect("serialized pod is valid")];

    if let Err(e) = stream.connect(
        pw::spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        report(&state, Err(CaptureError::Pipewire(format!("connect: {e}"))));
        return;
    }

    // Poll the stop flag on the loop so `PortalCapture` drop can shut us down
    // (the MainLoop is !Send, so we can't quit it from the media thread).
    let ml_stop = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            ml_stop.quit();
        }
    });
    let _ = timer.update_timer(
        Some(Duration::from_millis(50)),
        Some(Duration::from_millis(50)),
    );

    mainloop.run();
}

/// Validate the fixated format and lock in the pixel layout. Errors on a size
/// mismatch (no scaler in M1.4 — the operator sets `--width/--height`) or an
/// unexpected pixel format.
fn on_format(ud: &mut Rc<RefCell<StreamData>>, param: &Pod) -> Result<(), CaptureError> {
    let (media_type, media_subtype) = format_utils::parse_format(param)
        .map_err(|_| CaptureError::Pipewire("parse_format".into()))?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return Ok(());
    }

    // Use the canonical spa parser (spa_format_video_raw_parse) rather than
    // walking pod properties by hand — the fixated VideoFormat isn't a plain
    // Value::Id when deserialized generically.
    let mut info = VideoInfoRaw::new();
    info.parse(param)
        .map_err(|e| CaptureError::Pipewire(format!("parse video format: {e}")))?;

    let fmt = match info.format() {
        VideoFormat::BGRA | VideoFormat::BGRx => PixelFormat::BGRX,
        VideoFormat::RGBA | VideoFormat::RGBx => PixelFormat::RGBX,
        other => {
            return Err(CaptureError::Pipewire(format!(
                "unexpected pixel format {other:?}"
            )))
        }
    };
    let size = info.size();
    let (w, h) = (size.width, size.height);

    let mut s = ud.borrow_mut();
    if (w, h) != s.expected {
        return Err(CaptureError::SizeMismatch {
            got: (w, h),
            want: s.expected,
        });
    }
    s.fmt = Some(fmt);
    drop(s);

    report(ud, Ok(()));
    tracing::info!(target: "loom::capture", width = w, height = h, ?fmt, "capture format negotiated (SHM)");

    Ok(())
}

/// Per delivered frame: convert the packed SHM buffer to I420 and publish it.
fn on_process(stream: &pw::stream::StreamRef, ud: &mut Rc<RefCell<StreamData>>) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };

    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let stride = data.chunk().stride() as usize;
    let offset = data.chunk().offset() as usize;
    let Some(bytes) = data.data() else { return };
    if stride == 0 || offset + stride > bytes.len() {
        return;
    }

    let mut s = ud.borrow_mut();
    let Some(fmt) = s.fmt else { return };
    let (w, h) = s.expected;

    let mut frame = s.spare.take().unwrap_or_else(|| I420Buffer::new(w, h));
    convert::to_i420(&bytes[offset..], stride, fmt, &mut frame);

    let old = s.slot.lock().expect("frame slot poisoned").replace(frame);
    s.spare = old;
}

fn report(ud: &Rc<RefCell<StreamData>>, result: Result<(), CaptureError>) {
    if let Some(tx) = ud.borrow_mut().ready.take() {
        let _ = tx.send(result);
    }
}

/// Build an SHM `video/raw` EnumFormat pod fixed to `expected` size. Lists the
/// four packed 32-bit formats the converter handles; no VideoModifier property,
/// which is what keeps the offer on the SHM path.
fn build_shm_format((w, h): (u32, u32), refresh: u32) -> Result<Vec<u8>, CaptureError> {
    let props = vec![
        prop_id(FormatProperties::MediaType, MediaType::Video.as_raw()),
        prop_id(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
        Property {
            key: FormatProperties::VideoFormat.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Id(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: Id(VideoFormat::BGRA.as_raw()),
                    alternatives: vec![
                        Id(VideoFormat::BGRA.as_raw()),
                        Id(VideoFormat::RGBA.as_raw()),
                        Id(VideoFormat::BGRx.as_raw()),
                        Id(VideoFormat::RGBx.as_raw()),
                    ],
                },
            ))),
        },
        Property {
            key: FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle {
                        width: w,
                        height: h,
                    },
                    min: Rectangle {
                        width: 1,
                        height: 1,
                    },
                    max: Rectangle {
                        width: 8192,
                        height: 8192,
                    },
                },
            ))),
        },
        Property {
            key: FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction {
                        num: refresh.max(1),
                        denom: 1,
                    },
                    min: Fraction { num: 0, denom: 1 },
                    max: Fraction {
                        num: 1000,
                        denom: 1,
                    },
                },
            ))),
        },
    ];

    let value = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: props,
    });

    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map_err(|e| CaptureError::Pipewire(format!("serialize format: {e}")))?
        .0
        .into_inner();

    Ok(bytes)
}

fn prop_id(key: FormatProperties, raw: u32) -> Property {
    Property {
        key: key.as_raw(),
        flags: PropertyFlags::empty(),
        value: Value::Id(Id(raw)),
    }
}
