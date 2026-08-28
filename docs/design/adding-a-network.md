# Adding a network to astar

**What this is.** The recipe for lighting up a new digital-voice network, derived
from the two that exist — M17 (software codec) and D-Star (hardware vocoder) —
and from the bugs each of them produced on the way. It is meant to be followed
in order and argued with where it is wrong.

**The first thing to say: a network is not a codec.** The vocoder is one layer of
about fourteen, and on the evidence so far it is not the expensive one. When
D-Star reached the app in 0.1.8beta its vocoder, protocol crate, session, station
facade, C ABI and Swift binding were **already built** — the remaining work was
still four layers of plumbing and a mis-dial bug. M17's codec was `dlopen`-ing an
existing library. Budget accordingly: **plan for the plumbing, not the DSP.**

---

## 1. The layers, and who owns them

A network touches every level of the stack. Miss one and it fails silently
rather than loudly, which is the recurring theme of this document.

| # | Layer | Where | What it is |
|---|---|---|---|
| 1 | Protocol | `crates/astar-<net>/` | Framing, link FSM, headers. No I/O, no audio. |
| 2 | Vocoder | `crates/astar-codec/` | Encode/decode. Feature-gated; licence-sensitive. |
| 3 | Session | `crates/astar-console/src/<net>.rs` | Socket + audio + vocoder + run loop. |
| 4 | Console wiring | `crates/astar-console/src/session.rs` | Adopt, disconnect, exclusion, state mirror, **preference fan-out**. |
| 5 | Station facade | `crates/astar-station/src/station.rs` | `<net>_connect` / `_disconnect` / `_available` / `_state`. |
| 6 | Features | `crates/*/Cargo.toml` | The chain from `astar-sys` down. |
| 7 | C ABI | `crates/astar-sys/src/ffi.rs` | Entry points, snapshot fields, error code. |
| 8 | Header | `crates/astar-sys/include/astar.h` | Generated; must not drift. |
| 9 | Node guard | `crates/astar-server/src/controller.rs` | Remote keying refusal. |
| 10 | Swift binding | `bindings/swift/Sources/AstarStation/Station.swift` | Snapshot, state struct, methods. |
| 11 | Core model | `apps/macos/Packages/AstarCore/` | `CallSnapshot`, `StationDriving`, `Network`, `CallSession`. |
| 12 | Dial grammar | `AstarCore/Reflector*.swift` | `ReflectorDial` kind, resolution, address fallback. |
| 13 | Views | `apps/macos/Sources/` | Usually nothing. See §6. |
| 14 | Directory | hamcall-db | Rows with a `dial.kind` the client understands. |

---

## 2. The engine

### 2.1 Protocol crate

`crates/astar-dstar` is the model: `dsvt.rs` (framing), `fsm.rs` (link state),
`header.rs`, `reflector.rs` (a loopback server for tests), `tx.rs`. **No sockets
and no audio** — it turns bytes into frames and back, so it is testable without
hardware or a network.

Ship a loopback reflector with it, as `astar-dstar` does. Every session-level
test in the workspace binds one on `127.0.0.1:0`; without it the session layer is
untestable and the tests that exist would have to touch real infrastructure,
which is forbidden (§5.1).

### 2.2 Vocoder

Two shapes so far:

* **Software** (M17/Codec 2): `dlopen` at runtime via `astar-codec`'s
  `codec2-runtime`, with `codec2-static` as the shipped-app fallback. Driven by
  licensing — Codec 2 is LGPL, and a plain `cargo build` must stay free of it.
* **Hardware** (D-Star/AMBE): the ThumbDV over serial, `astar-codec/ambe-hw`,
  behind the `AmbeVoice` / `AmbeStream` traits with `open_ambe_stream()`.

**For the AMBE family this layer is mostly already built.** `vendor/ambe-thumbdv`
speaks the DVSI packet protocol, and the mode is selected by a single 12-byte
RATEP word — `packet::ratep_dstar()` is the only one implemented today. YSF DN,
NXDN and DMR are AMBE+2 on the same chip, the same serial transport and the same
driver: a different RATEP word plus that network's frame packing. Do not plan a
new vocoder integration for them; plan a RATEP table.

P25 Phase 1 is IMBE, a different rate again — **verify against the chip before
promising it**, rather than assuming the family extends. See
`p25-network.md`, which makes that verification the gating step and records what
to do if the answer is no.

Per-network designs: `ysf-network.md`, `nxdn-network.md`, `p25-network.md`,
`dmr-networks.md`.

> **Licence gate.** A vocoder's terms decide where it may live. Check them before
> writing code, put the dependency behind a non-default feature, and record the
> terms in `LICENSE-EXCEPTIONS.md`. `vendor/ambe-thumbdv` is MIT/Apache in an
> AGPL repo and its `Cargo.toml` hard-codes its own licence on purpose — never
> convert those to `.workspace = true`.

### 2.3 Session module

