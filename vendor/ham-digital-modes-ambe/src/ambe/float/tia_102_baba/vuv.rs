//! Voiced/unvoiced (V/UV) determination (TIA-102.BABA_2003.pdf section 5.2, Eq. 31-42) -- decides,
//! per frequency band, whether the current frame's speech spectrum is better explained by the
//! harmonic (voiced) model or not (unvoiced), reusing the same `S_w(m)` and synthetic-spectrum
//! machinery [`pitch_refinement`] already built for quarter-sample pitch refinement.
//!
//! All equations transcribed from a direct 600 DPI render of TIA-102.BABA_2003.pdf pages 33-35
//! (`pdftotext` garbles this section the same way it garbled Eq. 5 and 24-30 -- the base standard's
//! own custom Type3 digit font defeats plain text extraction for numeric literals throughout, not
//! just in the Annex tables), following the same discipline as `pitch.rs` and `pitch_refinement.rs`.
//!
//! # A real cross-reference oddity in the standard's own text, left as found
//!
//! The body text says "Due to the limits on `omega0_hat`, equation (37) confines `L_hat` to the
//! range 9 <= L_hat <= 56" -- but Eq. (37) in this same document is the V/UV threshold function
//! `Theta_xi`, not a constraint on `omega0_hat`; the range 9-56 is really a consequence of the
//! fundamental-frequency equation (`omega0 = 2*pi/P`) combined with Eq. 31 below, not of Eq. 37. This
//! looks like a genuine cross-reference slip in the standard itself (plausibly meant "equation (4)")
//! rather than a transcription error here -- the 9-56 bound itself is real and checked directly by
//! this module's own tests, only the equation number it's attributed to is suspect.
//!
//! # A second real oddity: Eq. 36's own denominator
//!
//! Eq. 35 (the per-band voicing measure for every band except the highest) divides by
//! `sum |S_w(m)|^2`. Eq. 36 (the highest band's own voicing measure) is textually identical except
//! the rendered denominator reads `sum |S_w(omega)|^2` -- a bare `omega` where every other instance
//! of this exact sum, in this equation and in Eq. 35, uses `m`. Re-rendered at 600 DPI and confirmed
//! this is really what the page shows, not an artifact of a lower-resolution extraction. Implemented
//! here as `S_w(m)` (matching Eq. 35 and this equation's own numerator, both indexed by `m`), on the
//! reading that this is a real, isolated typographical slip in the standard rather than a
//! substantively different quantity -- there is no other definition of a frequency-indexed `S_w(omega)`
//! anywhere in this document.

use std::f64::consts::PI;

use super::pitch_refinement::{window_dft_16384, RefinementFrame, SyntheticSpectrum};

/// `L_hat` (Eq. 31): the number of harmonics in the current segment, from the refined fundamental
/// frequency `omega0_hat`.
pub fn harmonics_count(omega0_hat: f64) -> u32 {
    let inner = (PI / omega0_hat + 0.25).floor();
    (0.9254 * inner).floor() as u32
}

/// `a_hat_l` (Eq. 32): the lower DFT-bin edge of harmonic `l`'s own frequency band, for `l >= 1`.
pub(crate) fn a_hat(l: u32, omega0_hat: f64) -> f64 {
    (256.0 / (2.0 * PI)) * (l as f64 - 0.5) * omega0_hat
}

/// `b_hat_l` (Eq. 33): the upper DFT-bin edge of harmonic `l`'s own frequency band, for `l >= 1`.
pub(crate) fn b_hat(l: u32, omega0_hat: f64) -> f64 {
    (256.0 / (2.0 * PI)) * (l as f64 + 0.5) * omega0_hat
}

/// `K_hat` (Eq. 34): the number of V/UV frequency bands, each (except possibly the last) spanning
/// three harmonics.
// [@ANCHOR: frequency_bands_count]
pub fn frequency_bands_count(l_hat: u32) -> u32 {
    if l_hat <= 36 {
        l_hat.div_ceil(3)
    } else {
        12
    }
}

