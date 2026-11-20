// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The decoder: slowrx behind astar's own API, plus the one thing slowrx
//! does not do, which is give up on a picture when the signal goes silent.

use std::collections::VecDeque;

use slowrx::{ModeSpec, SstvDecoder, SstvEvent, WORKING_SAMPLE_RATE_HZ};

use crate::Mode;
use crate::retime::{Plan, fill_rows, radio_lines, rows_of_line};

/// A sample whose magnitude is below this counts as silence (about −54 dBFS).
pub const SILENCE_LEVEL: i16 = 64;
/// This many seconds of unbroken silence abandon a picture in flight.
pub const SILENCE_SECS: u32 = 2;

/// Seconds of recent input kept, so the start of a picture can be re-timed
/// with the rest of it.
const HISTORY_SECS: u32 = 2;

/// Why a [`Decoder`] could not be made.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SstvError {
    /// The sample rate is 0 or above 192 000 Hz.
    #[error("unsupported SSTV input sample rate {0} Hz")]
    Rate(u32),
}

/// A picture, whole or in part.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub mode: Mode,
    pub width: u32,
    /// Display rows: Robot 12 and Robot 8 B/W send each row once and show
    /// it twice, so their height is twice the rows on the air.
    pub height: u32,
    /// Row-major 8-bit RGB, `width * height * 3` bytes. Rows never heard
    /// are black.
    pub rgb: Vec<u8>,
    /// Display rows heard, counted from the top: `height` for a finished
    /// picture.
    pub lines: u32,
    /// How clearly the line-sync pulses showed up, 0 to 1. Legible
    /// pictures score about 0.43 and up. Not measured for an abandoned
    /// picture, which reports 0.
    pub quality: f32,
    /// Input samples since [`Decoder::new`] at which the VIS header ended.
    pub at_sample: u64,
    /// True when the picture was abandoned before its last row.
    pub partial: bool,
    /// Display rows whose lost audio was made up (see [`Retime`]): a short
    /// gap filled from the line before, or the whole row rebuilt from the
    /// rows above and below.
    pub repaired_rows: u32,
}

/// Whether the decoder repairs the scan timing before decoding.
///
/// Linked audio arrives with whole 20 ms frames dropped, short runs of
/// zeros stuffed in and the sender's clock a little off. `slowrx` fits one
/// straight line through every line-sync pulse, so it cannot follow a
/// step: every line after a dropped frame lands in the wrong place. With
/// `On` (the default) each line's sync pulse is found and the audio is
/// re-timed so every line starts on time, and rows whose audio was lost are
/// filled from their neighbours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retime {
    #[default]
    On,
    Off,
}

impl Picture {
    /// At least half the rows arrived, so the picture is worth saving.
    #[must_use]
    pub fn worth_keeping(&self) -> bool {
        u64::from(self.lines) * 2 >= u64::from(self.height)
    }
}

/// What the decoder saw in the audio it was given.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A VIS header announced a picture.
    Started {
        mode: Mode,
        width: u32,
        height: u32,
        /// Input samples since [`Decoder::new`] at which the header ended.
        at_sample: u64,
    },
    /// One display row, `width * 3` bytes of RGB. slowrx decodes a picture
    /// only once all of its audio has arrived, so a picture's rows come in
    /// one burst just before `Finished` (or `Abandoned`), not as they air.
    Line { row: u32, rgb: Vec<u8> },
    /// The picture ran to its last row.
    Finished(Picture),
    /// The signal went silent, or the stream ended, mid-picture. The
    /// picture carries the rows heard before that.
    Abandoned(Picture),
    /// The picture ran its full length but was noise, not a picture.
    Rejected { mode: Mode },
}

