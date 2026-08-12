# AllStar dialect of IAX2

Tracker: au nugget iax-3522.

Stock IAX2 ([RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456)) is
necessary but not sufficient to talk to an AllStarLink hub. Asterisk 20 (the
basis of ASL3) tightens defaults — call tokens are mandatory — and the
`app_rpt` application running on every AllStar node uses a small set of
IAX2 control / DTMF / text behaviours to carry repeater semantics. A
client that only implements RFC 5456 will fail in two distinct ways: it
is silently dropped at NEW time by any modern hub, and even if accepted
cannot signal PTT, disconnect a link, or participate in multi-hop
key-status announcements.

This file catalogues those behaviours with byte-level wire detail and a
conformance-test plan per item. **Mandatory** items must ship in Phase 0;
**Candidate** items are research-grade and may slip to later phases.

## Mandatory behaviours

### 1. CALLTOKEN handshake on outgoing NEW

**Wire format.**

- New IAX2 command subclass: `IAX_COMMAND_CALLTOKEN = 40` (0x28) in the
  subclass byte of an `AST_FRAME_IAX` (= 6) full-frame header. Canonical
  definition in
  [`asterisk/channels/iax2/include/iax2.h`](https://github.com/asterisk/asterisk/blob/master/channels/iax2/include/iax2.h).
- New IE: `IAX_IE_CALLTOKEN = 54` (0x36), opaque variable-length.
  Same header: `#define IAX_IE_CALLTOKEN 54 /*!< Call number security
  token */`.
- Three-frame round-trip (RFC 5456
  [§8.6](https://datatracker.ietf.org/doc/html/rfc5456#section-8.6)):
  (1) Client → Server `NEW` with `IAX_IE_CALLTOKEN` length 0 (opt-in).
  (2) Server → Client `CALLTOKEN` (subclass 40), source call number 0,
  `IAX_IE_CALLTOKEN` carrying ~64–128 opaque bytes.
  (3) Client → Server `NEW` re-sent with `IAX_IE_CALLTOKEN` populated
  with the exact bytes from step 2, within 10 s.

**Direction.** Client → Server (steps 1 & 3); Server → Client (step 2).
Bidirectional in aggregate.

**Trigger.** Every outbound call setup. The empty-IE opt-in must be sent
unconditionally — servers with `requirecalltoken=no` ignore it and reply
with `AUTHREQ` directly (confirmed by RFC 5456 §8.6 and cross-checked
against DroidStar
[`iax.cpp:129-159`](https://github.com/nostar/DroidStar/blob/master/iax.cpp)).

**Failure mode.** ASL3 hubs default `requirecalltoken=yes` since Asterisk
20. Without the empty CALLTOKEN IE the hub silently drops the NEW — no
REJECT, no client-side log. The client times out a few seconds later
with no useful information. Server log:
`Call rejected, CallToken Support required` (see
[`iax2-call-tokens.md`](../../../astar/docs/architecture/iax2-call-tokens.md)
§3). This is the single most common reason a stock iaxclient build
mysteriously fails against any modern AllStarLink node.

**Test plan.** Asterisk-in-Docker harness from
[`iax2-call-tokens.md`](../../../astar/docs/architecture/iax2-call-tokens.md)
§5, with two `iax.conf` contexts: `[astartest]` (`requirecalltoken=yes`)
and `[astartest_notok]` (`requirecalltoken=no`). For each, place a call
and capture on `udp.port == 4569`. Against `[astartest]`: assert
exactly two `NEW` frames with one `CALLTOKEN` between them; the second
`NEW` carries an `IAX_IE_CALLTOKEN` whose bytes equal the reply's IE
bytes. Against `[astartest_notok]`: assert exactly one `NEW`
immediately followed by `AUTHREQ` (confirms the empty-IE opt-in is safe
against lenient peers). In both cases the echo extension `s` completes
audibly.

**Cross-refs.** Reference implementation of the C patch is astar's
vendored fork at
[`vendor/iaxclient/lib/libiax2/src/iax.c:2272-2289`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/iax.c)
(outgoing IE append) and `iax.c:2823-2835` (inbound CALLTOKEN
handler) — astar nugget `astar-39b5`. Tracked here as `iax-c333` (call
FSM) and `iax-d4e9` (control-frame model).

### 2. AST_CONTROL_RADIO_KEY / AST_CONTROL_RADIO_UNKEY (PTT signalling)

**Wire format.** Full frame, `frametype = AST_FRAME_CONTROL = 4`
(vendored
[`frame.h:27`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/frame.h),
matches upstream
[`include/asterisk/frame.h`](https://github.com/asterisk/asterisk/blob/master/include/asterisk/frame.h)
line 115). Subclass byte carries the radio control subtype:
`AST_CONTROL_RADIO_KEY = 12` (line 297, `/*!< Key Radio */`) and
`AST_CONTROL_RADIO_UNKEY = 13` (line 298, `/*!< Un-Key Radio */`). No
payload; the subclass alone carries the semantics.

Note the upstream names are **`RADIO_KEY` / `RADIO_UNKEY`**, not
`AST_CONTROL_KEY` / `AST_CONTROL_UNKEY` as sometimes seen in community
docs. The vendored iaxclient `frame.h` only goes up to
`AST_CONTROL_OPTION = 11` and does not define either; the Rust port
must add them. `chan_iax2`'s `iax2_is_control_frame_allowed()`
(upstream
[`chan_iax2.c`](https://github.com/asterisk/asterisk/blob/master/channels/chan_iax2.c)
≈ line 2650) explicitly permits both subclasses across IAX2 — they
propagate node-to-node, not just to the local PBX.

**Direction.** Both. A client driving PTT (e.g. a softphone with a hold-
to-talk button) emits `RADIO_KEY` when the user keys down and
`RADIO_UNKEY` when they release. The hub re-emits the same control to
every other connected node so multi-hop receivers know to open their
squelch.

**Trigger.** Client → hub: PTT transitions in the local UI. Hub → client:
any time another node on the linked talk-path keys or unkeys. In
`app_rpt` this is invoked via `ast_indicate(myrpt->txchannel,
AST_CONTROL_RADIO_KEY)` (≈ line 3850 of
[`AllStarLink/app_rpt/apps/app_rpt.c`](https://github.com/AllStarLink/app_rpt/blob/master/apps/app_rpt.c))
and the matching `RADIO_UNKEY` ≈ line 3869.

**Failure mode.** Without RADIO_KEY/UNKEY emission the hub never knows
the user is transmitting; courtesy tone, kerchunk timer, and ID stay
quiet, and remote nodes hear silence even though audio frames flow. The
hub may drop the call after the inactivity timeout if it never sees a
key indication. Without RADIO_KEY/UNKEY *reception* the client UI
cannot show talker state — degrades UX but does not break audio.

**Test plan.** Bring up the Docker harness with two containers each
running `app_rpt` linked over IAX2. Connect the Rust client to node A.
(1) Hold PTT for 2 s, release; assert two control frames in Wireshark
(`iax2 && iax2.iax.subclass == 4`) with subclasses `0x0c` then `0x0d`.
(2) From a second softphone on node A, key for 2 s; assert the Rust
client receives a control frame frametype `0x04` subclass `0x0c`
followed by `0x0d`. (3) Confirm hub-side `iax2 set debug on` reports
`Control Frame -- KEY` / `UNKEY`.

**Cross-refs.** astar nugget `astar-96f5` (C-patch side of the same
behaviour, queued post-CALLTOKEN). This repo: `iax-d4e9` (control-frame
model — must include RADIO_KEY/UNKEY in the public Rust enum).

### 3. DTMF BEGIN / END and AllStar control macros

**Wire format.** Two distinct wire encodings exist; the Rust port must
emit one and accept both:

- **Legacy single-frame DTMF.** Frametype `AST_FRAME_DTMF = 1` with the
  ASCII digit in the subclass field. Vendored
  [`frame.h:24`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/frame.h)
  (`/* A DTMF digit, subclass is the digit */`); emitted by vendored
  [`iax.c:1695-1697`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/iax.c)
  via `iax_send_dtmf()`. Single-frame per digit, no duration.
- **Modern BEGIN / END pair.** `AST_FRAME_DTMF_END = 1` and
  `AST_FRAME_DTMF_BEGIN = 12` per upstream
  [`include/asterisk/frame.h`](https://github.com/asterisk/asterisk/blob/master/include/asterisk/frame.h)
  enum `ast_frame_type` lines 109 and 130. RFC 5456
  [§6.7](https://datatracker.ietf.org/doc/html/rfc5456#section-6.7)
  documents the pair. Asterisk 20 emits BEGIN+END; the on-wire
  `DTMF_END=1` byte is identical to legacy `DTMF=1`, so old peers parse
  the END as a single-shot DTMF and ignore the BEGIN.

Subclass byte is the literal ASCII digit (`'0'`–`'9'`, `'*'`, `'#'`,
`'A'`–`'D'`); no payload. The Rust port should emit BEGIN+END pairs
and accept either form on receive; treat a legacy single-frame DTMF as
BEGIN followed by END at a default 100 ms duration in the public API.

**AllStar standard-command macros** (per
[allstarlink.github.io/basics/standardcommands/](https://allstarlink.github.io/basics/standardcommands/),
fetched 2026-05-30):

| Code         | Action                              | Mandatory? |
| ------------ | ----------------------------------- | ---------- |
| `*1<node>`   | Disconnect link                     | yes        |
| `*2<node>`   | Connect in monitor mode             | yes        |
| `*3<node>`   | Connect in transceive mode          | yes        |
| `*70`        | Local connection status             | yes        |
| `*71`        | Disconnect all links                | optional   |
| `*73`        | System-wide connection status       | optional   |
| `*80` / `*81`| Force ID / say system time          | optional   |

`*76` from the task brief is **not** on the standard-commands page as
of 2026-05-30; it is a community alias some nodes bind in their local
`rpt.conf [functions]` stanza. The standard disconnect-all is `*71`.
Expose `*71` as canonical and treat `*76` as a user-configurable alias.

**Direction.** Client → hub (the operator dials the macro). Hub → client
DTMF is also valid on the wire (e.g. relay of DTMF from a far-end RF
input) and must be parsed.

**Trigger.** A user action in the client UI — pressing a DTMF keypad
button, invoking a macro button, or typing a star-code in a quick-dial
field.

**Failure mode.** Without DTMF support the client cannot connect to or
disconnect from any other AllStar node — the macros *are* the
connect/disconnect UX on the public network. There is no IAX2-level
"connect link" command; everything goes through `*3<node>` DTMF
interpreted by `app_rpt` on the hub.

**Test plan.** Two-node Docker harness, Rust client on node A. (1)
Send DTMF sequence `*31234`; assert Wireshark shows six BEGIN+END
pairs (frametypes `0x0c`/`0x01`, subclass bytes `'*' '3' '1' '2' '3'
'4'`). (2) Assert `iax2 show channels` on node A reports a new link to
node 1234 within 2 s of the trailing digit. (3) Repeat with `*11234`
and assert the link tears down. (4) Send `*70` and assert the client
hears the recorded connection-status prompt.

**Cross-refs.** Macro list source:
[AllStarLink standard commands](https://allstarlink.github.io/basics/standardcommands/);
DTMF wire format: RFC 5456 §6.7 plus vendored
[`iax.c:1695`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/iax.c).
This repo: `iax-d4e9` (DTMF in the control-frame model).

## Candidate behaviours

### 4. NNX 6-digit AllStarLink node extensions

**Status:** research needed; recommend treating as string-handling, not
protocol.

Node numbers are 4–6 decimal digits today (`2000`, `27225`, `500000`).
`IAX_IE_CALLED_NUMBER` is a variable-length string IE with a one-byte
length prefix (max 255 chars), so 6 digits fit trivially. No wire
change is *required*. Open follow-ups: legacy hub dial-plans may cap
extensions at 4 digits before reaching `app_rpt`; `app_rpt`'s
`_macro_exec` path needs verification; registrar handling of a 6-digit
USERNAME IE is unverified. **Recommendation:** the Rust port imposes no
length cap on dial strings under 255 ASCII chars; revisit if hub-side
rejection surfaces. Track under astar nugget `astar-0f23`.

### 5. TEXT-channel K-status broadcast

**Wire format.** IAX2 full frame, frametype `AST_FRAME_TEXT = 7`
(vendored
[`frame.h:30`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/frame.h)
and upstream `frame.h` line 123). Subclass is unused; the payload is the
text string.

`app_rpt` emits the K-status message at approximately
[`AllStarLink/app_rpt/apps/app_rpt.c`](https://github.com/AllStarLink/app_rpt/blob/master/apps/app_rpt.c)
line 3461 inside `handle_link_data()`, with format:

```
K <src> <name> <keyed> <since>
```

Concretely: `snprintf(tmp1, sizeof(tmp1), "K %s %s %d %d", src,
myrpt->name, myrpt->keyed, n);` — four whitespace-separated fields:

- `src` — source node identifier (string, no quoting).
- `myrpt->name` — local repeater name from `rpt.conf` (string).
- `myrpt->keyed` — 0/1 boolean as decimal.
- `n` — seconds since last key transition as decimal.

A receiving client parses this off the IAX2 text channel and can
present "node 27225 is keying for 4 s on hub W6XYZ" in the UI without
needing AMI or the public Stats API.

**Direction.** Hub → client (broadcast). A client never originates
K-status messages.

**Trigger.** Every key/unkey transition on any node within the linked
talk-path; `app_rpt` re-broadcasts to every connected IAX2 link.

**Failure mode.** None at the audio level — the call continues to work.
The UI degrades: the client cannot show live talker / last-keyed-by
without falling back to the rate-limited Stats API
(`https://stats.allstarlink.org/api/stats/{NODE_ID}`, 30 req/min/IP).

**Test plan.** Two-node Docker harness, Rust client on node A. From
node B simulate a key (`RADIO_KEY` control or `rpt cmd 99 cop 1`).
Assert the Rust client receives an `AST_FRAME_TEXT` (frametype 7)
payload matching `^K \S+ \S+ \d \d+$`, and that the `keyed` field
flips 0 → 1 → 0 across the transition.

**Cross-refs.** Community ask: "K (Key) status in the IAX text channel"
([wishlist thread](https://community.allstarlink.org/t/creating-a-requested-feature-list/22699)).
astar's features survey lists this at
[`docs/research/allstarlink-features.md`](file:///Users/rob/dev/astar/docs/research/allstarlink-features.md)
line 56.

### 6. REGREQ with CALLTOKEN

**Status:** Implemented (iax-64b6, commit 07a6a3d). The registration FSM
(`crates/astar-iax-core/src/session/reg.rs`) handles both the with-token
and without-token paths: a CALLTOKEN reply to a REGREQ triggers a resent
REGREQ carrying the token (using `dest_call=0`, with `SetPeerCall` deferred
until REGACK); a direct REGACK without CALLTOKEN is also accepted. See
`RegState` and the CALLTOKEN arm in `RegFsm::handle`.

**Wire format.** Same `IAX_IE_CALLTOKEN = 54` IE appended to a `REGREQ`
(IAX command subclass 15) frame, with the same three-frame round-trip
as for NEW (§1). The CALLTOKEN reply from the server in step 2 uses
subclass 40 and source call number 0 exactly as for the NEW case.

**Direction.** Client → server (REGREQ + token); server → client
(CALLTOKEN reply, then REGACK).

**Trigger.** Every IAX2 registration round-trip (typically once per
session, refreshed every `refresh` seconds — default 60 s on the
AllStarLink registrar).

**Failure mode.** None today against `register.allstarlink.org` —
astar's discovered-quirks log dated 2026-05-29 records a REGACK without
preceding CALLTOKEN, confirming the registrar is currently lenient
([`iax2-protocol.md`](file:///Users/rob/dev/astar/docs/architecture/iax2-protocol.md)
line 134). Both paths are now handled by the registration FSM; the
lenient (no-token) path does not wait for a CALLTOKEN that never comes.

**Test plan.** Docker harness with the registrar configured
`requirecalltoken=yes`. Issue REGREQ; assert two `REGREQ` frames in
Wireshark with one `CALLTOKEN` between them (second REGREQ carries the
populated CALLTOKEN IE) followed by `REGACK`. Repeat against a
`requirecalltoken=no` registrar context; assert exactly one `REGREQ`
immediately followed by `REGACK` (the empty CALLTOKEN IE is ignored).

**Cross-refs.** astar nugget `astar-afd5`. Same `IAX_IE_CALLTOKEN`
state machine the §1 implementation already needs — the marginal cost
of also gating REGREQ on it is small.

### 7. Permanent / re-connecting links

**Status:** pure client-side concern, no wire impact.

The community ask "permanent / persistent connections that actually
re-establish after network drops" (WD5M, AllStarLink wishlist;
[`features.md` line 62](file:///Users/rob/dev/astar/docs/research/allstarlink-features.md))
sounds protocol-flavoured but is not. IAX2 has no "permanent link"
frame, no auto-reconnect IE, no link-quality SLA. The desired
behaviour: on link teardown (HANGUP, PING/PONG timeout, UDP
unreachable, host-network drop) the client re-runs full setup
(NEW → CALLTOKEN → AUTHREQ → AUTHREP → ACCEPT) with exponential
back-off, and replays user intent (e.g. re-dial `*3<node>` if that was
the active connection). All state-machine concerns above the IAX2
layer; no new wire features beyond §1–§3 required. The Rust port
should expose a "link policy" abstraction over the call FSM
(`iax-c333`).

**Failure mode.** Without re-connect logic the user must manually
re-dial; with it, drops are invisible for typical hub-restart
durations (30–60 s).

**Test plan.** Establish a link to node A; dial `*3<node B>` to bridge
to B. Run `docker compose restart asterisk`. Assert retry attempts
follow the documented back-off (1 s, 2 s, 4 s, … capped at 30 s); once
the hub returns, assert link re-establishment and automatic re-issue
of `*3<node B>`. Total operator-perceived disruption ≤ hub downtime
+ 30 s.

**Cross-refs.** Community discussion: AllStarLink wishlist thread.
This repo: link-policy FSM (open nugget — file under `iax-c333`).

## Summary table

| Behaviour                          | Tier      | Wire IE / subclass               | Direction | Trigger                       |
| ---------------------------------- | --------- | -------------------------------- | --------- | ----------------------------- |
| 1. CALLTOKEN on NEW                | Mandatory | `IAX_IE_CALLTOKEN=54`, cmd 40    | Both      | Every outbound call           |
| 2. RADIO_KEY / RADIO_UNKEY         | Mandatory | `AST_FRAME_CONTROL=4` sub 12/13  | Both      | PTT edge                      |
| 3. DTMF + AllStar macros           | Mandatory | `AST_FRAME_DTMF_BEGIN/END=12/1`  | Both      | Operator keypress / receive   |
| 4. NNX 6-digit extensions          | Candidate | `IAX_IE_CALLED_NUMBER` string    | Client→   | Long-form dial                |
| 5. TEXT K-status                   | Candidate | `AST_FRAME_TEXT=7` payload `K …` | Hub→      | Any key transition            |
| 6. REGREQ + CALLTOKEN              | Implemented | `IAX_IE_CALLTOKEN=54` on REGREQ  | Both      | Every registration round-trip |
| 7. Persistent re-connect           | Candidate | (none — client-side state only)  | n/a       | Link teardown                 |

## Sources

Authoritative protocol:

- [RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456) — §6.7
  (DTMF), §8.5 (auth), §8.6 (call tokens), §12 (security).
- [`asterisk/channels/iax2/include/iax2.h`](https://github.com/asterisk/asterisk/blob/master/channels/iax2/include/iax2.h)
  — confirmed `IAX_COMMAND_CALLTOKEN=40`, `IAX_IE_CALLTOKEN=54`.
- [`asterisk/include/asterisk/frame.h`](https://github.com/asterisk/asterisk/blob/master/include/asterisk/frame.h)
  — `enum ast_frame_type` (DTMF_END=1, CONTROL=4, IAX=6, TEXT=7,
  DTMF_BEGIN=12) and `enum ast_control_frame_type`
  (`RADIO_KEY=12` line 297, `RADIO_UNKEY=13` line 298).
- [`asterisk/channels/chan_iax2.c`](https://github.com/asterisk/asterisk/blob/master/channels/chan_iax2.c)
  `iax2_is_control_frame_allowed()` ≈ line 2650 permits RADIO_KEY/UNKEY
  across IAX2.

AllStar-specific:

- [AllStarLink standard commands](https://allstarlink.github.io/basics/standardcommands/)
  — DTMF macros.
- [`AllStarLink/app_rpt/apps/app_rpt.c`](https://github.com/AllStarLink/app_rpt/blob/master/apps/app_rpt.c)
  — K-status format `"K %s %s %d %d"` ≈ line 3461; RADIO_KEY/UNKEY
  emission ≈ lines 3850 / 3869.
- [AllStarLink iax.conf manual](https://allstarlink.github.io/config/iax_conf/)
  — `requirecalltoken` defaults.
- [AllStarLink wishlist thread](https://community.allstarlink.org/t/creating-a-requested-feature-list/22699)
  — origin of items §5 and §7.

astar repo (read-only sibling):

- [`docs/architecture/iax2-protocol.md`](file:///Users/rob/dev/astar/docs/architecture/iax2-protocol.md)
  — discovered-quirks log informs §1 and §6.
- [`docs/architecture/iax2-call-tokens.md`](file:///Users/rob/dev/astar/docs/architecture/iax2-call-tokens.md)
  — implementation guide for §1 and the Docker harness reused
  throughout this file.
- [`docs/research/allstarlink-features.md`](file:///Users/rob/dev/astar/docs/research/allstarlink-features.md)
  — community-asks survey behind §5 and §7.
- [`vendor/iaxclient/lib/libiax2/src/iax.c`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/iax.c)
  lines 2272–2289 (outgoing) and 2823–2835 (inbound) — astar's C
  reference patch for §1 (nugget `astar-39b5`).
- [`vendor/iaxclient/lib/libiax2/src/frame.h`](file:///Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/frame.h)
  — vendored frame constants; lacks RADIO_KEY/UNKEY (Rust port must
  add).

Cross-language cross-checks:

- [DroidStar `iax.cpp:129–159`](https://github.com/nostar/DroidStar/blob/master/iax.cpp)
  — independent confirmation of the empty-CALLTOKEN-IE wire shape.
- [Wireshark `packet-iax2.c`](https://github.com/wireshark/wireshark/blob/master/epan/dissectors/packet-iax2.c)
  — byte-level IE parsing reference.
- [jaracil/iax (Go)](https://github.com/jaracil/iax) — state-machine
  cross-check; treats `IAXCtlCallToken = 0x28` as first-class.
