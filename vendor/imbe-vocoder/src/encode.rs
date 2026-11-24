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

//! The encoder: 160 samples of 8 kHz speech in, one 88-bit frame out. Port
//! of `encode.cc` and the encoder's share of `imbe_vocoder.cc`.

use crate::basic_op::mult;
use crate::ch_encode::encode_frame_vector;
use crate::dc_rmv::dc_rmv;
use crate::dsp_sub::{FFTLENGTH, Fft};
use crate::pe_lpf::{PE_LPF_ORD, pe_lpf};
use crate::pitch_est::PitchEst;
use crate::pitch_ref::pitch_ref;
use crate::sa_encode::SaEncoder;
use crate::tables::WR;
use crate::v_uv_det::VUvDet;
use crate::{FRAME, FRAME_BYTES, FRAME_SAMPLES, ImbeParam, frame};

/// The analysis buffer: the current frame and two frames of look-ahead,
/// plus history.
const PITCH_EST_BUF_SIZE: usize = 621;

/// The IMBE encoder. Each encoder carries its own state, so one per
/// talker; frames must be fed in order.
#[derive(Clone, Debug)]
pub struct ImbeEncoder {
    param: ImbeParam,
    /// Input after the low-pass filter, for the pitch estimator.
    pitch_est_buf: [i16; PITCH_EST_BUF_SIZE],
    /// Input after DC removal, for refinement and the spectrum.
    pitch_ref_buf: [i16; PITCH_EST_BUF_SIZE],
    dc_rmv_mem: i32,
    pe_lpf_mem: [i16; PE_LPF_ORD],
    fft: Fft,
    pitch_est: PitchEst,
    v_uv_det: VUvDet,
    sa_enc: SaEncoder,
}

impl ImbeEncoder {
    /// An encoder in the state upstream's constructor and `encode_init`
    /// leave it.
    pub fn new() -> ImbeEncoder {
        // Upstream's encoder and decoder share one IMBE_PARAM, which the
        // constructor zeroes and `decode_init` then seeds. The encoder
        // writes every field it reads except b(L+2), which `ch_encode`
        // reads stale (see there), and that starts at 0 either way.
        let param = ImbeParam {
            fund_freq: 0x0cf6_474a,
            num_harms: 9,
            num_bands: 3,
            ..ImbeParam::default()
        };
        ImbeEncoder {
            param,
            pitch_est_buf: [0; PITCH_EST_BUF_SIZE],
            pitch_ref_buf: [0; PITCH_EST_BUF_SIZE],
            dc_rmv_mem: 0,
            pe_lpf_mem: [0; PE_LPF_ORD],
            fft: Fft::new(),
            pitch_est: PitchEst::new(),
            v_uv_det: VUvDet::new(),
            sa_enc: SaEncoder::new(),
        }
    }

    /// Encodes 160 samples of 8 kHz, 16-bit audio into one 11-byte frame
    /// (the MMDVMHost packing, see [`frame`]).
    ///
    /// The encoder keeps two frames of look-ahead: the spectrum each frame
    /// describes is centred 471 samples before the newest input sample, so
    /// the frames lag the audio fed in by about three frames (60 ms).
    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES]) -> [u8; FRAME_BYTES] {
        frame::pack(&self.encode_vector(pcm))
    }

    /// Upstream's `encode`: audio in, the code vectors û0..û7 out.
    fn encode_vector(&mut self, snd: &[i16; FRAME_SAMPLES]) -> [i16; 8] {
        const TAIL: usize = PITCH_EST_BUF_SIZE - FRAME;

        self.pitch_est_buf.copy_within(FRAME.., 0);
        self.pitch_ref_buf.copy_within(FRAME.., 0);

        dc_rmv(snd, &mut self.pitch_ref_buf[TAIL..], &mut self.dc_rmv_mem);
        pe_lpf(
            &self.pitch_ref_buf[TAIL..],
            &mut self.pitch_est_buf[TAIL..],
            &mut self.pe_lpf_mem,
        );

        self.pitch_est
            .pitch_est(&mut self.param, &self.pitch_est_buf);

        // Window the speech and take its spectrum. The window is
        // symmetric about sample 150 of the buffer, which lands in bin 0;
        // the half before it wraps to the top of the FFT. Interleaved re, im.
        let mut fft_buf = [0i16; 2 * FFTLENGTH];
        let sig = &self.pitch_ref_buf[40..];
        for (k, (&s, &w)) in sig.iter().zip(&WR).enumerate() {
            fft_buf[2 * (146 + k)] = mult(s, w);
        }
        fft_buf[0] = sig[WR.len()];
        for (k, (&s, &w)) in sig[WR.len() + 1..].iter().zip(WR.iter().rev()).enumerate() {
            fft_buf[2 * (1 + k)] = mult(s, w);
        }
        // Bins 111..145 stay zero.

        self.fft.fft(&mut fft_buf, FFTLENGTH, 1);

        pitch_ref(&mut self.param, &fft_buf);
        self.v_uv_det.v_uv_det(&mut self.param, &fft_buf);
        self.sa_enc.sa_encode(&mut self.param);

        let mut frame_vector = [0i16; 8];
        encode_frame_vector(&mut self.param, &mut frame_vector);
        frame_vector
    }
}

impl Default for ImbeEncoder {
    fn default() -> Self {
        Self::new()
    }
}
