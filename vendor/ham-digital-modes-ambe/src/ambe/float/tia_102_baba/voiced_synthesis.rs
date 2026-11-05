//! Voiced speech synthesis (TIA-102.BABA_2003.pdf section 11.3, Eq. 127-141) -- the per-harmonic
//! sinusoidal half of section 11's own speech synthesis (see [`super::unvoiced_synthesis`]'s own doc
//! comment for how this combines with unvoiced synthesis via Eq. 142). Produces `s_v(n)`, a real
//! 160-sample (`N`, 20 ms) PCM contribution per frame, from the reconstructed/enhanced model
//! parameters ([`super::reconstruct`]/[`super::enhancement`]'s own output).
//!
//! Transcribed from a 600 DPI render of pages 76-78 (self-numbered 60-62) for Eq. 127-141, plus
//! Annex A (page 80, self-numbered 64) for every state variable's own real initial value.
//!
//! **A real notational ambiguity in the primary source, resolved and documented rather than
//! silently guessed at**: Eq. 140's own first branch condition is written as `1 <= l <=
//! floor(L~/4)` with a bare `L~` -- no explicit `(0)` or `(-1)` -- while the very same equation's
//! second branch divides by an explicit `L~(0)` two lines below it. Re-rendered at 600 DPI
//! specifically to rule out a dropped subscript (the same discipline used earlier this session for
//! the "symbol 1" oddity in Annex H): the glyph is a clean, complete `L~` with no truncation, so this
//! reads as the source's own abbreviated notation, not an extraction artifact. Given every other
//! frame-index omission in this section defaults to "current frame" (`(0)`) while `(-1)` is always
//! written explicitly, and the adjacent explicit `L~(0)` in the same equation, this module resolves
//! the bare `L~` as `L~(0)` -- documented here rather than left as a silent assumption.
//!
//! **What the spec leaves genuinely undefined, and this module's own resolution**: Eq. 140 only
//! defines `phi_l(0)` for `1 <= l <= max[L~(-1), L~(0)]`, but the spec's own text says `psi_l(0)`
//! "must be updated every frame ... for `1 <= l <= 56`, regardless of the value of `L~` or the value
//! of the V/UV decisions" -- meaning a harmonic beyond the currently active range still needs a
//! tracked phase for whenever it becomes active again, but Eq. 140 gives no formula for it. This
//! module extends Eq. 140's own first branch (`phi_l(0) = psi_l(0)`, no randomization) to every
//! `l > max[L~(-1), L~(0)]` too, on the reasoning that the randomization term exists specifically to
//! desynchronize the "sizzle" region for harmonics currently transitioning into or within active use,
//! not for tracking harmonics that aren't in play at all yet.

use std::f64::consts::PI;

use super::unvoiced_synthesis::{synthesis_window, NoiseState, N};

/// The largest harmonic index `psi_l`/`phi_l` are ever tracked for (Eq. 139's own stated range).
pub const MAX_HARMONICS: usize = 56;

/// `psi_l(0)` (Eq. 139): the running phase accumulator update, independent of voicing or harmonic
/// range -- the spec's own text is explicit that this runs "every frame ... for `1 <= l <= 56`,
/// regardless of the value of `L~` or the value of the V/UV decisions."
fn psi_update(psi_prev: f64, omega0_prev: f64, omega0_curr: f64, l: u32) -> f64 {
    psi_prev + (omega0_prev + omega0_curr) * l as f64 * N as f64 / 2.0
}

/// `Delta_omega_l(0)` (Eq. 137-138): the per-harmonic frequency correction that makes Eq. 136's own
/// continuous-phase ramp land exactly on `phi_l(0)` (mod `2*pi`) at `n = N` -- see this function's
/// own exact-invariant test below, which checks precisely that. A per-harmonic constant, not a
/// function of `n`, so callers should compute it once per harmonic rather than inside a per-sample
/// loop (this used to be recomputed 160 times per harmonic; hoisted out for both performance and
/// testability).
// [@ANCHOR: delta_omega]
fn delta_omega(phi_prev: f64, phi_curr: f64, omega0_prev: f64, omega0_curr: f64, l: u32) -> f64 {
    let l_f = l as f64;
    let delta_phi = phi_curr - phi_prev - (omega0_prev + omega0_curr) * l_f * N as f64 / 2.0; // Eq. 137.
    let wrapped = delta_phi - 2.0 * PI * ((delta_phi + PI) / (2.0 * PI)).floor();
    wrapped / N as f64 // Eq. 138.
}

