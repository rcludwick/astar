# astar iOS client — feasibility & design

`au` nugget: **astar-1f8d** · Research/design only — **no app code changed by this doc.**

astar is a native Apple SwiftUI AllStarLink client. macOS ships today as a
menu‑bar app; this document scopes what it takes to ship a real **iOS** app
(iPhone + iPad) reusing the shared core.

> **Headline verdict (audio):** the iOS target **builds, links, and produces a
> signed‑able `.app` today**, but **on‑device duplex audio is NOT proven and
> almost certainly will not work as‑is.** The engine's audio backend is
> `cpal` (CoreAudio/AudioUnit on Apple), and **nothing in the entire stack
> configures or activates an `AVAudioSession`** — which iOS *requires* before
> RemoteIO will hand out the microphone. This needs an **iaxclient‑rs change
> plus an on‑device spike** before any UI work is worthwhile. See §3.

---

## 1. What is SHARED and already works on iOS

Everything below is platform‑neutral Swift and compiles into the iOS target.
I verified this by building the iOS scheme to completion (see §1.5).

### 1.1 AstarCore (`Packages/AstarCore/`) — fully multiplatform
No `import AppKit`, `import UIKit`, or `import IOKit` anywhere in the package
(verified: `grep -rn 'os(macOS)|os(iOS)|import AppKit|import UIKit|import IOKit'`
over `Packages/AstarCore/Sources/AstarCore/*.swift` returns **nothing**). The
core is genuinely portable:

- **`CallSession`** (`CallSession.swift`) — the `ObservableObject` view‑model
  over the poll loop. Pure Foundation/Combine. `start()` uses a `Timer` at
  20 Hz (`CallSession.swift:84`). iOS‑ready as‑is.
- **`CallSession.live()` / `makeStation(credentials:)`**
  (`Station+Driving.swift:50,67`) — builds the real `Station`, falls back to
  `NullStation` if construction fails. Portable.
- **`VoxGate`** (`VoxGate.swift`) — pure, deterministic gate; injected clock.
  Portable. (But VOX is functionally broken upstream — see §4.)
- **`AudioSettings` + `UserDefaultsAudioSettingsStore`** — UserDefaults‑backed;
  portable.
- **`HardwareProfile` / `HardwareProfileRegistry`** (`HardwareProfile.swift`) —
  platform‑neutral presets. On iOS, only the **Headset** profile is meaningful
  (the UCI150/serial presets are inert — see §2).
- **`Credentials` + `KeychainCredentialStore`** — the Security‑framework
  Keychain API is identical on iOS; no macOS‑only access group or
  `kSecUseDataProtectionKeychain` quirk in the file. Portable.

### 1.2 AstarStation binding + `astar.xcframework`
- `Packages/AstarStation/Package.swift` declares `.iOS(.v13)` and links
  CoreAudio/AudioToolbox on iOS, gating only **AudioUnit** to macOS
  (`Package.swift:19,28‑35`) because on iOS that API lives inside AudioToolbox.
- The vendored `astar.xcframework` ships **real iOS slices** — not stubs:
  `bindings/swift/astar.xcframework/ios-arm64/libastar_sys.a` and
  `ios-arm64-simulator/…` are present, each a compiled `astar-sys`
  staticlib produced by `build-xcframework.sh` for `aarch64-apple-ios` and
  `aarch64-apple-ios-sim` (script lines 60‑61, 110‑117).
- `project.yml:38` excludes `x86_64` for the simulator because the framework is
  arm64‑only (Apple‑Silicon dev). The whole `Station`/`Snapshot`/`Event`
  surface (`Station.swift`) is C‑ABI poll/snapshot with no AppKit/IOKit deps.

### 1.3 The WebTransceiver (WT) connect path
`CallSession.connect(node:)` (`CallSession.swift:143`) mints a portal token over
**HTTPS to the AllStarLink web portal** and dials via `connectWT` when
credentials exist, else a guest dial (`secret "allstar"`, e.g. the 55553
parrot). This is plain networking inside the Rust engine — no platform
assumptions — so the *signalling/dial* path is iOS‑ready. The open question is
**media** (§3), not signalling.

