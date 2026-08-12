# Session FSM — design

Status: approved 2026-06-02. Implementation tracked under au ticket **iax-c333**;
C-parity validation under **iax-64f0**.

This document is the design for the outgoing-call session state machine in
`astar-iax-core`. It does not redefine the wire protocol (see
[RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456) and
`docs/spec/iax2-conformance.md`) or the AllStar dialect quirks (see
`docs/spec/allstar-dialect.md`); it covers the in-process structure that
implements them.

## Goals

- Reach `Active` state against a real Asterisk peer running with either
  `requirecalltoken=yes` or `=no`.
- Be testable without an event loop, a UDP socket, or wall-clock time.
- Be runtime-agnostic: the same FSM can be driven by `mio` (the planned
  driver for `iaxclient`), a blocking thread, or a deterministic simulator.
  Note: an earlier draft named tokio as the planned driver; that was struck
  on 2026-06-02 in favor of `mio` + a blocking thread per call. IAX2 is
  low-rate and the C reference is single-threaded; an async runtime adds
  weight without buying concurrency we need.
- Property-test all transitions against arbitrary `(state, event)` pairs; no
  pair may panic.

## Non-goals (for the iax-c333 landing)

- PING/PONG keepalive — owned by iax-a307.
- DTMF / PTT-over-IAX / TEXT in-call event semantics — separate tickets layer
  on top by extending `AppCommand` and `AppEvent`.
- Incoming calls (peer-initiated NEW) — owned by iax-8baf.
- Plaintext and RSA auth — interfaces stubbed; MD5 is the only working method.
- A high-level public API — owned by iax-612e.

## Architecture

Three layers, all inside `astar-iax-core`:

```
┌──────────────────────────────────────────────────┐
│  iaxclient (high-level crate, planned: mio)      │
│  ┌────────────────────────────────────────────┐  │
│  │ Runtime: blocking thread per call          │  │
│  │   - owns UdpSocket (via mio::net), mpsc    │  │
│  │   - poll() loop: socket recv / timer wheel │  │
│  │     / api channel                          │  │
│  │   - drives Reliability + Fsm by calling    │  │
│  │     handle() and dispatching Actions       │  │
│  └────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────┘
                       │  (sync function calls)
                       ▼
┌──────────────────────────────────────────────────┐
│  astar-iax-core::session (pure logic, no I/O)    │
│  ┌────────────────────────────────────────────┐  │
│  │ Reliability                                │  │
│  │   - tracks unacked OSeqnos                 │  │
│  │   - emits retransmit-now / give-up         │  │
│  │   - consumes incoming ACKs (and VNAK)      │  │
│  │   - allocates OSeqno / ISeqno              │  │
│  └────────────────────────────────────────────┘  │
│  ┌────────────────────────────────────────────┐  │
│  │ Fsm (the call FSM proper)                  │  │
│  │   fn handle(&mut self, Event) ->           │  │
│  │     SmallVec<[Action; 4]>                  │  │
│  │   - state enum + match                     │  │
│  │   - never touches I/O, time, or threads    │  │
│  └────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────┘
```

Key properties:

- `Fsm::handle` is sync, deterministic, allocation-light. The returned vec
  is a `SmallVec<[Action; 4]>` — zero heap allocation for the common 0–3
  action case.
- Tests construct a `Fsm`, feed `Event`s, assert on returned state +
  Actions. No event loop in unit tests.
- Reliability sits between FSM and the runtime so that
  `Action::SendReliable(frame)` from the FSM gets ACK-tracking added by
  Reliability before the runtime hands it to the socket.
- The runtime layer (in `iaxclient`) is the only place I/O happens.

## State

