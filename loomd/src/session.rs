//! Host session state machine — PROTOCOL.md §1.1, §3.4, §5.
//!
//! This is a **sans-io** state machine: it takes decoded control frames in and
//! produces [`Output`]s out. It owns no sockets, spawns no tasks, and never
//! touches QUIC — exactly so it can be unit-tested against out-of-order message
//! orderings without a network (spec/ROADMAP.md M1.1 accept). The async driver
//! in [`crate::conn`] moves bytes; all *protocol* decisions live here.
//!
//! Wire encode/decode is delegated to `loom_proto` — this module only assembles
//! `loom_proto::cbor::Value` bodies and reads decoded ones; it never emits a
//! CBOR byte or a frame header itself (the DRY rule).
//!
//! ```text
//! WaitHello --HELLO--> (WELCOME, CONFIG) --> WaitConfigAck
//!   WaitConfigAck --CONFIG_ACK(gen)--> (START) --> Streaming
//!   any --BYE--> Closed (clean)
//!   any invalid ordering / bad frame --> ERROR(PROTOCOL_VIOLATION) + close
//!   HELLO wrong version --> ERROR(VERSION_UNSUPPORTED) + close
//!   HELLO no common codec --> ERROR(NO_COMMON_CODEC) + close
//! ```

use loom_proto::cbor::Value;
use loom_proto::control::{self, Decoded};
use loom_proto::{errors, PROTOCOL_VERSION};

/// The one codec `loomd` can encode in v1 (PROTOCOL.md §3.4 key 2: 1 = HEVC).
const CODEC_HEVC: u64 = 1;

/// Media parameters the host advertises in CONFIG (§3.4). Hardcoded to the v1
/// quality bar for now; a TOML config (ARCHITECTURE §5.3) is not required until
/// later milestones, so KISS — no config surface the ROADMAP doesn't ask for.
#[derive(Clone, Copy, Debug)]
pub struct MediaParams {
    /// Chosen video codec (1 = HEVC).
    pub codec: u64,
    /// Frame width in pixels.
    pub width: u64,
    /// Frame height in pixels.
    pub height: u64,
    /// Refresh rate in Hz.
    pub refresh: u64,
    /// Audio mode (0 = disabled, 1 = Opus). Audio lands in M5; 0 for now.
    pub audio: u64,
    /// Initial video bitrate in kbit/s (informative, §3.4 key 5).
    pub bitrate_kbps: u64,
}

impl Default for MediaParams {
    fn default() -> Self {
        // ARCHITECTURE §2: 2560×1440 @ 72 Hz, HEVC, 60 Mbps default.
        Self {
            codec: CODEC_HEVC,
            width: 2560,
            height: 1440,
            refresh: 72,
            audio: 0,
            bitrate_kbps: 60_000,
        }
    }
}

/// An instruction from the state machine to its async driver, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Encode and send this control message on the control stream.
    Send {
        /// Message type (§3.3).
        msg_type: u64,
        /// Canonical body entries (unknown keys already absent).
        body: Vec<(Value, Value)>,
    },
    /// START has been emitted; media may begin. Wired to the encoder in M1.2.
    StartMedia,
    /// The client asked for a fresh IDR (§3.6). The driver forwards this to the
    /// running encoder, which codes the next frame as an IDR.
    RequestIdr,
    /// Reply to a CLOCK_PING (§7): echo `t0`; the driver stamps host receive/send
    /// times from the shared clock (the SM stays clock-free / sans-io).
    ClockPong {
        /// The client send timestamp to echo (CLOCK_PING key 0).
        t0: i64,
    },
    /// A client STATS report (§3.7) for the host-side log (AIMD is M7.4).
    Stats(StatsReport),
    /// Flush pending sends, then close the QUIC connection with `code`
    /// (`errors::NONE` for a clean BYE-driven close).
    Close {
        /// Application close code (PROTOCOL.md §10).
        code: u64,
    },
}

