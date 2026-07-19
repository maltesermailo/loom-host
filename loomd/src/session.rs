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

/// Host encode/capture ceiling for a VIEWPORT request (§3.10). A requested size
/// is clamped to this and to the client's HELLO max before reconfiguring. 4K is
/// generous headroom above the v1 quality bar; the client's own decode max is
/// usually the tighter bound.
const HOST_MAX_WIDTH: u64 = 3840;
const HOST_MAX_HEIGHT: u64 = 2160;
/// Floor for a clamped VIEWPORT dimension, so a degenerate request can't ask for
/// a 0- or few-pixel stream. Dimensions are also forced even (4:2:0 encoders).
const MIN_DIM: u64 = 640;

/// Media parameters the host advertises in CONFIG (§3.4). Hardcoded to the v1
/// quality bar for now; a TOML config (ARCHITECTURE §5.3) is not required until
/// later milestones, so KISS — no config surface the ROADMAP doesn't ask for.
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// HELLO key 5 feature bit 1: the client can fan in concurrent video streams
/// (PROTOCOL §3.4 multi-display). Bit 0 (audio) is orthogonal and unused in v1.
const FEATURE_MULTI_DISPLAY: u64 = 0b10;

/// One additional video stream the host offers beyond the primary (stream_id 0)
/// — the protocol view of a display, carried in CONFIG key 6 (§3.4). The capture
/// side (which physical/virtual display feeds it) is a host concern kept out of
/// this sans-io state machine; see `conn::StreamSpec`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamConfig {
    /// Datagram stream_id (≥ 2, unique) this display streams on.
    pub stream_id: u16,
    /// Per-stream media parameters (its own native size, refresh, bitrate).
    pub params: MediaParams,
    /// This display's top-left position in the host's global layout (main display
    /// at the origin), so the client can place its panel to match (§3.4 key 4).
    pub x: i32,
    /// This display's top position in the host's global layout.
    pub y: i32,
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
    /// `multi` is true when multi-display was negotiated (§3.4): the driver spawns
    /// one pipeline per configured stream instead of only the primary.
    StartMedia {
        /// Whether the extra streams (CONFIG key 6) are active this session.
        multi: bool,
    },
    /// The client asked for a fresh IDR on a video stream (§3.6). The driver
    /// forwards this to that stream's encoder, which codes the next frame as an
    /// IDR. `stream_id` is IDR_REQUEST key 1 (default 0 = primary).
    RequestIdr {
        /// The video stream to refresh (0 = primary display).
        stream_id: u16,
    },
    /// A VIEWPORT-driven resolution change was ACKed (§8): reconfigure the
    /// running media thread to `params`. The next encoded frame is an IDR with
    /// the new parameter sets; `frame_seq` continues.
    Reconfigure {
        /// The new media parameters (already clamped, §3.10).
        params: MediaParams,
    },
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
    /// The video stream_id these counters describe (§3.7 key 7, default 0). With
    /// multiple streams the client sends one STATS per stream.
    pub stream_id: u16,
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
            stream_id: int_key(body, 7).unwrap_or(0) as u16,
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
    /// Client's max decodable size (HELLO key 3), the ceiling for a VIEWPORT
    /// clamp (§3.10). Captured on HELLO; defaults conservatively until then.
    client_max_width: u64,
    client_max_height: u64,
    /// The generation of a mid-session reconfiguration whose CONFIG_ACK we are
    /// still awaiting (§8). While set, media continues at the old resolution and
    /// further VIEWPORT requests are ignored.
    pending_generation: Option<u64>,
    /// Additional displays the host is configured to serve beyond the primary
    /// (stream_id 0). Advertised in CONFIG key 6 only once [`Self::multi_active`].
    extra_streams: Vec<StreamConfig>,
    /// Whether multi-display was negotiated: the client offered HELLO key 5 bit 1
    /// **and** the host has extra streams to serve. Decided at HELLO and fixed for
    /// the session. When false the host is single-stream, bit-exact with a peer
    /// that predates the feature.
    multi_active: bool,
}

