# IAX2 conformance checklist

Tracker: au nugget iax-f02d.

This file is the conformance checklist for the pure-Rust IAX2 stack — every entry below MUST have a test fixture before the corresponding behavior is considered implemented.

Source of truth: `~/dev/astar/docs/architecture/iax2-protocol.md` §"Discovered quirks (append-only)". That file is append-only in astar; when new quirks are added there, mirror them here (see "Upstream sync" below).

Sibling: `rfc5456-audit.md` tracks the RFC-side of the contract — every normative MUST/SHOULD/MAY and its implementation status (au:iax-d649). The audit is "what the RFC says we must do"; this checklist is "what astar's deployment actually does that the RFC doesn't specify." Quirks here that touch a normative requirement get cross-referenced from the audit row's Notes column.

## Upstream sync workflow

When astar appends a new quirk to its `iax2-protocol.md` log, copy the entry here verbatim (preserving date, frame/IE label, source citations, and implication wording) and add a fresh "Test plan" paragraph plus a "Status" line. Do NOT edit astar's log from this repo — astar owns that file as its truth-of-record, and any clarifications belong upstream there first. After a sync, bump the "Origin" section at the bottom with the new astar commit hash so future readers can diff cleanly.

## Quirks

### 2026-05-29 — vendored iaxclient is missing IAX2.h constants past 37

**Frame / IE**: subclass numbering in `lib/libiax2/src/iax2.h` vs `iax.h`.

**Source**: empirically while patching for [astar-39b5](../../../astar/docs/architecture/iax2-call-tokens.md). The vendored `iax.h` stops at `IAX_COMMAND_UNQUELCH = 29`; the parallel `iax2.h` goes to `IAX_COMMAND_FWDATA = 37`. Both headers exist and both are included via the build (different translation units). Highest IE in `iax2.h` is `IAX_IE_RR_OOO = 51`. Astar commit `3613bc9`.