### 1.4 The macOS UI that is *logic*, not *AppKit*
`MenuPopover.swift` is `#if os(macOS)`‑walled, but most of its body is ordinary
SwiftUI driven by `CallSession`: the status row, dial field, TX/RX `LevelMeter`,
mic‑processing toggles, hold‑to‑talk gesture. That layout/logic ports almost
verbatim to an iOS screen (§7); only the AppKit bits (NSImage app icon,
`NSEvent` spacebar monitor, `NSApp.terminate`) drop out.

### 1.5 The iOS target builds today — verified empirically
```
xcodebuild -scheme "astar (iOS)" -destination 'generic/platform=iOS' \
  CODE_SIGNING_ALLOWED=NO build
→ ** BUILD SUCCEEDED **   (produces Debug-iphoneos/astar.app)
```
With signing on, the *only* failure is `requires a development team` — i.e. the
code compiles and links for `arm64-apple-ios`; it's purely a provisioning gap
(`project.yml` has no `DEVELOPMENT_TEAM`, line 39‑40). **Building ≠ captures
audio**, though — see §3.

---

## 2. What is macOS‑ONLY (needs an iOS replacement, or is impossible)

| macOS feature | File / evidence | iOS status |
|---|---|---|
| Menu‑bar app (`NSStatusItem`/`AppDelegate`/`StatusItemController`) | `astarApp.swift:18‑59` | **Replace.** iOS is a normal `WindowGroup` app (already the iOS branch, `astarApp.swift:30‑34`). Needs a real windowed UI (§7). |
| `MenuPopover` whole view | `MenuPopover.swift:1` (`#if os(macOS)`) | **Re‑host** its logic in an iOS `CallView` (§7). |
| Status‑item TX/RX tint | `StatusIconState` + status controller | **Drop / re‑imagine.** No menu bar on iOS; surface call state in‑app and optionally via Live Activity / lock‑screen Now Playing later. |
| Global spacebar PTT (`NSEvent.addLocalMonitorForEvents`) | `MenuPopover.swift:258‑270` | **Drop.** No `NSEvent` on iOS; replace with on‑screen hold‑to‑talk + optional hardware PTT (§4). |
| `NSApp.terminate` "Quit" | `MenuPopover.swift:297` | **Drop.** iOS apps don't self‑quit. |
| **Serial PTT (UCI150) via AstarSerial** | `SerialController.swift:1`, `SerialView.swift:1` (both `#if os(macOS)`); `AstarSerial` links IOKit; `project.yml:46‑55` scopes it `platforms: [macOS]` | **IMPOSSIBLE on iOS.** No IOKit, no USB‑serial enumeration, no public serial API even over USB‑C. **There is no UCI150 serial handset on iOS.** Verified: AstarSerial is never linked into the iOS target (the iOS build succeeded without it). |

**Consequence:** the iOS app is a *headset/handheld* client. PTT is on‑screen
(or a BLE/MFi accessory — §4), never a wired serial radio interface.

---

## 3. Does the audio actually run on iOS? — the critical question

**Verdict: builds & links, but capture/playback is NOT proven and is very
likely broken without an engine change. Treat as "needs iaxclient‑rs work +
an on‑device spike."**

### Evidence
1. **The backend is `cpal`.** `crates/astar-audio/Cargo.toml:9` —
   *"Audio I/O for iaxclient. cpal-backed."*; dependency `cpal = "0.15"`
   (`astar-lib/Cargo.toml:44`, locked to `cpal 0.15.3` in `Cargo.lock`).
2. **No iOS‑specific audio handling.** `crates/astar-audio/src/stream.rs`
   builds the backend with `cpal::default_host()` and comments only about
   *"CoreAudio on macOS, WASAPI on Windows, ALSA/PipeWire on Linux"*
   (`stream.rs:204‑212`). There is **no iOS branch, no AVAudioSession setup,
   no RemoteIO category configuration** — `grep` for `target_os`, `ios`,
   `AVAudioSession`, `RemoteIO` across the audio crate finds only macOS/CoreAudio
   comments.
