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

//! Voiced/unvoiced determination and spectral amplitude estimation. Port of
//! `v_uv_det.cc`.

use crate::basic_op::*;
use crate::dsp_sub::FFTLENGTH;
use crate::math_sub::{l_mpy_ls, sqrt_l_exp};
use crate::tables::WR_SP;
use crate::{ImbeParam, NUM_BANDS_MAX, NUM_HARMS_MAX, NUM_HARMS_MIN};

const CNST_0_5625_Q1_15: i16 = 0x4800;
const CNST_0_45_Q1_15: i16 = 0x3999;
const CNST_0_1741_Q1_15: i16 = 0x164A;
const CNST_0_1393_Q1_15: i16 = 0x11D5;
const CNST_0_99_Q1_15: i16 = 0x7EB8;
const CNST_0_01_Q1_15: i16 = 0x0148;
const CNST_0_0025_Q1_15: i16 = 0x0051;
const CNST_0_25_Q1_15: i16 = 0x2000;
const CNST_PI_4_Q1_15: i16 = 0x6488;
const CNST_0_55_Q4_12: i16 = 0x08CD;
const CNST_0_25_Q8_8: i16 = 0x0040;
/// 0.9254 in unsigned Q0.16.
const CNST_0_9254_Q0_16: u32 = 60647;
/// 1/3 in unsigned Q0.16.
const CNST_0_33_Q0_16: u32 = 0x5556;

/// The detector's memory: last frame's per-band decisions and the
/// spectrum's running peak energy.
#[derive(Clone, Debug)]
pub struct VUvDet {
    v_uv_dsn: [i16; NUM_BANDS_MAX],
    th_max: i32,
}

/// A voiced harmonic's amplitude from its energy and window sum.
fn voiced_sa_calc(num: i32, den: i16) -> i16 {
    let l_tmp = l_mpy_ls(num, den);
    let (l_tmp, tmp) = sqrt_l_exp(l_tmp);
    extract_h(l_shr(l_tmp, tmp - 3))
}

/// An unvoiced harmonic's amplitude from its energy and width in bins.
fn unvoiced_sa_calc(num: i32, den: i16) -> i16 {
    let shift = norm_s(den);
    let tmp = div_s(0x4000, shl(den, shift));
    let l_tmp = l_shl(l_mpy_ls(num, tmp), shift + 2);
    let (l_tmp, tmp) = sqrt_l_exp(l_tmp);
    let l_tmp = l_shr(l_tmp, tmp - 2 + 6);
    extract_h(l_mpy_ls(l_tmp, 0x4A76))
}

impl VUvDet {
    /// Upstream's `pitch_ref_init`, which despite its name sets up this
    /// module's state.
    pub fn new() -> Self {
        VUvDet {
            v_uv_dsn: [0; NUM_BANDS_MAX],
            th_max: 0,
        }
    }

