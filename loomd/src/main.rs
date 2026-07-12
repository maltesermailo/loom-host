//! loomd — Loom host daemon binary (quinn server, session management).
//!
//! M1.1 scope: bring up the QUIC endpoint and drive the control-stream session
//! handshake (HELLO→WELCOME→CONFIG→CONFIG_ACK→START). Capture/encode/media land
//! in later milestones; see spec/ROADMAP.md.

use std::net::SocketAddr;

use clap::Parser;

use loomd::conn::HostCfg;
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

    /// Skip peer certificate verification. REQUIRED to accept any connection
    /// until certificate pinning lands in M7 — there is currently no way to
    /// authenticate peers, so without this flag loomd refuses to serve.
    #[arg(long)]
    insecure_dev: bool,

    /// Dev only: drop this percentage of outgoing media datagrams (deterministic,
    /// seeded) to exercise the freeze→IDR_REQUEST→recovery path. 0 = none.
    #[arg(long, default_value_t = 0)]
    drop_percent: u32,
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

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let endpoint = endpoint::server(addr)?;
    eprintln!(
        "loomd: listening on {} (ALPN loom/1, protocol v{})",
        endpoint.local_addr()?,
        loom_proto::PROTOCOL_VERSION
    );

    let params = MediaParams {
        width: args.width as u64,
        height: args.height as u64,
        ..MediaParams::default()
    };

    let cfg = HostCfg {
        name: args.name,
        params,
        drop_percent: args.drop_percent,
    };
    endpoint::accept_loop(endpoint, cfg).await;
    Ok(())
}
