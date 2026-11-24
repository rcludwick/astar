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

//! The IMBE decoder against golden vectors from the C++ original.

mod common;

use common::Signal;
use imbe_vocoder::ImbeDecoder;

#[test]
fn unpack_then_pack_is_identity_for_every_bit() {
    for bit in 0..88 {
        let mut f = [0u8; 11];
        f[bit / 8] = 0x80 >> (bit % 8);
        assert_eq!(
            imbe_vocoder::frame::pack(&imbe_vocoder::frame::unpack(&f)),
            f
        );
    }
}

#[test]
fn unpack_puts_u0_in_the_first_twelve_bits() {
    let u = imbe_vocoder::frame::unpack(&[0xFF, 0xF0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(u, [0x0FFF, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn decoder_matches_the_cpp_original_bit_for_bit() {
    let vectors = common::read_decode_vectors();
    // Seven signals of 100 frames, then 50 random, 10 null, 10 zero, 10 FF.
    assert_eq!(vectors.len(), 7 * common::FRAMES_PER_SIGNAL + 80);
    let mut dec = ImbeDecoder::new();
    for (i, (frame, want)) in vectors.iter().enumerate() {
        assert_eq!(&dec.decode(frame), want, "record {i}");
    }
}

#[test]
fn a_decoded_1khz_tone_peaks_at_1khz() {
    let v = common::read_decode_vectors();
    let mut dec = ImbeDecoder::new();
    let pcm: Vec<i16> = v[common::signal_frames(Signal::Tone1k)]
        .iter()
        .flat_map(|(f, _)| dec.decode(f))
        .collect();
    let hz = common::dominant_hz(&pcm[160 * 10..]);
    assert!((940.0..=1060.0).contains(&hz), "peak {hz} Hz");
}

#[test]
fn the_decode_vectors_start_with_the_encoders_frames() {
    // make-golden.sh decodes exactly the frames the encoder wrote, in order,
    // before its 80 hostile frames; a mismatched pair of files fails here.
    let enc = common::read_encode_vectors();
    let dec = common::read_decode_vectors();
    assert_eq!(enc.len(), 7 * common::FRAMES_PER_SIGNAL);
    for (i, ((_, encoded), (frame, _))) in enc.iter().zip(&dec).enumerate() {
        assert_eq!(encoded, frame, "record {i}");
    }
}
