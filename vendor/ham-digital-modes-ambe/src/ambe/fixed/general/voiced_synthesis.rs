// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of [`super::super::float::tia_102_baba::voiced_synthesis`] (TIA-102.BABA_2003.pdf
//! section 11.3, Eq. 127-141) -- see that module's own doc comment for the spec derivation, the two
//! documented notational resolutions (the bare `L~` in Eq. 140, and `phi_l(0)` for harmonics beyond
//! `max[L~(-1), L~(0)]`), both reused unchanged here.
//!
//! **The one genuinely new fixed-point risk this module has that `unvoiced_synthesis` doesn't**:
//! `psi_l`/`phi_l` (Eq. 139-140) are phase *accumulators* that add a per-frame increment forever,
//! for the lifetime of a call -- unlike everything in `unvoiced_synthesis`, which only ever combines
//! bounded, per-frame or per-sample quantities. A per-frame increment
//! (`(omega0_prev+omega0_curr)*l*N/2`, up to a few thousand radians at high harmonics) fits an `i32`
//! Q16.16 easily on its own, but the float sibling's own running `f64` sum of *many* such increments
//! grows without bound -- ported literally as a growing Q16.16 `i32`, it would overflow within a few
//! hundred frames (a handful of seconds of real audio), long before any real call to this module
//! would end.
//!
//! **The fix**: `psi`/`phi` are stored as this crate's own wrapping `u32` *phase* (`fixed::general::
//! trig`'s own convention, `1u32 << 32` = one full turn) rather than as growing Q16.16 radians --
//! `u32::wrapping_add` never overflows, and a phase's own wraparound *is* the mod-`2*pi` reduction a
//! real angle needs anyway, not a bug to guard against. Each frame's own per-harmonic increment (and
//! the dither term) is computed as an ordinary *bounded* Q16.16 radian value first (safe, since it's
//! never itself accumulated across frames) and converted to a phase *delta* via `trig::
//! phase_from_radians_q16` only at the point it's added to the persistent phase. The reverse
//! conversion, `trig::radians_from_phase_q16`, recovers a bounded `(-pi, pi]` radian value from the
//! stored phase whenever the per-sample synthesis formulas below need to add it to another genuine
//! radian quantity (`theta_l(n)`'s own `omega0*n*l` term, etc.) before a final `cos`/`sin` -- see that
//! function's own doc comment for why interpreting the same bits as `i32` gives exactly the right
//! symmetric-around-zero representative for free.

use super::fixed_ops::mul_q16;
use super::trig::{
    cos_q16, phase_from_radians_q16, phase_from_radians_q32, radians_q32_from_phase,
};
use super::unvoiced_synthesis::{synthesis_window_q16, NoiseState, N};

/// The largest harmonic index `psi_l`/`phi_l` are ever tracked for (Eq. 139's own stated range).
pub const MAX_HARMONICS: usize = 56;

/// `omega0(-1)` (Annex A): `round(0.02985 * pi * 2^32)`.
const OMEGA0_INITIAL_Q32: i64 = 402_767_175;

/// The per-frame phase increment shared by [`psi_update_q32`] and [`delta_omega_q32`]:
/// `(omega0_prev + omega0_curr) * l * N / 2`, computed as one Q32 radian value in `i64` (`l * N /
/// 2` is always an exact integer since `N` is even, so this needs no separate "0.5" literal).
///
/// **Pitch is Q32 (`radians/sample * 2^32`) throughout this module**, not Q16.16: a harmonic's phase
/// is `l * omega0 * n`, so a Q16.16 `omega0`'s ~1.5e-5 rad resolution (0.03-0.1% relative at low
/// pitch) grew to ~0.05 rad of phase error at high harmonics over one frame, which capped
/// fixed-vs-float SNR at roughly 26-35 dB. Q32 makes that error negligible (~1e-9 rad per unit).
fn frame_increment_q32(omega0_prev_q32: i64, omega0_curr_q32: i64, l: u32) -> i64 {
    let l_n_half = (l * N as u32 / 2) as i64;
    (omega0_prev_q32 + omega0_curr_q32) * l_n_half
}

