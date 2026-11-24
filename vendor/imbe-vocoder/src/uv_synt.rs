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

//! Unvoiced speech synthesis, port of `uv_synt.cc`: shaped noise in the
//! unvoiced bands, an inverse FFT, and a weighted overlap-add with the
//! previous frame.

use crate::basic_op::*;
use crate::dsp_sub::{FFTLENGTH, Fft};
use crate::rand_gen::RandGen;
use crate::tables::WS;
use crate::{FRAME, ImbeParam};

/// The unvoiced synthesiser's overlap memory.
#[derive(Clone, Debug)]
pub struct UvSynt {
    uv_mem: [i16; 105],
}

impl UvSynt {
    /// Upstream's `uv_synt_init` (whose `fft_init` is [`Fft::new`]).
    pub fn new() -> Self {
        UvSynt { uv_mem: [0; 105] }
    }

    /// Synthesises the unvoiced part of one frame.
    pub fn uv_synt(&mut self, p: &ImbeParam, rng: &mut RandGen, fft: &Fft) -> [i16; FRAME] {
        // Complex spectrum, interleaved re, im.
        let mut uw = [0i16; 2 * FFTLENGTH];
        let re = |k: i16| 2 * k as usize;
        let im = |k: i16| 2 * k as usize + 1;

        let fund_freq = p.fund_freq;
        let fund_freq_2 = l_shr(fund_freq, 1);

        let mut fund_freq_acc_a = l_sub(fund_freq, fund_freq_2);
        let mut fund_freq_acc_b = l_add(fund_freq, fund_freq_2);

        for i in 0..p.num_harms as usize {
            let ha = extract_h(fund_freq_acc_a);
            let hb = extract_h(fund_freq_acc_b);
            let mut index_a = (ha >> 8) + i16::from(ha & 0xFF != 0);
            let index_b = (hb >> 8) + i16::from(hb & 0xFF != 0);

            let mut index_aux = 256 - index_a;

            let sa = shl(p.sa[i], 3);

            if p.v_uv_dsn[i] != 0 {
                while index_a < index_b {
                    uw[re(index_a)] = 0;
                    uw[im(index_a)] = 0;
                    uw[re(index_aux)] = 0;
                    uw[im(index_aux)] = 0;
                    index_a += 1;
                    index_aux -= 1;
                }
            } else {
                while index_a < index_b {
                    let r = mult(sa, rng.rand_gen());
                    let m = mult(sa, rng.rand_gen());
                    uw[re(index_a)] = r;
                    uw[im(index_a)] = m;
                    uw[re(index_aux)] = r;
                    uw[im(index_aux)] = negate(m);
                    index_a += 1;
                    index_aux -= 1;
                }
            }

            fund_freq_acc_a = l_add(fund_freq_acc_a, fund_freq);
            fund_freq_acc_b = l_add(fund_freq_acc_b, fund_freq);
        }

        fft.fft(&mut uw, FFTLENGTH, -1);

        let mut snd = [0i16; FRAME];
        snd[..105].copy_from_slice(&self.uv_mem);

        for (k, s) in snd[105..].iter_mut().enumerate() {
            *s = shl(uw[re(73 + k as i16)], 3);
        }

        // Weighted overlap-add.
        for (k, i) in (56..105).enumerate() {
            let a = l_mult(snd[i], WS[48 - k]);
            let b = l_mult(shl(uw[re(24 + k as i16)], 3), WS[k]);
            snd[i] = extract_h(l_add(a, b));
        }

        for (k, m) in self.uv_mem.iter_mut().enumerate() {
            *m = shl(uw[re(128 + k as i16)], 3);
        }

        snd
    }
}
