# Code Review: astar-lib core (2026-06-05)

Subagent review of `crates/astar-iax-core` + `crates/astar-conformance` (harness/docs/fixtures excluded). HEAD `9840e51` (master, post iax-7022 merge).

## Headline takeaways

- Solid parser/encoder foundation: byte-perfect round-trip is enforced by unit fixtures and the replay harness; truncation is handled cleanly; no `unsafe` blocks anywhere in scope.
- Sequence-number dedup is not wrap-safe (`reliability.rs:186`) — straight `<` comparison on `u8` will misclassify post-wrap frames as new, corrupting ISeqno tracking on long calls.
- IE string/byte encoder silently truncates payloads > 255 bytes (`ie.rs:477-480, 487-490`) without validating UTF-8 boundaries, which can produce invalid frames on the wire and lossy round-trips.
- FSM is a single 1358-line module with one giant `handle()` match — already hitting `#[allow(clippy::too_many_lines)]` and `std::mem::replace(state, Init)` as a workaround; needs refactoring before more states/commands land.
- `OwnedFrame` invariant relies on `Ies::parse` succeeding but is enforced with `.expect("ie_bytes well-formed")` in two hot paths — a future caller that constructs `OwnedFullFrame` directly with adversarial `ie_bytes` will panic the runtime.

---

## Critical

### C1. Wrap-around bug in ISeqno dedupe
`crates/astar-iax-core/src/session/reliability.rs:186`

```rust
if full.oseqno < self.next_iseqno { ... Duplicate ... }
```
Raw `u8` comparison; every other seqno comparison in this file uses wrap-aware `wrapping_sub` (see `release_acked` at line 216). After 256 frames in either direction, dedup misfires both ways: a fresh frame with `oseqno=0` arriving after `next_iseqno` wrapped to ~250 will be treated as a duplicate, and a delayed re-delivery of `oseqno=250` arriving after wrap to `next_iseqno=5` will be re-delivered as new and double-advance ISeqno. Use the same `oseqno.wrapping_sub(next_iseqno)` window trick as `release_acked`. Realistic IAX2 calls can outlive 256 reliable frames easily (each AUTHREQ + ACCEPT + control + retransmits chips away fast in registration storms).

### C2. IE encoder truncates without round-trip safety
`crates/astar-iax-core/src/ie.rs:477-480` (`write_str`) and `:487-490` (`write_bytes`)

Both helpers do `let len = v.len().min(255); out.put_slice(&bytes[..len])`. For `write_str` this can split a multi-byte UTF-8 codepoint mid-sequence, producing wire bytes that our own `parse_str` (line 430) will reject as `MalformedIe`. The encoder will then emit a frame that fails self-round-trip — a property the replay harness considers a hard error. Better: return an error from `encode` (or reject at `Ies::encode`) when an IE exceeds 255 bytes, and have callers shorten by codepoint. At minimum, add a `debug_assert!(v.len() <= 255)` so this fires loudly in tests.

### C3. FSM hardcodes `dest_call: 0` on NEW frames
`crates/astar-iax-core/src/session/fsm.rs:904` and `build_new`

`build_new(..)` always sets `dest_call: 0`, correct for the *initial* NEW but wrong for the resent NEW after CALLTOKEN — RFC 5456 §8.6.1 requires the second NEW to address the server's chosen `scallno`. The Reliability layer overwrites `dest_call` from `peer_call` *only if* `set_peer_call` has been called (`reliability.rs:130`). `set_peer_call` is never wired up from the FSM — `Reliability::peer_call` stays `None` through the whole CALLTOKEN → AUTHREQ → AUTHREP path, so the resent NEW and the AUTHREP both go out with `dest_call=0`. Real Asterisk accepts them by matching `source` IP+port, but conformance against a strict peer or any future proxy is broken. Wire `Reliability::set_peer_call` from the FSM as soon as AUTHREQ is observed (and again on ACCEPT).

---

## High

### H1. `Ies::parse(&ie_bytes).expect(...)` in hot Reliability paths
`crates/astar-iax-core/src/session/reliability.rs:145, 247`

Claims the bytes are well-formed because they came from `Ies::encode`. Holds today because every code path constructs `OwnedFullFrame.ie_bytes` via the encoder. But `OwnedFullFrame` is a `pub struct` with public `ie_bytes: Vec<u8>` (`frame.rs:126`). A driver or scenario could construct one with arbitrary bytes and crash the runtime on `enqueue`. Fix direction: make `ie_bytes` non-public and expose a `OwnedFullFrame::with_ies(ies: &Ies) -> Self` constructor, OR have `enqueue` propagate an error instead of panicking.