    /// Sets `num_harms`, `num_bands`, `sa`, `v_uv_dsn`, `l_uv`, `b_vec[0]`
    /// (the pitch) and `b_vec[1]` (the band decisions) from the refined
    /// pitch and the spectrum `fft_buf` (interleaved re, im).
    pub fn v_uv_det(&mut self, p: &mut ImbeParam, fft_buf: &[i16]) {
        let re = |k: i16| fft_buf[2 * k as usize];
        let im = |k: i16| fft_buf[2 * k as usize + 1];

        // Upstream's index_tbl[30]; a harmonic spans at most 13 bins.
        let mut index_tbl = [0i16; FFTLENGTH];
        let mut m_num = [0i32; NUM_HARMS_MAX];
        let mut m_den = [0i16; NUM_HARMS_MAX];

        let fund_freq = p.fund_freq;

        // fix(pitch_cand / 2 + 0.5)
        let tmp = shr(add(shr(p.ref_pitch, 1), CNST_0_25_Q8_8), 8);
        // fix(0.9254 * fix(pitch_cand / 2 + 0.5))
        let mut num_harms = extract_h(CNST_0_9254_Q0_16.wrapping_mul(tmp as i32 as u32) as i32);
        num_harms = num_harms.clamp(NUM_HARMS_MIN, NUM_HARMS_MAX as i16);

        let num_bands = if num_harms <= 36 {
            // fix((L + 2) / 3)
            extract_h(((num_harms as i32 + 2) as u32).wrapping_mul(CNST_0_33_Q0_16) as i32)
        } else {
            NUM_BANDS_MAX as i16
        };

        p.num_harms = num_harms;
        p.num_bands = num_bands;

        // The M(th) function.
        let mut th_lf: i32 = 0;
        for j in 0..64 {
            th_lf = l_mac(th_lf, re(j), re(j));
            th_lf = l_mac(th_lf, im(j), im(j));
        }
        let mut th_hf: i32 = 0;
        for j in 64..128 {
            th_hf = l_mac(th_hf, re(j), re(j));
            th_hf = l_mac(th_hf, im(j), im(j));
        }
        let th0 = l_add(th_lf, th_hf);

        if th0 > self.th_max {
            self.th_max = l_shr(l_add(self.th_max, th0), 1);
        } else {
            self.th_max = l_add(
                l_mpy_ls(self.th_max, CNST_0_99_Q1_15),
                l_mpy_ls(th0, CNST_0_01_Q1_15),
            );
        }

        let mut m_fcn_num = l_add(th0, l_mpy_ls(self.th_max, CNST_0_0025_Q1_15));
        let mut m_fcn_den = l_add(th0, l_mpy_ls(self.th_max, CNST_0_01_Q1_15));
        let m_fcn = if m_fcn_den == 0 {
            CNST_0_25_Q1_15
        } else {
            let tmp = norm_l(m_fcn_den);
            m_fcn_den = l_shl(m_fcn_den, tmp);
            m_fcn_num = l_shl(m_fcn_num, tmp);

            let mut m_fcn = div_s(extract_h(m_fcn_num), extract_h(m_fcn_den));

            let l_tmp = l_add(l_shl(th_hf, 2), th_hf);
            if th_lf < l_tmp {
                // th_lf < 5 * th_hf
                let tmp = norm_l(l_tmp);
                let den = l_shl(l_tmp, tmp);
                let lf = l_shl(th_lf, tmp);

                let q = div_s(extract_h(lf), extract_h(den));
                let (mut l_tmp, sh) = sqrt_l_exp(l_deposit_h(q));
                if sh != 0 {
                    l_tmp = l_shr(l_tmp, sh);
                }
                m_fcn = mult(m_fcn, extract_h(l_tmp));
            }
            m_fcn
        };

        // ---------------------------------------------------------------
        let fund_fr_step = l_shl(l_mpy_ls(fund_freq, CNST_PI_4_Q1_15), 2); // times pi

        let mut uv_harms_cnt: i16 = 0;
        let mut b1_vec: i16 = 0;
        let mut band_cnt: usize = 0;
        let mut num_harms_cnt: i16 = 0;
        let mut d_num: i32 = 0;
        let mut d_den: i32 = 0;
        let mut dsn_thr: i16 = 0;

        let mut fund_fr_acc: i32 = 0;
        let mut fund_freq_acc = fund_freq;
        let fund_freq_2 = l_shr(fund_freq, 1);
        let mut fund_freq_acc_a = l_sub(fund_freq, fund_freq_2);
        let mut fund_freq_acc_b = l_add(fund_freq, fund_freq_2);

        // Upstream reads these two after the loop, from its last harmonic.
        let mut index_a_save: i16 = 0;
        let mut index_b: i16 = 0;

        for j in 0..num_harms as usize {
            let ha = extract_h(fund_freq_acc_a);
            let hb = extract_h(fund_freq_acc_b);
            let mut index_a = (ha >> 8) + i16::from(ha & 0xFF != 0);
            index_b = (hb >> 8) + i16::from(hb & 0xFF != 0);

            let mut l_tmp = l_shl(l_deposit_h(index_a), 8);
            l_tmp = l_sub(l_tmp, fund_freq_acc);
            l_tmp = l_add(l_tmp, 0x0002_0000); // for rounding
            l_tmp = l_shr(l_tmp, 2);

            index_a_save = index_a;
            let mut it_ind = 0usize;

            // The v/uv threshold, once per band.
            if num_harms_cnt == 0 {
                dsn_thr = if p.e_p > CNST_0_55_Q4_12 && band_cnt >= 1 {
                    0
                } else if self.v_uv_dsn[band_cnt] == 1 {
                    mult(
                        m_fcn,
                        sub(
                            CNST_0_5625_Q1_15,
                            mult(CNST_0_1741_Q1_15, extract_h(fund_fr_acc)),
                        ),
                    )
                } else {
                    mult(
                        m_fcn,
                        sub(
                            CNST_0_45_Q1_15,
                            mult(CNST_0_1393_Q1_15, extract_h(fund_fr_acc)),
                        ),
                    )
                };
                fund_fr_acc = l_add(fund_fr_acc, fund_fr_step);
            }

            let mut m_den_sum: i32 = 0;
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
                    m_den_sum = l_add(m_den_sum, i32::from(mult(w, w)));
                }

                index_a += 1;
                l_tmp = l_add(l_tmp, 0x0040_0000);
            }
            let sc_coef = div_s(0x4000, extract_l(l_shr(m_den_sum, 1)));
            let im_tmp2 = mult(extract_h(amp_im_acc), sc_coef);
            let re_tmp2 = mult(extract_h(amp_re_acc), sc_coef);

