//! Unvoiced speech synthesis (TIA-102.BABA_2003.pdf section 11.2, Eq. 117-126) -- the noise-based
//! half of section 11's own speech synthesis (see [`super::mod`]'s own Fig. 26 doc comment for how
//! this combines with voiced synthesis). Produces `s_uv(n)`, a real 160-sample (`N`, 20 ms) PCM
//! contribution per frame, from the reconstructed/enhanced model parameters
//! ([`super::reconstruct`]/[`super::enhancement`]'s own output).
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 74-75 (self-numbered 57-59) for
//! Eq. 117-126, plus Annex I (pages 111-112, self-numbered 95-96) for the synthesis window `w_S(n)`.
//!
//! **Independent cross-check against `kchmck/imbe.rs`'s own `unvoiced.rs`, with one deliberate,
//! documented deviation**: that crate does NOT implement Eq. 117's own deterministic noise recurrence
//! at all -- its own doc comment explains it substitutes a statistically-equivalent Gaussian random
//! generator instead, for real-time performance (avoiding an O(n) DFT of a literal noise sequence).
//! That is a legitimate engineering trade-off for a real-time decoder (the spec itself only says the
//! noise "can have an arbitrary mean," not that decoders must reproduce Eq. 117 bit-for-bit), but it
//! means this module's own choice to implement Eq. 117 literally is *not* redundant with anything
//! that crate already verified -- it remains its own from-scratch transcription. Everything else
//! (Eq. 120's own scaling, Eq. 121's own `gamma_w` numeric value, Eq. 122-123's own band edges, and
//! Eq. 126's own overlap-add) matches that crate's `edges`/`SCALING_COEF`/`Unvoiced::get` exactly.

use std::collections::VecDeque;
use std::f64::consts::PI;

use super::pitch::pitch_refinement_window;

/// `N` (section 11.1): the number of PCM samples in one synthesis frame, 20 ms at 8 kHz.
pub const N: usize = 160;

#[derive(Clone, Copy)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    const ZERO: Complex = Complex { re: 0.0, im: 0.0 };

    fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.re + other.re, self.im + other.im)
    }

    fn scale(self, s: f64) -> Self {
        Self::new(self.re * s, self.im * s)
    }

    fn mul(self, other: Self) -> Self {
        Self::new(
            self.re * other.re - self.im * other.im,
            self.re * other.im + self.im * other.re,
        )
    }

    fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
}

/// `w_S(n)` (Annex I): the speech synthesis window. Unlike `pitch::pitch_refinement_window`/
/// `initial_pitch_window` (Annexes C/B), this window has a real closed form: every one of Annex I's
/// own 211 tabulated values (`n` in `-105..=105`) was checked by hand against
/// `clamp(0.02*(105-|n|), 0.0, 1.0)` during transcription and matches exactly (a symmetric trapezoid,
/// flat at `1.0` for `|n| <= 55`, ramping linearly to `0.0` by `|n| == 105`), so a formula is used
/// here instead of a lookup table. Returns `0.0` outside `-105..=105`, the spec's own stated
/// convention for window functions.
// [@ANCHOR: synthesis_window]
pub fn synthesis_window(n: i32) -> f64 {
    let abs_n = n.unsigned_abs() as i64;
    if abs_n > 105 {
        0.0
    } else {
        (0.02 * (105 - abs_n) as f64).clamp(0.0, 1.0)
    }
}

/// Advances the noise recurrence (Eq. 117) by one step. The spec's own `171*u(n) + 11213 -
/// 53125*floor((171*u(n)+11213)/53125)` is exactly Euclidean remainder for a positive modulus
/// (`a - b*floor(a/b) == a.rem_euclid(b)` whenever `b > 0`, a real algebraic identity, not an
/// approximation), so `i64::rem_euclid` is used directly rather than transcribing the floor/subtract
/// form literally. `pub(crate)`, not private: `synthesis.rs`'s own section 7.8 comfort-noise
/// generator reuses this exact recurrence, seeded independently of [`NoiseState`]'s own shared
/// window so muting a frame never perturbs the noise sequence real unvoiced/voiced synthesis
/// depends on.
pub(crate) fn advance_noise(u: i64) -> i64 {
    (171 * u + 11213).rem_euclid(53125)
}

