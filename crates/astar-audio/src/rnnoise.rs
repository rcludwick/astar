// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Neural mic noise suppression at device rate
//! (`docs/design/noise-suppression.md`).
//!
//! [`RnnoiseStage`] wraps `nnnoiseless` — a pure-Rust port of Xiph's RNNoise
//! — in the shape the capture callback needs. The network itself is not the
//! interesting part here; the three things around it are.
//!
//! **Framing.** RNNoise consumes exactly 480 samples, which is 10 ms at
//! 48 kHz and nothing at any other rate. cpal delivers whatever the host
//! feels like — 64, 128, 512, 1024 — and can change it mid-stream. So this
//! is an accumulator, not a filter: it holds a remainder of at most 479
//! samples between callbacks, which is why the hook it serves takes
//! `&mut Vec<f32>` rather than `&[f32]`.
//!
//! **Scaling.** astar's pipeline is `[-1, 1]` everywhere. `process_frame`
//! wants f32 in *i16* range. Getting that wrong in one direction is a silent
//! 90 dB error, so both conversions live here and are pinned by a test.
//!
//! **Warm-up, and what it does not do.** Two separate first-call problems
//! get conflated easily, and only one of them a silence frame solves.
//!
//! `easyfft`'s FFT planner and scratch caches are thread-local and built on
//! first use, so the first `process_frame` on a given thread allocates. One
//! frame of silence pays that on the first callback after the stream opens
//! — once per stream, before the operator has keyed anything.
//!
//! It does **not** absorb the fade-in artefact the crate tells you to throw
//! away. Measured: with the silence frame and without it, the first real
//! output frame is identical and sits 25 dB down. The artefact is the
//! overlap-add of the first frame against an all-zero history, and a frame
//! of silence *is* an all-zero history — so warming with silence reproduces
//! the starting condition rather than consuming it. The first real output
//! frame is therefore dropped outright, which is what upstream's own
//! example does.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    // Sample counts and rates crossing into f32/f64 for signal generation and
    // timing arithmetic — the same class `resample.rs` allows module-wide.
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nnnoiseless::DenoiseState;

/// Samples per RNNoise frame — 10 ms at [`RNNOISE_RATE`].
pub const RNNOISE_FRAME: usize = DenoiseState::FRAME_SIZE;

/// The only sample rate RNNoise is defined at. Not resamplable: the frame
/// size is a duration only at this rate, the band table is in Hz, and the
/// pitch search periods are in samples.
pub const RNNOISE_RATE: u32 = 48_000;

/// `[-1, 1]` ↔ i16-range conversion factor.
const I16_SCALE: f32 = 32767.0;

/// Steady-state headroom for the input and output buffers, in samples:
/// 100 ms at 48 kHz. A host buffer larger than this amortises one realloc
/// rather than meeting a hard cap that would have to drop audio.
const RESERVE: usize = (RNNOISE_RATE as usize) / 10;

/// Which mic noise-reduction chain to run, from `ASTAR_MIC_DENOISE`.
///
/// A developer A/B override with no UI, following the `IAX_THUMBDV_PORT`
/// precedent: an environment variable that narrows behaviour for people who
/// know why they are setting it. It exists so the design's quality
/// evaluation can be run as a real A/B, and so a user with a mic that
/// sounds wrong can be asked to try exactly one thing.
///
/// It does not override the operator's "Noise reduction" checkbox — with
/// the checkbox off, nothing runs whatever this says. It chooses which
/// chain the checkbox turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DenoiseMode {
    /// Unset: neural where the device allows it, filter + gate otherwise.
    #[default]
    Auto,
    /// `neural` — same as `Auto` today; explicit so an A/B run records
    /// intent rather than relying on a default that may change.
    Neural,
    /// `legacy` — the pre-existing `HumFilter` + `NoiseGate` chain, even at
    /// 48 kHz. The other arm of the A/B.
    Legacy,
    /// `off` — no mic noise reduction at all, checkbox notwithstanding. The
    /// control arm: what the raw mic actually sounds like.
    Off,
}

