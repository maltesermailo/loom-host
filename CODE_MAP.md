# loom-host — Code Navigation Map

**Status:** Informative developer aid, not a contract. Regenerate/extend as files land.
**Scope:** every `.rs` file in `loom-host` (excludes `spec/` submodule and `target/`).
**Normative sources it points at:** `spec/PROTOCOL.md`, `spec/ARCHITECTURE.md`,
`spec/PAIRING.md`, `spec/VECTORS.md`. When this map and the spec disagree, the spec wins.

## How to use this

Each entry is: **path** — one-line purpose, then the key items to jump to and the
spec sections it implements. "Start here" markers flag the natural reading order.
`file.rs:NN` references are clickable in most editors.

## The 30-second mental model

```
                       ┌─────────────────────────────────────────────┐
   wire bytes  <──────>│ loom-proto  (pure: no I/O, no async, no QUIC) │
                       │  cbor · datagram · control · reassembly ·     │
                       │  clocksync · keymap · errors                  │
                       └───────────────▲─────────────────────────────┘
                                       │ used by (encode/decode only)
                       ┌───────────────┴─────────────────────────────┐
   QUIC / sockets <───>│ loomd  (async daemon)                        │
                       │  endpoint → conn (I/O) → session (sans-io SM) │
                       │  tls (dev certs)                             │
                       └──────────────────────────────────────────────┘
   loom-capture / loom-encode / loom-audio / loom-input /
   loom-vdisplay / tools/latency-probe  ── stub crates, later milestones
```

The one rule that explains the layout: **all wire logic lives in `loom-proto`**;
`loomd` may not hand-roll a header or a CBOR byte. `loomd` builds `loom_proto::cbor::Value`
maps and calls `loom_proto` to turn them into bytes.

---

# Crate: `loom-proto` — the wire protocol (pure library)

No I/O, no async, no QUIC types. Both this crate and the C++ `client/proto` are
*independent* implementations of the same contract, kept in agreement byte-for-byte by
the conformance vectors (`spec/vectors/`, run via `bin/vector-adapter`). `#![forbid(unsafe_code)]`.

### `loom-proto/src/lib.rs` — crate root ★ start here
- Module declarations + crate docs stating the "no I/O / no async / no QUIC" charter.
- `PROTOCOL_VERSION: u64 = 1` (`lib.rs:26`) — HELLO/WELCOME key 0 (§3.4).
- Re-exports `Error`, `Result`.

### `loom-proto/src/cbor.rs` — canonical CBOR value model (§3.2)
- `enum Value` (`cbor.rs:20`) — the CBOR shapes the control protocol uses; integers held
  as `i128` so full `u64` and negative ranges fit one variant.
- Accessors `as_int` / `as_map` / `as_array` (`cbor.rs:41-63`).
- `Value::to_canonical() -> Vec<u8>` (`cbor.rs:65`) — hand-rolled canonical encoder
  (definite lengths, shortest-form ints, sorted keys, shortest-form floats: `2.5 → f9 4100`).
- `decode(bytes) -> Result<Value>` (`cbor.rs:175`) — delegates to `ciborium`, lowers into `Value`.
- Internal encoders: `encode_head` / `encode_int` / `encode_map` / `encode_float` (`cbor.rs:97-150`).

### `loom-proto/src/datagram.rs` — video/audio datagram header (§4)
- Constants: `MAGIC=0x4C`, `FLAG_KEYFRAME`, `FLAG_LAST_FRAGMENT`, `HEADER_LEN=12`,
  `MAX_DATAGRAM_LEN=1350` (`datagram.rs:14-22`).
- `struct DatagramHeader` (`datagram.rs:26`) — keyframe, last_fragment, stream_id, frame_seq,
  frag_index, frag_count; `new()` derives `last_fragment` from position, `to_bytes()`/`encode()`.
- `enum DropReason` (`datagram.rs:92`) + `as_str()` — the stable reason strings the vectors assert.
- `struct DecodedDatagram` + `decode(bytes)` (`datagram.rs:126-135`) — validation in the exact
  normative order (length → magic → frag range → last-fragment → stream_id). Failures are
  **silent drops** in production (§6.6), so this returns `DropReason`, never an `Error`.

### `loom-proto/src/control.rs` — control-stream framing + message registry (§3)
- `MAX_FRAME_BODY=65536` (`control.rs:14`).
- Message-type constants `HELLO`…`PAIR_RESULT` (`control.rs:18-48`) — the §3.3 registry.
- `known_keys(msg_type)` (`control.rs:53`) — the body-map keys defined per type; unknown keys are
  stripped, unknown types ignored (§3.2 forward-compat).
- `enum Decoded { Message{msg_type, body}, Ignored }` (`control.rs:77`).
- `encode_frame(msg_type, body) -> Vec<u8>` (`control.rs:91`) — canonical `[msg_type, body]`
  with a big-endian `u32` length prefix. **This is the only place `loomd` gets frame bytes.**
