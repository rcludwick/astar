// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Speech in, speech back: did the phrase come through the link whole?
//!
//! The speech-echo scenarios send one spoken phrase ([`link_test_phrase`])
//! into a link and record what comes back. [`compare`] lines the recording
//! up against the phrase and measures how faithfully it returned.
//!
//! Everything works on the **loudness envelope**: the RMS level of each
//! 20 ms frame, the same frame a voice packet carries. Speech rises and
//! falls word by word, so the envelope is a fingerprint of the phrase that
//! survives a codec (ulaw, GSM) which would wreck a sample-by-sample
//! comparison, and it does not care about polarity or a level change.
//!
//! The steps:
//!
//! 1. **Find the echo.** Slide the phrase's envelope along the recording's
//!    and keep the offset where they correlate best (Pearson), first frame by
//!    frame, then refined to the sample.
//! 2. **Measure the level.** The median, over the phrase's speech frames, of
//!    received dB minus sent dB.
//! 3. **Follow the timeline.** A link can repeat a chunk (a jitter buffer
//!    replaying) or stall (audio held back, then resumed late); after either,
//!    the rest of the phrase arrives later than the first part did. A
//!    Viterbi pass picks, for every phrase frame, how many frames late it
//!    arrived, allowing that lateness only to grow, and charging for each
//!    change, so a change is only taken when the rest of the phrase really
//!    lines up better after it.
//! 4. **Check every speech frame.** With each frame lined up, a speech frame
//!    is present when it arrived no more than 12 dB below where the level
//!    says it should be, and above -75 dBFS (so a silent recording cannot
//!    "match" at a gain of -120 dB). Runs of missing frames are gaps; where the lateness
//!    grew, the inserted audio is a repeat if it was speech and a stall if it
//!    was silence.
//!
//! "Speech frame" means a phrase frame within 30 dB of the phrase's loudest
//! frame; the rest are the pauses between words, which are allowed to come
//! back as anything quiet.
//!
//! [`SpeechComparison`] carries the measurements; [`SpeechThresholds`] says
//! what passes, and [`SpeechComparison::failures`] lists what did not, one
//! line each, ready to become scenario checks.

// Frame counts and sample offsets are small (minutes of audio at most), so
// the usize/i64/f64 conversions below cannot lose anything that matters.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::fmt::Write as _;

use crate::audio::{FRAME_MS, frame_len};

/// The committed test phrase (see `fixtures/README.md`).
const PHRASE_WAV: &[u8] = include_bytes!("../fixtures/link-test-phrase.wav");

/// Frames within this many dB of the phrase's loudest frame are speech.
pub const SPEECH_DB: f64 = 30.0;
/// A speech frame is missing when it arrived more than this many dB below
/// the level the rest of the echo sets.
pub const MISSING_DB: f64 = 12.0;
/// Levels more than this far below the phrase's loudest frame all count as
/// the same silence when lining frames up.
const FLOOR_DB: f64 = 45.0;
/// A frame-to-frame level mismatch saturates here: a frame is either about
/// right, somewhat off, or simply wrong.
const COST_CAP_DB: f64 = 20.0;
/// What a change in lateness costs, in frames of complete mismatch. Larger
/// than any hole the comparison should report as a gap, smaller than the
/// mismatch a 300 ms repeat leaves behind it.
const SHIFT_PENALTY: f64 = 4.0;
/// How far (in frames, either way) the timeline may drift from the best
/// single alignment: 1 s.
const MAX_SHIFT_FRAMES: i64 = 50;
/// "The first and last 200 ms".
pub const EDGE_MS: u32 = 200;
/// A gap is at least this many missing speech frames (40 ms); one frame on
/// its own is a word edge, not a hole.
const MIN_GAP_FRAMES: usize = 2;
/// A received frame quieter than this is silence however the level is set:
/// without it, a silent recording would "match" at a gain of -120 dB.
const SILENCE_DBFS: f64 = -75.0;
/// Below this RMS (i16 units) a frame is digital silence; keeps log10 finite.
const RMS_FLOOR: f64 = 1e-3;

/// A stretch of the phrase, in phrase time (0 = the phrase's first sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Where it starts, ms into the phrase.
    pub at_ms: u32,
    /// How long it is, ms.
    pub len_ms: u32,
}

