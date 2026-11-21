// Examples fail fast on bad input by design.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Decode an SSTV WAV or MP3 back into an image.
//!
//! Accepts a `.wav`, an `.mp3` or a gzipped `.wav.gz` and writes the decoded
//! image to the given path (format inferred from its extension).
//!
//! ```text
//! cargo run --features image,wav,mp3 --example decode -- tests/assets/real_recording.wav.gz local/decoded.png [mode]
//! ```
//!
//! The recording in the example is fetched by `tests/scripts/fetch_assets.py`.
//!
//! Rows that could not be decoded are left black, so a misaligned or truncated
//! decode is still visible rather than fatal.

use std::env;
use std::fs::File;
use std::io::Read;

use sstv::{Decoder, Mode, modes};

fn has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
}

fn parse_mode(name: &str) -> Mode {
    modes::ALL
        .into_iter()
        .find(|mode| format!("{mode:?}").eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("unknown mode {name}, expected one of {:?}", modes::ALL))
}

fn main() {
    let mut args = env::args().skip(1);
    let usage = "usage: decode <input.wav|input.mp3|input.wav.gz> <output image> [mode]";
    let input = args.next().expect(usage);
    let output = args.next().expect(usage);
    // No mode argument means detecting it from the transmission's header.
    let mode = args.next().map(|name| parse_mode(&name));

    let audio = read_audio(&input);
    let mut decoder = if has_extension(&input, "mp3") {
        Decoder::from_mp3(&audio).expect("parse mp3")
    } else {
        Decoder::from_wav(&audio).expect("parse wav")
    };
    if let Some(mode) = mode {
        decoder = decoder.with_mode(mode);
    }

    let Some(image) = decoder.decode() else {
        panic!("no image found in {input}");
    };
    println!(
        "decoded a {}x{} {:?} image (complete: {})",
        image.width(),
        image.height(),
        image.mode(),
        image.complete(),
    );

    image::RgbImage::from(&image)
        .save(&output)
        .expect("save output image");
    println!("wrote {output}");
}

/// Read the input file, decompressing first if the path ends in `.gz`.
fn read_audio(path: &str) -> Vec<u8> {
    let file = File::open(path).expect("open input file");

    let mut bytes = Vec::new();
    if has_extension(path, "gz") {
        flate2::read::GzDecoder::new(file)
            .read_to_end(&mut bytes)
            .expect("gunzip input");
    } else {
        let mut file = file;
        file.read_to_end(&mut bytes).expect("read input");
    }
    bytes
}
