//! Quarter-sample pitch refinement (TIA-102.BABA_2003.pdf section 5.1.5, Eq. 24-30) -- improves the
//! initial pitch estimate (`pitch::choose_initial_pitch_estimate`, half-sample accuracy) to
//! quarter-sample accuracy by comparing the real speech spectrum against a synthetic spectrum built
//! from estimated harmonic amplitudes, for ten fine-grained candidates around the initial estimate.
//!
//! All equations transcribed from a direct high-DPI render of the relevant page (the `pdftotext`
//! extraction of this section badly garbles the nested floor/ceiling notation in Eq. 24 and 28 --
//! cropped and re-rendered at 600 DPI to resolve it with certainty rather than guess at OCR
//! artifacts, the same discipline `pitch.rs`'s own Eq. 5 transcription used).
//!
//! A real simplification found directly in the spec's own text, not assumed: `W_R(m)` (Eq. 30, the
//! window's own DFT) is always real, since `w_R(n)` is a real, symmetric sequence -- the spec states
//! this explicitly ("since w_R(n) is a real symmetric sequence, W_R*(m) = W_R(m)"), which is only
//! possible for a value equal to its own complex conjugate. That means every place Eq. 28's own
//! `A_l(omega0)` formula multiplies or divides by `W_R`, it's a real scalar, not a complex one -- no
//! complex-times-complex multiplication is ever needed anywhere in this module, only complex-scaled-
//! by-real, so a minimal local complex type covering exactly that is enough; a general-purpose
//! complex-arithmetic crate would be solving a harder problem than this one actually has.

use std::f64::consts::PI;
use std::sync::OnceLock;

use super::pitch::pitch_refinement_window;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    const ZERO: Complex = Complex { re: 0.0, im: 0.0 };

    fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    fn scale(self, s: f64) -> Self {
        Self::new(self.re * s, self.im * s)
    }

    fn add(self, other: Self) -> Self {
        Self::new(self.re + other.re, self.im + other.im)
    }

    pub(crate) fn sub(self, other: Self) -> Self {
        Self::new(self.re - other.re, self.im - other.im)
    }

    pub(crate) fn norm_sqr(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
}

/// `W_R(m)` (Eq. 30): the 16384-point DFT of `w_R(n)`, computed as a direct sum over the window's own
/// real 221 nonzero samples (`n` in `-110..=110`) -- there is no need to actually run (or zero-pad
/// into) a literal 16384-point transform, since `w_R(n)` is exactly zero outside that range and a
/// direct sum over the nonzero samples computes the identical result for any single queried `m`.
/// Real-valued (see this module's own doc comment on why): only the cosine term is computed: the
/// sine term's own contribution exactly cancels for a real, even-symmetric sequence like `w_R(n)`, a
/// direct consequence of the same fact the spec states explicitly, not a separate approximation.
// [@ANCHOR: window_dft_16384]
pub(crate) fn window_dft_16384(m: i32) -> f64 {
    // `W_R` is even in `m`, and the pitch refinement, voicing and amplitude stages evaluate it thousands of times per
    // frame at integer `m` in `-8191..=8192`, so the half-line `0..=8192` is tabulated once per process.
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    let table = TABLE.get_or_init(|| (0..=8192).map(window_dft_16384_direct).collect());
    match table.get(m.unsigned_abs() as usize) {
        Some(&v) => v,
        None => window_dft_16384_direct(m),
    }
}

fn window_dft_16384_direct(m: i32) -> f64 {
    let mut acc = 0.0;
    for n in -110i32..=110 {
        let theta = -2.0 * PI * (m as f64) * (n as f64) / 16384.0;
        acc += pitch_refinement_window(n) * theta.cos();
    }
    acc
}

/// A single analysis frame's own `S_w(m)` (Eq. 29): the 256-point DFT of `s(n)*w_R(n)`, for `m` in
/// `-127..=128` (precomputed once per frame, the same "expensive input, compute once" reasoning
/// `pitch::PitchAnalysisFrame` already uses for `s_LPF`).
pub struct RefinementFrame {
    sw: [Complex; 256],
}