/// Persistent state for the noise generator `u(n)` (Eq. 117), seeded at `u(-105) = 3147`: the real
/// state a decoder must carry frame to frame, since the recurrence can only be advanced one integer
/// step at a time (it has no closed form in `n`, unlike [`synthesis_window`]). Keeps a rolling window
/// of the most recent 209 values -- `u(n)` for `n` in the current frame's own `-104..=104` -- built by
/// [`Self::new`] for the first frame and shifted forward by [`Self::advance_frame`] (160 new samples,
/// "shifted by 20 ms.") for every frame after that.
pub struct NoiseState {
    last: i64,
    window: VecDeque<i64>,
}

impl NoiseState {
    // [@ANCHOR: NoiseState::new]
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

    // [@ANCHOR: NoiseState::step]
    fn step(&mut self) {
        self.last = advance_noise(self.last);
        if self.window.len() == 209 {
            self.window.pop_front();
        }
        self.window.push_back(self.last);
    }

    // [@ANCHOR: NoiseState::advance_frame]
    pub fn advance_frame(&mut self) {
        for _ in 0..N {
            self.step();
        }
    }

    /// `u(relative)` for `relative` in `-104..=104`, relative to the current frame's own `n = 0`.
    /// Returns `None` outside that range -- a real, checkable contract (used both by
    /// [`unvoiced_dft`] and by Eq. 141's own `rho_l(0)`, which indexes this same current-frame window
    /// by harmonic number) rather than a panic, since a caller mis-deriving an index is a real risk
    /// worth catching explicitly.
    // [@ANCHOR: NoiseState::at]
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

// [@ANCHOR: unvoiced_dft]
/// `exp(2*pi*j*k/256)` for `k` in `0..256`, computed once: the two 256-point transforms below reduce every product `m*n` to
/// this table instead of evaluating a sine and cosine per term.
fn twiddles() -> &'static [(f64, f64); 256] {
    static T: std::sync::OnceLock<[(f64, f64); 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        std::array::from_fn(|k| {
            let theta = 2.0 * PI * k as f64 / 256.0;
            (theta.cos(), theta.sin())
        })
    })
}

/// `U_w(m)` (Eq. 118): the 256-point DFT of the current frame's own windowed noise sequence
/// `u(n)*w_S(n)`, for `m` in `-128..=127`. Note this range is offset by one bin from
/// `pitch_refinement::RefinementFrame`'s own `S_w(m)` (`-127..=128`) -- a real difference between the
/// spec's own Eq. 29 and Eq. 118, not a copy-paste slip (each checked independently at 600 DPI).
fn unvoiced_dft(noise: &NoiseState) -> [Complex; 256] {
    let mut uw = [Complex::ZERO; 256];
    for (i, slot) in uw.iter_mut().enumerate() {
        let m = i as i32 - 128;
        let mut acc = Complex::ZERO;
        for n in -104i32..=104 {
            let sample =
                noise.at(n).expect("noise window covers -104..=104") as f64 * synthesis_window(n);
            let (c, s) = twiddles()[(m * n).rem_euclid(256) as usize];
            // exp(-j*2*pi*m*n/256) = conj of the table entry
            acc = acc.add(Complex::new(sample * c, -sample * s));
        }
        *slot = acc;
    }
    uw
}

fn bin_index(m: i32) -> usize {
    (m + 128) as usize
}

/// `a~_l` (Eq. 122): the l'th harmonic band's own lower frequency-bin edge (before the ceiling in
/// Eq. 119/120/124 is applied), from the reconstructed fundamental `omega0_tilde`.
fn band_edge_a(l: u32, omega0_tilde: f64) -> f64 {
    (256.0 / (2.0 * PI)) * (l as f64 - 0.5) * omega0_tilde
}

/// `b~_l` (Eq. 123): the l'th harmonic band's own upper frequency-bin edge.
fn band_edge_b(l: u32, omega0_tilde: f64) -> f64 {
    (256.0 / (2.0 * PI)) * (l as f64 + 0.5) * omega0_tilde
}