impl HostSession {
    /// A fresh session awaiting HELLO. `session_id` is UI/log-only (§3.4).
    /// `extra_streams` are the additional displays this host would serve if the
    /// client negotiates multi-display; empty for a single-display host.
    pub fn new(
        host_name: impl Into<String>,
        session_id: [u8; 16],
        params: MediaParams,
        extra_streams: Vec<StreamConfig>,
    ) -> Self {
        Self {
            state: State::WaitHello,
            host_name: host_name.into(),
            session_id,
            params,
            generation: 0,
            client_max_width: params.width,
            client_max_height: params.height,
            pending_generation: None,
            extra_streams,
            multi_active: false,
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
            control::IDR_REQUEST => {
                // Key 1 (default 0) selects the video stream to refresh (§3.6).
                let stream_id = int_key(body, 1).unwrap_or(0) as u16;
                vec![Output::RequestIdr { stream_id }]
            }
            control::STATS => vec![Output::Stats(StatsReport::from_body(body))],
            control::VIEWPORT => self.on_viewport(body),
            // A CONFIG_ACK here closes the mid-session reconfiguration gate (§8).
            control::CONFIG_ACK => self.on_reconfig_ack(body),
            // INPUT is a valid streaming message we do not act on yet (M4).
            control::INPUT => Vec::new(),
            _ => self.protocol_violation(),
        }
    }

    /// VIEWPORT (§3.10): a best-effort request to stream at the client's window
    /// size. Clamp it to host + client caps; if it names a genuinely new size and
    /// no reconfiguration is already in flight, bump the generation and send the
    /// new CONFIG (§8). Media keeps flowing at the old size until the ACK.
    fn on_viewport(&mut self, body: &[(Value, Value)]) -> Vec<Output> {
        let (rw, rh) = match (pair_elem(body, 0, 0), pair_elem(body, 0, 1)) {
            (Some(w), Some(h)) => (w as u64, h as u64),
            // Malformed request: it is best-effort, so ignore rather than error.
            _ => return Vec::new(),
        };

        let (w, h) = self.clamp_viewport(rw, rh);
        if self.pending_generation.is_some() || (w == self.params.width && h == self.params.height)
        {
            return Vec::new();
        }

        self.params.width = w;
        self.params.height = h;
        self.generation += 1;
        self.pending_generation = Some(self.generation);

        vec![Output::Send {
            msg_type: control::CONFIG,
            body: self.config_body(),
        }]
    }

    /// CONFIG_ACK while streaming (§8): completes a pending reconfiguration. Only
    /// the pending generation is valid; anything else is a violation.
    fn on_reconfig_ack(&mut self, body: &[(Value, Value)]) -> Vec<Output> {
        match (int_key(body, 0), self.pending_generation) {
            (Some(g), Some(pending)) if g == pending as i128 => {
                self.pending_generation = None;
                vec![Output::Reconfigure {
                    params: self.params,
                }]
            }
            _ => self.protocol_violation(),
        }
    }