/// A parsed client STATS report (§3.7). `e2e_us` is absent until the client has
/// its first clock sample.
#[derive(Clone, Debug, PartialEq)]
pub struct StatsReport {
    /// Video frames fully received.
    pub frames_received: i64,
    /// Video frames dropped (any fragment lost or stale).
    pub frames_dropped: i64,
    /// Datagrams received.
    pub datagrams: i64,
    /// Inter-arrival jitter estimate, ms.
    pub jitter_ms: f64,
    /// Mean decode time, µs.
    pub decode_us: i64,
    /// Current RTT estimate, µs.
    pub rtt_us: i64,
    /// Mean end-to-end video latency, µs (omitted before the first clock sample).
    pub e2e_us: Option<i64>,
}

impl StatsReport {
    fn from_body(body: &[(Value, Value)]) -> Self {
        Self {
            frames_received: int_key(body, 0).unwrap_or(0) as i64,
            frames_dropped: int_key(body, 1).unwrap_or(0) as i64,
            datagrams: int_key(body, 2).unwrap_or(0) as i64,
            jitter_ms: float_key(body, 3).unwrap_or(0.0),
            decode_us: int_key(body, 4).unwrap_or(0) as i64,
            rtt_us: int_key(body, 5).unwrap_or(0) as i64,
            e2e_us: int_key(body, 6).map(|v| v as i64),
        }
    }
}

/// Session phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Awaiting the client's HELLO (MUST be the first message).
    WaitHello,
    /// WELCOME+CONFIG sent; awaiting CONFIG_ACK for `generation`.
    WaitConfigAck,
    /// START sent; media flows.
    Streaming,
    /// Terminal — no further frames are processed.
    Closed,
}

/// The host side of one session.
pub struct HostSession {
    state: State,
    host_name: String,
    session_id: [u8; 16],
    params: MediaParams,
    /// CONFIG generation, starts at 1, increments per CONFIG (§3.4 key 0).
    generation: u64,
}

impl HostSession {
    /// A fresh session awaiting HELLO. `session_id` is UI/log-only (§3.4).
    pub fn new(host_name: impl Into<String>, session_id: [u8; 16], params: MediaParams) -> Self {
        Self {
            state: State::WaitHello,
            host_name: host_name.into(),
            session_id,
            params,
            generation: 0,
        }
    }

    /// Current phase.
    pub fn state(&self) -> State {
        self.state
    }

    /// Feed one decoded control frame. Returns the outputs it triggers, in
    /// order. `Decoded::Ignored` (unknown `msg_type`, §3.2) is a no-op.
    pub fn on_frame(&mut self, decoded: Decoded) -> Vec<Output> {
        let (msg_type, body) = match decoded {
            Decoded::Message { msg_type, body } => (msg_type, body),
            Decoded::Ignored => return Vec::new(),
        };

        if self.state == State::Closed {
            return Vec::new();
        }

        // BYE (§3.9) is a clean close in any phase and is never an error.
        if msg_type == control::BYE {
            self.state = State::Closed;
            return vec![Output::Close { code: errors::NONE }];
        }
        // CLOCK_PING is valid in any phase (§3.8): echo t0 in a CLOCK_PONG.
        if msg_type == control::CLOCK_PING {
            let t0 = int_key(&body, 0).unwrap_or(0) as i64;
            return vec![Output::ClockPong { t0 }];
        }

        match self.state {
            State::WaitHello => self.on_hello(msg_type, &body),
            State::WaitConfigAck => self.on_config_ack(msg_type, &body),
            State::Streaming => self.on_streaming(msg_type, &body),
            State::Closed => Vec::new(),
        }
    }

    /// Streaming-phase client→host messages (§3.3).
    fn on_streaming(&mut self, msg_type: u64, body: &[(Value, Value)]) -> Vec<Output> {
        match msg_type {
            control::IDR_REQUEST => vec![Output::RequestIdr],
            control::STATS => vec![Output::Stats(StatsReport::from_body(body))],
            // INPUT is a valid streaming message we do not act on yet (M4).
            control::INPUT => Vec::new(),
            _ => self.protocol_violation(),
        }
    }