/// `gamma_w` (Eq. 121): the fixed unvoiced scaling coefficient relating the pitch-refinement window
/// `w_R(n)` (`pitch::pitch_refinement_window`, Annex C) to this module's own synthesis window
/// `w_S(n)` (Annex I) -- a constant of the two window definitions alone, not per-frame data, matching
/// `kchmck/imbe.rs`'s own hardcoded `SCALING_COEF` (`146.6432708443356`) to full `f64` precision (see
/// the test below), a strong independent confirmation of both this formula and the two window
/// transcriptions it depends on.
// [@ANCHOR: unvoiced_scaling_coefficient]
pub fn unvoiced_scaling_coefficient() -> f64 {
    static VALUE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *VALUE.get_or_init(unvoiced_scaling_coefficient_uncached)
}

fn unvoiced_scaling_coefficient_uncached() -> f64 {
    let sum_w_r: f64 = (-110..=110).map(pitch_refinement_window).sum();
    let sum_w_s_sq: f64 = (-104..=104).map(|n| synthesis_window(n).powi(2)).sum();
    let sum_w_r_sq: f64 = (-110..=110)
        .map(|n| pitch_refinement_window(n).powi(2))
        .sum();
    sum_w_r * (sum_w_s_sq / sum_w_r_sq).sqrt()
}