- `decode_frame(bytes) -> Result<Decoded>` (`control.rs:105`) — framing/envelope violations →
  `Error::ProtocolViolation`.

### `loom-proto/src/reassembly.rs` — client receive model (§6 + §3.6)
- `struct Fragment` (`reassembly.rs:21`) — the header subset that drives reassembly.
- `enum Event { Deliver{…}, IdrRequest{…} }` (`reassembly.rs:34`) — decoder deliveries and
  IDR-requests, in occurrence order.
- `struct Counters` (`reassembly.rs:55`) — `dropped_incomplete`, `discarded_gap`, `stale_fragments`.
- `struct Reassembler` (`reassembly.rs:71`) + `push(t_ms, frag)` (`reassembly.rs:119`) — the state
  machine: **Rule 1** staleness (`:122`), **Rule 2** ≤2 incomplete frames (`:129`),
  **Rule 3** decode gating + IDR trigger (`:174`). `events()`/`counters()` expose results.
- *Used by the client, not `loomd`* — present here so `loom-proto` is a complete impl for the vectors.

### `loom-proto/src/clocksync.rs` — clock offset/RTT min-filter (§7)
- `WINDOW=16` (`clocksync.rs:15`).
- `struct Estimate { rtt, offset }` (`clocksync.rs:19`).
- `struct ClockFilter` + `push(t0,t1,t2,t3) -> Estimate` (`clocksync.rs:28-42`) — computes rtt/offset
  (floor division), keeps the **minimum-rtt** sample over the sliding window (ties → most recent).
- *Wired live in M1.3* (STEP 3); the math is done, the transport hookup is not.

### `loom-proto/src/keymap.rs` — evdev/AKEYCODE/CGKeyCode tables (§3.5)
- `struct Keymap` (`keymap.rs:20`) — one-directional integer lookup parsed from `keymaps/*.csv`.
- `from_csv(text)` (`keymap.rs:26`) — parses `from,to` rows (`#` comments/blank lines ignored);
  **parses only, no file I/O** (caller reads the file) to keep the crate I/O-free.
- `get(code)` / `len` / `is_empty` (`keymap.rs:58-68`).

### `loom-proto/src/error.rs` — library error type
- `enum Error { ProtocolViolation, Keymap(String) }` + `type Result<T>`.
- Note the deliberate split documented at the top: **datagram** failures use `DropReason`
  (silent drops), only control/keymap paths surface an `Error`.

### `loom-proto/src/errors.rs` — protocol error *codes* (§10)  ‹added M1.1›
- Constants `NONE`…`INTERNAL` (`errors.rs`) — used both as ERROR (0x40) body codes and as QUIC
  application close codes. `name(code)` maps to the stable string (unknown → "INTERNAL").
- The single DRY home for these wire constants; `loomd` and the state machine import them.

### `loom-proto/src/bin/vector-adapter.rs` — conformance adapter (VECTORS.md §2/§3)
- The **only** I/O in the crate. `vector-adapter <category>` reads a vector's JSON on stdin,
  runs each `op` against the library, prints `{"results":[…]}` on stdout.
- `main()` (`:21`) dispatches by category (`:43`): datagram/control/reassembly/clocksync/keymap.
- Per-op helpers: `datagram_encode/decode`, `control_encode/decode`, `reassembly_trace`,
  `clocksync_series`, `keymap_lookup`, plus `json_to_cbor` / `find_keymaps_dir`.
- Driven by `spec/vector-check`; run by `./check.sh` step 3.

---

# Crate: `loomd` — the host daemon (async binary + library)

Tokio + quinn. Split as **lib + bin** so the runtime is integration-testable. Layering:
`endpoint` (construction) → `conn` (transport I/O) → `session` (pure decisions); `tls` supplies
dev credentials. All protocol decisions live in `session`; everything else is glue.

### `loomd/src/lib.rs` — library root ★ start here
- Declares `pub mod conn, endpoint, session, tls` and documents the M1.1 layering.
- `type BoxErr = Box<dyn Error + Send + Sync>` — the daemon's fallible-setup alias.

### `loomd/src/session.rs` — sans-io host session state machine ★ the heart (§1.1, §3.4, §5)
- Pure, no async/QUIC — unit-tested against out-of-order orderings.
- `struct MediaParams` (+ `Default` = 2560×1440@72, HEVC, 60 Mbps) — what CONFIG advertises.
- `enum Output { Send{msg_type,body}, StartMedia, Close{code} }` — instructions for the driver.
- `enum State { WaitHello, WaitConfigAck, Streaming, Closed }`.
- `struct HostSession` + `on_frame(Decoded) -> Vec<Output>` — the transition function:
  `on_hello` (version + codec checks → WELCOME+CONFIG), `on_config_ack` (generation match → START),
  BYE→clean close, CLOCK_PING tolerated (PONG is TODO M1.3), else `protocol_violation`/`fatal`.
