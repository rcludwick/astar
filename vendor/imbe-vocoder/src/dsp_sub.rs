// Project 25 IMBE Encoder/Decoder Fixed-Point implementation
// Developed by Pavel Yazev E-mail: pyazev@gmail.com
// Version 1.0 (c) Copyright 2009
//
// Rust port (c) 2026 Rob Ludwick
//
// This is free software; you can redistribute it and/or modify it under the
// terms of the GNU General Public License as published by the Free Software
// Foundation; either version 3, or (at your option) any later version. It is
// distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
// PARTICULAR PURPOSE. See the GNU General Public License (COPYING) for more
// details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! DCT, inverse DCT and the fixed-point FFT, port of `dsp_sub.cc`.

use crate::basic_op::*;
use crate::math_sub::{ONE_Q15, cos_fxp, l_mpy_ls, sin_fxp};

/// 0.5 in Q1.15.
const CNST_0_5_Q1_15: u16 = 0x4000;
/// 0.5 in Q5.11.
const CNST_0_5_Q5_11: i16 = 0x0400;
/// 1.0 (as 32767) in Q1.15.
const CNST_1_0_Q1_15: u16 = 0x7FFF;

/// FFT length used by the vocoder.
pub const FFTLENGTH: usize = 256;

/// The angle step 0.5/`m_lim` and twice it, both in Q1.15.
fn angle_steps(m_lim: usize) -> (u16, u16) {
    if m_lim == 1 {
        (CNST_0_5_Q1_15, CNST_1_0_Q1_15)
    } else {
        let intl = div_s(CNST_0_5_Q5_11, (m_lim as i16) << 11) as u16; // 0.5/m_lim
        (intl, shl(intl as i16, 1) as u16)
    }
}

/// Inverse DCT of the first `m_lim` entries of `input` into the first
/// `i_lim` entries of `out`.
pub fn idct(input: &[i16], m_lim: usize, i_lim: usize, out: &mut [i16]) {
    let (angl_intl, angl_intl_2) = angle_steps(m_lim);

    let mut angl_step = angl_intl;
    for o in out[..i_lim].iter_mut() {
        let mut sum: i32 = 0;
        let mut angl_acc = angl_step;
        for &x in &input[1..m_lim] {
            sum = l_add(sum, l_shr(l_mult(x, cos_fxp(angl_acc as i16)), 7));
            angl_acc = angl_acc.wrapping_add(angl_step);
        }
        sum = l_add(sum, l_shr(l_deposit_h(input[0]), 8));
        *o = extract_l(l_shr_r(sum, 8));
        angl_step = angl_step.wrapping_add(angl_intl_2);
    }
}

/// DCT of the first `m_lim` entries of `input` into the first `i_lim`
/// entries of `out`.
pub fn dct(input: &[i16], m_lim: usize, i_lim: usize, out: &mut [i16]) {
    let (angl_intl, angl_intl_2) = angle_steps(m_lim);

    // The first coefficient.
    let mut sum: i32 = 0;
    for &x in &input[..m_lim] {
        sum = l_add(sum, l_deposit_l(x));
    }
    out[0] = extract_l(l_mpy_ls(sum, angl_intl_2 as i16));

    // The others.
    let mut angl_begin = angl_intl;
    let mut angl_step = angl_intl_2;
    for o in out[1..i_lim].iter_mut() {
        let mut sum: i32 = 0;
        let mut angl_acc = angl_begin;
        for &x in &input[..m_lim] {
            sum = l_add(sum, l_deposit_l(mult(x, cos_fxp(angl_acc as i16))));
            angl_acc = angl_acc.wrapping_add(angl_step);
        }
        *o = extract_l(l_mpy_ls(sum, angl_intl_2 as i16));

        angl_step = angl_step.wrapping_add(angl_intl_2);
        angl_begin = angl_begin.wrapping_add(angl_intl);
    }
}

/// The FFT's twiddle factors, built by upstream's `fft_init`.
#[derive(Clone, Debug)]
pub struct Fft {
    wr_array: [i16; FFTLENGTH / 2 + 1],
    wi_array: [i16; FFTLENGTH / 2 + 1],
}

impl Fft {
    /// Upstream's `fft_init`.
    pub fn new() -> Self {
        let mut wr_array = [0i16; FFTLENGTH / 2 + 1];
        let mut wi_array = [0i16; FFTLENGTH / 2 + 1];
        let fft_len2 = shr(FFTLENGTH as i16, 1);
        let shift = norm_s(fft_len2);
        let step = shl(2, shift);
        let mut theta: i16 = 0;

        for i in 0..=fft_len2 {
            wr_array[i as usize] = cos_fxp(theta);
            wi_array[i as usize] = sin_fxp(theta);
            if i >= fft_len2 - 1 {
                theta = ONE_Q15;
            } else {
                theta = add(theta, step);
            }
        }
        Fft { wr_array, wi_array }
    }

    /// In-place complex FFT of `nn` points, `data` interleaved as re, im,
    /// re, im. `isign` 1 is the forward transform, -1 the inverse (times
    /// `nn`), in the e^{+jwt} sign convention. Upstream's `fft`, which is
    /// written with 1-based indices; `d(k)` below is its `data[k]`.
    pub fn fft(&self, data: &mut [i16], nn: usize, isign: i16) {
        let n = 2 * nn;
        // 1-based index into `data`.
        let d = |k: usize| k - 1;

        // Bit-reversal reordering.
        let mut j = 1usize;
        let mut i = 1usize;
        while i < n {
            if j > i {
                data.swap(d(j), d(i));
                data.swap(d(j + 1), d(i + 1));
            }
            let mut m = nn;
            while m >= 2 && j > m {
                j -= m;
                m >>= 1;
            }
            j += m;
            i += 2;
        }

        let mut mmax = 2usize;
        let mut index_step = nn;

        while n > mmax {
            let istep = 2 * mmax;
            let mut index = 0usize;
            index_step >>= 1;

            let mut wr = ONE_Q15;
            let mut wi: i16 = 0;
            let mut m = 1usize;
            while m < mmax {
                let mut i = m;
                while i <= n {
                    let j = i + mmax;

                    // tempr = wr * data[j] - wi * data[j+1]
                    let l_temp1 = l_shr(l_mult(wr, data[d(j)]), 1);
                    let l_temp2 = l_shr(l_mult(wi, data[d(j + 1)]), 1);
                    let l_tempr = l_sub(l_temp1, l_temp2);

                    // tempi = wr * data[j+1] + wi * data[j]
                    let l_temp1 = l_shr(l_mult(wr, data[d(j + 1)]), 1);
                    let l_temp2 = l_shr(l_mult(wi, data[d(j)]), 1);
                    let l_tempi = l_add(l_temp1, l_temp2);

                    // data[j] = data[i] - tempr; data[i] += tempr
                    let l_temp1 = l_shr(l_deposit_h(data[d(i)]), 1);
                    data[d(j)] = round(l_sub(l_temp1, l_tempr));
                    data[d(i)] = round(l_add(l_temp1, l_tempr));

                    // data[j+1] = data[i+1] - tempi; data[i+1] += tempi
                    let l_temp1 = l_shr(l_deposit_h(data[d(i + 1)]), 1);
                    data[d(j + 1)] = round(l_sub(l_temp1, l_tempi));
                    data[d(i + 1)] = round(l_add(l_temp1, l_tempi));

                    i += istep;
                }
                index += index_step;
                wr = self.wr_array[index];
                wi = if isign < 0 {
                    negate(self.wi_array[index])
                } else {
                    self.wi_array[index]
                };
                m += 2;
            }
            mmax = istep;
        }
    }
}