/// `psi_l(0)` (Eq. 139) as a phase delta: advances `psi_prev`'s own stored phase by this frame's
/// bounded increment, converted to a phase via [`phase_from_radians_q32`] only at the point it's
/// added -- see this module's own doc comment for why `psi`/`phi` are phases, not growing radians.
fn psi_update_q32(psi_prev_phase: u32, omega0_prev_q32: i64, omega0_curr_q32: i64, l: u32) -> u32 {
    let increment_q32 = frame_increment_q32(omega0_prev_q32, omega0_curr_q32, l);
    psi_prev_phase.wrapping_add(phase_from_radians_q32(increment_q32))
}

/// `rho_l(0)` (Eq. 141) in Q16.16 radians, bounded to `(-pi, pi]` by construction (`u_l` is a plain
/// count `0..53125`, linearly mapped) -- computed directly as a radian value, not via the phase
/// machinery, since this result is used as an ordinary bounded angle in [`phi_new_q16`]'s own
/// dither term, not accumulated anywhere.
fn phase_dither_q16(noise: &NoiseState, l: u32) -> i32 {
    let u_l = noise
        .at(l as i32)
        .expect("harmonic index 1..=56 is within the noise window's own -104..=104 range");
    // (2*pi/53125)*u_l - pi, computed as one exact ratio: u_l * TWO_PI_Q16_16 is already Q16.16
    // (u_l is a plain integer), so dividing by 53125 (also plain/dimensionless) needs no extra shift.
    let scaled_q16 = (u_l * (super::fixed_ops::TWO_PI_Q16_16 as i64)) / 53125;
    scaled_q16 as i32 - super::fixed_ops::PI_Q16_16
}

/// `Delta_omega_l(0)` (Eq. 137-138) in Q32 radians/sample (`phi` arguments are Q32 radians too). The mod-`2*pi` wrap Eq. 138's own
/// literal `floor` form performs is done here via the exact phase round trip
/// (`phase_from_radians_q16` then `radians_from_phase_q16`) rather than a hand-rolled floor -- the
/// same reduction, reusing an already-exact primitive instead of a second implementation of it.
fn delta_omega_q32(
    phi_prev_q32: i64,
    phi_curr_q32: i64,
    omega0_prev_q32: i64,
    omega0_curr_q32: i64,
    l: u32,
) -> i64 {
    let increment_q32 = frame_increment_q32(omega0_prev_q32, omega0_curr_q32, l);
    let delta_phi_q32 = phi_curr_q32 - phi_prev_q32 - increment_q32;
    let wrapped_q32 = radians_q32_from_phase(phase_from_radians_q32(delta_phi_q32));
    // Round to nearest (ties away from zero), not truncate: this result is later multiplied by `n`
    // (up to 159) inside theta_q16_i64, so a truncated-toward-zero remainder here is amplified right
    // back up by that same factor rather than shrinking away -- this crate's own
    // theta_at_n_equals_capital_n_lands_near_phi_curr_mod_2pi test failed at l=7 by `~1074331` (of
    // `u32::MAX`) before this rounding was added, though a real share of that particular number also
    // came from a separate, larger scale error `trig::phase_from_radians_q16` had at the time (fixed
    // since, see `trig::PI_Q48`) rather than from this term's own truncation alone.
    let half = N as i64 / 2;
    let rounded = if wrapped_q32 >= 0 {
        wrapped_q32 + half
    } else {
        wrapped_q32 - half
    };
    rounded / N as i64
}

/// `theta_l(n)` (Eq. 136) in Q32 radians (not yet phase-reduced -- callers convert via
/// [`phase_from_radians_q32`] right before the final `cos`/`sin`, since this is itself only an
/// intermediate sum, not a value stored anywhere). Kept in `i64` throughout: `l` up to 56 and `n` up
/// to 159 make the quadratic `n*n` term's own numerator large enough that narrowing early would risk
/// exactly the kind of intermediate overflow this crate's own established discipline checks for,
/// even though the final angle itself is always a bounded, ordinary-sized radian value.
fn theta_q32_i64(
    n: i32,
    phi_prev_q32: i64,
    omega0_prev_q32: i64,
    omega0_curr_q32: i64,
    delta_omega_q32: i64,
    l: u32,
) -> i64 {
    let l_i = l as i64;
    let n_i = n as i64;
    let term1 = phi_prev_q32;
    let term2 = (omega0_prev_q32 * l_i + delta_omega_q32) * n_i;
    let term3 = ((omega0_curr_q32 - omega0_prev_q32) * l_i * n_i * n_i) / (2 * N as i64);
    term1 + term2 + term3
}