- Builders `welcome_body` / `config_body` construct `Value` maps (never raw bytes).
- `#[cfg(test)]` — 10 unit tests: happy path, wrong version, no-codec, ordering violations, BYE, etc.

### `loomd/src/conn.rs` — per-connection async driver (§1.1, §3.1)
- `struct HostCfg { name, params }` — per-daemon settings handed to each connection.
- `handle(incoming, slot, cfg)` — accept the connection; a 1-permit `Semaphore` (`slot`) enforces
  one session at a time; the surplus connection goes to `reject_busy`.
- `run_session` — accept the control stream, build `HostSession`, loop: `read_frame` →
  `control::decode_frame` → `session.on_frame` → `drive`.
- `drive(outputs)` — apply `Output`s: `Send`→`send_frame`, `StartMedia`→log (M1.2 hook),
  `Close`→`connection.close(code)`.
- `reject_busy` — ERROR(BUSY) on the control stream + close with BUSY (§10).
- `read_frame` — length-prefixed framing over `quinn::RecvStream`; `gen_session_id` (wall-clock,
  non-secret, WELCOME key 2).

### `loomd/src/endpoint.rs` — quinn endpoint + accept loop (ARCHITECTURE §5, PROTOCOL §2)
- `transport()` — keep-alive 5 s / idle timeout 15 s (§2).
- `server(addr)` — bound server endpoint (dev cert, ALPN loom/1).
- `client()` — trust-any client endpoint (**tests only**; production clients use msquic).
- `accept_loop(endpoint, cfg)` — spawn one `conn::handle` task per connection, sharing the semaphore.

### `loomd/src/tls.rs` — dev TLS, **verification skipped** (TODO M7)
- `ALPN = b"loom/1"`.
- `DevIdentity::generate()` — throwaway `localhost` self-signed cert via rcgen.
- `insecure_server_config()` / `insecure_client_config()` — TLS 1.3 quinn configs pinned to ALPN.
- `AcceptAnyServer` / `AcceptAnyClient` — rustls verifiers that accept anything;
  `AcceptAnyClient::client_auth_mandatory()==false` so the msquic client needs no cert.
  **Security hole by construction**, gated behind `--insecure-dev`; replace with pinning in M7.

### `loomd/src/main.rs` — CLI binary
- `struct Args` (clap): `--port` (default 47800), `--name` (WELCOME), `--insecure-dev`.
- `main()` — refuses to serve without `--insecure-dev` (no peer auth exists yet), else builds the
  endpoint and runs `accept_loop`.

### `loomd/tests/handshake.rs` — in-process loopback integration tests (M1.1 accept)
- `spawn_host()` — server endpoint + accept loop on an ephemeral loopback port, under the real semaphore.
- Helpers: `connect`, `send_msg`, `read_msg`, `close_code`.
- Tests: `full_handshake_completes`; `wrong_version_hello_gets_version_unsupported` (close code 0x01);
  `second_client_gets_busy` (close code 0x02). Fatal errors are asserted via the QUIC close code
  (authoritative per §10); the cross-repo live handshake lives in the STEP 4 demo.

---

# Stub crates (skeletons for later milestones)

Each is a 5-line `lib.rs` with `#![forbid(unsafe_code)]`, present so the workspace layout matches
ARCHITECTURE §3 and the wire types stay in the build graph. Nothing to implement here yet.

| File | Becomes | Milestone | Spec |
|---|---|---|---|
| `loom-capture/src/lib.rs` | screen capture trait + PipeWire/ScreenCaptureKit backends | M1.4 / M2.1 / M6 | §5.1–§5.2 |
| `loom-encode/src/lib.rs` | encode trait + NVENC/VideoToolbox backends | M1.5 / M2.2 | §5, PROTOCOL §5 |
| `loom-audio/src/lib.rs` | capture + Opus encode | M5 | §9 |
| `loom-input/src/lib.rs` | injection (portal RemoteDesktop / CGEvent) + keymap | M4 | §5, PROTOCOL §3.5 |
| `loom-vdisplay/src/lib.rs` | virtual display (EVDI / CGVirtualDisplay) | M6 | §5.1–§5.2 |
| `tools/latency-probe/src/main.rs` | click-to-photon measurement rig | M1 | §12 |

---

## Where to start reading, by task

- **Understand the wire format** → `loom-proto/src/lib.rs` → `control.rs` → `datagram.rs` → `cbor.rs`.
- **Understand a session** → `loomd/src/session.rs` (decisions) then `conn.rs` (how bytes flow).
- **Add/inspect a control message** → `control.rs` registry + `known_keys`, then build its body in `session.rs`.
- **Trace loss recovery (client model)** → `reassembly.rs` rules 1–3.
- **Run the conformance suite** → `./check.sh` (drives `bin/vector-adapter` against `spec/vectors/`).