```rust
pub enum SessionState {
    Init,

    /// NEW sent with empty CALLTOKEN; awaiting AUTHREQ or CALLTOKEN reply.
    NewSent(NewSentData),

    /// Server demanded CALLTOKEN; we cached it; about to resend NEW.
    /// Transient — usually we resend immediately on entry.
    CallTokenReceived(CallTokenReceivedData),

    /// NEW resent with populated CALLTOKEN; awaiting AUTHREQ.
    /// capabilities and ping_seq carry through from NewSent — the structure
    /// makes it impossible to re-roll them on resend (astar quirk #2).
    NewResent(NewResentData),

    /// Server sent AUTHREQ; we have the challenge; about to send AUTHREP.
    /// Not on the outbound happy-path dispatch — AUTHREP is sent directly
    /// from the NewSent/NewResent AUTHREQ handler (see transition table).
    AuthReqReceived(AuthReqReceivedData),

    /// AUTHREP sent; awaiting ACCEPT (or REJECT).
    AuthRepSent(AuthRepSentData),

    /// In a call. Voice + control frames flow in both directions.
    Active(ActiveData),

    /// HANGUP sent (by us) or received (from peer); awaiting final ACK.
    Hangup(HangupData),

    /// Terminal — successful teardown.
    Closed,

    /// Terminal — failed.
    Failed(FailReason),

    // Inbound (callee) handshake states (iax-8baf).
    NewReceived(NewReceivedData),
    CallTokenIssued(CallTokenIssuedData),
    AuthReqSent(AuthReqSentData),
    AcceptSent(AcceptSentData),
    AnswerSent(AnswerSentData),
}

pub struct NewSentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub attempts: u8,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    /// Dialled extension. Preserved so NEW retransmits re-use the same called-number IE.
    pub dest: String,
}

pub struct CallTokenReceivedData {
    pub token: CallToken,
    pub received_at: Instant,
    pub our_call: CallNo,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    pub dest: String,
}

pub struct NewResentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub token: CallToken,
    pub attempts: u8,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    pub dest: String,
}

pub struct AuthReqReceivedData {
    pub challenge: Vec<u8>,
    pub methods: AuthMethods,
    pub our_call: CallNo,
    pub peer_call: CallNo,
}

pub struct AuthRepSentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub attempts: u8,
    /// Cached so AuthRepRetry can re-send AUTHREP without re-receiving the challenge.
    pub challenge: Vec<u8>,
}

pub struct ActiveData {
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub established_at: Instant,
    /// Set on inbound DtmfBegin; cleared on inbound DtmfEnd (receive-only).
    pub pending_dtmf: Option<char>,
    /// Last outbound SendDtmf instant. Drives the 50 ms per-call rate limit.
    pub last_dtmf_at: Option<Instant>,
    /// Liveness / RTT bookkeeping (iax-a307). Driven by TimerKind::Keepalive and inbound frames.
    pub keepalive: KeepaliveState,
    /// Codec + timestamp of the last outbound FULL voice frame. Mini frames
    /// inherit the high 16 ts bits + codec from here; None until first voice sent. (iax-a116)
    pub last_full_voice: Option<(VoiceFormat, u32)>,
    /// Last PTT state we emitted a RADIO_KEY/UNKEY for; None until first SendPtt. (iax-d4e9)
    pub last_ptt: Option<bool>,
}

pub struct HangupData {
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub initiated_by: HangupOrigin,
    pub sent_at: Instant,
    pub attempts: u8,
}

pub enum FailReason {
    Rejected { cause: Option<String> },
    Timeout { in_state: &'static str },
    Aborted,
    /// Peer sent a HANGUP frame. `cause` carries the CAUSE IE text when present.
    /// Distinct from `Aborted`, which is reserved for locally-initiated teardowns.
    RemoteHangup { cause: Option<String> },
    /// Peer answered with INVAL — it has no state for this call (e.g. it restarted).
    /// Hard-fail; no automatic re-establish (RFC 5456 §6.9.2).
    PeerInval,
    InvalidTransition { from: &'static str, on: &'static str },
}

pub enum HangupOrigin { Local, Peer }
```

The split between `NewSent` and `NewResent` satisfies the
"haven't received token yet vs no token coming" requirement
(`docs/spec/allstar-dialect.md` §3): timeouts in each state map to distinct
`FailReason::Timeout { in_state }` values.

## Transition table

