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

//! P25 Phase 1 full-rate IMBE vocoder.
//!
//! A bit-exact Rust port of Pavel Yazev's fixed-point `imbe_vocoder` from
//! OP25 (see `VENDORED.md`), checked frame by frame against golden vectors
//! from the C++ original, encoder and decoder both.
//!
//! ```
//! use imbe_vocoder::{FRAME_SAMPLES, ImbeDecoder, ImbeEncoder};
//!
//! let mut dec = ImbeDecoder::new();
//! let null = [0x04, 0x0C, 0xFD, 0x7B, 0xFB, 0x7D, 0xF2, 0x7B, 0x3D, 0x9E, 0x45];
//! let pcm: [i16; FRAME_SAMPLES] = dec.decode(&null);
//! assert_eq!(pcm.len(), 160);
//!
//! let mut enc = ImbeEncoder::new();
//! let frame: [u8; 11] = enc.encode(&[0; FRAME_SAMPLES]);
//! let _ = dec.decode(&frame);
//! ```

mod aux_sub;
mod basic_op;
mod ch_decode;
mod ch_encode;
mod dc_rmv;
mod decode;
mod dsp_sub;
mod encode;
pub mod frame;
mod math_sub;
mod pe_lpf;
mod pitch_est;
mod pitch_ref;
mod qnt_sub;
mod rand_gen;
mod sa_decode;
mod sa_encode;
mod sa_enh;
mod tables;
mod uv_synt;
mod v_synt;
mod v_uv_det;

pub use decode::ImbeDecoder;
pub use encode::ImbeEncoder;

/// Bytes in one packed IMBE frame (88 bits).
pub const FRAME_BYTES: usize = 11;
/// Samples of 8 kHz audio in one frame (20 ms).
pub const FRAME_SAMPLES: usize = 160;

// Upstream's `imbe.h`.

/// Samples per frame.
pub(crate) const FRAME: usize = FRAME_SAMPLES;
/// Most harmonics a frame can carry.
pub(crate) const NUM_HARMS_MAX: usize = 56;
/// Fewest harmonics a frame can carry.
pub(crate) const NUM_HARMS_MIN: i16 = 9;
/// Most voiced/unvoiced bands.
pub(crate) const NUM_BANDS_MAX: usize = 12;
/// Longest block in the spectral amplitude coding.
pub(crate) const MAX_BLOCK_LEN: usize = 10;
/// Prediction residual blocks.
pub(crate) const NUM_PRED_RES_BLKS: usize = 6;
/// Entries in the parameter vector b beyond b0..b2.
pub(crate) const B_NUM: usize = NUM_HARMS_MAX - 1;

/// The model parameters of one frame: upstream's `IMBE_PARAM`.
#[derive(Clone, Debug)]
pub(crate) struct ImbeParam {
    pub e_p: i16,
    /// Q14.2.
    pub pitch: i16,
    /// Q8.8.
    pub ref_pitch: i16,
    pub fund_freq: i32,
    pub num_harms: i16,
    pub num_bands: i16,
    pub v_uv_dsn: [i16; NUM_HARMS_MAX],
    pub b_vec: [i16; NUM_HARMS_MAX + 3],
    pub bit_alloc: [i16; B_NUM + 4],
    pub sa: [i16; NUM_HARMS_MAX],
    pub l_uv: i16,
    pub div_one_by_num_harm: i16,
    pub div_one_by_num_harm_sh: i16,
}

impl Default for ImbeParam {
    fn default() -> Self {
        ImbeParam {
            e_p: 0,
            pitch: 0,
            ref_pitch: 0,
            fund_freq: 0,
            num_harms: 0,
            num_bands: 0,
            v_uv_dsn: [0; NUM_HARMS_MAX],
            b_vec: [0; NUM_HARMS_MAX + 3],
            bit_alloc: [0; B_NUM + 4],
            sa: [0; NUM_HARMS_MAX],
            l_uv: 0,
            div_one_by_num_harm: 0,
            div_one_by_num_harm_sh: 0,
        }
    }
}
