// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point quarter-sample pitch refinement (TIA-102.BABA_2003.pdf section 5.1.5): the 256-point
//! windowed DFT `S_w(m)`, the harmonic amplitude `A_l` (Eq. 26-28), the synthetic spectrum (Eq. 25), the
//! refinement error `E_R` (Eq. 24) and [`refine_pitch`]. Fixed-point sibling of
//! [`crate::ambe::float::tia_102_baba::pitch_refinement`].
//!
//! # Pitch representation: [`Pitch`] (an exact fraction), and `P8` for the refinement search
//!
//! The refinement returns a period in eighths of a sample, `P8`; [`Pitch`] wraps either a `P8` or an
//! arbitrary `omega0` (needed by the voicing/amplitude analysis at a decoder-quantized pitch) in the
//! exact-fraction form the band arithmetic uses. Every pitch the refinement examines is `p_hat_i +- k/8` with `p_hat_i` a half-sample candidate, so
//! the period is an exact integer `P8 = 8 * P`. With `omega0 = 2*pi/P` every place the float code
//! multiplies by `omega0` and by `256/(2*pi)` or `16384/(2*pi)` cancels the `pi`:
//!
//! * band edges `a_l = 256 (l - 1/2) / P = 1024 (2l - 1) / P8` (and `b_l = a_{l+1}`),
//! * window-DFT index `floor(64 m - 16384 l / P + 1/2) = floor((128 m P8 - 262144 l + P8) / (2 P8))`,
//! * `L_est = floor(0.9254 P / 2 - 1/2)`.
//!
//! All are evaluated with exact integer division, so no fixed-point `pi` or reciprocal is needed and
//! the band/bin bookkeeping cannot drift from the real-number definition. [`omega0_q30_from_p8`]
//! converts to radians per sample (Q30) for the consumers that need `omega0` itself.
//!
//! # Numeric formats
//!
//! * `S_w(m)`: complex, each part an `i64` in Q10 (a 16-bit input times the window sum 110 is below
//!   `2^23`, so Q10 is below `2^33`). Computed from `raw * w_R` (Q10) times a Q30 cosine table, exact
//!   to rounding of each partial product; `m < 0` uses conjugate symmetry of a real input.
//! * `W_R(m)`: Q22 table for `|m| <= 512` (indices beyond that are treated as 0; the reachable
//!   range for pitches of at least 19.875 is about 476).
//! * Harmonic amplitude `A_l`: Q16 complex `i64` (`num / den` with `num = sum S_w W_R` in Q32 and
//!   `den = sum W_R^2` in Q44, both `i128`).
//! * Error/energy sums: `i128`, Q60 (`|diff|^2` of Q30 differences).
//!
//! # Verification (see `tests/ambe_fixed_tia_102_baba_pitch_refinement.rs`)
//!
//! On real speech (all 7748 frames of the four `osr_speech` files, the same initial pitch handed to
//! both implementations) the refined period `P8` equals the float sibling's refined period, to the
//! eighth of a sample, on 99.99% of frames (one frame differs, by one eighth), and `E_R` at the chosen
//! period agrees to a worst-case relative error of 7.0e-5.

use super::pitch::p2_of_index;
use super::pitch_refinement_tables::{
    COS_256_Q30, REFINEMENT_WINDOW_Q30, WINDOW_DFT_16384_Q22, WINDOW_DFT_HALF_RANGE,
};

/// `round(2 * pi * 2^30)`.
pub const TWO_PI_Q30: i64 = 6_746_518_852;

/// A complex number with `i64` parts; the scale is stated by whoever holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cplx {
    pub re: i64,
    pub im: i64,
}

impl Cplx {
    pub const ZERO: Cplx = Cplx { re: 0, im: 0 };

    /// `re^2 + im^2` (no rescaling; the caller tracks the scale).
    pub fn norm_sqr(self) -> i128 {
        self.re as i128 * self.re as i128 + self.im as i128 * self.im as i128
    }
}

/// `round(256 / (2 * pi) * 2^40)`: bins per radian, Q40.
const BINS_PER_RADIAN_Q40: i128 = 44_798_133_900_177;

/// `omega0 = 2*pi / (P8 / 8)` in radians per sample, Q30.
pub fn omega0_q30_from_p8(p8: u32) -> i32 {
    let num = TWO_PI_Q30 * 8;
    ((num + p8 as i64 / 2) / p8 as i64) as i32
}

/// `P8` of a half-sample pitch candidate index.
pub fn p8_of_index(idx: usize) -> u32 {
    (4 * p2_of_index(idx)) as u32
}