impl DenoiseMode {
    /// Parse the variable's value. Anything unrecognised is [`Self::Auto`] —
    /// a typo in a debugging aid must not change what the operator hears.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("neural") => Self::Neural,
            Some("legacy") => Self::Legacy,
            Some("off") => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Read `ASTAR_MIC_DENOISE` once per process.
    #[must_use]
    pub fn from_env() -> Self {
        static MODE: std::sync::OnceLock<DenoiseMode> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            let mode = Self::parse(std::env::var("ASTAR_MIC_DENOISE").ok().as_deref());
            if mode != Self::Auto {
                tracing::info!(
                    target: "astar_audio",
                    ?mode,
                    "ASTAR_MIC_DENOISE override in effect"
                );
            }
            mode
        })
    }

    /// Whether the neural stage may run under this mode.
    #[must_use]
    pub fn allows_neural(self) -> bool {
        matches!(self, Self::Auto | Self::Neural)
    }

    /// Whether the classical `HumFilter` + `NoiseGate` chain may run.
    #[must_use]
    pub fn allows_legacy(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Which mic noise-reduction chain is actually live, and at what rate.
///
/// Read-only, reported rather than inferred. The 48 kHz guard is otherwise
/// invisible: a device that could not offer 48 kHz silently gets the
/// classical chain, and an operator whose mic sounds different from everyone
/// else's has no way to find out why. astar installs no `tracing`
/// subscriber, so a log line is not a substitute — this is the only place
/// the answer surfaces.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DenoiseStatus {
    /// The rate the capture stream opened at, or `0` when nothing is
    /// capturing (or when the sink is driven without a capture callback).
    pub device_rate: u32,
    /// The chain running on that stream.
    pub chain: DenoiseChain,
}

impl DenoiseStatus {
    /// Pack into one `u64` so the lane can publish it with a single relaxed
    /// store and a reader can never see a torn rate/chain pair.
    #[must_use]
    pub fn to_bits(self) -> u64 {
        (u64::from(self.chain as u32) << 32) | u64::from(self.device_rate)
    }

    /// Unpack [`Self::to_bits`].
    #[must_use]
    pub fn from_bits(bits: u64) -> Self {
        Self {
            device_rate: (bits & 0xFFFF_FFFF) as u32,
            #[allow(clippy::cast_possible_truncation)]
            chain: DenoiseChain::from_u32((bits >> 32) as u32),
        }
    }

    /// A short line for a UI: `"neural (48 kHz)"`,
    /// `"filter + gate (device 44.1 kHz)"`, `"off"`.
    ///
    /// The rate is named only when it explains something. On the neural line
    /// it confirms the guard passed; on the fallback line it is the reason.
    #[must_use]
    pub fn summary(&self) -> String {
        let khz = |r: u32| {
            if r % 1000 == 0 {
                format!("{} kHz", r / 1000)
            } else {
                format!("{:.1} kHz", f64::from(r) / 1000.0)
            }
        };
        match self.chain {
            DenoiseChain::NotCapturing => "not capturing".to_string(),
            DenoiseChain::Off => "off".to_string(),
            DenoiseChain::Neural => format!("neural ({})", khz(self.device_rate)),
            DenoiseChain::FilterGate if self.device_rate == RNNOISE_RATE => {
                "filter + gate".to_string()
            }
            DenoiseChain::FilterGate => {
                format!("filter + gate (device {})", khz(self.device_rate))
            }
        }
    }
}

/// The mic noise-reduction chain in effect.
///
/// The discriminants are explicit and load-bearing: they cross the C ABI as
/// integers and are mirrored by `IaxDenoiseChain` and the Swift enum, so
/// they are part of the published interface. Add variants at the end.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum DenoiseChain {
    /// No capture stream is open, so no chain is running.
    #[default]
    NotCapturing = 0,
    /// Capturing, but noise reduction is switched off.
    Off = 1,
    /// RNNoise at device rate, with the hum filter after it and no gate.
    Neural = 2,
    /// The classical `HumFilter` + `NoiseGate`. Either the device could not
    /// give us 48 kHz, or `ASTAR_MIC_DENOISE=legacy` asked for it.
    FilterGate = 3,
}

impl DenoiseChain {
    /// Recover a chain from its discriminant; anything unknown reads as
    /// [`Self::NotCapturing`], which is the honest answer to "a value this
    /// build does not understand".
    #[must_use]
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Off,
            2 => Self::Neural,
            3 => Self::FilterGate,
            _ => Self::NotCapturing,
        }
    }
}