    /// Clamp a requested VIEWPORT size to the host ceiling and the client's HELLO
    /// max, flooring at [`MIN_DIM`] and forcing even dimensions for 4:2:0 (§3.10).
    fn clamp_viewport(&self, w: u64, h: u64) -> (u64, u64) {
        let max_w = HOST_MAX_WIDTH.min(self.client_max_width);
        let max_h = HOST_MAX_HEIGHT.min(self.client_max_height);
        let cw = w.clamp(MIN_DIM, max_w.max(MIN_DIM)) & !1;
        let ch = h.clamp(MIN_DIM, max_h.max(MIN_DIM)) & !1;
        (cw, ch)
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

        // Key 3: client's max decodable [width, height] — the ceiling a later
        // VIEWPORT request (§3.10) is clamped to. Absent → keep the defaults.
        if let (Some(w), Some(h)) = (pair_elem(body, 3, 0), pair_elem(body, 3, 1)) {
            self.client_max_width = w as u64;
            self.client_max_height = h as u64;
        }

        // Key 5: feature bitmask. Multi-display activates only if the client can
        // fan in concurrent streams *and* the host has extra displays to serve;
        // otherwise the session stays single-stream (§3.4, §12).
        let client_features = int_key(body, 5).unwrap_or(0) as u64;
        let client_multi = client_features & FEATURE_MULTI_DISPLAY != 0;
        self.multi_active = client_multi && !self.extra_streams.is_empty();

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
            Output::StartMedia {
                multi: self.multi_active,
            },
        ]
    }

    fn welcome_body(&self) -> Vec<(Value, Value)> {
        let mut body = vec![
            (Value::Int(0), Value::Int(PROTOCOL_VERSION as i128)),
            (Value::Int(1), Value::Text(self.host_name.clone())),
            (Value::Int(2), Value::Bytes(self.session_id.to_vec())),
        ];
        // Key 3: active feature bitmask (§3.4). Emitted only when a feature is
        // active, so a single-display session's WELCOME is byte-identical to before.
        if self.multi_active {
            body.push((Value::Int(3), Value::Int(FEATURE_MULTI_DISPLAY as i128)));
        }
        body
    }

    fn config_body(&self) -> Vec<(Value, Value)> {
        let p = self.params;
        let mut body = vec![
            (Value::Int(0), Value::Int(self.generation as i128)),
            (Value::Int(1), Value::Int(p.codec as i128)),
            (
                Value::Int(2),
                Value::Array(vec![
                    Value::Int(p.width as i128),
                    Value::Int(p.height as i128),
                ]),
            ),
            (Value::Int(3), Value::Int(p.refresh as i128)),
            (Value::Int(4), Value::Int(p.audio as i128)),
            (Value::Int(5), Value::Int(p.bitrate_kbps as i128)),
        ];
        // Key 6: additional video streams (§3.4), present only when multi-display
        // is active. Keys 1–5 above describe the primary stream (stream_id 0).
        if self.multi_active {
            let extras = self
                .extra_streams
                .iter()
                .map(|s| {
                    Value::Map(vec![
                        (Value::Int(0), Value::Int(s.stream_id as i128)),
                        (
                            Value::Int(1),
                            Value::Array(vec![
                                Value::Int(s.params.width as i128),
                                Value::Int(s.params.height as i128),
                            ]),
                        ),
                        (Value::Int(2), Value::Int(s.params.refresh as i128)),
                        (Value::Int(3), Value::Int(s.params.bitrate_kbps as i128)),
                        // Key 4: display position in the host's global layout (§3.4).
                        (
                            Value::Int(4),
                            Value::Array(vec![Value::Int(s.x as i128), Value::Int(s.y as i128)]),
                        ),
                    ])
                })
                .collect();
            body.push((Value::Int(6), Value::Array(extras)));
        }
        body
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

/// Element `idx` of a 2-int array body value (e.g. CONFIG key 2 / VIEWPORT key 0
/// = [width, height]).
fn pair_elem(body: &[(Value, Value)], key: i128, idx: usize) -> Option<i128> {
    body.iter().find_map(|(k, v)| match (k, v) {
        (Value::Int(ki), Value::Array(items)) if *ki == key => match items.get(idx) {
            Some(Value::Int(vi)) => Some(*vi),
            _ => None,
        },
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
        (Value::Int(2), Value::Array(items)) => items
            .iter()
            .any(|c| matches!(c, Value::Int(i) if *i == codec as i128)),
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
        HostSession::new("test-host", [0u8; 16], MediaParams::default(), Vec::new())
    }

    /// A session configured with one extra display (stream_id 2, 1920×1080) — the
    /// host side of the owner's two-monitor setup.
    fn multi_new_session() -> HostSession {
        let extra = StreamConfig {
            stream_id: 2,
            params: MediaParams {
                width: 1920,
                height: 1080,
                ..MediaParams::default()
            },
            x: -1920,
            y: 0,
        };
        HostSession::new("test-host", [0u8; 16], MediaParams::default(), vec![extra])
    }

    /// HELLO advertising multi-display (key 5 bit 1) plus a decode max.
    fn hello_multi() -> Decoded {
        Decoded::Message {
            msg_type: control::HELLO,
            body: vec![
                (Value::Int(0), Value::Int(1)),
                (Value::Int(1), Value::Text("Quest 3".into())),
                (Value::Int(2), Value::Array(vec![Value::Int(1)])),
                (
                    Value::Int(3),
                    Value::Array(vec![Value::Int(2560), Value::Int(1440)]),
                ),
                (Value::Int(5), Value::Int(0b11)), // audio | multi-display
            ],
        }
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
        assert!(out.contains(&Output::StartMedia { multi: false }));
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn multi_display_negotiated_advertises_config_key6() {
        let mut s = multi_new_session();
        let out = s.on_frame(hello_multi());
        assert_eq!(sent_types(&out), vec![control::WELCOME, control::CONFIG]);

        // WELCOME key 3 = active features (bit 1 = multi-display).
        let welcome = out
            .iter()
            .find_map(|o| match o {
                Output::Send { msg_type, body } if *msg_type == control::WELCOME => Some(body),
                _ => None,
            })
            .unwrap();
        assert_eq!(int_key(welcome, 3), Some(0b10));

        // CONFIG key 6 = [{0: 2, 1: [1920,1080], 2: 72, 3: 60000}].
        let config = out
            .iter()
            .find_map(|o| match o {
                Output::Send { msg_type, body } if *msg_type == control::CONFIG => Some(body),
                _ => None,
            })
            .unwrap();
        match config
            .iter()
            .find(|(k, _)| *k == Value::Int(6))
            .map(|(_, v)| v)
        {
            Some(Value::Array(streams)) => {
                assert_eq!(streams.len(), 1);
                match &streams[0] {
                    Value::Map(desc) => {
                        assert_eq!(int_key(desc, 0), Some(2)); // stream_id
                        assert_eq!(pair_elem(desc, 1, 0), Some(1920));
                        assert_eq!(pair_elem(desc, 1, 1), Some(1080));
                    }
                    other => panic!("expected a stream descriptor map, got {other:?}"),
                }
            }
            other => panic!("expected CONFIG key 6 array, got {other:?}"),
        }

        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert!(out.contains(&Output::StartMedia { multi: true }));
    }

    #[test]
    fn multi_display_inactive_when_client_lacks_feature() {
        // Host has an extra stream, but a plain HELLO (no key 5 bit 1) → single.
        let mut s = multi_new_session();
        let out = s.on_frame(hello(1, vec![1]));
        let config = out
            .iter()
            .find_map(|o| match o {
                Output::Send { msg_type, body } if *msg_type == control::CONFIG => Some(body),
                _ => None,
            })
            .unwrap();
        assert!(config.iter().all(|(k, _)| *k != Value::Int(6)));
        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert!(out.contains(&Output::StartMedia { multi: false }));
    }

    #[test]
    fn multi_display_inactive_when_host_single() {
        // Client offers multi-display, but a single-display host has no extras.
        let mut s = new_session();
        let out = s.on_frame(hello_multi());
        let welcome = out
            .iter()
            .find_map(|o| match o {
                Output::Send { msg_type, body } if *msg_type == control::WELCOME => Some(body),
                _ => None,
            })
            .unwrap();
        assert_eq!(int_key(welcome, 3), None); // no feature echo
    }

    #[test]
    fn idr_request_targets_named_stream() {
        let mut s = new_session();
        s.on_frame(hello(1, vec![1]));
        s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        let out = s.on_frame(msg(
            control::IDR_REQUEST,
            vec![(0, Value::Int(0)), (1, Value::Int(2))],
        ));
        assert_eq!(out, vec![Output::RequestIdr { stream_id: 2 }]);
    }

    #[test]
    fn wrong_version_hello_gets_version_unsupported() {
        let mut s = new_session();
        let out = s.on_frame(hello(2, vec![1]));
        assert!(
            matches!(out.last(), Some(Output::Close { code }) if *code == errors::VERSION_UNSUPPORTED)
        );
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
        assert_eq!(out, vec![Output::RequestIdr { stream_id: 0 }]);
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
        assert!(s
            .on_frame(msg(control::INPUT, vec![(0, Value::Int(0))]))
            .is_empty());
        assert_eq!(s.state(), State::Streaming);
    }

    /// Drive a session to Streaming, advertising a client HELLO max of 2560×1440.
    fn streaming_session() -> HostSession {
        let mut s = new_session();
        s.on_frame(Decoded::Message {
            msg_type: control::HELLO,
            body: vec![
                (Value::Int(0), Value::Int(1)),
                (Value::Int(1), Value::Text("test-client".into())),
                (Value::Int(2), Value::Array(vec![Value::Int(1)])),
                (
                    Value::Int(3),
                    Value::Array(vec![Value::Int(2560), Value::Int(1440)]),
                ),
            ],
        });
        s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(1))]));
        assert_eq!(s.state(), State::Streaming);
        s
    }

    fn viewport(w: i128, h: i128) -> Decoded {
        msg(
            control::VIEWPORT,
            vec![(0, Value::Array(vec![Value::Int(w), Value::Int(h)]))],
        )
    }

    #[test]
    fn viewport_reconfigures_via_config_then_reconfigure() {
        let mut s = streaming_session();

        // A smaller window: within caps, so it is honored verbatim.
        let out = s.on_frame(viewport(1920, 1080));
        match out.as_slice() {
            [Output::Send { msg_type, body }] if *msg_type == control::CONFIG => {
                assert_eq!(int_key(body, 0), Some(2)); // generation bumped to 2
                assert_eq!(pair_elem(body, 2, 0), Some(1920));
                assert_eq!(pair_elem(body, 2, 1), Some(1080));
            }
            other => panic!("expected a CONFIG send, got {other:?}"),
        }
        assert_eq!(s.state(), State::Streaming); // media keeps flowing (§8)

        // The client ACKs the new generation → reconfigure the media thread.
        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(2))]));
        match out.as_slice() {
            [Output::Reconfigure { params }] => {
                assert_eq!(params.width, 1920);
                assert_eq!(params.height, 1080);
            }
            other => panic!("expected Reconfigure, got {other:?}"),
        }
    }

    #[test]
    fn viewport_is_clamped_to_client_hello_max() {
        let mut s = streaming_session();
        // Move off the 2560×1440 default first so the clamp target differs from
        // the current size (an equal size would be a no-op).
        s.on_frame(viewport(1920, 1080));
        s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(2))]));

        // Request beyond the client's 2560×1440 decode max → clamped down to it.
        let out = s.on_frame(viewport(7680, 4320));
        match out.as_slice() {
            [Output::Send { body, .. }] => {
                assert_eq!(pair_elem(body, 2, 0), Some(2560));
                assert_eq!(pair_elem(body, 2, 1), Some(1440));
            }
            other => panic!("expected a clamped CONFIG, got {other:?}"),
        }
    }

    #[test]
    fn viewport_matching_current_size_is_a_noop() {
        let mut s = streaming_session();
        // Default config is already 2560×1440.
        let out = s.on_frame(viewport(2560, 1440));
        assert!(out.is_empty());
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn second_viewport_while_reconfig_pending_is_ignored() {
        let mut s = streaming_session();
        assert!(!s.on_frame(viewport(1920, 1080)).is_empty());
        // A second request before the ACK is dropped (gate held, §8).
        assert!(s.on_frame(viewport(1280, 720)).is_empty());
    }

    #[test]
    fn malformed_viewport_is_ignored_not_fatal() {
        let mut s = streaming_session();
        // Missing the [w, h] array: best-effort request, so silently ignored.
        let out = s.on_frame(msg(control::VIEWPORT, vec![(0, Value::Int(5))]));
        assert!(out.is_empty());
        assert_eq!(s.state(), State::Streaming);
    }

    #[test]
    fn unexpected_config_ack_while_streaming_is_violation() {
        let mut s = streaming_session();
        // No reconfiguration pending → a CONFIG_ACK is a protocol violation.
        let out = s.on_frame(msg(control::CONFIG_ACK, vec![(0, Value::Int(2))]));
        assert_error_code(&out, errors::PROTOCOL_VIOLATION);
        assert_eq!(s.state(), State::Closed);
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
