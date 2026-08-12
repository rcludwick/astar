// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Dynamic-range processing for voice (iax-32cf).
//!
//! A feedforward [`Compressor`] — the classic "radio mic compressor": an
//! envelope follower drives a gain computer that tames peaks above a threshold
//! and, with makeup gain, lifts the quiet parts so speech sits at a more
//! consistent, punchier level. Operates on f32 mono PCM at the stream rate
//! (8 kHz for IAX2 voice), sample-stateful so it can run inside an audio
//! callback.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

/// Compressor tuning. Defaults are voice-friendly (radio speech processing).
#[derive(Debug, Clone, Copy)]
pub struct CompressorParams {
    /// Level (dBFS) above which gain reduction begins.
    pub threshold_db: f32,
    /// Compression ratio (e.g. `4.0` for 4:1).
    pub ratio: f32,
    /// Envelope attack time (ms): how fast it responds to a rising level.
    pub attack_ms: f32,
    /// Envelope release time (ms): how fast it recovers.
    pub release_ms: f32,
    /// Output makeup gain (dB) applied after compression.
    pub makeup_db: f32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            threshold_db: -24.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 80.0,
            makeup_db: 6.0,
        }
    }
}

impl CompressorParams {
    /// Map a 0.0..=1.0 **strength/level** to voice-compressor tuning (iax-d9bb).
    /// `level` is clamped to `[0, 1]`. Linear in three params (attack/release
    /// stay at the voice defaults, 5 ms / 80 ms):
    ///
    /// | level | threshold  | ratio  | makeup |
    /// |-------|------------|--------|--------|
    /// | 0.0   | −12 dBFS   | 1.5:1  | 0 dB   | barely any compression
    /// | 0.5   | −20 dBFS   | 3.75:1 | 3.5 dB |
    /// | 0.90  | −26.4 dBFS | 5.55:1 | 6.3 dB | default — punchy radio speech
    /// | 1.0   | −28 dBFS   | 6.0:1  | 7 dB   | most aggressive
    ///
    /// Lower threshold + higher ratio + more makeup = more level squashing and a
    /// hotter, more consistent voice. The default level (0.90) is a touch
    /// stronger than the old fixed defaults (−24/4:1/6 dB), intentionally — astar
    /// wants a punchy default with room to back off.
    #[must_use]
    pub fn for_level(level: f32) -> Self {
        let l = level.clamp(0.0, 1.0);
        Self {
            threshold_db: -12.0 - 16.0 * l,
            ratio: 1.5 + 4.5 * l,
            attack_ms: 5.0,
            release_ms: 80.0,
            makeup_db: 7.0 * l,
        }
    }
}

/// A feedforward dynamic-range compressor. Build with [`Compressor::new`] (voice
/// defaults) or [`Compressor::with_params`], then feed samples through
/// [`Compressor::process`] / [`Compressor::process_sample`].
#[derive(Debug, Clone)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    makeup_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
    /// Linear envelope estimate of `|x|`.
    env: f32,
}

/// One-pole smoothing coefficient for a time constant `t_ms` at `fs`.
fn time_coeff(t_ms: f32, sample_rate: u32) -> f32 {
    if t_ms <= 0.0 {
        return 0.0; // instantaneous
    }
    (-1.0 / (t_ms / 1000.0 * sample_rate as f32)).exp()
}