/// RNNoise in a push/replace shell, sized for the cpal capture callback.
pub struct RnnoiseStage {
    state: Box<DenoiseState<'static>>,
    /// Device-rate samples not yet forming a whole frame. Never ≥ 480 once
    /// [`RnnoiseStage::process`] returns.
    pending: Vec<f32>,
    /// Denoised samples for this callback. Swapped out rather than copied.
    out: Vec<f32>,
    frame_in: [f32; RNNOISE_FRAME],
    frame_out: [f32; RNNOISE_FRAME],
    /// Most recent voice-activity probability, `f32` bits. Published as
    /// telemetry, deliberately NOT used to drive anything — see the design's
    /// "surface the VAD, do not substitute it".
    vad: Arc<AtomicU32>,
    /// Cleared by the first [`RnnoiseStage::process`], on the audio thread.
    needs_warmup: bool,
    /// The first real output frame is the fade-in artefact; drop it. Costs
    /// 10 ms of audio, once, at stream open.
    drop_next_output: bool,
}

impl RnnoiseStage {
    /// Allocate the network and both buffers. Call from the control thread:
    /// `DenoiseState::new()` allocates, and so does the reserve.
    #[must_use]
    pub fn new(vad: Arc<AtomicU32>) -> Self {
        let mut pending = Vec::new();
        let mut out = Vec::new();
        // One host buffer plus a frame of slack, so neither grows in steady
        // state however the host sizes its callbacks.
        pending.reserve(RESERVE + RNNOISE_FRAME);
        out.reserve(RESERVE + RNNOISE_FRAME);
        Self {
            state: DenoiseState::new(),
            pending,
            out,
            frame_in: [0.0; RNNOISE_FRAME],
            frame_out: [0.0; RNNOISE_FRAME],
            vad,
            needs_warmup: true,
            drop_next_output: true,
        }
    }

    /// The latest voice-activity probability in `0.0..=1.0`.
    #[must_use]
    pub fn voice_probability(&self) -> f32 {
        f32::from_bits(self.vad.load(Ordering::Relaxed))
    }

    /// Denoise `samples` in place. Not length-preserving: output lags input
    /// by whatever does not fill a frame, so a callback can come back short
    /// (or, after a short one, long).
    ///
    /// `samples` must be mono at [`RNNOISE_RATE`]; the caller enforces that
    /// via `CaptureCapability`.
    pub fn process(&mut self, samples: &mut Vec<f32>) {
        // First call on this thread: build the FFT planner and the
        // thread-local scratch caches, and burn the documented fade-in
        // frame, before any of the operator's audio is at stake.
        if self.needs_warmup {
            self.needs_warmup = false;
            self.frame_in = [0.0; RNNOISE_FRAME];
            self.state
                .process_frame(&mut self.frame_out, &self.frame_in);
        }

        self.pending.extend_from_slice(samples);
        self.out.clear();

        let frames = self.pending.len() / RNNOISE_FRAME;
        for f in 0..frames {
            let src = &self.pending[f * RNNOISE_FRAME..(f + 1) * RNNOISE_FRAME];
            for (dst, &s) in self.frame_in.iter_mut().zip(src) {
                *dst = s * I16_SCALE;
            }
            let p = self
                .state
                .process_frame(&mut self.frame_out, &self.frame_in);
            self.vad.store(p.to_bits(), Ordering::Relaxed);
            if self.drop_next_output {
                // The fade-in frame. Everything else about it — the VAD
                // store above, the network's advanced state — is kept; only
                // the samples are withheld.
                self.drop_next_output = false;
                continue;
            }
            self.out
                .extend(self.frame_out.iter().map(|&s| s / I16_SCALE));
        }
        self.pending.drain(..frames * RNNOISE_FRAME);

        // Copy back rather than swap. A swap avoids this memcpy, but it
        // hands `out` to the caller and adopts the caller's buffer — which
        // is sized for the host's callback, not for this stage's output.
        // Those differ: with 512-sample callbacks the remainder grows 32
        // samples each time, so roughly every fifteenth callback completes
        // two frames and emits 960 samples into a buffer sized 512. That
        // reallocates, on the audio thread, forever. Keeping `out` means the
        // reserve in `new` stays where it was put; the copy is at most
        // ~4 KB against a 36 µs-per-frame network.
        samples.clear();
        samples.extend_from_slice(&self.out);
    }