/// How faithfully a recording carries the phrase. Built by [`compare`].
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechComparison {
    /// Sample rate everything below is measured at (the phrase's).
    pub rate: u32,
    /// Where the phrase's first sample landed in the recording, in samples
    /// at `rate`. Negative when the recording started after the phrase did.
    pub offset_samples: i64,
    /// The same offset in milliseconds.
    pub offset_ms: f64,
    /// Pearson correlation of the two loudness envelopes at the best single
    /// alignment: 1.0 is the same speech, near 0 is unrelated audio. A repeat
    /// or stall lowers it, because one alignment can only fit one side.
    pub correlation: f64,
    /// Received level minus sent level over the speech frames that arrived,
    /// dB (median, so a hole does not drag it down).
    pub level_diff_db: f64,
    /// Fraction of the phrase's speech frames that arrived.
    pub present_fraction: f64,
    /// Fraction of the speech frames in the phrase's first 200 ms that
    /// arrived.
    pub head_fraction: f64,
    /// Fraction of the speech frames in the phrase's last 200 ms that
    /// arrived.
    pub tail_fraction: f64,
    /// Runs of missing speech (two frames, 40 ms, or longer).
    pub gaps: Vec<Span>,
    /// Places where audio already heard was played again; `len_ms` is how
    /// much extra audio was inserted.
    pub repeats: Vec<Span>,
    /// Places where silence was inserted and the phrase carried on late;
    /// `len_ms` is how late.
    pub stalls: Vec<Span>,
    /// Number of 20 ms frames in the phrase, and how many of them are speech.
    pub frames: usize,
    /// See `frames`.
    pub speech_frames: usize,
}

/// What counts as the phrase coming back intact. The defaults are a
/// starting point; scenarios set theirs from measured runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeechThresholds {
    /// Lowest acceptable [`SpeechComparison::correlation`].
    pub min_correlation: f64,
    /// Largest acceptable `|level_diff_db|`.
    pub max_level_diff_db: f64,
    /// Lowest acceptable [`SpeechComparison::present_fraction`].
    pub min_present_fraction: f64,
    /// Lowest head / tail fraction for the first / last 200 ms to count as
    /// present.
    pub min_edge_fraction: f64,
    /// How many gaps, repeats and stalls are tolerated (each).
    pub max_gaps: usize,
    /// See `max_gaps`.
    pub max_repeats: usize,
    /// See `max_gaps`.
    pub max_stalls: usize,
}

impl Default for SpeechThresholds {
    fn default() -> Self {
        Self {
            min_correlation: 0.9,
            max_level_diff_db: 6.0,
            min_present_fraction: 0.97,
            min_edge_fraction: 0.8,
            max_gaps: 0,
            max_repeats: 0,
            max_stalls: 0,
        }
    }
}

impl SpeechComparison {
    /// The first 200 ms arrived (at least 80% of its speech frames).
    #[must_use]
    pub fn head_present(&self) -> bool {
        self.head_fraction >= SpeechThresholds::default().min_edge_fraction
    }

    /// The last 200 ms arrived (at least 80% of its speech frames).
    #[must_use]
    pub fn tail_present(&self) -> bool {
        self.tail_fraction >= SpeechThresholds::default().min_edge_fraction
    }

    /// Every way this comparison falls short of `t`, one line each; empty
    /// when the phrase came back intact.
    #[must_use]
    pub fn failures(&self, t: &SpeechThresholds) -> Vec<String> {
        let mut f = Vec::new();
        if self.correlation < t.min_correlation {
            f.push(format!(
                "loudness curve correlation {:.3} < {:.3}",
                self.correlation, t.min_correlation
            ));
        }
        if self.level_diff_db.is_nan() || self.level_diff_db.abs() > t.max_level_diff_db {
            f.push(format!(
                "level {:+.1} dB, outside ±{:.1} dB",
                self.level_diff_db, t.max_level_diff_db
            ));
        }
        if self.present_fraction < t.min_present_fraction {
            f.push(format!(
                "{:.1}% of the speech arrived, < {:.1}%",
                self.present_fraction * 100.0,
                t.min_present_fraction * 100.0
            ));
        }
        if self.head_fraction < t.min_edge_fraction {
            f.push(format!(
                "first {EDGE_MS} ms: {:.0}% arrived",
                self.head_fraction * 100.0
            ));
        }
        if self.tail_fraction < t.min_edge_fraction {
            f.push(format!(
                "last {EDGE_MS} ms: {:.0}% arrived",
                self.tail_fraction * 100.0
            ));
        }
        for (what, spans, max) in [
            ("gap", &self.gaps, t.max_gaps),
            ("repeat", &self.repeats, t.max_repeats),
            ("stall", &self.stalls, t.max_stalls),
        ] {
            if spans.len() > max {
                f.push(format!(
                    "{} {what}(s): {}",
                    spans.len(),
                    describe_spans(spans)
                ));
            }
        }
        f
    }