impl Compressor {
    /// A compressor at `sample_rate` with voice defaults.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self::with_params(sample_rate, CompressorParams::default())
    }

    /// A compressor at `sample_rate` with explicit parameters.
    #[must_use]
    pub fn with_params(sample_rate: u32, params: CompressorParams) -> Self {
        Self {
            threshold_db: params.threshold_db,
            ratio: params.ratio,
            makeup_db: params.makeup_db,
            attack_coeff: time_coeff(params.attack_ms, sample_rate),
            release_coeff: time_coeff(params.release_ms, sample_rate),
            env: 0.0,
        }
    }

    /// Process one sample, advancing the envelope. Returns the processed sample.
    pub fn process_sample(&mut self, x: f32) -> f32 {
        // Envelope follower: chase |x|, fast up (attack) / slow down (release).
        let ax = x.abs();
        let coeff = if ax > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.env = coeff * self.env + (1.0 - coeff) * ax;

        // Gain computer (hard knee). dB over threshold → reduction at `ratio`.
        let env_db = if self.env > 1e-6 {
            20.0 * self.env.log10()
        } else {
            -120.0
        };
        let over = env_db - self.threshold_db;
        let reduction_db = if over > 0.0 {
            over * (1.0 / self.ratio - 1.0) // negative: attenuation
        } else {
            0.0
        };
        let gain = 10f32.powf((reduction_db + self.makeup_db) / 20.0);
        (x * gain).clamp(-1.0, 1.0)
    }

    /// Process a buffer in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }

    /// Clear the envelope state (e.g. when toggling the compressor on).
    pub fn reset(&mut self) {
        self.env = 0.0;
    }

    /// Re-tune the compressor in place (e.g. a live level change), keeping the
    /// current envelope state so there's no click. `sample_rate` is needed to
    /// recompute the attack/release coefficients (iax-d9bb).
    pub fn set_params(&mut self, sample_rate: u32, params: CompressorParams) {
        self.threshold_db = params.threshold_db;
        self.ratio = params.ratio;
        self.makeup_db = params.makeup_db;
        self.attack_coeff = time_coeff(params.attack_ms, sample_rate);
        self.release_coeff = time_coeff(params.release_ms, sample_rate);
    }
}

/// Tuning for the [`NoiseGate`].
#[derive(Debug, Clone, Copy)]
pub struct NoiseGateParams {
    /// Level (dBFS) below which the gate closes (after hold).
    pub threshold_db: f32,
    /// Gate-open ramp time (ms): fast, so onsets aren't clipped.
    pub attack_ms: f32,
    /// How long (ms) to stay open after the level drops, protecting word tails.
    pub hold_ms: f32,
    /// Gate-close ramp time (ms): slow, so it closes gracefully.
    pub release_ms: f32,
    /// Attenuation (dB, negative) applied when fully closed. Not a hard mute —
    /// a finite floor sounds more natural.
    pub range_db: f32,
}

impl Default for NoiseGateParams {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            attack_ms: 5.0,
            hold_ms: 150.0,
            release_ms: 200.0,
            range_db: -35.0,
        }
    }
}

/// A downward expander / noise gate: passes signal above the threshold and
/// attenuates it (toward `range_db`) when the level drops below, after a hold.
#[derive(Debug, Clone)]
pub struct NoiseGate {
    threshold_db: f32,
    range_lin: f32,
    det_coeff: f32,
    open_coeff: f32,
    close_coeff: f32,
    hold_samples: u32,
    /// Linear level estimate.
    env: f32,
    /// Current applied gain (linear), smoothed toward the target.
    gain: f32,
    /// Samples remaining on the open-hold timer.
    hold: u32,
}

