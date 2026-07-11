# loom-host — Code Navigation Map

Developer aid (informative), not a contract. Covers every `.rs` file in `loom-host`
(excludes `spec/` and `target/`). Points at the normative spec: **PROTOCOL.md**,
**ARCHITECTURE.md**, **PAIRING.md**, **VECTORS.md** — the spec wins on any disagreement.
The C++ twin of this repo's `loom-proto` is documented in `loom-client/CODE_MAP.md`.

**Reading the tables:** file names link to the file; `symbols` are the key types/functions
to jump to; the **§** column lists the spec sections a file implements.

## Layout at a glance

```
                       ┌─────────────────────────────────────────────┐
   wire bytes  <──────>│ loom-proto  (pure: no I/O, no async, no QUIC) │
                       │  cbor · datagram · control · reassembly ·     │
                       │  clocksync · keymap · errors                  │
                       └───────────────▲─────────────────────────────┘
                                       │ encode / decode only
                       ┌───────────────┴─────────────────────────────┐
   QUIC / sockets <───>│ loomd  (async daemon)                        │
                       │  endpoint → conn (I/O) → session (sans-io SM) │
                       │  tls (dev certs)                             │
                       └──────────────────────────────────────────────┘
   loom-encode ── libx265 HEVC (M1.2, real)
   loom-capture · loom-audio · loom-input · loom-vdisplay ·
   tools/latency-probe   ── stub crates, filled in later milestones
```

**The one rule that explains the layout:** all wire logic lives in `loom-proto`; `loomd`
never hand-rolls a header or a CBOR byte — it builds `Value` maps and calls `loom-proto`.

---

## `loom-proto` — the wire protocol (pure library)

No I/O, no async, no QUIC. Independent twin of the C++ `client/proto`, kept in lockstep by the
conformance vectors. `#![forbid(unsafe_code)]`.

| File | What it is | Key symbols | § |
|---|---|---|---|
| [`lib.rs`](loom-proto/src/lib.rs) | Crate root; module list + charter | `PROTOCOL_VERSION` | — |
| [`cbor.rs`](loom-proto/src/cbor.rs) | Canonical CBOR value model | `Value`, `to_canonical`, `decode` | 3.2 |
| [`datagram.rs`](loom-proto/src/datagram.rs) | Video/audio 12-byte header + fragmentation | `DatagramHeader`, `decode`→`DropReason`, `fragment`, `MAX_PAYLOAD` | 4 |
| [`control.rs`](loom-proto/src/control.rs) | Control framing + message registry | `encode_frame`, `decode_frame`, `Decoded`, `known_keys` | 3 |
| [`reassembly.rs`](loom-proto/src/reassembly.rs) | Client receive model (loss recovery) | `Reassembler::push`, `Event`, `Counters` | 6, 3.6 |
| [`clocksync.rs`](loom-proto/src/clocksync.rs) | Clock offset/RTT min-filter | `ClockFilter::push`, `Estimate` | 7 |
| [`keymap.rs`](loom-proto/src/keymap.rs) | evdev/AKEYCODE/CGKeyCode CSV parse | `Keymap::from_csv`, `get` | 3.5 |
| [`error.rs`](loom-proto/src/error.rs) | Library error type | `Error`, `Result` | — |
| [`errors.rs`](loom-proto/src/errors.rs) | Protocol error *codes* ‹M1.1› | `NONE`…`INTERNAL`, `name` | 10 |
| [`bin/vector-adapter.rs`](loom-proto/src/bin/vector-adapter.rs) | Conformance adapter — the crate's only I/O | `main` (dispatch by category) | VECTORS 2/3 |

**Notes**
- `datagram::decode` returns `DropReason`, never an `Error` — datagram failures are *silent drops* (§6.6).
- `reassembly` and `clocksync` are client-role logic; they live here so `loom-proto` is a complete impl for the vectors. `loomd` uses only the encode/decode parts today.

---

## `loom-encode` — software HEVC encode (M1.2)

Safe wrapper over the libx265 C API. Pure *mechanism*: the §5 knobs are passed in, so policy
stays in `loomd`. Needs Homebrew libx265 (pkg-config); FFI is generated at build time.

| File | What it is | Key symbols | § |
|---|---|---|---|
| [`src/lib.rs`](loom-encode/src/lib.rs) | Safe HEVC encoder | `HevcEncoder::{new,encode_i420}`, `EncoderConfig`, `AccessUnit` | 5, 4.1 |
| [`src/ffi.rs`](loom-encode/src/ffi.rs) | bindgen libx265 FFI (lints off) | `include!(x265_bindings.rs)` | — |
| [`build.rs`](loom-encode/build.rs) · [`src/x265_shim.c`](loom-encode/src/x265_shim.c) | pkg-config + bindgen + shim for the version-macro'd open | `loom_x265_encoder_open` | — |

## `loomd` — the host daemon (lib + bin)

Tokio + quinn. `endpoint` builds it, `conn` moves bytes, `session` makes every protocol decision,
`media` runs the synthetic video pipeline, `tls` supplies dev credentials.

