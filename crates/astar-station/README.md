# astar-station

One-dependency facade over the IAX2 station machinery (console session, audio,
the ASL3 WT-connect recipe) behind a thread-safe [`Station`]. The consumer
writes only its own event loop and UI. PTT-source-agnostic: consumers call
`set_ptt(bool)` to key the mic; the hardware PTT driver lives in
`astar-ptt` / `astar-serial-sys` and is wired up by the application
layer, not this crate.

The whole surface is **poll-based**: call `snapshot()` for live meters/status
and `next_event()` for discrete lifecycle edges — no callbacks into a managed
runtime. This keeps the C/Python/Swift bindings simple and sound.

Vendor-neutral: a generic IAX2 station that *also* offers AllStar WT-connect as
one convenience method. AllStar is a policy layer, never baked into the core.

## Operating modes

A `Station` always runs a single `ConsoleSession`/`Manager` engine. Inbound
listening and outbound registration are **independent opt-in capabilities** that
can be toggled live:

| Capability | Enable | Disable |
|---|---|---|
| Inbound listener | `station.enable_inbound(InboundConfig)` | `station.disable_inbound()` |
| Outbound registration | `station.register(RegisterConfig)` | `station.deregister()` |

The `OperatingMode` enum (`Wt` / `Node`) is a **derived compatibility label**:
the station reports `Node` when an inbound listener is running, `Wt` otherwise.
`set_mode(Node)` / `set_mode(Wt)` are retained as a back-compat shim over
`enable_inbound` / `disable_inbound`. There is no separate `NodeEngine`.

### WT mode (dial out)

```rust
use astar_station::prelude::*;

let station = Station::new(StationConfig::default()); // mode = Wt
station.connect_wt("55553")?;        // AllStar WT-connect recipe, or:
// station.connect(host, caller_id, dest, secret)?;  // plain IAX2 dial

loop {
    while let Some(ev) = station.next_event() {
        match ev {
            StationEvent::Answered => { /* media flowing */ }
            StationEvent::Hangup { reason } => return Ok(()),
            _ => {}
        }
    }
    let snap = station.snapshot();       // live meters/status
    station.set_ptt(true)?;              // key the mic
}
```

### Inbound mode (accept calls — always-on node)

```rust
use astar_station::prelude::*;

let station = Station::new(StationConfig::default());
// Opt in to inbound — the WT dial-out path remains available concurrently.
station.enable_inbound(InboundConfig::default())?;   // binds 0.0.0.0:4569

loop {
    while let Some(ev) = station.next_event() {
        match ev {
            StationEvent::IncomingCall { from } => {
                // Manual policy only: ringing, awaiting a decision.
                station.answer()?;           // or station.reject()?
            }
            StationEvent::Answered => { /* bridged to the handset */ }
            StationEvent::Hangup { reason } => { /* caller hung up */ }
            _ => {}
        }
    }
    let snap = station.snapshot();
    station.set_ptt(true)?;                  // keys the routed call's mic
}
```

- **`AnswerPolicy::Auto`** — the engine answers each inbound offer automatically
  and bridges it to the local mic/speaker (the default).
- **`AnswerPolicy::Manual`** — the offer is parked and surfaced as
  `StationEvent::IncomingCall { from }`; the operator calls `answer()` / `reject()`.
- Up to `max_calls` (default 20) simultaneous calls; additional offers above
  this limit are adopted as **monitor-only** (RX only).

### Register AS a node (reachable by node number)

Set `NodeConfig::register = Some(RegisterConfig { peer, username, refresh })` to
register the node with an upstream registrar so callers can reach it by node
number. The registrar password is **never** stored in config — it is resolved
at runtime via `station.set_secret_resolver(..)`. Success/failure surface as
`StationEvent::Registered` / `RegisterFailed { reason }`.

## Secret-free

Call secrets (the guest secret and, on the WT path, a minted token) are
call-time arguments consumed into the session. Inbound-auth credentials live in
`policy.credentials`; the registrar password arrives only through the runtime
resolver hook. **No secret** ever appears in a `NodeConfig`, a snapshot, an
event, a device list, `Debug`, or any tracing line.

## Errors

`StationError` maps 1:1 to the C-ABI error codes. Mode-relevant variants:

- `Unsupported` — operation not available in the current build/mode.
- `NoPendingCall` — `answer`/`reject` called when not in Node mode or when no
  inbound offer is parked.
- `AtCapacity` — inbound offer rejected because `max_calls` is reached.

## Live handoff — testing node-as-handset solo

A solo operator can verify the Node-mode handset without a second person using
the in-process **parrot** (record-then-playback) test client:

1. **Start the node.** Bring up a `Station` in `OperatingMode::Node` with
   `auth = Off` for the test (the astar-inspect harness "Start Node" tab
   does this on `0.0.0.0:<port>`).
2. **Start the parrot.** `astar_iax::dial_raw(node_addr, "echo-test", "s", "",
   CallMode::Standard)` then `run_parrot(..)` — or run the example:
   `cargo run -p astar-iax --example echo -- 127.0.0.1:4569`. The harness can run
   this in-process via its "Start Parrot" button.
3. **Key, talk, release.** Use **headphones** (speaker→mic feedback otherwise).
   Hold the node-handset PTT, talk, release. The parrot records while you are
   keyed, waits ~3 s after you unkey, then plays your recording back through the
   node — you hear yourself.
4. **Real inbound.** Point a real ASL3/Asterisk node at the Node-mode `Station`;
   the operator hears the caller and PTTs back. The WT live-parrot path is
   unchanged.

See `crates/astar-iax/examples/echo.rs` (parrot test client) and
`crates/astar-iax/src/parrot.rs` (`run_parrot` / `ParrotConfig`).
