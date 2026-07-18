//! capture-dump — standalone capture repro (ARCHITECTURE §14 working note).
//!
//! Starts a capture at the requested size and, for ~5 s, reports delivered-frame
//! timing and writes the first few frames as raw I420 to /tmp so the capture path
//! can be exercised without the whole daemon. Not part of the streaming pipeline
//! — a desk-side debugging aid.
//!
//! Usage: `cargo run -p loom-capture --example capture-dump -- [WIDTH HEIGHT]`
//!
//! View a dump with:
//!   `ffplay -f rawvideo -pixel_format yuv420p -video_size WxH /tmp/loom-capture-0.i420`

// Errors print via Display, not the Debug form `fn main() -> Result` would use:
// CaptureError's actionable text (permission hints) is the whole point of it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    if let Err(e) = run() {
        eprintln!("capture-dump: {e}");
        std::process::exit(1);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use loom_capture::I420Buffer;

    let mut args = std::env::args().skip(1);
    let width: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(2560);
    let height: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1440);

    let capture = start(width, height)?;
    eprintln!("capture-dump: negotiated; sampling for 5 s");

    let mut frame = I420Buffer::new(width, height);
    let (mut frames, mut dumped) = (0u32, 0u32);
    let (start_time, mut last) = (Instant::now(), Instant::now());
    let (mut min_gap, mut max_gap) = (f64::MAX, 0.0f64);

    while start_time.elapsed() < Duration::from_secs(5) {
        if capture.fill(&mut frame) {
            let now = Instant::now();
            let gap = now.duration_since(last).as_secs_f64() * 1000.0;
            if frames > 0 {
                min_gap = min_gap.min(gap);
                max_gap = max_gap.max(gap);
            }
            last = now;
            frames += 1;

            if dumped < 3 {
                let path = format!("/tmp/loom-capture-{dumped}.i420");
                let mut f = std::fs::File::create(&path)?;
                for plane in frame.planes() {
                    f.write_all(plane)?;
                }
                eprintln!("capture-dump: wrote {path}");
                dumped += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // A capture that "works" but publishes an all-black luma plane is the exact
    // failure a permission problem produces, so report the mean rather than
    // leaving it to look right.
    let luma = frame.planes()[0];
    let mean = luma.iter().map(|&p| p as u64).sum::<u64>() / luma.len().max(1) as u64;
    eprintln!(
        "capture-dump: {frames} polls saw a frame; inter-poll gap min {min_gap:.1} ms / max {max_gap:.1} ms; mean luma {mean}"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
fn start(
    width: u32,
    height: u32,
) -> Result<loom_capture::PortalCapture, loom_capture::CaptureError> {
    eprintln!("capture-dump: requesting {width}x{height} — approve the portal dialog");
    loom_capture::PortalCapture::start(width, height, 72)
}

#[cfg(target_os = "macos")]
fn start(
    width: u32,
    height: u32,
) -> Result<loom_capture::ScreenCapture, loom_capture::CaptureError> {
    eprintln!("capture-dump: requesting {width}x{height} via ScreenCaptureKit");
    // None = main display; this aid predates multi-display target selection.
    loom_capture::ScreenCapture::start(None, width, height, 72)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("capture-dump: no capture backend on this platform");
}
