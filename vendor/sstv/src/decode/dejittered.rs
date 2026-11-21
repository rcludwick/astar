//! Decoding with the [`dejitter`](crate::dejitter) in front:
//! [`Decoder::decode_dejittered`].

extern crate std;
use core::cell::Cell;
use core::ops::Range;
use std::vec;
use std::vec::Vec;

use super::{DecodedImage, Decoder, Demodulator};
use crate::RgbPixel;
use crate::dejitter::{Plan, fill_rows, radio_lines};
use crate::modes::Mode;

impl<I: Iterator<Item = i16>> Decoder<I> {
    /// Decode the first image in the stream as [`decode`](Self::decode)
    /// does, with its timing repaired first.
    ///
    /// The samples are collected first, since the repair needs the whole
    /// picture. Once the header has named the mode, every line of the
    /// picture's audio is put back on the mode's timing grid by a
    /// [`dejitter::Plan`](crate::dejitter::Plan), and the rows whose audio
    /// was lost are rebuilt from the rows either side of them. A frame of
    /// audio lost or stuffed on a network link, which moves every later line
    /// and tears the rest of the picture, then costs a row or two.
    ///
    /// `None` if the samples run out before an image is found.
    ///
    /// ```no_run
    /// use sstv::Decoder;
    ///
    /// # let samples = std::vec::Vec::<i16>::new().into_iter();
    /// if let Some(image) = Decoder::new(samples, 8000).decode_dejittered() {
    ///     let _ = (image.mode(), image.pixels());
    /// }
    /// ```
    #[must_use]
    pub fn decode_dejittered(self) -> Option<DecodedImage> {
        let (demodulator, mode, without_header) = self.into_parts();
        let rate = demodulator.sample_rate();
        let samples: Vec<i16> = demodulator.into_samples().collect();

        let image = configured(
            Decoder::new(samples.iter().copied(), rate),
            mode,
            without_header,
        )
        .find_image()?;
        let found = image.mode();
        let start = if without_header {
            0
        } else {
            let first = first_estimate_at(&samples, rate);
            (first + image.sequence_start().max(0.0).round() as usize).min(samples.len())
        };

        let timing = found.scan_timing();
        let raw = &samples[start..];
        let plan = Plan::new(raw, 0, rate, &timing, &[]);
        // The picture's nominal length and one line more, but only as far as
        // the input reaches.
        let nominal = ((f64::from(radio_lines(&timing)) + 1.0)
            * timing.line_seconds
            * f64::from(rate))
        .ceil() as usize;
        let length = plan.covered(0, nominal);
        let used = plan.raw_at(length, raw.len());

        let mut audio = samples[..start].to_vec();
        audio.extend(
            plan.render(0, length)
                .into_iter()
                .map(|v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16),
        );
        audio.extend_from_slice(&raw[used..]);

        let decoded = configured(
            Decoder::new(audio.into_iter(), rate),
            Some(found),
            without_header,
        )
        .decode()?;
        Some(repaired(&decoded, &plan))
    }
}

/// `decoder` told the mode and whether to skip the header search.
const fn configured<J: Iterator<Item = i16>>(
    decoder: Decoder<J>,
    mode: Option<Mode>,
    without_header: bool,
) -> Decoder<J> {
    let decoder = match mode {
        Some(mode) => decoder.with_mode(mode),
        None => decoder,
    };
    if without_header {
        decoder.without_header()
    } else {
        decoder
    }
}

/// The index into `samples` of the sample the demodulator's first frequency
/// estimate belongs to: it reads a few samples to warm up before the first.
fn first_estimate_at(samples: &[i16], rate: u32) -> usize {
    let read = Cell::new(0_usize);
    let counted = samples.iter().copied().inspect(|_| read.set(read.get() + 1));
    let mut demodulator = Demodulator::new(counted, rate);
    if demodulator.next().is_none() {
        return samples.len();
    }
    read.get().saturating_sub(1)
}

/// The image rows radio line `k` of `mode` feeds in this crate's decoder:
/// every row of the pass through the timing sequence that the line is part
/// of.
fn rows_of_line(mode: Mode, k: usize) -> Range<usize> {
    let pass = k / mode.sync_count().max(1);
    pass * mode.lines_per_sequence..(pass + 1) * mode.lines_per_sequence
}

/// Per image row: whether a radio line marked in `lines` feeds it.
fn marked_rows(mode: Mode, lines: &[bool], height: usize) -> Vec<bool> {
    let mut rows = vec![false; height];
    for (k, _) in lines.iter().enumerate().filter(|(_, marked)| **marked) {
        for row in rows_of_line(mode, k) {
            if let Some(slot) = rows.get_mut(row) {
                *slot = true;
            }
        }
    }
    rows
}

/// `image` with the rows `plan` marked rebuilt from their neighbours: the
/// damaged rows first, then the patched rows that are not damaged, so a
/// patched row next to a long damaged run never makes it too long to fill.
fn repaired(image: &DecodedImage, plan: &Plan) -> DecodedImage {
    let mode = image.mode();
    let (width, height) = (image.width(), image.height());
    let mut rgb: Vec<u8> = image
        .pixels()
        .iter()
        .flat_map(|p| [p.red(), p.green(), p.blue()])
        .collect();
    let damaged = marked_rows(mode, plan.damaged(), height);
    let patched: Vec<bool> = marked_rows(mode, plan.patched(), height)
        .into_iter()
        .zip(&damaged)
        .map(|(p, d)| p && !*d)
        .collect();
    let limit = height as u32;
    fill_rows(&mut rgb, width as u32, &damaged, limit);
    fill_rows(&mut rgb, width as u32, &patched, limit);
    let pixels = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| RgbPixel::new(r, g, b))
        .collect();
    DecodedImage::new(mode, image.complete(), pixels)
}