/// `U~_w(m)` (Eq. 119-120 and 124 combined): starts every bin at zero (satisfying both Eq. 119's own
/// "voiced harmonics stay zero" rule and Eq. 124's own "outside every harmonic band, zero" rule at
/// once, since a harmonic's own unvoiced band is the only case that ever writes a nonzero value), then
/// fills in only the bands belonging to an *unvoiced* harmonic (Eq. 120) with `U_w(m)` rescaled to
/// match that harmonic's own enhanced spectral amplitude. `voiced`/`spectral_amplitudes` are both
/// 1-indexed by harmonic (`voiced[0]` is harmonic 1, i.e. `v_bar_1`/`M_bar_1(0)`) and must be the same
/// length; returns `None` on a length mismatch.
///
/// **History: this was first investigated and proven unreachable for TIA-102.BABA's own decode path (below), then
/// became reachable once D-STAR/AMBE+2 started feeding this same shared synthesis: their harmonic count `L` comes
/// from a table indexed by `b0`, not from `harmonics_count(omega0)`, and the chip's D-STAR pitch is ~3% above the
/// formula the tables were paired with, so `(L+0.5)*omega0` can exceed pi. The loop below therefore clamps the
/// band to the valid bins (`unvoiced_spectrum_clamps_bands_past_nyquist`). TIA-102.BABA's original proof follows.** `bin_index` panics if `band_edge_b(l_hat,
/// omega0_tilde).ceil()` reaches `128` for the top harmonic `l_hat`, i.e. whenever
/// `(l_hat + 0.5) * omega0_tilde > pi`. This *is* reachable from a hand-constructed, physically
/// invalid `(l_hat, omega0_tilde)` pair (the mistake a synthetic test made while building the
/// fixed-point port's own orchestration tests, `l_hat=40` with `omega0_tilde` from `b0=90` -- see
/// `night_shift_todo/high/unvoiced-spectrum-panic-on-invalid-l-hat-omega0-pair-a1c9e3f7.md` in
/// hams_com for that history), but every real caller in this codebase gets `l_hat` from `voiced.len()`
/// alone (this function never receives `l_hat` as its own separate argument), and every real
/// production caller builds `voiced` with exactly `vuv::harmonics_count(omega0_tilde)` elements for
/// the *same* `omega0_tilde` passed in -- `decode.rs`'s own `decode_parameters` (`let l_hat =
/// harmonics_count(omega0_tilde);`, right after dequantizing the same `omega0_tilde`), and
/// `SynthesisState::synthesize_repeated_frame`, which reuses a `(omega0_tilde, voiced, amplitudes)`
/// tuple saved together from one real prior frame, never mixing a `voiced` from one frame with an
/// `omega0_tilde` from another. `l_hat` is *never* decoded from its own independent bitfield.
///
/// That leaves exactly one question: does `harmonics_count`'s own formula ever produce an `l_hat` that
/// violates the constraint for some `omega0_tilde` this decoder could actually derive? `b0`'s raw
/// bitfield (`extract_fundamental_frequency_quantizer`, 6+2 bits) spans `0..=255` even under arbitrary
/// bit corruption, but `decode.rs`'s own `MAX_VALID_B0` check (`b0 > 207` forces a repeat) rejects
/// every value above `207` *before* `dequantize_fundamental_frequency`/`harmonics_count` are ever
/// called -- so `0..=207` is the complete real domain, corrupted bitstreams included. Checked directly
/// for all 208 values (`unvoiced_spectrum_band_edge_stays_in_range_for_every_valid_b0` below): the
/// worst-case margin is `pi - (l_hat + 0.5) * omega0_tilde ~= 0.0915` rad at `b0=12`, comfortably
/// positive, not a knife-edge that floating-point rounding could flip. This is a real, checked
/// property (not merely an unexamined assumption), encoded as a permanent regression test rather than
/// left as prose alone.
// [@ANCHOR: unvoiced_spectrum]
fn unvoiced_spectrum(
    noise: &NoiseState,
    omega0_tilde: f64,
    voiced: &[bool],
    spectral_amplitudes: &[f64],
    gamma_w: f64,
) -> Option<[Complex; 256]> {
    if voiced.len() != spectral_amplitudes.len() {
        return None;
    }
    let uw = unvoiced_dft(noise);
    let mut result = [Complex::ZERO; 256];
    let l_hat = voiced.len() as u32;

    for l in 1..=l_hat {
        if voiced[(l - 1) as usize] {
            continue; // Eq. 119: stays zero.
        }
        // Clamp to the 256-point DFT's valid bins (`bin_index` covers -128..=127): a harmonic whose band
        // reaches past Nyquist contributes only its in-range part instead of panicking.
        let b = (band_edge_b(l, omega0_tilde).ceil() as i32).min(128);
        let a = (band_edge_a(l, omega0_tilde).ceil() as i32).min(b);
        if b <= a {
            continue; // Degenerate (unreachable for any real pitch period) zero-width band.
        }

        let power: f64 =
            (a..b).map(|eta| uw[bin_index(eta)].norm_sqr()).sum::<f64>() / (b - a) as f64;
        let scale = gamma_w * spectral_amplitudes[(l - 1) as usize] / power.sqrt();

        for m in a..b {
            for sign_m in [m, -m] {
                result[bin_index(sign_m)] = uw[bin_index(sign_m)].scale(scale);
            }
        }
    }
    Some(result)
}

/// `u~_w(n)` (Eq. 125): the 256-point inverse DFT of `U~_w(m)`, for `n` in `-128..=127`. The result is
/// mathematically guaranteed real (a real, checkable consequence of `U~_w`'s own construction:
/// `U_w(-m) == conj(U_w(m))` since it's the DFT of a real signal, and every band edit above scales a
/// `+m`/`-m` pair by the same real scalar, which preserves that conjugate symmetry) -- verified
/// against that guarantee in the test below (asserting the discarded imaginary part is negligible)
/// rather than merely assumed.
// [@ANCHOR: unvoiced_time_domain]
fn unvoiced_time_domain(spectrum: &[Complex; 256]) -> [f64; 256] {
    let mut out = [0.0; 256];
    for (i, slot) in out.iter_mut().enumerate() {
        let n = i as i32 - 128;
        let mut acc = Complex::ZERO;
        for (j, &bin) in spectrum.iter().enumerate() {
            let m = j as i32 - 128;
            let (c, s) = twiddles()[(m * n).rem_euclid(256) as usize];
            acc = acc.add(bin.mul(Complex::new(c, s)));
        }
        *slot = acc.scale(1.0 / 256.0).re;
    }
    out
}

