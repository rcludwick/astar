// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Real SSTV recordings from Wikimedia Commons (see
//! `tests/fixtures/CREDITS.md`), converted to 8 kHz mono FLAC.

mod common;

use std::path::PathBuf;

use astar_sstv::{Decoder, Event, Mode, Picture};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn decode_flac(name: &str) -> Picture {
    let mut reader = claxon::FlacReader::open(fixture(name)).expect("open flac");
    let info = reader.streaminfo();
    assert_eq!((info.channels, info.bits_per_sample), (1, 16));
    let rate = info.sample_rate;
    let mut samples: Vec<i16> = reader
        .samples()
        .map(|s| i16::try_from(s.expect("sample")).expect("16-bit"))
        .collect();
    samples.extend(std::iter::repeat_n(0, rate as usize));
    let mut decoder = Decoder::new(rate).expect("decoder");
    common::run(&mut decoder, &samples)
        .into_iter()
        .find_map(|e| match e {
            Event::Finished(p) => Some(p),
            _ => None,
        })
        .expect("a finished picture")
}

#[test]
fn martin1_wikimedia_decodes() {
    let pic = decode_flac("martin1-sunset-8k.flac");
    assert_eq!(pic.mode, Mode::Martin1);
    assert_eq!((pic.width, pic.height), (320, 256));
    assert!(pic.quality >= 0.43, "quality {}", pic.quality);
}

#[test]
fn robot36_wikimedia_matches_its_reference() {
    let pic = decode_flac("robot36-french-logo-8k.flac");
    assert_eq!(pic.mode, Mode::Robot36);
    assert_eq!((pic.width, pic.height), (320, 240));
    assert!(pic.quality >= 0.43, "quality {}", pic.quality);

    let mut reader = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(fixture("robot36-french-logo.png")).expect("reference"),
    ))
    .read_info()
    .expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    assert_eq!((info.width, info.height), (320, 240));
    let step = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => panic!("reference colour type {other:?}"),
    };
    let reference: Vec<u8> = buf[..info.buffer_size()]
        .chunks(step)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert_eq!(reference.len(), pic.rgb.len());
    let total: u64 = reference
        .iter()
        .zip(&pic.rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total as f64 / reference.len() as f64;
    assert!(mean <= 40.0, "mean abs error {mean}");
}