            let mut m_num_sum: i32 = 0;
            it_ind = 0;
            index_a = index_a_save;
            while index_a < index_b {
                let index_wr = index_tbl[it_ind];
                it_ind += 1;
                let (sp_rec_re, sp_rec_im) = if !(0..=320).contains(&index_wr) {
                    (0, 0)
                } else {
                    let w = WR_SP[index_wr as usize];
                    (mult(re_tmp2, w), mult(im_tmp2, w))
                };

                let re_tmp = sub(re(index_a), sp_rec_re);
                let im_tmp = sub(im(index_a), sp_rec_im);
                d_num = l_mac(d_num, re_tmp, re_tmp);
                d_num = l_mac(d_num, im_tmp, im_tmp);

                m_num_sum = l_mac(m_num_sum, re(index_a), re(index_a));
                m_num_sum = l_mac(m_num_sum, im(index_a), im(index_a));

                index_a += 1;
            }

            m_den[j] = sc_coef;
            m_num[j] = m_num_sum;
            d_den = l_add(d_den, m_num_sum);

            num_harms_cnt += 1;
            if num_harms_cnt == 3 && (band_cnt as i16) < num_bands - 1 {
                b1_vec <<= 1;
                let voiced = dk(d_num, d_den) < dsn_thr;
                self.v_uv_dsn[band_cnt] = i16::from(voiced);
                if voiced {
                    b1_vec |= 1;
                }
                for i in j - 2..=j {
                    if voiced {
                        p.sa[i] = voiced_sa_calc(m_num[i], m_den[i]);
                    } else {
                        p.sa[i] = unvoiced_sa_calc(m_num[i], index_b - index_a_save);
                        uv_harms_cnt += 1;
                    }
                    p.v_uv_dsn[i] = i16::from(voiced);
                }

                d_num = 0;
                d_den = 0;
                num_harms_cnt = 0;
                band_cnt += 1;
            }

            fund_freq_acc_a = l_add(fund_freq_acc_a, fund_freq);
            fund_freq_acc_b = l_add(fund_freq_acc_b, fund_freq);
            fund_freq_acc = l_add(fund_freq_acc, fund_freq);
        }

        // The last band takes the harmonics left over.
        if num_harms_cnt != 0 {
            b1_vec <<= 1;
            let voiced = dk(d_num, d_den) < dsn_thr;
            self.v_uv_dsn[band_cnt] = i16::from(voiced);
            if voiced {
                b1_vec |= 1;
            }
            for i in (num_harms - num_harms_cnt) as usize..num_harms as usize {
                if voiced {
                    p.sa[i] = voiced_sa_calc(m_num[i], m_den[i]);
                } else {
                    p.sa[i] = unvoiced_sa_calc(m_num[i], index_b - index_a_save);
                    uv_harms_cnt += 1;
                }
                p.v_uv_dsn[i] = i16::from(voiced);
            }
        }

        p.l_uv = uv_harms_cnt;

        p.b_vec[1] = b1_vec; // the voiced/unvoiced decisions
        p.b_vec[0] = shr(sub(p.ref_pitch, 0x1380), 7); // the pitch, fix(2 * pitch - 39)
    }
}

/// The band's normalised fit error D_num / D_den in Q15, 1.0 when the model
/// does not fit at all.
fn dk(d_num: i32, d_den: i32) -> i16 {
    if d_den > d_num && d_den != 0 {
        let tmp = norm_l(d_den);
        div_s(extract_h(l_shl(d_num, tmp)), extract_h(l_shl(d_den, tmp)))
    } else {
        MAX_16
    }
}
