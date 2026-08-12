# Backlog

Durable backlog for **astar-lib**.

Session work goes in the **Claude Code task tracker** (`TaskCreate` / `TaskUpdate` /
`TaskList`): create a task before starting, mark it `in_progress` when you pick it up,
`completed` when it's done. Anything that outlives the session belongs in this file.

Migrated off the **beads** (`bd`) tracker on 2026-07-29 — do NOT invoke `bd`.
The 80 open items below each carry their full description and design text
inline. All 228 issues (164 of them closed) were exported to
`docs/issues-archive.jsonl`, which is gitignored and local-only; a committed copy of the
tracker's final state survives in git history at the migration commit.

## Open items (88)

### iax-c7e1 — Finish removing the CH34x dext: ONE HARDWARE STEP LEFT
*P1 high · task · labels: ptt, hardware, macos, docs, cx:1*

BLOCKS GOING PUBLIC (Rob, 2026-08-10): "before we make this public, I want to
remove the serial driver."

STATUS 2026-08-11 — steps 1 and 3 are DONE; only step 2 (Rob's hardware) is
left, and it no longer blocks the publish:

  - **Step 1 done.** `SerialConfig.transport` now defaults to `.usb`
    (`bindings/swift-serial/.../SerialClient.swift`). All three fallback sites
    inherit that single default, so `SerialLineSpec+Config` and
    `SerialSettings` needed only their comments corrected. Decision on existing
    configs: a config that NAMES a transport keeps it — nobody is migrated off
    `.tty` — only absent/unmapped falls through to `.usb`.
    `testDefaultTransportIsUsb` pins it. The Rust FFI doc was corrected too: it
    no longer calls `Tty` "the default", it calls it the zero value and warns
    that zero-init lands there. The C enum was NOT renumbered — that is an ABI
    break, and `Transport`'s Swift raw values mirror it.
  - **Step 3 done.** The cask is gone from the first-run path everywhere: the
    in-app onboarding checklist (`SerialView.swift`) no longer mentions a
    driver, `docs/site/macos/hardware.md` presents raw USB as the default and
    the tty path as a manual opt-in, and README + CLAUDE.md/AGENTS.md say the
    same. `guard-distribution-claims.sh` still allows the one cask because the
    tty path legitimately needs it.
  - **Step 2 still open** — see below. It is a verification, not a code change,
    and the shipped default no longer depends on its outcome.

Corrected state — an earlier version of this item claimed the dext was still
required. That was read off the CODE default and was wrong in practice:

  - astar's UCI150 hardware preset ALREADY selects raw-USB
    (`HardwareProfile.swift`: `transportRaw: 1  // raw-USB`, astar-f772).
  - Rob's own persisted config is ALREADY on it: `defaults read` shows
    `"serial.transport" = 1`.
  - So the running client does not use the tty path, and the dext
    (`cn.wch.CH34xVCPDriver 1.0/1`, at
    `/Library/SystemExtensions/697C0C39-.../`) is installed but unused by
    astar. `/dev/cu.wchusbserial5B210098201` exists only because the dext
    publishes it.

What is actually left:
  1. Flip the FALLBACK default from `.tty` to `.usb` so a fresh install never
     touches the tty path. Three places keep `.tty` for back-compatibility:
     `SerialClient.swift` (`var transport: Transport = .tty`),
     `SerialLineSpec+Config.swift` (nil/unmapped → .tty),
     `SerialSettings.swift` (absent persisted key → .tty). The Rust FFI
     (`astar-serial-sys/src/ffi.rs`) documents `Tty` as its default too.
     Decide whether existing `.tty` configs should be migrated or left alone.
  2. Uninstall the dext and VERIFY PTT still keys over raw-USB. This matters
     because `uci150_usb.rs`'s own module doc says "only the WCH *dext* must
     be absent" — yet Rob has been running raw-USB WITH the dext present. One
     of those two is wrong, and which one decides whether the uninstall is a
     no-op or a fix. Rob's hardware, Rob keys.
  3. Strip the CH34x cask from install docs, CLAUDE.md, and the new monorepo.

CAVEAT before uninstalling: the dext serves ALL WCH parts, not just the
UCI150's CH343. CH340/CH341 cables — the common radio-programming cable — are
not CDC-ACM compliant and need this vendor driver. Removing it may break
CHIRP-style programming cables. The CH343 in the UCI150 is CDC-ACM capable,
which is why raw-USB works for it. This is a per-machine trade, not a
consequence of astar's code.

### iax-3f7a — Flaky: m17_session dual-stack reflector test fails under parallel load
*P2 medium · bug · labels: test, m17, flaky, cx:2*

Observed 2026-08-11 during a `just ci-full` run on the Mac:

```
m17_session_connects_via_localhost_to_a_dual_stack_reflector
  panicked at crates/astar-console/tests/m17_session.rs:677:
  v4 client must be ACKN'd by the same dual-stack reflector:
  Os { code: 35, kind: WouldBlock, message: "Resource temporarily unavailable" }
```

Reproduction rate: **intermittent**. The same test passed 3/3 when run alone
immediately afterwards, and the full gate had passed three times earlier in the
same session. It fails only inside a full `cargo test --workspace` run, i.e.
under parallel load.

EAGAIN/`WouldBlock` on a UDP loopback read is a read timeout being hit, not a
protocol failure — the reflector's ACKN almost certainly arrived late rather
than never, because the machine was busy compiling and running the rest of the
workspace. So the bug is in the test's timing assumption, not in `astar-m17`.

Fix direction: give the receive a bounded retry loop against a deadline instead
of a single read with a fixed timeout, so a late ACKN still passes and a truly
missing one still fails. Check whether the other loopback/parrot tests in that
file share the same pattern — if so, fix them together.

Why it matters now: this is exactly the failure that will turn a public repo's
CI red at random and teach everyone to re-run the pipeline instead of reading
it.

### iax-9e4c — Confirm the hot-unplug hang fix against real hardware
*P1 high · task · labels: audio, hardware, cx:1*

iax-8d21 bounded every wait between a caller and the cpal stream thread,
which by code inspection accounts for all three symptoms Rob reported with
the UCI150 (dial hangs, Quit hangs, UI still paints). The mechanism has NOT
been confirmed against the physical failure — no sample of the hung process
was captured before the fix went in.

Confirm: unplug the UCI150 mid-session, replug, dial. Expect a dial that
fails within ~5s with a "disabled" device error instead of hanging, and a
Quit that always completes. If it still hangs, capture `sample astar 5` and
find which thread is blocked — the wedge may be somewhere the four bounded
waits don't cover (the serial worker, or CoreAudio enumeration inside
`CpalBackend::enumerate`, which is still unbounded by design since it does
not cross a thread boundary).

### iax-b8c1 — Surface vocoder TX health in the D-Star snapshot
*P2 medium · task · labels: dstar, ambe, cx:1*

Fallout from iax-2f6b (review minor 11, only partially fixed). If the
`HwAmbeStream` worker thread is gone, `submit_encode` now logs a warning but
still silently drops every frame handed to it: the operator sees PTT engage,
hears their own sidetone-free silence, and transmits nothing. The same hole
exists on the RX submit path.

Fix: add a health/`tx_capable` signal to `DstarSnapshotState` (and whatever
the RX side needs to match), fed by the worker's liveness, so a UI can grey
out PTT rather than offering a key that cannot produce audio. This is a
snapshot-contract change — new field, new semantics for something astar
reads to decide whether to offer PTT — so it wants its own design pass
rather than being bolted onto a review-fix round.

### iax-d9f4 — Enforce (not just document) the astar-server D-Star keying hazard
*P2 medium · task · labels: dstar, safety, cx:1*

`Station::set_ptt` is the single entry point that can key a transmitter and
is network-agnostic by design. `astar-server` exposes it remotely via
`POST /key` and a TUI keystroke. Today its `astar-station` dependency
takes default features only (no `dstar`), so D-Star cannot be keyed through
it — but Cargo feature unification means any workspace build that enables
`astar-station/dstar` for another crate turns `POST /key` into a remote
D-Star transmit trigger. iax-2f6b documented this in `station.rs` and
`astar-server/Cargo.toml`; a comment is not a guard.

Fix: make it structural. Options worth weighing — a runtime refusal in the
node's key path when the active session is D-Star, a `compile_error!`
tripwire, or splitting the remote-keyable surface away from `Station`.
Practical exposure is currently low (the node has no path to
`dstar_connect`, so no D-Star session can be active there), which is why
this is P2 and not urgent — but the hazard should not depend on that
staying true.

### iax-e5b2 — TX RPT1/RPT2 derivation unverified against a live reflector
*P2 medium · task · labels: dstar, protocol, cx:1*

iax-2f6b fills the TX RF header's repeater fields as `RPT2 = "<REF>
<module>"` (destination) and `RPT1 = "<REF> G"` (gateway), derived from the
host's first DNS label when `DstarConfig::reflector_callsign` is `None`.
That shape matches the captured XLX458 header and what xlxd's DPlus
`IsValidModule(rpt2.GetModule())` gates on, but no transmission has been
made to a live reflector to confirm it is accepted and attributed
correctly. Connecting by bare IP transmits with both fields blank (logged
as a warning).

Verify on air (Rob keys, never an agent): confirm the reflector dashboard
shows the transmission attributed to AJ7HR on the right module. Record the
result in `docs/research/research-dstar.md` §8 alongside the RX findings.

### iax-f7a3 — M17 parrot playback pacing test is flaky
*P3 low · bug · labels: m17, test, cx:1*

`astar-m17`'s `parrot_playback_packets_are_paced_34_to_48ms_apart_over_many_packets`
asserts every inter-packet gap lands in a 34-48 ms band around the 40 ms
target. On a loaded machine it fails routinely (observed 48.2 / 49.1 / 53.2
/ 62.3 / 72.4 ms) — on master as well as on branches, so it is not caused by
any current work, but it makes `cargo test --workspace` unreliable as a
merge gate.

Fix: assert the property that actually matters (mean cadence, or a
percentile bound, or a generous ceiling that still catches a genuinely
broken pacer) instead of a hard per-gap window that OS scheduling jitter
alone can breach.

### iax-3a5c — Loopback reflector should mimic real reflector laxity (zero header CRC)
*P2 medium · task · labels: dstar, test, cx:1*

Lesson from iax-7c19. Our loopback/parrot `Reflector` in astar-dstar
computes a correct RF-header CRC when it replays a stream. Real reflectors
do not: XLX458 (KC-Wide) sends `0x0000` in that field on every forwarded
header. Because our fixture was politer than reality, all 52 protocol tests
passed while the client could not decode a single live transmission — the
bug only surfaced on air.

Fix: have the parrot/relay path forward headers with a zeroed CRC by
default (optionally a flag to compute one), so the fixture reproduces the
peer behaviour clients actually meet. Add a regression test asserting the
replayed header's CRC trailer is zero and that the client still accepts it.
Generalise where cheap: a test reflector should reproduce real peers'
laxity, not its own idea of correctness.

