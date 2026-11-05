//! Spectral amplitude estimation (TIA-102.BABA_2003.pdf section 5.3, Eq. 43-44) -- estimates each
//! harmonic's own spectral magnitude `M_hat_l`, using the voiced/unvoiced decision already made per
//! frequency band ([`super::vuv`]) to choose between a voiced estimator (Eq. 43, a windowed-energy
//! ratio against the analysis window's own spectral shape) and an unvoiced estimator (Eq. 44, a flat
//! average spectral density over the band).
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf page 37 (Type3 digit font, the same
//! `pdftotext` limitation as every other body-text equation in this spec).
//!
//! # A third real oddity in the standard's own text
//!
//! Eq. 43's numerator sums over `m = ceil(a_hat_l) .. ceil(b_hat_l) - 1` (hatted, i.e. using the
//! refined fundamental frequency `omega0_hat`, matching every other equation in sections 5.2-5.3),
//! but its own denominator sum is written with plain (unhatted) `a_l`/`b_l`. Since `a_l`/`b_l` and
//! `a_hat_l`/`b_hat_l` are the exact same formula (Eq. 32/33) evaluated at the same `omega0_hat` --
//! the "hat" elsewhere in this document distinguishes which frequency estimate is in scope, not a
//! different formula -- this is read the same way `vuv.rs`'s own module doc already reads Eq. 36's
//! `S_w(omega)` slip: a real, isolated notational inconsistency in the standard's own typesetting,
//! not a substantively different quantity. Implemented with the identical hatted bin range in both
//! the numerator and denominator.

use std::f64::consts::PI;

use super::pitch::pitch_refinement_window;
use super::pitch_refinement::{window_dft_16384, RefinementFrame};
use super::vuv::{a_hat, b_hat};

/// The voiced spectral amplitude estimate (Eq. 43): the square root of the ratio between the band's
/// own real spectral energy and the analysis window's own spectral energy over the same bins --
/// effectively "how much of this harmonic band's energy exceeds what the window alone would
/// produce," which is what makes this a magnitude estimate rather than a raw energy figure.
// [@ANCHOR: voiced_amplitude]
pub fn voiced_amplitude(frame: &RefinementFrame, l: u32, omega0_hat: f64) -> f64 {
    let m_lo = a_hat(l, omega0_hat).ceil() as i32;
    let m_hi = b_hat(l, omega0_hat).ceil() as i32; // exclusive
    let mut signal_energy = 0.0;
    let mut window_energy = 0.0;
    for m in m_lo..m_hi {
        signal_energy += frame.sw_at(m).norm_sqr();
        let wr_index = (64.0 * (m as f64) - (16384.0 / (2.0 * PI)) * (l as f64) * omega0_hat + 0.5)
            .floor() as i32;
        let wr = window_dft_16384(wr_index);
        window_energy += wr * wr;
    }
    if window_energy.abs() < 1e-12 {
        0.0
    } else {
        (signal_energy / window_energy).sqrt()
    }
}

/// The analysis window's own total tap sum, `sum_{n=-110}^{110} w_R(n)` -- the constant normalizer
/// Eq. 44 divides by (the same window [`pitch_refinement_window`] used throughout sections 5.1.5 and
/// 5.2-5.3).
fn window_tap_sum() -> f64 {
    (-110i32..=110).map(pitch_refinement_window).sum()
}

/// The unvoiced spectral amplitude estimate (Eq. 44): the band's own average spectral energy density
/// (real signal energy per bin, not compared against the window at all), normalized by the window's
/// total tap sum -- unvoiced harmonics get a flat noise-like amplitude rather than the voiced
/// estimator's per-bin comparison against the window's own shape.
// [@ANCHOR: unvoiced_amplitude]
pub fn unvoiced_amplitude(frame: &RefinementFrame, l: u32, omega0_hat: f64) -> f64 {
    let m_lo = a_hat(l, omega0_hat).ceil() as i32;
    let m_hi = b_hat(l, omega0_hat).ceil() as i32; // exclusive
    let band_width = (m_hi - m_lo) as f64;
    if band_width <= 0.0 {
        return 0.0;
    }
    let signal_energy: f64 = (m_lo..m_hi).map(|m| frame.sw_at(m).norm_sqr()).sum();
    (1.0 / window_tap_sum()) * (signal_energy / band_width).sqrt()
}

/// Estimates every harmonic's own spectral amplitude `M_hat_l` for `1 <= l <= l_hat` (Fig. 13):
/// harmonic `l` falls in V/UV band `k = min(ceil(l/3), k_hat)` (the `min` covers the highest band,
/// which per section 5.2's own text may hold more or fewer than three harmonics), and uses
/// [`voiced_amplitude`] or [`unvoiced_amplitude`] according to that band's own decision in `voiced`
/// (as returned by `vuv::determine_voicing`).
// [@ANCHOR: estimate_spectral_amplitudes]
pub fn estimate_spectral_amplitudes(
    frame: &RefinementFrame,
    l_hat: u32,
    k_hat: u32,
    omega0_hat: f64,
    voiced: &[bool],
) -> Vec<f64> {
    (1..=l_hat)
        .map(|l| {
            let k = (l as f64 / 3.0).ceil() as u32;
            let k = k.min(k_hat).max(1);
            let is_voiced = voiced.get((k - 1) as usize).copied().unwrap_or(false);
            if is_voiced {
                voiced_amplitude(frame, l, omega0_hat)
            } else {
                unvoiced_amplitude(frame, l, omega0_hat)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harmonic_signal(
        fundamental_hz: f64,
        sample_rate: f64,
        num_harmonics: u32,
        amplitude: f64,
        total_len: usize,
    ) -> Vec<f64> {
        (0..total_len)
            .map(|n| {
                let t = n as f64 / sample_rate;
                amplitude
                    * (1..=num_harmonics)
                        .map(|k| {
                            (1.0 / k as f64) * (2.0 * PI * fundamental_hz * k as f64 * t).sin()
                        })
                        .sum::<f64>()
            })
            .collect()
    }

    #[test]
    // Tests [@ANCHOR: voiced_amplitude]
    fn voiced_amplitude_scales_linearly_with_signal_amplitude() {
        // Eq. 43 is a square root of an energy ratio; doubling a signal's own amplitude quadruples
        // its energy, so the estimate itself should double -- a real, checkable linearity property
        // independent of the exact numeric value, which depends on window/DFT details this test
        // doesn't need to re-derive by hand.
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let omega0_hat = 2.0 * PI / period;

        let raw1 = harmonic_signal(fundamental_hz, sample_rate, 36, 1.0, 400);
        let frame1 = RefinementFrame::new(&raw1, 200);
        let m1 = voiced_amplitude(&frame1, 1, omega0_hat);

        let raw2 = harmonic_signal(fundamental_hz, sample_rate, 36, 2.0, 400);
        let frame2 = RefinementFrame::new(&raw2, 200);
        let m2 = voiced_amplitude(&frame2, 1, omega0_hat);

        assert!(m1 > 0.0, "expected a positive amplitude estimate, got {m1}");
        let ratio = m2 / m1;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "expected doubling the signal amplitude to double the estimate, got ratio {ratio}"
        );
    }

    #[test]
    // Tests [@ANCHOR: unvoiced_amplitude]
    fn unvoiced_amplitude_is_positive_for_a_real_signal() {
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let omega0_hat = 2.0 * PI / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 1.0, 400);
        let frame = RefinementFrame::new(&raw, 200);

        let m = unvoiced_amplitude(&frame, 1, omega0_hat);
        assert!(
            m > 0.0,
            "expected a positive unvoiced amplitude estimate, got {m}"
        );
    }

    #[test]
    // Tests [@ANCHOR: estimate_spectral_amplitudes]
    fn estimate_spectral_amplitudes_routes_each_harmonic_to_its_own_bands_decision() {
        let sample_rate = 8000.0;
        let period = 80.0;
        let fundamental_hz = sample_rate / period;
        let omega0_hat = 2.0 * PI / period;
        let raw = harmonic_signal(fundamental_hz, sample_rate, 36, 1.0, 400);
        let frame = RefinementFrame::new(&raw, 200);

        // l_hat=4, k_hat=2: band 1 covers l=1..=3 (voiced), band 2 covers l=4 as its own highest,
        // possibly-narrower band (unvoiced).
        let l_hat = 4;
        let k_hat = 2;
        let voiced = [true, false];
        let amplitudes = estimate_spectral_amplitudes(&frame, l_hat, k_hat, omega0_hat, &voiced);

        assert_eq!(amplitudes.len(), 4);
        for &l in &[1u32, 2, 3] {
            let expected = voiced_amplitude(&frame, l, omega0_hat);
            assert!(
                (amplitudes[(l - 1) as usize] - expected).abs() < 1e-12,
                "harmonic {l} should have used the voiced estimator"
            );
        }
        let expected_unvoiced = unvoiced_amplitude(&frame, 4, omega0_hat);
        assert!(
            (amplitudes[3] - expected_unvoiced).abs() < 1e-12,
            "harmonic 4 should have used the unvoiced estimator"
        );
    }
}