    /// One line: offset, correlation, level, presence, and any defects.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut s = format!(
            "offset {:.0} ms, correlation {:.3}, level {:+.1} dB, {:.1}% present (head {:.0}%, tail {:.0}%)",
            self.offset_ms,
            self.correlation,
            self.level_diff_db,
            self.present_fraction * 100.0,
            self.head_fraction * 100.0,
            self.tail_fraction * 100.0
        );
        for (what, spans) in [
            ("gaps", &self.gaps),
            ("repeats", &self.repeats),
            ("stalls", &self.stalls),
        ] {
            if !spans.is_empty() {
                let _ = write!(s, "; {what} {}", describe_spans(spans));
            }
        }
        s
    }
}

fn describe_spans(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|s| format!("{} ms at {} ms", s.len_ms, s.at_ms))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Mono 16-bit PCM and its sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// The samples.
    pub samples: Vec<i16>,
    /// Samples per second.
    pub rate: u32,
}

/// The committed test phrase: "This is the astar link test. One, two,
/// three, four, five. Testing complete." 8 kHz mono, about 5.2 s.
#[must_use]
pub fn link_test_phrase() -> Phrase {
    let (samples, rate) = parse_wav(PHRASE_WAV).expect("the committed phrase is a valid WAV");
    Phrase { samples, rate }
}

/// Read a mono 16-bit PCM WAV (the shape [`crate::audio::wav_bytes`]
/// writes): the samples and the sample rate.
///
/// # Errors
/// Not RIFF/WAVE, no `fmt ` or `data` chunk, or not mono 16-bit PCM.
pub fn parse_wav(bytes: &[u8]) -> Result<(Vec<i16>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut pos = 12;
    let mut rate = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(bytes, pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(len).min(bytes.len());
        match id {
            b"fmt " => {
                if end - body < 16 {
                    return Err("short fmt chunk".into());
                }
                let (format, channels, bits) = (
                    u16_at(bytes, body),
                    u16_at(bytes, body + 2),
                    u16_at(bytes, body + 14),
                );
                if format != 1 || channels != 1 || bits != 16 {
                    return Err(format!(
                        "need mono 16-bit PCM, got format {format}, {channels} channel(s), {bits} bits"
                    ));
                }
                rate = Some(u32_at(bytes, body + 4));
            }
            b"data" => {
                let rate = rate.ok_or("data chunk before fmt chunk")?;
                let samples = bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| i16::from_le_bytes(c))
                    .collect();
                return Ok((samples, rate));
            }
            _ => {}
        }
        pos = body + len + (len & 1);
    }
    Err("no data chunk".into())
}

/// Which 20 ms frames of `samples` are speech (within [`SPEECH_DB`] of the
/// loudest frame). All false for digital silence.
#[must_use]
pub fn speech_mask(samples: &[i16], rate: u32) -> Vec<bool> {
    let f = frame_len(rate);
    let n = samples.len() / f;
    let db: Vec<f64> = envelope(samples, 0, n, f).into_iter().map(to_db).collect();
    let peak = db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let silent = to_db(0.0);
    db.iter()
        .map(|&d| d > silent && d >= peak - SPEECH_DB)
        .collect()
}