`crates/astar-console/src/dstar.rs` and `m17.rs`. This owns the socket, the audio
router lanes, the vocoder stream and a run-loop thread. The shape both share:

* `<Net>Config` — primitive fields only (host, port, module, callsign, devices).
* `<Net>Session::connect(cfg, backend_factory)` — **blocking**, and for hardware
  it blocks for seconds (a serial scan plus a per-port init). It must run with no
  session lock held; see §5.4.
* A **test seam** that takes a fake vocoder and a fake audio backend.
  `DstarSession::connect_with_stream` is the one to copy; M17 has no equivalent
  and is the poorer for it. Provide one, or the session layer is untestable and
  every test that wants it has to reach for hardware — which §5.1 forbids.
* `SharedState` — atomics and mutexes the run loop writes and the outside reads:
  link, talker, PTT, the three level meters, **and the listener-side audio
  preferences**.
* `apply_audio(&self, router, out)` — pushes those preferences onto the router.
  Call it once at connect *before the thread starts* and again every tick. It is
  three atomic loads; it needs no dirty-flag tracking.

### 2.4 Console wiring — the layer that bites

`ConsoleSession` in `session.rs`. Every item below is one a real implementation
forgot at least once.

- [ ] `<net>: Option<<Net>Session>` field
- [ ] `<net>_can_connect()` — mutual exclusion against IAX2 *and* every other
      network, checked again at adopt because state can change while the
      blocking connect runs
- [ ] `<net>_adopt(session)` — installs it, **and seeds it with the operator's
      current audio preferences**
- [ ] `<net>_disconnect()`
- [ ] `<net>_is_active()` / `<net>_state()`
- [ ] Snapshot mirror: `ptt`, `remote_ptt`, the three level meters, and
      **`status` mapped to `CallStatus`** — `Linked → Answered`,
      `Connecting → Dialing`, `Failed → Hangup`. This is what lets a front-end
      run **one** connection state machine for every network instead of a
      per-network special case. Get it wrong and the UI shows "Not connected"
      on a working link.
- [ ] `<net>_active` / `<net>_available` flags on `ConsoleState`
- [ ] **The audio-preference fan-out.** `set_output_gain`, `set_rx_compress`,
      `set_rx_compression_level` each need an arm for the new network.

> **This is the trap.** D-Star shipped in 0.1.8beta missing all three arms. The
> operator's volume never reached a D-Star session, so it played at the router's
> unity default while every other network sat where it had been set — heard on
> air as "D-Star is louder than it should be". It reads as a missing per-network
> level and it is a missing `if let`. `the_listener_side_preferences_reach_a_dstar_session`
> pins it; write the equivalent for the new network **before** going on air.

### 2.5 Station facade

`Station::<net>_connect/_disconnect/_available/_state` in `astar-station`.

* Signature takes **primitives, not the console's config type** — that type only
  exists when the feature is compiled in, and the method must stay
  byte-identically callable either way.
* With the feature off: the methods still exist, return
  `StationError::<Net>("... support not compiled")` or are no-ops or return
  `false`. Downstream code must never need its own `#[cfg]`.
* `<net>_available()` must report **reality**, not the build. For hardware that
  means "a dongle is present". The probe is memoized process-wide, so a dongle
  plugged in after launch is not seen until relaunch — document it, because it
  looks like a bug to a user.

### 2.6 Features

The chain, all the way down:

```
astar-sys/default = ["dstar"]
  └── astar-station/dstar = ["astar-console/dstar"]
        └── astar-console/dstar = ["dep:astar-dstar", "astar-codec/ambe-hw"]
```

Enabling a feature on `astar-sys` unifies it into **every** crate in a workspace
build — `astar-server` included. That is precisely why the node's key-refusal is
a runtime snapshot check and not a `#[cfg]` (§5.2).

### 2.7 C ABI and header

In `crates/astar-sys/src/ffi.rs`: `iax_station_connect_<net>`,
`iax_station_<net>_disconnect`, `iax_station_<net>_state` (JSON out through a
caller-sized buffer), `<net>_available` / `<net>_active` on `IaxSnapshot`, and an
`IAX_ERR_<NET>` code.

Then `just cbindgen` — the committed `astar.h` must not drift, and CI checks it.

> **Know what the error code costs you.** `iax_error_text()` returns a *static
> string per code*: `IAX_ERR_DSTAR` is `"dstar error"`. The engine's careful
> classification — "ThumbDV at /dev/cu.usbserial-… is busy — another process has
> it open" — **does not cross the ABI**. Until there is a last-error accessor,
> the app has to write its own message for the code (see
> `connectFailureMessage`), which can only name likely causes rather than the
> real one.

### 2.8 The node guard

`astar-server` exposes remote keying over HTTP. `key_refusal` refuses while the
snapshot reports `dstar_active`. A new network that can transmit needs the same
treatment, on the **snapshot flag** — never a `#[cfg]`, because §2.6 means the
feature is on in a workspace build.