### iax-c4f2 — D-Star reflector daemon (M1)
*P2 medium · feature · labels: dstar, reflector, cx:2*

Promote `astar-dstar`'s loopback/parrot `Reflector` (test-only today)
into a runnable daemon: a binary with host/port/module args and a `just
dstar-reflector` recipe, mirroring the M17 mrefd parrot dev loop. Gives
astar and `dstar-listen` a local reflector to test against without touching
live networks. Blocked on nothing; spec when picked up. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-d5a1 — D-Star reflector: multi-client, multi-module (M2)
*P2 medium · feature · labels: dstar, reflector, cx:3*

Grow the M1 daemon (iax-c4f2) into a real linkable reflector: several
clients per module, modules A-Z, link/callsign policy, status output,
eviction and capacity limits. Spec when picked up. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-e7c3 — D-Star transcoding bridge via ThumbDV (M3)
*P2 medium · feature · labels: dstar, reflector, thumbdv, m17, cx:4*

The ThumbDV payoff: bridge D-Star into M17/AllStar — ThumbDV decodes AMBE,
the engine re-encodes to Codec 2 / G.711, and back. One dongle suffices
because D-Star conferences are half-duplex (one talker per module), BUT the
budget is tight: pipelined decode plus encode is ~15 ms of the 20 ms frame,
so one bridged stream per dongle is the design limit. M3 must measure this
before committing to a shape. Blocked on iax-b3e7 (pipelined vocoder) and
iax-d5a1. Roadmap context:
`../astar/docs/superpowers/specs/2026-08-08-thumbdv-realtime-dstar-design.md`.

### iax-5562 — Unbuffered SSE: flush /events per frame (status page lags ~3 s)
*P1 high · bug · labels: node, http, cx:2*

Found during iax-24e2's Task 5 smoke and confirmed by its final review:
`tiny_http`'s default `chunked_transfer::Encoder` (8192-byte buffer, no
per-write flush) batches the 33 ms / ~90-byte SSE heartbeat frames on
`GET /events`, so browsers see ~3 s of blank page before the first live
update and every event (including the `rx_active` "talking" pill) lags up
to ~3 s behind the wire. Predates iax-24e2 (the SSE endpoint shipped with
iax-d829.1); the status page is its first real consumer. Fix shape (from
the Task 5 diagnosis): serve `/events` in `astar-server`'s server layer
via `Request::into_writer()` / `upgrade()` writing `sse_frame` output with
an explicit `.flush()` per frame, instead of wrapping `SseReader` in
`tiny_http::Response`'s `Read` adapter. Test: bound-server integration
test asserting the first `data:` frame arrives within a sub-second
deadline. Immediate next item after iax-24e2 per its final review ruling
(the buffering undercuts the live premise of the status page). Detail in
git history: iax-24e2 Task 5 report (SDD workspace, now deleted).

### iax-c1d5 — Error transparency: carry engine error detail across the ABI; fix lossy M17 error mapping
*P2 med · task · labels: ffi, ux, cx:3*

Rob 2026-08-04, after a useless "astarstation error -8: iax error" from an
M17 localhost dial: (1) the engine's rich error detail (connection refused,
resolve failure with hostname, dial-stage context) never crosses the C ABI —
only the numeric code does. Add a last-error-text accessor
(`iax_station_last_error_text(st, buf, cap)` using the fill_buf pattern;
secret-free by construction) populated on every failed call, plus Swift/
Python surfacing, so UIs can show the real reason. (2) Fix the lossy M17
error mapping found reproducing this: an M17 connect resolve failure maps to
`StationError::Iax("resolve")` (-8) instead of `Resolve` (-6) — audit
`map_console_err`/the m17 arms so every ConsoleError variant maps to its
matching StationError. Pairs with astar-0217 (friendly connect-failure
messages), which should consume the new text. Also fold in (review-parked 2026-08-04): the m17 connect candidate-iteration loop discards the real io::Error on the all-candidates-fail path (m17.rs connect_udp_socket), synthesizing a misleading could-not-resolve message — surface the last real failure instead.

### iax-a4e7 — Output AGC: auto gain for quiet stations on RX
*P2 med · feature · labels: audio, dsp, cx:3*

Rob 2026-08-03, live on M17-KCW: one participant far quieter than the rest —
wants automatic gain on the OUTPUT/RX path so quiet stations get lifted
toward a target level (slow attack/release AGC or loudness normalization,
per-bus in the AudioRouter output path, network-agnostic so AllStar + M17
both benefit). Surface an on/off + target-level knob through
Station/FFI/bindings; default OFF (byte-identical rule).

PHASE 1 (Rob 2026-08-04, do first) SHIPPED 2026-08-04 on the Rust/FFI/Swift
side: reuses the mic-path `Compressor` (makeup gain included) on the
`OutputBus`, applied BEFORE the bus gain multiply in `OutputBus::read` so the
4x output-gain range amplifies the leveled signal — `rx_compress`/
`rx_compress_level` threaded AudioRouter → Manager → ConsoleSession (standing
prefs, re-pushed on (re)connect, now a 10-pref re-push) → M17Session (prefs +
live setters + `applied_rx_compress`/`applied_rx_compress_level` readback) →
Station (`set_rx_compression`/`set_rx_compression_level`) → FFI
(`iax_station_set_rx_compression`/`_level`) → Swift
(`setRxCompression`/`setRxCompressionLevel`). Default OFF everywhere
(byte-identical rule verified by a router-level regression test). Python
bindings intentionally NOT extended (matches existing lockstep gap: Python
already lacks `tx_trim`/`vox_preroll_ms`/`spectrum_decay` setters too).
STILL OPEN from Phase 1: Quick settings UI on both platforms (astar's
`QuickConfigView.swift` speaker card, gui-rs's `speaker_card` in
`gui-rs/src/view/config.rs`) — the Rust/FFI/bindings plumbing is ready for it
but no UI wiring was done (out of scope for the engine-only task that shipped
this). PHASE 2 (original ask): full auto-target AGC. Design pass needed
(attack/release constants, clipping guard, interaction with the existing
output gain + half-duplex mute).