// [@ANCHOR: time_domain_at]
fn time_domain_at(samples: &[f64; 256], n: i32) -> f64 {
    if (-128..=127).contains(&n) {
        samples[(n + 128) as usize]
    } else {
        0.0 // Assumed zero outside -128..=127, per the spec's own stated convention.
    }
}

/// Persistent unvoiced-synthesis state: just the previous frame's own time-domain unvoiced signal
/// (`u~_w(n, -1)`), needed by [`Self::synthesize`]'s own Eq. 126 overlap-add. The noise generator
/// itself ([`NoiseState`]) is owned by the caller, not by this struct: Eq. 141's own `rho_l(0)` (used
/// by voiced synthesis, `super::voiced_synthesis`) reads `u(l)` from this *same* current-frame noise
/// window ("the shifted noise sequence for the current frame, described in Section 11.2" -- the
/// spec's own words), not an independent one, so a single [`NoiseState`] must be shared between both
/// halves of synthesis rather than each owning its own.
pub struct UnvoicedState {
    previous_time_domain: [f64; 256],
}

impl UnvoicedState {
    pub fn new() -> Self {
        Self {
            previous_time_domain: [0.0; 256], // Eq. 126's own "zero outside the defined range" [p64].
        }
    }

    /// Synthesizes the current frame's own unvoiced speech component `s_uv(n)` (Eq. 117-126),
    /// advancing the overlap-add history for the *next* call. `noise` must already reflect the
    /// current frame (i.e. the caller has already called [`NoiseState::advance_frame`] for every
    /// frame after the first). `voiced` and `spectral_amplitudes` are the current frame's own
    /// enhanced, 1-indexed-by-harmonic V/UV decisions and amplitudes; returns `None` on a length
    /// mismatch between the two.
    // [@ANCHOR: UnvoicedState::synthesize]
    pub fn synthesize(
        &mut self,
        noise: &NoiseState,
        omega0_tilde: f64,
        voiced: &[bool],
        spectral_amplitudes: &[f64],
    ) -> Option<[f64; N]> {
        let gamma_w = unvoiced_scaling_coefficient();
        let spectrum =
            unvoiced_spectrum(noise, omega0_tilde, voiced, spectral_amplitudes, gamma_w)?;
        let current_time_domain = unvoiced_time_domain(&spectrum);

        let mut s_uv = [0.0; N];
        for (n, slot) in s_uv.iter_mut().enumerate() {
            let n = n as i32;
            let w_n = synthesis_window(n);
            let w_shifted = synthesis_window(n - N as i32);
            let numerator = w_n * time_domain_at(&self.previous_time_domain, n)
                + w_shifted * time_domain_at(&current_time_domain, n - N as i32);
            let denominator = w_n * w_n + w_shifted * w_shifted;
            *slot = numerator / denominator;
        }

        self.previous_time_domain = current_time_domain;
        Some(s_uv)
    }
}