---

## 3. The Swift binding

`bindings/swift/Sources/AstarStation/Station.swift`:

- [ ] `Snapshot.<net>Available` / `.<net>Active`
- [ ] `<Net>State` value type + hand-written JSON decode. Decode an unrecognised
      link string to the **safe** case — a UI that believes the link is down will
      not offer PTT.
- [ ] `connect<Net>(...)`, `<net>Disconnect()`, `<net>State()`
- [ ] Document that connect blocks and must not be called on the main thread

Then `just xcframework` — **both** frameworks, always. A stale cache lies: it
compiles against yesterday's C ABI and fails with "extra argument in call" in a
file you did not touch.

---

## 4. The app core

`apps/macos/Packages/AstarCore/`:

- [ ] `CallSnapshot`: the two flags, defaulted `false` so fixtures need no change
- [ ] `Station+Driving`: map them
- [ ] `StationDriving`: the three methods — **and `NullStation` and every test
      fake**, or the suite stops compiling
- [ ] `Network`: the case, `displayName`, `badge`, `symbol`, `dialPlaceholder`,
      `admitsDialCharacter`, and the `available(...)`/`resolve(...)` capability
      parameter
- [ ] `Network.reflectorNetwork` and `Network.matching` — the bridge to directory
      rows, and what makes them dialable rather than merely listed
- [ ] `ReflectorDial`: the `kind`, its `endpoint`/`callsign`/`modules`, and
      **`addressesModule`** — whether the protocol has rooms. Derive it from the
      protocol, *never* from whether `modules` happens to be populated
- [ ] `CallSession`: published capability flag, `<net>Target()` resolving
      **directory first and address second**, the `connect` arm, `canDial`,
      the `disconnect` teardown, the poll teardown edge, `ConnectError` cases,
      and a `connectFailureMessage` arm
- [ ] Address grammar: `ReflectorAddressDial.parse(_:defaultPort:)` if the
      network is reflector-shaped — one parser, a different default port

Model the connect arm on `connectM17`/`connectDStar` **exactly**: validate before
touching state, claim the single-flight dial slot, generation-gate every
post-completion write. The dial races are the same races for every network, and
two connect paths guarding them differently is how one ends up not guarding them
at all.

---

## 5. Invariants that outrank convenience

### 5.1 Never transmit on the air
Connecting to live nodes and keying PTT are Rob's manual actions. No test may
transmit anywhere but `127.0.0.1`. This is why §2.1 asks for a loopback
reflector: it is the only way a session-level suite can exist.

### 5.2 The node's key refusal stays
See §2.8.

### 5.3 Secrets are in-args only
Node secret, portal pass: never on a `Station`, never in a snapshot, event, error
or log.

### 5.4 Connect blocks; the lock does not
Build the session with the session mutex released, then install it under the
lock. A slow dongle must delay only its own caller, never every snapshot poll.

### 5.5 The capability flag must be honest
An entry offered that always fails to connect is worse than no entry.

### 5.6 Never guess a module
On D-Star and M17 the module *is* the room. A guessed one does not fail visibly
— it succeeds, and puts an operator into someone else's conversation under their
own callsign. Offer the letters, remember the last one used, and require a
choice. `modules` being empty means "not published", never "none".

---

## 6. What you do *not* have to build

The directory work paid for the client side once, for every network:

* **The picker segment** appears from `Network.available` — no view change.
* **The search sheet** works on any row with a `dial.kind`, filters by network,
  and shows undialable rows disabled with a reason.
* **The module picker** offers published modules where they exist and A–Z where
  they do not, and remembers per reflector.
* **Dial-by-name** resolves through `ReflectorIndex` ahead of every address
  parser.
* **Sync, freshness, attribution** are network-agnostic.

So for a reflector-shaped network already in hamcall-db — YSF, NXDN and P25 all
are — layers 12–14 are close to free and the work is layers 1–11.

The exception is a network that is **not** reflector-shaped. EchoLink is
node-based with its own directory, its own login and per-user callsign
validation; it reuses the engine recipe above and almost none of §6.

---

## 7. Order of work

1. Protocol crate + loopback reflector. Tests pass with no hardware.
2. Vocoder, if new. Licence first, code second.
3. Session module with `connect_with_stream`. Pipeline tests against loopback.
4. Console wiring — **including the fan-out** — with the preference test.
5. Station facade + features. `just ci`.
6. C ABI + `just cbindgen`. Header must not drift.
7. Swift binding + `just xcframework`.
8. AstarCore: snapshot, protocol, `Network`, `CallSession`. `just app-test`.
9. Directory rows in hamcall-db, if not already published.
10. `just app`, verify the picker appears and the dial resolves — **without
    connecting**.
11. Hand it to Rob for the on-air test.

Steps 1–4 are most of the work. Step 10 is where you find out you forgot §2.4.