RE-RAISED by Rob 2026-08-08, same symptom on M17/KC-Wide ("some signals are
several dB lower than what should be transmitted"), which confirms Phase 1's
fixed-ratio compressor is not enough on its own — it lifts a quiet talker
only within its threshold/makeup envelope and does not converge different
talkers onto a common target. Phase 1's UI has since landed on astar
(QuickConfigView.swift:159 "RX compression" toggle + level slider), so the
interim mitigation is available; Phase 2 is what actually equalises talker-
to-talker loudness. Wants its own brainstorm/spec cycle: per-talker vs
per-bus adaptation (M17 stream boundaries give natural per-talker resets),
target loudness units (LUFS-style vs peak), attack/release, clipping guard,
and how it composes with rx_compress rather than fighting it.

DATA POINT (Rob, 2026-08-09, listening to a live KC-Wide QSO on D-Star via
the ThumbDV): D-Star RX "sounded really close to M17" on the same network.
The D-Star decode path has NO compressor or gain stage in front of it at
all — raw AMBE out of the vocoder straight to the output bus — so a mode
that is entirely unprocessed lands at about the same perceived level as the
processed M17 path. That argues the problem Phase 2 must solve is
TALKER-TO-TALKER variance, not a per-mode level offset, and that a
per-talker adapting target (reset on stream boundaries, which both M17 and
D-Star provide cleanly) is the right shape. It also means D-Star is a
useful control when evaluating any AGC: unprocessed audio on the same
network, same listener, same session.

### iax-e2c8 — M17 client hardening (post-KC-Wide follow-ups)
*P3 low · task · labels: m17, cx:3*

Consolidated from the iax-f2b8 final review + task-review deferrals: (1)
RX jitter/reorder stage (JitterBuf<[u8;16]> or minimal drop-older-than-
last-played guard) — top item before heavy real-Internet use; (2) TX burst
pacing (~50 ms recv-blocked bursts vs smooth 40 ms cadence); (3)
`encode_callsign` post-uppercase length/overflow guards (ß→SS expansion);
(4) `M17Config::default` / public DEFAULT_KEEPALIVE const; (5) evaluate two
codec instances for duplex (likely fine as-is); (6) mirror `input_level_db`
while M17 active so VOX can drive M17 PTT (astar UI half rides
astar-6f1b); (7) distinguish NACK-refused vs link-timeout in status text;
(8) key-while-Connecting sends pre-link stream packets (note); (9) m17 teardown joins the run-loop thread under the session lock on the disconnect hot path (bounded ~50-100 ms; split off-lock like the IAX2 detach/hangup convention — parked from the fix-wave re-review 2026-08-03).

### iax-d3b6 — M17 reflector daemon hardening
*P4 backlog · task · labels: m17, cx:3*

The in-engine reflector is test-harness-grade (spec §7 scope-out). Before
any standalone daemon use: configurable reflector callsign (hard-coded
"M17-REF"), re-CONN liveness edge, single-parse of control packets, the two
racy client_count test asserts, dashboard/logging, LSTN (listen-only)
support. (The stale "(no I/O)" Cargo.toml description was fixed alongside
iax-91f4's parrot mode.)

### iax-b5e1 — astarserial.h cbindgen drift (pre-existing, iax-239a era)
*P3 low · bug · labels: serial, cx:1*

Found during iax-f2b8 Task 5: `check-cbindgen-serial` is red on master —
`astar-serial-sys`'s committed header has drifted from its Rust source
since the iax-239a worker-thread work. Regen, diff-audit (ABI-affecting or
comment-only?), and add the serial check to the ci recipe if it isn't there.

### iax-e5d9 — Clean-room Rust Codec 2 (permissively licensed) — the iOS M17 gate
*P4 backlog · feature · labels: m17, codec, cx:5*

Ports of libcodec2 (incl. the Rust codec2 crate) are derivative → LGPL, and
LGPL is effectively incompatible with App Store distribution — so M17 on
iOS needs a clean-room Codec 2 implemented from David Rowe's papers/blog
DSP descriptions, NOT the C source, validated by golden vectors +
cross-decode interop against libcodec2 both directions + listening tests.
3200 mode first. Slots in behind iax-f2b8's CodecProvider seam as a drop-in.
Does not block anything else; parallelizable.

### iax-d7e3 — D-Star engine backend: XLX/XRF reflector linking (DExtra/DCS)
*P4 backlog · feature · labels: dstar, protocol, cx:5*

Per Rob 2026-08-03 (astar-d5a7). HARD GATE FIRST: D-Star voice is AMBE —
no freely-licensable software vocoder exists (options: AMBE hardware
dongle via USB, or mbelib which is patent-encumbered/legally gray). The
design must resolve the vocoder question before any protocol work.
Protocol side is tractable: DExtra/DCS linking to XLX/XRF reflectors
(KC-Wide: XLX458/XRF458 module A), behind the CodecProvider + Station
capability pattern from iax-f2b8.

### iax-e8a4 — YSF engine backend: YSFReflector protocol (C4FM)
*P4 backlog · feature · labels: ysf, protocol, cx:5*

Per Rob 2026-08-03 (astar-e7b3). Same AMBE+2 vocoder gate as iax-d7e3 —
resolve first. Protocol: YSFReflector UDP (KC-Wide: US-KCWIDE YSF32453 /
US-XLX458 YSF28054); also the practical bridge into Wires-X rooms (native
Wires-X is closed — see astar-a3c9, which deliberately has no engine item).
CodecProvider + capability pattern from iax-f2b8.

### iax-b9c2 — NXDN engine backend: NXDNReflector protocol
*P4 backlog · feature · labels: nxdn, protocol, cx:4*

Per Rob 2026-08-03 (astar-b8e4). Same AMBE+2 vocoder gate. Protocol:
NXDNReflector UDP talkgroups (KC-Wide: TG 31313). CodecProvider +
capability pattern from iax-f2b8.

### iax-c9f4 — Hams Over IP engine backend: SIP/RTP client (G.711)
*P3 low · feature · labels: hoip, protocol, cx:4*

Per Rob 2026-08-03 (astar-f1c6). SIP registration + RTP audio with G.711 —
the engine already speaks G.711, so NO vocoder gate: after M17 this is
likely the cheapest new network. Needs a SIP stack decision (rust crate vs
minimal hand-rolled REGISTER/INVITE for the HoIP use-case), account
credentials via the SecretResolver pattern, Station capability flag.
Target: KC-Wide extension 15135.

### iax-b3d7 — SvxReflector client ("hamlink"): TCP control + UDP/Opus audio behind the Station contract
*P3 low · feature · labels: hamlink, protocol, cx:5*

astar's hamlink network (astar-9b3e; spec in
astar/docs/superpowers/specs/2026-08-02-network-switcher-design.md): a native
SvxReflector protocol client so the phone/desktop client can join the same
reflector a SvxLink-driven HF or VHF/UHF repeater sits on. TCP control
channel (auth, talkgroup select) + UDP audio (Opus), surfaced through
Station/ConsoleSession behind the existing poll + snapshot contract — no
callbacks — plus a capability flag the astar UI reads to make the `hamlink`
network available (`Network.available`). One active connection at a time,
shared PTT/meters. Long-term both this native path AND an AllStar
bridge-node path stay on the table (Rob, 2026-08-02); M17/DMR/D-Star
reflector families would follow the same capability pattern. Undesigned
beyond the seam — full protocol design when this item is picked up.

### iax-9c41 — In-band DTMF reports success when the tone can never transmit
*P1 high · bug · labels: dtmf, audio, announce, cx:2*

Diagnosed live 2026-08-02 while chasing "`*355553` does nothing from
astar". `Station::send_dtmf` (in-band mode) plans the tone and hands it to
the announce path, which returns `Ok(handle)` — but the announce TX lane is
clocked by MICROPHONE CAPTURE CALLBACKS. With no capture running (a
`NullBackend` station, a muted/denied/absent input device, or a call with
no mic routed), the tone sits queued forever and ZERO voice frames reach
the wire, while every layer up to the UI reports the digit as sent. Proven
with a frame-observer probe against the live hub: `announce() -> Ok`,
outbound voice frames during the whole announce window = 0; swapping the
same client to `CpalBackend` made the identical digit sequence arrive and
drive the hub's `*3` link command.

This silently breaks EVERY in-band DTMF consumer (astar's keypad, the
iax-4b7a sequencer) whenever capture isn't live, and it cost hours of
mis-attributed debugging — the failure is indistinguishable from a
far-end decode problem.

WHAT:
1. Fail loudly: `send_dtmf`/`send_dtmf_for`/`send_dtmf_string` should
   return an error (extend `AnnounceUnavailable`, or a new
   `StationError::NoCaptureClock`) when the active call has no routed mic
   or the capture stream is not delivering — rather than `Ok(())`.
2. Consider decoupling: drive announce-queue drain from a timer/pump tick
   instead of the capture callback so a tone can transmit on a
   capture-less station (a node daemon injecting audio has the same
   problem). Keep byte-identical behavior when capture IS live.
3. Surface it: a snapshot/telemetry bit ("tx lane not clocked") so a UI can
   show why nothing is going out.

VERIFY: a station with a silent/absent input device gets an Err from
`send_dtmf` (unit test with the existing `NullBackend`); with a clocked
backend the tone still transmits byte-identically (frame-count test).
Related: iax-8d2f (the pitch half of this bug, fixed 2026-08-02).

### iax-7e10 — Manager emits no LinkEvent for one-shot links (SSE link stream silent)
*P2 med · bug · labels: linking, node, cx:2*

During the live iax-d829.1 `/link` exercise (2026-08-02, VPS hub → local
Asterisk echo service) a link went connecting → up → disconnected and the
node's SSE stream carried ONLY 1/s snapshots — zero
`{"event":"link", ...}` edges. The consumer path is fine (the node's
`drain_link_events` → `link_event_to_node_events` → broadcast is
unit-tested); the gap is on the producer side: `LinkEvent::Connected` /
`Disconnected` are only emitted by permanent-link supervision in
`Manager::tick` (which the node doesn't call), and `Keyed` only by
`key_link`/`unkey_link`. A plain one-shot link reaching Active — or ending
— emits nothing. Emit Connected when a link's call transitions to Active
and Disconnected (with the hangup reason) when it ends, for ALL links, in
whatever pump path the Manager already owns; then a node SSE integration
test asserting the edges arrive around a fake link's lifecycle. Roster
polling via snapshots masks this today, which is why /status watchers see
links but event subscribers don't.

### iax-3f34 — Expose live-call RX + TX spectrum over the C-ABI + Swift binding (rxSpectrum/txSpectrum) — unblocks astar FFT
*P1 high · task · labels: binding, cx:1*

**Design:** WHY
astar's in-call FFT dropdown needs the live-call RX + TX spectrum, but the Station C-ABI + Swift binding only exposes micSpectrum (the no-call mic monitor). The Rust engine ALREADY has the methods from iax-2b09 — Station::tx_spectrum(&self, out: &mut [f32]) -> usize (crates/astar-station/src/station.rs:839) and Station::rx_spectrum (station.rs:848), tapping the active network call. They just aren't surfaced through the binding astar consumes (pp-c2bb). This is the focused binding-exposure slice (analogous to iax-0e9b for DTMF).

WHAT — mirror the existing mic_spectrum plumbing for tx + rx:
1. C-ABI (crates/astar-sys/src/ffi.rs): add iax_station_tx_spectrum and iax_station_rx_spectrum, mirroring iax_station_mic_spectrum exactly (same float *out + uintptr_t cap, returns count, IAX_SPECTRUM_BINS contract, NULL/panic guards). Wire to Station::tx_spectrum / rx_spectrum.
2. Swift wrapper (bindings/swift/Sources/AstarStation/Station.swift): add public func txSpectrum() throws -> [Float] and rxSpectrum() throws -> [Float], mirroring micSpectrum() (same buffer alloc + IAX_SPECTRUM_BINS + status->error mapping). Poll-only; no callbacks; honors pp-1df2.
3. Regenerate the cbindgen header (crates/astar-sys/include/astar.h) via scripts/check-cbindgen.sh / cbindgen so header-drift passes.
4. Tests: mirror the mic_spectrum FFI + station tests for tx/rx — on an active call the spectrum copies bins; idle/no-active-call returns 0/empty gracefully (no error).

VERIFY
- just ci green (fmt, clippy -D warnings, tests, cbindgen).
- After merge + astar re-vendor (just vendor + rebuilt xcframework), Station.swift exposes txSpectrum()/rxSpectrum().

DELIVERABLE FOR ASTAR: a vendored binding with public func txSpectrum()/rxSpectrum() throws -> [Float] returning the live-call dBFS bins (same format as micSpectrum).

### iax-5088 — astar-inspect: protocol inspector / conformance harness with expected-vs-actual frame diffing
*P1 high · feature · labels: cx:2*

Build a web/human + agent testing framework that drives the IAX2 FSM and shows every frame on the wire annotated with expected-vs-actual verdicts. Seeded by the throwaway crates/astar-iax/examples/probe.rs. Primary near-term payoff: it IS the debugger for iax-3fca (the ASL3 web-transceiver connection gap) -- first scenario is the WT handshake.

LOCKED DECISIONS (brainstorm 2026-06-10):
- Q1 intent: value-first -- ship a thin tracing core + verdict engine that immediately debugs iax-3fca, designed so web UI and agent interface layer on top. Not a throwaway, not a big-bang suite.
- Q2 frame sources: live UDP, recorded replay, and in-process fake-peer are CO-EQUAL, chosen per-run (--source live|replay|fake-peer). Live is real traffic against a real node (e.g. parrot 55553) -- deliberate, not constant.
- Q3 agent interface: CLI emitting JSON. Command shape: "iax-inspect run --source ... --scenario ... --json" prints the full trace to stdout; agents shell out + parse. Plus a --pretty human terminal mode (promoted probe.rs).
- Q4 human web view: static self-contained HTML report NOW (--report out.html bakes timeline+hex+decode+verdicts into one file); live local web server LATER (deferred phase).

ARCHITECTURE: new tooling crate named astar-inspect (NOT in astar-iax-core -- keeps core dependency-free). The new crate may use serde/serde_json freely; NOTE there is currently NO serde anywhere in the workspace, so the inspector defines small OWNED TraceEvent mirror types because the core frame types borrow &str/&[u8] and cannot derive Serialize. Three layers over one trace model:
1. Tracing core: wraps the existing Fsm+Reliability drive loop (same handle->dispatch->on_frame_in->tick cycle as run_loop/probe.rs); records Vec<TraceEvent>{seq, t_ms, direction Tx/Rx, raw_hex, decoded(frame+IEs), fsm_state_before/after, actions, verdict}.
2. Expectation engine: per FSM state classify each frame Expected | Unexpected | Divergent{field, expected, actual}. Outbound diffs vs the DroidStar web-transceiver golden (the shape we PROVED works against 55553); inbound vs the FSM state-transition table.
3. Front-ends over the trace: CLI/JSON (agents), --pretty terminal (humans now), static HTML report (humans now), live web server (later).

REUSE: frame.rs (Frame/FullFrame/MiniFrame, parse/parse_lenient/encode -- all Debug), ie.rs Ies (~60 typed fields), subclass.rs enums. Drive-loop reference: client.rs run_loop + astar-conformance driver.rs + examples/probe.rs.

PROPOSED PLAN DECOMPOSITION: (1) tracing core + owned model + fake-peer source; (2) expectation engine; (3) CLI/JSON + pretty output; (4) live + replay sources; (5) static HTML report; (6) ASL3 WT scenario wired end-to-end. Spec to be written to docs/superpowers/specs/.

### iax-64f0 — Session FSM parity vs C iax.c (pcap-driven)
*P1 high · task · labels: cx:3, tests, tests,session,protocol*
**Blocked by:** iax-7022 (closed)

**Design:** Validate the Rust session FSM emits the same outbound frame sequence as the vendored C iax.c, given the same inbound frame sequence. Uses the pcap fixtures captured for iax-7022. For each recorded scenario (register, NEW+CALLTOKEN+AUTHREQ+AUTHREP+ACCEPT, hangup, …): replay the peer→client frames into the Rust FSM, assert the FSM's emitted client→peer frames match the originals in the pcap modulo non-deterministic fields (call numbers, timestamps, MD5 challenge response is determined by challenge so structurally check). Anything that diverges is either a Rust bug or an intentional deviation that gets documented.

### iax-b1fe — RX noise reduction: adaptive NR on the output bus (post-mix, toggle)
*P1 high · feature · labels: cx:3*

**Design:** WHAT: RX noise reduction - one adaptive NoiseReducer on the mixed output path, toggle default OFF, mirroring the mic-NR plumbing (set_noise_reduction) at every layer.
PLACEMENT: OutputBus (crates/astar-audio/src/router.rs ~line 820) owns nr: NoiseReducer (bus sample rate) + rx_denoise: Arc<AtomicBool> (default false) + accessor. Processing order in OutputBus::read: mix -> NR -> rx_gain -> rx_peak meter -> rx_spectrum (NR before gain so volume does not disturb the noise estimate; meter/spectrum after so they show what is heard).
PLUMBING: Router::set_output_denoise (setter only); console session persists the flag across reconnects + re-pushes on connect (pattern: compression level / tx trim, see iax-750a); Station::set_rx_noise_reduction(bool); C-ABI iax_station_set_rx_noise_reduction (pattern: iax_station_set_noise_reduction) + cbindgen header regen (scripts/check-cbindgen.sh).
FUTURE (deferred by Rob, do NOT build now): per-call NoiseReducer before the mixer if multi-call monitoring shows blended-floor artifacts - keep the control plumbing shaped so that variant can slot in behind the same single user toggle.
Design doc: astar repo docs/superpowers/specs/2026-07-06-rx-noise-reduction-design.md.
VERIFY: TDD. Tests: NR measurably reduces a synthetic noise floor post-mix; toggle off = bit-exact passthrough; rx_peak reads post-NR level; session flag survives reconnect. cargo test/clippy/fmt + header check green. Consumed by astar-70b9.

### iax-d829 — EPIC: Node link-control parity with AllStar (DTMF *3/*2/*1 + node-to-node linking)
*P1 high · epic · labels: astar, cx:2, dtmf, linking, node*

**Design:** Make astar-server link to/from other AllStar nodes the way a real AllStar node does, driven by AllStar's ilink DTMF control surface.

GOAL / ACCEPTANCE
- *3<node> connects (full two-way), *2<node> monitors (RX-only, mic withheld), *1<node> disconnects.
- Driven BOTH via the node's HTTP control channel AND via real in-band DTMF decoded from radio/mic audio.
- CALLTOKEN + per-node link auth verified (token requested on the link dial path, dest_call=0 on resend; inbound link issues/validates token + auth).
- Each child merges on harness/conformance proof (astar-conformance + local Asterisk/ASL3 podman); a final live-AllStar bring-up over the WireGuard tunnel closes the epic.

WHAT ALREADY EXISTS (do not rebuild)
- Library link substrate is implemented in Manager: connect_link / disconnect_link / link_roster / link_events, transceive<->monitor (crates/astar-iax/src/{manager,link,link_control}.rs; design iax-42e9).
- The GAP is at the node-daemon edge: NodeCommand + control HTTP only do single-call Dial/Hangup; no link control, no DTMF->link mapping, no in-band decode wired in, no link-auth conformance, no live bring-up.

DECOMPOSITION (children, depend-on existing Station/relay/reachability nuggets rather than duplicate):
 1. Node link-control over HTTP (the testable MVP spine) -- START HERE.
 2. DTMF -> link command mapper (site-configurable ilink map).
 3. In-band DTMF decode into the node (integrates iax-d111 + iax-3746).
 4. CALLTOKEN + node-to-node link auth conformance (astar-conformance).
 5. Harness: link FSM + *3/*2/*1 transition tests vs local Asterisk/ASL3.
 6. Live AllStar bring-up over the WireGuard tunnel (absorbs iax-be48) -- closes the epic.

Linked deps (not children): iax-3746, iax-d111 (feed #3); iax-42ce (N-way relay, after 1:1); iax-be48 (feeds #6); iax-b764 POKE (optional self-check for #6).

Reference: docs/allstar-interop.md (DNS resolution, IAX2 exchange + dest_call=0 rule, link modes & DTMF table, NAT/CGNAT).

### iax-dd42 — astar-console + web operator console (Tauri-reusable call engine)
*P1 high · feature · labels: astar, console, cx:8, ui*

Reusable astar-console library + web operator console harness. Design: docs/superpowers/specs/2026-06-10-astar-console-operator-console-design.md

PRIMARY DRIVER: the downstream astar app is a Tauri app (Rust backend + webview). Its call engine is this console core; its webview is this meter/PTT/status surface. So the core must be a clean, reusable, runtime-agnostic (no-tokio) library, and the web frontend must lift into the Tauri webview.

Crate split: (1) crates/astar-console LIB (reusable, dep-light) — ConsoleSession (connect WT call, set_ptt, disconnect, list_devices, snapshot), MeteringBackend audio-decorator (TX+RX RMS to dBFS via Arc<AtomicU32>, zero core changes), ConsoleConfig, ConsoleState (single source of truth), serde feature-gated off by default. astar depends only on this. (2) crates/astar-inspect BIN (harness) — tiny_http + SSE web server bound to 127.0.0.1, vanilla HTML/CSS/JS frontend split into transport-agnostic render(state)/sendCommand + a thin SSE/fetch adapter (Tauri swaps invoke/listen). 

First build = operator console only: node/secret/input/output fields, live TX+RX meters, red/green PTT light (press-and-hold), call status (Answered/rtt/Hangup). Built on the iax-3fca WT call shape.

Phasing: P1 console lib (fully tested vs the existing fake WT peer + null backend, offline), P2 web front-end, P3 (later) TUI ratatui, P4 (later) frame inspector = the original iax-5088 expected-vs-actual diffing.

New deps (gated): tiny_http, serde_json (inspect bin); serde optional (console lib). None in core/iaxclient. Tests offline/deterministic; live parrot run is operator-driven. Lineage: evolved from iax-5088; unblocked by iax-3fca (done).

### iax-f56c — uci150_usb backend hard-codes active-low; CTS/active-high setups read inverted
*P1 high · bug · labels: cx:2, ptt*

**Design:** The raw-USB UCI150 PTT backend (crates/astar-ptt/src/uci150_usb.rs) assumes ALL modem-status lines are ACTIVE-LOW:

    fn decode_status(raw, line) -> bool { (!raw) & bit != 0 }   // asserted when bit==0

This was correct for the BENCH unit (DCD active-low: idle 0xff -> keyed 0xf7). But the line AND its polarity are MicPTT-Dest-switch / wiring dependent. A common AllStar UCI150 config drives PTT on CTS, ACTIVE-HIGH — which this backend would read INVERTED (keyed reported as idle and vice-versa).

FIX: make polarity selectable alongside the existing KeyLine selection.
- Add an enum/flag (e.g. Polarity::ActiveHigh | ActiveLow) to Uci150Usb::open_with and the ModemPort/decode path.
- decode: ActiveHigh -> (raw & bit != 0); ActiveLow -> (!raw & bit != 0).
- Keep the bit map fixed (CTS=bit0, DSR=bit1, RI=bit2, DCD=bit3 — CH341 status register, req 0x95 / wValue 0x0706).
- Default: pick the AllStar-common default (CTS / active-high) OR keep DCD/active-low but make both trivially overridable; document that the operator must match their MicPTT Dest switch.
- Consider a tiny diagnostic (print the live raw byte) so users can determine line+polarity empirically.

Surfaced while writing the astar implementation guide (astar-fc4e): operator confirmed their AllStar box is CTS / active-high, opposite the bench DCD / active-low. Same correction is needed in astar`s Swift reimpl.

### iax-031c — Standalone audio-DSP library binding: C-ABI + PyO3/numpy (IAX2-free) exposing DTMF/CTCSS/NR/compression/mic-profile
*P2 medium · feature · labels: audio, cx:2, dsp, ffi, python*
**Blocked by:** iax-dedb (closed), iax-e549

**Design:** Package astar-audio + the DSP parts of astar-codec (DTMF) behind their own binding so non-IAX2 consumers (Python, etc.) use the DSP on raw PCM buffers, independent of the call engine. astar-audio and astar-codec already have ZERO dependency on iaxclient/-core; a CI check must keep them call-engine-free.
Binding tech DECIDED: C-ABI core (uniform with iax-dedb's Station FFI strategy; serves C/Swift/.NET) PLUS a thin PyO3/numpy wrapper over the same Rust core for an idiomatic pip-installable Python module with zero-copy numpy arrays. This is safe here (unlike the Station FFI) because the DSP path is pure buffer-in/buffer-out: no callbacks, no long-lived managed runtime.
Surface over PCM (f32/i16) buffers: DTMF generate (digits->samples) + Goertzel decode (samples->digits); CTCSS encode/decode (from the CTCSS ticket); noise reduction; dynamic-range compression; mic characterization -> MicProfile. Config structs (e.g. MicProfile) cross as secret-free JSON, reusing the existing serde models.

### iax-08d8 — Harness: codec negotiation view + select (show negotiated/offered, force preferred)
*P2 medium · task · labels: cx:2*

**Design:** Add codec visibility + selection to the inspect harness. Show which audio codec the call negotiated and what the peer offered; allow forcing/preferring a codec before/at connect (e.g. uLaw/GSM/Speex per build).

SCOPE:
- Display negotiated codec + offered/allowed caps for the active call (from the core capability negotiation).
- Connect-form control to set preferred/allowed codec mask before dialing.
- Reflect mid-call codec if it changes.
SOURCE: astar-codec + core capability/format fields. Verify what the FSM exposes for negotiated format and allowed caps; thread into Station snapshot + the connect path.
UI: codec shown in call status; selector on the connect form (and/or a Codec sub-panel).

OPEN: confirm the core exposes negotiated format + cap mask; if not, add accessors. Coordinate with vendor-neutral IAX2 stance (codec is a call param, not AllStar policy).

### iax-12fb — Node inbound authentication (auth=Required + per-caller secret map): the deployed hub runs listener auth=off …
*P2 medium · task · labels: cx:2, node, security*

Node inbound authentication (auth=Required + per-caller secret map): the deployed hub runs listener auth=off. iax-8baf implemented server-side MD5 challenge/validate, but node.toml has no credentials-map wiring and the estate default is open. Add a [listener] credentials/secret surface to node config (secret-free: runtime/vault only, never in the config struct/export/logs), wire it to IncomingCallPolicy auth=Required, and flip the gh-runners hub to authenticated. This is the last unbuilt piece of the iax-6461 asterisk-lite MVP (D2).

### iax-1ea2 — Harness: per-stream audio level/VU meters + gain/AGC controls
*P2 medium · task · labels: cx:2*

**Design:** Expose per-stream audio level/VU meters and gain/AGC controls in the inspect harness. The harness already has coarse rx_level_db/tx meters; this adds finer per-stream metering and input/output gain + AGC control surfaced from the audio path.

SCOPE:
- Input (mic) and output (speaker) VU/peak meters, separate from the call's rx/tx level.
- Gain sliders for input and output; AGC on/off if the audio backend supports it.
- Persist/restore via the existing harness config where sensible.
SOURCE: astar-audio (CpalBackend) — verify what level/gain/AGC hooks exist; some may need adding to the audio backend trait.
UI: a Levels/Audio panel (or fold into an existing tab); live meters via the existing poll/SSE.

OPEN: does CpalBackend expose gain/AGC today? If not, scope the backend additions separately. Relates to UCI150 mic-input/monitor-output device selection ([[project_uci150_dry_run]]).

### iax-2047 — Harness: surface call/network quality stats (jitter buffer, loss, RTT, OOO) in Network tab
*P2 medium · task · labels: cx:2*

**Design:** Populate the inspect harness Network tab with live call/network quality stats from the iax library. Currently the harness shows TX/RX level meters and call phase, but not transport quality.

SCOPE — surface (read-only) per active call:
- jitter buffer depth / target, jitter estimate
- packet loss (rx/tx), out-of-order count, dropped/late frames
- round-trip time / ping
- frames sent/received, bytes
SOURCE: whatever astar-iax-core exposes (or needs to expose) as a stats snapshot — verify the available fields first; some may need plumbing from the core session into Station::snapshot.
UI: Network tab table, polled or via SSE alongside the existing meters.

PREREQ/OPEN: audit astar-iax-core for an existing netstats struct; if absent, add a stats accessor on the session and thread it through Station. May depend on core work.

### iax-21c8 — astar-sys: C-ABI compatibility shim for astar drop-in
*P2 medium · feature · labels: api, cx:5, ffi, migration*
**Blocked by:** iax-802e (closed)

**Design:** Thin C-ABI layer that re-exports the iaxc_* symbols astar already calls, so astar/crates/astar-astar-sys can swap its build.rs to point at our static lib without astar-core changing.

ABI surface (from astar/crates/astar-core/src/client.rs — full audit needed before locking):
  - iaxc_initialize / iaxc_shutdown
  - iaxc_register
  - iaxc_call / iaxc_hangup
  - iaxc_input_level_set / iaxc_output_level_set
  - iaxc_send_dtmf
  - iaxc_set_audio_prefs
  - iaxc_set_event_callback
  - iaxc_audio_devices_get
  - (plus the IAXC_CALL_STATE_* constants and event struct layouts)

NOT in scope: every iaxc_* symbol upstream defines. Only what astar actually links — keep surface small.

Acceptance:
  - astar/crates/astar-astar-sys/build.rs points at our libiaxclient.a
  - astar-app builds + runs without changes to astar-core
  - All astar features that worked with patched C continue to work

References:
  - astar/crates/astar-astar-sys/wrapper.h
  - astar/crates/astar-core/src/client.rs + ffi_glue.rs

### iax-28b3 — iaxclient-devices: interface/handset capability model + catalog + RigConfig resolver
*P2 medium · feature · labels: cx:5, device, handset, station*
**Blocked by:** iax-dedb (closed)

**Design:** New crate `iaxclient-devices`. Guided two-step device/handset selection that resolves to a secret-free config Station consumes. Brainstorm 2026-06-14.

Model (interface = transports, handset = signals):
- Transport: audio-in, audio-out, serial port, CM108 HID line.
- InterfaceProfile: name, detection signature, transports provided, default audio in/out matchers, PTT transport.
- HandsetProfile: name, DTMF source (InBandAudio | Serial | SoftwareDialpad), PTT trigger (serial CTS | VOX | software), required transports.
- Rig/RigConfig: resolved (interface + handset + overrides).

Two-step flow: pick interface -> compatible_handsets(iface) (filtered by required transports) -> resolve(iface, handset, overrides) -> RigConfig.

Resolution/merge:
- audio in/out: interface device matchers, overridable.
- PTT: interface supplies the backend (Uci150Serial / Cm108Hid / none, from astar-ptt); handset supplies the trigger over the existing PttBridge.
- DTMF source: handset-driven -> InBandAudio activates the mic-capture decoder (iax-d111), Serial activates the serial DTMF source (iax-5a5c) on the interface serial port, SoftwareDialpad -> UI dialpad only.

Config in/out (NOT persistence): RigConfig is serde-serializable. The crate INGESTS a config (deserialize -> resolve) and EMITS one (serialize). No disk I/O; the UI owns storage. SECRET-FREE: no credentials in RigConfig; building a StationConfig fills only non-secret fields, the UI injects portal/secret straight into the Station.

Initial catalog: interfaces = AllScan UCI150 (USB audio + WCH/CH340 serial PTT), generic CM108 dongle (HID PTT), built-in/default audio, Custom. Handsets = software dialpad, plain mic, generic in-band-DTMF mic (-> Alinco EMS-79 once confirmed), generic serial mic (-> ICOM once confirmed).

Depends on crates astar-audio (device enumeration) + astar-ptt (backends); integrates iax-dedb (Station/StationConfig). Auto-detect is a follow-up. Unit-tested: compatibility filter, resolve shapes, RigConfig serde round-trip.

### iax-31e9 — Harness: link FSM + *3/*2/*1 transition conformance (vs local Asterisk/ASL3)
*P2 medium · task · labels: cx:3, linking, protocol, test, tests*

Deterministic connect/monitor/disconnect transition tests for the link state machine against a local Asterisk/ASL3 podman. Merge gate for the link children. Builds on iax-c648 live-parrot harness + astar-conformance.

### iax-3448 — Add AGPL-3.0 SPDX license headers to all astar-lib source files
*P2 medium · task · labels: cx:2, licensing*

**Design:** WHY
astar-lib was relicensed MIT -> AGPL-3.0-only. Each source file should carry a per-file SPDX header so the license is unambiguous at the file level. NOTE (superseded in part): this repo is now AGPL-3.0-only end to end — the app is no longer proprietary and there is no open-core dual license, so the only per-file header anywhere is the AGPL one. The single exception is `vendor/ambe-thumbdv/`, which keeps its own MIT/Apache-2.0 notices and must NOT be given AGPL headers.

TIMING
Do this AFTER the planned astar-lib history reset / WT-Node refactor (Rob: 'iaxclient after reset'), since that churns file locations. Until then this nugget just tracks the intent.

WHAT
- Add to the TOP of every hand-written source file (~172 .rs across crates/, plus build/helper shell + python scripts):
    // Copyright (c) 2026 Rob Ludwick
    // SPDX-License-Identifier: AGPL-3.0-only
  * .rs: place ABOVE the leading //! module-doc comments (regular // comments may precede //!).
  * shell/python: '#' comment, AFTER the '#!' shebang.
- NO 'no-AI-training' clause anywhere; AGPL-3.0 forbids further restrictions (Rob: 'The AGPL3 code is fine'). (Originally this clause was scoped to the then-proprietary app; the app is AGPL now, so it applies nowhere.)
- Idempotent: skip files already containing Copyright/SPDX.
- SKIP: target/, .worktrees/, vendored/third-party, and anything generated. For the cbindgen-generated C header (crates/astar-sys/include/astar.h), prefer configuring cbindgen's 'header' preamble (cbindgen.toml) to emit the SPDX line rather than hand-editing the generated file.

VERIFY
- Every .rs under crates/ (excl target/.worktrees) contains 'SPDX-License-Identifier: AGPL-3.0-only'.
- cargo build + clippy still pass (headers are comments).
- cbindgen header carries the notice if the preamble is configured; check-cbindgen stays green.

### iax-3746 — Station DTMF router + sinks (out-of-band / in-band / sidetone)
*P2 medium · feature · labels: cx:5, dtmf, station*
**Blocked by:** iax-68eb (closed), iax-dedb (closed)

**Design:** DTMF orchestration in the Station/console layer. Source -> router(policy) -> sink model (brainstorm 2026-06-14).

Normalized event: `DtmfDigit(char)`. A router applies a user-configurable policy mapping each source's digits to one or more sinks.

Sinks:
- out-of-band: emit IAX2 DTMF frames (reuse iax-be21).
- in-band: synthesize the tone (codec generator, iax-68eb) into the OUTGOING audio.
- local sidetone: play the tone to the OUTPUT/playback path only, never the wire; can accompany either sink.

Sources covered here: the UI dialpad (a `station.send_dtmf(digit)` call). Audio and serial sources land in follow-up tickets.

Policy: one global default (default OUT-OF-BAND — the correct AllStar path) with optional per-source override. RX-decoded digits surface as `StationEvent::DtmfDetected(char)` regardless of policy.

Station API (sketch): `play_dtmf`, `send_dtmf` (policy-routed), explicit `send_dtmf_inband`, plus `DtmfDetected` events and last-digit in ConsoleState.

Depends on iax-68eb (codec primitive) and iax-dedb (Station library).

### iax-3ec5 — Resolve announcement phrases off the session lock (TTS no-stall)
*P2 medium · chore · labels: audio, cx:2, node, refactor*

Final-review I1 residual: synth_via_piper now enforces cfg.timeout but still runs UNDER the Station session mutex, so a slow/large TTS synth holds the lock (stalls pump/SSE) for up to the timeout. Resolve the Phrase→PCM (incl. TTS subprocess) BEFORE taking the session lock; lock only to enqueue/play. Removes the control-plane stall entirely.

### iax-42ba — Fix queued (non-started) announcement placeholder handle
*P2 medium · bug · labels: audio, cx:2*

Final-review M1: the non-preempting queued path returns AnnounceHandle::new_placeholder() which is already is_done()==true while the real announcement plays later (real handle discarded in poll_announcements). Benign now (every caller drops the handle) but a latent API-correctness bug: a caller that polls the queued handle sees 'done' immediately. Store the resolved request + return the real handle when it starts, or redesign the queued-handle contract.

### iax-4af7 — Investigate selectable denoise filters: front-end-choosable strategies to clean a noisy line
*P2 medium · task · labels: audio, cx:3, dsp, research, ux*

Research + prototype a menu of noise-reduction strategies an operator can pick per-line, since one filter doesn't fit every noisy condition. Survey: spectral subtraction, Wiener/adaptive filtering, RNNoise-style ML denoise, tonal notch/comb for whine (already have calibrated MicProfile notches, iax-fb8d), adaptive noise gate + hum filter (current generic NoiseReducer), multiband compression. Goal: a front-end picker ('clean a noisy line' presets) backed by a DspStage chain (relates iax-0465) and the per-mic router lane (iax-64b6 P1 surfaced gain/denoise/compress/profile cells). Out: a recommendation on which 2-4 options to ship + how to expose them. Relates iax-267f (spectral NS), iax-d50d (NR+compression).

### iax-6520 — FFI: inbound codec_policy on IaxNodeConfig (bidirectional wideband for node mode)
*P2 medium · task · labels: cx:2*

**Design:** iax-3e53 exposed codec_policy on IaxConfig (outbound + engine rate) and negotiated_format on IaxState, but IaxNodeConfig has no codec_policy field — an FFI/Swift Node-mode station cannot request slin16 for INBOUND calls (IncomingCallPolicy stays UlawOnly). Mismatches don't break (capped_to_rate downgrades with only a tracing::warn, invisible over FFI; the negotiated_format snapshot lets the UI observe what actually landed), but bidirectional wideband node-to-node needs the inbound knob. Scope: codec_policy string field on IaxNodeConfig -> IncomingCallPolicy.codec_policy (same parse/error conventions as IaxConfig), header regen, Swift NodeConfig mirror, tests. While there: (a) add VoiceFormat to astar-station prelude re-exports (lib.rs:65 gap); (b) note the Swift VoiceFormat enum decodes unknown bits as nil — indistinguishable from no-call; whoever adds the next CodecPolicy variant must extend the Swift enum (leave a comment breadcrumb at both sites).

### iax-66ba — FFI: device catalog + rig resolve/apply over the C-ABI (JSON, secret-free)
*P2 medium · feature · labels: cross-platform, cx:3, device, ffi*
**Blocked by:** iax-28b3, iax-dedb (closed)

**Design:** Expose iaxclient-devices over the astar-sys C-ABI so cross-language native clients (C / Swift / Python / .NET; Linux / Windows / Mac / iOS) drive the two-step selection. Decision: catalog + RigConfig cross as JSON strings (one serde model serving UI rendering, the UI save/load, and the FFI). Live call state stays #[repr(C)] poll/snapshot.

C-ABI (sketch):
  iax_devices_catalog_json(buf,len)                  -> catalog JSON
  iax_devices_compatible_handsets(iface_id, buf,len) -> filtered handsets JSON
  iax_rig_resolve(json_selection, buf,len)           -> resolved RigConfig JSON
  iax_station_apply_rig(station, json_config)        -> apply non-secret config

SECRET-FREE across the boundary: the UI injects portal/secret directly into the Station, never through the rig JSON. Caller-buffer + truncation rules as in the existing Station C-ABI. Swift/Python/C examples that list the catalog, resolve a UCI150 + handset selection, and apply it.

Depends on iaxclient-devices (iax-28b3) and iax-dedb (Station C-ABI in astar-sys).

### iax-72df — In-band DTMF decode wired into the node (RX/mic -> mapper)
*P2 medium · feature · labels: audio, cx:3, dtmf, node*
**Blocked by:** iax-d111, iax-3746

Feed real in-band DTMF (RX + mic, Goertzel) through the child #2 mapper inside the daemon. Largely INTEGRATION of existing iax-d111 (DTMF audio sources) + iax-3746 (Station DTMF router + sinks) into astar-server. Depends on those two.

### iax-74be — uci150_usb: adopt nusb 0.2.5 macOS unconfigured-device guidance in open_wch
*P2 medium · task · labels: cx:2*

**Design:** nusb 0.2.5 documents (commit df0ef13) that macOS only auto-configures composite-class devices or those with a known driver; vendor-class devices may be unconfigured after open. Our CH343 works because Apple's CDC-ACM driver attaches (known driver -> configured), but CH340/341 variants are vendor-class 0xFF and would be unconfigured. Add the documented pattern to open_wch() right after open(): if device.active_configuration().is_err() { device.set_configuration(1).wait() } (CH34x have exactly one configuration; when the CDC driver holds iface 0 the check short-circuits before the exclusive-open requirement matters). Unit-testable only by inspection/hardware; keep it minimal + doc comment citing the nusb quirk note.

### iax-7b9d — Cross-platform device auto-detection (DeviceDetector trait + macOS)
*P2 medium · feature · labels: cross-platform, cx:3, device*
**Blocked by:** iax-28b3

**Design:** A `DeviceDetector` abstraction that scans connected hardware, matches it to catalog InterfaceProfiles, ranks candidates, and pre-selects the best; always falls back to Custom. macOS impl first (CoreAudio device names + IOKit/serial enumeration; serial via the serialport crate, CM108 via hidapi). Linux/Windows impls are separate follow-up tickets.

Seeds the existing UCI150 autodetect (WCH cu.wchusbserial port). Output feeds the iaxclient-devices two-step picker (pre-selected interface).

Depends on iaxclient-devices (iax-28b3). Cross-platform note: serial + HID libs are portable; only audio-device + USB enumeration is per-OS.

### iax-809f — Test gaps from 2026-06-05 review
*P2 medium · task · labels: cx:3, review-2026-06-05, tests*

Test coverage gaps surfaced by the 2026-06-05 review:

- Sequence-number wrap-around (Reliability and FSM). The test that would have caught iax-b47c. Add a proptest that drives 1000+ frames through Reliability with random duplicates and OOO arrival, asserting accurate ISeqno tracking.
- Fuzz targets fuzz_parse_full and fuzz_parse_ies (fuzz/ dir exists but excluded from scope). Parser's adversarial-input resistance is currently asserted only by hand-picked truncation tests at tests/fixtures.rs:261-275.
- Malformed-IE-inside-AUTHREQ regression: fsm.rs:263 does Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty()) so a malformed AUTHREQ silently loses the challenge and FSM produces empty MD5 response. No test pins this.
- text.rs proptest only generates valid K-status payloads; add a strategy with arbitrary \x00-containing or extremely long inputs to confirm the Raw fallback never panics.
- Reliability::tick interleaved with peer ACK mid-tick. Race-free by single-threaded design but FSM-test combination doesn't pin that.
- RxOutcome::GaveUp → Event::DeliveryFailed path: FSM has no arm, falls through to LogInvalid (silent failure). Add a test that drives Reliability to retransmit exhaustion and asserts the FSM surfaces a typed error to the app.

### iax-85c9 — Human-phrased event-table + voice-ID announcements
*P2 medium · feature · labels: cx:2, node*

Final-review M4: maybe_fire_event_announcement currently synthesizes the raw config KEY (e.g. 'incoming_call'/'hangup') to air via Phrase::Text(key); id_request voice mode uses 'node {id}'. Map events to human phrases (e.g. 'node {id} connected') or named samples, per spec §Triggers examples.

### iax-8b0a — Asterisk harness: unhappy-path scenarios (REJECT, timeout, expired CALLTOKEN, malformed frames)
*P2 medium · task · labels: cx:2, infra, test, tests*

**Design:** Once iax-6813 happy-path harness lands, extend it with adversarial scenarios:

Goals:
  - Bad password -> AUTHREQ -> AUTHREP -> REJECT -> Failed
  - CALLTOKEN expired (delay >10s between server CALLTOKEN reply and client re-NEW) -> server rejects
  - Server unreachable (no Asterisk listening) -> NewSent retry exhausted -> Failed
  - Malformed inbound frame (truncated header, bogus IE length) -> Fsm rejects without panic
  - Asterisk-initiated HANGUP mid-voice (dialplan timeout) -> Active->Closed via inbound HANGUP
  - Network jitter / packet loss simulated via tc/netem in the bridge -> Reliability retransmit kicks in

Each scenario produces a sanitized pcap fixture so replay.rs catches regressions without spinning up the harness.

References:
  - iax-6813 (parent harness, happy-path v1)
  - iax-c333 plan follow-up: 'peer-initiated Hangup ACK' test gap
  - iax-7022 (real ASL3 captures, complementary)

### iax-9450 — ENCKEY (IE 44): add length/format guard so malformed payloads can propagate in lenient mode
*P2 medium · task · labels: cx:2, protocol, security*

iax-1741 made CHALLENGE/MD5_RESULT/RSA_RESULT/PASSWORD/ENCRYPTION strict in lenient mode. ENCKEY (id 44) was NOT added: it is parsed as opaque Some(payload) with no malformed-payload error path, so there is nothing to propagate and listing it in is_strict_in_lenient_mode() would be inert/untestable. If the AES/encryption work (iax-6c64) lands, ENCKEY should get a validation guard in apply_ie (size/format per the key schedule) so a malformed key errors instead of silently downgrading; then add it to the strict list with the two-test pattern.

### iax-9a71 — iOS audio: AVAudioSession ownership + RemoteIO capture/playback verification (cpal 0.15 on iOS)
*P2 medium · task · labels: astar, audio, cx:3, ios*

**Design:** Heads-up from the astar iOS feasibility review (astar docs/design/ios-client.md section 3; astar M1 spike astar-44c3). The audio backend is cpal 0.15 then CoreAudio/AudioUnit RemoteIO on Apple (astar-audio/Cargo.toml:9, stream.rs:204-212). NOTHING in astar-lib configures or activates an AVAudioSession (repo-wide grep of crates/bindings/scripts for AVAudioSession, RemoteIO, set_active = zero hits). On iOS the OS will not grant the mic to RemoteIO until an app has a .playAndRecord AVAudioSession setActive(true) — by Apple design that is the responsibility of the app, so astar will own the session in its Swift layer first (no Rust change needed; that is the M1 spike).

This ticket is the engine-side follow-up, CONTINGENT on the astar M1 result: first, confirm cpal RemoteIO capture and playback work against an app-activated AVAudioSession on a real device; second, if RemoteIO misbehaves vs a live session (sample-rate or buffer-size negotiation, route changes, Bluetooth SCO, simulator vs device), provide a first-class iOS audio path — a session-aware cpal configuration or an explicit AVAudioEngine/RemoteIO backend. Filing now for visibility; do not start until the astar M1 spike reports whether the app-layer session is sufficient.

### iax-9edf — Emit AnnouncementFinished/failed SSE lifecycle events
*P2 medium · feature · labels: cx:2, node*

Final-review spec gap: only AnnouncementStarted fires in production; AnnouncementFinished exists (TUI/serde) but is never emitted, and 'failed' is absent. SSE 'ANNOUNCING…' UI lights but never clears. Have the controller observe Manager announcement completion (poll_announcements) and broadcast Finished/failed. Spec §Triggers wants started/finished/failed.

### iax-be48 — SP-2: reachable node delta — asl3 register-host helper + ops/reachability doc + live AllStar bring-up
*P2 medium · task · labels: asl3, astar, cx:3, node*
**Blocked by:** iax-a1fb (closed)

Follow-on to the always-on node (SP-1). Register-as-node is ~80% built (iax-bc14 Registrar + iax-64b6 P7 wiring + refresh cadence). Remaining: (1) asl3 helper to resolve/provide AllStar's REGISTRATION host (today RegisterConfig.peer is a raw SocketAddr); (2) ops/reachability doc (UDP 4569 port-forward, public/DDNS IP); (3) live bring-up: register a real node number+secret and confirm an inbound call lands (iax-6461 steps 3 & 5). Depends on SP-1 for always-on-while-dialing.

### iax-c0bc — flaky: announce_service tests race with NoActiveCall (auto-unkey / queue-drain)
*P2 medium · task · labels: cx:2*

**Design:** crates/astar-iax/tests/announce_service.rs is flaky on clean master (observed 2026-07-06 during iax-2f2c gates): finished_to_air_announcement_auto_unkeys_when_not_operator_keyed and queue_drain_preserves_was_operator_keyed_across_fifo_chain intermittently panic with 'called Result::unwrap() on an Err value: NoActiveCall' (announce_service.rs:47 / :154). ~2-4 failures per 10 runs, different test each time — a timing race in test setup (announce issued before the call is active). Unrelated to iax-2f2c (reproduced with its diff stashed).

### iax-c308 — C-iaxclient call scenarios: capture against real ASL3 hub
*P2 medium · task · labels: cx:2, fixtures, rfc-audit, test*

**Design:** Sub-ticket of iax-7022: capture call_notoken, call_token, call_ulaw, and peer_hangup scenarios from the patched-C iaxclient against a real ASL3 hub (not a local Asterisk-in-Podman). Required because the patched-C iaxclient's CALLTOKEN-resend flow does not reset oseqno after dcallno=0 reset; our local strict Asterisk emits VNAK to the resent NEW and call setup stalls. Real ASL3 apparently tolerates this. Prereq: hub access (private throwaway node or write a tolerance test against a known-good hub). Output: 4 .pcap files under crates/astar-conformance/fixtures/c-iaxclient/ plus companion .md describing each flow.

### iax-c648 — live-parrot integration test (backgrounded against ASL3 podman)
*P2 medium · task · labels: cx:3, integration, live, test, tests*
**Blocked by:** iax-bbc6 (closed), iax-6813 (closed)

**Design:** End-to-end live test: ASL3 server in podman ↔ astar-cli ↔ default audio devices ↔ parrot.

Script: scripts/live-parrot.sh
  1. Start podman ASL3 (iax-6813 harness) in background
  2. Wait for IAX registration port + parrot extension to be answerable
  3. Launch astar-cli parrot subcommand in background, feeding test tone or mic
  4. Sample meter/jitter stats for N seconds
  5. SIGTERM the CLI, gather logs, tear down podman
  6. Report PASS/FAIL based on round-trip audio energy threshold

Runs MANUALLY (not in CI by default). Always backgrounded — the live session is long-running.

Acceptance:
  - Single-command invocation produces measurable round-trip audio
  - All processes (podman, CLI) cleanly terminate on exit / Ctrl-C
  - Logs land in target/live-parrot/<timestamp>/

### iax-cb69 — Cutover plan: astar swaps from patched C iaxclient to Rust implementation
*P2 medium · task · labels: cx:3, migration, strategy*

**Design:** Coordinate the actual swap once astar-sys is feature-complete enough for astar.

Phases:
  1. Side-by-side: iax-probe crate (already in astar workspace) gets a feature flag to run our stack vs vendored C. Compare wire traces.
  2. Soft cutover: astar-astar-sys depends on our static lib in a branch. Internal dogfood for 1-2 weeks.
  3. Hard cutover: vendor/iaxclient deleted from astar. astar-astar-sys → thin wrapper around iaxclient (high-level) crate, or deleted entirely.
  4. Pure Rust API migration: astar-core moves from astar-sys to iaxclient (Option C from strategy ticket).

Risk gates:
  - Phase 2 entry: 24h harness soak passes
  - Phase 3 entry: side-by-side audio quality A/B blinded acceptable
  - Phase 4 entry: astar-core refactor reviewed

References:
  - astar nugget astar-4e91 (long-term pure-Rust IAX2 — this IS that nugget's execution)
  - astar/crates/iax-probe (already exists — A/B test home)

### iax-d111 — DTMF audio sources: mic-capture + RX in-band decode -> router
*P2 medium · feature · labels: audio, cx:3, dtmf, station*
**Blocked by:** iax-3746, iax-68eb (closed)

**Design:** Wire the Goertzel decoder (iax-68eb) into two audio taps feeding the Station DTMF router (iax-3746).

1. Mic-capture in-band decode: some handset mics generate DTMF tones directly into the capture stream. Decode them off the mic input and feed the router. Tone squelch: when a mic-audio digit is routed OUT-OF-BAND, notch/suppress the tone from TX so it is not double-sent as both tone and frame. When routed IN-BAND, pass the original tone through (lowest latency, no regen).

2. RX in-band decode: decode tones the remote node sends in the decoded RX audio (the AllStar in-band *-command case) -> StationEvent::DtmfDetected.

Both decoders run in the call/audio thread at 8 kHz (before resample).

Tests: piped fixture audio with embedded tones decodes to the right digits; squelch removes the tone from TX when out-of-band; no false positives on voice.

Depends on iax-68eb + iax-3746.

### iax-d396 — CALLTOKEN-bearing register pcap fixtures (register_token.pcap, register_reject_token.pcap)
*P2 medium · task · labels: cx:2, test,pcap,calltoken,registration, tests*

**Design:** Capture ground-truth pcap fixtures for the CALLTOKEN-bearing registration handshake against an ASL3 Asterisk peer.

# Why deferred
iax-bc14 (Registration FSM) implementation plan defers these fixtures because they require harness extension to drive CALLTOKEN-on-REGREQ. The base iax-bc14 plan ships state-progression tests against the existing `register.pcap` and `register_reject.pcap` fixtures (no token); CALLTOKEN-bearing variants need their own capture cycle, following the iax-7022 pattern.

# Scope
- Extend the AllStar-in-Podman harness (iax-6813) to provoke a CALLTOKEN challenge on REGREQ.
- Capture `register_token.pcap`: client sends REGREQ → server replies REGAUTH with CALLTOKEN IE → client sends REGREQ#2 echoing the token → server completes with REGACK.
- Capture `register_reject_token.pcap`: same handshake but server emits REGREJ after the tokened REGREQ#2 (e.g., bad auth response).
- Place fixtures under `crates/astar-iax-core/tests/fixtures/` following existing conventions.
- Add replay tests in the bc14 FSM that drive these new pcaps end-to-end.

# Acceptance
- Both pcaps committed and parseable by the existing pcap-replay harness.
- New replay tests assert state-progression through the CALLTOKEN path (no byte-equality on MD5).
- `cargo test --workspace` green.

# References
- iax-bc14: registration FSM (state-progression assertions for the non-token path)
- iax-7022: prior ground-truth pcap capture pattern (the model for this work)
- iax-6813: AllStar-in-Podman harness
- RFC 5456 §8.6: CALLTOKEN handshake

### iax-e13e — Live AllStar bring-up: connect my node to another node over the tunnel (closes epic)
*P2 medium · task · labels: astar, cx:3, linking, node*
**Blocked by:** iax-be48

End-to-end: *3<peer> connects, two-way audio passes, *1 disconnects against a REAL AllStar peer over the WireGuard tunnel (iax-99ae). Absorbs iax-be48 reachability bring-up; optional POKE self-check (iax-b764). Closing nugget for iax-d829.

### iax-e549 — CTCSS encode/decode: sub-audible PL tone detect (Goertzel) + tone inject in astar-audio
*P2 medium · feature · labels: audio, ctcss, cx:2, dsp*

**Design:** New DSP primitive in astar-audio (alongside denoise/filter/dynamics). Decode: detect presence of a sub-audible CTCSS/PL tone via Goertzel at a configured tone frequency (the standard EIA set ~67.0-254.1 Hz) for receiver access gating, emitting a tone-present signal. Encode: inject the configured PL tone into TX audio.
Mechanism only: the ACCESS POLICY (carrier-only vs CTCSS-only vs AND/OR combining) lives in the consumer/app; this ticket exposes the detector + a simple tone-present output and the encoder. Surfaced to other languages by Ticket 1's binding.

### iax-fd14 — Wire local radio into the conference when include_local_radio=true
*P2 medium · feature · labels: audio, conference, cx:2, node*

**Design:** iax-647d plumbed include_local_radio end-to-end as config/state and the Conference engine fully supports + unit-tests local_mic/local_out, BUT the Manager builds the conference with local_mic:None, local_out:None — so include_local_radio=true is currently INERT (no local mic into the mix, no local speaker monitor). Inert at the daemon default (local radio OFF), so no default behavior changed. Follow-on: when include_local_radio=true, wire the AudioRouters local capture (mic) Receiver into Conference.local_mic and a local speaker Sender into Conference.local_out, so a lone connected user can reach the local radio and the operator hears the conference.

### iax-0465 — astar-audio: pluggable DSP stage chain (DspStage trait + DspChain)
*P3 low · feature · labels: audio, cx:5, dsp*

**Design:** Phase 2 of the calibration/pluggable-filter idea. Refactor the fixed mic chain (notch bank -> gate -> compressor) into a DspStage trait + an ordered DspChain configured from a profile, so stages can be added / reordered / swapped per mic (future: spectral or ML denoise from iax-267f). Follow-up to the phase-1 characterization (auto-notch + gate). Do once there are enough stages to justify the abstraction.

### iax-09c6 — AstarSerial: manual radio-line assert (test PTT output) for a self-test
*P3 low · task · labels: astar, cx:2, serial*

**Design:** SerialClient today only exposes pttTick(remoteKeyed,rxDb) — the radio output line (RTS) is driven from the debounced operator key input (CTS). For astar serial PTT self-test (astar-9661) this is an OPTIONAL enhancement: a method to assert/release the radio output line DIRECTLY (respecting radioLine + radioActiveHigh) without going through pttTick, so the user can verify the RTS->radio wiring independent of the handset PTT. Add SerialClient.setRadio(on: Bool) (or a timed pulse) over the C-ABI; secret-free; deinit must still fail-safe the line low. NOTE: astar primary self-test needs NO engine change (a guided press-your-handset test that reads the live pttTick key state) — this ticket is the nicer output-only variant. WARNING for consumers: asserting the radio line keys the transceiver = transmits RF. Acceptance: a consumer asserts+releases the radio line for a timed test; release/deinit drops it.

### iax-267f — astar-audio: spectral noise suppression (clean under speech)
*P3 low · feature · labels: audio, cx:5, dsp*

**Design:** Follow-up to the gate-based noise reduction (iax-a9d7): FFT-based spectral suppression that attenuates the estimated noise floor across the spectrum INCLUDING under speech (the gate only helps in pauses). Overlap-add windowing, noise estimation (minimum-statistics or gate-gated), spectral gain with a floor to limit musical noise. Likely needs an FFT (hand-rolled radix-2 or a small dep). Slots into the NoiseReducer chain ahead of / replacing the gate behind the same checkbox.

### iax-2768 — slin_probe: send full WT NEW (empty CALLTOKEN) first
*P3 low · task · labels: cx:1*

**Design:** Found live vs node 77777 (2026-07-12): the wt-mode probe opens with a minimal NEW carrying only an empty CALLTOKEN IE; peers with requirecalltoken=no (ASL3 default guest path) process that bare NEW and REJECT 'Mandatory IE missing'. The app (builders.rs) sends the full IE set + empty CALLTOKEN up front and works against both token-requiring (55553) and no-token peers. Make the probe match: full WT-shape NEW with token=Some(&[]) first; the existing CALLTOKEN handler already covers the resend. Verified live both ways during the 77777 codec probe.

### iax-2c21 — Device auto-detection: Windows (WASAPI/SetupAPI) detector impl
*P3 low · feature · labels: cross-platform, cx:2, device, windows*
**Blocked by:** iax-7b9d

**Design:** Windows `DeviceDetector` impl: enumerate WASAPI audio endpoints + match serial/USB via SetupAPI (VID:PID). Feeds the iaxclient-devices picker. Depends on the detector trait. Activate when the Windows native client starts.

### iax-40bb — Device auto-detection: Linux (ALSA/udev) detector impl
*P3 low · feature · labels: cross-platform, cx:2, device, linux*
**Blocked by:** iax-7b9d

**Design:** Linux `DeviceDetector` impl: enumerate ALSA audio devices + match serial/USB via udev/sysfs (VID:PID). Feeds the iaxclient-devices picker. Depends on the detector trait. Activate when the Linux native client starts.

### iax-5a5c — DTMF-over-serial source (handset keypad -> router)
*P3 low · feature · labels: cx:3, dtmf, ptt, research, serial*
**Blocked by:** iax-3746

**Design:** Some handsets have no audio keypad — they send DTMF digits over SERIAL (no tones). Convert serial keypad signaling into normalized DtmfDigit events and feed the Station DTMF router (iax-3746), so a serial digit can go out in-band or out-of-band per policy, same as any other source. Natural extension of astar-ptt (the existing UCI150 serial bridge).

NEEDS PROTOCOL RESEARCH FIRST: the serial-DTMF framing is device-specific. Likely first target is the AllScan UCI150 (already used for serial PTT) — but confirm whether/how it emits keypad digits over serial before designing the parser. If no device emits DTMF over serial, this stays parked.

Depends on iax-3746. Low priority until a concrete device + protocol is confirmed.

### iax-6940 — GSM / Speex / iLBC codecs (post-1.0, behind feature flags)
*P3 low · feature · labels: codec, cx:5*

**Design:** Stock iaxclient ships GSM, Speex, iLBC. AllStar uses μ-law by default but nodes can negotiate others. Defer to post-1.0 unless we hit a real-world need.

Options per codec:
  - Pure Rust port (Speex has rust-speex; GSM has 'gsm' crate)
  - FFI to libgsm / libspeex / liblbc (system libs)
  - Skip entirely until measured demand

Scope when picked up:
  - Feature flags: --features codec-gsm, codec-speex, codec-ilbc
  - Codec negotiation in NEW IE handling (IAX_IE_FORMAT, IAX_IE_CAPABILITY)
  - Fall-through to μ-law if negotiation fails

References:
  - vendored iaxclient codec_*.c
  - RFC 5456 §8.7 (capability negotiation)

### iax-6975 — Native engine VOX mode (auto-key the gate from the mic input level)
*P3 low · feature · labels: astar, audio, cx:3, ffi, station, vox*

**Design:** Follow-up to iax-5c30 (which exposes the unkeyed mic input level so a CONSUMER can do VOX). This is the fuller option: the ENGINE owns voice-activity detection and auto-keys the PTT gate from the input level on the audio thread, with proper attack/release + hysteresis, exposed as a station mode over the C-ABI.

WHY (beyond iax-5c30): consumer-side VOX polls the snapshot (~20Hz = ~50ms granularity) and round-trips set_ptt over the FFI, so attack/release timing is coarse and jittery. Native VOX runs in MicLane on the audio thread for tight, smooth keying. Consistent with the source-agnostic PTT design (VOX is just another PTT source, like a button / spacebar / serial line) and benefits ALL consumers, not just astar.

SHAPE (sketch): VoxConfig{ enabled, threshold_db, attack_ms, release_ms, hang_ms } on the mic lane; when enabled, MicLane drives the gate from mic_input_peak (built by iax-5c30) instead of the explicit PTT bool. FFI: iax_station_set_vox(enabled, threshold_db, attack_ms, release_ms, hang_ms) (secret-free; plain numbers). Interplay with explicit set_ptt and with half-duplex etiquette (don't key over active remote PTT) to be designed.

PARKED pending decision (see iax-5c30 open questions Q2): only build if polled consumer-side VOX (after iax-5c30) proves too laggy. Minimal unblock is iax-5c30 alone.

### iax-6c64 — IAX2 encryption (AES key + ENCKEY/ENCRYPTION IEs)
*P3 low · task · labels: cx:2, protocol, rfc-audit, security*

**Design:** RFC 5456 §7.4 / §10 encryption — IAX media encryption via shared-secret AES key derived from auth. Not deployed in astar's network; not required for 1.0. Track here as the catch-all for AES IEs + key rotation if/when needed.

### iax-761b — RFC 5456 audit phase 2: cover §1-3 (intro/conventions/terminology), §10 (IANA), §11+ (refs)
*P3 low · task · labels: cx:2, docs, spec*

**Design:** Extend iax-d649's rfc5456-audit.md to cover the sections excluded from phase 1:

- §1 Introduction (non-normative but flags scope)
- §2 Conventions
- §3 Terminology — useful to map RFC terms to our type names
- §10 IANA Considerations — the IE / subclass registries
- §11+ References, Acknowledgements

These add no wire-affecting requirements but complete the audit so any future RFC update can be diffed cleanly. Defer until phase 1 (iax-d649) lands and stabilizes.

### iax-80c8 — parrot pump calls blocking hangup under the session lock in poll_announcements; move to remove + off-lock hangup
*P3 low · task · labels: cx:1, node*

*(no description recorded)*

### iax-90d1 — Call path optimization (TXREQ family)
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §6.5 call path optimization — TXREQ/TXCNT/TXACC/TXREADY/TXREL/TXMEDIA/TXREJ. Lets two peers cut a middle box out of the media path once both endpoints can reach each other directly. Not needed for ASL3 endpoints (they always terminate calls) but RFC-required to be ignorable. Phase: post-1.0.

### iax-9ec5 — Trunking (RFC §7.1)
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §7.1 trunking — multiplex multiple call legs onto a single UDP stream with shared headers. Single-call ASL3 endpoints do not need this; flag as Won't unless a future use case appears.

### iax-a288 — au: nugget created inside a worktree is invisible to the main checkout until synced
*P3 low · task · labels: cx:2*

**Design:** UX sharp-edge hit while working iax-ceba/iax-d937 (2026-06-22).

REPRO:
1. From inside .worktrees/<id>, run `au add "..."` -> nugget iax-XXXX created (its .au/iax-XXXX.am is written in THAT worktree only).
2. From the main checkout, `au claim iax-XXXX` (or show/list) -> 'Not found.' The nugget is invisible until its .am is committed AND merged/synced into the main checkout's .au.

WHY IT BITES: the CRDT-per-worktree model is correct by design (parallel agents edit nugget state on different branches, git merge reconciles). But `au add` gives no signal that the new nugget is local-to-this-worktree, and `au claim` from another checkout fails with a bare 'Not found.' that looks like the id was mistyped, not 'exists elsewhere, not yet synced'. An agent then can't tell a typo from an unsynced nugget.

SUGGESTED FIXES (pick one+):
- `au add` prints a hint when run inside a worktree: 'created in worktree <id>; run au sync from main (or commit+merge) to surface it elsewhere.'
- `au claim/show <id>` on a miss scans sibling worktrees' .au and, if found, reports 'exists in worktree <path>, not synced' instead of 'Not found.'
- or auto-sync new nuggets to the repo root .au on add.

WORKAROUND USED: stayed in the origin worktree and used `au start` (in-progress, no new worktree) instead of claiming from main.

### iax-b764 — POKE reachability check
*P3 low · task · labels: cx:2, protocol, rfc-audit*

**Design:** RFC 5456 §6.7.1 POKE — lightweight reachability ping that does not establish a call. Useful as a status-check primitive for monitoring. Folds in with iax-a307's PING/PONG work but conceptually separate (POKE happens outside an active call, PING inside one).

### iax-b84f — signal report spoken_codec unknown arm: non-negotiated codecs (G729/GSM/iLBC) would speak 'Codec unknown, 16 bit, 8 kilohertz' (wrong depth+rate); omit the sentence for unknown formats. Latent only — parrot never negotiates them.
*P3 low · task · labels: audio, cx:1*

*(no description recorded)*

### iax-bb01 — RSA authentication (RSA_RESULT IE)
*P3 low · task · labels: cx:2, rfc-audit, security*

**Design:** RFC 5456 §8.6.16 RSA authentication — RSA_RESULT (IE 17) instead of MD5_RESULT. astar uses MD5 + CALLTOKEN exclusively; RSA support is gated behind future need for inter-network federation.

### iax-df6c — FailReason: dedicated DeliveryFailed variant (vs reused Timeout)
*P3 low · chore · labels: cx:2, protocol, refactor*

iax-6c21 made Event::DeliveryFailed terminate the call, but reused FailReason::Timeout { in_state } for retransmit-exhaustion (the no-new-shared-enum-variant guard was in force during the parallel wave). Retransmit exhaustion is arguably distinct from a protocol timeout. Decide whether to mint FailReason::DeliveryFailed { in_state } and route on_delivery_failed to it; update Display (iax-1c24) and any consumers. Pure enum-variant change -> must build --workspace (breaks exhaustive matches).

### iax-e9fb — astar-cli polish + Manager call-state API gaps
*P3 low · task · labels: api, cli, cx:2*

Open questions from iax-bbc6 (astar-cli): (1) Manager::dial returns immediately after spawn with no synchronous 'connecting' confirmation — progress only via CallEvent (Ringing/Answered); consider a connecting/dialing state or a returned handle that can be awaited. (2) Manager::key before Answered: unverified whether the engine buffers or drops pre-answer mic frames; if pre-answer keying should be rejected, expose a Manager-level call-state predicate so the CLI can gate it (currently only snapshot() exposes state). (3) register subcommand is one-shot (deregisters on first Registered); a stay-registered-until-SIGINT mode needs a signal handler + public affordance. (4) note: CallEvent is #[non_exhaustive], RegistrationEvent is not.

### iax-fd95 — live POST /bridge flip across the Parrot boundary doesn't rebuild the conference engine (parrot stays off/on); rebuild or reject
*P3 low · task · labels: cx:1, node*

*(no description recorded)*