/// A fundamental frequency in the form the band arithmetic needs: `u = 256 * omega0 / (2 * pi)`, the
/// spacing of the harmonics in `S_w` bins, as an exact fraction `n / d`, plus `omega0` itself in Q30.
///
/// Every band edge is `a_l = (l - 1/2) u` and every window index `floor(64 (m - l u) + 1/2)`, so with
/// `u = n / d` both are exact integer divisions. From a period in eighths of a sample
/// ([`Pitch::from_p8`]) `u = 2048 / P8` is exact (no `pi` at all); from an arbitrary `omega0`
/// ([`Pitch::from_omega0_q30`]) `u = omega0 * (256 / 2 pi)` is formed with a Q40 constant, an
/// integer product with no rounding beyond that of `omega0` itself.
#[derive(Clone, Copy, Debug)]
pub struct Pitch {
    n: i128,
    d: i128,
    omega0_q30: i64,
}

impl Pitch {
    /// From a period of `p8 / 8` samples.
    pub fn from_p8(p8: u32) -> Self {
        Self {
            n: 2048,
            d: p8 as i128,
            omega0_q30: omega0_q30_from_p8(p8) as i64,
        }
    }

    /// From `omega0` in radians per sample, Q30 (must be positive).
    pub fn from_omega0_q30(omega0_q30: i64) -> Self {
        Self {
            n: omega0_q30 as i128 * BINS_PER_RADIAN_Q40,
            d: 1i128 << 70,
            omega0_q30,
        }
    }

    /// From `omega0` in radians per sample, Q16.16 (the decoders' own convention; only 16 fractional
    /// bits, so band edges that fall within about `l * u * 2^-17 / omega0` of an integer bin may
    /// round differently from a higher-precision `omega0`).
    pub fn from_omega0_q16(omega0_q16: i32) -> Self {
        Self::from_omega0_q30((omega0_q16 as i64) << 14)
    }

    /// `omega0` in radians per sample, Q30.
    pub fn omega0_q30(&self) -> i64 {
        self.omega0_q30
    }

    /// `ceil(a_l) = ceil((l - 1/2) u)`, which is also the (exclusive) upper bin of band `l - 1`.
    pub fn band_start(&self, l: i32) -> i32 {
        let num = (2 * l as i128 - 1) * self.n;
        -((-num).div_euclid(2 * self.d)) as i32
    }

    /// Index into `W_R` for bin `m` of harmonic `l`: `floor(64 (m - l u) + 1/2)`.
    pub fn window_index(&self, m: i32, l: i32) -> i32 {
        (128 * (m as i128 * self.d - l as i128 * self.n) + self.d).div_euclid(2 * self.d) as i32
    }

    /// `floor(0.9254 * pi / omega0 - 1/2)`, Eq. 24's harmonic limit (`pi / omega0 = 128 / u`).
    pub fn l_estimate(&self) -> i64 {
        (9254 * 256 * self.d - 10000 * self.n).div_euclid(20000 * self.n) as i64
    }

    /// `floor(l_est * u)`: the last bin Eq. 24 sums over.
    pub fn upper_bin(&self, l_est: i64) -> i32 {
        (l_est as i128 * self.n).div_euclid(self.d) as i32
    }

    /// `b_hat_0 = floor(4 pi / omega0 - 39)` (Eq. 45), clamped below at 0 like the float `as u32` cast
    /// (`4 pi / omega0 = 512 / u = 512 d / n`; exact integer arithmetic).
    pub fn quantizer_b0(&self) -> u32 {
        (512 * self.d - 39 * self.n)
            .div_euclid(self.n)
            .clamp(0, u32::MAX as i128) as u32
    }

    /// `L_hat = floor(0.9254 * floor(pi / omega0 + 1/4))` (Eq. 31).
    pub fn harmonics_count(&self) -> u32 {
        let inner = (512 * self.d + self.n).div_euclid(4 * self.n);
        (9254 * inner).div_euclid(10000) as u32
    }
}

/// The 256-point windowed DFT of one frame: `S_w(m)` for `m = -127..=128`, Q10.
pub struct RefinementFrame {
    sw: [Cplx; 256],
}

impl RefinementFrame {
    /// `raw` is PCM; the window is centred on `raw[center]` and reads `center-110..=center+110`.
    pub fn new(raw: &[i32], center: usize) -> Self {
        let mut xw = [0i64; 221];
        for (i, slot) in xw.iter_mut().enumerate() {
            let n = i as i32 - 110;
            let w = REFINEMENT_WINDOW_Q30[n.unsigned_abs() as usize] as i64;
            *slot = (raw[(center as i32 + n) as usize] as i64 * w + (1 << 19)) >> 20;
        }
        let mut sw = [Cplx::ZERO; 256];
        for m in 0..=128i32 {
            let mut re = 0i64;
            let mut im = 0i64;
            for (i, &x) in xw.iter().enumerate() {
                let n = i as i32 - 110;
                let k = (m * n).rem_euclid(256) as usize;
                re += x * COS_256_Q30[k] as i64;
                im -= x * COS_256_Q30[(k + 192) & 255] as i64;
            }
            let bin = Cplx {
                re: (re + (1 << 29)) >> 30,
                im: (im + (1 << 29)) >> 30,
            };
            sw[(m + 127) as usize] = bin;
            if (1..=127).contains(&m) {
                sw[(-m + 127) as usize] = Cplx {
                    re: bin.re,
                    im: -bin.im,
                };
            }
        }
        Self { sw }
    }