| From | Event | To | Actions emitted |
|---|---|---|---|
| Init | `App(StartCall)` | NewSent | `SendReliable(NEW + empty CALLTOKEN)`, `SetTimer(NewRetry, 1s)` |
| NewSent | `Frame(CallToken(t))` | NewResent | `SendReliable(NEW + t)`, `SetTimer(TokenExpiry, 10s)`, `SetTimer(NewRetry, 1s)` |
| NewSent | `Frame(AuthReq)` | AuthRepSent | `CancelTimer(NewRetry)`, `SetPeerCall`, `SendReliable(AUTHREP+md5)`, `SetTimer(AuthRepRetry, 1s)` |
| NewSent | `Frame(Accept)` | Active | `CancelTimer(NewRetry)`, `SetPeerCall`, `AppEvent(Connected)`, `SetTimer(Keepalive, interval)` |
| NewSent | `Frame(Reject)` | Failed | `CancelTimer(NewRetry)`, `AppEvent(Disconnected(Rejected{cause}))` |
| NewSent | `Timer(NewRetry)`, attempts<5 | NewSent | `SendReliable(NEW + empty)`, `SetTimer(NewRetry, backoff)` |
| NewSent | `Timer(NewRetry)`, attempts≥5 | Failed | `AppEvent(Disconnected(Timeout{"NewSent"}))` |
| NewResent | `Frame(AuthReq)` | AuthRepSent | `CancelTimer(NewRetry)`, `CancelTimer(TokenExpiry)`, `SetPeerCall`, `SendReliable(AUTHREP+md5)`, `SetTimer(AuthRepRetry, 1s)` |
| NewResent | `Timer(TokenExpiry)` | Failed | `AppEvent(Disconnected(Timeout{"NewResent"}))` |
| NewResent | `Timer(NewRetry)`, attempts<5 | NewResent | `SendReliable(NEW + t)`, `SetTimer(NewRetry, backoff)` |
| NewResent | `Timer(NewRetry)`, attempts≥5 | Failed | `AppEvent(Disconnected(Timeout{"NewResent"}))` |
| AuthReqReceived | (any) | AuthReqReceived | `LogInvalid` — state exists but has no outbound handler; AUTHREP is emitted from NewSent/NewResent |
| AuthRepSent | `Frame(Accept)` | Active | `CancelTimer(AuthRepRetry)`, `AppEvent(Connected{peer_call})` |
| AuthRepSent | `Frame(Reject(c))` | Failed | `AppEvent(Disconnected(Rejected{cause:c}))` |
| AuthRepSent | `Timer(AuthRepRetry)`, attempts<5 | AuthRepSent | `SendReliable(AUTHREP)`, `SetTimer(AuthRepRetry, backoff)` |
| Active | `Frame(Voice/Mini)` | Active | `AppEvent(VoiceReceived{...})` |
| Active | `Frame(Control(RadioKey/Unkey))` | Active | `AppEvent(RemotePtt(...))` |
| Active | `Frame(Text)` | Active | `AppEvent(TextReceived(...))` |
| Active | `Frame(Dtmf)` | Active | `AppEvent(DtmfReceived(...))` |
| Active | `Frame(Control(Hangup))` or `Frame(Iax(Hangup))` | Hangup{initiated_by:Peer} | `CancelTimer(Keepalive)`, `SetTimer(HangupRetry, 1s)`, `AppEvent(Disconnected(RemoteHangup{cause}))` |
| Active | `Frame(Text("!!DISCONNECT!!"))` | Hangup{initiated_by:Peer} | `CancelTimer(Keepalive)`, `SetTimer(HangupRetry, 1s)`, `AppEvent(Disconnected(RemoteHangup{cause:"disconnect"}))` |
| Active | `App(Hangup)` | Hangup{initiated_by:Local} | `SendReliable(HANGUP)`, `SetTimer(HangupRetry, 1s)` |
| Active | `App(SendVoice/Dtmf/Ptt/Text)` | Active | `SendUnreliable(...)` or `SendReliable(...)` per frame type |
| Hangup | `Frame(Ack)` | Closed | `CancelTimer(HangupRetry)`, `AppEvent(Disconnected)` if local-initiated |
| Hangup | `Timer(HangupRetry)`, attempts<3 | Hangup | `SendReliable(HANGUP)`, `SetTimer(HangupRetry, backoff)` |
| Hangup | `Timer(HangupRetry)`, attempts≥3 | Closed | best-effort end |
| any | unexpected event | unchanged | `LogInvalid { reason }` |