impl Default for UnvoicedState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Tests [@ANCHOR: synthesis_window]
    fn synthesis_window_matches_the_transcribed_annex_i_table_at_every_spot_checked_value() {
        // Spot-checked directly against the 600 DPI render during transcription (both the up-ramp,
        // the flat plateau, and the down-ramp to zero, plus both symmetric halves).
        let expected = [
            (-105, 0.0),
            (-104, 0.02),
            (-100, 0.10),
            (-74, 0.62),
            (-55, 1.0),
            (-1, 1.0),
            (0, 1.0),
            (55, 1.0),
            (56, 0.98),
            (74, 0.62),
            (100, 0.10),
            (104, 0.02),
            (105, 0.0),
        ];
        for (n, want) in expected {
            let got = synthesis_window(n);
            assert!(
                (got - want).abs() < 1e-9,
                "w_S({n}) = {got}, expected {want}"
            );
        }
        assert_eq!(synthesis_window(106), 0.0);
        assert_eq!(synthesis_window(-106), 0.0);
        assert_eq!(synthesis_window(1000), 0.0);
    }

    #[test]
    fn synthesis_window_is_symmetric() {
        for n in 0..=120 {
            assert_eq!(synthesis_window(n), synthesis_window(-n));
        }
    }

    #[test]
    fn advance_noise_matches_a_hand_computed_sequence_from_the_real_seed() {
        // Independently computed in Python from u(-105) = 3147 via Eq. 117's own literal formula
        // (not rem_euclid), to check the rem_euclid simplification against the spec's own literal
        // floor/subtract form, not just against itself.
        let expected = [
            18100, 25063, 46986, 23944, 15012, 28265, 10153, 47376, 37509, 50252,
        ];
        let mut u = 3147i64;
        for &want in &expected {
            u = advance_noise(u);
            assert_eq!(u, want);
        }
    }

    #[test]
    // Tests [@ANCHOR: NoiseState::new]
    // Tests [@ANCHOR: NoiseState::step]
    // Tests [@ANCHOR: NoiseState::at]
    fn noise_state_new_fills_the_first_frames_window_starting_from_the_real_seed() {
        let state = NoiseState::new();
        // u(-104) is the first value generated from the seed u(-105) = 3147.
        assert_eq!(state.at(-104), Some(18100));
        assert_eq!(state.at(-103), Some(25063));
        assert_eq!(state.at(105), None);
        assert_eq!(state.at(-105), None);
    }

    #[test]
    // Tests [@ANCHOR: unvoiced_scaling_coefficient]
    fn unvoiced_scaling_coefficient_matches_imbe_rs_own_hardcoded_constant() {
        // kchmck/imbe.rs's own unvoiced.rs hardcodes SCALING_COEF = 146.6432708443356, computed from
        // the same two window definitions (Annex C's w_R, Annex I's w_S) via Eq. 121.
        let gamma_w = unvoiced_scaling_coefficient();
        assert!(
            (gamma_w - 146.643_270_844_335_6).abs() < 1e-6,
            "gamma_w = {gamma_w}, expected ~146.6432708443356"
        );
    }

    #[test]
    // Tests [@ANCHOR: unvoiced_dft]
    // Tests [@ANCHOR: unvoiced_spectrum]
    fn unvoiced_time_domain_is_real_for_a_real_conjugate_symmetric_spectrum() {
        let noise = NoiseState::new();
        let omega0_tilde = 2.0 * PI / 100.0;
        let voiced = vec![false; 16];
        let amplitudes = vec![500.0; 16];
        let gamma_w = unvoiced_scaling_coefficient();
        let spectrum =
            unvoiced_spectrum(&noise, omega0_tilde, &voiced, &amplitudes, gamma_w).unwrap();

        // Recompute the full complex IDFT (not just the real part unvoiced_time_domain keeps) to
        // check the discarded imaginary part is actually negligible, not just assumed to be.
        for n in -128i32..=127 {
            let mut acc = Complex::ZERO;
            for (j, &bin) in spectrum.iter().enumerate() {
                let m = j as i32 - 128;
                let theta = 2.0 * PI * (m as f64) * (n as f64) / 256.0;
                acc = acc.add(bin.mul(Complex::new(theta.cos(), theta.sin())));
            }
            let im = acc.scale(1.0 / 256.0).im;
            assert!(
                im.abs() < 1e-6,
                "imaginary part at n={n} was {im}, expected ~0"
            );
        }
    }

    #[test]
    fn unvoiced_spectrum_clamps_bands_past_nyquist() {
        // l_hat=40 with omega0 from b0=90 (~0.097 rad): (40.5)*0.097 > pi, previously an index-out-of-bounds panic.
        let noise = NoiseState::new();
        let voiced = vec![false; 40];
        let amplitudes = vec![100.0; 40];
        let spectrum =
            unvoiced_spectrum(&noise, 0.09703761092169245, &voiced, &amplitudes, 146.0).unwrap();
        assert!(spectrum
            .iter()
            .all(|c| c.re.is_finite() && c.im.is_finite()));
    }

    #[test]
    fn unvoiced_spectrum_rejects_a_length_mismatch() {
        let noise = NoiseState::new();
        let voiced = vec![false; 5];
        let amplitudes = vec![100.0; 6];
        assert!(unvoiced_spectrum(&noise, 0.1, &voiced, &amplitudes, 146.0).is_none());
    }

    /// The permanent regression check backing `unvoiced_spectrum`'s own doc comment: proves, rather
    /// than merely asserts in prose, that no real decode path (valid or bit-corrupted) can ever pass
    /// an `(l_hat, omega0_tilde)` pair into `unvoiced_spectrum` that panics `bin_index`.
    ///
    /// `decode.rs`'s own `l_hat = harmonics_count(omega0_tilde)` means the only inputs this crate's
    /// decoder can ever produce are `omega0_tilde = dequantize_fundamental_frequency(b0)` for `b0` in
    /// `0..=207` (`decode.rs`'s own `MAX_VALID_B0`, checked before `harmonics_count` is ever called;
    /// `b0`'s raw 8-bit field spans `0..=255` even under arbitrary bit corruption, so `208..=255`
    /// covers every value a corrupted frame could add, and all of those are already rejected as a
    /// repeat before reaching this formula). This test sweeps the complete real domain (all 208 valid
    /// `b0` values) and checks the actual geometric constraint `unvoiced_spectrum`'s own loop depends
    /// on: the top harmonic's own upper band edge, after the same `.ceil()` the real code applies,
    /// must stay within `bin_index`'s valid `-128..127` domain.
    #[test]
    fn unvoiced_spectrum_band_edge_stays_in_range_for_every_valid_b0() {
        use crate::ambe::float::tia_102_baba::parameter_encoding::dequantize_fundamental_frequency;
        use crate::ambe::float::tia_102_baba::vuv::harmonics_count;

        const MAX_VALID_B0: u32 = 207; // Mirrors decode.rs's own private `MAX_VALID_B0` exactly.
        let mut worst_margin = f64::INFINITY;
        for b0 in 0..=MAX_VALID_B0 {
            let omega0_tilde = dequantize_fundamental_frequency(b0);
            let l_hat = harmonics_count(omega0_tilde);
            let top_edge = band_edge_b(l_hat, omega0_tilde).ceil() as i32;
            assert!(
                (-128..=127).contains(&top_edge),
                "b0={b0}: l_hat={l_hat}, omega0_tilde={omega0_tilde}, top_edge={top_edge} would panic bin_index"
            );
            let margin = PI - (l_hat as f64 + 0.5) * omega0_tilde;
            worst_margin = worst_margin.min(margin);
        }
        // A real, comfortable margin (not a knife-edge floating-point rounding could flip) --
        // asserted directly so a future change to `harmonics_count`'s own formula that erodes this
        // margin gets caught here rather than only in a live decode panic.
        assert!(
            worst_margin > 0.05,
            "worst-case margin against the bin_index panic shrank to {worst_margin}, investigate before trusting this is still unreachable"
        );
    }

    #[test]
    // Tests [@ANCHOR: UnvoicedState::synthesize]
    // Tests [@ANCHOR: unvoiced_time_domain]
    // Tests [@ANCHOR: time_domain_at]
    // Tests [@ANCHOR: NoiseState::advance_frame]
    fn synthesize_produces_a_full_finite_frame_across_several_calls() {
        let mut state = UnvoicedState::new();
        let mut noise = NoiseState::new();
        let omega0_tilde = 2.0 * PI / 100.0;
        let voiced = vec![false; 16];
        let amplitudes = vec![500.0; 16];

        for i in 0..3 {
            if i > 0 {
                noise.advance_frame();
            }
            let frame = state
                .synthesize(&noise, omega0_tilde, &voiced, &amplitudes)
                .unwrap();
            assert_eq!(frame.len(), N);
            for &sample in &frame {
                assert!(sample.is_finite(), "non-finite unvoiced sample: {sample}");
            }
        }
    }

    #[test]
    fn synthesize_rejects_a_length_mismatch() {
        let mut state = UnvoicedState::new();
        let noise = NoiseState::new();
        let voiced = vec![false; 5];
        let amplitudes = vec![100.0; 6];
        assert!(state
            .synthesize(&noise, 0.1, &voiced, &amplitudes)
            .is_none());
    }
}