/// Compare a recording against the phrase that was sent into the link.
///
/// `sent` and `recording` may be at different rates; the recording is
/// resampled to the phrase's rate first. Returns `None` only when `sent`
/// has no speech in it. An empty or silent recording is a comparison that
/// fails, not an error.
#[must_use]
pub fn compare(
    sent: &[i16],
    sent_rate: u32,
    recording: &[i16],
    recording_rate: u32,
) -> Option<SpeechComparison> {
    let resampled;
    let recv = if recording_rate == sent_rate {
        recording
    } else {
        resampled = resample(recording, recording_rate, sent_rate);
        &resampled
    };
    let rate = sent_rate;
    let f = frame_len(rate);
    let n = sent.len() / f;
    let speech = speech_mask(&sent[..n * f], rate);
    let speech_frames = speech.iter().filter(|&&s| s).count();
    if speech_frames == 0 {
        return None;
    }
    let env_s = envelope(sent, 0, n, f);
    let sdb: Vec<f64> = env_s.iter().copied().map(to_db).collect();
    let peak = sdb.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    // 1. Find the echo: frame-level search, then refine to the sample.
    let (offset0, correlation) = align(&env_s, recv, f);

    // 2. Level, from the single best alignment.
    let m = MAX_SHIFT_FRAMES;
    let first = offset0 - m * f as i64;
    let total = n + 2 * m as usize;
    let rdb: Vec<f64> = envelope(recv, first, total, f)
        .into_iter()
        .map(to_db)
        .collect();
    let r_at = |i: usize, lag: i64| rdb[(i as i64 + lag + m) as usize];
    let gain = median(
        (0..n)
            .filter(|&i| speech[i])
            .map(|i| r_at(i, 0) - sdb[i])
            .collect(),
    );

    // 3. Follow the timeline.
    let floor = peak - FLOOR_DB;
    let s_lvl: Vec<f64> = sdb.iter().map(|&d| d.max(floor)).collect();
    let r_lvl = |i: usize, lag: i64| (r_at(i, lag) - gain).max(floor);
    let lags = timeline(n, m, |i, lag| {
        (s_lvl[i] - r_lvl(i, lag)).abs().min(COST_CAP_DB) / COST_CAP_DB
    });

    // 4. Check every speech frame.
    let present: Vec<bool> = (0..n)
        .map(|i| {
            speech[i]
                && r_at(i, lags[i]) > SILENCE_DBFS
                && r_lvl(i, lags[i]) >= s_lvl[i] - MISSING_DB
        })
        .collect();
    let arrived: Vec<f64> = (0..n)
        .filter(|&i| present[i])
        .map(|i| r_at(i, lags[i]) - sdb[i])
        .collect();
    let level_diff_db = if arrived.is_empty() {
        gain
    } else {
        median(arrived)
    };
    let present_count = present.iter().filter(|&&p| p).count();

    let edge = (EDGE_MS / FRAME_MS) as usize;
    let first_speech = speech.iter().position(|&s| s).unwrap_or(0);
    let last_speech = speech.iter().rposition(|&s| s).unwrap_or(0);
    let fraction_in = |range: std::ops::Range<usize>| {
        let idx: Vec<usize> = range.filter(|&i| speech[i]).collect();
        idx.iter().filter(|&&i| present[i]).count() as f64 / idx.len().max(1) as f64
    };
    let head_fraction = fraction_in(first_speech..(first_speech + edge).min(n));
    let tail_fraction = fraction_in((last_speech + 1).saturating_sub(edge)..last_speech + 1);

    let gaps = find_gaps(&speech, &present);
    let (repeats, stalls) = find_shifts(&lags, r_lvl, peak - SPEECH_DB);

    let offset_samples = offset0 + lags[0] * f as i64;
    Some(SpeechComparison {
        rate,
        offset_samples,
        offset_ms: offset_samples as f64 * 1000.0 / f64::from(rate),
        correlation,
        level_diff_db,
        present_fraction: present_count as f64 / speech_frames as f64,
        head_fraction,
        tail_fraction,
        gaps,
        repeats,
        stalls,
        frames: n,
        speech_frames,
    })
}

/// The best single alignment of the phrase envelope `env_s` in `recv`:
/// (offset of the phrase's first sample in samples, envelope correlation).
fn align(env_s: &[f64], recv: &[i16], f: usize) -> (i64, f64) {
    let n = env_s.len() as i64;
    let fl = f as i64;
    let nr = (recv.len() / f) as i64 + 1;
    let env_r = envelope(recv, 0, nr as usize, f);
    let at = |j: i64| {
        if (0..nr).contains(&j) {
            env_r[j as usize]
        } else {
            0.0
        }
    };
    // The phrase may hang off either end of the recording by up to half.
    let mut best = (0i64, f64::NEG_INFINITY);
    let mut window = vec![0.0; env_s.len()];
    for lag in -(n / 2)..=(nr - n / 2).max(-(n / 2)) {
        for (i, w) in window.iter_mut().enumerate() {
            *w = at(i as i64 + lag);
        }
        let c = pearson(env_s, &window);
        if c > best.1 {
            best = (lag, c);
        }
    }
    // Refine to the sample within a frame either side.
    let coarse = best.0 * fl;
    let mut fine = (coarse, best.1);
    for d in (coarse - fl)..=(coarse + fl) {
        let c = pearson(env_s, &envelope(recv, d, env_s.len(), f));
        if c > fine.1 {
            fine = (d, c);
        }
    }
    (fine.0, fine.1.max(0.0))
}