**Implication**: do not key constants off any single SourceForge header — cross-check against [Asterisk `channels/iax2/include/iax2.h`](https://github.com/asterisk/asterisk/blob/master/channels/iax2/include/iax2.h) and [Wireshark `epan/dissectors/packet-iax2.h`](https://github.com/wireshark/wireshark/blob/master/epan/dissectors/packet-iax2.h). The spec is what's on the wire, not what the C tree says.

**Test plan**: build a constants-table unit test that asserts every `Subclass` and `InformationElement` enum variant matches the value defined in the Asterisk `iax2.h` header (vendored as a fixture under `tests/fixtures/asterisk-iax2.h`) and the Wireshark dissector header. The test parses both upstream headers at build time (or via a checked-in snapshot) and fails on any divergence, so adding a Rust variant without updating the snapshot — or upstream adding a frame type we don't model — produces a CI failure rather than a wire-time `REJECT`.

**Status**: not-yet-implemented

### 2026-05-29 — `iax_call()` recursion is the cleanest CALLTOKEN resend path

**Frame / IE**: `NEW`.

**Source**: patch in [`iax.c`](../../../astar/vendor/iaxclient/lib/libiax2/src/iax.c). The first plan was to extract IE-building into a helper and replay from cached args. In practice `iax_call()` is short enough that just *re-entering* it with a `calltoken_resend` flag guarding the once-per-call setup (capability assignment, ping scheduler) is fewer LOC and easier to read. Astar commit `3613bc9`.

**Implication**: model NEW emission as a pure function over `(session, has_token)` rather than an imperative pipeline. The C's parse-tmp-via-strtok mutation is a code smell that becomes painless in a type-safe layer.

**Test plan**: property test that `build_new_frame(session, token=None)` and `build_new_frame(session, token=Some(t))` produce byte-identical output except for the CALLTOKEN IE region, with no observable side effects on `session` (capability assignment, ping scheduler state, OSeqno) between the two invocations. Pair with a state-machine test that drives `NEW → CALLTOKEN → NEW(with token) → AUTHREQ` and asserts the ping scheduler was armed exactly once.

**Status**: Implemented. `handlers_outbound.rs`: `IaxCommand::CallToken` in the NewSent/CallTokenReceived states extracts the token and emits a resent NEW with the token IE; `on_calltoken_received` drives the `CallTokenReceived` → `NewSent` transition. The initial NEW always carries an empty CALLTOKEN IE.

### 2026-05-29 — empty CALLTOKEN IE is safe against `requirecalltoken=no` peers

**Frame / IE**: `NEW` with `IAX_IE_CALLTOKEN` length=0.

**Source**: cross-check with [DroidStar iax.cpp:129-159](https://github.com/nostar/DroidStar/blob/master/iax.cpp) — same wire shape. Confirmed by [RFC 5456 §8.6](https://datatracker.ietf.org/doc/html/rfc5456#section-8.6) that the empty IE is the opt-in signal; servers without the requirement ignore it and reply with `AUTHREQ` directly. Astar commit `3613bc9`.

**Implication**: always emit the CALLTOKEN IE; there's no compatibility cost. Distinguish "haven't received token yet" (`len == 0`, `pending == true`) from "no token will ever come" (`len == 0`, `pending == false`, no resend) in the state machine.

**Test plan**: two parallel integration tests against fixture servers — one that requires call tokens (responds with `CALLTOKEN`) and one that does not (responds with `AUTHREQ` directly). Both initial `NEW` frames must be byte-identical and contain the empty `IAX_IE_CALLTOKEN`. Add a unit test on the call FSM that asserts the `pending` flag transitions `true → false` only after receiving either `CALLTOKEN` or `AUTHREQ`, and that a subsequent `NEW` is only emitted in the former case.

**Status**: Implemented. The initial NEW carries `IAX_IE_CALLTOKEN` length=0; `IaxCommand::CallToken` triggers the resend; `IaxCommand::AuthReq` arriving without a prior CALLTOKEN takes the no-token path directly. Both arms verified in `handlers_outbound.rs`.

### 2026-05-29 — IAX2 token is opaque + per-call + 10s TTL

**Frame / IE**: `IAX_IE_CALLTOKEN` payload.

**Source**: [RFC 5456 §8.6](https://datatracker.ietf.org/doc/html/rfc5456#section-8.6); also [`docs/architecture/iax2-call-tokens.md`](../../../astar/docs/architecture/iax2-call-tokens.md) §1. Server generates it as `SHA1(remote_ip + remote_port + timestamp + random)`; client never inspects. Astar commit `3613bc9`.

**Implication**: do NOT cache the token across calls in any per-peer or per-session structure. Treat it as ephemeral per-NEW state. Don't bother with TTL tracking — server REJECT on expiry is sufficient.

**Test plan**: type-level test — the token field lives on the per-call `PendingNew` state, not on `Peer` or `Session`, and is dropped when the call transitions out of `AwaitingAccept`. Add a unit test that simulates a slow client (delays >10s between receiving `CALLTOKEN` and sending the second `NEW`) and asserts the resulting server `REJECT` is surfaced cleanly to the caller rather than triggering a retry loop. The client must never compare two tokens for equality or persist one to disk; a grep-based test in CI forbids `token.eq(` and `token.clone()` outside the single per-call state struct.

**Status**: not-yet-implemented

### 2026-05-29 — REGREQ does not yet need CALLTOKEN against `register.allstarlink.org`

**Frame / IE**: `REGREQ`.

**Source**: empirical — observed REGACK with no preceding CALLTOKEN reply during astar-39b5 work. Astar commit `3613bc9`.

**Implication**: registrar and per-node Asterisk instances have independent calltoken-enforcement policies. The spec must allow both REGREQ-with-token and REGREQ-without-token round-trips. Tracking: [astar-NNNN follow-up nugget for REGREQ calltoken path].

**Test plan**: integration tests against two fixture registrars — one that ACKs the bare `REGREQ` and one that demands a token first. The registration FSM must drive both to a successful `REGACK` without code-path divergence above the frame layer (the calltoken handling is the same logic as for `NEW`, parameterized on frame type). A pcap-replay test captures a real `register.allstarlink.org` exchange and asserts the client accepts the no-token path without timing out waiting for `CALLTOKEN`.

**Status**: Implemented (iax-64b6, commit 07a6a3d). `reg.rs` `RegFsm::handle`: a CALLTOKEN reply to REGREQ triggers a resent REGREQ with the token; a direct REGACK (no-token path) is also accepted. `SetPeerCall` is deferred until REGACK in both cases.

## Origin

Quirks ported from astar `docs/architecture/iax2-protocol.md` as of 2026-05-30. astar commit hash: `3613bc9`.
