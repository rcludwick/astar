# Neural noise suppression — design

**Status:** milestones 1–5 and 7 are built and on `main`. What is left is the
part that needs ears and hardware rather than code: the evaluation (6), the
Pi-class measurement (8), and the two default decisions (9 and the server's).
The default is still **off**, and stays off until milestone 6 has happened. This supersedes backlog item
`iax-267f` (hand-rolled spectral subtraction) — see "What this replaces".
**Read first:** `crates/astar-audio/src/denoise.rs` and
`crates/astar-audio/src/router.rs:1304` (`MicLane::dsp_quantize`). This document
is about moving one stage of that chain, and about the two things that move
breaks.

## What is there today

The mic chain is a chain of hand-tuned classical DSP, and it is fine as far as
it goes.

| Stage | Where | What it does |
|---|---|---|
| Input gain | `router.rs:1304` | Scalar, clamped |
| `HumFilter` | `filter.rs:138` | RBJ biquads: 90 Hz high-pass, then a comb of notches — 60 Hz × 3 harmonics, Q 12, by default |
| `NoiseGate` | `dynamics.rs:199` | Downward expander: threshold −45 dBFS, attack 5 ms, hold 150 ms, release 200 ms, range −35 dB |
| `Compressor` | `dsp_quantize` | Optional, level-driven |
| TX trim | `dsp_quantize` | Always-on final gain (`iax-750a`) |
| f32 → i16, 20 ms framing | `dsp_quantize` | |

`NoiseReducer` (`denoise.rs`) is the first two of those, in that order and for a
reason: the hum filter runs first so its tones do not fool the gate's level
detector. `NoiseReducer::from_profile` rebuilds both from a measured
`MicProfile` — notches found by `characterize.rs` (silence capture → FFT →
spectral-median floor → peaks ≥12 dB over floor, at most 6, Q 20, scanned
100–3800 Hz) and a gate threshold of floor + 6 dB. Hot-swap onto the live lane
is generation-counter guarded (`router.rs:1364`): a relaxed atomic load per
buffer, a mutex and a rebuild only when the control side pushed something new.

**The gate is the weak stage.** It is a level detector with a threshold. It can
only help in the gaps — the moment speech is present, the gate is open and the
noise rides through underneath it at full strength. That is the entire problem
`iax-267f` was written to solve, and it is what a network solves properly.

## Why `nnnoiseless`

`nnnoiseless` 0.5.2 (June 2026) is a pure-Rust port of Xiph's RNNoise: a small
recurrent network over 22 band energies plus cepstral deltas and a pitch
estimate, producing per-band gains. It is not state of the art. It is
*deployable*, which state of the art is not (see "Deferred: DeepFilterNet").

| | |
|---|---|
| Licence | BSD-3-Clause |
| Frame | `DenoiseState::FRAME_SIZE` = 480 samples = exactly 10 ms at 48 kHz |
| API | `DenoiseState::new() -> Box<DenoiseState>`; `process_frame(&mut self, out: &mut [f32], input: &[f32]) -> f32` |
| Return value | A voice-activity probability, free with every frame |
| Scaling | Input and output f32 in **[−32768, 32767]**, *not* ±1 |
| Model | `weights.rnn`, 87,521 bytes, compiled into the crate — no runtime file, nothing to sign, nothing to bundle |
| Allocation | `process_frame` uses stack scratch only (`[f32; 22]`, `[f32; 481]`) |
| Cost | **36.5 µs per 480-sample frame** against a 10 ms budget on an M-series Mac — **0.37 % of one core** |

Two API details are traps and both must be pinned by a test. astar's pipeline is
±1 everywhere; `process_frame` wants i16-scaled floats, so the stage scales in
and out and any drift there is a silent 90 dB gain error. And the docs say to
discard the very first output frame (fade-in artefacts), which means the stage
needs a warm-up that runs somewhere the operator cannot hear it.

### Dependency weight

With `default-features = false` the tree is **five crates astar does not already
have**:

| Crate | Licence | Note |
|---|---|---|
| `nnnoiseless` | BSD-3-Clause | |
| `easyfft` | MIT OR Apache-2.0 | wraps `rustfft`/`realfft`, both already in the lock |
| `generic_singleton` | MIT OR Apache-2.0 | |
| `anymap3` | BlueOak-1.0.0 OR MIT OR Apache-2.0 | via `generic_singleton` |
| `array-init` | MIT OR Apache-2.0 | via `easyfft` |

`rustfft`, `realfft`, `num-complex`, `transpose`, `strength_reduce`,
`primal-check` and `parking_lot` are all present in `Cargo.lock` already.
`astar-audio` already depends on `rustfft = "6"` for `characterize.rs`.

With **default** features on, `nnnoiseless` also drags in `clap`, `hound` and
`dasp` for its own CLI examples — 61 crates. `default-features = false` is
mandatory, not a preference, and is worth a comment in `Cargo.toml` next to the
dependency so nobody "fixes" it later.

**The global-lock worry is resolved, and the answer is not the obvious one.**
`easyfft` caches its FFT plans and scratch buffers through
`generic_singleton::get_or_init_thread_local!` — a `thread_local!` `RefCell`,
not the crate's process-global `get_or_init!`. `parking_lot` appears in the
graph because `generic_singleton` offers that global variant; the path
`nnnoiseless` actually takes never reaches it
(`easyfft-0.4.2/src/dyn_size/realfft.rs:96`). So there is no cross-thread lock
on the audio callback.

What is left is smaller but real: the first `process_frame` **on a given
thread** builds the planner and allocates the scratch buffers. In steady state
it is two `HashMap<usize, Box<[..]>>` lookups per transform and no allocation.
That first call must not be the first live capture callback — which the "discard
the first frame" advice already tells us, and which the warm-up in the design
below handles by construction.

## Where the stage goes

**In the capture callback, at device rate, between the mono downmix and the
resampler** — `stream.rs:781` to `stream.rs:793`.

This falls out of a structural fact that is easy to miss:
`AudioBackend::open_input` (`stream.rs:418`) opens the cpal stream at the
**device's native rate** (`stream.rs:436`) and downsamples to the pipeline rate
with `AntiAliasResampler` *inside the callback* (`process_input`,
`stream.rs:767`). On essentially every Mac and every USB radio interface that
native rate is 48 kHz — exactly what RNNoise was trained on and hard-wired to.
The full-band audio is already there, in the right place, and today it is thrown
away by the resampler before any noise reduction sees it.

### Why not in `dsp_quantize`, where the rest of the chain lives

Because the pipeline rate is 8 kHz (ulaw) or 16 kHz (slin16) —
`manager.rs:378`, `config.sample_rate = policy.max_sample_rate()` — and M17 and
D-Star both pin `StreamConfig::default()`, so 8 kHz is the common case, not the
edge case. Denoising there means upsampling band-limited audio to 48 kHz and
feeding a network whose features are measured across the full band.

That is not a vague objection. RNNoise's band edges are published in the crate
(`nnnoiseless-0.5.2/src/lib.rs`, `EBAND_5MS`): 0, 200, 400, 600, 800, 1 k,
1.2 k, 1.4 k, 1.6 k, 2 k, 2.4 k, 2.8 k, 3.2 k, 4 k, 4.8 k, 5.6 k, 6.8 k, 8 k,
9.6 k, 12 k, 15.6 k, 20 k Hz.

* From an **8 kHz** pipeline, the nine band energies at and above 4 kHz measure
  interpolator output, which is to say they measure nothing. Nine of
  twenty-two features are constant near zero.
* From a **16 kHz** pipeline it is four (9.6 k, 12 k, 15.6 k, 20 k), which is
  better and still wrong.

The 18 cepstral-delta features are derived from those same bands, so the damage
is not confined to nine inputs. And the pitch search runs on a 48 kHz timebase
(`PITCH_MIN_PERIOD` 60, `PITCH_MAX_PERIOD` 768) over harmonic structure that,
above the pipeline's Nyquist, the interpolator invented. A network fed a
distribution it never saw in training does not fail loudly; it fails by
producing plausible-sounding gains that are wrong, which is the worst failure
mode available.

Placing the stage at device rate also puts it before the anti-alias resampler,
which is the right order on its own merits: cleaning broadband noise before
decimation is better than decimating noise and cleaning what survives.

### The seam

`process_input` is inside the cpal backend and knows nothing about `MicLane`,
which is where the `denoise` flag and the profile cells live (`router.rs:1133`).
There are three ways to bridge that and only one of them is cheap.

1. **A default-implemented hook on `InputSink`.** Add
   `fn device_rate_stage(&mut self, samples: &mut Vec<f32>, device_rate: u32) {}`
   with an empty default body, and call it from `process_input` after the
   downmix and before the resampler. `MicLane` implements it and owns the
   `DenoiseState`, the accumulator and the scaling. **This is the
   recommendation.** The trait already exists and is already documented as
   "called from cpal's audio thread; implementations must not block". A default
   body means the roughly twenty `open_input`/`InputSink` test doubles across
   `crates/astar-iax/tests`, `crates/astar-station/tests` and
   `crates/astar-inspect` compile unchanged. The lane keeps one home for its
   control cells, and `write` and the hook run on the same thread, so `&mut
   self` is sound with no new synchronisation.
2. Put an `Arc<AtomicBool>` on `StreamConfig`. Rejected: `StreamConfig` is
   `Copy` and passed by value everywhere; an `Arc` breaks that and the ripple is
   large for no gain.
3. Widen `AudioBackend::open_input`. Rejected: it changes every test double for
   a change none of them care about.

The hook takes `&mut Vec<f32>` rather than `&[f32]` because the stage is not
length-preserving — it holds a remainder. See the next section.

## The 48 kHz guard

RNNoise is not resamplable. 480 samples *is* 10 ms only at 48 kHz, the band
table is in Hz, and the pitch periods are in samples. A 44.1 kHz device is
therefore a real case that needs a decided answer.

**Recommendation, in two parts.**

**Ask for 48 kHz at open time.** `open_input` today calls
`default_input_config()` unconditionally (`stream.rs:434`) and takes whatever
the device's *default* is. Enumerate `supported_input_configs()` first and pick
48 kHz when the device offers it in any of its ranges, falling back to the
default config otherwise. Most devices whose default config is 44.1 kHz still
support 48 kHz; this costs one enumeration at open and nothing per callback.
It is also a small independent improvement: 48 → 8 kHz is a clean 6:1
decimation where 44.1 → 8 kHz is 5.5125:1.

**When the device genuinely cannot do 48 kHz, do not run the network.** Fall
back to today's `HumFilter` + `NoiseGate` and *say so* — a read-only line in the
snapshot naming which chain is live (see "Control surface"). An operator whose
mic sounds different from everyone else's must be able to find out why without
reading source.

**What is deliberately not built:** a 44.1 → 48 → denoise → 48 → 8 kHz double
resample inside the callback. It is possible — `AntiAliasResampler` is right
there — and it is a bad trade. It adds a second rubato instance on the audio
thread, adds that resampler's own chunk latency on top of the 10 ms the
accumulator already costs, and spends all of it feeding the network audio that
has been rate-converted twice, for a device class that is rare on the hardware
astar targets: USB radio interfaces are overwhelmingly 48 kHz native. Revisit
if a real 44.1-only device shows up in a bug report; do not build it on
speculation.

The honest gap: the claim "most 44.1 kHz-default devices also support 48 kHz" is
an expectation, not a measurement. Milestone 2 below settles it by logging
`supported_input_configs()` for every device on the machines we have.

**First measurement (2026-08-29, Rob's Mac mini, `cargo run -p astar-audio
--example capture_rates`).** Five input devices, **all five already default to
48 kHz**: two USB radio interfaces (`USB Audio Device`, `KT USB Audio`) and
three virtual devices (BlackHole and two aggregate devices). Nothing was
rescued and nothing was stuck, so on this machine the preference is a no-op.

That is a weak result, and it should be read as one. It does *not* confirm that
44.1-default devices can be rescued — no such device was present to rescue. What
it does establish is the thing that actually matters for the design: the
hardware astar targets is 48 kHz native, so the guard's fallback path is the
rare case rather than the common one. `USB Audio Device` advertises
`[(44100, 44100), (48000, 48000)]` and picks 48 kHz itself, which is at least
consistent with the expectation. The question stays open until a 44.1-default
device is actually seen.

## The frame accumulator

cpal delivers whatever buffer size the host feels like — 64, 128, 512, 1024
frames, and it can change. RNNoise needs exactly 480. So the stage is a small
accumulator, not a filter:

```
pending: Vec<f32>      // device-rate samples not yet a full frame
frame_in:  [f32; 480]  // scaled to i16 range
frame_out: [f32; 480]
out: Vec<f32>          // denoised samples ready for the resampler
```

Per callback: append the downmixed samples to `pending`; while
`pending.len() >= 480`, take 480, scale ×32767 into `frame_in`, `process_frame`,
scale ÷32767 out of `frame_out` into `out`; keep the remainder in `pending`;
hand `out` to the resampler. `pending` never exceeds 479 samples between
callbacks.

Both vectors are sized once, off the audio thread, at lane construction. Reserve
generously — 100 ms at 48 kHz is 4800 samples and costs 19 KB — and accept that
a pathological host buffer would amortise a realloc rather than imposing a hard
cap that would drop audio. Nothing here allocates in steady state.

**Warm-up.** `DenoiseState::new()` allocates on the control thread, but the FFT
planner and `easyfft`'s scratch caches are thread-local and initialise on first
use. Both problems have the same fix: on the first callback after the stream
opens, run one frame of silence through `process_frame` and discard the output.
That builds the planner, allocates the caches, and discards the documented
fade-in frame in one step. It happens once per stream open, before the operator
has keyed anything.

**Latency: up to ~20 ms on TX, ~15 ms typical.** *(Corrected 2026-08-29; this
section originally said 10 ms worst case, counting only the accumulator.)*

There are two costs, not one. The accumulator holds up to 479 samples, so 0 to
just under 10 ms. On top of that **the network itself adds exactly one frame**:
`process_frame` returns the *previous* frame's completed overlap-add, so output
sample *j* carries input sample *j*−480. Measured against an aperiodic chirp,
correlation is 0.9364 at a lag of exactly −480 samples and 0.0118 at lag 0.

So the total is up to about 20 ms, averaging around 15. It applies to TX only —
RX is untouched — and it does not delay PTT keying, only the audio behind it.

Measure this with an **aperiodic** probe. A harmonic test signal is periodic, so
a correlation search cannot resolve a lag beyond one period; two attempts here
returned confident wrong answers (−238 and −478 samples) before the chirp
settled it.

**Is that acceptable?** Yes, though the corrected figure makes the argument
tighter than it was. The TX path already quantises to 20 ms voice frames
(`VOICE_FRAME_MS`), so the added delay is about one frame time rather than
comfortably under it; there is no sidetone
through the mic lane, so the operator never hears their own voice through this
delay; and half-duplex PTT over a network leg already carries well over 100 ms
end to end. The one genuinely affected thing is the pre-roll ring (`iax-2733`),
which retains post-DSP frames on key-up — its contents shift 10 ms earlier
relative to the key edge, which if anything captures slightly more of the onset.

## What happens to `HumFilter` and `NoiseGate`

**Keep the hum filter. Retire the gate when the network is running.**

### Keep the hum filter, where it is, at pipeline rate

Not out of caution — because it does something the network does not.

A cheap mic's narrowband whine is device-specific, steady, tonal and sits *in
the speech band*: the reference mic in `characterize.rs` whines around
240–600 Hz (~588 Hz at 48 kHz) and runs to its 5th/6th harmonic, ~2940/3528 Hz
— which is why `MAX_NOTCHES` is 6. A general-purpose speech/noise
discriminator has no reason to call a stable in-band tone "noise"; it is
narrowband, harmonically related and periodic, which is what voiced speech also
looks like to a band-energy feature set. RNNoise may attenuate it, may not, and
will not tell us which. A measured notch at 12 dB over the floor removes it
deterministically for the cost of one biquad, and `characterize.rs` already
knows where to put it.

This matters more for astar than for a general audio product: a residual tone
that an FM ear would forgive still corrupts a low-bitrate vocoder, which is
exactly the reasoning already written down at `characterize.rs:49`.

It also stays at pipeline rate rather than moving up to 48 kHz, because the
profile that configures it is *measured* at pipeline rate — `characterize.rs`
scans 100–3800 Hz explicitly "staying under the 4 kHz Nyquist". Moving the
notches to device rate means re-deriving the characterization, invalidating
every saved profile, and buying nothing.

### Retire the gate

The gate and the network are the same job done twice, and the second one is
worse:

* The gate only acts in the pauses. That was its known limit and it is precisely
  what the network fixes.
* Its thresholds become meaningless. Both the fixed −45 dBFS default and the
  profile's `gate_threshold_db` (floor + 6 dB) are calibrated against the *raw*
  noise floor. Downstream of a denoiser the floor has moved, by an amount that
  varies with content. Applying a threshold derived from one signal to a
  different signal is not conservative; it is a bug that happens to be quiet.
* Its characteristic failure — chopping a soft onset — is documented in this
  repository as the reason the VOX meter tap had to be moved ahead of it
  (`iax-5c30`, `router.rs:1373`). Keeping it downstream of a network that
  already handled the noise means paying that cost for nothing.

Concretely: `NoiseReducer` grows an optional gate (`gate: Option<NoiseGate>`, or
a `from_parts` variant that takes none) and `MicLane` builds it without one when
the device-rate stage is actually running. The gate stays fully intact for the
fallback path — network off, or a device that could not give us 48 kHz — so
nothing is deleted and the old behaviour is one flag away.

## VOX: the tap stays ahead of the network

This is the part of the change that can break something the operator has already
tuned, and it deserves the most care.

The mic input meter is deliberately tapped **post-gain, pre-`NoiseReducer`**
(`router.rs:1373`), so VOX can key from a soft speech onset that the gate would
otherwise suppress. That is `iax-5c30`, and it was a deliberate correction.
Putting RNNoise in the capture callback puts it **upstream of that tap** — VOX
would see denoised audio unless something is done about it.

**Both directions, honestly:**

*In favour of VOX on denoised audio.* Fewer false keys. A fan, traffic, or a
neighbour's mower currently pushes the raw peak over the threshold and keys the
transmitter with nothing on it. That is a real and annoying failure, and the
network is very good at exactly that class of noise. RNNoise also hands back a
voice-activity probability per frame, which is a genuinely better VOX detector
than a peak meter — it is the thing VOX has always been approximating.

*Against.* Three reasons, and the third is decisive.

1. It reintroduces the problem `iax-5c30` fixed. The network attenuates the
   onset until its own recurrent state decides speech has started, and that
   decision has lag. VOX would key late and clip the first syllable. The
   pre-roll ring rescues audio that was captured; it cannot rescue a key that
   never happened.
2. The attenuation is time-varying and content-dependent, so the relationship
   between "how loud I spoke" and "what VOX saw" stops being monotone in any way
   the operator can build intuition about.
3. **`voxThresholdDBFS` is persisted, and it is calibrated against the raw
   floor.** It defaults to −40 (`CallSession.swift:112`), the operator tunes it,
   and `VoxBackgroundCheck` measures the background floor *from this same tap*
   and warns when the threshold does not clear it by 6 dB. Put a denoiser
   upstream and a saved threshold silently changes meaning — the floor it was
   calibrated against drops, and it drops only while the noise-reduction
   checkbox happens to be ticked. A user's stored number that means one thing on
   Monday and another on Tuesday because they toggled an unrelated control is
   precisely the situation `CLAUDE.md`'s config-version rule describes as owing
   a translation. A noise-reduction feature should not be spending a config
   version number.

**Recommendation: keep the VOX tap on raw audio.** Publish `mic_input_peak` from
the new device-rate hook, *before* the network runs, rather than from `write`.
Gain is a scalar, so `g × peak(pre-resample)` and `g × peak(post-resample)`
agree to within resampler ripple; semantics are preserved and
`VoxBackgroundCheck`, the threshold slider and every saved value keep meaning
what they meant.

**How to verify:** a test on the null backend that pushes a fixed onset through
a lane with the stage on and with it off and asserts `mic_input_peak` is
identical in both runs. If that test can be made to pass, the invariant is real;
if it cannot, the tap moved and we would rather find out from CI than from a
user whose VOX stopped working.

**And then: surface the VAD, do not substitute it.** `process_frame` returns a
voice-activity probability for free. Publish it as telemetry from day one — it
costs an atomic store — so the question "would VAD-driven VOX be better than
peak-driven VOX?" can be answered from real recordings instead of argued. If the
answer is yes, that is its own change, with its own control and its own
migration for the saved threshold. It is not something to slip in under a
checkbox labelled "Noise reduction".

## Strength

RNNoise has **no strength parameter**. It emits per-band gains from a trained
model; there is no knob inside it. The control is a dry/wet mix,
`(1-s)·dry + s·wet`, which limits the maximum attenuation — which is the useful
thing, because over-suppression dulling speech is the failure mode to dial back
from, and it is the same shape as the compressor's existing strength slider.

**The mix must be delay-compensated, and this is the trap.** The wet path lags
the dry by exactly one frame (see "The frame accumulator"), so mixing them
as-is combines two essentially uncorrelated signals — correlation 0.0118 at
lag 0 against 0.9364 at −480. That is a 10 ms slapback with severe comb
filtering, not a strength control. The stage therefore runs the dry path
through a one-frame delay line before mixing: 480 floats, and no added latency,
because the wet path already pays those 10 ms.

The test that pins it is `strength_zero_returns_the_input_unchanged`: at `s = 0`
the output must equal the input sample for sample, which is only true when the
compensation is exactly right. Note that the withheld fade-in frame and the
one-frame lag cancel, so bypass output starts at input sample 0.

A caveat for whoever evaluates this: on synthetic xorshift white noise RNNoise
only takes about 0.9 dB off (1.4 dB over four seconds). It is trained on
real-world noise and a uniform PRNG is not that, so **synthetic signals are not
a proxy for how much this helps**. That has to come from real recordings.

## Control surface

**It rides the existing checkbox. No new user-facing control.**

`audio.noiseReduction` (`AudioSettings.swift:115`, default **off**) already
means "clean up my mic", and it already reaches the engine through
`Station::set_noise_reduction` → `set_denoise` → the lane's `denoise` atomic
(`station.rs:1671`, `router.rs`). The M17 per-network override
(`m17.audio.noiseReduction`, `CallSession.swift:377`) inherits it for free. The
Iced client's `settings.noise_reduction` (`apps/gui/src/settings.rs:121`) does
too. Nothing new is persisted, so **no `ConfigVersion` bump** — this adds no
field, renames nothing and inverts no meaning.

A ham who ticks "Noise reduction" wants less noise. They do not want a DSP menu.
If a menu is ever right, `iax-4af7` ("selectable denoise filters", `docs/BACKLOG.md:946`)
is where that argument belongs, and this design deliberately does not pre-empt
it — though it does answer a large part of its research question, and `iax-4af7`
should be re-read once this ships. `iax-0465` (`DspStage` trait + `DspChain`) is
the refactor that would make a menu cheap; this design does not need it, and
adding one more stage is not the "enough stages to justify the abstraction"
threshold that item sets for itself.

Three things that are *not* controls:

* **A capability line, read-only.** In the snapshot and behind the advanced
  disclosure: which chain is live and why — `neural (48 kHz)` versus
  `filter + gate (device 44100)`. Without it, the 48 kHz guard is invisible and
  its failure is indistinguishable from a bug.
* **The VAD probability**, as telemetry, per the previous section.
* **A developer A/B override.** `ASTAR_MIC_DENOISE=neural|legacy|off`, read once
  at stream open, no UI. This follows the `IAX_THUMBDV_PORT` precedent: an
  environment variable that narrows behaviour for people who know why they are
  setting it. It exists so the quality evaluation below can be run as a real
  A/B, and so a user with a broken mic can be asked to try one thing.

The default stays **off**. Turning it on by default is a separate decision, and
it should be made after the evaluation, not as part of shipping the code.

## Cross-platform

It is pure Rust with the model compiled in, so `apps/gui` (Iced,
Windows/Linux) and `crates/astar-server` inherit it with no per-platform work
and no bundled resource. Nothing to sign, nothing to staple, no path to resolve
at runtime, ~85 KB of binary growth. That is most of why this crate was chosen
over a better one.

**The CPU budget on a Pi-class machine is an estimate, and should be labelled as
one.** 36.5 µs/frame is a measurement on Apple silicon and nothing else. A
naive scaling to a Cortex-A76 would be somewhere in the low single-digit percent
of one core, but "somewhere in the low single-digit percent" is a guess dressed
as a number and `astar-server` is the deployment where being wrong matters —
a single-node VPS or a Pi running the node daemon has no headroom to spare and
no operator watching a CPU meter.

**How to measure it, rather than guess:** `crates/astar-audio/examples/denoise_bench.rs`
now exists — a `RnnoiseStage`, a deterministic harmonic-plus-hiss signal, and a
loop with an `Instant`. Run it with `--release`; a debug build measures the
wrong thing by an order of magnitude.

Re-measured on Rob's Mac mini through the real stage rather than a bare
`DenoiseState`: **35.7 µs per frame, 0.357 % of one core** over 2,000 frames,
against the 36.5 µs the scratch spike reported. The accumulator, the two
scalings and the copy-back are inside that number and cost nothing detectable.
Build it for `aarch64-unknown-linux-gnu`, run it on
the actual target, read the number. Do that before the stage is enabled anywhere
`astar-server` runs, and record the result in this document. Until then the
server's default should be whatever the client's is — off — and the milestone
that turns it on for the server is gated on that measurement existing.

## Licensing

`nnnoiseless` is **BSD-3-Clause**; astar is AGPL-3.0-only. BSD-3-Clause is
GPL-compatible, so the combination is fine and the resulting binary is
distributable under the AGPL. What BSD-3 asks in return is attribution:
the copyright notice, the licence text and the no-endorsement clause must be
reproduced in the documentation or other materials accompanying a binary
distribution.

The notices are not just Joe Neeman's — `nnnoiseless-0.5.2/COPYING` carries
five copyright lines: Joe Neeman (2020), Mozilla (2017), Jean-Marc Valin
(2007–2017), the Xiph.Org Foundation (2005–2017), and Mark Borgerding
(2003–2004). All five ship together; the Xiph no-endorsement clause names the
Foundation specifically.

This repository already has an established pattern for exactly this and the
obligation should be discharged through it, not invented fresh:

| Where | What to add |
|---|---|
| `README.md`, "Third-party components" table (line ~337) | A row for `nnnoiseless`. Note this table currently lists *paths* in the repository; `nnnoiseless` is a crates.io dependency rather than vendored source, so the row needs a "Dependency" convention or a second short table. `codec2` is already listed this way, so the precedent exists. |
| `docs/site/about/license.md`, "Third-party components" (line ~40) | The matching row. That file says explicitly that it mirrors the README table and that the shipped licence files win on disagreement — so the two must be edited together. |
| `LICENSE-EXCEPTIONS.md` | A short section reproducing the five copyright lines and the BSD-3 text, in the style of the existing Codec 2 section. This is the "documentation and other materials" the licence asks for. |

Two things this is **not**. It is not a vendoring case: `vendor/ambe-thumbdv`
lives under `vendor/` because it is *first-party source under different terms*
and the licence boundary needed to be visible in the tree
(`vendor/ambe-thumbdv/VENDORED.md`). `nnnoiseless` is an ordinary upstream
dependency and belongs in `Cargo.toml`. And it is not a Codec 2 case: BSD-3
imposes no relink obligation and no app-store conflict, so the additional
permission in `LICENSE-EXCEPTIONS.md` §1 is not implicated. There is nothing
here that complicates a signed, notarized `.dmg`.

Worth confirming while doing this: whether `ci/guard-spdx-headers.sh` or
`ci/guard-codec2-licensing.sh` need to know about a fourth non-AGPL component.
They probably do not — this adds no first-party file — but the guards are the
place a licensing mistake would be caught, so read them rather than assume.

## How quality gets evaluated

Cost is measured. **Quality is not, and nothing in this document should be read
as claiming it is.** RNNoise is well regarded on conference-call audio; that is
not the same as being good on a ham's cheap electret through a UCI150 into a
half-rate vocoder.

Everything below runs offline or on `127.0.0.1`. Nothing transmits.

**1. Offline WAV A/B.** The primary artefact. Take real captures — noisy speech
recorded by Rob at 48 kHz through the actual hardware — and run each through
three chains: today's `HumFilter` + `NoiseGate`; `HumFilter` alone; and
RNNoise + `HumFilter`. Write all three to WAV. `resample_offline`
(`crates/astar-audio/src/resample.rs:270`) already exists for getting fixtures
to and from the pipeline rate. This is a `cargo run --example`, not a `#[test]`
— it produces files for a human, and a test that asserts nothing is worse than
no test.

**2. The local parrot.** `crates/astar-console/src/parrot.rs` is a pure
mic → speaker loopback with no network at all: record while keyed, play back on
release. With `ASTAR_MIC_DENOISE` it becomes a live A/B the operator can run in
their own shack in ten seconds. This is the fastest useful feedback loop and it
should exist before the offline harness does.

**3. Through the vocoder, which is the test that actually matters.** A denoiser
that trades broadband hiss for musical noise can make a *low-bitrate vocoder*
worse even while making the raw audio better — Codec 2 at 3200 bps and AMBE+2
half-rate both allocate bits by spectral structure, and isolated warbling
tonal artefacts are exactly the structure they will spend bits on. So the
comparison is not denoised-vs-raw WAV; it is
`denoise → encode → decode` versus `raw → encode → decode`. The M17 parrot
(`just m17-parrot`, loopback on `[::]`) and `crates/astar-iax/tests/parrot_loopback.rs`
both provide that round trip without leaving the machine. **If this test
disagrees with test 1, this one wins.**

**4. Objective proxies, as a regression tripwire only.** Noise-floor delta over
gate-closed windows; speech-band energy retention during speech (should be
close to unchanged — a denoiser that also removes 3 dB of voice has not
helped); the VAD probability's agreement with a hand-marked speech/silence
label. These are cheap to compute, easy to assert on, and no substitute for the
next item.

**5. Rob listens, blind, to paired clips.** This is the deciding test. Say so
plainly. No metric in item 4 gets to overrule it, and if the network sounds
worse on his mic than the gate does, the answer is that it does not ship on by
default — which is exactly why the default stays off until this has happened.

## Deferred: DeepFilterNet

`deep_filter` (DeepFilterNet 2/3) is the obvious better answer on quality. It is
also 48 kHz, and it is permissively licensed (MIT/Apache — **verify before
adopting; this was not checked against the crate in this session**).

It is deferred for one reason, and it is a distribution reason rather than a
technical one: it runs its model through the `tract` inference engine and loads
model weights at runtime. For astar that means either embedding several
megabytes in the binary or shipping a resource inside the `.app` bundle that has
to be located, code-signed, notarized and stapled — and located again on
Windows, on Linux, and inside `astar-server`. `nnnoiseless` has an 85 KB const
array and no runtime file at all. That difference is worth more than a few dB of
SNR for a first cut.

Its CPU cost is also unmeasured here. It is certainly heavier than 36.5 µs per
10 ms frame; how much heavier is not something to invent, and the same spike
that measured `nnnoiseless` would measure it in an afternoon.

**Deferred, not dismissed.** The seam this design builds — one device-rate stage
behind one flag, with the accumulator and the scaling already solved — is the
same seam DeepFilterNet would occupy. If the evaluation says RNNoise is not good
enough, swapping the implementation behind that seam is a contained change, and
the milestones below are ordered so that the seam lands before the network does.

## What this replaces

**`iax-267f` — "astar-audio: spectral noise suppression (clean under speech)"**
(P3, `docs/BACKLOG.md:1174`) proposed hand-rolling FFT spectral subtraction:
overlap-add windowing, minimum-statistics noise estimation, spectral gain with a
floor to limit musical noise, slotting into the `NoiseReducer` chain behind the
same checkbox.

The problem statement is correct and unchanged — the gate only helps in the
pauses; something must clean *under* speech. The proposed solution should be
retired in favour of this one:

* It is the same problem with a worse tool. Spectral subtraction with a
  minimum-statistics estimator is roughly what RNNoise's ancestors did, and the
  reason RNNoise exists is that hand-tuned spectral gain produces musical noise
  that is hard to suppress without also dulling speech. `iax-267f` names that
  risk itself ("a floor to limit musical noise") without solving it.
* The tuning burden is the real cost. Every constant in a spectral subtractor —
  window length, overshoot factor, gain floor, estimator time constants — is a
  judgement call that needs listening tests to settle, and there are a dozen of
  them. RNNoise's equivalent constants were settled by training on hundreds of
  hours of speech and noise. We are not going to beat that with a weekend and an
  FFT.
* It would have landed in the wrong place. `iax-267f` says "slots into the
  `NoiseReducer` chain", which is `dsp_quantize`, which is 8 kHz — the same
  band-limited-input mistake analysed above, and it is a mistake for spectral
  subtraction too, just less catastrophically.

What `iax-267f` was right about and this design keeps: the checkbox does not
change, and the new stage supersedes the gate rather than sitting beside it.

## Milestones

Each one is independently testable and each one leaves the tree shippable.

1. **The seam, doing nothing.** Add the default-implemented `InputSink` hook and
   call it from `process_input`. No dependency, no network, no behaviour change.
   Test: an `InputSink` double that records what the hook sees and asserts it is
   the post-downmix, pre-resample buffer at device rate. Proves the twenty-odd
   existing test doubles still compile untouched. **Done.**
2. **48 kHz preference and the capability line.** Enumerate
   `supported_input_configs()` and prefer 48 kHz; report the device rate and
   which chain is live through the snapshot. Still no dependency. Test: a
   backend double advertising 44.1-only, 48-only, and both, asserting the
   selection and the reported capability. This is also where the "do 44.1
   devices offer 48?" question gets its real answer, from logs on real hardware.

   **Done, with one deliberate deferral.** The preference, the fallback and
   `CaptureCapability` are built and tested, and `examples/capture_rates.rs`
   answered the hardware question (see "The 48 kHz guard"). The *snapshot*
   half — carrying the capability through `astar-station`, the C ABI and the
   Swift binding to a read-only line under the advanced disclosure — is not
   built. It is reporting rather than behaviour, it crosses four layers, and
   until milestone 9 turns the default on it would report a constant to an
   operator who has not enabled anything. `MicLane::neural_active()` and
   `CaptureCapability` are the values it needs; it should land with the
   default change.
3. **The stage, off.** Add `nnnoiseless` with `default-features = false`. Build
   `RnnoiseStage` — accumulator, scaling, warm-up, VAD store — as a plain type
   with unit tests and no wiring into the lane. Test: a 480-sample frame of
   known content round-trips with the right scaling; a sequence of odd-sized
   pushes yields exactly the same output as one big push; no allocation after
   warm-up; the first frame is discarded.

   **Done.** Two corrections came out of building it, both measured:

   * *A silence warm-up does not absorb the fade-in frame.* With the warm-up
     and without it, the first real output frame is bit-identical and sits
     25 dB down (0.028 against a 0.5 tone). The artefact is the overlap-add of
     the first frame against an all-zero history, and a frame of silence *is*
     an all-zero history — so warming reproduces the starting condition rather
     than consuming it. The warm-up is kept, because it still moves the FFT
     planner and `easyfft`'s thread-local caches off the first real callback,
     but the first real output frame is now dropped outright, as upstream's own
     example does. Cost: 10 ms of audio, once, at stream open.
   * *Handing the output back by `mem::swap` reallocates forever.* The swap
     avoids a memcpy but gives the stage's reserved buffer away and adopts the
     caller's, which is sized for the host callback rather than for this
     stage's output. Those differ: with 512-sample callbacks the remainder
     gains 32 samples each time, so roughly every fifteenth callback completes
     two frames and writes 960 samples into a 512-capacity buffer. The stage
     copies back instead — at most ~4 KB against a 36 µs network — and the
     reserve stays where `new` put it. The test settles for 64 callbacks
     specifically so it contains a two-frame one; a shorter settle never sees
     the case that reallocates.
4. **The VOX tap moves.** Publish `mic_input_peak` from the hook rather than
   from `write`, still with no denoising in between. Test: the on/off identity
   assertion from the VOX section, which at this milestone is trivially true —
   which is the point. It is true *before* the network is introduced, so when it
   later fails, the network is why. **Done**, and after milestone 5 the
   assertion is no longer trivial: it also checks the network ran and changed
   the audio in the `on` arm, so the identity is tested against something.

   One thing the design did not anticipate: `device_rate_stage` runs only on the
   capture path, so moving the store there outright silently un-meters every
   other driver of `write` — the null backend, the router's own tests, anything
   delivering pipeline-rate audio with no capture callback in front of it. A
   flag set by the hook and cleared by `write` keeps those metered as before.
5. **Wire it up.** `MicLane` runs the stage when the flag is set and the device
   is at 48 kHz; `NoiseReducer` drops its gate in that case. Add
   `ASTAR_MIC_DENOISE`. Test: the VOX identity assertion now with denoising
   live; a lane-level test that the gate is absent when the stage is active and
   present when it is not. **Done.** The gate test walks all four states — off,
   on at 48 kHz, on at 44.1 kHz (the guard), and off again — because the
   interesting one is the third: the flag is set and the gate must still be
   there.
6. **Evaluate.** Parrot A/B first (fastest), then the offline WAV harness, then
   the vocoder round trip. Record the results here.
7. **Licence and docs.** README row, `docs/site/about/license.md` row,
   `LICENSE-EXCEPTIONS.md` section. Could be done at step 3; must not be later
   than the first release that carries the dependency. **Done** — all three,
   with the five copyright lines and the full BSD-3 text. The guards needed no
   change, as predicted: this adds no first-party file, so
   `guard-spdx-headers.sh` has nothing to check, and BSD-3 imposes no relink
   obligation for `guard-codec2-licensing.sh` to mirror. Worth considering
   later: that guard's *shape* — `cargo tree` asserting a licence-relevant
   invariant — would suit the `default-features = false` on `nnnoiseless`,
   which is the difference between five new crates and sixty-one.
8. **Measure on Pi-class hardware**, then decide `astar-server`'s default.
9. **Decide the client default**, on the strength of milestone 6 and nothing
   else.

## Open questions

* **Does the 48 kHz preference actually rescue 44.1 kHz devices?** Still open,
  and now open for a specific reason: the first measurement (see "The 48 kHz
  guard") found five devices on Rob's Mac and all five already default to
  48 kHz, so the preference had nothing to rescue and the fallback path went
  unexercised. `crates/astar-audio/examples/capture_rates.rs` exists to be run
  on any other machine — a Windows box, a Pi, a laptop's built-in mic — and it
  prints the verdict per device. If a meaningful share turn out to be 44.1-only,
  the rejected double-resample comes back onto the table and that section needs
  rewriting rather than patching.
* **Does RNNoise leave the characterized whine alone?** The argument for keeping
  the hum filter assumes it does. It is testable directly in milestone 6 — run a
  recording of the whining mic through the network with the notches disabled and
  look at the spectrum. If the network kills the tone anyway, the hum filter
  becomes belt-and-braces rather than load-bearing, which changes nothing about
  keeping it but is worth knowing.
* **Does denoising help or hurt through a half-rate vocoder?** Genuinely open,
  and the single biggest risk in this design. Test 3 answers it. If the answer
  is "hurts", this stage should default off for AMBE/Codec 2 networks even if it
  defaults on for ulaw IAX — which the existing M17 per-network override set is
  already shaped to express.
* **Should the returned VAD eventually drive VOX?** Probably, and deliberately
  not now. It needs its own design, its own control, and a migration for the
  saved `voxThresholdDBFS`.
* **What is the cost on a Pi?** Unmeasured. Milestone 8. Do not ship a server
  default that depends on a guess.
* **Is `easyfft`'s thread-local scratch cache stable across cpal stream
  restarts?** The caches are keyed by thread, and cpal's stream-owning thread is
  respawned on device change (`spawn_input_stream`, `stream.rs:508`). Each new
  thread re-pays the first-frame initialisation — which the warm-up covers — but
  the old thread's caches are dropped with it. Expected to be fine; worth a
  glance under a device-hotswap test rather than an assumption.
