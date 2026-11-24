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

//! The IMBE decoder: one 88-bit frame in, 20 ms of 8 kHz audio out. Port
//! of `decode.cc`, with the decoder's share of upstream's `imbe_vocoder`
//! class state gathered in [`ImbeDecoder`].

use crate::basic_op::add;
use crate::ch_decode::{decode_frame_vector, v_uv_decode};
use crate::dsp_sub::Fft;
use crate::rand_gen::RandGen;
use crate::sa_decode::SaDecoder;
use crate::sa_enh::sa_enh;
use crate::uv_synt::UvSynt;
use crate::v_synt::VSynt;
use crate::{FRAME_BYTES, FRAME_SAMPLES, ImbeParam, frame};

/// A P25 Phase 1 full-rate IMBE decoder.
///
/// It keeps state from frame to frame (phases, the previous spectrum, the
/// overlap-add tail and the noise generator), so give each stream of frames
/// its own decoder and feed it every frame in order.
#[derive(Clone, Debug)]
pub struct ImbeDecoder {
    param: ImbeParam,
    rng: RandGen,
    fft: Fft,
    sa_dec: SaDecoder,
    v_synt: VSynt,
    uv_synt: UvSynt,
}

impl ImbeDecoder {
    /// A decoder in the state upstream's `decode_init` leaves it.
    pub fn new() -> ImbeDecoder {
        let mut rng = RandGen::new();
        // Upstream's order: v_synt_init, uv_synt_init, sa_decode_init. Only
        // the first draws from the generator.
        let v_synt = VSynt::new(&mut rng);
        let uv_synt = UvSynt::new();
        let fft = Fft::new();
        let sa_dec = SaDecoder::new();

        // The "previous frame" the first frame is decoded against.
        let param = ImbeParam {
            fund_freq: 0x0cf6_474a,
            num_harms: 9,
            num_bands: 3,
            ..ImbeParam::default()
        };

        ImbeDecoder {
            param,
            rng,
            fft,
            sa_dec,
            v_synt,
            uv_synt,
        }
    }

    /// Decodes one 11-byte frame (the MMDVMHost packing, see [`frame`]) into
    /// 160 samples of 8 kHz, 16-bit audio.
    pub fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; FRAME_SAMPLES] {
        self.decode_vector(&frame::unpack(frame))
    }

    /// Upstream's `decode`: the code vectors û0..û7 in, audio out.
    fn decode_vector(&mut self, frame_vector: &[i16; 8]) -> [i16; FRAME_SAMPLES] {
        let p = &mut self.param;
        decode_frame_vector(p, frame_vector);
        v_uv_decode(p);
        self.sa_dec.sa_decode(p);
        sa_enh(p);
        let mut snd = self.v_synt.v_synt(p, &mut self.rng);
        let snd_tmp = self.uv_synt.uv_synt(p, &mut self.rng, &self.fft);

        for (s, &t) in snd.iter_mut().zip(&snd_tmp) {
            *s = add(*s, t);
        }
        snd
    }
}

impl Default for ImbeDecoder {
    fn default() -> Self {
        Self::new()
    }
}
