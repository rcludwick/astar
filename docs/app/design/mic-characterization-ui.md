# Mic characterization UI — design spec

_astar nugget: `astar-ee09`. Builds on the division-of-labor note in
`mic-characterization.md` and the engine spec
`astar-lib/docs/superpowers/specs/2026-06-20-mic-characterization-design.md`.
Date: 2026-06-22._

## Status: unblocked

The engine FFI has **landed and is vendored** in `AstarStation`. `Station.swift`
already exposes the five methods this feature consumes:

- `monitorStart(input:)` / `monitorStop()` — open the mic lane with no call (`iax-2377`).
- `micSpectrum() -> [Float]` — peak-held dBFS bins (`-120…0`), log-spaced
  ~100 Hz–3.9 kHz, length `IAX_SPECTRUM_BINS`, polled ~20 Hz (`iax-e73e`).
- `characterize(harmonicComb:) -> String` — opaque, secret-free `MicProfile` JSON
  (noise floor + harmonic notch comb), `harmonicComb` default off (`iax-5fb6`).
- `setMicProfile(_ json: String?)` — apply (or clear with `nil`) a profile,
  rebuilding the live noise-reduction comb (`iax-2095`).

So astar is a pure draw-and-persist consumer: it renders the engine's spectrum,
triggers `characterize()`, and persists/recalls the resulting JSON per device. It
computes no FFTs and never parses the profile JSON for application.

## Scope decision

Characterization is a property of the **physical microphone**, so the per-device
`MicProfile` owns **only** the characterization. The mic **gain / compression /
noise-reduction** the user tunes per saved config (`Setup`) and live
(`AudioSettings`) are unchanged — this feature is purely additive. (Two configs on
the same mic can keep different gains while sharing one characterization.)

The mic profile is **exposed** (status + Analyze link + Apply toggle) wherever a
mic is chosen — simple settings and each saved config — and **created** in a
dedicated Mic Analyzer window.

## 1. Data model (`AstarCore`)

`MicProfile` is currently unused scaffolding (`{deviceName, inputGain,
noiseReduction, compression}`, none wired into the app). Repurpose it as the
per-device characterization record:

```swift
public struct MicProfile: Codable, Equatable {
    public var deviceName: String              // key
    public var characterizationJSON: String?   // opaque engine blob; nil = uncharacterized
    public var enabled: Bool                    // apply on mic-select? default true
    public var characterizedAt: Date?           // for "characterized 2d ago" display
}
```

`MicProfileStore` is unchanged: keyed by device name, all profiles as one JSON map
under `audio.micProfiles` (`UserDefaultsMicProfileStore`). The
`characterizationJSON` is stored opaquely; astar parses it only for a small
read-only readout (noise floor + notch list), never for application. Existing
`MicProfileStoreTests` are updated for the new shape; `Optional`/added fields keep
Codable forgiving.

## 2. Engine seam (`StationDriving` + fakes)

The real `Station` already implements the five methods; the protocol + fakes do
not. Add to `StationDriving`:

```swift
func monitorStart(input: String?) throws
func monitorStop() throws
func micSpectrum() throws -> [Float]
func characterize(harmonicComb: Bool) throws -> String
func setMicProfile(_ json: String?) throws
```

- `Station` conforms for free.
- `NullStation`: monitor/`setMicProfile` no-op; `micSpectrum() -> []`;
  `characterize() -> ""`.
- `FakeStation` (tests): settable `spectrumToReturn: [Float]` and
  `characterizeJSON: String`, plus call recording (`setMicProfileCalls: [String?]`,
  monitor start/stop flags) so the view-model + recall are TDD-able with no audio.

## 3. Mic Analyzer window

A **separate, resizable `NSWindow`** opened from Settings via a window controller
(the app is `LSUIElement`; opening activates the app and shows the window — same
pattern as the existing menu-bar popover window). It hosts `MicAnalyzerView`,
driven by a `MicCharacterization` `@MainActor ObservableObject`:

- **Lifecycle:** on appear → `monitorStart(input: selectedDevice)` when no call is
  active (if a call is live, the engine shares the live mic path — it guards
  double-open). On close → `monitorStop()`. A ~20 Hz timer polls `micSpectrum()`
  into `@Published spectrum: [Float]`.