/// Viterbi over lateness: for each phrase frame, a lag in
/// `-max..=max` frames relative to the best single alignment, never
/// decreasing, each increase costing [`SHIFT_PENALTY`]. `cost(i, lag)` is in
/// `0..=1`. Returns the lag of every frame.
fn timeline(n: usize, max: i64, cost: impl Fn(usize, i64) -> f64) -> Vec<i64> {
    let k = (2 * max + 1) as usize;
    let lag_of = |s: usize| s as i64 - max;
    // A whisper of preference for the single best alignment, to break ties
    // (frames of silence line up equally well anywhere).
    let bias = |s: usize| 1e-6 * lag_of(s).abs() as f64;
    let mut dp: Vec<f64> = (0..k).map(|s| cost(0, lag_of(s)) + bias(s)).collect();
    let mut back = vec![0usize; n * k];
    let mut next = vec![0.0; k];
    for i in 1..n {
        let (mut best_lower, mut best_lower_at) = (f64::INFINITY, 0);
        for s in 0..k {
            let (mut c, mut from) = (dp[s], s);
            if best_lower + SHIFT_PENALTY < c {
                c = best_lower + SHIFT_PENALTY;
                from = best_lower_at;
            }
            next[s] = c + cost(i, lag_of(s)) + bias(s);
            back[i * k + s] = from;
            if dp[s] < best_lower {
                best_lower = dp[s];
                best_lower_at = s;
            }
        }
        std::mem::swap(&mut dp, &mut next);
    }
    let mut s = (0..k)
        .min_by(|&a, &b| dp[a].total_cmp(&dp[b]))
        .unwrap_or(max as usize);
    let mut lags = vec![0; n];
    for i in (0..n).rev() {
        lags[i] = lag_of(s);
        s = back[i * k + s];
    }
    lags
}

/// Runs of at least [`MIN_GAP_FRAMES`] missing speech frames. Pause frames
/// inside a run neither end it nor count towards it.
fn find_gaps(speech: &[bool], present: &[bool]) -> Vec<Span> {
    let mut gaps = Vec::new();
    let mut run: Option<(usize, usize, usize)> = None; // (start, last, missing)
    let mut close = |run: &mut Option<(usize, usize, usize)>| {
        if let Some((start, last, missing)) = run.take()
            && missing >= MIN_GAP_FRAMES
        {
            gaps.push(span(start, last + 1 - start));
        }
    };
    for i in 0..speech.len() {
        if !speech[i] {
            continue;
        }
        if present[i] {
            close(&mut run);
        } else {
            run = Some(match run {
                Some((start, _, missing)) => (start, i, missing + 1),
                None => (i, i, 1),
            });
        }
    }
    close(&mut run);
    gaps
}

/// Where the lateness grew: a repeat when at least half the inserted frames
/// reached speech level (`speech_lvl`), a stall when they stayed quiet. Half,
/// not any: a shift that falls mid-frame leaves part of a word in the first
/// inserted frame even when what was inserted is silence.
fn find_shifts(
    lags: &[i64],
    r_lvl: impl Fn(usize, i64) -> f64,
    speech_lvl: f64,
) -> (Vec<Span>, Vec<Span>) {
    let (mut repeats, mut stalls) = (Vec::new(), Vec::new());
    for i in 1..lags.len() {
        let grew = lags[i] - lags[i - 1];
        if grew <= 0 {
            continue;
        }
        // Frames i-1+lags[i-1]+1 ..= i-1+lags[i-1]+grew of the recording
        // (relative to the phrase's own frame numbers) were inserted; read
        // them as phrase frame i-1 with lags of lags[i-1]+1 ..= +grew.
        let loud_frames = (1..=grew)
            .filter(|&d| r_lvl(i - 1, lags[i - 1] + d) >= speech_lvl)
            .count() as i64;
        let loud = 2 * loud_frames >= grew;
        let s = span(i, grew as usize);
        if loud {
            repeats.push(s);
        } else {
            stalls.push(s);
        }
    }
    (repeats, stalls)
}

fn span(start_frame: usize, frames: usize) -> Span {
    Span {
        at_ms: start_frame as u32 * FRAME_MS,
        len_ms: frames as u32 * FRAME_MS,
    }
}

/// RMS (i16 units) of `frames` consecutive frames of `f` samples starting
/// at sample `start`; samples outside `x` count as zero.
fn envelope(x: &[i16], start: i64, frames: usize, f: usize) -> Vec<f64> {
    (0..frames)
        .map(|j| {
            let s = start + (j * f) as i64;
            let lo = s.clamp(0, x.len() as i64) as usize;
            let hi = (s + f as i64).clamp(0, x.len() as i64) as usize;
            let sum: f64 = x[lo..hi]
                .iter()
                .map(|&v| {
                    let v = f64::from(v);
                    v * v
                })
                .sum();
            (sum / f as f64).sqrt()
        })
        .collect()
}