    /// `S_w(m)` (Q10), zero outside `-127..=128`.
    pub fn sw_at(&self, m: i32) -> Cplx {
        if (-127..=128).contains(&m) {
            self.sw[(m + 127) as usize]
        } else {
            Cplx::ZERO
        }
    }
}

/// `W_R(m)` (Q22), zero outside the table.
pub fn window_dft_16384_q22(m: i32) -> i64 {
    if m.abs() <= WINDOW_DFT_HALF_RANGE {
        WINDOW_DFT_16384_Q22[(m + WINDOW_DFT_HALF_RANGE) as usize] as i64
    } else {
        0
    }
}

/// `A_l` (Eq. 26-28) as a Q16 complex, from the bins `[ceil(a_l), ceil(b_l))`.
pub fn harmonic_amplitude_q16(frame: &RefinementFrame, l: u32, pitch: &Pitch) -> Cplx {
    let (lo, hi) = (pitch.band_start(l as i32), pitch.band_start(l as i32 + 1));
    let mut num_re = 0i128;
    let mut num_im = 0i128;
    let mut den = 0i128;
    for m in lo..hi {
        let wr = window_dft_16384_q22(pitch.window_index(m, l as i32)) as i128;
        let s = frame.sw_at(m);
        num_re += s.re as i128 * wr;
        num_im += s.im as i128 * wr;
        den += wr * wr;
    }
    if den == 0 {
        return Cplx::ZERO;
    }
    Cplx {
        re: ((num_re << 28) / den) as i64,
        im: ((num_im << 28) / den) as i64,
    }
}

/// Over the bins `m_start <= m < m_end`: returns `(err, real)` with `real = sum |S_w(m)|^2` and
/// `err = sum |S_w(m) - S_synth(m)|^2`, where the synthetic spectrum is built from harmonics
/// `0..=max_l` at period `p8` (Eq. 25; a bin in no harmonic's band has synthetic value 0). Both are
/// Q60 `i128`.
pub fn spectrum_error_and_energy(
    frame: &RefinementFrame,
    pitch: &Pitch,
    max_l: u32,
    m_start: i32,
    m_end: i32,
) -> (i128, i128) {
    let to_q30 = |c: Cplx| Cplx {
        re: c.re << 20,
        im: c.im << 20,
    };
    let mut real = 0i128;
    for m in m_start..m_end {
        real += to_q30(frame.sw_at(m)).norm_sqr();
    }
    let mut err = real;
    for l in 0..=max_l as i32 {
        let (lo, hi) = (pitch.band_start(l), pitch.band_start(l + 1));
        let (s, e) = (lo.max(m_start), hi.min(m_end));
        if s >= e {
            continue;
        }
        let amp = harmonic_amplitude_q16(frame, l as u32, pitch);
        for m in s..e {
            let wr = window_dft_16384_q22(pitch.window_index(m, l));
            let syn = Cplx {
                re: (amp.re * wr) >> 8,
                im: (amp.im * wr) >> 8,
            }; // Q16 * Q22 = Q38 -> Q30
            let real_bin = to_q30(frame.sw_at(m));
            let diff = Cplx {
                re: real_bin.re - syn.re,
                im: real_bin.im - syn.im,
            };
            err += diff.norm_sqr() - real_bin.norm_sqr();
        }
    }
    (err, real)
}

/// The refinement error `E_R` (Eq. 24) at `pitch`, Q60.
pub fn refinement_error(frame: &RefinementFrame, pitch: &Pitch) -> i128 {
    let l_est = pitch.l_estimate();
    let upper_m = pitch.upper_bin(l_est);
    let max_l = l_est.max(0) as u32 + 1;
    spectrum_error_and_energy(frame, pitch, max_l, 50, upper_m + 1).0
}

/// Refines the half-sample candidate `p_hat_i_index` (see [`super::pitch`]) to quarter-sample
/// accuracy: tries `P = p_hat_i + {-9,-7,...,+9}/8` and returns the `P8` with the smallest `E_R`
/// (first of equals, as the float sibling's `min_by`).
pub fn refine_pitch(frame: &RefinementFrame, p_hat_i_index: usize) -> u32 {
    let base = p8_of_index(p_hat_i_index) as i32;
    let mut best_p8 = 0u32;
    let mut best_err = i128::MAX;
    for off in [-9, -7, -5, -3, -1, 1, 3, 5, 7, 9] {
        let p8 = (base + off) as u32;
        let e = refinement_error(frame, &Pitch::from_p8(p8));
        if e < best_err {
            best_err = e;
            best_p8 = p8;
        }
    }
    best_p8
}
