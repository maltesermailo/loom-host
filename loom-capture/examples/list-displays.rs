//! list-displays — enumerate the displays ScreenCaptureKit can capture (M6.2).
//!
//! Prints each display's `CGDirectDisplayID` and native size — the ids the
//! multi-display fan-out streams by. A desk-side aid, like `capture-dump`; needs
//! Screen Recording permission for the launching terminal (the error says how).
//!
//! Usage: `cargo run -p loom-capture --example list-displays`

#[cfg(target_os = "macos")]
fn main() {
    match loom_capture::displays() {
        Ok(displays) => {
            println!("{} display(s):", displays.len());
            for (i, d) in displays.iter().enumerate() {
                let role = if i == 0 { " (main)" } else { "" };
                println!("  [{i}] id={} {}x{}{role}", d.id, d.width, d.height);
            }
        }
        Err(e) => {
            eprintln!("list-displays: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("list-displays: macOS-only (ScreenCaptureKit).");
    std::process::exit(1);
}
