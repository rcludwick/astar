// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! GSM 06.10 checks that run on every `just ci`, with no outside files.
//!
//! The bit-exact proof is `gsm_etsi.rs`, which needs the ETSI vectors. This
//! file pins the encoder's output for a fixed tone (so a dependency bump
//! that changes a single bit fails here), and checks that the decoded tone
//! still sounds like the tone.
#![cfg(feature = "gsm")]

use astar_codec::gsm::{FRAME_BYTES, FRAME_SAMPLES, GsmDecoder, GsmEncoder};

const FRAMES: usize = 50;
const GOLDEN: &[u8] = include_bytes!("fixtures/gsm/tone1k.gsm");

/// 1000 Hz at -12 dBFS (peak 8231) sampled at 8 kHz: one cycle is exactly
/// eight samples, so the table is the whole signal and no floating point is
/// involved in making it. 8231 * sin(45 deg) = 5820.25, rounded to 5820.
fn tone_1k() -> Vec<i16> {
    const CYCLE: [i16; 8] = [0, 5820, 8231, 5820, 0, -5820, -8231, -5820];
    (0..FRAMES * FRAME_SAMPLES).map(|i| CYCLE[i % 8]).collect()
}

fn encode_all(pcm: &[i16]) -> Vec<[u8; FRAME_BYTES]> {
    let mut enc = GsmEncoder::new();
    let (frames, rest) = pcm.as_chunks::<FRAME_SAMPLES>();
    assert_eq!(rest, [] as [i16; 0]);
    frames.iter().map(|c| enc.encode(c)).collect()
}

fn rms(samples: &[i16]) -> f64 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let n = f64::from(u32::try_from(samples.len()).expect("short signal"));
    (sum / n).sqrt()
}

/// Goertzel power at `freq` Hz for 8 kHz samples.
fn goertzel(samples: &[i16], freq: f64) -> f64 {
    let coeff = 2.0 * (2.0 * std::f64::consts::PI * freq / 8000.0).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s0 = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

#[test]
fn encoding_the_1k_tone_reproduces_the_golden_frames() {
    let encoded: Vec<u8> = encode_all(&tone_1k()).concat();
    assert_eq!(GOLDEN.len(), FRAMES * FRAME_BYTES);
    assert_eq!(encoded.len(), GOLDEN.len());
    let (got_frames, _) = encoded.as_chunks::<FRAME_BYTES>();
    let (want_frames, _) = GOLDEN.as_chunks::<FRAME_BYTES>();
    for (f, (got, want)) in got_frames.iter().zip(want_frames).enumerate() {
        assert_eq!(got, want, "frame {f}");
    }
}

#[test]
fn every_encoded_frame_starts_with_the_d_signature() {
    for (f, frame) in encode_all(&tone_1k()).iter().enumerate() {
        assert_eq!(frame[0] >> 4, 0xD, "frame {f}");
    }
}

#[test]
fn decoded_tone_keeps_its_level_and_pitch() {
    let tone = tone_1k();
    let mut dec = GsmDecoder::new();
    let decoded: Vec<i16> = encode_all(&tone)
        .iter()
        .flat_map(|frame| dec.decode(frame))
        .collect();
    // Skip the first ten frames while the decoder's filters settle.
    let steady = &decoded[10 * FRAME_SAMPLES..];
    let input = &tone[10 * FRAME_SAMPLES..];

    let level_db = 20.0 * (rms(steady) / rms(input)).log10();
    assert!(
        level_db.abs() <= 3.0,
        "decoded level is {level_db:.2} dB from the input"
    );

    let pitch_db = 10.0 * (goertzel(steady, 1000.0) / goertzel(steady, 2500.0)).log10();
    assert!(
        pitch_db >= 20.0,
        "1000 Hz is only {pitch_db:.1} dB above 2500 Hz"
    );
}

#[test]
fn a_frame_without_the_signature_decodes_as_silence() {
    let mut dec = GsmDecoder::new();
    assert_eq!(dec.decode(&[0u8; FRAME_BYTES]), [0i16; FRAME_SAMPLES]);
}

#[test]
fn the_codec_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<GsmEncoder>();
    assert_send::<GsmDecoder>();
}
