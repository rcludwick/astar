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

//! IMBE vocoder test helpers, shared by the golden-vector tests.

// Each integration test uses a different subset of these helpers.
#![allow(dead_code)]

use std::ops::Range;

/// Frames per test signal in `encode.bin` (100 frames, 2 s).
pub const FRAMES_PER_SIGNAL: usize = 100;

/// The test signals `tools/make-golden.sh` encodes, in file order. The first
/// 700 records of `decode.bin` are the frames these produced, in the same
/// order, so [`signal_frames`] names records in both files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// 1 kHz sine, -20 dBFS RMS.
    Tone1k,
    /// 400 Hz sine, -20 dBFS RMS.
    Tone400,
    /// 125 Hz pulse train through formants at 700, 1220 and 2600 Hz, -20 dBFS.
    Vowel,
    /// Uniform LCG noise, seed 1, -30 dBFS RMS.
    Noise,
    /// All zeros.
    Silence,
    /// 200 to 3400 Hz linear chirp, -20 dBFS RMS.
    Chirp,
    /// 1 kHz square wave at +/-32767.
    FullScale,
}

impl Signal {
    pub const ALL: [Signal; 7] = [
        Signal::Tone1k,
        Signal::Tone400,
        Signal::Vowel,
        Signal::Noise,
        Signal::Silence,
        Signal::Chirp,
        Signal::FullScale,
    ];
}

/// The records that belong to `signal`.
pub fn signal_frames(signal: Signal) -> Range<usize> {
    let i = Signal::ALL.iter().position(|s| *s == signal).unwrap();
    i * FRAMES_PER_SIGNAL..(i + 1) * FRAMES_PER_SIGNAL
}

const MAGIC: &[u8; 8] = b"IMBEGLD1";
const DECODE_BIN: &[u8] = include_bytes!("../golden/decode.bin");
const ENCODE_BIN: &[u8] = include_bytes!("../golden/encode.bin");

fn records(bin: &[u8], record_len: usize) -> impl Iterator<Item = &[u8]> {
    assert_eq!(&bin[..8], MAGIC, "golden file magic");
    let n = u32::from_le_bytes(bin[8..12].try_into().unwrap()) as usize;
    let body = &bin[12..];
    assert_eq!(body.len(), n * record_len, "golden file length");
    body.chunks_exact(record_len)
}

fn pcm(bytes: &[u8]) -> [i16; 160] {
    let mut out = [0i16; 160];
    for (i, o) in out.iter_mut().enumerate() {
        *o = i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]);
    }
    out
}

/// `decode.bin`: each record's 11 input bytes and the C++ decoder's 160
/// output samples.
pub fn read_decode_vectors() -> Vec<([u8; 11], [i16; 160])> {
    records(DECODE_BIN, 11 + 320)
        .map(|r| (r[..11].try_into().unwrap(), pcm(&r[11..])))
        .collect()
}

/// `encode.bin`: each record's 160 input samples and the C++ encoder's 11
/// output bytes.
pub fn read_encode_vectors() -> Vec<([i16; 160], [u8; 11])> {
    records(ENCODE_BIN, 320 + 11)
        .map(|r| (pcm(&r[..320]), r[320..].try_into().unwrap()))
        .collect()
}

/// RMS level in dBFS, where 0 dBFS is an RMS of 32768. Silence is -inf.
pub fn rms_dbfs(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (sum / pcm.len() as f64).sqrt();
    if rms == 0.0 {
        f32::NEG_INFINITY
    } else {
        (20.0 * (rms / 32768.0).log10()) as f32
    }
}

/// Power at `hz` (8 kHz sample rate) by the Goertzel algorithm.
fn goertzel_power(pcm: &[i16], hz: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * hz / 8000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in pcm {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// The strongest frequency between 50 Hz and 3950 Hz, to the nearest 5 Hz.
pub fn dominant_hz(pcm: &[i16]) -> f32 {
    let mut best = (0.0f64, 0.0f64);
    let mut hz = 50.0;
    while hz <= 3950.0 {
        let p = goertzel_power(pcm, hz);
        if p > best.1 {
            best = (hz, p);
        }
        hz += 5.0;
    }
    best.0 as f32
}

/// Pearson correlation of the two signals' magnitude spectra, sampled every
/// 50 Hz from 100 Hz to 3800 Hz. 1.0 is the same spectral shape.
pub fn spectral_correlation(a: &[i16], b: &[i16]) -> f32 {
    let bins: Vec<f64> = (2..=76).map(|k| f64::from(k) * 50.0).collect();
    let sa: Vec<f64> = bins.iter().map(|&h| goertzel_power(a, h).sqrt()).collect();
    let sb: Vec<f64> = bins.iter().map(|&h| goertzel_power(b, h).sqrt()).collect();
    let n = bins.len() as f64;
    let ma = sa.iter().sum::<f64>() / n;
    let mb = sb.iter().sum::<f64>() / n;
    let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
    for (x, y) in sa.iter().zip(&sb) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    if va == 0.0 || vb == 0.0 {
        return 0.0;
    }
    (cov / (va.sqrt() * vb.sqrt())) as f32
}
