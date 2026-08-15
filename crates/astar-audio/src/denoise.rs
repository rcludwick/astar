// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Mic noise reduction (iax-a9d7): a [`NoiseReducer`] that chains a
//! [`HumFilter`] (strip mains hum / narrowband whine) into a [`NoiseGate`]
//! (attenuate the broadband floor in the gaps). The hum filter runs first so
//! its tones don't fool the gate's level detector.
//!
//! [`NoiseReducer::new`] uses a fixed 60 Hz hum comb + default gate;
//! [`NoiseReducer::from_profile`] builds a per-mic version from a measured
//! [`MicProfile`] (its notch list + gate threshold).

use crate::characterize::MicProfile;
use crate::dynamics::{NoiseGate, NoiseGateParams};
use crate::filter::HumFilter;

/// Hum filter followed by a noise gate, applied to mic capture.
#[derive(Debug, Clone)]
pub struct NoiseReducer {
    hum: HumFilter,
    gate: NoiseGate,
}

impl NoiseReducer {
    /// A reducer at `sample_rate` with 60 Hz hum defaults and a voice gate.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self::from_parts(HumFilter::new(sample_rate), NoiseGate::new(sample_rate))
    }

    /// Compose from an already-configured hum filter and gate (e.g. a
    /// characterized notch set).
    #[must_use]
    pub fn from_parts(hum: HumFilter, gate: NoiseGate) -> Self {
        Self { hum, gate }
    }

    /// Build a per-mic reducer from a measured [`MicProfile`]: its narrowband
    /// notches + high-pass, and a gate at the profile's derived threshold.
    #[must_use]
    pub fn from_profile(sample_rate: u32, profile: &MicProfile) -> Self {
        let notches: Vec<(f32, f32)> = profile.notches.iter().map(|n| (n.freq_hz, n.q)).collect();
        let hum = HumFilter::from_notches(sample_rate, profile.highpass_hz, &notches);
        let gate = NoiseGate::with_params(
            sample_rate,
            NoiseGateParams {
                threshold_db: profile.gate_threshold_db,
                ..NoiseGateParams::default()
            },
        );
        Self::from_parts(hum, gate)
    }

    /// Process one sample: hum filter, then gate.
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let filtered = self.hum.process_sample(x);
        self.gate.process_sample(filtered)
    }

    /// Process a buffer in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            *s = self.process_sample(*s);
        }
    }

    /// Reset all stage state.
    pub fn reset(&mut self) {
        self.hum.reset();
        self.gate.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[allow(clippy::cast_precision_loss)]
    fn tone(freq: f32, fs: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (TAU * freq * i as f32 / fs as f32).sin())
            .collect()
    }

    fn tail_peak(out: &[f32]) -> f32 {
        out[out.len() / 2..]
            .iter()
            .fold(0.0_f32, |p, &s| p.max(s.abs()))
    }

    #[test]
    fn removes_mains_hum_but_passes_voice() {
        let mut nr = NoiseReducer::new(8000);
        let mut hum = tone(60.0, 8000, 4000);
        nr.process(&mut hum);
        assert!(
            tail_peak(&hum) < 0.2,
            "60 Hz hum should be filtered: {}",
            tail_peak(&hum)
        );

        let mut nr2 = NoiseReducer::new(8000);
        let mut voice = tone(1000.0, 8000, 4000);
        nr2.process(&mut voice);
        assert!(
            tail_peak(&voice) > 0.8,
            "1 kHz voice should pass: {}",
            tail_peak(&voice)
        );
    }

    #[test]
    fn gates_sustained_quiet() {
        let mut nr = NoiseReducer::new(8000);
        // −54 dBFS broadband-ish quiet (a low 1 kHz tone), held past the gate's
        // hold + release.
        let mut quiet: Vec<f32> = tone(1000.0, 8000, 8000).iter().map(|s| s * 0.002).collect();
        nr.process(&mut quiet);
        assert!(
            tail_peak(&quiet) < 0.002 * 0.2,
            "quiet should be gated down"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn json_round_tripped_profile_rebuilds_an_equivalent_noise_reducer() {
        // iax-2095: a recalled (JSON-persisted) profile must rebuild the SAME
        // NoiseReducer. NoiseReducer has no PartialEq, so prove equivalence
        // behaviorally: identical output for identical input.
        use crate::characterize::{MicProfile, NotchSpec};
        let profile = MicProfile {
            highpass_hz: 90.0,
            notches: vec![
                NotchSpec {
                    freq_hz: 588.0,
                    q: 20.0,
                },
                NotchSpec {
                    freq_hz: 1176.0,
                    q: 20.0,
                },
            ],
            noise_floor_dbfs: -52.0,
            gate_threshold_db: -46.0,
        };
        let json = serde_json::to_string(&profile).unwrap();
        let recalled: MicProfile = serde_json::from_str(&json).unwrap();

        let mut a = NoiseReducer::from_profile(8000, &profile);
        let mut b = NoiseReducer::from_profile(8000, &recalled);
        // Drive the 588 Hz whine + a 1 kHz voice tone through both.
        let mut sig_a: Vec<f32> = tone(588.0, 8000, 4000)
            .iter()
            .zip(tone(1000.0, 8000, 4000).iter())
            .map(|(w, v)| w + v)
            .collect();
        let mut sig_b = sig_a.clone();
        a.process(&mut sig_a);
        b.process(&mut sig_b);
        assert!(
            sig_a
                .iter()
                .zip(sig_b.iter())
                .all(|(x, y)| x.to_bits() == y.to_bits()),
            "JSON-round-tripped profile must rebuild a bit-identical NoiseReducer"
        );
    }

    #[test]
    fn from_profile_notches_the_measured_tone() {
        use crate::characterize::{MicProfile, NotchSpec};
        // A profile that says "this mic whines at 450 Hz".
        let profile = MicProfile {
            highpass_hz: 90.0,
            notches: vec![NotchSpec {
                freq_hz: 450.0,
                q: 20.0,
            }],
            noise_floor_dbfs: -50.0,
            gate_threshold_db: -44.0,
        };
        let mut nr = NoiseReducer::from_profile(8000, &profile);
        let mut whine = tone(450.0, 8000, 4000);
        nr.process(&mut whine);
        assert!(
            tail_peak(&whine) < 0.2,
            "the profile's 450 Hz notch should crush 450 Hz: {}",
            tail_peak(&whine)
        );
    }
}
