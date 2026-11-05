// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`super::super::float::tia_102_baba::unvoiced_synthesis`] (TIA-102.BABA_2003.pdf
//! section 11.2, Eq. 117-126) -- see that module's own doc comment for the spec derivation and the
//! `kchmck/imbe.rs` cross-check; this module only documents the *fixed-point-specific* choices.
//!
//! Lives under `fixed::general`, not `fixed::tia_102_baba`, on purpose: this synthesis stage is
//! mode-independent MBE (mbelib itself shares one `mbe_synthesizeSpeech` across every mode), and
//! TIA-102.BABA is only the first mode with a float reference to port from -- AMBE+2 half-rate/D-STAR
//! will reuse this unchanged once/if they grow their own synthesis.
//!
//! **Numeric design, decided before writing any arithmetic below**: the noise recurrence (Eq. 117)
//! and its `NoiseState` window are *already* pure integer in the float sibling (no `f64` anywhere in
//! that state), so they're duplicated here verbatim rather than "ported" -- there is nothing to
//! convert. Everything downstream of it is genuinely real-valued and needs a real fixed-point format,
//! but at a much wider range than this crate's usual Q16.16 `i32`: `u(n)` itself ranges `0..53125`
//! (unnormalized, not `[-1,1]`), so the windowed noise DFT's own per-bin magnitude can reach the
//! hundreds of thousands in real terms -- `gamma_w * M_l` (the final per-band rescale target) can
//! itself exceed an `i32` Q16.16's `~32767` real ceiling for a loud harmonic. So every complex
//! spectral value in this pipeline (`ComplexQ16`, `unvoiced_dft_q16`'s output, the rescaled spectrum)
//! is kept as **`i64` Q16.16** throughout, narrowing back to a normal `i32` Q16.16 PCM sample only at
//! the very last step (the overlap-add output), matching `fixed_ops::mul_q16_i64`/`div_q16_i64`'s own
//! "wide container, same 16-fractional-bit convention" idiom used elsewhere in this crate.
//!
//! **A wide square root, not a "square everything first" reorganization**: Eq. 120's own
//! `scale = gamma_w * M_l / sqrt(power)` needs the square root of a value that can be far larger than
//! what [`super::fixed_ops::sqrt_q16`] (a plain `i32` result) can hold -- `sqrt(power)` itself is on
//! the same large scale as a DFT bin magnitude, not a normal Q16.16 quantity. An earlier version of
//! this module instead squared the whole scale factor first (`(gamma_w*M_l)^2 / power`, reasoning
//! that would land back in a normal small range before one final ordinary `sqrt_q16`) -- but that
//! makes the intermediate ratio *smaller* exactly when it's already small (a quiet harmonic, where
//! `scale` itself might be `~0.003`), and squaring a small ratio can underflow Q16.16's own
//! resolution entirely before the final sqrt ever runs (confirmed directly: a real quiet-frame test
//! measured only `~13 dB` SNR against float with that approach, far under this port's `40 dB` bar).
//! [`super::fixed_ops::sqrt_wide_q16`] avoids the overflow at its root (a normalize-shift-then-unshift
//! trick, not squaring anything first) -- but a *second*, related regression surfaced even after
//! that fix, at a still-quieter amplitude (peak `0.5`, near the real chip's own observed `R_M0`
//! floor): precomputing `scale = gamma_w * M_l / sqrt_wide_q16(power)` as its own `i32` Q16.16 value
//! before multiplying every bin by it throws away precision *scale itself* doesn't have room for at
//! that magnitude (`~0.0001`, a handful of representable Q16.16 levels), even though the *final*
//! rescaled bin (`~gamma_w * M_l` in size) does. The fix: never materialize `scale` at all --
//! [`mul_div_wide`] rescales each bin as `(bin * gamma_w * M_l) / sqrt_wide_q16(power)` in one
//! `i128`-widened division per component, so the only rounding that happens is the one the *final*,
//! properly-sized result actually needs.