### H2. `tick()` retransmit loop can spin forever on zero-duration RTO
`crates/astar-iax-core/src/session/reliability.rs:226-263`

`while now >= entry.next_retry_at` advances the deadline by `new_rto`; if `new_rto` is `Duration::ZERO` (config bug, but `ReliabilityConfig` is `pub` and trivially constructible), the loop is infinite. Add `debug_assert!(new_rto > Duration::ZERO)` and/or clamp `new_rto = new_rto.max(Duration::from_millis(1))`. Flag in `ReliabilityConfig` docs that `initial_rto > 0` is required.

### H3. CallNo allocator is O(n) per allocation and uses ~32KB heap
`crates/astar-iax-core/src/session/call_no.rs:36-66`

`Vec<bool>` of 32k entries (32KB) walks every slot on each `alloc()`. A `BitVec` + last-freed stack would compress this to ~4KB and O(1) alloc. Not urgent; mostly an API smell — peer-allocated 15-bit ids and our local 15-bit ids are independent spaces.

### H4. Active state ignores incoming full-frame voice payload
`crates/astar-iax-core/src/session/fsm.rs:663-674`

```rust
Subclass::Voice(format) => {
    out.push(Action::AppEvent(AppEvent::VoiceReceived {
        format, payload: Vec::new(), ts: full.timestamp,
    }));
```
A full-frame voice packet (first of a stream, or after any subclass change) carries the audio payload in the post-header bytes. FSM throws it away — receiver loses the first 20 ms of every G.711 stream and every sample of any codec that always sends full frames (G.722 in some Asterisk modes). Parser design gap: voice full frames don't carry IEs, they carry raw samples. Audit `parse_full_with` and the `FrameType::Voice` path; payload should be carried alongside `ies` (or `ies` should be empty and a `payload: &[u8]` field added to `FullFrame` for non-IE-bearing frame types).

### H5. Lenient/strict parser asymmetry can drop CALLTOKEN
`crates/astar-iax-core/src/frame.rs:208 + ie.rs:263`

`parse_lenient` swallows any per-IE error (`let _ = apply_ie(...)`) including `MalformedIe`. If a peer sends a malformed CALLTOKEN (length-prefix lies about actual byte count and parser sees it as per-IE error rather than `Truncated`), FSM in `NewSent` sees no `calltoken` IE and replies with nothing, then times out. Lenient mode is wrong default for security-bearing IEs. Either keep CALLTOKEN strict even in lenient mode, or have driver log when lenient-skipped so failure mode is debuggable.

---

## Medium

### M1. `fsm.rs::handle` is 580+ lines in a single match; pattern is `mem::replace(state, Init)`
`crates/astar-iax-core/src/session/fsm.rs:221-799`

`#[allow(clippy::too_many_lines)]`, the giant `match (state, event)`, and the `std::mem::replace` trampoline are all symptoms that the state-transition representation is fighting Rust ownership. Split `handle` into one method per source state, OR make `SessionState` a struct with an enum `Phase` plus a shared `CallContext { our_call, peer_call, ... }` so transitions move `Phase` independently of context.

### M2. FSM constructs `OwnedFullFrame` with bogus `oseqno=0/iseqno=0` everywhere
`crates/astar-iax-core/src/session/fsm.rs:832-833, 872-873, 907-908`

`build_authrep`, `build_hangup`, `build_new` all set `oseqno: 0, iseqno: 0`. `Reliability::enqueue` stamps the real values. Working but extremely leaky. Either have `build_*` take `&Reliability` and stamp seqnos there, OR replace the `OwnedFullFrame` action payload with a `PendingFullFrame` newtype that has `oseqno`/`iseqno` typed as `Unassigned`.

### M3. `CodecMask = u32` and `CallToken = Vec<u8>` are not newtypes
`crates/astar-iax-core/src/session/fsm.rs:16-19`

`pub type` aliases give zero safety. Given the codec-mask bit-shift compression and the adversarial CALLTOKEN bytes RFC 5456 §8.6 exists to mitigate, both deserve newtypes (with `CallToken::new(bytes)` validating size ≤ 255).

### M4. `parse_subclass_byte` semantic gap for `FrameType::DtmfEnd/DtmfBegin`
`crates/astar-iax-core/src/frame.rs:317-318`

