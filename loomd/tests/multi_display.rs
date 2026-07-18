//! In-process loopback test for multi-display fan-out — spec/ROADMAP.md M6.2.
//!
//! Drives a real quinn client through the multi-display handshake against a host
//! configured with two streams, and asserts the host (a) advertises the feature
//! (WELCOME key 3 + CONFIG key 6) and (b) actually fans out — video datagrams
//! arrive on **both** stream_id 0 (primary) and stream_id 2 (the extra display).
//!
//! The streams use the synthetic source so the test is hermetic (no
//! ScreenCaptureKit / Screen-Recording permission); the fan-out mechanism is
//! source-agnostic, so this exercises exactly the §4 wire behavior a real
//! two-monitor SCK session produces.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::Semaphore;

use loom_proto::cbor::Value;
use loom_proto::control::{self, Decoded};
use loom_proto::datagram;

use loomd::conn::{self, HostCfg};
use loomd::endpoint;
use loomd::media::{CaptureSource, EncoderKind};
use loomd::session::MediaParams;

/// The extra display's stream_id (§3.4: 0 primary, 1 audio, ≥ 2 extra).
const EXTRA_STREAM: u16 = 2;

/// Host serving two small synthetic streams: primary (0) + one extra (2).
fn spawn_host() -> SocketAddr {
    let server = endpoint::server(([127, 0, 0, 1], 0).into()).expect("server");
    let bound = server.local_addr().expect("addr");
    let stream = |stream_id| conn::StreamSpec {
        stream_id,
        params: MediaParams {
            width: 320,
            height: 240,
            ..MediaParams::default()
        },
        source: CaptureSource::Synthetic,
        display: None,
    };
    let cfg = HostCfg {
        name: "multi-host".into(),
        streams: vec![stream(0), stream(EXTRA_STREAM)],
        encoder: EncoderKind::X265,
        drop_percent: 0,
    };
    let slot = Arc::new(Semaphore::new(1));
    tokio::spawn(async move {
        while let Some(inc) = server.accept().await {
            tokio::spawn(conn::handle(inc, slot.clone(), cfg.clone()));
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
    send.write_all(&control::encode_frame(msg_type, body))
        .await
        .expect("write frame");
}

async fn read_msg(recv: &mut RecvStream) -> Decoded {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut frame = len_buf.to_vec();
    frame.resize(4 + len, 0);
    recv.read_exact(&mut frame[4..]).await.expect("read body");
    control::decode_frame(&frame).expect("decode")
}

/// HELLO advertising multi-display fan-in (key 5 bit 1) and a decode max.
fn hello_multi() -> Vec<(Value, Value)> {
    vec![
        (Value::Int(0), Value::Int(1)),
        (Value::Int(1), Value::Text("test-client".into())),
        (Value::Int(2), Value::Array(vec![Value::Int(1)])),
        (
            Value::Int(3),
            Value::Array(vec![Value::Int(2560), Value::Int(1440)]),
        ),
        (Value::Int(4), Value::Int(72)),
        (Value::Int(5), Value::Int(0b11)), // audio | multi-display
    ]
}

fn body_of(d: &Decoded) -> &[(Value, Value)] {
    match d {
        Decoded::Message { body, .. } => body,
        Decoded::Ignored => panic!("unexpected Ignored"),
    }
}

fn int_key(body: &[(Value, Value)], key: i128) -> Option<i128> {
    body.iter().find_map(|(k, v)| match (k, v) {
        (Value::Int(ki), Value::Int(vi)) if *ki == key => Some(*vi),
        _ => None,
    })
}

#[tokio::test]
async fn multi_display_negotiates_and_fans_out() {
    let addr = spawn_host();
    let (_ep, conn, mut send, mut recv) = connect(addr).await;

    send_msg(&mut send, control::HELLO, &hello_multi()).await;

    // WELCOME echoes the active feature set: bit 1 = multi-display.
    let welcome = read_msg(&mut recv).await;
    assert_eq!(int_key(body_of(&welcome), 3), Some(0b10), "WELCOME key 3");

    // CONFIG carries the extra stream in key 6.
    let config = read_msg(&mut recv).await;
    let extras = body_of(&config)
        .iter()
        .find(|(k, _)| *k == Value::Int(6))
        .map(|(_, v)| v);
    match extras {
        Some(Value::Array(streams)) => {
            assert_eq!(streams.len(), 1);
            match &streams[0] {
                Value::Map(desc) => assert_eq!(int_key(desc, 0), Some(EXTRA_STREAM as i128)),
                other => panic!("bad stream descriptor: {other:?}"),
            }
        }
        other => panic!("expected CONFIG key 6 array, got {other:?}"),
    }

    send_msg(
        &mut send,
        control::CONFIG_ACK,
        &[(Value::Int(0), Value::Int(1))],
    )
    .await;
    assert!(
        matches!(read_msg(&mut recv).await, Decoded::Message { msg_type, .. } if msg_type == control::START)
    );

    // The host now fans out: collect stream_ids off the datagram path until both
    // the primary (0) and the extra (2) have delivered a frame.
    let mut seen: HashSet<u16> = HashSet::new();
    let collect = async {
        while seen.len() < 2 {
            let dg = conn.read_datagram().await.expect("datagram");
            // The extra stream_id (2) is only accepted once negotiated (§4).
            let d = datagram::decode_with_streams(&dg, &[EXTRA_STREAM]).expect("valid datagram");
            seen.insert(d.header.stream_id);
        }
    };
    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .expect("saw both streams within 10s");

    assert!(seen.contains(&0), "primary stream delivered");
    assert!(
        seen.contains(&EXTRA_STREAM),
        "extra display stream delivered"
    );
}