| File | What it is | Key symbols | § |
|---|---|---|---|
| [`lib.rs`](loomd/src/lib.rs) | Library root; module layering + `BoxErr` | — | — |
| [`session.rs`](loomd/src/session.rs) | **Sans-io session state machine** (pure) | `HostSession::on_frame`, `Output`, `State`, `MediaParams` | 1.1, 3.4, 5 |
| [`conn.rs`](loomd/src/conn.rs) | Per-connection async driver (transport I/O) | `handle`, `run_session`, `reject_busy`, `HostCfg` | 1.1, 3.1 |
| [`media/mod.rs`](loomd/src/media/mod.rs) | Media thread: pattern→encode→fragment→datagram | `spawn`, `MediaHandle`, `DropInjector` | 4, 5 |
| [`media/testpattern.rs`](loomd/src/media/testpattern.rs) | Synthetic source (gradient + counter + parity border) | `TestPattern::{render,planes}` | — |
| [`media/constraints.rs`](loomd/src/media/constraints.rs) | **The one §5 constants home** (DRY) | `encoder_config`, `BFRAMES`, `MAX_REF`, … | 5 |
| [`endpoint.rs`](loomd/src/endpoint.rs) | quinn endpoint build + accept loop | `server`, `client`, `accept_loop` | 2, 5 |
| [`tls.rs`](loomd/src/tls.rs) | Dev self-signed certs; **verification skipped** (TODO M7) | `insecure_server_config`, `AcceptAnyClient`, `ALPN` | 2 |
| [`main.rs`](loomd/src/main.rs) | CLI binary + JSON tracing init | `Args` (`--port/--name/--insecure-dev/--drop-percent`) | — |
| [`tests/handshake.rs`](loomd/tests/handshake.rs) | Loopback handshake / version / BUSY | `full_handshake_completes`, `second_client_gets_busy` | M1.1 |
| [`tests/bitstream_conformance.rs`](loomd/tests/bitstream_conformance.rs) | §5 checks via ffprobe + NAL census | `no_b_frames_and_idr_only…`, `every_idr_carries…` | M1.2 |
| [`tests/recovery.rs`](loomd/tests/recovery.rs) | freeze→IDR_REQUEST→recovery < 200 ms | `freeze_idr_request_recovery_under_200ms` | M1.2 |

### Inside `session.rs` — the state machine

| Trigger | Guard | Result |
|---|---|---|
| HELLO | version = 1 **and** offers HEVC | send WELCOME + CONFIG → `WaitConfigAck` |
| HELLO | version ≠ 1 | ERROR `VERSION_UNSUPPORTED` + close |
| HELLO | no common codec | ERROR `NO_COMMON_CODEC` + close |
| CONFIG_ACK | generation matches | send START + `StartMedia` → `Streaming` |
| IDR_REQUEST | while `Streaming` | `RequestIdr` (driver forces an encoder IDR) |
| STATS / INPUT | while `Streaming` | tolerated no-op (M1.3 / M4) |
| BYE | any state | clean close (`NONE`) |
| CLOCK_PING | any state | tolerated no-op (PONG is TODO M1.3) |
| anything else | wrong for state | ERROR `PROTOCOL_VIOLATION` + close |

### Inside `conn.rs` — the I/O glue

| Function | Role |
|---|---|
| [`handle`](loomd/src/conn.rs) | Accept connection; 1-permit semaphore gates the single session; surplus → `reject_busy` |
| [`run_session`](loomd/src/conn.rs) | Accept control stream; loop `read_frame` → `decode_frame` → `on_frame` → `drive` |
| [`drive`](loomd/src/conn.rs) | Apply `Output`s: `Send`→write, `StartMedia`→spawn media thread, `RequestIdr`→forward, `Close`→close |
| [`reject_busy`](loomd/src/conn.rs) | ERROR `BUSY` on the control stream, then close with BUSY (§10) |
| [`read_frame`](loomd/src/conn.rs) | Length-prefixed framing over `quinn::RecvStream` |

---

## Stub crates (skeletons for later milestones)

Each is a ~5-line `lib.rs` (`#![forbid(unsafe_code)]`) present so the workspace matches
ARCHITECTURE §3. Nothing to implement yet.

| File | Becomes | Milestone | § |
|---|---|---|---|
| [`loom-capture/src/lib.rs`](loom-capture/src/lib.rs) | Capture trait + PipeWire / ScreenCaptureKit | M1.4 / M2.1 / M6 | 5.1–5.2 |
| [`loom-audio/src/lib.rs`](loom-audio/src/lib.rs) | Capture + Opus encode | M5 | 9 |
| [`loom-input/src/lib.rs`](loom-input/src/lib.rs) | Injection (portal / CGEvent) + keymap | M4 | 5, PROTO 3.5 |
| [`loom-vdisplay/src/lib.rs`](loom-vdisplay/src/lib.rs) | Virtual display (EVDI / CGVirtualDisplay) | M6 | 5.1–5.2 |
| [`tools/latency-probe/src/main.rs`](tools/latency-probe/src/main.rs) | Click-to-photon measurement rig | M1 | 12 |

---

## Where to start, by task

| I want to… | Read, in order |
|---|---|
| Understand the wire format | [`lib.rs`](loom-proto/src/lib.rs) → [`control.rs`](loom-proto/src/control.rs) → [`datagram.rs`](loom-proto/src/datagram.rs) → [`cbor.rs`](loom-proto/src/cbor.rs) |
| Understand a session | [`session.rs`](loomd/src/session.rs) (decisions) → [`conn.rs`](loomd/src/conn.rs) (byte flow) |
| Trace the media path | [`media/testpattern.rs`](loomd/src/media/testpattern.rs) → [`loom-encode`](loom-encode/src/lib.rs) → `datagram::fragment` → [`media/mod.rs`](loomd/src/media/mod.rs) |
| Add/inspect a control message | [`control.rs`](loom-proto/src/control.rs) registry → build its body in [`session.rs`](loomd/src/session.rs) |
| Trace loss recovery | [`reassembly.rs`](loom-proto/src/reassembly.rs) rules 1–3 |
| Run the conformance suite | `./check.sh` (drives `vector-adapter` against `spec/vectors/`) |