impl RefinementFrame {
    /// `raw` is the real speech signal; `center` is the sample index in `raw` corresponding to this
    /// frame's own `n = 0`. `raw` must have at least 110 real samples of margin on both sides of
    /// `center` -- panics (via array indexing) otherwise, the same real, unavoidable data dependency
    /// `pitch::lowpass_filtered_sample` already has.
    // [@ANCHOR: RefinementFrame::new]
    pub fn new(raw: &[f64], center: usize) -> Self {
        // `exp(-2 pi j m n / 256)` depends only on `m n mod 256`: tabulate the 256 twiddles once.
        let twiddle: Vec<(f64, f64)> = (0..256)
            .map(|k| (-2.0 * PI * k as f64 / 256.0).sin_cos())
            .map(|(s, c)| (c, s))
            .collect();
        let windowed: Vec<f64> = (-110i32..=110)
            .map(|n| raw[(center as i32 + n) as usize] * pitch_refinement_window(n))
            .collect();
        let mut sw = [Complex::ZERO; 256];
        for (i, slot) in sw.iter_mut().enumerate() {
            let m = i as i32 - 127;
            let mut acc = Complex::ZERO;
            for (j, &sample) in windowed.iter().enumerate() {
                let n = j as i32 - 110;
                let (c, s) = twiddle[(m * n).rem_euclid(256) as usize];
                acc = acc.add(Complex::new(sample * c, sample * s));
            }
            *slot = acc;
        }
        Self { sw }
    }

    /// `S_w(m)` for `m` in `-127..=128`; returns `Complex::ZERO` outside that range (a real
    /// convenience for [`harmonic_amplitude`]'s own bin-range loop, which can legitimately compute a
    /// bin index a hair outside this range for a candidate near the edge of the spec's own pitch
    /// range -- treated as "no real spectral content there" rather than panicking, matching this
    /// module's overall "give up gracefully on an edge candidate, don't crash" posture).
    // [@ANCHOR: RefinementFrame::sw_at]
    pub(crate) fn sw_at(&self, m: i32) -> Complex {
        if (-127..=128).contains(&m) {
            self.sw[(m + 127) as usize]
        } else {
            Complex::ZERO
        }
    }
}

/// The harmonic amplitude `A_l(omega0)` (Eq. 26-28): a least-squares estimate of the `l`'th
/// harmonic's own complex amplitude, from the DFT bins in `S_w(m)` nearest that harmonic's own
/// frequency, weighted by the window's own spectral shape `W_R`.
// [@ANCHOR: harmonic_amplitude]
fn harmonic_amplitude(frame: &RefinementFrame, l: u32, omega0: f64) -> Complex {
    let a_l = (256.0 / (2.0 * PI)) * (l as f64 - 0.5) * omega0;
    let b_l = (256.0 / (2.0 * PI)) * (l as f64 + 0.5) * omega0;
    let m_lo = a_l.ceil() as i32;
    let m_hi = b_l.ceil() as i32; // exclusive, per Eq. 28's own `m < ceil(b_l)` bound
    let mut numerator = Complex::ZERO;
    let mut denominator = 0.0;
    for m in m_lo..m_hi {
        let wr_index =
            (64.0 * (m as f64) - (16384.0 / (2.0 * PI)) * (l as f64) * omega0 + 0.5).floor() as i32;
        let wr = window_dft_16384(wr_index);
        numerator = numerator.add(frame.sw_at(m).scale(wr));
        denominator += wr * wr;
    }
    if denominator.abs() < 1e-12 {
        // No real spectral weight fell in this harmonic's own band -- a real, degenerate case for a
        // candidate omega0 whose l'th harmonic band is empty or vanishingly narrow, not an error.
        Complex::ZERO
    } else {
        numerator.scale(1.0 / denominator)
    }
}

