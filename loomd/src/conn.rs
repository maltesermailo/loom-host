//! Per-connection async driver — PROTOCOL.md §1.1, §3.1.
//!
//! One task per QUIC connection. It owns the transport (the control stream) and
//! nothing else: it reads length-prefixed frames, hands them to the sans-io
//! [`HostSession`], and flushes the session's [`Output`]s back onto the wire.
//! All protocol decisions live in [`crate::session`]; this file is I/O glue.
//!
//! Single-session policy (ARCHITECTURE §5): a shared 1-permit semaphore gates
//! the live session. A second connection that cannot take the permit is told
//! ERROR `BUSY` and closed (PROTOCOL.md §10).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use quinn::{Connection, RecvStream, SendStream, VarInt};
use tokio::sync::Semaphore;

use loom_proto::cbor::Value;
use loom_proto::control::{self, Decoded};
use loom_proto::errors;

use crate::media::{self, MediaHandle};
use crate::session::{HostSession, MediaParams, Output, State};

/// Immutable per-daemon settings handed to each connection.
#[derive(Clone)]
pub struct HostCfg {
    /// Host display name (WELCOME key 1).
    pub name: String,
    /// Media parameters advertised in CONFIG.
    pub params: MediaParams,
    /// Frame source the media thread encodes (`--source`).
    pub source: media::CaptureSource,
    /// HEVC encoder backend (`--encoder`).
    pub encoder: media::EncoderKind,
    /// Dev datagram-loss injection percentage (`--drop-percent`; 0 = none).
    pub drop_percent: u32,
}

/// Accept and drive one inbound connection to completion. Never panics; logs
/// and returns on any transport error.
pub async fn handle(incoming: quinn::Incoming, slot: Arc<Semaphore>, cfg: HostCfg) {
    let remote = incoming.remote_address();
    let connection = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[conn] handshake from {remote} failed: {e}");
            return;
        }
    };

    // Single-session gate (§5): a second live connection is BUSY.
    let permit = match slot.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("[conn] {remote} rejected: BUSY");
            reject_busy(&connection).await;
            return;
        }
    };

    if let Err(e) = run_session(&connection, &cfg).await {
        eprintln!("[conn] {remote} session ended: {e}");
    }
    drop(permit);
}

/// Drive the session state machine over the accepted control stream, owning the
/// media thread's lifetime so it is always joined on exit.
async fn run_session(connection: &Connection, cfg: &HostCfg) -> std::io::Result<()> {
    let mut media: Option<MediaHandle> = None;
    let result = session_loop(connection, cfg, &mut media).await;
    if let Some(m) = media.take() {
        // The connection is closed by now, so the media thread stops promptly.
        m.join();
    }
    result
}

async fn session_loop(
    connection: &Connection,
    cfg: &HostCfg,
    media: &mut Option<MediaHandle>,
) -> std::io::Result<()> {
    // The client opens exactly one bidirectional stream: the control stream.
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| std::io::Error::other(format!("accept_bi: {e}")))?;

    let mut session = HostSession::new(cfg.name.clone(), gen_session_id(), cfg.params);

    loop {
        match read_frame(&mut recv).await? {
            FrameRead::Frame(bytes) => match control::decode_frame(&bytes) {
                Ok(decoded) => {
                    log_incoming(&decoded);
                    let outputs = session.on_frame(decoded);
                    if drive(&mut send, connection, cfg, media, outputs).await? {
                        return Ok(());
                    }
                }
                Err(_) => {
                    // Malformed framing/CBOR the SM never sees (§6.6).
                    send_frame(
                        &mut send,
                        control::ERROR,
                        &error_body(errors::PROTOCOL_VIOLATION),
                    )
                    .await?;
                    let _ = send.finish();
                    connection.close(
                        VarInt::from_u32(errors::PROTOCOL_VIOLATION as u32),
                        b"bad frame",
                    );
                    return Ok(());
                }
            },
            FrameRead::Eof => {
                // Peer finished the stream without BYE; treat as a clean close.
                connection.close(VarInt::from_u32(errors::NONE as u32), b"eof");
                return Ok(());
            }
        }
        if session.state() == State::Closed {
            return Ok(());
        }
    }
}