/// The picture being decoded.
#[derive(Debug)]
struct InFlight {
    mode: Mode,
    width: u32,
    height: u32,
    /// Display rows per row slowrx decodes (2 for Robot 12/8 B/W).
    line_height: u32,
    /// Decoded rows per radio line (2 for PD, which packs two per line).
    rows_per_radio_line: u32,
    /// One radio line's length in input samples.
    line_samples: f64,
    /// Rows slowrx decodes: `height / line_height`.
    image_rows: u32,
    at_sample: u64,
    spec: ModeSpec,
    /// Display rows to rebuild from their neighbours, once re-timed.
    repair: Vec<bool>,
    /// Display rows where a short stretch of lost audio was made up from
    /// the line before; they are rebuilt from their neighbours too.
    patched: Vec<bool>,
}

impl InFlight {
    /// Rebuild the marked rows of `rgb` among the first `limit`, and count
    /// every row repaired either way.
    ///
    /// The damaged rows go first. Then each patched row is rebuilt from the
    /// rows either side of it: the audio made up from the line before is
    /// a different line's, spliced in with a jump in phase, and leaves a
    /// short dash of wrong colour even where it lands in the right place.
    /// They are a pass of their own so that a patched row next to a long
    /// damaged run never makes that run too long to fill.
    fn repair_rows(&self, rgb: &mut [u8], limit: u32) -> u32 {
        let filled = fill_rows(rgb, self.width, &self.repair, limit);
        let only: Vec<bool> = self
            .patched
            .iter()
            .zip(self.repair.iter().chain(std::iter::repeat(&false)))
            .map(|(p, r)| *p && !*r)
            .collect();
        fill_rows(rgb, self.width, &only, limit);
        let limit = (limit as usize).min(only.len());
        let patched = only[..limit].iter().filter(|p| **p).count();
        filled + u32::try_from(patched).unwrap_or(u32::MAX)
    }
}

/// A picture's audio, held back from slowrx until all of it has arrived so
/// that it can be re-timed as a whole.
struct Held {
    /// Input from the end of the header.
    audio: Vec<i16>,
    /// How many of those samples slowrx already has (it took them with the
    /// header).
    given: usize,
    /// Hold this many samples, then re-time and decode.
    target: usize,
}

impl InFlight {
    /// Display rows whose audio had fully arrived by input sample `at`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn rows_heard_by(&self, at: u64) -> u32 {
        let elapsed = at.saturating_sub(self.at_sample) as f64;
        let radio_lines = (elapsed / self.line_samples).floor() as u64;
        let rows = radio_lines * u64::from(self.rows_per_radio_line) * u64::from(self.line_height);
        u32::try_from(rows.min(u64::from(self.height))).unwrap_or(self.height)
    }
}

/// An SSTV decoder fed 16-bit mono audio at one sample rate.
pub struct Decoder {
    inner: SstvDecoder,
    rate: u32,
    samples_in: u64,
    /// `samples_in` when `inner` last started counting from zero.
    base: u64,
    current: Option<InFlight>,
    /// Consecutive silent samples, across calls to `push`.
    silent_run: u64,
    /// Display rows heard when the current silent run began.
    lines_at_silence: u32,
    retime: Retime,
    /// The last few seconds of input, ending at `raw_pos`.
    history: VecDeque<i16>,
    /// Input position of the next sample to go to slowrx or `held`.
    raw_pos: u64,
    /// Input samples minus slowrx samples since `base`: re-timing changes
    /// the audio's length.
    skew: i64,
    /// A header was just heard and its picture is to be held.
    hold_pending: bool,
    held: Option<Held>,
    /// Input after a released picture, still to be given to slowrx.
    leftover: Option<Vec<i16>>,
    /// Audio known to be lost in the picture being held, as (input sample,
    /// length): from [`Decoder::push_hole`].
    known_holes: Vec<(u64, u64)>,
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("rate", &self.rate)
            .field("samples_in", &self.samples_in)
            .field("in_flight", &self.in_flight())
            .finish_non_exhaustive()
    }
}

