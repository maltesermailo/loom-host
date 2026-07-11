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

    /// Skip peer certificate verification. REQUIRED to accept any connection
    /// until certificate pinning lands in M7 — there is currently no way to
    /// authenticate peers, so without this flag loomd refuses to serve.
    #[arg(long)]
    insecure_dev: bool,
}

#[tokio::main]
async fn main() -> Result<(), BoxErr> {
    let args = Args::parse();

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

    let cfg = HostCfg {
        name: args.name,
        params: MediaParams::default(),
    };
    endpoint::accept_loop(endpoint, cfg).await;
    Ok(())
}
