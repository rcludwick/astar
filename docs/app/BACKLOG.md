# Backlog

Durable backlog for **astar**.

Session work goes in the **Claude Code task tracker** (`TaskCreate` / `TaskUpdate` /
`TaskList`): create a task before starting, mark it `in_progress` when you pick it up,
`completed` when it's done. Anything that outlives the session belongs in this file.

Migrated off the **beads** (`bd`) tracker on 2026-07-29 — do NOT invoke `bd`.
The 33 open items below each carry their full description and design text
inline. All 129 issues (107 of them closed) were exported to
`docs/issues-archive.jsonl`, which is gitignored and local-only; a committed copy of the
tracker's final state survives in git history at the migration commit.

## Open items (35)

### astar-2b71 — TX level graph stays flat on some built-in microphones
*P2 medium · bug · labels: audio, ptt, macos, cx:3* — **targeted at 0.1.5beta**

Reported by Rob against 0.1.3beta: on a MacBook Air using the built-in mic, the
TX meter and TX trace stay at the floor when keyed. The same build with a UCI150
on a Mac mini is fine, so it is device- or path-specific, not a general break.

Both TX renderers gate on the same flag — `MenuPopover.swift` `VUMetersPane`
(`db: session.ptt ? meters.txDBHeld : -60`) and `LevelGraphView.swift`
(`tx: session.ptt ? session.meters.rawTxDB : -60`) — and `session.ptt` is
assigned only from the engine snapshot (`CallSession.swift`, `if ptt != snap.ptt`).
So a flat TX has exactly two causes: `ptt` never goes true, or it does and the
mic lane is delivering silence.

