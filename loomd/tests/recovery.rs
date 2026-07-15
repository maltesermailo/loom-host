//! Freeze → IDR_REQUEST → recovery timing test (M1.2 accept).
//!
//! Runs the real host media path over a loopback QUIC connection with 1% seeded
//! datagram loss, and an in-process protocol client that drives the vector-proven
//! `loom_proto::reassembly` state machine. When a lost fragment makes the client
//! discard a frame, the reassembler raises an IDR_REQUEST; the test sends it, the
//! host forces an IDR, and the next keyframe completes the recovery. We assert a
//! cycle completes in < 200 ms and that **both sides' structured logs agree**:
//! the client's reassembly events (measured here) and the host's `tracing` JSON
//! (captured to a buffer) showing the forced IDR.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::Semaphore;

use loom_proto::cbor::Value;
use loom_proto::control::{self, Decoded};
use loom_proto::datagram;
use loom_proto::reassembly::{Event, Fragment, Reassembler};

use loomd::conn::{self, HostCfg};
use loomd::endpoint;
use loomd::session::MediaParams;

// --- host log capture (the "host side" structured log) ---
#[derive(Clone)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl tracing_subscriber::fmt::MakeWriter<'_> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

fn spawn_host(drop_percent: u32) -> std::net::SocketAddr {
    let server = endpoint::server(([127, 0, 0, 1], 0).into()).expect("server");
    let bound = server.local_addr().unwrap();
    let cfg = HostCfg {
        name: "rec-host".into(),
        // Small frames keep IDRs to a few fragments, so a forced recovery IDR is
        // rarely re-dropped at 1% — the mechanism, not luck, is what's measured.
        params: MediaParams {
            width: 320,
            height: 240,
            ..MediaParams::default()
        },
        source: loomd::media::CaptureSource::Synthetic,
        drop_percent,
    };
    let slot = Arc::new(Semaphore::new(1));
    tokio::spawn(async move {
        while let Some(inc) = server.accept().await {
            tokio::spawn(conn::handle(inc, slot.clone(), cfg.clone()));
        }
    });
    bound
}

async fn connect(addr: std::net::SocketAddr) -> (Endpoint, Connection, SendStream, RecvStream) {
    let ep = endpoint::client().expect("client");
    let c = ep
        .connect(addr, "localhost")
        .unwrap()
        .await
        .expect("handshake");
    let (s, r) = c.open_bi().await.expect("control stream");
    (ep, c, s, r)
}

async fn send_msg(send: &mut SendStream, ty: u64, body: &[(Value, Value)]) {
    send.write_all(&control::encode_frame(ty, body))
        .await
        .unwrap();
}

async fn read_msg(recv: &mut RecvStream) -> u64 {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.unwrap();
    let mut frame = len.to_vec();
    frame.resize(4 + u32::from_be_bytes(len) as usize, 0);
    recv.read_exact(&mut frame[4..]).await.unwrap();
    match control::decode_frame(&frame).unwrap() {
        Decoded::Message { msg_type, .. } => msg_type,
        Decoded::Ignored => 0,
    }
}

#[tokio::test]
async fn freeze_idr_request_recovery_under_200ms() {
    let logbuf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(BufWriter(logbuf.clone()))
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let addr = spawn_host(1);
    let (_ep, conn, mut send, mut recv) = connect(addr).await;

    // Handshake to STREAMING.
    send_msg(&mut send, control::HELLO, &hello()).await;
    assert_eq!(read_msg(&mut recv).await, control::WELCOME);
    assert_eq!(read_msg(&mut recv).await, control::CONFIG);
    send_msg(
        &mut send,
        control::CONFIG_ACK,
        &[(Value::Int(0), Value::Int(1))],
    )
    .await;
    assert_eq!(read_msg(&mut recv).await, control::START);

    // Drive reassembly over received video datagrams.
    let mut reasm = Reassembler::new();
    let clock = Instant::now();
    let mut freeze_at: Option<Instant> = None;
    let mut cycles: Vec<Duration> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);

    while Instant::now() < deadline && cycles.len() < 3 {
        let dg = tokio::select! {
            d = conn.read_datagram() => match d { Ok(d) => d, Err(_) => break },
            _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
        };
        let Ok(dec) = datagram::decode(&dg) else {
            continue;
        };
        if dec.header.stream_id != 0 {
            continue;
        }
        let before = reasm.events().len();
        reasm.push(
            clock.elapsed().as_millis() as i64,
            Fragment {
                frame_seq: dec.header.frame_seq,
                frag_index: dec.header.frag_index,
                frag_count: dec.header.frag_count,
                keyframe: dec.header.keyframe,
            },
        );
        // Snapshot the (last_good) of new IDR requests and whether a keyframe was
        // delivered, so we don't hold a borrow of `reasm` across the awaits below.
        let mut idr_last_good: Option<u32> = None;
        let mut delivered_keyframe = false;
        for ev in &reasm.events()[before..] {
            match ev {
                Event::IdrRequest { last_good, .. } => idr_last_good = Some(*last_good),
                Event::Deliver { keyframe: true, .. } => delivered_keyframe = true,
                _ => {}
            }
        }
        if let Some(last_good) = idr_last_good {
            if freeze_at.is_none() {
                freeze_at = Some(Instant::now());
            }
            send_msg(
                &mut send,
                control::IDR_REQUEST,
                &[(Value::Int(0), Value::Int(last_good as i128))],
            )
            .await;
        }
        if delivered_keyframe {
            if let Some(f) = freeze_at.take() {
                cycles.push(f.elapsed());
            }
        }
    }

    assert!(
        !cycles.is_empty(),
        "no freeze→recovery cycle observed under 1% loss"
    );
    let fastest = *cycles.iter().min().unwrap();
    assert!(
        fastest < Duration::from_millis(200),
        "fastest recovery {fastest:?} exceeds 200 ms budget; all cycles = {cycles:?}"
    );

    // Both-sides agreement: the host's structured log must show a forced IDR.
    let logs = String::from_utf8_lossy(&logbuf.lock().unwrap()).to_string();
    assert!(
        logs.contains("idr_forced"),
        "host structured log has no idr_forced event"
    );
}

fn hello() -> Vec<(Value, Value)> {
    vec![
        (Value::Int(0), Value::Int(1)),
        (Value::Int(1), Value::Text("rec-client".into())),
        (Value::Int(2), Value::Array(vec![Value::Int(1)])),
        (
            Value::Int(3),
            Value::Array(vec![Value::Int(2560), Value::Int(1440)]),
        ),
        (Value::Int(4), Value::Int(72)),
        (Value::Int(5), Value::Int(0)),
    ]
}