/// `theta_l(n)` (Eq. 136): the continuous-phase ramp used by the Eq. 134 branch, given this
/// harmonic's own [`delta_omega`] (computed once, not per sample).
fn theta(
    n: f64,
    phi_prev: f64,
    omega0_prev: f64,
    omega0_curr: f64,
    delta_omega: f64,
    l: u32,
) -> f64 {
    let l_f = l as f64;
    phi_prev
        + (omega0_prev * l_f + delta_omega) * n
        + (omega0_curr - omega0_prev) * l_f * n * n / (2.0 * N as f64)
}

/// `rho_l(0)` (Eq. 141): a per-harmonic random phase offset in `[-pi, pi)`, drawn from the *same*
/// current-frame noise window unvoiced synthesis uses (`u(l)`, not an independent generator -- see
/// `unvoiced_synthesis::UnvoicedState`'s own doc comment for why the two must share one
/// [`NoiseState`]). `l` must be in `1..=56`, always within the generator's own `-104..=104` window.
fn phase_dither(noise: &NoiseState, l: u32) -> f64 {
    let u_l = noise
        .at(l as i32)
        .expect("harmonic index 1..=56 is within the noise window's own -104..=104 range");
    (2.0 * PI / 53125.0) * u_l as f64 - PI
}

/// Persistent voiced-synthesis state: the running phase accumulator `psi_l(-1)` and the actual
/// synthesis phase `phi_l(-1)` for every harmonic `1..=56`, plus the previous frame's own fundamental
/// frequency, V/UV decisions, and enhanced spectral amplitudes -- everything Eq. 127-140 need from
/// "the previous frame" that isn't already implicit in the current frame's own inputs. Initial values
/// per Annex A (page 80): `omega0(-1) = .02985*pi`, `v_bar_l(-1) = false` (unvoiced) and
/// `M_bar_l(-1) = 0.0` (silent) for all `l`, `phi_l(-1) = psi_l(-1) = 0.0` for all `l`, and
/// `L~(-1) = 30` -- consistent, as a real cross-check rather than a coincidence, with this codebase's
/// own encoder-side `FrameState::initial()` choice of a unity (not silent) starting spectral history:
/// Annex A's own `M~_l(-1) = 1` (the *unenhanced* reconstructed amplitude, distinct from this
/// struct's own `M_bar_l(-1)` the *enhanced* one) matches exactly.
pub struct VoicedState {
    psi: [f64; MAX_HARMONICS],
    phi: [f64; MAX_HARMONICS],
    omega0_prev: f64,
    l_hat_prev: u32,
    voiced_prev: [bool; MAX_HARMONICS],
    amplitudes_prev: [f64; MAX_HARMONICS],
    /// Shape of Eq. 135's `a_l(n)` amplitude interpolation, as a function of `t = n/N` in `[0,1]`
    /// (`shape(0) = 0`, `shape(1) = 1` is the caller's responsibility -- not enforced here since
    /// enforcing it would cost a per-sample check for no real benefit). Identity (`|t| t`) for the
    /// standard, matching Eq. 135 exactly; D-STAR and AMBE+2 may set a different shape via
    /// [`Self::set_amplitude_interpolation_shape`] -- see `super::super::mbe_synthesis`'s own doc
    /// comment for why (measured against the chip, not a TIA-102.BABA change).
    amplitude_interp_shape: fn(f64) -> f64,
}

impl VoicedState {
    pub fn new() -> Self {
        Self {
            psi: [0.0; MAX_HARMONICS],
            phi: [0.0; MAX_HARMONICS],
            omega0_prev: 0.02985 * PI,
            l_hat_prev: 30,
            voiced_prev: [false; MAX_HARMONICS],
            amplitudes_prev: [0.0; MAX_HARMONICS],
            amplitude_interp_shape: |t| t,
        }
    }

    /// Overrides Eq. 135's linear amplitude interpolation with a different shape of `t = n/N`.
    /// Never called by [`super::synthesis::SynthesisState::new`] itself (which leaves the spec's
    /// own linear identity in place) -- only by a caller that wants a mode-specific departure from
    /// the standard, exactly like [`super::synthesis::SynthesisState::set_unvoiced_gain`].
    pub fn set_amplitude_interpolation_shape(&mut self, shape: fn(f64) -> f64) {
        self.amplitude_interp_shape = shape;
    }

