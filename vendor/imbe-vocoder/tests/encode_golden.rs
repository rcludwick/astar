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

//! The IMBE encoder against golden vectors from the C++ original, and the
//! round trip through both halves of the port.

mod common;

use common::Signal;
use imbe_vocoder::{FRAME_SAMPLES, ImbeDecoder, ImbeEncoder};

/// `signal`'s input frames from `encode.bin`.
fn input_frames(signal: Signal) -> Vec<[i16; FRAME_SAMPLES]> {
    common::read_encode_vectors()[common::signal_frames(signal)]
        .iter()
        .map(|(pcm, _)| *pcm)
        .collect()
}

/// `signal`'s input and what a fresh encoder and a fresh decoder make of it.
fn round_trip_with_input(signal: Signal) -> (Vec<i16>, Vec<i16>) {
    let mut enc = ImbeEncoder::new();
    let mut dec = ImbeDecoder::new();
    let frames = input_frames(signal);
    let input: Vec<i16> = frames.iter().flatten().copied().collect();
    let output: Vec<i16> = frames
        .iter()
        .flat_map(|pcm| dec.decode(&enc.encode(pcm)))
        .collect();
    (input, output)
}

fn round_trip(signal: Signal) -> Vec<i16> {
    round_trip_with_input(signal).1
}

#[test]
fn encoder_matches_the_cpp_original_bit_for_bit() {
    let vectors = common::read_encode_vectors();
    assert_eq!(vectors.len(), 7 * common::FRAMES_PER_SIGNAL);
    let mut enc = ImbeEncoder::new();
    for (i, (pcm, want)) in vectors.iter().enumerate() {
        assert_eq!(&enc.encode(pcm), want, "record {i}");
    }
}

#[test]
fn a_default_encoder_is_a_new_one() {
    let pcm = input_frames(Signal::Vowel);
    let (mut a, mut b) = (ImbeEncoder::new(), ImbeEncoder::default());
    for f in &pcm {
        assert_eq!(a.encode(f), b.encode(f));
    }
}

#[test]
fn a_1khz_tone_round_trips_to_1khz() {
    let out = round_trip(Signal::Tone1k);
    let hz = common::dominant_hz(&out[160 * 10..]);
    assert!((940.0..=1060.0).contains(&hz), "peak {hz} Hz");
}

#[test]
fn a_400hz_tone_round_trips_near_400hz() {
    let hz = common::dominant_hz(&round_trip(Signal::Tone400)[160 * 10..]);
    assert!((370.0..=430.0).contains(&hz), "peak {hz} Hz");
}

#[test]
fn a_vowel_round_trips_with_its_spectral_envelope() {
    let (input, out) = round_trip_with_input(Signal::Vowel);
    // The gating note measured +0.79 for this path on the C++ original.
    let r = common::spectral_correlation(&input[1600..], &out[1600..]);
    assert!(r >= 0.70, "correlation {r}");
}

#[test]
fn full_scale_input_round_trips_without_panicking() {
    let _ = round_trip(Signal::FullScale);
}