    /// Samples held back, waiting to complete a frame. Always < 480.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

impl std::fmt::Debug for RnnoiseStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RnnoiseStage")
            .field("pending", &self.pending.len())
            .field("needs_warmup", &self.needs_warmup)
            .field("vad", &self.voice_probability())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn stage() -> RnnoiseStage {
        RnnoiseStage::new(Arc::new(AtomicU32::new(0)))
    }

    /// 1 kHz at 48 kHz, `[-1, 1]`.
    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (TAU * 1_000.0 * i as f32 / RNNOISE_RATE as f32).sin() * amp)
            .collect()
    }

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0.0_f32, |a, b| a.max(b.abs()))
    }

    /// The frame is 10 ms at 48 kHz, and both of those are load-bearing
    /// constants rather than arbitrary ones.
    #[test]
    fn frame_is_ten_milliseconds_at_forty_eight_k() {
        assert_eq!(RNNOISE_FRAME, 480);
        assert_eq!(RNNOISE_FRAME * 100, RNNOISE_RATE as usize);
    }

    /// The scaling is right in BOTH directions.
    ///
    /// This is the test the design asks for by name: the pipeline is
    /// `[-1, 1]` and `process_frame` wants i16-range floats, so a stage that
    /// scaled in but not out — or neither — would be a silent ~90 dB error.
    /// A loud tone must come back at roughly the amplitude it went in at,
    /// which is true only when both conversions are present and inverse.
    #[test]
    fn output_stays_in_unit_range_so_both_conversions_are_present() {
        let mut st = stage();
        // Ten frames of a loud, clean tone: RNNoise keeps voiced-looking
        // periodic content, so the level should survive.
        let mut buf = tone(RNNOISE_FRAME * 10, 0.5);
        st.process(&mut buf);

        assert!(!buf.is_empty(), "produced nothing");
        assert!(buf.iter().all(|v| v.is_finite()), "non-finite sample");
        let p = peak(&buf);
        assert!(
            p > 0.05 && p <= 1.0,
            "peak {p} is outside unit range — one of the two i16 conversions is missing \
             (no scaling out gives ~32767, no scaling in gives ~1e-5)"
        );
    }

    /// Pushing in odd, host-shaped chunks yields exactly the same samples as
    /// one big push. This is the accumulator's whole contract: cpal changes
    /// its buffer size whenever it likes.
    #[test]
    fn chunked_pushes_equal_one_big_push() {
        let input = tone(RNNOISE_FRAME * 8, 0.4);

        let mut a = stage();
        let mut one = input.clone();
        a.process(&mut one);

        let mut b = stage();
        let mut many = Vec::new();
        // 64, 128, 512, 1024 are all real cpal buffer sizes; 97 is not, and
        // is there because a prime stride crosses frame boundaries in every
        // possible phase.
        let mut i = 0;
        for &n in [64_usize, 128, 97, 512, 1024, 97].iter().cycle() {
            if i >= input.len() {
                break;
            }
            let end = (i + n).min(input.len());
            let mut chunk = input[i..end].to_vec();
            b.process(&mut chunk);
            many.extend_from_slice(&chunk);
            i = end;
        }

        assert_eq!(
            one.len(),
            many.len(),
            "same total output regardless of push sizing"
        );
        for (k, (x, y)) in one.iter().zip(&many).enumerate() {
            assert!(
                (x - y).abs() < 1e-6,
                "sample {k} differs between chunked and single push: {x} vs {y}"
            );
        }
    }

    /// The remainder is bounded by one frame, always — that bound is what
    /// makes the added TX latency "under 10 ms" rather than unbounded.
    #[test]
    fn remainder_never_reaches_a_whole_frame() {
        let mut st = stage();
        for n in [1_usize, 479, 480, 481, 959, 960, 1, 1023] {
            let mut buf = tone(n, 0.3);
            st.process(&mut buf);
            assert!(
                st.pending_len() < RNNOISE_FRAME,
                "held {} samples after a {n}-sample push; must stay under one frame",
                st.pending_len()
            );
        }
    }

    /// Nothing allocates once the stage is warm.
    ///
    /// Modelled on the real caller: `process_input` owns one `mono` buffer
    /// and reuses it every callback. That matters because `process` swaps
    /// buffers with the caller, so the two ping-pong — a test that handed
    /// over a freshly allocated `Vec` each time would be measuring the
    /// test's own allocation, not the stage's.
    #[test]
    fn steady_state_does_not_allocate() {
        let mut st = stage();
        let mut mono: Vec<f32> = Vec::new();
        let src = tone(512, 0.3);

        // Long enough to include a two-frame callback: with 512-sample
        // pushes the remainder gains 32 each time, so every ~15th callback
        // emits 960 samples instead of 480. A shorter settle would never
        // see the case that actually reallocates.
        for _ in 0..64 {
            mono.clear();
            mono.extend_from_slice(&src);
            st.process(&mut mono);
        }
        let (pending_cap, out_cap, mono_cap) =
            (st.pending.capacity(), st.out.capacity(), mono.capacity());

        for _ in 0..500 {
            mono.clear();
            mono.extend_from_slice(&src);
            st.process(&mut mono);
        }
        assert_eq!(
            st.pending.capacity(),
            pending_cap,
            "pending grew on the audio thread"
        );
        assert_eq!(st.out.capacity(), out_cap, "out grew on the audio thread");
        assert_eq!(
            mono.capacity(),
            mono_cap,
            "the caller's buffer grew on the audio thread"
        );
    }

    /// The fade-in artefact never reaches the caller.
    ///
    /// Measured behaviour, not assumed: a cold `DenoiseState` fed a steady
    /// 0.5-amplitude tone emits 0.028 for its first output frame and 0.51
    /// from the second onward — 25 dB down, because that frame is the
    /// overlap-add against an all-zero history. Warming with a frame of
    /// silence does NOT change this (silence *is* the all-zero history), so
    /// the first real output frame is dropped instead.
    ///
    /// The assertion is on the first frame the CALLER sees. It is allowed to
    /// be a little below the second — the network still settles — but not
    /// the 25 dB the artefact would show.
    #[test]
    fn the_fade_in_artefact_never_reaches_the_caller() {
        let mut st = stage();
        // Two frames in; one comes back, because the first was dropped.
        let mut first = tone(RNNOISE_FRAME * 2, 0.5);
        st.process(&mut first);
        assert_eq!(
            first.len(),
            RNNOISE_FRAME,
            "exactly one frame should be withheld at stream open"
        );

        let mut second = tone(RNNOISE_FRAME, 0.5);
        st.process(&mut second);

        let (p1, p2) = (peak(&first), peak(&second));
        let ratio_db = 20.0 * (p1 / p2).log10();
        assert!(
            ratio_db > -6.0,
            "first delivered frame is {ratio_db:.1} dB below the next — the fade-in \
             frame looks like it reached the caller (peaks {p1:.4} vs {p2:.4})"
        );
    }

    /// The drop costs exactly one frame, once — not one per callback.
    #[test]
    fn only_the_very_first_frame_is_dropped() {
        let mut st = stage();
        let mut a = tone(RNNOISE_FRAME * 3, 0.5);
        st.process(&mut a);
        assert_eq!(a.len(), RNNOISE_FRAME * 2, "three frames in, two out, once");

        let mut b = tone(RNNOISE_FRAME * 3, 0.5);
        st.process(&mut b);
        assert_eq!(
            b.len(),
            RNNOISE_FRAME * 3,
            "and nothing is withheld thereafter"
        );
    }

    /// The VAD probability is published, in range, and is telemetry only.
    #[test]
    fn voice_probability_is_published_in_range() {
        let mut st = stage();
        let mut buf = tone(RNNOISE_FRAME * 4, 0.5);
        st.process(&mut buf);
        let p = st.voice_probability();
        assert!(
            (0.0..=1.0).contains(&p),
            "voice probability {p} out of range"
        );
    }

    /// A push too short to complete a frame produces nothing and loses
    /// nothing — the samples come back on a later call.
    #[test]
    fn a_short_push_returns_empty_and_keeps_the_samples() {
        let mut st = stage();
        let mut buf = tone(100, 0.5);
        st.process(&mut buf);
        assert!(buf.is_empty(), "a sub-frame push cannot produce output yet");
        assert_eq!(st.pending_len(), 100, "and must not drop the samples");

        // Completing the frame processes it — and it is the fade-in frame,
        // so it is withheld. The frame after that is delivered whole.
        let mut rest = tone(RNNOISE_FRAME - 100, 0.5);
        st.process(&mut rest);
        assert!(
            rest.is_empty(),
            "the first completed frame is the dropped one"
        );
        let mut next = tone(RNNOISE_FRAME, 0.5);
        st.process(&mut next);
        assert_eq!(
            next.len(),
            RNNOISE_FRAME,
            "the next frame is delivered whole"
        );
    }

    #[test]
    fn denoise_mode_parses_its_three_values_and_ignores_the_rest() {
        assert_eq!(DenoiseMode::parse(Some("neural")), DenoiseMode::Neural);
        assert_eq!(DenoiseMode::parse(Some("legacy")), DenoiseMode::Legacy);
        assert_eq!(DenoiseMode::parse(Some("off")), DenoiseMode::Off);
        // Case and whitespace are forgiven; a typo is not obeyed.
        assert_eq!(DenoiseMode::parse(Some("  LEGACY ")), DenoiseMode::Legacy);
        assert_eq!(DenoiseMode::parse(Some("nueral")), DenoiseMode::Auto);
        assert_eq!(DenoiseMode::parse(Some("")), DenoiseMode::Auto);
        assert_eq!(DenoiseMode::parse(None), DenoiseMode::Auto);
    }

    #[test]
    fn denoise_mode_gates_the_two_chains() {
        assert!(DenoiseMode::Auto.allows_neural() && DenoiseMode::Auto.allows_legacy());
        assert!(DenoiseMode::Neural.allows_neural());
        assert!(
            !DenoiseMode::Legacy.allows_neural(),
            "legacy must never run the network"
        );
        assert!(DenoiseMode::Legacy.allows_legacy());
        assert!(!DenoiseMode::Off.allows_neural(), "off means off");
        assert!(!DenoiseMode::Off.allows_legacy(), "off means off");
    }

    #[test]
    fn denoise_status_summary_names_the_rate_only_where_it_explains_something() {
        let st = |chain, device_rate| DenoiseStatus { device_rate, chain }.summary();
        assert_eq!(st(DenoiseChain::Neural, 48_000), "neural (48 kHz)");
        // The fallback line at a non-48 rate: the rate IS the reason.
        assert_eq!(
            st(DenoiseChain::FilterGate, 44_100),
            "filter + gate (device 44.1 kHz)"
        );
        // The fallback at 48 kHz means the mode asked for it, not the guard —
        // naming the rate there would suggest a fault that isn't there.
        assert_eq!(st(DenoiseChain::FilterGate, 48_000), "filter + gate");
        assert_eq!(st(DenoiseChain::Off, 48_000), "off");
        assert_eq!(st(DenoiseChain::NotCapturing, 0), "not capturing");
    }

    #[test]
    fn denoise_status_survives_the_round_trip_through_one_u64() {
        for chain in [
            DenoiseChain::NotCapturing,
            DenoiseChain::Off,
            DenoiseChain::Neural,
            DenoiseChain::FilterGate,
        ] {
            for device_rate in [0_u32, 8_000, 44_100, 48_000, 768_000] {
                let st = DenoiseStatus { device_rate, chain };
                assert_eq!(DenoiseStatus::from_bits(st.to_bits()), st);
            }
        }
    }

    /// The discriminants cross the C ABI, so they are pinned here rather
    /// than left to declaration order.
    #[test]
    fn denoise_chain_discriminants_are_stable() {
        assert_eq!(DenoiseChain::NotCapturing as u32, 0);
        assert_eq!(DenoiseChain::Off as u32, 1);
        assert_eq!(DenoiseChain::Neural as u32, 2);
        assert_eq!(DenoiseChain::FilterGate as u32, 3);
        // A value from a newer build reads as "unknown", not as a wrong chain.
        assert_eq!(DenoiseChain::from_u32(99), DenoiseChain::NotCapturing);
    }
}
