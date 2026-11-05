// SPDX-License-Identifier: LGPL-3.0-or-later
//! Damaged-frame concealment for the D-STAR and AMBE+2 half-rate decoders (`ErrorPolicy::Concealing`), designed for audio quality
//! rather than for compatibility with any reference decoder.
//!
//! Two facts about these frames drive the design. First, only 47 of the 72 bits are error protected (the two Golay blocks); the other
//! 25 arrive raw, and they include the whole voicing pattern in D-STAR, the low bits of the gain, the pitch and the spectral shape.
//! Second, speech parameters change slowly from frame to frame. So for each frame the decoder considers a few candidate readings
//! of the raw bits (as received, and each single-bit correction of the bits that matter most) and picks the one with the smallest
//! total cost, in nats: a flip costs `ln((1-p)/p)` for the current bit error rate estimate `p` (learned from how many errors the
//! Golay blocks corrected), and each candidate pays a continuity cost against the previous accepted frame (Laplace-shaped
//! priors on the level change, spectral shape change, voicing change and pitch change, with scales measured on real speech by
//! `examples/ambe_frame_statistics.rs`). If even the best candidate is implausible, the previous frame is repeated with a fade
//! instead of muting; the predictor state is only advanced by accepted frames.
//!
//! Which bits are worth correcting was measured with `examples/ambe_bit_sensitivity.rs`.

/// What the continuity prior looks at, computed from one decoded frame.
#[derive(Clone, Debug)]
pub struct Descriptor {
    pub w0: f64,
    /// Mean level of the harmonic amplitudes, dB.
    pub level_db: f64,
    /// Log amplitude (dB) at 16 points evenly spaced in relative frequency.
    pub spectrum: [f64; 16],
    /// Voicing of 8 evenly spaced frequency bands.
    pub voicing: [bool; 8],
}

impl Descriptor {
    /// `voiced` and `ml` are 1-indexed by harmonic (index 0 unused), as the decoders' parameter structs carry them.
    pub fn new(w0: f64, voiced: &[bool], ml: &[f64]) -> Self {
        let l = ml.len() - 1;
        let level_db = 20.0
            * (ml[1..].iter().map(|m| m * m).sum::<f64>() / l as f64)
                .sqrt()
                .max(1e-3)
                .log10();
        let spectrum = std::array::from_fn(|k| 20.0 * ml[1 + (k * l) / 16].max(1e-3).log10());
        let voicing = std::array::from_fn(|b| voiced[1 + (b * l) / 8]);
        Self {
            w0,
            level_db,
            spectrum,
            voicing,
        }
    }

    fn any_voiced(&self) -> bool {
        self.voicing.iter().any(|&v| v)
    }
}

/// Tunable constants; `Default` is the tuned set.
#[derive(Clone, Copy, Debug)]
pub struct ConcealParams {
    /// Laplace scale of the level change between frames, dB.
    pub level_scale_db: f64,
    /// Laplace scale of the spectral shape change (root mean square over 16 points), dB.
    pub shape_scale_db: f64,
    /// Cost per voicing band that differs from the previous frame, nats.
    pub voicing_cost: f64,
    /// Laplace scale of `|ln(w0 / w0_previous)|` (both frames voiced).
    pub pitch_scale: f64,
    /// Cost, in nats, of "this frame is garbage" (how unlikely the received parameters would be if they were random), before adding
    /// the cost of an error having happened at all. Repeating the previous frame costs this plus `-ln(P(a raw bit is wrong))`, so on a clean
    /// channel a large jump (a real speech onset) is accepted, and on a bad channel it is not.
    pub garbage_cost: f64,
    /// Extra cost per corrected error the channel code reported on the received frame, nats (suspicion of a miscorrection).
    pub error_suspicion: f64,
    /// Smoothing of the bit error rate estimate and its floor.
    pub ber_alpha: f64,
    pub ber_floor: f64,
    /// Bit corrections are only considered once the estimated error rate exceeds `ber_floor * flip_gate`, so a link that has shown no
    /// errors is decoded exactly as received.
    pub flip_gate: f64,
    /// Amplitude factor applied per consecutive repeated frame.
    pub fade_per_frame: f64,
    /// Repeated frames in a row after which the decoder state is reset.
    pub reset_after: u32,
    /// How much the continuity priors widen per consecutive repeated frame (the previous accepted frame gets older).
    pub widen_per_repeat: f64,
}

