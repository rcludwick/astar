# Mic characterization (Advanced tab) — astar-side design

_astar nugget: `astar-ee09`. The engine (astar-lib) owns the DSP and exposes
a poll-only FFI; astar renders + persists. Engine design spec:
`astar-lib/docs/superpowers/specs/2026-06-20-mic-characterization-design.md`._

## Division of labor (settled with the iax side, 2026-06-20)

The astar-lib side filed the engine backend and did the FFT/DSP design there,
so **astar does NOT compute FFTs (no vDSP/Accelerate)** — a correction to the
first cut of `astar-ee09`. The engine produces log-binned, peak-held dBFS
magnitudes; astar renders them.

| Concern | Owner | Engine ticket |
|---|---|---|
| Capture mic **without a call** (setup before dialing) | engine | `iax-2377` |
| Live voice-band **spectrum** (windowed FFT → log bins + peak-hold) | engine | `iax-e73e` |
| **Characterize** from silence → noise floor + harmonic notch comb | engine | `iax-5fb6` |
| **MicProfile** JSON get/set (recall rebuilds live NoiseReducer) | engine | `iax-2095` |
| UI (Advanced tab), render spectrum, trigger characterize, **persist/recall profile per device** | **astar** | `astar-ee09` |

## FFI surface astar will consume (once vended into AstarStation)

- `iax_station_monitor_start(input_device)` / `iax_station_monitor_stop()` — open
  the mic lane with no call; no-op/shares the path if a call is active (`iax-2377`).
- `iax_station_mic_spectrum(out_bins*, cap) -> count` — log-binned dBFS bins,
  polled ~20 Hz; a dedicated getter (too big for the scalar snapshot) (`iax-e73e`).
- `iax_station_characterize() -> MicProfile` (secret-free JSON) (`iax-5fb6`).
- `iax_station_set_mic_profile(json)` (`iax-2095`).

## astar plumbing

- **`StationDriving` + `Station` adapter**: add `monitorStart(device:)`,
  `monitorStop()`, `micSpectrum() -> [Float]` (dBFS bins), `characterize() -> String`
  (JSON), `setMicProfile(_ json: String)`. `NullStation`/`FakeStation` no-op /
  scriptable. Mostly passthrough; TDD the bits with logic.
- **`MicProfileStore` (AstarCore, TDD)**: persist the MicProfile JSON **keyed by
  input device name** (UserDefaults or app-support file). Recall on device
  select / call start → `setMicProfile`. Platform-neutral, iOS-ready.
- **Mic-setup view-model**: monitor lifecycle (start on Advanced-tab appear when
  no call is active; stop on disappear) + a published `spectrum: [Float]` polled
  ~20 Hz while visible + `characterize()` action + profile save/recall.

## UI — the Advanced tab

- Settings grows tabs (**Audio / Hardware / Advanced**) — this IA change is part
  of `astar-ee09`. (Today Settings is one scroll of sections.)
- **Spectrum view**: render the engine's log-binned dBFS bins as a bar/area
  spectrum with a **log frequency axis** (engine already log-bins; voice band
  ≤ ~4 kHz, post-resample — "what is actually transmitted") and **peak-hold**
  (engine provides, so silence peaks stay visible). astar just draws the bins.
  Poll via `TimelineView`/timer while visible.
- **Characterize**: an "Analyze (stay silent)" button → `characterize()` → show
  the detected noise floor + notch frequencies; **"Save mic profile"** persists
  the JSON for this input device.
- **Recall**: a saved profile is applied (`setMicProfile`) when its device is
  selected or a call starts — rebuilding the live NoiseReducer comb in the engine.

## Monitor vs in-call

`monitor_start` opens the mic without a call (characterize before dialing). If a
call is active, share the live mic path — the engine guards against double-open.
astar opens monitor when the Advanced tab is shown and no call is active.

## Phasing (astar)

- **M1** — spectrum view: `StationDriving` passthroughs + the view + ~20 Hz poll.
  Gated on `iax-2377` (monitor) + `iax-e73e` (spectrum).
- **M2** — characterize + per-device profile save/recall + `MicProfileStore`.
  Gated on `iax-5fb6` (characterize) + `iax-2095` (profile FFI).
- astar can build `MicProfileStore` + the UI scaffolding against a `FakeStation`
  now (no engine dependency) so it's ready when the FFI lands.

## Open questions (for review)

- **Settings IA**: real tabs vs. a collapsible "Advanced" section?
- **Profile auto-apply**: silently per-device, or opt-in per device?
- **Notch display**: show the suggested notches as an editable list, or apply
  opaquely? (Engine gates the harmonic comb behind a default-off toggle until
  validated against a real fake-Icom recording — see `iax-5fb6`.)
- **iOS**: monitor + spectrum need no serial, so this is a strong iOS feature —
  but it rides on the iOS audio spike (`astar-44c3`) landing first.

## Dependencies

Blocked on the engine FFI: `iax-2377` → (`iax-e73e`, `iax-5fb6`), and `iax-2095`.
Scaffolding (store + UI against a fake) is unblocked.