use std::collections::VecDeque;

use super::fixed_ops::{div_q16_i64, mul_q16_i64, sqrt_wide_q16};
use super::trig::{cos_q16, sin_q16};

/// `N` (section 11.1): the number of PCM samples in one synthesis frame, 20 ms at 8 kHz.
pub const N: usize = 160;

/// A spectral value at `i64` Q16.16 precision -- see this module's own doc comment for why the
/// ordinary `i32` Q16.16 convention isn't wide enough here.
#[derive(Clone, Copy)]
pub(crate) struct ComplexQ16 {
    pub re: i64,
    pub im: i64,
}

impl ComplexQ16 {
    const ZERO: ComplexQ16 = ComplexQ16 { re: 0, im: 0 };

    fn add(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }

    /// Complex multiply by an `i32` Q16.16 unit phasor (`cos_val + j*sin_val`), the shape every DFT
    /// twiddle factor in this module takes.
    fn mul_phasor(self, cos_val: i32, sin_val: i32) -> Self {
        Self {
            re: mul_q16_i64(self.re, cos_val) - mul_q16_i64(self.im, sin_val),
            im: mul_q16_i64(self.re, sin_val) + mul_q16_i64(self.im, cos_val),
        }
    }

    /// `|self|^2` in Q16.16, via an `i128` intermediate (`self.re`/`self.im` can be large enough that
    /// squaring directly in `i64` would overflow -- same reasoning as `fixed_ops::mul_q16_i64`'s own
    /// doc comment).
    fn norm_sqr_q16(self) -> i64 {
        let re2 = (self.re as i128) * (self.re as i128);
        let im2 = (self.im as i128) * (self.im as i128);
        ((re2 + im2) >> 16) as i64
    }
}

/// `a_q16 * b_q16 / c_q16`, all three sharing the same `i64` Q16.16 convention, computed as one
/// division of a widened `i128` product rather than `div_q16_i64(mul_q16_i64(a, b_as_i32), c)` --
/// which would need `b` narrowed to a plain `i32` first (this module's own `gm_q16` routinely
/// doesn't fit one) and would separately round twice (once in the multiply, once in the divide)
/// instead of once here. Used by [`unvoiced_spectrum_q16`] to rescale a spectral bin by
/// `gamma_w * M_l / sqrt(power)` directly, without ever materializing that ratio as its own
/// (potentially far-too-imprecise-for-a-quiet-harmonic) intermediate value -- see this module's own
/// doc comment.
fn mul_div_wide(a_q16: i64, b_q16: i64, c_q16: i64) -> i64 {
    if c_q16 == 0 {
        return 0;
    }
    ((a_q16 as i128) * (b_q16 as i128) / (c_q16 as i128)) as i64
}

/// Advances the noise recurrence (Eq. 117) by one step -- byte-for-byte the same integer recurrence
/// as `float::tia_102_baba::unvoiced_synthesis::advance_noise` (already pure integer, nothing to port).
pub(crate) fn advance_noise(u: i64) -> i64 {
    (171 * u + 11213).rem_euclid(53125)
}

/// Persistent state for the noise generator `u(n)` -- see the float sibling's own doc comment for the
/// full derivation; this is a verbatim duplicate (not a numeric port) since the recurrence was already
/// pure integer.
pub struct NoiseState {
    last: i64,
    window: VecDeque<i64>,
}

impl NoiseState {
    pub fn new() -> Self {
        let mut state = Self {
            last: 3147,
            window: VecDeque::with_capacity(209),
        };
        for _ in 0..209 {
            state.step();
        }
        state
    }

    fn step(&mut self) {
        self.last = advance_noise(self.last);
        if self.window.len() == 209 {
            self.window.pop_front();
        }
        self.window.push_back(self.last);
    }

    pub fn advance_frame(&mut self) {
        for _ in 0..N {
            self.step();
        }
    }