impl Default for ConcealParams {
    /// Tuned with `examples/ambe_error_concealment_eval.rs` (coordinate search on a weighted objective over error rates from 0% to
    /// 10% and a bursty channel, favouring the low error rates links run at), then checked on different random error patterns and on
    /// the other mode.
    fn default() -> Self {
        Self {
            level_scale_db: 3.5,
            shape_scale_db: 6.0,
            voicing_cost: 0.3,
            pitch_scale: 0.15,
            garbage_cost: 20.0,
            error_suspicion: 2.0,
            ber_alpha: 0.1,
            ber_floor: 0.01,
            flip_gate: 2.0,
            fade_per_frame: 0.5,
            reset_after: 30,
            widen_per_repeat: 0.25,
        }
    }
}

/// What to do with the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// Use candidate `index`.
    Accept(usize),
    /// Repeat the previous frame, scaled by this factor (0 = silence). `reset` says the decoder state should be reset first.
    Repeat { scale: f64, reset: bool },
}

pub struct Concealer {
    pub params: ConcealParams,
    previous: Option<Descriptor>,
    ber: f64,
    repeats: u32,
}

impl Concealer {
    pub fn new(params: ConcealParams) -> Self {
        Self {
            params,
            previous: None,
            ber: params.ber_floor,
            repeats: 0,
        }
    }

    pub fn error_rate(&self) -> f64 {
        self.ber
    }

    fn continuity_cost(&self, c: &Descriptor) -> f64 {
        let Some(p) = &self.previous else { return 0.0 };
        let widen = 1.0 + self.params.widen_per_repeat * self.repeats as f64;
        let q = &self.params;
        let mut cost = (c.level_db - p.level_db).abs() / (q.level_scale_db * widen);
        let shape = (c
            .spectrum
            .iter()
            .zip(&p.spectrum)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f64>()
            / 16.0)
            .sqrt();
        cost += shape / (q.shape_scale_db * widen);
        cost += q.voicing_cost
            * c.voicing
                .iter()
                .zip(&p.voicing)
                .filter(|(a, b)| a != b)
                .count() as f64;
        if c.any_voiced() && p.any_voiced() {
            cost += (c.w0 / p.w0).ln().abs() / (q.pitch_scale * widen);
        }
        cost
    }

    /// Chooses among the candidate readings of one frame. `candidates[i] = (flips, descriptor)` where `flips` is the number of raw bits
    /// changed from the received frame (candidate 0 is the frame as received, and is `None` if it is not a speech frame at all);
    /// `epsilon` is the total number of errors the channel code corrected in the received frame.
    pub fn decide(&mut self, candidates: &[(u32, Option<Descriptor>)], epsilon: u32) -> Decision {
        // Bit error rate estimate from the protected bits (47 per frame).
        let observed = epsilon as f64 / 47.0;
        self.ber = ((1.0 - self.params.ber_alpha) * self.ber + self.params.ber_alpha * observed)
            .clamp(self.params.ber_floor, 0.4);
        // No evidence of channel errors (nothing corrected now, and the running estimate at its floor): decode as received.
        if epsilon == 0 && self.ber <= self.params.ber_floor * self.params.flip_gate {
            if let Some((_, Some(d))) = candidates.first() {
                self.previous = Some(d.clone());
                self.repeats = 0;
                return Decision::Accept(0);
            }
        }
        let flip_cost = ((1.0 - self.ber) / self.ber).ln();
        let mut best: Option<(usize, f64)> = None;
        for (i, (flips, desc)) in candidates.iter().enumerate() {
            let Some(d) = desc else { continue };
            if *flips > 0 && self.ber <= self.params.ber_floor * self.params.flip_gate {
                continue;
            }
            let cost = flip_cost * *flips as f64
                + self.continuity_cost(d)
                + self.params.error_suspicion * epsilon as f64;
            if best.is_none_or(|(_, b)| cost < b) {
                best = Some((i, cost));
            }
        }
        // Probability that at least one of the 25 raw bits of this frame is wrong at the estimated error rate.
        let p_raw_error = 1.0 - (1.0 - self.ber).powi(25);
        let repeat_cost = self.params.garbage_cost - p_raw_error.max(1e-6).ln();
        match best {
            Some((i, cost)) if cost <= repeat_cost => {
                self.previous = candidates[i].1.clone();
                self.repeats = 0;
                Decision::Accept(i)
            }
            _ => {
                self.repeats += 1;
                let scale = self.params.fade_per_frame.powi(self.repeats as i32);
                Decision::Repeat {
                    scale: if scale < 0.03 { 0.0 } else { scale },
                    reset: self.repeats >= self.params.reset_after,
                }
            }
        }
    }
}