    /// HELLO handling (§3.4). Anything else here is a violation.
    fn on_hello(&mut self, msg_type: u64, body: &[(Value, Value)]) -> Vec<Output> {
        if msg_type != control::HELLO {
            return self.protocol_violation();
        }
        // Key 0: protocol_version. Absent or non-1 → VERSION_UNSUPPORTED.
        match int_key(body, 0) {
            Some(v) if v == PROTOCOL_VERSION as i128 => {}
            _ => return self.fatal(errors::VERSION_UNSUPPORTED, "unsupported protocol_version"),
        }
        // Key 2: preference-ordered codec list. Host encodes HEVC only in v1.
        if !offers_codec(body, CODEC_HEVC) {
            return self.fatal(errors::NO_COMMON_CODEC, "no common codec (host: HEVC)");
        }

        self.generation = 1;
        self.state = State::WaitConfigAck;
        vec![
            Output::Send {
                msg_type: control::WELCOME,
                body: self.welcome_body(),
            },
            Output::Send {
                msg_type: control::CONFIG,
                body: self.config_body(),
            },
        ]
    }

    /// CONFIG_ACK handling (§3.4). Must carry the current generation.
    fn on_config_ack(&mut self, msg_type: u64, body: &[(Value, Value)]) -> Vec<Output> {
        if msg_type != control::CONFIG_ACK {
            return self.protocol_violation();
        }
        match int_key(body, 0) {
            Some(g) if g == self.generation as i128 => {}
            _ => return self.protocol_violation(),
        }
        self.state = State::Streaming;
        vec![
            Output::Send {
                msg_type: control::START,
                body: Vec::new(),
            },
            Output::StartMedia,
        ]
    }

    fn welcome_body(&self) -> Vec<(Value, Value)> {
        vec![
            (Value::Int(0), Value::Int(PROTOCOL_VERSION as i128)),
            (Value::Int(1), Value::Text(self.host_name.clone())),
            (Value::Int(2), Value::Bytes(self.session_id.to_vec())),
        ]
    }

    fn config_body(&self) -> Vec<(Value, Value)> {
        let p = self.params;
        vec![
            (Value::Int(0), Value::Int(self.generation as i128)),
            (Value::Int(1), Value::Int(p.codec as i128)),
            (
                Value::Int(2),
                Value::Array(vec![Value::Int(p.width as i128), Value::Int(p.height as i128)]),
            ),
            (Value::Int(3), Value::Int(p.refresh as i128)),
            (Value::Int(4), Value::Int(p.audio as i128)),
            (Value::Int(5), Value::Int(p.bitrate_kbps as i128)),
        ]
    }

    /// Emit ERROR + close for a framing/ordering violation (§10 0x04).
    fn protocol_violation(&mut self) -> Vec<Output> {
        self.fatal(errors::PROTOCOL_VIOLATION, "unexpected message for state")
    }

    /// Emit ERROR(code) followed by a matching connection close, and go Closed.
    fn fatal(&mut self, code: u64, detail: &str) -> Vec<Output> {
        self.state = State::Closed;
        vec![
            Output::Send {
                msg_type: control::ERROR,
                body: vec![
                    (Value::Int(0), Value::Int(code as i128)),
                    (Value::Int(1), Value::Text(detail.to_string())),
                ],
            },
            Output::Close { code },
        ]
    }
}

/// Look up an integer body value by key.
fn int_key(body: &[(Value, Value)], key: i128) -> Option<i128> {
    body.iter().find_map(|(k, v)| match (k, v) {
        (Value::Int(ki), Value::Int(vi)) if *ki == key => Some(*vi),
        _ => None,
    })
}

/// Look up a floating-point body value by key (STATS jitter, §3.7 key 3).
fn float_key(body: &[(Value, Value)], key: i128) -> Option<f64> {
    body.iter().find_map(|(k, v)| match (k, v) {
        (Value::Int(ki), Value::Float(f)) if *ki == key => Some(*f),
        _ => None,
    })
}