3. **`AVAudioSession` is configured *nowhere* in the entire stack.** A repo‑wide
   `grep -rn 'AVAudioSession|avfaudio|set_active|RemoteIO'` over
   `astar-lib/{crates,bindings,scripts}` returns **zero hits**, and the
   astar Swift app has no audio‑session code either.

### Why that's a blocker on iOS specifically
cpal *does* support iOS — its CoreAudio host runs on iOS via the AudioUnit
**RemoteIO** unit. **But on iOS the OS will not grant microphone input to
RemoteIO until the app has an `AVAudioSession` in a record‑capable category
(`.playAndRecord`) that has been `setActive(true)`.** On macOS there is no
audio session — CoreAudio just opens the device — which is exactly why this has
"worked" on macOS and why nobody has needed a session yet. On iOS, with no
session activated, `input_devices()`/stream build will at best return no input
or fail at stream start, and there is no route to the receiver/speaker without a
category either.

cpal itself does **not** create or activate the session — that is, by Apple's
design, the *app's* responsibility. So one of two places must own it:

- **(preferred) The app**, before constructing/connecting the `Station`: link
  `AVFAudio`, set `.playAndRecord` with `.voiceChat`/`.videoChat` mode +
  `.defaultToSpeaker`/`.allowBluetooth`, `setActive(true)`, and handle
  interruptions (§5). This can be done **entirely in astar's iOS layer with no
  Rust change** — and is the fastest path to prove it.
- **(alternative) iaxclient‑rs**, if the engine should own its session. More
  correct long‑term (the engine knows when it wants the mic) but a bigger change.

### Recommended classification: **(b) needs work, then (a) spike**
- Most likely the app can drive an `AVAudioSession` from Swift and cpal's
  RemoteIO will then capture/play — **no Rust change required.** That is the
  thing to prove first (M1).
- If RemoteIO still misbehaves (sample‑rate / buffer‑size negotiation against a
  live session, route changes, Bluetooth SCO), file an **iaxclient‑rs ticket**
  for a first‑class iOS audio path (session‑aware backend, or an explicit
  `AVAudioEngine`/RemoteIO backend). 
- **Action:** file iaxclient‑rs ticket *"iOS audio: AVAudioSession ownership +
  RemoteIO capture/playback verification (cpal 0.15 on iOS)"* and gate M2+ on
  the M1 spike result.

**Bottom line:** do **not** assume audio works because the app builds. The
build proves linkage only. M1 must be a device spike that gets the **parrot
(55553) loopback** audible on a real iPhone.

---

## 4. iOS PTT options without serial

- **On‑screen hold‑to‑talk — WORKS.** The macOS `pttButton` is a SwiftUI
  `DragGesture(minimumDistance: 0)` driving `session.setPTT`
  (`MenuPopover.swift:217‑234`) — fully portable; just enlarge it for touch.
  This is the baseline iOS PTT.