The catch-all "unexpected event" arm is how the acceptance criterion
"invalid state transitions produce errors, not panics" is satisfied: never
panic, always log, leave state unchanged.

## Event and Action language

```rust
pub enum Event<'a> {
    App(AppCommand),
    Frame(Frame<'a>),       // already passed Reliability dedup + ACK consumption
    Timer(TimerKind),
    DeliveryFailed { oseqno: u8 },
}

pub enum AppCommand {
    StartCall { dest: String, now: Instant },
    Hangup { cause: Option<String>, now: Instant },
    SendVoice { format: VoiceFormat, payload: Vec<u8>, ts: u32 },
    SendDtmf { digit: char, now: Instant },
    SendPtt(bool),                  // true=key, false=unkey
    SendText(String),
    // Inbound (callee) seams (iax-8baf).
    DriveInbound { now: Instant },
    AcceptIncoming { now: Instant },
    AnswerIncoming { now: Instant },
    RejectIncoming { cause: Option<String>, now: Instant },
    AnswerAcked { now: Instant },
}

pub enum TimerKind {
    NewRetry,
    HangupRetry,
    AuthRepRetry,
    TokenExpiry,
    Keepalive,      // PING/LAGRQ cadence + inbound-silence check while Active (iax-a307)
}

pub enum Action {
    SendReliable(OwnedFullFrame),   // through Reliability
    SendUnreliable(Vec<u8>),        // direct to socket (mini-frames)
    SetTimer(TimerKind, Duration),
    CancelTimer(TimerKind),
    AppEvent(AppEvent),
    /// Signal to the runtime that the peer's scallno is now known; the runtime
    /// calls `Reliability::set_peer_call` before the next enqueue (iax-e402).
    SetPeerCall(CallNo),
    /// Tell the runtime to reset Reliability sequence-number state for this leg.
    /// Emitted on the inbound CALLTOKEN path before AUTHREQ/ACCEPT (iax-8baf).
    ResetReliability,
    LogInvalid { reason: &'static str },
}

pub enum AppEvent {
    Connected { peer_call: CallNo },
    Disconnected { reason: FailReason },
    VoiceReceived { format: VoiceFormat, payload: Vec<u8>, ts: u32 },
    DtmfReceived(char),
    RemotePtt(bool),
    TextReceived(OwnedTextEvent),
    /// Keepalive: inbound silence exceeded the lost threshold. Edge-triggered —
    /// emitted once per loss episode; the call is NOT torn down (iax-a307).
    ConnectionLost,
    /// Keepalive: first inbound frame after a ConnectionLost (iax-a307).
    ConnectionRestored,
    /// An inbound NEW was demuxed and parsed; surface the offer to the app
    /// so it can accept/answer/reject (iax-8baf).
    IncomingCall {
        our_call: CallNo,
        peer_call: CallNo,
        calling_number: Option<String>,
        calling_name: Option<String>,
        called_number: Option<String>,
        username: Option<String>,
        offered_codecs: CodecMask,
        preferred_codec: Option<VoiceFormat>,
        language: Option<String>,
    },
}
```

Notes:

- `Frame<'a>` is the existing borrowed-bytes frame from `astar-iax-core::frame`.
- `AppEvent` carries owned data (`Vec<u8>`, `OwnedTextEvent`) so it can cross
  the runtime → app channel boundary without lifetime gymnastics.
- `SendUnreliable(Vec<u8>)` is pre-serialized bytes because mini-frame voice
  is the 50fps hot path; one Vec allocation per frame is the budget.
- `SetTimer` is idempotent by `TimerKind`: setting `NewRetry` twice replaces
  the deadline. Runtime is expected to honor this.