/// The per-band voicing measure `D_k` (Eq. 35, for `1 <= k <= K_hat - 1`) or `D_K_hat` (Eq. 36, the
/// highest band): the fraction of the band's own real spectral energy that the synthetic
/// (harmonic-model) spectrum fails to explain. Near zero means the harmonic model fits well (voiced);
/// near one means it doesn't (unvoiced).
///
/// `is_highest_band` selects Eq. 36's own upper bound (`ceil(b_hat(l_hat)) - 1`, since the highest
/// band may hold more or fewer than three harmonics) instead of Eq. 35's `ceil(b_hat(3*k)) - 1`.
// [@ANCHOR: voicing_measure]
pub fn voicing_measure(
    frame: &RefinementFrame,
    k: u32,
    l_hat: u32,
    omega0_hat: f64,
    is_highest_band: bool,
) -> f64 {
    let m_lo = a_hat(3 * k - 2, omega0_hat).ceil() as i32;
    let upper_l = if is_highest_band { l_hat } else { 3 * k };
    let m_hi = b_hat(upper_l, omega0_hat).ceil() as i32; // exclusive, per Eq. 35/36's own m < ceil(b) bound

    let mut error_energy = 0.0;
    let mut real_energy = 0.0;
    let mut synthetic_spectrum = SyntheticSpectrum::new(frame, omega0_hat, l_hat);
    for m in m_lo..m_hi {
        let real = frame.sw_at(m);
        let synthetic = synthetic_spectrum.at(m);
        error_energy += real.sub(synthetic).norm_sqr();
        real_energy += real.norm_sqr();
    }
    if real_energy.abs() < 1e-12 {
        // No real spectral energy in this band at all -- nothing for the harmonic model to
        // explain or fail to explain; treat as maximally unvoiced rather than dividing by zero.
        1.0
    } else {
        error_energy / real_energy
    }
}

/// `xi_LF` (Eq. 38): the low-frequency (DFT bins 0-63) energy, normalized by the analysis window's
/// own DC gain `|W_R(0)|^2`.
pub fn xi_lf(frame: &RefinementFrame) -> f64 {
    let wr0_sqr = window_dft_16384(0).powi(2);
    (0..=63).map(|m| frame.sw_at(m).norm_sqr()).sum::<f64>() / wr0_sqr
}

/// `xi_HF` (Eq. 39): the high-frequency (DFT bins 64-128) energy, normalized the same way as
/// [`xi_lf`].
pub fn xi_hf(frame: &RefinementFrame) -> f64 {
    let wr0_sqr = window_dft_16384(0).powi(2);
    (64..=128).map(|m| frame.sw_at(m).norm_sqr()).sum::<f64>() / wr0_sqr
}

/// `xi_0` (Eq. 40): total frame energy, the sum of its low- and high-frequency parts.
pub fn xi_0(xi_lf: f64, xi_hf: f64) -> f64 {
    xi_lf + xi_hf
}

/// Updates `xi_max` for the current frame (Eq. 41) from its value in the previous frame and the
/// current frame's own `xi_0`. A slow-decaying running maximum of the frame energy, used by
/// [`energy_dependent_function`] to normalize the V/UV threshold against how loud speech has recently
/// been -- clamped to a floor of 20000 so a long stretch of near-silence doesn't drive the threshold
/// toward zero.
// [@ANCHOR: update_xi_max]
pub fn update_xi_max(xi_max_prev: f64, xi_0: f64) -> f64 {
    if xi_0 > xi_max_prev {
        0.5 * xi_max_prev + 0.5 * xi_0
    } else {
        let decayed = 0.99 * xi_max_prev + 0.01 * xi_0;
        if decayed > 20000.0 {
            decayed
        } else {
            20000.0
        }
    }
}

/// `M(xi)` (Eq. 42): an energy-dependent scaling function used by [`voicing_threshold`], comparing
/// the current frame's own energy (`xi_0`) against its recent running maximum (`xi_max`), with an
/// extra low-frequency-dominance correction when the spectrum isn't clearly low-frequency-heavy.
// [@ANCHOR: energy_dependent_function]
pub fn energy_dependent_function(xi_max: f64, xi_0: f64, xi_lf: f64, xi_hf: f64) -> f64 {
    let base = (0.0025 * xi_max + xi_0) / (0.01 * xi_max + xi_0);
    if xi_lf >= 5.0 * xi_hf {
        base
    } else {
        base * (xi_lf / (5.0 * xi_hf)).sqrt()
    }
}