/// Apply session outputs to the wire. Returns `true` when the connection is done.
async fn drive(
    send: &mut SendStream,
    connection: &Connection,
    cfg: &HostCfg,
    media: &mut Option<MediaHandle>,
    outputs: Vec<Output>,
) -> std::io::Result<bool> {
    for out in outputs {
        match out {
            Output::Send { msg_type, body } => {
                send_frame(send, msg_type, &body).await?;
            }
            Output::StartMedia => {
                // Spawn the media pipeline (synthetic or portal capture) on its
                // own thread; §5 encode + §4 fragmentation are source-agnostic.
                *media = Some(media::spawn(
                    connection.clone(),
                    cfg.params,
                    cfg.source,
                    cfg.encoder,
                    cfg.drop_percent,
                ));
            }
            Output::RequestIdr => {
                if let Some(m) = media.as_ref() {
                    m.request_idr();
                }
            }
            Output::Reconfigure { params } => {
                // A VIEWPORT-driven resolution change was ACKed (§8): switch the
                // running media thread to the new size on its next frame.
                if let Some(m) = media.as_ref() {
                    m.reconfigure(params);
                }
            }
            Output::ClockPong { t0 } => {
                // Stamp host receive/send times from the shared clock (§7). On
                // loopback the receive→send gap is sub-µs, so t1 == t2 is fine.
                let now = crate::clock::host_now_us() as i128;
                send_frame(
                    send,
                    control::CLOCK_PONG,
                    &[
                        (Value::Int(0), Value::Int(t0 as i128)),
                        (Value::Int(1), Value::Int(now)),
                        (Value::Int(2), Value::Int(now)),
                    ],
                )
                .await?;
            }
            Output::Stats(r) => {
                tracing::info!(
                    target: "loom::stats", event = "stats",
                    frames_received = r.frames_received, frames_dropped = r.frames_dropped,
                    datagrams = r.datagrams, jitter_ms = r.jitter_ms, decode_us = r.decode_us,
                    rtt_us = r.rtt_us, e2e_us = r.e2e_us.unwrap_or(-1)
                );
            }
            Output::Close { code } => {
                let _ = send.finish();
                connection.close(VarInt::from_u32(code as u32), errors::name(code).as_bytes());
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Tell a surplus connection it's BUSY, on its own control stream, then close.
async fn reject_busy(connection: &Connection) {
    if let Ok((mut send, _recv)) = connection.accept_bi().await {
        let _ = send_frame(&mut send, control::ERROR, &error_body(errors::BUSY)).await;
        let _ = send.finish();
    }
    connection.close(VarInt::from_u32(errors::BUSY as u32), b"busy");
}

fn error_body(code: u64) -> Vec<(Value, Value)> {
    vec![
        (Value::Int(0), Value::Int(code as i128)),
        (Value::Int(1), Value::Text(errors::name(code).to_string())),
    ]
}

/// Encode a message via `loom_proto` and write it to the control stream.
async fn send_frame(
    send: &mut SendStream,
    msg_type: u64,
    body: &[(Value, Value)],
) -> std::io::Result<()> {
    let frame = control::encode_frame(msg_type, body);
    send.write_all(&frame)
        .await
        .map_err(|e| std::io::Error::other(format!("write: {e}")))
}

enum FrameRead {
    Frame(Vec<u8>),
    Eof,
}

/// Read one length-prefixed control frame (§3.1). Returns the full frame bytes
/// (length prefix included) so `decode_frame` can validate them uniformly.
async fn read_frame(recv: &mut RecvStream) -> std::io::Result<FrameRead> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(FrameRead::Eof),
        Err(e) => return Err(std::io::Error::other(format!("read len: {e}"))),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > control::MAX_FRAME_BODY {
        return Err(std::io::Error::other("frame body over limit"));
    }
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&len_buf);
    frame.resize(4 + len, 0);
    recv.read_exact(&mut frame[4..])
        .await
        .map_err(|e| std::io::Error::other(format!("read body: {e}")))?;
    Ok(FrameRead::Frame(frame))
}

fn log_incoming(decoded: &Decoded) {
    if let Decoded::Message { msg_type, .. } = decoded {
        eprintln!("[conn] <- msg 0x{msg_type:02x}");
    }
}

/// A non-semantic 16-byte session id (WELCOME key 2, "for logs/UI, no protocol
/// semantics"). Dev-grade uniqueness from the wall clock — not a secret.
fn gen_session_id() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut id = [0u8; 16];
    id.copy_from_slice(&nanos.to_be_bytes());
    id
}