/// Persistent voiced-synthesis state -- see this module's own doc comment for why `psi`/`phi` are
/// stored as phases, and the float sibling's own doc comment for the full Annex A initial-value
/// derivation (`M_bar_l(-1) = 0.0`, `v_bar_l(-1) = false`, `L~(-1) = 30`, all reused unchanged here).
pub struct VoicedState {
    psi_phase: [u32; MAX_HARMONICS],
    phi_phase: [u32; MAX_HARMONICS],
    omega0_prev_q32: i64,
    l_hat_prev: u32,
    voiced_prev: [bool; MAX_HARMONICS],
    amplitudes_prev_q16: [i32; MAX_HARMONICS],
}

impl VoicedState {
    pub fn new() -> Self {
        Self {
            psi_phase: [0; MAX_HARMONICS],
            phi_phase: [0; MAX_HARMONICS],
            omega0_prev_q32: OMEGA0_INITIAL_Q32,
            l_hat_prev: 30,
            voiced_prev: [false; MAX_HARMONICS],
            amplitudes_prev_q16: [0; MAX_HARMONICS],
        }
    }

    /// Synthesizes the current frame's own voiced speech component `s_v(n)` (Eq. 127-141) as `i32`
    /// Q16.16 PCM samples, advancing the phase-tracking state for the next call. See the float
    /// sibling's own doc comment for the full parameter contract (`noise` must already reflect the
    /// current frame, shared with `unvoiced_synthesis::UnvoicedState`).
    pub fn synthesize(
        &mut self,
        noise: &NoiseState,
        omega0_curr_q32: i64,
        voiced: &[bool],
        spectral_amplitudes_q16: &[i32],
    ) -> Option<[i32; N]> {
        if voiced.len() != spectral_amplitudes_q16.len() || voiced.len() > MAX_HARMONICS {
            return None;
        }
        let l_hat_curr = voiced.len() as u32;
        let quarter_l_hat_curr = l_hat_curr / 4;
        let l_uv_curr = voiced.iter().filter(|&&v| !v).count() as i64;
        let max_l = self.l_hat_prev.max(l_hat_curr);

        let mut phi_prev_phase = [0u32; MAX_HARMONICS];
        let mut phi_curr_phase = [0u32; MAX_HARMONICS];
        for l in 1..=MAX_HARMONICS as u32 {
            let idx = (l - 1) as usize;
            phi_prev_phase[idx] = self.phi_phase[idx];

            let psi_new_phase = psi_update_q32(
                self.psi_phase[idx],
                self.omega0_prev_q32,
                omega0_curr_q32,
                l,
            );

            let phi_new_phase = if l <= quarter_l_hat_curr {
                psi_new_phase
            } else if l <= max_l && l_hat_curr > 0 {
                // (l_uv_curr / l_hat_curr) * phase_dither(l) -- a fraction (<=1) of a bounded angle,
                // itself always bounded, so plain Q16.16 radian arithmetic (not phase-native) is
                // exact here; only the final addition to psi_new needs the phase conversion.
                let ratio_q16 =
                    super::fixed_ops::div_q16((l_uv_curr as i32) << 16, (l_hat_curr as i32) << 16);
                let dither_term_q16 = mul_q16(ratio_q16, phase_dither_q16(noise, l));
                psi_new_phase.wrapping_add(phase_from_radians_q16(dither_term_q16))
            } else {
                psi_new_phase
            };

            self.psi_phase[idx] = psi_new_phase;
            self.phi_phase[idx] = phi_new_phase;
            phi_curr_phase[idx] = phi_new_phase;
        }

        let mut s_v = [0i64; N];
        for l in 1..=max_l {
            let idx = (l - 1) as usize;
            let was_voiced = self.voiced_prev[idx];
            let was_amp_q16 = self.amplitudes_prev_q16[idx];
            let is_voiced = l <= l_hat_curr && voiced[idx];
            let is_amp_q16 = if l <= l_hat_curr {
                spectral_amplitudes_q16[idx]
            } else {
                0
            };

            let phi_prev_q32 = radians_q32_from_phase(phi_prev_phase[idx]);
            let phi_curr_q32 = radians_q32_from_phase(phi_curr_phase[idx]);
            let delta_omega_l_q32 = delta_omega_q32(
                phi_prev_q32,
                phi_curr_q32,
                self.omega0_prev_q32,
                omega0_curr_q32,
                l,
            );

            for (n, slot) in s_v.iter_mut().enumerate() {
                let n_i = n as i32;
                let n_shifted = n_i - N as i32;

                let sample_q16: i64 = match (was_voiced, is_voiced) {
                    (false, false) => 0, // Eq. 130.
                    (true, false) => {
                        // Eq. 131.
                        let angle = self.omega0_prev_q32 * (l as i64) * (n_i as i64) + phi_prev_q32;
                        let cos_val = cos_q16(phase_from_radians_q32(angle));
                        let w = synthesis_window_q16(n_i);
                        mul_q16(mul_q16(w, was_amp_q16), cos_val) as i64
                    }
                    (false, true) => {
                        // Eq. 132.
                        let angle =
                            omega0_curr_q32 * (l as i64) * (n_shifted as i64) + phi_curr_q32;
                        let cos_val = cos_q16(phase_from_radians_q32(angle));
                        let w = synthesis_window_q16(n_shifted);
                        mul_q16(mul_q16(w, is_amp_q16), cos_val) as i64
                    }
                    (true, true) => {
                        let omega0_diff = (omega0_curr_q32 - self.omega0_prev_q32).abs();
                        let threshold = omega0_curr_q32.abs() / 10; // 0.1 * omega0_curr.
                        let big_jump = l >= 8 || omega0_diff >= threshold;
                        if big_jump {
                            // Eq. 133: both halves synthesized independently and summed.
                            let angle_prev =
                                self.omega0_prev_q32 * (l as i64) * (n_i as i64) + phi_prev_q32;
                            let cos_prev = cos_q16(phase_from_radians_q32(angle_prev));
                            let w_prev = synthesis_window_q16(n_i);
                            let angle_curr =
                                omega0_curr_q32 * (l as i64) * (n_shifted as i64) + phi_curr_q32;
                            let cos_curr = cos_q16(phase_from_radians_q32(angle_curr));
                            let w_curr = synthesis_window_q16(n_shifted);
                            (mul_q16(mul_q16(w_prev, was_amp_q16), cos_prev) as i64)
                                + (mul_q16(mul_q16(w_curr, is_amp_q16), cos_curr) as i64)
                        } else {
                            // Eq. 134-135: continuous-phase interpolation.
                            let a_l_n_q16 = was_amp_q16 as i64
                                + ((n_i as i64) * (is_amp_q16 as i64 - was_amp_q16 as i64))
                                    / N as i64;
                            let theta_l_n = theta_q32_i64(
                                n_i,
                                phi_prev_q32,
                                self.omega0_prev_q32,
                                omega0_curr_q32,
                                delta_omega_l_q32,
                                l,
                            );
                            let cos_val = cos_q16(phase_from_radians_q32(theta_l_n)) as i64;
                            (a_l_n_q16 * cos_val) >> 16
                        }
                    }
                };
                *slot += 2 * sample_q16; // Eq. 127's own factor of 2.
            }
        }

        self.omega0_prev_q32 = omega0_curr_q32;
        self.l_hat_prev = l_hat_curr;
        for l in 1..=MAX_HARMONICS as u32 {
            let idx = (l - 1) as usize;
            self.voiced_prev[idx] = l <= l_hat_curr && voiced[idx];
            self.amplitudes_prev_q16[idx] = if l <= l_hat_curr {
                spectral_amplitudes_q16[idx]
            } else {
                0
            };
        }

        // Saturate rather than let `as i32` silently wrap: `s_v` sums up to `MAX_HARMONICS` (56)
        // harmonics' own Q16.16 amplitudes (Eq. 127's own literal formula, no headroom reserved), and
        // real, richly-harmonic voiced speech genuinely can align enough harmonics in phase at a
        // pitch pulse's own peak to exceed `i32`'s Q16.16 range -- confirmed directly (found via a
        // test sweeping harmonic count with realistic-scale amplitudes: `l_hat=40` stayed a normal
        // `~200`-unit fixed/float gap, `l_hat=50` jumped to `~65500`, the unmistakable signature of a
        // wrapped sign flip, not a precision issue). This crate's own `fixed_ops::div_q16` already
        // saturates rather than wraps on its own out-of-range case for the same reason: a wrapped
        // sample is a catastrophic, audible glitch (a huge sign-flipped spike), while a saturated one
        // is ordinary, expected clipping. `i32` Q16.16 gives exactly `+-32768.0` of real headroom --
        // zero margin over a full 16-bit PCM range even before `synthesis::SynthesisState` (not yet
        // ported) adds the unvoiced component on top, and the float sibling's own reference chip
        // comparison is already documented as running "several times louder than the chip" (see
        // `examples/ambe_chip_pcm_vs_float_synthesis_ratet27.rs`'s own doc comment). Whether this
        // intermediate needs a wider representation (e.g. `i64` Q16.16) is a real, open question the
        // orchestration layer's own port will have to settle once voiced and unvoiced are actually
        // combined; saturating here is simply the correct behavior for `i32` regardless of how that's
        // resolved.
        let mut out = [0i32; N];
        for (o, &s) in out.iter_mut().zip(s_v.iter()) {
            *o = s.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }
        Some(out)
    }
}