/// The V/UV threshold function `Theta_xi(k, omega0_hat)` (Eq. 37), compared against [`voicing_measure`]
/// to decide band `k`'s own voiced/unvoiced state: band `k` is voiced (`v_hat_k = 1`) iff
/// `D_k < Theta_xi(k, omega0_hat)`.
///
/// `initial_pitch_error` is `E(P_hat_I)`, the initial (half-sample) pitch estimate's own error
/// function value from `pitch::PitchAnalysisFrame::error_function` -- a large value there means the
/// initial pitch estimate itself was unreliable, in which case every band but the first is forced
/// unvoiced outright (the spec's own first case below).
// [@ANCHOR: voicing_threshold]
pub fn voicing_threshold(
    k: u32,
    omega0_hat: f64,
    initial_pitch_error: f64,
    previous_band_voiced: bool,
    m_xi: f64,
) -> f64 {
    if initial_pitch_error > 0.5 && k >= 2 {
        0.0
    } else if previous_band_voiced {
        0.5625 * (1.0 - 0.3096 * (k as f64 - 1.0) * omega0_hat) * m_xi
    } else {
        0.45 * (1.0 - 0.3096 * (k as f64 - 1.0) * omega0_hat) * m_xi
    }
}

/// Runs the full V/UV determination for one frame (Fig. 11): computes `L_hat`, `K_hat`, and every
/// band's own voiced/unvoiced decision `v_hat_k`, returning them as `voiced[k-1]` for `1 <= k <=
/// K_hat`. `xi_max_prev` and `previous_v` carry state from the prior frame (Eq. 37's own
/// `v_hat_k(-1)` and Eq. 41's own `xi_max(-1)`); `previous_v` is indexed the same way the return
/// value is, and a previous frame with fewer bands than the current one (or no previous frame at
/// all, at stream start) is read as "not voiced" for any band index it doesn't cover -- the spec's
/// own text doesn't address a change in `K_hat` between frames, so this is this implementation's own
/// reasonable default, not a transcribed rule.
// [@ANCHOR: determine_voicing]
pub fn determine_voicing(
    frame: &RefinementFrame,
    omega0_hat: f64,
    initial_pitch_error: f64,
    xi_max_prev: f64,
    previous_v: &[bool],
) -> (Vec<bool>, f64) {
    let l_hat = harmonics_count(omega0_hat);
    let k_hat = frequency_bands_count(l_hat);

    let xi_lf_val = xi_lf(frame);
    let xi_hf_val = xi_hf(frame);
    let xi_0_val = xi_0(xi_lf_val, xi_hf_val);
    let xi_max = update_xi_max(xi_max_prev, xi_0_val);
    let m_xi = energy_dependent_function(xi_max, xi_0_val, xi_lf_val, xi_hf_val);

    let voiced = (1..=k_hat)
        .map(|k| {
            let is_highest = k == k_hat;
            let d_k = voicing_measure(frame, k, l_hat, omega0_hat, is_highest);
            let previous_band_voiced = previous_v.get((k - 1) as usize).copied().unwrap_or(false);
            let theta = voicing_threshold(
                k,
                omega0_hat,
                initial_pitch_error,
                previous_band_voiced,
                m_xi,
            );
            d_k < theta
        })
        .collect();
    (voiced, xi_max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI as PI64;

    #[test]
    fn harmonics_count_matches_eq31_at_representative_omega0_values() {
        // Hand-computed: omega0 = 2*pi/60 -> pi/omega0 = 30 -> floor(30 + .25) = 30 -> floor(.9254*30) = 27
        assert_eq!(harmonics_count(2.0 * PI64 / 60.0), 27);
        // omega0 = 2*pi/122 (near the low end of the pitch range) -> pi/omega0 = 61 -> floor(61.25)=61
        // -> floor(.9254*61) = floor(56.4494) = 56, the spec's own stated upper bound.
        assert_eq!(harmonics_count(2.0 * PI64 / 122.0), 56);
        // omega0 = 2*pi/21 (near the high end / short period) -> pi/omega0 = 10.5 -> floor(10.75)=10
        // -> floor(.9254*10) = floor(9.254) = 9, the spec's own stated lower bound.
        assert_eq!(harmonics_count(2.0 * PI64 / 21.0), 9);
    }

    #[test]
    // Tests [@ANCHOR: frequency_bands_count]
    fn frequency_bands_count_matches_eq34_including_the_l_le_36_boundary() {
        assert_eq!(frequency_bands_count(9), 9u32.div_ceil(3)); // = 3
        assert_eq!(frequency_bands_count(36), 36u32.div_ceil(3)); // = 12, still the "L<=36" branch
        assert_eq!(frequency_bands_count(37), 12); // now the "otherwise" branch, still 12
        assert_eq!(frequency_bands_count(56), 12);
    }

    #[test]
    // Tests [@ANCHOR: energy_dependent_function]
    fn energy_dependent_function_matches_eq42_in_both_branches() {
        // Branch 1: xi_LF >= 5*xi_HF.
        let m = energy_dependent_function(20000.0, 20000.0, 100.0, 10.0);
        let expected = (0.0025 * 20000.0 + 20000.0) / (0.01 * 20000.0 + 20000.0);
        assert!((m - expected).abs() < 1e-9);

        // Branch 2: otherwise, with the extra sqrt(xi_LF / (5*xi_HF)) factor.
        let m2 = energy_dependent_function(20000.0, 20000.0, 10.0, 100.0);
        let expected2 = expected * (10.0f64 / (5.0 * 100.0)).sqrt();
        assert!((m2 - expected2).abs() < 1e-9);
    }

    #[test]
    // Tests [@ANCHOR: update_xi_max]
    fn update_xi_max_matches_eq41_in_all_three_branches() {
        // Branch 1: xi_0 exceeds the previous max.
        assert!((update_xi_max(20000.0, 30000.0) - 25000.0).abs() < 1e-9);
        // Branch 2: decayed value stays above the 20000 floor.
        let decayed = 0.99 * 100_000.0 + 0.01 * 50_000.0;
        assert!((update_xi_max(100_000.0, 50_000.0) - decayed).abs() < 1e-9);
        assert!(decayed > 20000.0);
        // Branch 3: decayed value would fall below the floor, so the floor wins.
        assert!((update_xi_max(20000.0, 0.0) - 20000.0).abs() < 1e-9);
    }

    #[test]
    fn xi_lf_plus_xi_hf_equals_xi_0() {
        assert!((xi_0(123.4, 56.7) - 180.1).abs() < 1e-9);
    }

    /// A synthetic exact-harmonic signal (same construction `pitch_refinement`'s own tests use):
    /// when the candidate `omega0_hat` really is the signal's true fundamental, the synthetic
    /// spectrum should reconstruct the real spectrum almost exactly in the low bands, so the first
    /// band's own voicing measure `D_1` should be near zero (the harmonic/voiced model fits).
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
                    .map(|k| (1.0 / k as f64) * (2.0 * PI64 * fundamental_hz * k as f64 * t).sin())
                    .sum()
            })
            .collect()
    }

    #[test]
    // Tests [@ANCHOR: voicing_measure]
    fn voicing_measure_is_near_zero_for_the_first_band_of_a_real_harmonic_signal() {
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 400);
        let frame = RefinementFrame::new(&raw, 200);
        let omega0_hat = 2.0 * PI64 / period;
        let l_hat = harmonics_count(omega0_hat);

        let d1 = voicing_measure(&frame, 1, l_hat, omega0_hat, false);
        assert!(
            d1 < 0.05,
            "expected the first band's voicing measure to be near zero for an exact harmonic \
             signal at its own true fundamental, got {d1}"
        );
    }

    #[test]
    // Tests [@ANCHOR: determine_voicing]
    // Tests [@ANCHOR: voicing_threshold]
    fn determine_voicing_declares_a_real_harmonic_signals_low_bands_voiced() {
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 400);
        let frame = RefinementFrame::new(&raw, 200);
        let omega0_hat = 2.0 * PI64 / period;

        // A small initial-pitch error (the harmonic signal's own pitch is unambiguous) and no
        // previous-frame history -- the honest first-frame case.
        let (voiced, xi_max) = determine_voicing(&frame, omega0_hat, 0.01, 20000.0, &[]);
        assert!(!voiced.is_empty());
        assert!(
            voiced[0],
            "expected the lowest frequency band of a clean harmonic signal to be declared voiced"
        );
        assert!(xi_max >= 20000.0);
    }
}