fn to_db(rms: f64) -> f64 {
    20.0 * (rms.max(RMS_FLOOR) / 32768.0).log10()
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let ma = a[..n].iter().sum::<f64>() / n as f64;
    let mb = b[..n].iter().sum::<f64>() / n as f64;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (da, db) = (a[i] - ma, b[i] - mb);
        sab += da * db;
        saa += da * da;
        sbb += db * db;
    }
    if saa <= 0.0 || sbb <= 0.0 {
        0.0
    } else {
        sab / (saa * sbb).sqrt()
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        f64::midpoint(v[m - 1], v[m])
    }
}

/// Linear-interpolation resample. Enough for a loudness envelope: the
/// comparison never looks at anything finer than a 20 ms frame.
#[must_use]
pub fn resample(x: &[i16], from: u32, to: u32) -> Vec<i16> {
    if x.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    let out_len = (x.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    let step = f64::from(from) / f64::from(to);
    (0..out_len)
        .map(|j| {
            let pos = j as f64 * step;
            let i = pos.floor() as usize;
            let frac = pos - i as f64;
            let a = f64::from(x[i.min(x.len() - 1)]);
            let b = f64::from(x[(i + 1).min(x.len() - 1)]);
            (a + (b - a) * frac).round() as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8000;

    fn phrase() -> Vec<i16> {
        let p = link_test_phrase();
        assert_eq!(p.rate, RATE);
        p.samples
    }

    fn silence(ms: u32) -> Vec<i16> {
        vec![0; (RATE * ms / 1000) as usize]
    }

    fn cat(parts: &[&[i16]]) -> Vec<i16> {
        parts.concat()
    }

    fn scale(x: &[i16], gain: f64) -> Vec<i16> {
        #[allow(clippy::cast_possible_truncation)]
        x.iter()
            .map(|&s| (f64::from(s) * gain).round().clamp(-32768.0, 32767.0) as i16)
            .collect()
    }

    /// Deterministic xorshift noise, roughly Gaussian with the given RMS.
    fn noise(n: usize, rms: f64, mut seed: u64) -> Vec<i16> {
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            #[allow(clippy::cast_precision_loss)]
            let u = (seed >> 11) as f64 / (1u64 << 53) as f64;
            u
        };
        (0..n)
            .map(|_| {
                let g = (0..12).map(|_| next()).sum::<f64>() - 6.0;
                #[allow(clippy::cast_possible_truncation)]
                let s = (g * rms).round().clamp(-32768.0, 32767.0) as i16;
                s
            })
            .collect()
    }

    /// Start (in samples) of the first run of `ms` milliseconds at or after
    /// `from_ms` in which every 20 ms frame of the phrase is speech.
    fn speech_run(p: &[i16], from_ms: u32, ms: u32) -> usize {
        let f = frame_len(RATE);
        let need = (ms / FRAME_MS) as usize;
        let speech = speech_mask(p, RATE);
        let first = (from_ms / FRAME_MS) as usize;
        let mut run = 0;
        for (i, &s) in speech.iter().enumerate().skip(first) {
            run = if s { run + 1 } else { 0 };
            if run == need {
                return (i + 1 - need) * f;
            }
        }
        panic!("no {ms} ms speech run after {from_ms} ms");
    }

    fn assert_match(c: &SpeechComparison) {
        let f = c.failures(&SpeechThresholds::default());
        assert!(f.is_empty(), "expected a match, got {f:?}\n{c:#?}");
    }

    #[test]
    fn fixture_is_five_seconds_of_8k_speech_with_pauses() {
        let p = phrase();
        #[allow(clippy::cast_precision_loss)]
        let secs = p.len() as f64 / f64::from(RATE);
        assert!((4.5..6.0).contains(&secs), "{secs} s");
        let speech = speech_mask(&p, RATE);
        assert!(speech[0], "the phrase starts on speech");
        assert!(*speech.last().unwrap(), "the phrase ends on speech");
        // Pauses of 100 ms or more inside the phrase (commas, full stops).
        let mut pauses = 0;
        let mut run = 0;
        for &s in &speech {
            if s {
                if run >= 5 {
                    pauses += 1;
                }
                run = 0;
            } else {
                run += 1;
            }
        }
        assert!(pauses >= 3, "only {pauses} pauses");
    }

    #[test]
    fn identical_is_a_perfect_match() {
        let p = phrase();
        let c = compare(&p, RATE, &p, RATE).unwrap();
        assert_eq!(c.offset_samples, 0);
        assert!(c.correlation > 0.999, "{}", c.correlation);
        assert!(c.level_diff_db.abs() < 0.1, "{}", c.level_diff_db);
        assert!((c.present_fraction - 1.0).abs() < f64::EPSILON);
        assert!(c.head_present() && c.tail_present());
        assert!(c.gaps.is_empty() && c.repeats.is_empty() && c.stalls.is_empty());
        assert_match(&c);
    }

    #[test]
    fn delayed_echo_is_found_at_its_offset() {
        let p = phrase();
        let rec = cat(&[&silence(1234), &p, &silence(1000)]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        let want = i64::from(RATE * 1234 / 1000);
        assert!(
            (c.offset_samples - want).abs() <= 8,
            "{} vs {want}",
            c.offset_samples
        );
        assert!((c.offset_ms - 1234.0).abs() <= 1.0, "{}", c.offset_ms);
        assert_match(&c);
    }

    #[test]
    fn attenuated_echo_reports_the_level_drop() {
        let p = phrase();
        let rec = cat(&[&silence(500), &scale(&p, 0.25), &silence(500)]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert!((c.level_diff_db + 12.04).abs() < 0.5, "{}", c.level_diff_db);
        assert!(c.correlation > 0.99);
        assert!((c.present_fraction - 1.0).abs() < f64::EPSILON);
        // 12 dB down is outside the default ±6 dB, and only that fails.
        let f = c.failures(&SpeechThresholds::default());
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("level"), "{f:?}");
    }

    #[test]
    fn clipped_first_200_ms_is_missing_head() {
        let p = phrase();
        let cut = (RATE / 5) as usize;
        let rec = cat(&[&silence(500), &silence(200), &p[cut..], &silence(500)]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert!(!c.head_present(), "{c:#?}");
        assert!(c.head_fraction < 0.2, "{}", c.head_fraction);
        assert!(c.tail_present());
        // The rest still lines up where it should.
        assert!(
            (c.offset_samples - i64::from(RATE / 2)).abs() <= 8,
            "{}",
            c.offset_samples
        );
        assert!(c.repeats.is_empty() && c.stalls.is_empty(), "{c:#?}");
        assert!(
            c.failures(&SpeechThresholds::default())
                .iter()
                .any(|f| f.contains("first"))
        );
    }

    #[test]
    fn clipped_last_200_ms_is_missing_tail() {
        let p = phrase();
        let cut = p.len() - (RATE / 5) as usize;
        let rec = cat(&[&silence(300), &p[..cut], &silence(700)]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert!(c.head_present());
        assert!(!c.tail_present(), "{c:#?}");
    }

    #[test]
    fn a_100_ms_hole_is_one_gap_in_the_right_place() {
        let p = phrase();
        let at = speech_run(&p, 1500, 200) + 400; // inside 200 ms of speech
        let mut holed = p.clone();
        let len = (RATE / 10) as usize;
        holed[at..at + len].fill(0);
        let rec = cat(&[&silence(400), &holed, &silence(400)]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert_eq!(c.gaps.len(), 1, "{c:#?}");
        let g = c.gaps[0];
        #[allow(clippy::cast_possible_truncation)]
        let at_ms = (at * 1000 / RATE as usize) as u32;
        assert!(g.at_ms.abs_diff(at_ms) <= 20, "{g:?} vs {at_ms}");
        assert!((80..=120).contains(&g.len_ms), "{g:?}");
        assert!(c.present_fraction < 1.0 && c.present_fraction > 0.9);
        assert!(c.repeats.is_empty() && c.stalls.is_empty(), "{c:#?}");
        assert!(c.head_present() && c.tail_present());
        assert!(
            c.failures(&SpeechThresholds::default())
                .iter()
                .any(|f| f.contains("gap"))
        );
    }

    #[test]
    fn a_repeated_chunk_is_one_repeat() {
        let p = phrase();
        let chunk = (RATE * 3 / 10) as usize; // 300 ms
        let t = speech_run(&p, 2000, 300) + chunk; // the 300 ms before t is speech
        let rec = cat(&[
            &silence(400),
            &p[..t],
            &p[t - chunk..t],
            &p[t..],
            &silence(400),
        ]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert_eq!(c.repeats.len(), 1, "{c:#?}");
        let r = c.repeats[0];
        #[allow(clippy::cast_possible_truncation)]
        let t_ms = (t * 1000 / RATE as usize) as u32;
        assert!(r.at_ms.abs_diff(t_ms) <= 100, "{r:?} vs {t_ms}");
        assert!(r.len_ms.abs_diff(300) <= 20, "{r:?}");
        assert!(c.stalls.is_empty(), "{c:#?}");
        assert!(c.gaps.is_empty(), "{c:#?}");
        // Offset is where the phrase's start landed.
        assert!(
            (c.offset_samples - i64::from(RATE * 4 / 10)).abs() <= 8,
            "{}",
            c.offset_samples
        );
        assert!(
            c.failures(&SpeechThresholds::default())
                .iter()
                .any(|f| f.contains("repeat"))
        );
    }

    #[test]
    fn inserted_silence_is_a_stall_not_a_repeat() {
        let p = phrase();
        let t = speech_run(&p, 2000, 300) + 1200;
        let rec = cat(&[
            &silence(400),
            &p[..t],
            &silence(300),
            &p[t..],
            &silence(400),
        ]);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert_eq!(c.stalls.len(), 1, "{c:#?}");
        assert!(c.stalls[0].len_ms.abs_diff(300) <= 20, "{c:#?}");
        assert!(c.repeats.is_empty(), "{c:#?}");
    }

    #[test]
    fn pure_noise_is_not_a_match() {
        let p = phrase();
        let rec = noise(RATE as usize * 8, 3000.0, 0x5eed);
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert!(c.correlation < 0.5, "{}", c.correlation);
        assert_ne!(
            c.failures(&SpeechThresholds::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn silence_is_not_a_match() {
        let p = phrase();
        let c = compare(&p, RATE, &silence(8000), RATE).unwrap();
        assert!(c.present_fraction < 0.01, "{c:#?}");
        assert_ne!(
            c.failures(&SpeechThresholds::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_empty_recording_is_not_a_match() {
        let p = phrase();
        let c = compare(&p, RATE, &[], RATE).unwrap();
        assert_ne!(
            c.failures(&SpeechThresholds::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_silent_phrase_cannot_be_compared() {
        assert!(compare(&silence(1000), RATE, &silence(1000), RATE).is_none());
    }

    #[test]
    fn ulaw_round_trip_with_delay_and_noise_still_matches() {
        let p = phrase();
        let mut bytes = vec![0u8; p.len()];
        astar_codec::g711::ulaw_encode_slice(&p, &mut bytes);
        let mut back = vec![0i16; p.len()];
        astar_codec::g711::ulaw_decode_slice(&bytes, &mut back);
        let quiet = noise(RATE as usize * 7, 30.0, 7); // about -60 dBFS hiss
        let mut rec = cat(&[&silence(777), &back, &silence(1000)]);
        for (r, n) in rec.iter_mut().zip(&quiet) {
            *r = r.saturating_add(*n);
        }
        let c = compare(&p, RATE, &rec, RATE).unwrap();
        assert!(c.correlation > 0.99, "{}", c.correlation);
        assert!(c.level_diff_db.abs() < 0.5, "{}", c.level_diff_db);
        assert!(
            c.gaps.is_empty() && c.repeats.is_empty() && c.stalls.is_empty(),
            "{c:#?}"
        );
        assert_match(&c);
    }

    #[test]
    fn a_16k_recording_compares_against_the_8k_phrase() {
        let p = phrase();
        // Upsample by two with linear interpolation.
        let mut up = Vec::with_capacity(p.len() * 2);
        for w in p.windows(2) {
            up.push(w[0]);
            #[allow(clippy::cast_possible_truncation)]
            up.push(i16::midpoint(w[0], w[1]));
        }
        let rec = cat(&[&vec![0; 16000], &up, &vec![0; 8000]]);
        let c = compare(&p, RATE, &rec, 16000).unwrap();
        assert!((c.offset_ms - 1000.0).abs() <= 2.0, "{}", c.offset_ms);
        assert_match(&c);
    }

    #[test]
    fn parse_wav_reads_what_write_wav_writes() {
        let x: Vec<i16> = (0..400).map(|i| (i * 37 % 2000 - 1000) as i16).collect();
        let w = crate::audio::wav_bytes(16000, &x);
        let (y, rate) = parse_wav(&w).unwrap();
        assert_eq!(rate, 16000);
        assert_eq!(y, x);
        assert!(parse_wav(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(parse_wav(b"not a wav").is_err());
    }
}