- `LogInvalid` is the only Action with no side effect outside observability.
  Useful in fuzz and proptest where unexpected events should generate them.

## Reliability

```rust
pub struct Reliability {
    our_call: CallNo,
    peer_call: Option<CallNo>,
    next_oseqno: u8,
    next_iseqno: u8,
    in_flight: HashMap<u8, InFlight>,
    config: ReliabilityConfig,
}

struct InFlight {
    frame: OwnedFullFrame,
    first_sent: Instant,
    last_sent: Instant,
    attempts: u8,
    next_retry_at: Instant,
}

pub struct ReliabilityConfig {
    pub initial_rto: Duration,  // 1s per RFC 5456
    pub max_rto: Duration,      // 4s
    pub max_attempts: u8,       // 5
    pub backoff: f32,           // 2.0
}

impl Reliability {
    pub fn enqueue(&mut self, frame: OwnedFullFrame, now: Instant) -> Vec<u8>;
    pub fn on_frame_in<'a>(&mut self, frame: Frame<'a>, now: Instant) -> RxOutcome<'a>;
    pub fn tick(&mut self, now: Instant) -> TickOutcome;
}

pub enum RxOutcome<'a> {
    Deliver { frame: Frame<'a>, send_ack: Option<Vec<u8>> },
    Consumed,                                        // pure ACK consumed internally
    Duplicate { resend_ack: Option<Vec<u8>> },
    Vnak(u8),                                        // peer NAKed; runtime resends
    GaveUp { oseqno: u8 },                           // permanent — FSM gets DeliveryFailed
}

pub struct TickOutcome {
    pub retransmit: Vec<Vec<u8>>,
    pub next_deadline: Option<Instant>,
    pub gave_up: Vec<u8>,
}
```

Three orthogonal responsibilities:

1. **OSeqno bookkeeping** — allocate, ACK-consume, retransmit, give up.
2. **ISeqno bookkeeping** — track next expected, dedupe, generate ACKs.
3. **Piggyback ACK handling** — every incoming full frame carries an ISeqno
   acking the peer's most recent OSeqno; update in-flight on every frame.

VNAK is *signalled* to the runtime (not auto-handled) so the runtime can
throttle or coalesce resends if it wants to.

ACK frames are emitted as pre-serialized bytes (`Vec<u8>`), not
`OwnedFullFrame`s, to keep the per-ACK allocation budget at one Vec.

## CALLTOKEN handshake

Four behaviors mandated by `docs/spec/allstar-dialect.md`:

1. **Always emit `IAX_IE_CALLTOKEN`**, even when empty. The first NEW carries
   the IE with empty value; the resent NEW carries the IE with the peer's
   token. Peers with `requirecalltoken=no` ignore the empty IE.
2. **Don't reinit ping/capabilities on resend.** Carrying `capabilities` and
   `ping_seq` *through the state variant* into `NewResent` makes it
   structurally impossible to re-roll them.
3. **10s token TTL.** `Action::SetTimer(TokenExpiry, 10s)` on entry to
   `NewResent`; firing the timer fails the call rather than reusing a stale
   token. Tokens are never cached across calls — `CallToken` lives inside
   the state variant, not at `Fsm` top level.
4. **Distinct "haven't received yet" vs "not coming" timeouts.**
   `NewSent` timeout and `NewResent` timeout produce
   `FailReason::Timeout { in_state }` values that differ, so the app can
   distinguish network failure from auth-after-token failure.

`CallToken` is modeled as `Vec<u8>` — opaque bytes. RFC 5456 §8.6 doesn't
define internal structure; we don't try to.

## Credentials

```rust
pub struct Credentials {
    pub username: String,
    pub password: Secret,           // zeroize on drop
    pub allowed_methods: AuthMethods,
}
```

Preloaded at `Fsm::new`. When the FSM transitions through `AuthReqReceived`
it computes `md5(challenge || password)` inline; no callback to the runtime.
Plaintext and RSA stubs return `Action::LogInvalid` for now.

## Public API surface