/// Whether HELLO key 2 (codec list) contains `codec`.
fn offers_codec(body: &[(Value, Value)], codec: u64) -> bool {
    body.iter().any(|(k, v)| match (k, v) {
        (Value::Int(2), Value::Array(items)) => {
            items.iter().any(|c| matches!(c, Value::Int(i) if *i == codec as i128))
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(version: i128, codecs: Vec<i128>) -> Decoded {
        Decoded::Message {
            msg_type: control::HELLO,
            body: vec![
                (Value::Int(0), Value::Int(version)),
                (Value::Int(1), Value::Text("test-client".into())),
                (
                    Value::Int(2),
                    Value::Array(codecs.into_iter().map(Value::Int).collect()),
                ),
            ],
        }
    }

    fn msg(msg_type: u64, body: Vec<(i128, Value)>) -> Decoded {
        Decoded::Message {
            msg_type,
            body: body.into_iter().map(|(k, v)| (Value::Int(k), v)).collect(),
        }
    }

    fn new_session() -> HostSession {
        HostSession::new("test-host", [0u8; 16], MediaParams::default())
    }

    fn sent_types(out: &[Output]) -> Vec<u64> {
        out.iter()
            .filter_map(|o| match o {
                Output::Send { msg_type, .. } => Some(*msg_type),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_happy_path() {
        let mut s = new_session();
        let out = s.on_frame(hello(1, vec![1]));
        assert_eq!(sent_types(&out), vec![control::WELCOME, control::CONFIG]);
        assert_eq!(s.state(), State::WaitConfigAck);

        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert_eq!(sent_types(&out), vec![control::START]);
        assert!(out.contains(&Output::StartMedia));
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn wrong_version_hello_gets_version_unsupported() {
        let mut s = new_session();
        let out = s.on_frame(hello(2, vec![1]));
        assert!(matches!(out.last(), Some(Output::Close { code }) if *code == errors::VERSION_UNSUPPORTED));
        assert_error_code(&out, errors::VERSION_UNSUPPORTED);
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn hello_without_hevc_gets_no_common_codec() {
        let mut s = new_session();
        // Client offers only AV1 (2); host encodes HEVC only.
        let out = s.on_frame(hello(1, vec![2]));
        assert_error_code(&out, errors::NO_COMMON_CODEC);
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn config_ack_before_hello_is_violation() {
        let mut s = new_session();
        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert_error_code(&out, errors::PROTOCOL_VIOLATION);
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn second_hello_is_violation() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        let out = s.on_frame(hello(1, vec![1]));
        assert_error_code(&out, errors::PROTOCOL_VIOLATION);
    }

    #[test]
    fn config_ack_wrong_generation_is_violation() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(7))]));
        assert_error_code(&out, errors::PROTOCOL_VIOLATION);
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn bye_closes_cleanly_from_any_state() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        let out = s.on_frame(msg(control::BYE, vec![(0, Value::Int(0))]));
        assert_eq!(out, vec![Output::Close { code: errors::NONE }]);
        assert_eq!(s.state(), State::Closed);
    }

    #[test]
    fn idr_request_while_streaming_asks_encoder() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert_eq!(s.state(), State::Streaming);
        let out = s.on_frame(msg(control::IDR_REQUEST, vec![(0, Value::Int(0))]));
        assert_eq!(out, vec![Output::RequestIdr]);
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn stats_logged_input_tolerated_while_streaming() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        let out = s.on_frame(msg(
            control::STATS,
            vec![(0, Value::Int(42)), (5, Value::Int(3000))],
        ));
        match out.as_slice() {
            [Output::Stats(r)] => {
                assert_eq!(r.frames_received, 42);
                assert_eq!(r.rtt_us, 3000);
                assert_eq!(r.e2e_us, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(s.on_frame(msg(control::INPUT, vec![(0, Value::Int(0))])).is_empty());
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn clock_ping_replies_pong_during_setup() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        let out = s.on_frame(msg(control::CLOCK_PING, vec![(0, Value::Int(123))]));
        assert_eq!(out, vec![Output::ClockPong { t0: 123 }]);
        assert_eq!(s.state(), State::WaitConfigAck);
    }

    #[test]
    fn unknown_type_is_ignored() {
        let mut s = new_session();
        let out = s.on_frame(Decoded::Ignored);
        assert!(out.is_empty());
        assert_eq!(s.state(), State::WaitHello);
    }

    #[test]
    fn frames_after_close_are_dropped() {
        let mut s = new_session();
        s.on_frame(msg(control::BYE, vec![(0, Value::Int(0))]));
        let out = s.on_frame(hello(1, vec![1]));
        assert!(out.is_empty());
    }

    fn assert_error_code(out: &[Output], code: u64) {
        let err = out.iter().find_map(|o| match o {
            Output::Send { msg_type, body } if *msg_type == control::ERROR => Some(body),
            _ => None,
        });
        let body = err.expect("an ERROR message");
        assert_eq!(int_key(body, 0), Some(code as i128));
    }
}