impl Default for VoicedState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed-point analogue of the float sibling's own
    /// `theta_at_n_equals_capital_n_lands_exactly_on_phi_curr_mod_2pi` -- a pure integer
    /// self-consistency check needing no float comparison at all (see `isqrt.rs`'s own doc comment
    /// for why such checks live inline here rather than in the integration test files). `theta_l(N)`
    /// must land on `phi_curr` (mod `2*pi`) to within a small tolerance (Q16.16's own rounding, not
    /// zero, unlike the float sibling's exact `f64` invariant).
    #[test]
    fn theta_at_n_equals_capital_n_lands_near_phi_curr_mod_2pi() {
        // Literal Q16.16 values (`round(x * 65536)`), not computed from a float expression here --
        // this whole file is `#![deny(clippy::float_arithmetic)]`, which lints `#[cfg(test)]` code
        // too (see `trig.rs`'s own doc comment). Mirrors the float sibling's own test cases exactly:
        // phi_prev=0.3, phi_curr=4.1, omega0_prev=2*pi/100, omega0_curr=2*pi/105, l=1 and l=7; then
        // phi_prev=5.9, phi_curr=0.2, omega0_prev=2*pi/60, omega0_curr=2*pi/58, l=3 (phi_curr <
        // phi_prev); then phi_prev=-2.0, phi_curr=2.0, omega0_prev=2*pi/40, omega0_curr=2*pi/200,
        // l=6 (a large omega0 jump).
        let cases = [
            (
                19661i64 << 16,
                268698i64 << 16,
                4118i64 << 16,
                3922i64 << 16,
                1u32,
            ),
            (19661 << 16, 268698 << 16, 4118 << 16, 3922 << 16, 7u32),
            (386662 << 16, 13107 << 16, 6863 << 16, 7100 << 16, 3u32),
            (-131072 << 16, 131072 << 16, 10294 << 16, 2059 << 16, 6u32),
        ];
        for (phi_prev, phi_curr, omega0_prev, omega0_curr, l) in cases {
            let d_omega = delta_omega_q32(phi_prev, phi_curr, omega0_prev, omega0_curr, l);
            let theta_n = theta_q32_i64(N as i32, phi_prev, omega0_prev, omega0_curr, d_omega, l);
            let phase_diff =
                phase_from_radians_q32(theta_n).wrapping_sub(phase_from_radians_q32(phi_curr));
            let signed_diff = phase_diff as i32; // nearest-to-zero representative of the wrap distance.
                                                 // `delta_omega_q16` is itself rounded to the nearest Q16.16 unit (a genuine, expected
                                                 // quantization step -- see `delta_omega_q16`'s own doc comment), and that rounding error
                                                 // is multiplied by `n` (up to `N`) inside `theta_q16_i64`, so the worst case here is
                                                 // `~0.5/65536 * N` radians of residual phase error, not zero -- measured directly across
                                                 // these four cases at `~504287..681343` (of `u32::MAX`), i.e. up to `~0.01` radians,
                                                 // `~0.16%` of a full turn (this tolerance was loosened further, to `1u32 << 21`, back when
                                                 // `trig::phase_from_radians_q16`'s own pi constant carried an additional, larger scale
                                                 // error that also fed into this same residual -- see `trig::PI_Q48`'s own doc comment;
                                                 // with that fixed, `delta_omega_q16`'s rounding is the only mechanism left, and the
                                                 // measured worst case has real headroom under the tolerance below without needing to be
                                                 // that loose). Not the exact-to-`1e-9` invariant the float sibling's own analogous test
                                                 // holds to.
            assert!(
                signed_diff.unsigned_abs() < (1u32 << 20),
                "phi_prev={phi_prev} phi_curr={phi_curr} l={l}: theta_n phase differs from phi_curr by {signed_diff} (of u32::MAX={})",
                u32::MAX
            );
        }
    }
}
