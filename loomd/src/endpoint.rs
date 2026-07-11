//! quinn endpoint construction + the accept loop — ARCHITECTURE §5, PROTOCOL §2.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::{Endpoint, TransportConfig};
use tokio::sync::Semaphore;

use crate::conn::{self, HostCfg};
use crate::{tls, BoxErr};

/// Transport tuning per PROTOCOL.md §2: keep-alive ≤ 5 s, idle timeout ≤ 15 s.
fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(
        Duration::from_secs(15)
            .try_into()
            .expect("15s is a valid idle timeout"),
    ));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    Arc::new(t)
}

/// A server endpoint bound to `addr`, presenting a dev cert and skipping peer
/// verification (TODO(M7); gated behind `--insecure-dev` by the binary).
pub fn server(addr: SocketAddr) -> Result<Endpoint, BoxErr> {
    let mut cfg = tls::insecure_server_config()?;
    cfg.transport_config(transport());
    Ok(Endpoint::server(cfg, addr)?)
}

/// A client endpoint that trusts any server cert (TODO(M7)). Used by the
/// in-process handshake test; production clients use msquic, not this.
pub fn client() -> Result<Endpoint, BoxErr> {
    let mut ep = Endpoint::client("0.0.0.0:0".parse().expect("valid bind addr"))?;
    ep.set_default_client_config(tls::insecure_client_config()?);
    Ok(ep)
}

/// Accept connections forever, one task each. A single 1-permit semaphore
/// enforces the one-session-at-a-time policy across all tasks (§5).
pub async fn accept_loop(endpoint: Endpoint, cfg: HostCfg) {
    let slot = Arc::new(Semaphore::new(1));
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(conn::handle(incoming, slot.clone(), cfg.clone()));
    }
}