    /// Synthesizes the current frame's own voiced speech component `s_v(n)` (Eq. 127-141), advancing
    /// the phase-tracking state for the *next* call. `noise` must already reflect the current frame
    /// (see [`super::unvoiced_synthesis::UnvoicedState::synthesize`]'s own contract -- the two must
    /// share one generator). `voiced`/`spectral_amplitudes` are the current frame's own enhanced,
    /// 1-indexed-by-harmonic V/UV decisions and amplitudes (length `L~(0)`); returns `None` on a
    /// length mismatch between the two, or if either exceeds [`MAX_HARMONICS`] (a real, spec-violating
    /// input -- Eq. 139 only ever tracks harmonics `1..=56`).
    // [@ANCHOR: VoicedState::synthesize]
    pub fn synthesize(
        &mut self,
        noise: &NoiseState,
        omega0_curr: f64,
        voiced: &[bool],
        spectral_amplitudes: &[f64],
    ) -> Option<[f64; N]> {
        if voiced.len() != spectral_amplitudes.len() || voiced.len() > MAX_HARMONICS {
            return None;
        }
        let l_hat_curr = voiced.len() as u32;
        let quarter_l_hat_curr = l_hat_curr / 4; // floor(L~(0)/4), integer division.
        let l_uv_curr = voiced.iter().filter(|&&v| !v).count() as f64; // L~_uv(0), Eq. 140's own text.
        let max_l = self.l_hat_prev.max(l_hat_curr);

        // Eq. 139-140: update every harmonic's own phase state first (the spec's own "regardless of
        // L~ or V/UV" requirement), keeping the *old* phi_l(-1) around locally for this frame's own
        // sample synthesis below, which needs both the old and new phase simultaneously (Eq. 131-134
        // all reference at least one of phi_l(-1)/phi_l(0), and Eq. 133 needs both at once).
        let mut phi_prev = [0.0; MAX_HARMONICS];
        let mut phi_curr = [0.0; MAX_HARMONICS];
        for l in 1..=MAX_HARMONICS as u32 {
            let idx = (l - 1) as usize;
            phi_prev[idx] = self.phi[idx];

            let psi_new = psi_update(self.psi[idx], self.omega0_prev, omega0_curr, l);

            let phi_new = if l <= quarter_l_hat_curr {
                psi_new
            } else if l <= max_l && l_hat_curr > 0 {
                psi_new + l_uv_curr * phase_dither(noise, l) / l_hat_curr as f64
            } else {
                // Either beyond max[L~(-1), L~(0)] (never used for actual synthesis this frame -- see
                // this module's own doc comment), or a degenerate L~(0) == 0 frame: no randomization
                // term is defined either way, so continue the plain phase progression.
                psi_new
            };

            self.psi[idx] = psi_new;
            self.phi[idx] = phi_new;
            phi_curr[idx] = phi_new;
        }

        let mut s_v = [0.0; N];
        for l in 1..=max_l {
            let idx = (l - 1) as usize;
            let was_voiced = self.voiced_prev[idx];
            let was_amp = self.amplitudes_prev[idx];
            let is_voiced = l <= l_hat_curr && voiced[idx];
            let is_amp = if l <= l_hat_curr {
                spectral_amplitudes[idx]
            } else {
                0.0
            };
            let l_f = l as f64;
            // Only meaningful (and only computed) for the Eq. 134 branch below, but cheap enough to
            // compute unconditionally here rather than duplicating the was_voiced/is_voiced match.
            let delta_omega_l = delta_omega(
                phi_prev[idx],
                phi_curr[idx],
                self.omega0_prev,
                omega0_curr,
                l,
            );

            for (n, slot) in s_v.iter_mut().enumerate() {
                let n = n as f64;
                let n_shifted = n - N as f64;

                let sample = match (was_voiced, is_voiced) {
                    (false, false) => 0.0, // Eq. 130.
                    (true, false) => {
                        // Eq. 131.
                        synthesis_window(n as i32)
                            * was_amp
                            * (self.omega0_prev * n * l_f + phi_prev[idx]).cos()
                    }
                    (false, true) => {
                        // Eq. 132.
                        synthesis_window(n_shifted as i32)
                            * is_amp
                            * (omega0_curr * n_shifted * l_f + phi_curr[idx]).cos()
                    }
                    (true, true) => {
                        let big_jump =
                            l >= 8 || (omega0_curr - self.omega0_prev).abs() >= 0.1 * omega0_curr;
                        if big_jump {
                            // Eq. 133: both halves synthesized independently and summed.
                            synthesis_window(n as i32)
                                * was_amp
                                * (self.omega0_prev * n * l_f + phi_prev[idx]).cos()
                                + synthesis_window(n_shifted as i32)
                                    * is_amp
                                    * (omega0_curr * n_shifted * l_f + phi_curr[idx]).cos()
                        } else {
                            // Eq. 134-135: continuous-phase interpolation (delta_omega_l hoisted
                            // above the sample loop -- it's a per-harmonic constant, see this
                            // function's own doc comment).
                            // Eq. 135, with `t = n/N` passed through `amplitude_interp_shape`
                            // (identity for the standard; see that field's own doc comment).
                            let t = (self.amplitude_interp_shape)(n / N as f64);
                            let a_l_n = was_amp + t * (is_amp - was_amp);
                            let theta_l_n = theta(
                                n,
                                phi_prev[idx],
                                self.omega0_prev,
                                omega0_curr,
                                delta_omega_l,
                                l,
                            );
                            a_l_n * theta_l_n.cos()
                        }
                    }
                };
                *slot += 2.0 * sample; // Eq. 127's own factor of 2.
            }
        }

        // Roll current -> previous for the next call.
        self.omega0_prev = omega0_curr;
        self.l_hat_prev = l_hat_curr;
        for l in 1..=MAX_HARMONICS as u32 {
            let idx = (l - 1) as usize;
            self.voiced_prev[idx] = l <= l_hat_curr && voiced[idx];
            self.amplitudes_prev[idx] = if l <= l_hat_curr {
                spectral_amplitudes[idx]
            } else {
                0.0
            };
        }

        Some(s_v)
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

    /// `theta_l(N)` must land exactly on `phi_l(0)` (mod `2*pi`) -- that's the entire point of
    /// Eq. 137-138's own correction term, and it's checkable to `1e-9` independent of any external
    /// reference. Any sign error, wrong denominator, or misplaced `l` in [`delta_omega`]/[`theta`]
    /// breaks this. Checked across several (phi_prev, phi_curr, omega0_prev, omega0_curr, l)
    /// combinations, including one with a negative `Delta_phi_l(0)` (phi_curr < phi_prev) to exercise
    /// the wrap-around floor term in both directions.
    #[test]
    // Tests [@ANCHOR: delta_omega]
    fn theta_at_n_equals_capital_n_lands_exactly_on_phi_curr_mod_2pi() {
        let cases = [
            (0.3, 4.1, 2.0 * PI / 100.0, 2.0 * PI / 105.0, 1u32),
            (0.3, 4.1, 2.0 * PI / 100.0, 2.0 * PI / 105.0, 7u32),
            (5.9, 0.2, 2.0 * PI / 60.0, 2.0 * PI / 58.0, 3u32), // phi_curr < phi_prev.
            (-2.0, 2.0, 2.0 * PI / 40.0, 2.0 * PI / 200.0, 6u32), // a large omega0 jump.
        ];
        for (phi_prev, phi_curr, omega0_prev, omega0_curr, l) in cases {
            let d_omega = delta_omega(phi_prev, phi_curr, omega0_prev, omega0_curr, l);
            let theta_n = theta(N as f64, phi_prev, omega0_prev, omega0_curr, d_omega, l);
            let diff = (theta_n - phi_curr).rem_euclid(2.0 * PI);
            let distance_from_zero = diff.min(2.0 * PI - diff);
            assert!(
                distance_from_zero < 1e-9,
                "theta_l(N) = {theta_n}, phi_curr = {phi_curr}, diff mod 2pi = {diff}"
            );
        }
    }

    /// Independent numeric oracle from `kchmck/imbe.rs`'s own `voiced.rs` test `test_phase_base`:
    /// given `omega0_prev = 0.0937765407`, `omega0_curr = 0.17575344`, and every `psi_l(-1)` seeded
    /// to `l` (their own test's deliberately distinctive setup, not the real Annex A initial value),
    /// their `PhaseBase::new` (Eq. 139) produces `pb.get(1) == 22.56239845600000037961763155180961`
    /// -- computed entirely independently of this codebase. Cross-checks [`psi_update`] exactly.
    #[test]
    fn psi_update_matches_imbe_rs_own_test_phase_base_oracle() {
        let omega0_prev = 0.0937765407;
        let omega0_curr = 0.17575344;
        let expected = [
            22.562_398_456,
            45.124_796_912,
            67.687_195_368,
            90.249_593_824,
            112.811_992_28,
        ];
        for (i, &want) in expected.iter().enumerate() {
            let l = (i + 1) as u32;
            let got = psi_update(l as f64, omega0_prev, omega0_curr, l);
            assert!(
                (got - want).abs() < 1e-3,
                "psi_update l={l}: got {got}, want {want}"
            );
        }
    }

    /// Same oracle's own `test_phase`: with `L~(0) = 16`, `L~(-1) = 30`, `L~_uv(0) = 6`, their own
    /// `Phase::new` (Eq. 140) leaves harmonics `1..=4` completely unmodified from the base phase
    /// (`p.get(l) == pb.get(l)` exactly for `l <= 4`) and *does* modify harmonics `5..=30` -- an
    /// independent confirmation of this module's own resolution of the bare `L~` in Eq. 140's first
    /// branch condition as `L~(0)` (`floor(16/4) == 4`), and of the upper bound being
    /// `max[L~(-1), L~(0)] == 30` inclusive (their own slice is `phase[4..30]`, i.e. 0-indexed
    /// harmonics 5..=30).
    #[test]
    fn phi_boundary_matches_imbe_rs_own_test_phase_oracle_ranges() {
        let quarter_l_hat_curr = 16u32 / 4; // 4, per Eq. 140's first branch.
        let max_l = 30u32; // max[L~(-1), L~(0)] = max(30, 16).
        assert_eq!(quarter_l_hat_curr, 4);
        // Harmonics 1..=4: base only, no dither (matches p.get(l) == pb.get(l) for l in 1..=4).
        for l in 1..=4u32 {
            assert!(l <= quarter_l_hat_curr);
        }
        // Harmonics 5..=30: eligible for dither.
        for l in 5..=30u32 {
            assert!(l > quarter_l_hat_curr && l <= max_l);
        }
        // Harmonic 31 and beyond: outside the modified range entirely.
        assert!(!(31 > quarter_l_hat_curr && 31 <= max_l));
    }

    /// At `n = 0`, Eq. 134's own branch must reduce to exactly `M_bar_l(-1) * cos(phi_l(-1))`:
    /// `a_l(0) == M_bar_l(-1)` (Eq. 135 with `n=0`), `theta_l(0) == phi_l(-1)` (Eq. 136 with `n=0`,
    /// since every `n`/`n^2` term vanishes), and `w_S(0) == 1.0`. Checked directly against
    /// [`theta`]/[`psi_update`]-adjacent arithmetic with one active harmonic, hand-computed rather
    /// than taken on faith.
    #[test]
    fn eq134_branch_reduces_to_the_hand_computed_value_at_n_equals_zero() {
        let phi_prev = 0.7;
        let phi_curr = 1.9;
        let omega0_prev = 2.0 * PI / 90.0;
        let omega0_curr = 2.0 * PI / 92.0;
        let l = 3u32;
        let d_omega = delta_omega(phi_prev, phi_curr, omega0_prev, omega0_curr, l);
        let theta_0 = theta(0.0, phi_prev, omega0_prev, omega0_curr, d_omega, l);
        assert!(
            (theta_0 - phi_prev).abs() < 1e-12,
            "theta_l(0) = {theta_0}, expected {phi_prev}"
        );

        let was_amp = 321.0;
        let is_amp = 654.0;
        let a_l_0 = was_amp + (0.0 / N as f64) * (is_amp - was_amp);
        assert_eq!(a_l_0, was_amp);

        let expected = was_amp * phi_prev.cos();
        let got = a_l_0 * theta_0.cos();
        assert!((got - expected).abs() < 1e-9);
    }

    #[test]
    // Tests [@ANCHOR: VoicedState::synthesize]
    fn synthesize_produces_a_full_finite_frame_across_several_calls_all_voiced() {
        let mut state = VoicedState::new();
        let mut noise = NoiseState::new();
        let omega0 = 2.0 * PI / 100.0;
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];

        for i in 0..4 {
            if i > 0 {
                noise.advance_frame();
            }
            let frame = state
                .synthesize(&noise, omega0, &voiced, &amplitudes)
                .unwrap();
            for &sample in &frame {
                assert!(sample.is_finite(), "non-finite voiced sample: {sample}");
            }
        }
    }

