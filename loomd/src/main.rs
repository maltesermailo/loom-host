//! loomd — Loom host daemon binary (quinn server, session management).
//!
//! M1.1 scope: bring up the QUIC endpoint and drive the control-stream session
//! handshake (HELLO→WELCOME→CONFIG→CONFIG_ACK→START). Capture/encode/media land
//! in later milestones; see spec/ROADMAP.md.

use std::net::SocketAddr;

use clap::Parser;

use loomd::conn::{HostCfg, StreamSpec};
use loomd::media::{CaptureSource, EncoderKind};
use loomd::session::MediaParams;
use loomd::{endpoint, BoxErr};

#[derive(Parser, Debug)]
#[command(name = "loomd", about = "Loom host daemon")]
struct Args {
    /// UDP port to bind (ARCHITECTURE §5.3 default).
    #[arg(long, default_value_t = 47800)]
    port: u16,

    /// Host display name shown to clients (WELCOME).
    #[arg(long, default_value = "loomd")]
    name: String,

    /// Virtual display width in pixels (even). Default is the v1 bar (§2).
    #[arg(long, default_value_t = 2560)]
    width: u32,

    /// Virtual display height in pixels (even).
    #[arg(long, default_value_t = 1440)]
    height: u32,

    /// Frame source. `synthetic` is the M1.2 test pattern (default, all
    /// platforms); `portal` is real Linux desktop capture (M1.4) and requires
    /// `--width/--height` to match the monitor's native resolution; `sck` is real
    /// macOS desktop capture (M2.1), which scales the display to `--width/--height`.
    #[arg(long, value_enum, default_value_t = CaptureSource::Synthetic)]
    source: CaptureSource,

    /// Which displays to stream (multi-display, M6.2). `main` (default) streams a
    /// single display, exactly as before. `all` streams every connected display,
    /// each as its own video stream; a comma-separated list of CGDirectDisplayIDs
    /// (see `cargo run -p loom-capture --example list-displays`) streams exactly
    /// those. `all`/list require `--source sck`; the extra streams are sent only
    /// to a client that negotiates multi-display (§3.4).
    #[arg(long, default_value = "main")]
    displays: String,

    /// HEVC encoder. `x265` is software (default, all platforms); `nvenc` is
    /// hardware and only exists in a build compiled with `--features nvenc`;
    /// `videotoolbox` is hardware and exists only in a macOS build.
    #[arg(long, value_enum, default_value_t = EncoderKind::X265)]
    encoder: EncoderKind,

    /// Skip peer certificate verification. REQUIRED to accept any connection
    /// until certificate pinning lands in M7 — there is currently no way to
    /// authenticate peers, so without this flag loomd refuses to serve.
    #[arg(long)]
    insecure_dev: bool,

    /// Dev only: drop this percentage of outgoing media datagrams (deterministic,
    /// seeded) to exercise the freeze→IDR_REQUEST→recovery path. 0 = none.
    #[arg(long, default_value_t = 0)]
    drop_percent: u32,

    /// Dev only: instead of serving, encode `--dump-frames` frames of the
    /// synthetic pattern to this raw Annex-B `.hevc` file and exit. This is the
    /// M3.2 offline test bitstream for Quest decoder bring-up (no network).
    #[arg(long)]
    dump_hevc: Option<std::path::PathBuf>,

    /// Frame count for `--dump-hevc`. The client loops the file, so a few hundred
    /// frames give plenty of decode samples for the R5 measurement.
    #[arg(long, default_value_t = 600)]
    dump_frames: u32,
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let args = Args::parse();

    // Structured JSON logs on stderr (the M1.2 recovery test + future latency
    // tooling parse these). RUST_LOG overrides the default info level.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    if !args.insecure_dev {
        eprintln!("loomd: refusing to serve without --insecure-dev.");
        eprintln!("       Certificate pinning is not implemented until M7; there is no");
        eprintln!("       way to authenticate peers yet. Pass --insecure-dev to run with");
        eprintln!("       peer verification DISABLED (dev / loopback only).");
        std::process::exit(2);
    }
    eprintln!("loomd: WARNING --insecure-dev — peer certificate verification is OFF (TODO M7).");

    if let Some(path) = &args.dump_hevc {
        let params = MediaParams {
            width: args.width as u64,
            height: args.height as u64,
            ..MediaParams::default()
        };
        eprintln!(
            "loomd: dumping {} synthetic HEVC frames ({}×{}) to {}",
            args.dump_frames,
            args.width,
            args.height,
            path.display()
        );
        loomd::media::dump_hevc(path, params, args.encoder, args.dump_frames)?;
        eprintln!("loomd: dump complete");
        return Ok(());
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let endpoint = endpoint::server(addr)?;
    eprintln!(
        "loomd: listening on {} (ALPN loom/1, protocol v{})",
        endpoint.local_addr()?,
        loom_proto::PROTOCOL_VERSION
    );

    let streams = match resolve_streams(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("loomd: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "loomd: serving {} video stream(s){}",
        streams.len(),
        if streams.len() > 1 {
            " — multi-display, offered to clients that negotiate it"
        } else {
            ""
        }
    );

    let cfg = HostCfg {
        name: args.name,
        streams,
        encoder: args.encoder,
        drop_percent: args.drop_percent,
    };
    endpoint::accept_loop(endpoint, cfg).await;
    Ok(())
}

/// Resolve `--displays` into the video streams to serve. `main` is a single
/// stream (today's behavior); `all` / an id list fan out one stream per display
/// (macOS/SCK only in M6.2). Streams get stream_ids 0, 2, 3, … (1 is audio), each
/// at its display's native resolution.
fn resolve_streams(args: &Args) -> Result<Vec<StreamSpec>, String> {
    let base = MediaParams {
        width: args.width as u64,
        height: args.height as u64,
        ..MediaParams::default()
    };

    if args.displays == "main" {
        return Ok(vec![StreamSpec {
            stream_id: 0,
            params: base,
            source: args.source,
            display: None,
            x: 0,
            y: 0,
        }]);
    }

    #[cfg(target_os = "macos")]
    {
        if args.source != CaptureSource::Sck {
            return Err(format!(
                "--displays {} requires --source sck (multi-display captures physical displays)",
                args.displays
            ));
        }

        let available = loom_capture::displays().map_err(|e| e.to_string())?;
        let selected: Vec<loom_capture::DisplayInfo> = if args.displays == "all" {
            available
        } else {
            let mut out = Vec::new();
            for tok in args.displays.split(',') {
                let id: u32 = tok.trim().parse().map_err(|_| {
                    format!("--displays: '{tok}' is not 'main', 'all', or a numeric display id")
                })?;
                let d = available.iter().find(|d| d.id == id).copied().ok_or_else(|| {
                    format!("--displays: display id {id} is not connected (see the list-displays example)")
                })?;
                out.push(d);
            }
            out
        };

        if selected.is_empty() {
            return Err("--displays: no displays available to stream".into());
        }

        // stream_id 0 for the first display, then 2, 3, … (1 is audio).
        let streams = selected
            .iter()
            .enumerate()
            .map(|(i, d)| StreamSpec {
                stream_id: if i == 0 { 0 } else { (i + 1) as u16 },
                params: MediaParams {
                    width: d.width as u64,
                    height: d.height as u64,
                    ..base
                },
                source: CaptureSource::Sck,
                display: Some(d.id),
                x: d.x,
                y: d.y,
            })
            .collect();
        Ok(streams)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err("--displays: only 'main' is supported on this platform (M6.2 multi-display is macOS/SCK)".into())
    }
}