impl NoiseGate {
    /// A gate at `sample_rate` with voice defaults.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self::with_params(sample_rate, NoiseGateParams::default())
    }

    /// A gate at `sample_rate` with explicit parameters.
    #[must_use]
    pub fn with_params(sample_rate: u32, params: NoiseGateParams) -> Self {
        Self {
            threshold_db: params.threshold_db,
            range_lin: 10f32.powf(params.range_db / 20.0),
            det_coeff: time_coeff(2.0, sample_rate), // fast level detector
            open_coeff: time_coeff(params.attack_ms, sample_rate),
            close_coeff: time_coeff(params.release_ms, sample_rate),
            hold_samples: (params.hold_ms / 1000.0 * sample_rate as f32) as u32,
            env: 0.0,
            gain: 1.0,
            hold: 0,
        }
    }

    /// Process one sample.
    pub fn process_sample(&mut self, x: f32) -> f32 {
        // Fast level detector.
        let ax = x.abs();
        self.env = self.det_coeff * self.env + (1.0 - self.det_coeff) * ax;
        let env_db = if self.env > 1e-6 {
            20.0 * self.env.log10()
        } else {
            -120.0
        };

        // Target gain: open above threshold; stay open through the hold; then
        // close to the range floor.
        let target = if env_db > self.threshold_db {
            self.hold = self.hold_samples;
            1.0
        } else if self.hold > 0 {
            self.hold -= 1;
            1.0
        } else {
            self.range_lin
        };

        // Ramp the gain toward the target: fast to open, slow to close.
        let coeff = if target > self.gain {
            self.open_coeff
        } else {
            self.close_coeff
        };
        self.gain = coeff * self.gain + (1.0 - coeff) * target;
        x * self.gain
    }

    /// Process a buffer in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }

    /// Reset gain/hold/level state (gate fully open).
    pub fn reset(&mut self) {
        self.env = 0.0;
        self.gain = 1.0;
        self.hold = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_level_clamps_and_scales_monotonically() {
        // Out-of-range levels clamp to the endpoints.
        let lo = CompressorParams::for_level(-5.0);
        let hi = CompressorParams::for_level(2.0);
        // Clamping is deterministic: out-of-range inputs reproduce the endpoint
        // exactly, so an epsilon of 0 captures the intended bit-identical result.
        assert!(
            (lo.threshold_db - CompressorParams::for_level(0.0).threshold_db).abs() <= f32::EPSILON
        );
        assert!((hi.ratio - CompressorParams::for_level(1.0).ratio).abs() <= f32::EPSILON);
        // Endpoints + the documented default anchor (0.90).
        let l0 = CompressorParams::for_level(0.0);
        let l1 = CompressorParams::for_level(1.0);
        assert_eq!((l0.threshold_db, l0.ratio, l0.makeup_db), (-12.0, 1.5, 0.0));
        assert_eq!((l1.threshold_db, l1.ratio, l1.makeup_db), (-28.0, 6.0, 7.0));
        let d = CompressorParams::for_level(0.90);
        assert!((d.threshold_db - -26.4).abs() < 1e-3);
        assert!((d.ratio - 5.55).abs() < 1e-3);
        assert!((d.makeup_db - 6.3).abs() < 1e-3);
        // Higher level ⇒ lower threshold, higher ratio, more makeup (stronger).
        let mid = CompressorParams::for_level(0.5);
        assert!(l1.threshold_db < mid.threshold_db && mid.threshold_db < l0.threshold_db);
        assert!(l0.ratio < mid.ratio && mid.ratio < l1.ratio);
        assert!(l0.makeup_db < mid.makeup_db && mid.makeup_db < l1.makeup_db);
    }

    #[test]
    fn set_params_retunes_without_resetting_envelope() {
        let mut c = Compressor::new(8_000);
        // Build up some envelope state.
        let _ = settle(&mut c, 0.5, 200);
        let env_before = c.env;
        c.set_params(8_000, CompressorParams::for_level(0.2));
        // Envelope preserved (no click); set_params must not touch it, so the
        // value is bit-identical (epsilon 0).
        assert!((c.env - env_before).abs() <= f32::EPSILON);
        assert!((c.ratio - CompressorParams::for_level(0.2).ratio).abs() < 1e-6);
    }

    /// Feed a constant `amp` for `n` samples and return the final output.
    fn settle(c: &mut Compressor, amp: f32, n: usize) -> f32 {
        let mut last = 0.0;
        for _ in 0..n {
            last = c.process_sample(amp);
        }
        last
    }

    #[test]
    fn quiet_signal_gets_only_makeup_gain() {
        // -40 dBFS (0.01) is below the -24 dB threshold → no reduction, just
        // the +6 dB makeup (×1.995).
        let mut c = Compressor::new(8000);
        let out = settle(&mut c, 0.01, 2000);
        let expected = 0.01 * 10f32.powf(6.0 / 20.0);
        assert!(
            (out - expected).abs() < 1e-3,
            "quiet out {out}, want {expected}"
        );
    }

    #[test]
    fn loud_signal_is_compressed_toward_the_threshold() {
        // -6 dBFS (0.5) is 18 dB over threshold; at 4:1 that's -13.5 dB
        // reduction, +6 dB makeup → -7.5 dB net (×0.4217).
        let mut c = Compressor::new(8000);
        let out = settle(&mut c, 0.5, 6000);
        let expected = 0.5 * 10f32.powf(-7.5 / 20.0);
        assert!(
            (out - expected).abs() < 5e-3,
            "loud out {out}, want {expected}"
        );
        assert!(
            out < 0.5,
            "a loud signal nets to attenuation after compression"
        );
    }

    #[test]
    fn attack_engages_gradually() {
        // From silence, the first loud sample sees a low envelope (no reduction
        // yet) → louder than the settled, fully-compressed output.
        let mut c = Compressor::new(8000);
        let first = c.process_sample(0.5);
        let settled = settle(&mut c, 0.5, 6000);
        assert!(
            first > settled,
            "attack ramps in: first {first} > settled {settled}"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)] // identical inputs through identical state → exact
    fn process_buffer_matches_sample_loop() {
        let input: Vec<f32> = (0..256).map(|i| 0.4 * ((i as f32) * 0.1).sin()).collect();

        let mut a = Compressor::new(8000);
        let mut buf = input.clone();
        a.process(&mut buf);

        let mut b = Compressor::new(8000);
        let looped: Vec<f32> = input.iter().map(|&x| b.process_sample(x)).collect();

        assert_eq!(buf, looped);
    }

    #[test]
    fn reset_clears_the_envelope() {
        let mut c = Compressor::new(8000);
        settle(&mut c, 0.8, 4000); // build a large envelope
        c.reset();
        // After reset, the first loud sample behaves like a fresh start: almost
        // no reduction (envelope near zero), so output ≈ input × makeup.
        let fresh = Compressor::new(8000);
        let mut fresh2 = fresh;
        let after_reset = c.process_sample(0.5);
        let from_fresh = fresh2.process_sample(0.5);
        assert!(
            (after_reset - from_fresh).abs() < 1e-6,
            "reset == fresh start"
        );
    }

    // ---------- noise gate ----------

    /// Peak of the gate's output over the last quarter of `n` constant samples.
    fn gate_tail_peak(g: &mut NoiseGate, amp: f32, n: usize) -> f32 {
        let mut peak = 0.0_f32;
        for i in 0..n {
            let y = g.process_sample(amp);
            if i > 3 * n / 4 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn gate_passes_a_loud_signal() {
        let mut g = NoiseGate::new(8000);
        let peak = gate_tail_peak(&mut g, 0.5, 4000);
        assert!(peak > 0.45, "loud signal should pass open: {peak}");
    }

    #[test]
    fn gate_attenuates_sustained_quiet() {
        // −54 dBFS, well below the −45 dB threshold; after hold + release the
        // gate closes to the −35 dB range.
        let mut g = NoiseGate::new(8000);
        let peak = gate_tail_peak(&mut g, 0.002, 8000);
        assert!(
            peak < 0.002 * 0.1,
            "sustained quiet should be gated: {peak}"
        );
    }

    #[test]
    fn gate_holds_open_briefly_after_loud() {
        // Open on a loud burst, then a short quiet patch (< hold): still open.
        let mut g = NoiseGate::new(8000);
        for _ in 0..2000 {
            g.process_sample(0.5);
        }
        let mut peak = 0.0_f32;
        for _ in 0..400 {
            // 50 ms < 150 ms hold
            peak = peak.max(g.process_sample(0.002).abs());
        }
        assert!(
            peak > 0.0015,
            "hold should keep quiet passing briefly: {peak}"
        );
    }
}