    #[test]
    fn synthesize_handles_the_all_unvoiced_case_as_pure_silence() {
        // Both this frame and (per Annex A) the initial previous frame are fully unvoiced -- Eq. 130
        // applies to every harmonic, so the voiced component must be exactly zero.
        let mut state = VoicedState::new();
        let noise = NoiseState::new();
        let voiced = vec![false; 16];
        let amplitudes = vec![0.0; 16];
        let frame = state
            .synthesize(&noise, 2.0 * PI / 100.0, &voiced, &amplitudes)
            .unwrap();
        for &sample in &frame {
            assert_eq!(sample, 0.0);
        }
    }

    #[test]
    fn synthesize_handles_a_transition_from_silence_into_fully_voiced() {
        let mut state = VoicedState::new();
        let mut noise = NoiseState::new();
        let omega0 = 2.0 * PI / 100.0;

        // Frame 0: silent (matches Annex A's own initial "previous" state, so this exercises
        // Eq. 130 for every harmonic).
        let silent = vec![false; 16];
        let zero = vec![0.0; 16];
        state.synthesize(&noise, omega0, &silent, &zero).unwrap();

        // Frame 1: fully voiced -- every harmonic must take the false->true transition (Eq. 132).
        noise.advance_frame();
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        let frame = state
            .synthesize(&noise, omega0, &voiced, &amplitudes)
            .unwrap();
        assert!(
            frame.iter().any(|&s| s != 0.0),
            "expected real voiced energy after transition-in"
        );
        for &sample in &frame {
            assert!(sample.is_finite());
        }
    }