- **Spectrum view:** a SwiftUI `Canvas` drawing the bins as an area/bar plot —
  **log frequency x-axis** (engine already log-bins; label 100 Hz / 500 / 1k / 2k /
  3.9k), dBFS y-axis (−120…0). The engine provides peak-hold, so silence peaks
  persist.
- **Controls:** a mic picker (defaults to the current input);
  **"Analyze (stay silent)"** → `characterize(harmonicComb:)` → read-only readout
  (noise floor dBFS + detected notch frequencies); **"Save mic profile"** persists
  `characterizationJSON` (+ `characterizedAt`) to that device's `MicProfile` and
  applies it immediately; an **"Harmonic comb (experimental)"** advanced toggle,
  **default off** (matches the engine's gating until validated against a real
  fake-Icom recording).

## 4. Exposure surfaces (simple settings + saved configs)

Both surfaces show a compact, read-mostly row reflecting the **selected input
device's** `MicProfile` (no data duplication):

- **Simple settings (`QuickConfigView`)** — under the Mic gain row:
  `Mic profile: ✓ characterized · [Analyze…] · Apply ⃝` (or
  `⊘ not characterized · [Analyze…]`). "Analyze…" opens the Analyzer window
  defaulted to the current input; the Apply toggle is the per-device `enabled`
  flag.
- **Saved config card (`ConfigCard`, expanded)** — next to the Input picker, the
  same row for *that config's* input device.

Because characterization is per-device, two configs (or the live setting) on the
same mic show the **same** status and share the `enabled` flag — flipping Apply
from any surface flips the one per-device flag. A one-line caption on first display
sets the expectation ("Mic profiles are saved per microphone").

## 5. Apply / recall wiring (`CallSession`)

`CallSession` gets an injected `MicProfileStore` (default
`UserDefaultsMicProfileStore()`, mirroring `audioStore`) plus:

- `setMicProfile(_ json: String?)` — passthrough to the station.
- `applyMicProfile(forInput device: String?)` — look up the store; if
  `enabled && characterizationJSON != nil` → `station.setMicProfile(json)`, else
  `setMicProfile(nil)` (clear to the generic reducer).

`applyMicProfile` is the single recall path, fired on:

- **input device change** — after `selectDevices` in `QuickConfigView`;
- **applying a saved config** — `SetupController.apply`, after `selectDevices`;
- **call start** — so a fresh call rebuilds the comb;
- **Analyzer Save** — immediate apply for the current mic.

## 6. Testing, phasing, errors

- **TDD (`AstarCore`):** `MicProfile` round-trip + Codable back-compat;
  `CallSession.applyMicProfile` calls `setMicProfile(json)` vs `nil` based on
  `enabled` + presence (FakeStation); the `MicCharacterization` view-model's
  characterize→save flow (scriptable FakeStation). The `Canvas` spectrum render is
  visual — verified on-device, not unit-tested.
- **Phasing:** **M1** = engine seam (`StationDriving` + fakes) + Analyzer window
  with live spectrum (monitor + 20 Hz poll + draw). **M2** = characterize + Save +
  per-device persistence + recall wiring + the two exposure rows. Both are
  unblocked (FFI vendored); the split keeps PRs reviewable.
- **Errors:** `monitorStart` failure (device busy / mic permission) → inline
  message in the window; `micSpectrum` throwing → stop polling + show error;
  `characterize()` returning `""` (not enough buffered silence) → "Couldn't
  analyze — try again in a quiet moment."

## Resolved questions

- **Analyzer home:** separate resizable window (not popover tabs / list section) —
  the spectrum needs width + height.
- **Profile scope:** characterization per-device; gain/NR/comp stay per-config.
- **Notch display:** shown read-only (transparency); applied opaquely (no in-app
  editing in v1).
- **Auto-apply:** per-device `enabled` toggle, default on after Save, flippable
  from either exposure surface.
- **Harmonic comb:** advanced toggle, default off.

## Deferred

- iOS: monitor + spectrum need no serial, so this is a strong iOS feature, but it
  rides on the iOS audio spike (`astar-44c3`) landing first.
- Editable / manual notch entry, multi-mic comparison, export — out of scope.
