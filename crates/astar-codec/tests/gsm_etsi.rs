// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! GSM 06.10 against the ETSI EN 300 961 test sequences.
//!
//! The vectors are ETSI's and are never committed. Point
//! `ASTAR_GSM_ETSI_DIR` at an unzipped copy of the EN 300 961 V8.1.1
//! electronic attachment; the files are found recursively and by
//! case-insensitive name, so the `DISK1`..`DISK3` layout works as-is. Every
//! file is checked against `fixtures/gsm/ETSI-SHA256SUMS` before use. With
//! the variable unset these tests print a skip line and pass, which keeps
//! `just ci` offline.
//!
//! File layout (EN 300 961 §1.7): `.inp` and `.out` are 160 little-endian
//! 16-bit samples per frame; `.cod` is 76 little-endian 16-bit words per
//! frame, one Table 1.1 parameter per word.
#![cfg(feature = "gsm")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use astar_codec::gsm::{FRAME_BYTES, FRAME_SAMPLES, GsmDecoder, GsmEncoder};
use sha2::{Digest, Sha256};

const COD_WORDS: usize = 76;
const MANIFEST: &str = include_str!("fixtures/gsm/ETSI-SHA256SUMS");

/// Pack one 76-word `.cod` frame into the 33-byte `.gsm` byte frame: the
/// `0xD` signature nibble, then every Table 1.1 parameter MSB-first —
/// `LARc[1..8]` at widths 6,6,5,5,4,4,3,3, then four sub-frames of `Nc` 7,
/// `bc` 2, `Mc` 2, `xmaxc` 6 and thirteen `xMc` of 3 bits. 4 + 260 = 264
/// bits, exactly 33 bytes. Written from the standard, independent of the
/// codec crate, so the two can disagree.
fn pack_cod_frame(words: &[u16]) -> [u8; FRAME_BYTES] {
    assert_eq!(words.len(), COD_WORDS);
    let mut widths: Vec<u32> = vec![6, 6, 5, 5, 4, 4, 3, 3];
    for _ in 0..4 {
        widths.extend([7, 2, 2, 6]);
        widths.extend([3; 13]);
    }
    assert_eq!(widths.len(), COD_WORDS);

    let mut out = [0u8; FRAME_BYTES];
    let mut bit = 0usize;
    let mut put = |value: u32, width: u32| {
        for i in (0..width).rev() {
            if (value >> i) & 1 == 1 {
                out[bit / 8] |= 0x80 >> (bit % 8);
            }
            bit += 1;
        }
    };
    put(0xD, 4);
    for (&w, &width) in words.iter().zip(&widths) {
        assert!(
            u32::from(w) < (1 << width),
            "parameter {w} does not fit {width} bits"
        );
        put(u32::from(w), width);
    }
    assert_eq!(bit, FRAME_BYTES * 8);
    out
}

#[test]
fn pack_cod_frame_of_zeros_is_d0_then_zeros() {
    let mut expected = [0u8; FRAME_BYTES];
    expected[0] = 0xD0;
    assert_eq!(pack_cod_frame(&[0u16; COD_WORDS]), expected);
}

/// The vector directory, or `None` (after printing the skip line) when
/// `ASTAR_GSM_ETSI_DIR` is unset.
fn etsi_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ASTAR_GSM_ETSI_DIR") {
        Some(PathBuf::from(dir))
    } else {
        println!("ASTAR_GSM_ETSI_DIR not set; skipping");
        None
    }
}

fn index_files(dir: &Path, into: &mut HashMap<String, PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            index_files(&path, into);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            into.entry(name.to_ascii_lowercase()).or_insert(path);
        }
    }
}

/// Every manifest file, by its manifest name, after checking its SHA-256.
fn verified_vectors(dir: &Path) -> HashMap<String, Vec<u8>> {
    let mut found = HashMap::new();
    index_files(dir, &mut found);
    let mut vectors = HashMap::new();
    for line in MANIFEST.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, name) = line.split_once("  ").expect("manifest line");
        let path = found
            .get(&name.to_ascii_lowercase())
            .unwrap_or_else(|| panic!("{name} not found under {}", dir.display()));
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let actual = Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut hex, b| {
                use std::fmt::Write;
                let _ = write!(hex, "{b:02x}");
                hex
            });
        assert_eq!(actual, hash, "{name} does not match ETSI-SHA256SUMS");
        vectors.insert(name.to_string(), bytes);
    }
    vectors
}

fn le_words(bytes: &[u8]) -> Vec<u16> {
    let (words, rest) = bytes.as_chunks::<2>();
    assert!(rest.is_empty(), "odd byte count");
    words.iter().map(|&w| u16::from_le_bytes(w)).collect()
}

fn le_samples(bytes: &[u8]) -> Vec<i16> {
    le_words(bytes)
        .into_iter()
        .map(|w| i16::from_le_bytes(w.to_le_bytes()))
        .collect()
}

#[test]
fn etsi_encoder_reproduces_seq01_to_seq04_cod_bit_exactly() {
    let Some(dir) = etsi_dir() else { return };
    let vectors = verified_vectors(&dir);
    for n in 1..=4 {
        let inp = le_samples(&vectors[&format!("Seq{n:02}.inp")]);
        let cod_words = le_words(&vectors[&format!("Seq{n:02}.cod")]);
        let frames = inp.len() / FRAME_SAMPLES;
        assert_eq!(cod_words.len(), frames * COD_WORDS, "Seq{n:02} frame count");
        let mut enc = GsmEncoder::new();
        for f in 0..frames {
            let pcm: &[i16; FRAME_SAMPLES] = inp[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES]
                .try_into()
                .expect("160 samples");
            let encoded = enc.encode(pcm);
            let expected = pack_cod_frame(&cod_words[f * COD_WORDS..(f + 1) * COD_WORDS]);
            assert_eq!(encoded, expected, "Seq{n:02} frame {f}");
        }
    }
}

#[test]
fn etsi_decoder_reproduces_seq01_to_seq05_out_sample_exactly() {
    let Some(dir) = etsi_dir() else { return };
    let vectors = verified_vectors(&dir);
    for n in 1..=5 {
        let cod_words = le_words(&vectors[&format!("Seq{n:02}.cod")]);
        let out = le_samples(&vectors[&format!("Seq{n:02}.out")]);
        let frames = cod_words.len() / COD_WORDS;
        assert_eq!(out.len(), frames * FRAME_SAMPLES, "Seq{n:02} frame count");
        let mut dec = GsmDecoder::new();
        for f in 0..frames {
            let frame = pack_cod_frame(&cod_words[f * COD_WORDS..(f + 1) * COD_WORDS]);
            let decoded = dec.decode(&frame);
            let expected = &out[f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES];
            assert_eq!(decoded.as_slice(), expected, "Seq{n:02} frame {f}");
        }
    }
}