DTMF subclass values are ASCII digits (RFC 5456 §6.6), not opaque numbers. `Subclass::Raw(value)` fallback works for round-trip but provides no type-safety. Add a typed `Subclass::Dtmf(char)` arm. **Note**: directly relevant to iax-be21 open question 3.

### M5. FSM voice handling assumes G.711U on every mini frame
`crates/astar-iax-core/src/session/fsm.rs:650-655`

Per RFC 5456 §6.4 mini-frame's implicit format is the *last full voice frame's format*, not hardcoded G.711U. Once H4 is fixed, FSM also needs to track "current voice format on this leg".

### M6. `Reliability::enqueue` parses-then-encodes IEs for every send
`crates/astar-iax-core/src/session/reliability.rs:145-147`

Each enqueue re-parses `ie_bytes` into a typed `Ies` only to feed it to `frame::encode` which immediately serializes it again. Stamp seqno bytes directly into already-encoded wire bytes at fixed offsets (8, 9 for oseqno/iseqno; 0-3 for callno).

### M7. `Frame::Full(Box<FullFrame>)` is boxed but the box is reallocated on `into_owned`
`crates/astar-iax-core/src/frame.rs:140-156`

Two heap operations per round-trip. Make `OwnedFullFrame` boxed too.

### M8. Driver auto-ACK in `recv_one_frame` uses wrong iseqno
`crates/astar-conformance/src/driver.rs:30-34`

Extract the ACK builder into a shared helper used by both Reliability and the registration scenario.

---

## Low

- **L1.** `Ies` struct has 53 fields, all `Option` (~600 bytes per `Ies`). A `SmallVec<[(u8, IePayload); 8]>` keyed by IE id would be smaller.
- **L2.** `OwnedFrame::as_frame` returns `Result` but well-formedness is supposed to be an invariant. Should be `pub(crate)` or `OwnedFullFrame.ie_bytes` should be private.
- **L3.** `replay.rs:265-274` lossy timestamp scaling clamps overflow to `u32::MAX` silently.
- **L4.** `text.rs:203` K-status parser uses `split(' ')` not `split_whitespace`; double-space breaks parse, falls through to Raw silently.
- **L5.** `Secret` is `Clone` (`auth.rs:23`) — defeats the point of `zeroize`. Pass as `Arc<Secret>` or `&Secret`.

---

## Test gaps

- No tests for sequence-number wrap-around (Reliability or FSM). Given C1, this is the missing test that would have caught it.
- No fuzzing entry point in scope. Two short fuzz targets (`fuzz_parse_full`, `fuzz_parse_ies`) would be cheap.
- No test that a malformed IE inside CALLTOKEN's frame doesn't crash the FSM — `fsm.rs:263` does `Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty())` which means a malformed AUTHREQ silently loses the challenge and FSM produces empty MD5 response.
- `text.rs` proptest only generates valid K-status payloads.
- `Reliability::tick` not tested for interleaving where peer ACK arrives mid-tick.
- No test covers `RxOutcome::GaveUp` flowing into `Event::DeliveryFailed`. FSM has no arm — falls through to `LogInvalid`. Silent failure.

---

## What looks good

- Zero `unsafe` in scope; workspace lints (`pedantic` warn, `unused_must_use` deny) are tight.
- `parse(encode(frame)) == frame` enforced by unit fixtures and the pcap replay harness, including hand-spelled minimal-NEW byte-layout test pinning RFC 5456 §6.3.
- Subclass byte compression correctly implemented with bounds-check and explicit RFC citation; proptest covers every power-of-two shift.
- Zero-copy IE parsing with explicit borrowing through `Ies<'a>` is the right design — `OwnedFrame` cleanly bridges to long-lived data.
- `Secret` zeroizes on drop and has a `Debug` impl that doesn't leak.
- Proptests cover frame round-trip and FSM no-panic invariant (all 10 states crossed with all timer kinds and app commands).
- Driver loopback test is a real socket end-to-end smoke test pinning OS UDP stack integration.
- Clear module boundaries: `astar-iax-core` is genuinely I/O-free; `astar-conformance` owns sockets; FSM exposes Actions and is driven externally — exactly the shape for the planned mio + blocking-thread runtime.
- RFC citations inline at every non-obvious decision (CALLTOKEN=40, AUTOANSWER zero-length, MAX_SHIFT).
