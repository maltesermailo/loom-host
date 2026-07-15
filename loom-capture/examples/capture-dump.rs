//! capture-dump — standalone portal-capture repro (ARCHITECTURE §14 working note).
//!
//! Starts a ScreenCast capture at the requested size and, for ~5 s, reports
//! delivered-frame timing and writes the first few frames as raw I420 to /tmp so
//! the capture path can be exercised without the whole daemon. Not part of the
//! streaming pipeline — a desk-side debugging aid.
//!
//! Usage: `cargo run -p loom-capture --example capture-dump -- [WIDTH HEIGHT]`
//!
//! Linux-only: portal capture does not exist on the macOS host.

#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use loom_capture::{I420Buffer, PortalCapture};

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("capture-dump: portal capture is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let width: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(2560);
    let height: u32 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1440);

    eprintln!("capture-dump: requesting {width}x{height} — approve the portal dialog");
    let capture = PortalCapture::start(width, height, 72)?;
    eprintln!("capture-dump: negotiated; sampling for 5 s");

    let mut frame = I420Buffer::new(width, height);
    let (mut frames, mut dumped) = (0u32, 0u32);
    let (start, mut last) = (Instant::now(), Instant::now());
    let (mut min_gap, mut max_gap) = (f64::MAX, 0.0f64);

    while start.elapsed() < Duration::from_secs(5) {
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

    eprintln!(
        "capture-dump: {frames} polls saw a frame; inter-poll gap min {min_gap:.1} ms / max {max_gap:.1} ms"
    );

    Ok(())
}