RULED OUT — do not re-derive this. The first theory was that a fresh install
seeded a UCI150 hardware profile on a machine with no UCI150 (the seeding bug
fixed in astar-1f7d), enabling serial and force-unkeying every poll. It cannot:
`SerialController`'s `pttSourceTick` closure guards `let client = self.client
else { return nil }`, and `nil` means "leave PTT alone this tick".

TWO SURVIVING MECHANISMS, and one observation separates them — when keyed, does
the TX bar brighten (its tint goes full opacity when `active`, i.e. `ptt` is
true) while still reading 0%, or does nothing change at all?

* Brightens but zero → the mic lane is producing zeros. The signed DMG has a
  different code signature than a source build, so macOS treats it as a separate
  app for microphone permission with its own TCC prompt and its own Privacy
  entry. Denied or dismissed, CoreAudio returns a stream of zeros rather than an
  error. `NSMicrophoneUsageDescription` and the audio-input entitlement are both
  present, so the prompt does appear — it just has to be accepted. Check whether
  the working Mac mini is running `just run` (authorized long ago) rather than
  the DMG.
* Nothing changes → half-duplex. `CallSession.swift` refuses to let VOX key while
  `receiving`, and on an Air the built-in speakers are physically coupled to the
  built-in mic — which is exactly what that guard exists to prevent. `receiving`
  is `snap.remotePTT || rxActivityGate.update(...)`, so ordinary far-end audio
  through the speakers holds VOX unkeyed indefinitely. Only bites VOX, not the
  on-screen hold-to-talk.

Pairs with the muted/silent-audio warning Rob asked for (hardware mute, zeroed
gains, AND denied mic permission are three causes with one symptom — a dead
meter). That warning would have diagnosed this on its own, so build them
together.

### astar-9d21 — Docs still say the macOS app is built on `MenuBarExtra`
*P3 low · chore · labels: docs, cx:1*

The app moved off `MenuBarExtra` to a hand-rolled `NSStatusItem` +
`StatusItemController` (the only surviving mention in code is a historical
comment at `StatusItemController.swift:73`), but three docs still present
`MenuBarExtra` as the current architecture — and, worse, as the *reason* for the
macOS 13 floor: `docs/site/macos/index.md:18`,
`docs/site/build/prerequisites.md:49`, `README.md:149`. Establish what actually
sets the floor now (SwiftUI/Ventura API use, not `MenuBarExtra`) and reword all
three. Spotted while correcting the Dock-icon claims for astar-7c31.

### astar-e2b9 — D-Star network light-up (blocked on iax-a9d4)
*P2 medium · feature · labels: network, dstar, cx:2*

When the engine's decode-only D-Star milestone (astar-lib iax-a9d4)
lands: network-switcher gains a D-Star entry (listen-only at first —
reuse the existing TX-disabled/listen-only presentation), reflector
host + module target field, per-network audio profile, talker callsign +
slow-data message display, vendored binding bump for the new Station
surface. Spec: `docs/superpowers/specs/2026-08-06-ambe-thumbdv-dstar-design.md`
(later-milestones section); same design bar as the M17 light-up. Optional
build: D-Star only appears when the engine was built with its features on.

### astar-d4c2 — Accessibility phase 2.5: remaining P2s (focus, menu-bar state, color-only tint, canvas values, contrast)
*P2 medium · task · labels: a11y, cx:2*

**Design:** The audit P2s not covered by phases 1-2 (see
docs/superpowers/reviews/2026-08-05-accessibility-audit.md): F8 focus
management (`@FocusState` + `.defaultFocus` on the node field when idle;
focusable PTT with `.onKeyPress`); F9 menu-bar item (live
accessibilityLabel/toolTip tracking `updateIcon` — "astar — transmitting"
etc.; decide the AXShowMenu story for the right-click menu); F11 menu-bar
tint is color-only (vary the mark per state, not just hue — needs a design
pass, ties to astar-3f57's tint system); F12 talk-timer text ("1:40") next
to the 8 pt dot; F14 canvas accessibilityValues (level-history summary in
dB, FFT hidden or summarized, Mic Analyzer exposes detectedPeaks); F16
contrast pass (promote `.tertiary` instructional captions to `.secondary`,
test over a white desktop with Increase Contrast). Phase 1 = astar-a9c3
(closed 2026-08-05), phase 2 = astar-b167 (closed 2026-08-05).

### astar-f92a — Accessibility phase 3: Dynamic Type, contrast, Reduce Motion polish + gui-rs platform decision
*P3 low · task · labels: a11y, gui-rs, cx:3*

**Design:** The audit's P3 tail: fixed caption2 sizes vs Dynamic Type,
muted-on-translucent contrast checks, Reduce Motion gating for the tap
flash if needed. Plus the PLATFORM decision to track: gui-rs (iced 0.14,
no accesskit in the tree) exposes NO accessibility tree — NVDA/JAWS/Orca
see one opaque canvas. Options when picked up: track iced's AccessKit
integration, contribute, or document the limitation and point
screen-reader users at the Mac app. Details in the audit doc.

### astar-8c4d — Ship libcodec2 with the app so M17 works without Homebrew
*P2 medium · task · labels: m17, cx:2* — **targeted at 0.1.4beta**

DIRECTION CHANGED 2026-08-17 (Rob): prefer **static linking** over the bundled
dylib this item was originally specced around, because of notarization. A
hardened-runtime binary will not load a dylib that is not signed with the same
Team ID, so a bundled `libcodec2.dylib` would have to be signed as part of the
app and re-signed on every build — and a Homebrew one on a user's machine is
both unsigned by us and usually absent. Static removes the load-time failure
mode and a runtime dependency from the DMG at once.

`astar-codec` already has both backends built (`crates/astar-codec/src/codec2.rs`):
`codec2-runtime` dlopens a system libcodec2, and **`codec2-static` links the
pure-Rust `codec2` crate** — NOT the C library. So the macOS job is close to
"turn the existing feature on for the app build and verify M17 lights up on a
brew-less machine", not "vendor and sign a dylib".

Neither feature may enter a default feature set — that rule stays (it is what
keeps a plain `cargo build` LGPL-free). Enable it for the shipped app build
specifically.

macOS licensing is fine: the `codec2` crate is `LGPL-2.1-only AND MIT`, and
LGPL's relink requirement is satisfied by astar's complete public AGPL source.
Static linking LGPL into a *proprietary* binary is the restricted case, which
this is not.

iOS is a DIFFERENT answer and is NOT unblocked by this — see `iax-e5d9` in
`docs/BACKLOG.md`. That SPDX expression is `AND`, not `OR`: both licenses apply
cumulatively, so there is no MIT-only path out, and LGPL is effectively
incompatible with App Store distribution. iOS M17 needs the clean-room codec,
not a different linkage strategy.

Original item follows.

From the iax-f2b8 final review: the spec's LGPL distribution story (bundle
`libcodec2.dylib` in astar.app/Frameworks, dlopen it) has no implementation —
astar never calls `setCodecDirs`, so M17 lights up only on machines with a
Homebrew libcodec2. Fix: build/vendor the dylib into the app bundle (build
script step), call `session.station.setCodecDirs([bundleFrameworksPath])`
at Station init, and verify m17Available flips on a brew-less machine.
Blocks distributing M17 builds to anyone else. gui-rs equivalent: ship the
dylib beside the exe / document the path (its own sub-decision).

### astar-6f1b — M17 UX polish + gui-rs callsign parity gaps
*P3 low · task · labels: m17, ux, cx:2*

Consolidated from iax-f2b8 reviews: (1) gui-rs has NO surface to edit a
committed callsign (Mac has the Settings field) — violates every-feature-
everywhere; add a Quick Config field. (2) Mac callsignDraft resets on picker
flip mid-type (discards untyped text). (3) uppercase-on-keystroke cursor
jump note (both platforms). (4) M17 connect failures use plain seam wording
(map to friendlier text like AllStar's connectFailureMessage). (5) Swift
M17Dial: document + test the IPv6 fail-closed rule (gui-rs side already
does). (6) hide/disable VOX controls while M17 active until the engine
mirrors input_level_db (iax-e2c8 item 6). (7) network-switch animation
shipped on Mac 2026-08-03 (work/astar-anim); gui-rs look-and-feel parity
pending.

### astar-d5a7 — D-Star network: Network.dstar light-up (XLX/XRF reflectors)
*P4 backlog · feature · labels: cx:3, dstar*
**Blocked by:** iax-d7e3 (D-Star engine backend)

**Design:** Undesigned placeholder per Rob 2026-08-03: D-Star as a switcher
network (KC-Wide is XLX458/XRF458 module A). UI shape mirrors astar-c2e5
(reflector + module + callsign). HARD GATE inherited from the engine item:
D-Star voice is AMBE — no freely-licensable software vocoder (hardware
dongle or legally-gray mbelib); design starts there.

### astar-e7b3 — YSF network: Network.ysf light-up (YSF reflectors)
*P4 backlog · feature · labels: cx:3, ysf*
**Blocked by:** iax-e8a4 (YSF engine backend)

**Design:** Undesigned placeholder per Rob 2026-08-03: System Fusion YSF
reflectors as a switcher network (KC-Wide: US-KCWIDE YSF32453 /
US-XLX458 YSF28054). Same AMBE+2 vocoder gate as D-Star/NXDN — inherited
from the engine item. Also the practical route to Wires-X rooms (astar-a3c9).

### astar-a3c9 — Wires-X reach: via YSF-bridged rooms (native protocol is closed)
*P4 backlog · feature · labels: cx:2, ysf*
**Blocked by:** astar-e7b3 (YSF light-up)

**Design:** Undesigned placeholder per Rob 2026-08-03: Yaesu's Wires-X
protocol is proprietary/closed — no native client is realistic. The
practical path is YSF reflectors bridged into Wires-X rooms (e.g. KC-Wide's
YSF28054 ↔ Wires-X room 28054), so this item is likely UI sugar over the
YSF network (labeling/directory awareness of bridged rooms), not its own
protocol. Decide when YSF lands.

### astar-b8e4 — NXDN network: Network.nxdn light-up (NXDN reflectors)
*P4 backlog · feature · labels: cx:3, nxdn*
**Blocked by:** iax-b9c2 (NXDN engine backend)

**Design:** Undesigned placeholder per Rob 2026-08-03: NXDN reflectors/
talkgroups as a switcher network (KC-Wide: TG 31313). Same AMBE+2 vocoder
gate — inherited from the engine item.

### astar-f1c6 — Hams Over IP network: Network.hoip light-up (SIP/G.711)
*P3 low · feature · labels: cx:3, hoip*
**Blocked by:** iax-c9f4 (Hams Over IP engine backend)

**Design:** Undesigned placeholder per Rob 2026-08-03: Hams Over IP as a
switcher network (KC-Wide: extension 15135). SIP/RTP with G.711 — NO vocoder
problem (same codec family the engine already speaks), so after M17 this is
likely the cheapest new network. Dial surface = extension number; callsign/
extension credentials from HoIP account settings.

### astar-c7a1 — Hamlink light-up: wire Network.available to the engine capability + deferred switcher items
*P3 low · task · labels: cx:3, hamlink*
**Blocked by:** iax-b3d7 (SvxReflector engine client)

**Design:** When iax-b3d7 lands, flip the astar-9b3e scaffolding live: Swift
`Network.available` grows the session/capability parameter (gui-rs already
has `available(hamlink:)`; its boot-cached `network_available` needs a
refresh path if capability can appear post-boot), and the picker/badges
appear automatically. Must land WITH light-up (found in the astar-9b3e final
review, 2026-08-02): (1) favorites/recents write paths (`addFavorite`,
`recordRecent`, gui-rs `add_favorite`/`record_recent`) must stamp the active
network — today they default allstar, so hamlink-session saves would
mislabel and auto-switch would dial the wrong network; (2) status badge
needs isInCall/status gating so a remote hangup can't leave a stale badge
(Mac: `activeCallNetwork` is only cleared by `disconnect()`); (3) gui-rs
needs the connect-seam network dispatch + unsupportedNetwork mirror the Mac
has; (4) gui-rs badge derives from picker selection, valid only while the
picker is idle-only; (5) Swift segmented Picker binds the raw AppStorage
string — a stale-unavailable raw with >1 available renders no selected
segment; (6) address dials don't stamp `activeCallNetwork` (arguably
`.allstar`); (7) decide whether mid-call network switching should be
possible (today the picker is unreachable in-call on both platforms — the
spec's "switching while connected doesn't disconnect" holds vacuously).
Deferred minors riding along: unchanged-selection dials still persist
settings (spurious write, gui-rs); `connect(node:network:)` doc omits the
off-main-thread contract; `network_picker` fn lifetime style (dial.rs).

### astar-3f03 — Dialpad doesn't emit touch tones (DTMF) — investigate UI → engine DTMF path
*P1 high · task · labels: cx:2*

**Design:** Reported by Rob 2026-07-15: pressing dialpad keys in astar produces no DTMF touch tones (unclear yet whether the tone is missing on the wire, in local sidetone, or both — characterize first). Check: astar dialpad action wiring → Station/CallSession DTMF send → astar-lib DTMF frame emission (and any local tone playback). Deferred behind iax-42ce conference-mode work per Rob. 2026-08-02: characterized under iax-4b7a (see astar-lib BACKLOG/history): the tone IS synthesized, but (a) nothing over the C ABI pumped poll_announcements, so the auto-key stuck and queued digits never started — fixed for sequence sends, which are now the only path astar uses; (b) astar runs in-band DtmfMode while Asterisk/AllStar expects protocol frames from IAX clients — the likeliest on-air symptom; whether astar should select .protocolFrames is Rob’s on-air call; (c) in-band tones garble on slin16 (iax-8d2f). Remaining: Rob’s on-air decode test via the new compose-then-send dialpad (astar-7d21, merged 2026-08-02).

### astar-44c3 — iOS audio spike (M1 gate): own AVAudioSession + prove parrot loopback on a real device
*P1 high · task · labels: cx:3, ios*

**Design:** GATES all iOS UI work. iOS builds+links but duplex audio is unproven: the engine is cpal-backed and NOTHING configures an AVAudioSession, which iOS requires before RemoteIO grants the mic (docs/design/ios-client.md §3). First approach, no Rust change: in astar iOS app layer configure AVAudioSession (.playAndRecord, mode .voiceChat, .defaultToSpeaker/.allowBluetooth), setActive(true) BEFORE constructing/connecting the Station; handle interruptions + route changes. Then dial parrot 55553 and confirm the loopback is AUDIBLE on a real iPhone (mic capture + speaker playback). Needs a real device + Apple Developer team for signing — the simulator cannot validate the RemoteIO mic path. If RemoteIO misbehaves vs a live session (sample-rate/buffer negotiation, BT SCO, route changes) escalate to the astar-lib iOS-audio ticket. Part of astar-1f8d.

### astar-734b — Vendor scripts trust stale xcframework caches (VENDOR_REV lies)
*P1 high · bug · labels: cx:2*

**Design:** Found 2026-07-11 debugging the 55553 reject: Tools/update-astarstation.sh and update-astarserial.sh only run build-xcframework.sh when the framework dir is ABSENT. A cached build in astar-lib gets re-copied forever while VENDOR_REV is stamped with current HEAD — so astar linked a pre-iax-866f core while claiming 3c7080e, and the 'fixed' app still sent the no-caps NEW (wire-verified via pcap). Second-order failure: the two Rust staticlibs then came from different rustc builds and collided on duplicate _rust_eh_personality at link. Fix: (1) astar-lib build-xcframework.sh (both swift and swift-serial) writes a BUILD_REV file (git rev + rustc version) inside the built xcframework; (2) astar update scripts rebuild when BUILD_REV is missing or != astar-lib HEAD or rustc -V differs; (3) VENDOR_REV is written FROM the framework's BUILD_REV, never from live git, so it cannot lie. Verify: touch a core commit, just vendor, confirm rebuild fires and VENDOR_REV matches; second vendor with no changes is a cheap no-op.

### astar-0217 — Friendly connect-failure messages (offline node shows useless error)
*P2 medium · task · labels: cx:2*

**Design:** Dialing an offline/unregistered node (e.g. 61057 on 2026-07-07) fails in the core's directory lookup (Asl3Error::NoRecords -> C-ABI IAX_ERR_RESOLVE = -6) and the popover shows "Couldn't connect: " + error.localizedDescription. StationError does not conform to LocalizedError, so localizedDescription is the useless Foundation default ("The operation couldn't be completed..."). The user gets no hint that the node is simply offline.

Fix (astar-only; do NOT touch vendored Packages/AstarStation sources — they are regenerated by `just vendor`):

- AstarCore: add a small, testable mapper `connectFailureMessage(for error: Error, node: String) -> String` (name/placement flexible — near CallSession.ConnectError is natural):
  - StationError with code IAX_ERR_RESOLVE (-6): "Node <n> wasn't found on the AllStarLink network — it's probably offline or not registered." (resolve failures can also be local DNS trouble, so keep the wording hedged, not "the node is down").
  - StationError with code IAX_ERR_PORTAL (-5): message pointing at AllStarLink account/credentials in Settings.
  - Any other StationError: use its `description` (carries "astarstation error <code>: <text>") — never localizedDescription.
  - CallSession.ConnectError and unknown errors: keep existing localizedDescription behavior (ConnectError already has good errorDescription).
  - The IAX_ERR_* constants come from the vendored C header via the AstarStation module; confirm they're visible from AstarCore (Station.swift uses them). If not importable, match on the raw code values with a comment naming the constants.
- MenuPopover.swift connect catch (~line 798): errorText = connectFailureMessage(for:node:) instead of "Couldn't connect: \(error.localizedDescription)".
- Tests (AstarCore): resolve-code error maps to the offline wording containing the node number; portal-code maps to account wording; unknown StationError falls back to description; ConnectError.needsAccount unchanged.

Verify the exact current bad text once by constructing StationError(code:-6,...) and printing localizedDescription in a test, so the nugget log records the before/after.

### astar-0bba — vox slider test.
*P2 medium · task · **in progress** · labels: cx:2*

Add a VOX test that tests the vox background level and validates that the vox slider is above the background level.

### astar-1efc — gui-rs: WT credentials UI
*P2 medium · feature · labels: cx:2, gui-rs*
**Blocked by:** astar-2fde (closed)

**Design:** Web-Transceiver credentials pane: inherits whatever astar-2fde settles on Mac, i.e. auto-save plus a Test button that validates token minting. Uses the existing connect_wt seam method. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-43eb — DMG M2: Developer ID sign + notarize + staple
*P2 medium · task · labels: cx:2*

Follow-up to au-a360. Sign + notarize + staple the DMG for public distribution. Blocked on a paid Apple Developer ID (deferred until closer to release).

**Design:** Make the astar.dmg distributable to other Macs without Gatekeeper warnings. Builds on au-a360 M1 (Tools/make-dmg.sh produces an ad-hoc/unsigned arm64 DMG; CI green). M2 = real signing: codesign astar.app with a Developer ID Application cert + hardened runtime + the microphone entitlement (com.apple.security.device.audio-input); notarize the DMG via notarytool (Apple ID + app-specific password or an App Store Connect API key); xcrun stapler staple. Wire the signing identity + notary creds into Tools/make-dmg.sh (the M2 steps are already stubbed in its trailer comment) and into the GitHub Actions workflow as secrets. BLOCKED on a paid Apple Developer ID (Rob will obtain one closer to a release). Acceptance: astar.dmg downloaded on a clean Mac opens without 'unidentified developer'/'damaged' and passes spctl --assess.

### astar-651e — gui-rs: Windows verification pass 2 (end of phase 1)
*P2 medium · task · labels: cx:2, gui-rs, test, windows*
**Blocked by:** astar-22cf (closed)

**Design:** End-of-phase-1 Windows pass: full operating-set surface including tray/popover shell, WT credentials, and UCI150 serial PTT on the vendor CH34x Windows driver. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-6a8e — android: audio spike (M1 gate) - Oboe + foreground service + parrot loopback
*P2 medium · task · labels: android, cx:3*

**Design:** Sibling gate to the iOS spike astar-44c3. Android pairs with iOS, not desktop: Kotlin/Compose view layer over the same Rust core, UniFFI already targets Kotlin. Spike proves low-latency duplex audio via Oboe/AAudio plus a foreground service keeping the connection and RX audio alive in background, demonstrated with parrot loopback on a real device. Mobile parity becomes its own roadmap once a spike passes. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-6c65 — Online node-name lookup: resolve un-named node numbers via the AllStarLink DB (cached)
*P2 medium · task · labels: cx:3, ui*

**Design:** WHY
Follow-on to astar-b13a: for node numbers the user hasn't named, show the callsign/name looked up from the AllStarLink node database, so 'numbers have names' even for un-favorited nodes.

WHAT (astar-side HTTP; metadata, not IAX2 — pp-c2bb allows astar's own HTTP for non-protocol data):
- Fetch the AllStarLink node directory (the public node list / astdb-style data from allstarlink.org) over HTTP; parse node -> callsign (+ location?).
- CACHE it locally (it's large — hundreds of thousands of nodes): persist to disk, refresh periodically / on demand; work offline from cache. Handle missing/failed fetch gracefully (fall back to the number).
- Plug into the NameResolver from b13a as a SECOND source: resolver order = saved favorite name -> cached AllStarLink name -> the bare number.

OPEN (brainstorm before building): exact data source/endpoint + format (full node-list download vs per-node query); cache storage + refresh cadence + size bounds; whether to show location too.

VERIFY: an un-named node shows its AllStarLink callsign in the dial menu / recents / connected header; works offline from cache; falls back to the number when unknown; saved favorite names still win.

### astar-70b9 — RX noise reduction: denoise the speaker path like the mic path
*P2 medium · task · labels: cx:3*

**Design:** Surface RX noise reduction end to end. DEPENDS ON iax-b1fe (core: adaptive NR on the OutputBus post-mix, Station::set_rx_noise_reduction + C-ABI - must merge in astar-lib first, then refresh vendored frameworks via just vendor). Design doc: docs/superpowers/specs/2026-07-06-rx-noise-reduction-design.md.
- Station.swift binding: setRxNoiseReduction(_:) (pattern setNoiseReduction).
- AstarCore: AudioSettings.rxNoiseReduction Bool default false (decode-with-default); StationDriving + CallSession.setRxNoiseReduction + push in applyAudioSettings; Setup override rxNoiseReduction: Bool? (nil = keep global - the astar-5c9a lesson) incl. apply + save-current paths.
- Quick Config: 'Noise reduction' switchRow in the Speaker card where the astar-76b9 comment marks the output-DSP home, same style as the mic's.
- gui-rs: rx_noise_reduction bool (serde default false) in AudioSettings + Option<bool> in Setup; slider/toggle surfaces with astar-f4d9.
FUTURE (deferred): per-call NR pre-mixer stays a core concern (see iax-b1fe note); no astar work.
VERIFY: TDD (applyAudioSettings pushes it; legacy settings decode false; Setup override applies/leaves; gui-rs round-trip + defaults). just ci green. Live: Rob toggles it on a hissy node and hears the floor drop.

### astar-a5c8 — gui-rs: Windows verification pass 1
*P2 medium · task · labels: cx:2, gui-rs, test, windows*
**Blocked by:** astar-f4d9 (closed)

**Design:** Linux-first development means Windows runtime behavior is verified in batches. Pass 1: whole-surface manual pass on Windows after the quick config panel lands - build, audio devices, connect, PTT, meters, persistence paths. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-b2dc — gui-rs: node-name lookup
*P2 medium · feature · labels: cx:2, gui-rs*
**Blocked by:** astar-6c65

**Design:** Resolve un-named node numbers via the AllStarLink DB with caching. Resolution and cache live in the Rust core so both clients share it; gui-rs contributes only the display wiring. Sequenced after astar-6c65 defines the core piece. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-b692 — gui-rs: talk timer
*P2 medium · feature · labels: cx:2, gui-rs*
**Blocked by:** astar-fda3 (closed)

**Design:** Port of the per-node TX-duration indicator, green to amber to red, to avoid repeater timeout. Phase math lives in shared Rust per the fat-core principle; sequenced after the Mac design settles in astar-fda3. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-c450 — gui-rs: serial/UCI150 PTT parity
*P2 medium · feature · labels: cx:3, gui-rs, serial*
**Blocked by:** astar-de0a (closed)

**Design:** Full hardware PTT parity via the serialport crate: port picker, RTS/DTR line selection with invert, COS sensing. Serial gets a small trait seam, PTT keying plus COS, with a fake for demos and a serialport-backed real impl so logic is testable without hardware. Linux uses in-kernel ch341; Windows needs the vendor CH34x driver documented. Last in phase 1 because it needs hardware in hand. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-d263 — VOX pre-roll: expose + wire set_vox_preroll_ms (consumer of iax-2733)
*P2 medium · feature · labels: audio, cx:2, vox*

Consumer of astar-lib iax-2733 (VOX pre-roll / look-back buffer). BLOCKED until iax-2733 lands (adds C-ABI iax_station_set_vox_preroll_ms + Swift Station.setVoxPrerollMs(_:), clamp 0..250ms, engine DEFAULT 0 = OFF). Because the engine default is off, astar must opt in or there is no behavior change. Work: (1) ensure the Swift shim setVoxPrerollMs is vendored (update-astarstation.sh / rebuild xcframework to pick up the regenerated header); (2) on the dialing-page VOX options, expose a pre-roll control (slider/stepper 0..250ms, sensible default ~250 when VOX is enabled) OR just set a fixed 250ms when VOX turns on if we don't want UI; (3) call station.setVoxPrerollMs(ms) on VOX enable + on change; (4) verify the first syllable is no longer clipped once VOX keys (pairs with astar-e9ff which wires VOX off inputDB from iax-5c30). Note open engine question Q1 in iax-2733: if the NoiseReducer gate suppresses the quiet onset, the prepended pre-roll may be near-silent — validate with real audio.

### astar-dbcb — Expose always-on node controls (iax-a1fb): accept-calls + register toggles + multi-call list
*P2 medium · feature · labels: cx:2, node, ui*

astar-lib shipped the always-on concurrent node (iax-a1fb): one Station can dial out AND accept inbound AND register concurrently (N calls), via NEW additive C-ABI toggles iax_station_enable_inbound/disable_inbound (InboundConfig: bind, AnswerPolicy Auto/Manual, max_calls default 20) and iax_station_register/deregister, plus a multi-call snapshot (ConsoleState.calls / Swift Snapshot). The existing iax_station_set_mode still works as a non-breaking shim (no change needed to ship), so this is ADDITIVE. To expose: an 'Accept calls' switch (enable_inbound), a 'Register node' switch (register), Auto/Manual answer + answer()/reject() UI for inbound, and a calls list from the multi-call snapshot. Park alongside the config-UI rework; set_mode keeps current behavior working in the meantime. Ref astar-lib spec 2026-06-22-iax-a1fb-always-on-node-design.md; cutover iax-cb69.

### astar-fd98 — gui-rs: always-on node controls
*P2 medium · feature · labels: cx:2, gui-rs*
**Blocked by:** astar-dbcb

**Design:** Accept-calls and register toggles plus multi-call list, mirroring the Mac feature from astar-dbcb; the core API iax-a1fb already exists. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-6193 — gui-rs: mic analyzer + characterization + profiles (phase 2)
*P3 low · feature · labels: cx:5, gui-rs, phase2*

**Design:** Phase 2 analysis suite, deliberately undesigned for now. Port of the Mac mic analyzer, characterization flow, and per-mic profiles; analysis math belongs in shared Rust. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-99e6 — gui-rs: level graph (phase 2)
*P3 low · feature · labels: cx:2, gui-rs, phase2*

**Design:** Phase 2 analysis tool, deliberately undesigned for now. Port of the Mac level graph; level history buffer belongs in shared Rust. Design: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md

### astar-d0df — Quick settings: only show 'Save changes' when the config is actually dirty
*P3 low · chore · labels: cx:2, ui*

The 'Save changes to <config>' button in Quick settings always shows even when nothing has changed vs the saved config. Show (or enable) it only when the live settings differ from the active setup's saved values (a dirty check over devices/gains/mic/VOX), so it's not misleading. Operator-suggested during the device-persistence debugging.

**Design:** QuickConfigView: the Save button (setups.saveCurrentToSelected) is gated only on selectedEditableSetup != nil. Add a 'dirty' computed check comparing the live audio state (audioStore / session) against the active Setup's saved fields (inputDevice, outputDevice, gains, compression, noiseReduction, vox, micProfileID) and only show/enable the button when they differ. Consider a SetupController.hasUnsavedChanges(for:) helper in the app layer (or a pure comparator testable in AstarCore).

### astar-e396 — gui-rs: call spectrum (phase 2)
*P3 low · feature · labels: cx:3, gui-rs, phase2*

**Design:** Phase 2 analysis tool, deliberately undesigned for now. Port of the Mac call spectrum; FFT axis math and trace state belong in shared Rust per the fat-core principle. Design when phase 1 operating set ships: docs/superpowers/specs/2026-07-01-gui-rs-parity-roadmap-design.md