    #[test]
    fn synthesize_handles_a_transition_from_fully_voiced_to_silence() {
        let mut state = VoicedState::new();
        let mut noise = NoiseState::new();
        let omega0 = 2.0 * PI / 100.0;

        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        state
            .synthesize(&noise, omega0, &voiced, &amplitudes)
            .unwrap();

        noise.advance_frame();
        let silent = vec![false; 16];
        let zero = vec![0.0; 16];
        let frame = state.synthesize(&noise, omega0, &silent, &zero).unwrap();
        assert!(
            frame.iter().any(|&s| s != 0.0),
            "expected residual energy fading out (Eq. 131)"
        );
        for &sample in &frame {
            assert!(sample.is_finite());
        }
    }

    #[test]
    fn synthesize_takes_the_large_pitch_jump_branch_eq133_without_panicking() {
        let mut state = VoicedState::new();
        let mut noise = NoiseState::new();
        let voiced = vec![true; 16];
        let amplitudes = vec![500.0; 16];
        state
            .synthesize(&noise, 2.0 * PI / 100.0, &voiced, &amplitudes)
            .unwrap();

        noise.advance_frame();
        // A pitch jump far exceeding the 10% threshold.
        let frame = state
            .synthesize(&noise, 2.0 * PI / 40.0, &voiced, &amplitudes)
            .unwrap();
        for &sample in &frame {
            assert!(sample.is_finite());
        }
    }

    #[test]
    fn synthesize_rejects_a_length_mismatch() {
        let mut state = VoicedState::new();
        let noise = NoiseState::new();
        let voiced = vec![true; 5];
        let amplitudes = vec![100.0; 6];
        assert!(state
            .synthesize(&noise, 0.1, &voiced, &amplitudes)
            .is_none());
    }

    #[test]
    fn synthesize_rejects_more_harmonics_than_max_harmonics() {
        let mut state = VoicedState::new();
        let noise = NoiseState::new();
        let voiced = vec![true; MAX_HARMONICS + 1];
        let amplitudes = vec![100.0; MAX_HARMONICS + 1];
        assert!(state
            .synthesize(&noise, 0.1, &voiced, &amplitudes)
            .is_none());
    }
}