- **VOX — present but currently BROKEN.** `VoxGate` + `CallSession.poll`
  (`CallSession.swift:112‑118`) are wired, but VOX doesn't key because the
  engine appears to meter TX level **only while keyed** — chicken‑and‑egg.
  Tracked in **au `astar-e9ff`** ("VOX not working: root‑cause + design
  review"), whose spec names exactly this hypothesis. **Do not rely on VOX for
  iOS** until `astar-e9ff` lands an engine fix (continuous unkeyed mic
  metering or a native engine VOX mode). On a phone in a pocket VOX is also
  undesirable (false keying).
- **Hardware PTT accessories:**
  - **BLE PTT buttons** (e.g. AINA, B01, Zello‑style pucks) — connect via
    CoreBluetooth as a GATT peripheral or HID. Most expose either a HID
    keyboard‑style keypress or a vendor GATT characteristic. Viable, but each
    model is bespoke; pick 1–2 to support (open question §9). No special
    entitlement for BLE beyond the usage string.
  - **MFi / Made‑for‑iPhone PTT** — requires the MFi program; out of scope for
    a hobby app.
  - **Wired/Bluetooth headset remote** (play/pause) — capturable via
    `MPRemoteCommandCenter`, but it's a toggle, not hold‑to‑talk; awkward for
    PTT. Possible "toggle TX" affordance only.
  - **Volume‑button capture for PTT** — technically possible but **App Store
    rejection risk** (private/abusive use of hardware buttons). Avoid for a
    Store build; acceptable only as a TestFlight experiment.
- **CallKit** — *optional, later.* Modeling a node connection as a CallKit call
  gives a system call UI, proper audio‑session priority, and clean interruption
  handling. But CallKit imposes the system in‑call UI and is overkill for an
  amateur‑radio half‑duplex link; revisit only if interruption handling proves
  hard without it.

---

## 5. Background operation

To keep a node connection alive when the app backgrounds or the screen locks:

- **`AVAudioSession` category/mode:** `.playAndRecord` with mode `.voiceChat`
  (or `.videoChat`) and options `.allowBluetooth`, `.allowBluetoothA2DP`,
  `.defaultToSpeaker`. This is the same session §3 requires for capture to work
  at all — so it's not extra work, it's the *same* work.
- **`UIBackgroundModes`:** add **`audio`** (continue I/O in background). The
  **`voip`** mode is largely legacy (its real value was socket‑wake via
  `setKeepAliveTimeout`, deprecated in favor of PushKit); `audio` is what keeps
  the RemoteIO stream running while backgrounded. Add `voip` only if a PushKit
  wake design emerges later. These go in the Info.plist (XcodeGen:
  `INFOPLIST_KEY_UIBackgroundModes`).
- **Interruptions:** observe `AVAudioSession.interruptionNotification` (phone
  call, Siri) — on `.began` unkey/pause, on `.ended` reactivate the session and
  resume. Observe `routeChangeNotification` (headphones/Bluetooth plug/unplug)
  — re‑evaluate input/output and **fail‑safe unkey** (mirror the macOS
  fail‑safe in `SerialController` / `removeKeyMonitor`). A dropped route while
  keyed must never leave TX latched.
- **Screen lock:** with the `audio` background mode + active session, audio
  continues; but the SwiftUI `Timer` poll (`CallSession.start`, 20 Hz) may be
  throttled/suspended when backgrounded. **Open question:** confirm the poll
  loop survives backgrounding, or move polling onto an audio‑driven tick / a
  background‑safe timer so status/PTT stay live. (The engine's media runs on its
  own thread; only the *UI poll* is at risk.)

---

## 6. Permissions / entitlements / App Store

- **Microphone usage string** — already declared:
  `project.yml:75` sets `INFOPLIST_KEY_NSMicrophoneUsageDescription`
  ("astar uses the microphone to transmit on AllStar nodes."). Good — and it's
  *required* the moment the session goes record‑capable.
- **Background audio** — add `audio` to `UIBackgroundModes` (§5). No special
  entitlement; it's an Info.plist key.
- **Local Network permission (`NSLocalNetworkUsageDescription` + Bonjour
  services)** — **likely NOT needed.** The WT/IAX path dials **WAN** hosts
  (AllStar portal over HTTPS, then IAX2/UDP 4569 to the node's public address).
  iOS's local‑network prompt fires only for LAN/multicast/Bonjour discovery,
  which astar doesn't do. **Caveat:** if a user dials a node on the **same
  LAN/subnet**, iOS 14+ may trip the local‑network gate on that UDP traffic —
  worth verifying in the M1/M4 spike; add the string only if observed.
- **CoreBluetooth** (only if BLE PTT ships) — `NSBluetoothAlwaysUsageDescription`.
- **App Store review concerns for an amateur‑radio VoIP app:**
  - VoIP background audio is well‑trodden; fine with proper session + usage
    strings.
  - Amateur radio implies **licensing/identification** norms. Apple may ask
    what the app does; be ready to explain it's a client to the operator's own
    licensed AllStarLink node (auth via their portal account). Not a barrier,
    but have review notes.
  - **Avoid** volume‑button PTT and any private API for hardware buttons (§4) —
    the main rejection risk vector here.
  - Microphone + background audio with a clear purpose string is the standard
    bar; no entitlement approvals needed beyond the capability toggles.

---

## 7. UI plan (SwiftUI, reusing AstarCore)

Replace the placeholder `ContentView` (`ContentView.swift` — currently a
star + label) with a real app. The iOS scene already injects the session
(`astarApp.swift:31‑33`). Adapt `MenuPopover`'s proven layout into screens:

```
NavigationStack
├─ HomeView (root)
│   ├─ StatusHeader   ← statusRow: dot + title + RTT  (MenuPopover.swift:122)
│   ├─ DialField      ← node TextField + Connect       (…:146)  (.keyboardType(.numberPad))
│   ├─ when in call:
│   │   ├─ LevelMeters TX/RX + LevelGraphView          (…:277,281)
│   │   ├─ BIG hold‑to‑talk button (touch‑sized)       (…:217)
│   │   └─ Disconnect
│   └─ toolbar → SettingsView
└─ SettingsView (NavigationStack push / sheet)
    ├─ CredentialsView   ← reuse almost verbatim (portable; CredentialsView.swift)
    ├─ AudioView         ← reuse DevicesView gains; device pickers degrade to
    │                       "System Default" (iOS routes via AVAudioSession, not
    │                       named CoreAudio devices — hide/disable the pickers)
    └─ HardwareView      ← Headset profile only; NO serial UI (SerialView is
                            #if os(macOS)). Show PTT‑mode help instead.
```

- **Dialing/call/meters/toggles** all come straight from `MenuPopover` logic.
- **Settings** = Credentials + Audio (gains, **minus** named‑device pickers) +
  Hardware (Headset only). `SerialView`/`SerialController` are excluded by
  `#if os(macOS)`, so the iOS Hardware screen is just the Headset note.
- **Device pickers:** `DevicesView` enumerates named CoreAudio devices
  (`DevicesView.swift:71‑72` → `session.inputs()/outputs()`). On iOS routing is
  AVAudioSession‑driven (receiver/speaker/Bluetooth), not arbitrary named
  devices — so on iOS surface a **route picker / "System Default"** and keep
  only the gain sliders, which are portable.
- **iPhone vs iPad:** start iPhone‑first, single‑column `NavigationStack`. iPad
  gets the same layout free (compact‑friendly); a `NavigationSplitView`
  (directory sidebar + call detail) is a nice‑to‑have later, not M‑critical.
  `project.yml:73` already targets `TARGETED_DEVICE_FAMILY = "1,2"` (iPhone +
  iPad). Fix the "all interface orientations" build warning (declare supported
  orientations) before Store submission.

---

## 8. Phased build plan

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 — Provisioning** | Set `DEVELOPMENT_TEAM` in `project.yml`; create a bundle id/profile; get a device on the team. | iOS scheme installs to a physical iPhone. |
| **M1 — AUDIO SPIKE (gate)** | Add an `AVAudioSession` (`.playAndRecord`/`.voiceChat`, activate) in the iOS app layer **before** `Station` connect. Dial the **parrot 55553** guest. Prove mic→parrot→speaker loopback **on a real device**. If RemoteIO won't capture against the session, file the iaxclient‑rs iOS‑audio ticket and spike an engine‑side fix. | **You hear yourself back from the parrot on an iPhone.** Everything below is blocked on this. |
| **M2 — Dial + call UI** | Build `HomeView`: status, numeric dial field, Connect/Disconnect, TX/RX meters, big hold‑to‑talk. Port `MenuPopover` logic. | Connect to a node, hear RX, key TX with the on‑screen button. |
| **M3 — Settings + persistence** | `CredentialsView` (reuse), Audio (gains + route, no named devices), Hardware (Headset only). Keychain creds + UserDefaults audio prefs (already portable). | Save account, dial via WT (on‑air); prefs survive relaunch. |
| **M4 — Background + interruptions** | `audio` background mode; interruption + route‑change handlers with fail‑safe unkey; verify poll loop survives background/lock; verify local‑network behavior for LAN nodes. | Call stays up screen‑locked; phone call interrupts and resumes cleanly; never latches TX on route loss. |
| **M5 — Hardware PTT accessories** | CoreBluetooth integration for 1–2 chosen BLE PTT buttons; map to `setPTT`. | A BLE puck keys/unkeys the call. |
| **M6 — Polish + Store/TestFlight** | Orientation fix, icons, review notes, design polish (Rob's bar — au pp‑d817). | TestFlight build; Store submission if desired. |

VOX is intentionally **not** a milestone — it's blocked on **au `astar-e9ff`**
and is a poor fit for a phone regardless.

---

## 9. Open questions for the user

1. **Device & provisioning:** Is there a physical iPhone available for testing,
   and an Apple Developer account / team to set `DEVELOPMENT_TEAM` for on‑device
   builds? (M0/M1 are blocked without both — the simulator can't validate the
   RemoteIO mic path meaningfully.)
2. **Audio‑session ownership:** Are we OK owning the `AVAudioSession` in the
   astar **app** layer (fastest, no Rust change), or do you want it pushed into
   **iaxclient‑rs** as a proper iOS audio backend (cleaner, bigger)? This
   decides whether M1 is app‑only or also files/blocks on an engine ticket.
3. **iPad:** Ship iPad as a first‑class target now (same codebase, minor layout
   work) or iPhone‑only initially?
4. **Target iOS version:** Keep `iOS 16.0` (`project.yml:12`) or raise (e.g. 17)
   for newer SwiftUI / Live Activities? Lowering to 13 (the AstarStation package
   floor) buys little.
5. **BLE PTT hardware:** Which specific accessory(ies) should M5 support (AINA,
   B01, a particular Zello‑compatible puck)? Each is a bespoke GATT/HID
   integration; we should pick 1–2.
6. **Distribution:** App Store, or TestFlight‑only? (Affects how strict we are
   about volume‑button PTT avoidance, review notes, and orientation/polish.)
7. **CallKit:** Do you want system call UI / lock‑screen call treatment
   (CallKit), or keep it a plain in‑app audio app? (Affects M4 design.)
8. **Local‑network nodes:** Do users dial nodes on the same LAN? If so we must
   handle the iOS local‑network permission; if it's always WAN, we can skip it.
9. **VOX on iOS:** Even after `astar-e9ff` fixes the engine, do we want VOX on a
   phone at all (false‑keying risk in a pocket)? Default recommendation: omit.

---

## Appendix — how key claims were verified

- **iOS builds:** `xcodebuild -scheme "astar (iOS)" -destination
  'generic/platform=iOS' CODE_SIGNING_ALLOWED=NO build` → `** BUILD SUCCEEDED **`
  (produced `Debug-iphoneos/astar.app`). With signing on, the only error is the
  missing development team.
- **Serial impossible on iOS:** `AstarSerial` links IOKit and is scoped
  `platforms: [macOS]` (`project.yml:46‑55`); `SerialController`/`SerialView`
  are `#if os(macOS)` (`SerialController.swift:1`, `SerialView.swift:1`); the
  iOS build linked and ran the validation step without AstarSerial.
- **Audio backend = cpal, no iOS session:** `astar-audio/Cargo.toml:9`
  (cpal‑backed), `Cargo.toml:44` (`cpal = "0.15"`), `stream.rs:204‑212`
  (default host, macOS/Windows/Linux comments only), and a repo‑wide grep for
  `AVAudioSession|RemoteIO|set_active` across iaxclient‑rs returning **zero
  hits**.
- **xcframework has real iOS slices:**
  `bindings/swift/astar.xcframework/ios-arm64/libastar_sys.a` (+
  simulator) present; `build-xcframework.sh:60‑61,110‑117` builds
  `aarch64-apple-ios` / `aarch64-apple-ios-sim`.
- **AstarCore is platform‑neutral:** no `import AppKit/UIKit/IOKit` and no
  `os(macOS)/os(iOS)` in `Packages/AstarCore/Sources/AstarCore/*.swift`.
- **VOX broken:** au `astar-e9ff` spec (mic metered only while keyed).