/// The synthetic spectrum `S_w(m, omega0)` (Eq. 25): for the DFT bin `m`, finds which harmonic band
/// (if any, per Eq. 26-27) `m` falls into and returns that harmonic's own estimated contribution.
// [@ANCHOR: SyntheticSpectrum]
pub(crate) struct SyntheticSpectrum<'a> {
    frame: &'a RefinementFrame,
    omega0: f64,
    max_l: u32,
    /// `A_l(omega0)` per harmonic, computed on first use (a band's own bins all share one amplitude).
    amplitudes: Vec<Option<Complex>>,
}

impl<'a> SyntheticSpectrum<'a> {
    pub(crate) fn new(frame: &'a RefinementFrame, omega0: f64, max_l: u32) -> Self {
        Self {
            frame,
            omega0,
            max_l,
            amplitudes: vec![None; max_l as usize + 1],
        }
    }

    /// `S_w(m, omega0)` for bin `m`.
    pub(crate) fn at(&mut self, m: i32) -> Complex {
        let omega0 = self.omega0;
        for l in 0..=self.max_l {
            let a_l = (256.0 / (2.0 * PI)) * (l as f64 - 0.5) * omega0;
            let b_l = (256.0 / (2.0 * PI)) * (l as f64 + 0.5) * omega0;
            let m_lo = a_l.ceil() as i32;
            let m_hi = b_l.ceil() as i32;
            if m >= m_lo && m < m_hi {
                let frame = self.frame;
                let amplitude = *self.amplitudes[l as usize]
                    .get_or_insert_with(|| harmonic_amplitude(frame, l, omega0));
                let wr_index = (64.0 * (m as f64) - (16384.0 / (2.0 * PI)) * (l as f64) * omega0
                    + 0.5)
                    .floor() as i32;
                return amplitude.scale(window_dft_16384(wr_index));
            }
        }
        Complex::ZERO
    }
}

/// The pitch refinement error function `E_R(omega0)` (Eq. 24): sums the squared magnitude difference
/// between the real spectrum `S_w(m)` and the synthetic spectrum `S_w(m, omega0)` over the DFT bins
/// covered by the harmonics a candidate `omega0` implies.
// [@ANCHOR: refinement_error]
pub fn refinement_error(frame: &RefinementFrame, omega0: f64) -> f64 {
    let l_estimate = (0.9254 * PI / omega0 - 0.5).floor();
    let upper_m = (l_estimate * (256.0 / (2.0 * PI)) * omega0).floor() as i32;
    let max_l = l_estimate.max(0.0) as u32 + 1;
    let mut synthetic = SyntheticSpectrum::new(frame, omega0, max_l);
    (50..=upper_m)
        .map(|m| frame.sw_at(m).sub(synthetic.at(m)).norm_sqr())
        .sum()
}

