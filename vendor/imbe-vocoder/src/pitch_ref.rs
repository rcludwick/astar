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

//! Pitch refinement: tries 19 periods around the initial estimate, in
//! eighth-sample steps, and keeps the one whose harmonic model fits the
//! spectrum best. Port of `pitch_ref.cc`.

use crate::ImbeParam;
use crate::basic_op::*;
use crate::dsp_sub::FFTLENGTH;
use crate::tables::WR_SP;

/// The lowest FFT bin the fit is measured from.
const MIN_INDEX: i16 = 50;

/// 0.9254 in unsigned Q0.16.
const CNST_0_9254_Q0_16: u32 = 60647;
const CNST_1_125_Q8_8: i16 = 0x0120;
const CNST_0_5_Q8_8: i16 = 0x0080;
const CNST_0_125_Q8_8: i16 = 0x0020;

/// Sets `p.ref_pitch` (Q8.8) and `p.fund_freq` from `p.pitch` and the
/// windowed speech spectrum `fft_buf` (interleaved re, im).
pub fn pitch_ref(p: &mut ImbeParam, fft_buf: &[i16]) {
    let re = |k: i16| fft_buf[2 * k as usize];
    let im = |k: i16| fft_buf[2 * k as usize + 1];

    // The reconstructed spectrum. Upstream sizes it FFTLENGTH/2 and leaves
    // it uninitialised; every bin the fit reads is written first. It is
    // full length and zeroed here so a short period near the top of the
    // range cannot index past it.
    let mut sp_rec_re = [0i16; FFTLENGTH];
    let mut sp_rec_im = [0i16; FFTLENGTH];
    // Upstream's index_tbl[20]; a harmonic spans at most 13 bins.
    let mut index_tbl = [0i16; FFTLENGTH];

    let mut pitch_est = shl(p.pitch, 7); // Q8.8
    pitch_est = sub(pitch_est, CNST_1_125_Q8_8); // minus 9/8

    let mut l_diff_min = MAX_32;
    let mut pitch_cand: i16 = 0;
    let mut fund_freq_cand: i32 = 0;

    for _ in 0..19 {
        let shift = norm_s(pitch_est);
        let tmp = shl(pitch_est, shift);
        let tmp = div_s(0x4000, tmp);
        let fund_freq = l_shl(i32::from(tmp), shift + 11);

        let mut fund_freq_acc = fund_freq;
        let fund_freq_2 = l_shr(fund_freq, 1);
        let mut fund_freq_acc_a = l_sub(fund_freq, fund_freq_2);
        let mut fund_freq_acc_b = l_add(fund_freq, fund_freq_2);

        // The upper limit of the spectrum reconstruction: 0.9254/fund_freq,
        // minus 0.5, its integer part, times fund_freq.
        let product = CNST_0_9254_Q0_16.wrapping_mul(pitch_est as i32 as u32) as i32;
        let mut up_lim = extract_h(l_shr(product, 1));
        up_lim = sub(up_lim, CNST_0_5_Q8_8);
        up_lim &= 0xFF00u16 as i16;
        up_lim = mult(up_lim, extract_h(fund_freq));
        up_lim = shr(up_lim, 1);

        let mut index_b: i16 = 0;
        while index_b <= up_lim {
            let ha = extract_h(fund_freq_acc_a);
            let hb = extract_h(fund_freq_acc_b);
            let mut index_a = (ha >> 8) + i16::from(ha & 0xFF != 0);
            index_b = (hb >> 8) + i16::from(hb & 0xFF != 0);

            if index_b >= MIN_INDEX {
                let mut l_tmp = l_shl(l_deposit_h(index_a), 8);
                l_tmp = l_sub(l_tmp, fund_freq_acc);
                l_tmp = l_add(l_tmp, 0x0002_0000); // for rounding
                l_tmp = l_shr(l_tmp, 2);

                let index_a_save = index_a;
                let mut it_ind = 0usize;

                let mut amp_re_acc: i32 = 0;
                let mut amp_im_acc: i32 = 0;
                while index_a < index_b {
                    let mut index_wr = extract_h(l_tmp);
                    if index_wr < 0 && (l_tmp & 0xFFFF) != 0 {
                        index_wr = add(index_wr, 1); // truncate toward zero
                    }
                    index_wr = add(index_wr, 160);
                    index_tbl[it_ind] = index_wr;
                    it_ind += 1;
                    if (0..=320).contains(&index_wr) {
                        let w = WR_SP[index_wr as usize];
                        amp_re_acc = l_mac(amp_re_acc, re(index_a), w);
                        amp_im_acc = l_mac(amp_im_acc, im(index_a), w);
                    }

                    index_a += 1;
                    l_tmp = l_add(l_tmp, 0x0040_0000);
                }

                it_ind = 0;
                index_a = index_a_save;
                while index_a < index_b {
                    let index_wr = index_tbl[it_ind];
                    it_ind += 1;
                    let k = index_a as usize;
                    if !(0..=320).contains(&index_wr) {
                        sp_rec_im[k] = 0;
                        sp_rec_re[k] = 0;
                    } else {
                        let w = WR_SP[index_wr as usize];
                        sp_rec_im[k] = mult(mult(extract_h(amp_im_acc), w), 0x6666);
                        sp_rec_re[k] = mult(mult(extract_h(amp_re_acc), w), 0x6666);
                    }
                    index_a += 1;
                }
            }

            fund_freq_acc_a = l_add(fund_freq_acc_a, fund_freq);
            fund_freq_acc_b = l_add(fund_freq_acc_b, fund_freq);
            fund_freq_acc = l_add(fund_freq_acc, fund_freq);
        }

        let mut l_sum: i32 = 0;
        for j in MIN_INDEX..=up_lim {
            let re_tmp = sub(re(j), sp_rec_re[j as usize]);
            let im_tmp = sub(im(j), sp_rec_im[j as usize]);
            l_sum = l_mac(l_sum, re_tmp, re_tmp);
            l_sum = l_mac(l_sum, im_tmp, im_tmp);
        }

        if l_sum < l_diff_min {
            l_diff_min = l_sum;
            pitch_cand = pitch_est;
            fund_freq_cand = fund_freq;
        }

        pitch_est = add(pitch_est, CNST_0_125_Q8_8); // plus 1/8
    }

    p.ref_pitch = pitch_cand;
    p.fund_freq = fund_freq_cand;
}
