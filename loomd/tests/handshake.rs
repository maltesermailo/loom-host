//! In-process loopback handshake tests — spec/ROADMAP.md M1.1 accept criteria.
//!
//! These drive a real quinn client against loomd's own accept loop over the
//! loopback interface (no separate process, no external client). They cover:
//!   * a full HELLO→WELCOME→CONFIG→CONFIG_ACK→START handshake,
//!   * a wrong-version HELLO → ERROR VERSION_UNSUPPORTED,
//!   * a second concurrent client → ERROR BUSY.
//!
//! The client half deliberately hand-encodes with `loom_proto` (the same crate
//! the daemon uses) — the *independent* C++ client is exercised end-to-end by
//! the STEP 4 demo, not here; this keeps the host repo's check.sh hermetic.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::Semaphore;

use loom_proto::cbor::Value;
use loom_proto::control::{self, Decoded};
use loom_proto::errors;

use loomd::conn::{self, HostCfg};
use loomd::endpoint;
use loomd::session::MediaParams;

/// Spin up a server endpoint on an ephemeral loopback port and an accept loop
/// that honours the single-session semaphore. Returns the bound address.
fn spawn_host() -> SocketAddr {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let server = endpoint::server(addr).expect("server endpoint");
    let bound = server.local_addr().expect("local addr");
    let cfg = HostCfg {
        name: "test-host".into(),
        // Small frames keep these handshake tests light — they reach STREAMING,
        // which now spawns the real encoder/media thread.
        params: MediaParams {
            width: 320,
            height: 240,
            ..MediaParams::default()
        },
        source: loomd::media::CaptureSource::Synthetic,
        encoder: loomd::media::EncoderKind::X265,
        drop_percent: 0,
    };
    // Mirror endpoint::accept_loop but keep the handle-per-conn behaviour under
    // the test's own semaphore so BUSY is exercised exactly as in production.
    let slot = Arc::new(Semaphore::new(1));
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            tokio::spawn(conn::handle(incoming, slot.clone(), cfg.clone()));
        }
    });
    bound
}

async fn connect(addr: SocketAddr) -> (Endpoint, Connection, SendStream, RecvStream) {
    let ep = endpoint::client().expect("client endpoint");
    let conn = ep
        .connect(addr, "localhost")
        .expect("connect")
        .await
        .expect("handshake");
    let (send, recv) = conn.open_bi().await.expect("open control stream");
    (ep, conn, send, recv)
}

async fn send_msg(send: &mut SendStream, msg_type: u64, body: &[(Value, Value)]) {
    let frame = control::encode_frame(msg_type, body);
    send.write_all(&frame).await.expect("write frame");
}

/// Read one length-prefixed control frame and decode it with `loom_proto`.
async fn read_msg(recv: &mut RecvStream) -> Decoded {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut frame = len_buf.to_vec();
    frame.resize(4 + len, 0);
    recv.read_exact(&mut frame[4..]).await.expect("read body");
    control::decode_frame(&frame).expect("decode")
}

fn hello_body(version: i128, codecs: Vec<i128>) -> Vec<(Value, Value)> {
    vec![
        (Value::Int(0), Value::Int(version)),
        (Value::Int(1), Value::Text("test-client".into())),
        (
            Value::Int(2),
            Value::Array(codecs.into_iter().map(Value::Int).collect()),
        ),
        (
            Value::Int(3),
            Value::Array(vec![Value::Int(2560), Value::Int(1440)]),
        ),
        (Value::Int(4), Value::Int(72)),
        (Value::Int(5), Value::Int(0)),
    ]
}

fn msg_type_of(d: &Decoded) -> u64 {
    match d {
        Decoded::Message { msg_type, .. } => *msg_type,
        Decoded::Ignored => panic!("unexpected Ignored"),
    }
}

fn int_key(d: &Decoded, key: i128) -> Option<i128> {
    match d {
        Decoded::Message { body, .. } => body.iter().find_map(|(k, v)| match (k, v) {
            (Value::Int(ki), Value::Int(vi)) if *ki == key => Some(*vi),
            _ => None,
        }),
        Decoded::Ignored => None,
    }
}

/// Await the connection's terminal state and return its application close code.
/// Fatal errors are signalled by the QUIC application close code (PROTOCOL §10
/// defines these codes as exactly that); the preceding ERROR frame is
/// best-effort and can be preempted by the close, so tests key off the code.
async fn close_code(conn: &Connection) -> u64 {
    match conn.closed().await {
        quinn::ConnectionError::ApplicationClosed(ac) => ac.error_code.into_inner(),
        other => panic!("expected application close, got {other:?}"),
    }
}

#[tokio::test]
async fn full_handshake_completes() {
    let addr = spawn_host();
    let (_ep, _conn, mut send, mut recv) = connect(addr).await;

    send_msg(&mut send, control::HELLO, &hello_body(1, vec![1])).await;

    let welcome = read_msg(&mut recv).await;
    assert_eq!(msg_type_of(&welcome), control::WELCOME);
    assert_eq!(int_key(&welcome, 0), Some(1)); // chosen protocol_version

    let config = read_msg(&mut recv).await;
    assert_eq!(msg_type_of(&config), control::CONFIG);
    assert_eq!(int_key(&config, 0), Some(1)); // generation 1
    assert_eq!(int_key(&config, 1), Some(1)); // HEVC

    send_msg(
        &mut send,
        control::CONFIG_ACK,
        &[(Value::Int(0), Value::Int(1))],
    )
    .await;

    let start = read_msg(&mut recv).await;
    assert_eq!(msg_type_of(&start), control::START);
}

#[tokio::test]
async fn wrong_version_hello_gets_version_unsupported() {
    let addr = spawn_host();
    let (_ep, conn, mut send, _recv) = connect(addr).await;

    send_msg(&mut send, control::HELLO, &hello_body(2, vec![1])).await;

    assert_eq!(close_code(&conn).await, errors::VERSION_UNSUPPORTED);
}

#[tokio::test]
async fn second_client_gets_busy() {
    let addr = spawn_host();

    // First client completes the handshake and holds the session open.
    let (_ep1, _conn1, mut send1, mut recv1) = connect(addr).await;
    send_msg(&mut send1, control::HELLO, &hello_body(1, vec![1])).await;
    assert_eq!(msg_type_of(&read_msg(&mut recv1).await), control::WELCOME);
    assert_eq!(msg_type_of(&read_msg(&mut recv1).await), control::CONFIG);

    // Second client, while the first is still live, must be told BUSY. It sends
    // HELLO like any real client (which also materialises its control stream on
    // the wire so the host can reply on it) and observes a BUSY close.
    let (_ep2, conn2, mut send2, _recv2) = connect(addr).await;
    send_msg(&mut send2, control::HELLO, &hello_body(1, vec![1])).await;
    assert_eq!(close_code(&conn2).await, errors::BUSY);
}