    /// `u(relative)` for `relative` in `-104..=104`, relative to the current frame's own `n = 0`.
    pub fn at(&self, relative: i32) -> Option<i64> {
        if !(-104..=104).contains(&relative) {
            return None;
        }
        self.window.get((relative + 104) as usize).copied()
    }
}

impl Default for NoiseState {
    fn default() -> Self {
        Self::new()
    }
}

/// `w_S(n)` (Annex I) in Q16.16: `clamp((105-|n|)/50, 0, 1)`, computed as one exact integer ratio
/// (`1/50`, i.e. the spec's own `0.02` literal) rather than a pre-rounded `0.02` constant, so no
/// extra rounding error is introduced beyond the final Q16.16 truncation.
pub fn synthesis_window_q16(n: i32) -> i32 {
    let abs_n = n.unsigned_abs() as i64;
    if abs_n > 105 {
        return 0;
    }
    let numerator = (105 - abs_n) * 65536 + 25; // +25 rounds to nearest before the /50.
    (numerator / 50).clamp(0, 65536) as i32
}

/// Converts a bin/sample index pair `(m, n)` for the `256`-point DFT/IDFT into this crate's own `u32`
/// phase-turn convention, exactly (no precision loss at all): the twiddle angle `2*pi*m*n/256` is
/// `m*n/256` *turns*, and a full turn is `1u32 << 32`, so this is `m*n` scaled by `2^32/256 = 2^24`
/// and wrapped -- an exact integer computation, unlike converting through radians first.
fn dft_phase(m: i32, n: i32) -> u32 {
    ((m as i64) * (n as i64)).wrapping_mul(1i64 << 24) as u32
}

/// `(cos, sin)` in Q16.16 of the twiddle angle `2*pi*(m*n)/256`, from a table of the 256 distinct phases computed once with the
/// very same `cos_q16`/`sin_q16` the per-term evaluation used, so results are bit-identical and the transforms below no longer
/// evaluate trigonometry per term.
fn twiddle_q16(m: i32, n: i32) -> (i32, i32) {
    static TABLE: std::sync::OnceLock<Vec<(i32, i32)>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        (0..256)
            .map(|k| {
                let phase = dft_phase(k, 1);
                (cos_q16(phase), sin_q16(phase))
            })
            .collect()
    });
    table[((m as i64) * (n as i64)).rem_euclid(256) as usize]
}

/// `U_w(m)` (Eq. 118): the 256-point DFT of the current frame's own windowed noise sequence, for `m`
/// in `-128..=127`.
fn unvoiced_dft_q16(noise: &NoiseState) -> [ComplexQ16; 256] {
    let mut uw = [ComplexQ16::ZERO; 256];
    for (i, slot) in uw.iter_mut().enumerate() {
        let m = i as i32 - 128;
        let mut acc = ComplexQ16::ZERO;
        for n in -104i32..=104 {
            let u_n = noise.at(n).expect("noise window covers -104..=104");
            let sample_q16 = u_n * (synthesis_window_q16(n) as i64); // plain int * Q16.16 = Q16.16.
                                                                     // Eq. 118's own twiddle is negative (-2*pi*m*n/256); negate via -m rather than negating
                                                                     // the whole phase, since dft_phase's own m*n product already handles negative operands.
            let (cos_val, sin_val) = twiddle_q16(-m, n);
            acc = acc.add(ComplexQ16 {
                re: mul_q16_i64(sample_q16, cos_val),
                im: mul_q16_i64(sample_q16, sin_val),
            });
        }
        *slot = acc;
    }
    uw
}

fn bin_index(m: i32) -> usize {
    (m + 128) as usize
}

/// `256 / (2*pi)` in Q32 -- the shared scale factor of both harmonic-band-edge equations.
const BAND_EDGE_SCALE_Q32: i128 = 174_992_710_548; // round(256.0 / (2.0*PI) * 2^32)