impl Decoder {
    /// A decoder for audio at `rate` Hz (1 to 192 000).
    pub fn new(rate: u32) -> Result<Self, SstvError> {
        if rate == 0 || rate > slowrx::MAX_INPUT_SAMPLE_RATE_HZ {
            return Err(SstvError::Rate(rate));
        }
        let inner = SstvDecoder::new(rate).map_err(|_| SstvError::Rate(rate))?;
        Ok(Self {
            inner,
            rate,
            samples_in: 0,
            base: 0,
            current: None,
            silent_run: 0,
            lines_at_silence: 0,
            retime: Retime::default(),
            history: VecDeque::new(),
            raw_pos: 0,
            skew: 0,
            hold_pending: false,
            held: None,
            leftover: None,
            known_holes: Vec::new(),
        })
    }

    /// Switch scan-timing repair on or off; it takes effect from the next
    /// picture.
    pub fn set_retime(&mut self, retime: Retime) {
        self.retime = retime;
    }

    /// Whether scan-timing repair is on.
    #[must_use]
    pub fn retime(&self) -> Retime {
        self.retime
    }

    /// The input sample rate.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// A picture has started and has not ended yet.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.current.is_some()
    }

    /// Display rows of the picture in flight whose audio has arrived, by the
    /// clock of the input; `None` between pictures. slowrx decodes only at
    /// the end, so this is the progress of the transmission, not of decoding.
    #[must_use]
    pub fn lines_heard(&self) -> Option<u32> {
        self.current
            .as_ref()
            .map(|p| p.rows_heard_by(self.samples_in))
    }

    /// How long the picture in flight takes to air, in seconds, by its
    /// mode's timing; `None` between pictures.
    #[must_use]
    pub fn expected_secs(&self) -> Option<f64> {
        self.current.as_ref().map(|p| {
            f64::from(p.image_rows) / f64::from(p.rows_per_radio_line) * p.line_samples
                / f64::from(self.rate)
        })
    }

    /// Input samples pushed since [`Decoder::new`].
    #[must_use]
    pub fn samples_in(&self) -> u64 {
        self.samples_in
    }

    /// Decode `samples` (any length) and return what happened in them.
    ///
    /// A call can take seconds: a whole-picture decode runs when a picture's
    /// audio is complete, and a silence abandon decodes the zero-filled rest
    /// of the picture. Never call it on the audio thread; hand samples to a
    /// worker.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Event> {
        let threshold = u64::from(SILENCE_SECS) * u64::from(self.rate);
        let mut out = Vec::new();
        let mut fed = 0;
        let mut run_start = None;
        for (i, &s) in samples.iter().enumerate() {
            if s.unsigned_abs() >= SILENCE_LEVEL.unsigned_abs() {
                self.silent_run = 0;
                run_start = None;
                continue;
            }
            if self.silent_run == 0 {
                run_start = Some(i);
            }
            self.silent_run += 1;
            if self.silent_run == threshold {
                // Decode up to the start of the silence first, so
                // `samples_in` marks exactly where it began.
                if let Some(start) = run_start.take() {
                    self.feed(&samples[fed..start], &mut out);
                    fed = start;
                    self.mark_silence();
                }
                self.feed(&samples[fed..=i], &mut out);
                fed = i + 1;
                if self.current.is_some() {
                    let lines = self.lines_at_silence;
                    out.extend(self.abandon(lines, true));
                }
            }
        }
        if let Some(start) = run_start {
            self.feed(&samples[fed..start], &mut out);
            fed = start;
            self.mark_silence();
        }
        self.feed(&samples[fed..], &mut out);
        out
    }

    /// `len` samples of audio known to be lost, such as the frames missing
    /// from audio rebuilt from captured packets.
    ///
    /// The decoder keeps their time: the picture's later lines stay where
    /// they belong, and the gap is never mistaken for stuffed zeros and cut.
    /// Once a picture's mode is known (and re-timing is on), the gap is
    /// filled with the audio from the same place one scan line earlier and
    /// the rows it touches are counted in [`Picture::repaired_rows`]. Before
    /// a header, or with re-timing off, it is fed as silence.
    pub fn push_hole(&mut self, len: usize) -> Vec<Event> {
        if len == 0 {
            return Vec::new();
        }
        let (Some(pic), Some(held)) = (self.current.as_ref(), self.held.as_ref()) else {
            return self.push(&vec![0; len]);
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let line = pic.line_samples.round() as usize;
        let have = held.audio.len();
        let mut fill = Vec::with_capacity(len);
        for i in 0..len {
            let s = match (have + i).checked_sub(line) {
                Some(src) if src < have => held.audio[src],
                Some(src) if line > 0 => fill[src - have],
                _ => 0,
            };
            fill.push(s);
        }
        self.known_holes.push((self.samples_in, len as u64));
        self.push(&fill)
    }

    /// Feed audio rebuilt from packets: `samples`, of which the spans in
    /// `holes` (first sample, length; in order, not overlapping) are known
    /// to be lost. The rest goes to [`Self::push`] and each hole to
    /// [`Self::push_hole`], in 20 ms pieces as a live tap would deliver it.
    pub fn push_rebuilt(&mut self, samples: &[i16], holes: &[(u64, u32)]) -> Vec<Event> {
        let step = (self.rate as usize / 50).max(1);
        let mut out = Vec::new();
        let mut at = 0;
        for &(start, len) in holes {
            let start = usize::try_from(start)
                .unwrap_or(usize::MAX)
                .min(samples.len());
            if start < at {
                continue;
            }
            for piece in samples[at..start].chunks(step) {
                out.extend(self.push(piece));
            }
            out.extend(self.push_hole(len as usize));
            at = (start + len as usize).min(samples.len());
        }
        for piece in samples[at..].chunks(step) {
            out.extend(self.push(piece));
        }
        out
    }

    /// The stream has ended: a picture in flight is abandoned with every row
    /// heard, and the decoder starts afresh. `None` when nothing was in
    /// flight, so calling it twice is harmless.
    pub fn finish(&mut self) -> Option<Event> {
        let cur = self.current.as_ref()?;
        // Audio since a silence began is not picture: keep only the rows
        // heard before it.
        let lines = if self.silent_run > 0 {
            self.lines_at_silence
        } else {
            cur.rows_heard_by(self.samples_in)
        };
        self.silent_run = 0;
        self.abandon(lines, false).pop()
    }

    fn mark_silence(&mut self) {
        let at = self.samples_in;
        self.lines_at_silence = self.current.as_ref().map_or(0, |p| p.rows_heard_by(at));
    }

    /// Give up on the picture in flight, keeping its first `lines` display
    /// rows, and start afresh.
    ///
    /// slowrx buffers a picture's whole audio and decodes every row in one
    /// burst at the end, so nothing is decoded yet. Zeros fill out the rest
    /// of the picture's length (they are not counted in `samples_in`), and
    /// the rows whose audio was heard are kept from what slowrx decodes.
    /// With `with_lines`, those rows are reported as `Line`s first.
    fn abandon(&mut self, lines: u32, with_lines: bool) -> Vec<Event> {
        if self.current.is_none() {
            return Vec::new();
        }
        let mut image = None;
        // Re-time whatever audio of the picture is held, and give it to
        // slowrx first.
        if let Some(audio) = self.release_held(true) {
            for event in self.inner.process(&audio) {
                if let SstvEvent::ImageComplete { image: done, .. } = event {
                    image = Some(done.pixels);
                }
            }
        }
        let Some(pic) = self.current.take() else {
            return Vec::new();
        };
        let chunk = vec![0.0_f32; self.rate as usize];
        // The picture's own length, the decoder's headroom and a margin.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let budget = (f64::from(pic.image_rows / pic.rows_per_radio_line) * pic.line_samples
            / f64::from(self.rate)
            * 1.5) as u32
            + 4;
        for _ in 0..budget {
            if image.is_some() {
                break;
            }
            for event in self.inner.process(&chunk) {
                if let SstvEvent::ImageComplete { image: done, .. } = event {
                    image = Some(done.pixels);
                }
            }
        }
        self.reset_inner();

        let keep = lines.min(pic.height);
        let stride = pic.width as usize * 3;
        let mut rgb = image.map_or_else(
            || vec![0; stride * pic.height as usize],
            |pixels| doubled(&pixels, pic.width, pic.line_height, pic.height),
        );
        let repaired_rows = pic.repair_rows(&mut rgb, keep);
        rgb[keep as usize * stride..].fill(0);
        let mut out = Vec::new();
        if with_lines {
            for row in 0..keep {
                let at = row as usize * stride;
                out.push(Event::Line {
                    row,
                    rgb: rgb[at..at + stride].to_vec(),
                });
            }
        }
        out.push(Event::Abandoned(Picture {
            mode: pic.mode,
            width: pic.width,
            height: pic.height,
            rgb,
            lines: keep,
            quality: 0.0,
            at_sample: pic.at_sample,
            partial: true,
            repaired_rows,
        }));
        out
    }

    fn reset_inner(&mut self) {
        self.inner.reset();
        self.base = self.samples_in;
        self.skew = 0;
        self.held = None;
        self.hold_pending = false;
        self.history.clear();
        self.raw_pos = self.samples_in;
        self.known_holes.clear();
    }

    fn feed(&mut self, samples: &[i16], out: &mut Vec<Event>) {
        if samples.is_empty() {
            return;
        }
        self.samples_in += samples.len() as u64;
        self.feed_raw(samples.to_vec(), out);
    }

    /// Give input to slowrx, or hold it while a picture is being gathered
    /// for re-timing.
    fn feed_raw(&mut self, mut samples: Vec<i16>, out: &mut Vec<Event>) {
        // While nothing is held, slowrx gets the input in 20 ms pieces, so a
        // header is noticed before the picture after it has gone through.
        let step = (self.rate as usize / 50).max(1);
        let keep = (self.rate * HISTORY_SECS) as usize;
        let mut at = 0;
        while at < samples.len() {
            if let Some(held) = self.held.as_mut() {
                held.audio.extend_from_slice(&samples[at..]);
                self.raw_pos += (samples.len() - at) as u64;
                if held.audio.len() < held.target {
                    return;
                }
                let Some(audio) = self.release_held(false) else {
                    return;
                };
                let rest = self.leftover.take().unwrap_or_default();
                for event in self.inner.process(&audio) {
                    self.handle(event, out);
                }
                samples = rest;
                at = 0;
                continue;
            }
            let end = if self.retime == Retime::On {
                (at + step).min(samples.len())
            } else {
                samples.len()
            };
            let piece = &samples[at..end];
            let audio: Vec<f32> = piece.iter().map(|&s| f32::from(s) / 32768.0).collect();
            if self.retime == Retime::On {
                self.history.extend(piece);
                let excess = self.history.len().saturating_sub(keep);
                self.history.drain(..excess);
            }
            self.raw_pos += piece.len() as u64;
            at = end;
            for event in self.inner.process(&audio) {
                self.handle(event, out);
            }
            if self.hold_pending {
                self.hold_pending = false;
                self.start_hold();
            }
        }
    }

    /// A picture's header has just been heard: hold its audio from here,
    /// starting with what slowrx was already given after the header.
    fn start_hold(&mut self) {
        let Some(pic) = self.current.as_ref() else {
            return;
        };
        let given = usize::try_from(self.raw_pos.saturating_sub(pic.at_sample)).unwrap_or(0);
        let have = given.min(self.history.len());
        let mut audio = vec![0; given - have];
        audio.extend(self.history.range(self.history.len() - have..));
        let spec = &pic.spec;
        // The picture and a little more, for a slow clock or stuffed
        // audio. Lost audio makes the picture end sooner, never later.
        let secs = (f64::from(radio_lines(spec)) + 0.25) * spec.line_seconds + 0.05;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let target = (secs * f64::from(self.rate)).ceil() as usize;
        self.held = Some(Held {
            audio,
            given,
            target,
        });
    }

    /// Re-time the held picture and return the audio slowrx still needs, on
    /// a perfect grid. The input after the picture is left in `leftover`
    /// (dropped when `abandoning`). Marks the rows to repair.
    fn release_held(&mut self, abandoning: bool) -> Option<Vec<f32>> {
        let held = self.held.take()?;
        let pic = self.current.as_mut()?;
        let spec = pic.spec;
        // Known holes, as spans of the held audio, which starts at the end
        // of the header.
        let known: Vec<(usize, usize)> = std::mem::take(&mut self.known_holes)
            .into_iter()
            .filter_map(|(start, len)| {
                let from = start.max(pic.at_sample) - pic.at_sample;
                let to = (start + len).saturating_sub(pic.at_sample);
                let from = usize::try_from(from).ok()?.min(held.audio.len());
                let to = usize::try_from(to).ok()?.min(held.audio.len());
                (to > from).then_some((from, to - from))
            })
            .collect();
        let plan = Plan::new(&held.audio, held.given, self.rate, &spec, &known);

        pic.repair = display_rows(&plan.damaged, &spec, pic.line_height, pic.height);
        pic.patched = display_rows(&plan.patched, &spec, pic.line_height, pic.height);

        // The picture's nominal length and one line more, but only as far
        // as the input reaches: what comes after the picture goes to slowrx
        // as it is (or, abandoning, slowrx is given silence), never made-up
        // zeros ahead of the next real audio.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let nominal =
            ((f64::from(radio_lines(&spec)) + 1.0) * spec.line_seconds * f64::from(self.rate))
                .ceil() as usize;
        let length = plan.covered(held.given, nominal);
        let used = plan.raw_at(length, held.audio.len());
        let audio = plan.render(held.given, length);

        self.skew +=
            i64::try_from(used).unwrap_or(i64::MAX) - i64::try_from(length).unwrap_or(i64::MAX);
        self.raw_pos = pic.at_sample + used as u64;
        self.history.clear();
        self.leftover = (!abandoning).then(|| held.audio[used..].to_vec());
        Some(audio)
    }

    fn handle(&mut self, event: SstvEvent, out: &mut Vec<Event>) {
        match event {
            SstvEvent::VisDetected {
                mode,
                sample_offset,
                ..
            } => {
                let spec = slowrx::for_mode(mode);
                let line_height = spec.line_height.max(1);
                let width = spec.line_pixels;
                let height = spec.image_lines * line_height;
                let inner_at = self.base
                    + sample_offset * u64::from(self.rate) / u64::from(WORKING_SAMPLE_RATE_HZ);
                let at_sample = inner_at.saturating_add_signed(self.skew);
                let rows_per_radio_line = match spec.channel_layout {
                    slowrx::ChannelLayout::PdYcbcr => 2,
                    _ => 1,
                };
                let mode = Mode::from_slowrx(mode);
                self.current = Some(InFlight {
                    mode,
                    width,
                    height,
                    line_height,
                    rows_per_radio_line,
                    line_samples: spec.line_seconds * f64::from(self.rate),
                    image_rows: spec.image_lines,
                    at_sample,
                    spec,
                    repair: Vec::new(),
                    patched: Vec::new(),
                });
                self.hold_pending = self.retime == Retime::On;
                out.push(Event::Started {
                    mode,
                    width,
                    height,
                    at_sample,
                });
            }
            SstvEvent::LineDecoded {
                line_index, pixels, ..
            } => {
                let Some(pic) = self.current.as_ref() else {
                    return;
                };
                let rgb: Vec<u8> = pixels
                    .iter()
                    .take(pic.width as usize)
                    .flatten()
                    .copied()
                    .collect();
                for k in 0..pic.line_height {
                    let row = line_index * pic.line_height + k;
                    if row >= pic.height {
                        break;
                    }
                    out.push(Event::Line {
                        row,
                        rgb: rgb.clone(),
                    });
                }
            }
            SstvEvent::ImageComplete {
                image,
                sync_quality,
                ..
            } => {
                let Some(pic) = self.current.take() else {
                    return;
                };
                if sync_quality.looks_like_noise() {
                    out.push(Event::Rejected { mode: pic.mode });
                    return;
                }
                let mut rgb = doubled(&image.pixels, pic.width, pic.line_height, pic.height);
                let repaired_rows = pic.repair_rows(&mut rgb, pic.height);
                if pic.repair.iter().chain(&pic.patched).any(|&r| r) {
                    patch_lines(out, &rgb, pic.width);
                }
                out.push(Event::Finished(Picture {
                    mode: pic.mode,
                    width: pic.width,
                    height: pic.height,
                    rgb,
                    lines: pic.height,
                    quality: sync_quality.value(),
                    at_sample: pic.at_sample,
                    partial: false,
                    repaired_rows,
                }));
            }
            // An unknown VIS code starts nothing; headerless starts are
            // never switched on, so nothing else can begin a picture.
            _ => {}
        }
    }
}