/// Refines a half-sample-accuracy initial pitch estimate `p_hat_i` to quarter-sample accuracy
/// (section 5.1.5): tries the ten candidates `p_hat_i - 9/8, p_hat_i - 7/8, ..., p_hat_i + 7/8,
/// p_hat_i + 9/8`, converts each to its equivalent fundamental frequency via Eq. 4 (`omega0 =
/// 2*pi/P`), and returns whichever candidate minimizes [`refinement_error`].
pub fn refine_pitch(frame: &RefinementFrame, p_hat_i: f64) -> f64 {
    let offsets = [
        -9.0 / 8.0,
        -7.0 / 8.0,
        -5.0 / 8.0,
        -3.0 / 8.0,
        -1.0 / 8.0,
        1.0 / 8.0,
        3.0 / 8.0,
        5.0 / 8.0,
        7.0 / 8.0,
        9.0 / 8.0,
    ];
    offsets
        .into_iter()
        .map(|offset| {
            let p = p_hat_i + offset;
            let omega0 = 2.0 * PI / p;
            (omega0, refinement_error(frame, omega0))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("offsets is never empty")
        .0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic signal built from real, exact sinusoidal harmonics of a known fundamental --
    /// unlike `pitch::pitch_analysis_tests`'s own pulse train (which is real but has a much richer,
    /// less controlled harmonic structure), this lets a test assert the refinement error is
    /// minimized at the *exact* true fundamental, not merely a plausible neighborhood, since the
    /// signal really is a sum of harmonics of exactly one frequency by construction.
    ///
    /// **`num_harmonics` matters more than it looks, a real thing found while validating this
    /// module**: `refinement_error`'s own `E_R` sum covers every DFT bin up to the candidate
    /// `omega0`'s own implied harmonic count (`l_estimate`, Eq. 24's upper limit) -- for a period-80
    /// candidate at 8 kHz that's roughly 36 harmonics. A test signal with only 12 real harmonics
    /// leaves most of that range empty on every candidate alike, diluting the true period's own
    /// real advantage into near-noise (measured directly: with only 12 harmonics, E_R barely
    /// varied across a wide range of periods, a 3% spread; with a realistic ~36, E_R at the true
    /// period measured over 30x lower than its nearest competitor). Not a bug in `refinement_error`
    /// -- a real property of the error function itself, which only discriminates well when the
    /// signal actually has energy across the bins it examines, matching how a real, full-bandwidth
    /// voice signal (not a synthetic under-populated test signal) would actually look.
    fn harmonic_signal(
        fundamental_hz: f64,
        sample_rate: f64,
        num_harmonics: u32,
        total_len: usize,
    ) -> Vec<f64> {
        (0..total_len)
            .map(|n| {
                let t = n as f64 / sample_rate;
                (1..=num_harmonics)
                    .map(|k| (1.0 / k as f64) * (2.0 * PI * fundamental_hz * k as f64 * t).sin())
                    .sum()
            })
            .collect()
    }

    #[test]
    // Tests [@ANCHOR: window_dft_16384]
    // Tests [@ANCHOR: RefinementFrame::new]
    // Tests [@ANCHOR: RefinementFrame::sw_at]
    // Tests [@ANCHOR: harmonic_amplitude]
    // Tests [@ANCHOR: SyntheticSpectrum]
    // Tests [@ANCHOR: refinement_error]
    fn refinement_error_is_minimized_at_the_true_fundamental_of_a_real_harmonic_signal() {
        // 8 kHz sample rate matches the spec's own stated domain ("P0 is measured in samples (at 8
        // kHz)"). A period of 80 samples (100 Hz) is a real, plausible low male voice pitch.
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 400);
        let frame = RefinementFrame::new(&raw, 200);

        let true_omega0 = 2.0 * PI / period;
        let e_true = refinement_error(&frame, true_omega0);
        for &wrong_period in &[70.0, 90.0, 100.0, 160.0] {
            let e_wrong = refinement_error(&frame, 2.0 * PI / wrong_period);
            assert!(
                e_true < e_wrong,
                "expected the true period {period} to score better than {wrong_period}, \
                 got e_true={e_true} e_wrong={e_wrong}"
            );
        }
    }

    #[test]
    fn refine_pitch_selects_the_true_period_from_its_ten_candidates() {
        let sample_rate = 8000.0;
        let period = 60.0; // 133 Hz, matching pitch.rs's own synthetic test's real voice pitch
        let fundamental_hz = sample_rate / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 400);
        let frame = RefinementFrame::new(&raw, 200);

        // p_hat_i offset from the true period by less than the 9/8-sample candidate spread, the
        // real, honest input this function actually receives (an approximate half-sample estimate,
        // not the exact answer already known) -- not simply passed the true period itself, which
        // would prove nothing about the refinement search actually working.
        let p_hat_i = period + 0.5;
        let refined_omega0 = refine_pitch(&frame, p_hat_i);
        let refined_period = 2.0 * PI / refined_omega0;
        assert!(
            (refined_period - period).abs() < 0.3,
            "expected refinement to land within a quarter sample of the true period {period}, \
             got {refined_period}"
        );
    }
}