```rust
impl Fsm {
    pub fn new(credentials: Credentials, our_call: CallNo) -> Self;
    pub fn handle(&mut self, event: Event<'_>) -> SmallVec<[Action; 4]>;
    pub fn state(&self) -> &SessionState;
}

impl Reliability {
    pub fn new(our_call: CallNo, config: ReliabilityConfig) -> Self;
    pub fn enqueue(&mut self, frame: OwnedFullFrame, now: Instant) -> Vec<u8>;
    pub fn on_frame_in<'a>(&mut self, frame: Frame<'a>, now: Instant) -> RxOutcome<'a>;
    pub fn tick(&mut self, now: Instant) -> TickOutcome;
}

pub struct CallNo(u16);  // newtype, allocator helper in call_no.rs
```

Both types are `Send` (no internal `Rc`, no shared state). They are not
`Sync` — one instance per call, mutated by the owning runtime task only.

## Module layout

```
crates/astar-iax-core/src/
  session/
    mod.rs              — re-exports + module docstring
    fsm.rs              — SessionState, Fsm, Event, AppCommand, Action, AppEvent,
                          all per-state data structs (NewSentData, ActiveData, …)
    handlers_outbound.rs — per-state on_<state> methods for the outbound (caller) path
    handlers_inbound.rs  — per-state on_<state> methods for the inbound (callee) path (iax-8baf)
    reliability.rs      — Reliability, ReliabilityConfig, RxOutcome, TickOutcome
    keepalive.rs        — KeepaliveState, KeepaliveConfig — pure liveness/RTT for Active (iax-a307)
    builders.rs         — outbound IAX2 frame constructors shared by the handlers
    auth.rs             — Credentials, MD5 helper, plaintext/RSA stubs
    call_no.rs          — CallNo newtype + allocator
    call_profile.rs     — mode-varying NEW frame parameters (iax-3fca)
    reg.rs              — pure registration FSM (structural twin of fsm.rs, iax-bc14)
  lib.rs                — adds `pub mod session;`
```

## Testing strategy

**(a) Unit tests on `Fsm::handle`.** One per row of the transition table,
~30 total. Construct an Fsm directly, feed one Event, assert state + Actions.

**(b) Property tests on transition safety.**

```rust
proptest! {
    fn invalid_transitions_dont_panic(state in any_state(), event in any_event()) {
        let mut fsm = Fsm::with_state(state, default_creds());
        let _ = fsm.handle(event);
    }
}
```

`Fsm::with_state` is a `#[cfg(test)]` constructor — not part of the public
API. Property tests must never crash; unexpected pairs become
`Action::LogInvalid`.

**(c) Reliability unit tests.** Enqueue/tick/ACK flow, duplicates, VNAK,
give-up, piggyback ACKs.

**(d) Loopback integration test.** Two `Fsm`s wired through an in-memory
"wire" with optional packet loss. A tiny test-only server FSM (just enough
to accept NEW, send AUTHREQ, accept AUTHREP, send ACCEPT) drives the client
to `Active` under both clean and 10%-loss conditions.

**(e) C-FSM parity** — tracked under iax-64f0; runs against real pcap
fixtures captured from astar against an ASL3 hub, asserts the Rust FSM emits
structurally equivalent outbound frames given the same inbound sequence.

## Open questions

None at design-approval time. If implementation surfaces issues, append to
this section as ADR-style notes rather than editing the design above.

## References

- [RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456) (IAX2 protocol)
- `docs/spec/iax2-conformance.md` (RFC quirks)
- `docs/spec/allstar-dialect.md` (AllStar/app_rpt behaviors)
- `astar/vendor/iaxclient/lib/libiax2/src/iax.c` (C reference, with
  CALLTOKEN patch from astar commit `3613bc9`)
- DroidStar `iax.cpp:129-159` (independent CALLTOKEN impl)
- au tickets: **iax-c333** (this), **iax-a307** (keepalive),
  **iax-be21/d4e9/e0f8** (in-call event semantics), **iax-8baf** (incoming
  calls), **iax-64f0** (C parity test), **iax-612e** (high-level API).