/// Per display row: whether any radio line marked in `lines` feeds it.
fn display_rows(lines: &[bool], spec: &ModeSpec, line_height: u32, height: u32) -> Vec<bool> {
    let mut rows = vec![false; height as usize];
    for (k, _) in lines.iter().enumerate().filter(|(_, m)| **m) {
        let k = u32::try_from(k).unwrap_or(u32::MAX);
        for row in rows_of_line(spec, k) {
            for r in row * line_height..(row + 1) * line_height {
                if let Some(slot) = rows.get_mut(r as usize) {
                    *slot = true;
                }
            }
        }
    }
    rows
}

/// Bring the `Line` events of the picture just decoded (the run of them at
/// the end of `out`) up to date with its repaired rows.
fn patch_lines(out: &mut [Event], rgb: &[u8], width: u32) {
    let stride = width as usize * 3;
    for event in out.iter_mut().rev() {
        let Event::Line { row, rgb: line } = event else {
            break;
        };
        let at = *row as usize * stride;
        if let Some(fixed) = rgb.get(at..at + stride) {
            line.copy_from_slice(fixed);
        }
    }
}

/// slowrx's on-air rows as display rows: each repeated `line_height` times,
/// padded or cut to exactly `height` rows.
fn doubled(pixels: &[[u8; 3]], width: u32, line_height: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let mut rgb = Vec::with_capacity(width * height as usize * 3);
    for row in pixels.chunks(width) {
        for _ in 0..line_height {
            rgb.extend(row.iter().flatten());
        }
    }
    rgb.resize(width * height as usize * 3, 0);
    rgb
}

/// Decode a whole stretch of audio rebuilt from packets (see
/// [`Decoder::push_rebuilt`]) at `rate`, re-timed, with enough silence
/// after it for the last picture to finish, and return every event.
///
/// # Errors
///
/// [`SstvError::Rate`] for an unsupported rate.
pub fn decode_rebuilt(
    rate: u32,
    samples: &[i16],
    holes: &[(u64, u32)],
) -> Result<Vec<Event>, SstvError> {
    let mut decoder = Decoder::new(rate)?;
    let mut out = decoder.push_rebuilt(samples, holes);
    // Past the silence that abandons a picture cut short, so a whole one
    // finishes and a partial one is given up on.
    let tail = vec![0; (SILENCE_SECS as usize + 1) * rate as usize];
    for piece in tail.chunks((rate as usize / 50).max(1)) {
        out.extend(decoder.push(piece));
    }
    out.extend(decoder.finish());
    Ok(out)
}