/// `ceil(a~_l)` / `ceil(b~_l)` (Eq. 122/123) share this shape: `(256/(2pi)) * (l +- 0.5) * omega0_tilde`,
/// computed as `(256/(2pi)) * (2l +- 1) * omega0_tilde / 2` to keep `2l +- 1` an exact integer. The
/// pitch is Q32 (`radians/sample * 2^32`), so the whole product carries `2^64` (scale times pitch),
/// plus one more bit for the shared "/2"; the ceiling is taken on that wide product directly rather
/// than on a Q16.16 intermediate. Both edges are always non-negative (`l >= 1`, `omega0_tilde > 0`).
fn band_edge_ceil(two_l_plus_minus_1: i64, omega0_tilde_q32: i64) -> i32 {
    let product = BAND_EDGE_SCALE_Q32 * (two_l_plus_minus_1 as i128) * (omega0_tilde_q32 as i128);
    ((product + ((1i128 << 65) - 1)) >> 65) as i32
}

fn band_edge_a_ceil(l: u32, omega0_tilde_q32: i64) -> i32 {
    band_edge_ceil(2 * l as i64 - 1, omega0_tilde_q32)
}

fn band_edge_b_ceil(l: u32, omega0_tilde_q32: i64) -> i32 {
    band_edge_ceil(2 * l as i64 + 1, omega0_tilde_q32)
}

/// `gamma_w` (Eq. 121) in Q16.16: `round(146.6432708443356 * 65536)`, the exact same constant the
/// float sibling computes from Annex C/I's own window definitions (`unvoiced_scaling_coefficient`) --
/// precomputed once offline (it depends on no per-frame data at all) rather than re-summing two
/// 211/209-term window tables in fixed point on every call. Matches the float value to 1 part in
/// ~170,000 (`146.6432647705078` vs `146.6432708443356`), far inside this crate's 1% tolerance.
pub const UNVOICED_SCALING_COEFFICIENT_Q16: i32 = 9_610_413;

/// `U~_w(m)` (Eq. 119-120, 124): see this module's own doc comment for why `scale` is derived via
/// [`sqrt_wide_q16`] directly, not a "square everything first" reorganization.
fn unvoiced_spectrum_q16(
    noise: &NoiseState,
    omega0_tilde_q32: i64,
    voiced: &[bool],
    spectral_amplitudes_q16: &[i32],
    gamma_w_q16: i32,
) -> Option<[ComplexQ16; 256]> {
    if voiced.len() != spectral_amplitudes_q16.len() {
        return None;
    }
    let uw = unvoiced_dft_q16(noise);
    let mut result = [ComplexQ16::ZERO; 256];
    let l_hat = voiced.len() as u32;

    for l in 1..=l_hat {
        if voiced[(l - 1) as usize] {
            continue; // Eq. 119: stays zero.
        }
        let a = band_edge_a_ceil(l, omega0_tilde_q32);
        let b = band_edge_b_ceil(l, omega0_tilde_q32);
        if b <= a {
            continue; // Degenerate (unreachable for any real pitch period) zero-width band.
        }

        let power_q16: i64 = (a..b)
            .map(|eta| uw[bin_index(eta)].norm_sqr_q16())
            .sum::<i64>()
            / (b - a) as i64;
        if power_q16 == 0 {
            continue; // No noise energy in this band (degenerate input); leave it zeroed.
        }

        let gm_q16 = mul_q16_i64(
            spectral_amplitudes_q16[(l - 1) as usize] as i64,
            gamma_w_q16,
        );
        let sqrt_power_q16 = sqrt_wide_q16(power_q16);
        if sqrt_power_q16 == 0 {
            continue;
        }

        for m in a..b {
            for sign_m in [m, -m] {
                let bin = uw[bin_index(sign_m)];
                // result = bin * gamma_w * M_l / sqrt(power), done as one division per component
                // directly (`mul_div_wide`) rather than precomputing a separate `scale = gamma_w *
                // M_l / sqrt(power)` ratio first -- see this module's own doc comment: for a very
                // quiet harmonic, `scale` alone can be too small for Q16.16 to hold with any real
                // precision, even though the final rescaled bin (comparable in size to `gamma_w *
                // M_l` itself) is not.
                result[bin_index(sign_m)] = ComplexQ16 {
                    re: mul_div_wide(bin.re, gm_q16, sqrt_power_q16),
                    im: mul_div_wide(bin.im, gm_q16, sqrt_power_q16),
                };
            }
        }
    }
    Some(result)
}

/// `u~_w(n)` (Eq. 125): the 256-point inverse DFT of `U~_w(m)`, for `n` in `-128..=127`. Only the real
/// part is kept, exactly as the float sibling does (mathematically guaranteed real by the spectrum's
/// own conjugate symmetry -- see that module's own doc comment and test).
fn unvoiced_time_domain_q16(spectrum: &[ComplexQ16; 256]) -> [i64; 256] {
    let mut out = [0i64; 256];
    for (i, slot) in out.iter_mut().enumerate() {
        let n = i as i32 - 128;
        let mut acc = ComplexQ16::ZERO;
        for (j, &bin) in spectrum.iter().enumerate() {
            let m = j as i32 - 128;
            let (cos_val, sin_val) = twiddle_q16(m, n);
            acc = acc.add(bin.mul_phasor(cos_val, sin_val));
        }
        *slot = acc.re / 256; // Eq. 125's own 1/256 normalization; only the real part is kept.
    }
    out
}

fn time_domain_at_q16(samples: &[i64; 256], n: i32) -> i64 {
    if (-128..=127).contains(&n) {
        samples[(n + 128) as usize]
    } else {
        0
    }
}

/// Persistent unvoiced-synthesis state: the previous frame's own time-domain unvoiced signal, for
/// Eq. 126's own overlap-add. See the float sibling's own doc comment for why the noise generator
/// itself is owned by the caller, shared with voiced synthesis, not by this struct.
pub struct UnvoicedState {
    previous_time_domain_q16: [i64; 256],
}

impl UnvoicedState {
    pub fn new() -> Self {
        Self {
            previous_time_domain_q16: [0; 256],
        }
    }

    /// Synthesizes the current frame's own unvoiced speech component `s_uv(n)` (Eq. 117-126) as
    /// `i32` Q16.16 PCM samples, advancing the overlap-add history for the next call. See the float
    /// sibling's own doc comment for the full parameter contract.
    pub fn synthesize(
        &mut self,
        noise: &NoiseState,
        omega0_tilde_q32: i64,
        voiced: &[bool],
        spectral_amplitudes_q16: &[i32],
    ) -> Option<[i32; N]> {
        let spectrum = unvoiced_spectrum_q16(
            noise,
            omega0_tilde_q32,
            voiced,
            spectral_amplitudes_q16,
            UNVOICED_SCALING_COEFFICIENT_Q16,
        )?;
        let current_time_domain_q16 = unvoiced_time_domain_q16(&spectrum);

        let mut s_uv = [0i32; N];
        for (n, slot) in s_uv.iter_mut().enumerate() {
            let n = n as i32;
            let w_n = synthesis_window_q16(n) as i64;
            let w_shifted = synthesis_window_q16(n - N as i32) as i64;
            let prev = time_domain_at_q16(&self.previous_time_domain_q16, n);
            let curr = time_domain_at_q16(&current_time_domain_q16, n - N as i32);
            // numerator/denominator share the same "two Q16.16 factors" scale (w * sample, w^2), so
            // dividing them via div_q16_i64 (which normalizes both operands together) recovers a
            // plain Q16.16 result directly, with no separate rescale.
            let numerator = mul_q16_i64(prev, w_n as i32) + mul_q16_i64(curr, w_shifted as i32);
            let denominator = ((w_n * w_n) >> 16) + ((w_shifted * w_shifted) >> 16);
            *slot = div_q16_i64(numerator, denominator);
        }

        self.previous_time_domain_q16 = current_time_domain_q16;
        Some(s_uv)
    }
}

impl Default for UnvoicedState {
    fn default() -> Self {
        Self::new()
    }
}
