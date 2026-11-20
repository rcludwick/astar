// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The scene model of frame A: what one video frame shows, worked out from
//! the session's events and audio, with no pixels involved.
//!
//! A [`SceneBuilder`] is fed in media-time order: audio blocks, and the start,
//! talker and end of each over. [`SceneBuilder::frame_at`] then answers "what
//! does the frame at this offset show?" as a [`FrameState`]. It is pure: no
//! I/O and no clock, so the same inputs always give the same frames.
//!
//! Every moving value is quantised (the ring in thousandths of a turn, the
//! level bars and the scroll in whole mockup pixels), so two moments that look
//! the same compare equal. The renderer uses that equality as its cache key:
//! between overs, nothing changes but the clock once a second.
//!
//! "Mockup pixels" are CSS pixels of frame A in `video-frames.html`, which is
//! drawn at 1200×675; the renderer scales them to the output size.

use std::collections::{BTreeSet, VecDeque};

use astar_netarchive::{
    Call, PEAK_BINS, SHORT_OVER_MS, SessionMeta, base_call, carries_callsigns, network_label,
    peak_bins, split_call,
};
use jiff::fmt::temporal::Pieces;
use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp};

/// Off-air text (callsigns) and the net's own strings are cut to this many
/// characters before anything measures them: text fitting is quadratic in
/// length, and nothing longer could be read in the frame anyway.
pub const MAX_TEXT_CHARS: usize = 256;

/// How far a new bubble pushes the column up, in mockup px: one bubble
/// (padding, callsign, waveform and meta lines) plus the 12 px gap.
pub const BUBBLE_PITCH_PX: u16 = 107;

/// How long a new bubble takes to scroll into place.
pub const SCROLL_MS: u64 = 400;

/// What a bubble or the talker panel says when nobody was named.
pub const UNKNOWN_STATION: &str = "Unknown station";

/// Bubbles kept before the renderer says how many fit.
const DEFAULT_CAPACITY: usize = 5;
/// Never keep more closed overs than this, whatever the renderer reports.
const MAX_KEPT: usize = 64;
/// Live, the ring makes one turn per this much of the current over.
const LIVE_TURN_MS: u64 = 120_000;
/// Level bars: six windows of this length, newest on the right.
const BAR_WINDOW_MS: u64 = 50;
const BAR_MIN_PX: f64 = 22.0;
const BAR_MAX_PX: f64 = 60.0;
const BAR_FLOOR_DB: f64 = -50.0;
/// Recent audio kept for the level bars (more than six windows).
const LEVEL_KEEP_MS: u64 = 1_000;
/// An open over's peaks are kept as the loudest sample of each chunk; past
/// this many chunks, neighbours merge and the chunk doubles, so a stuck
/// over never grows without bound.
const MAX_CHUNKS: usize = 1 << 14;

/// What a session is, for the frame: from `session.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub net: String,
    /// `session.json`'s network: `"m17"`, `"dstar"`, ...
    pub network: String,
    pub target: String,
    /// The net's own UTC offset; every clock in the frame is at it.
    pub offset: Offset,
    /// Offset 0 of the session's audio.
    pub started_at: Timestamp,
    pub sample_rate: u32,
    /// Whether the recorder's own calls sit on the right.
    pub my_side: bool,
    pub my_callsigns: Vec<String>,
}

impl SessionInfo {
    /// The frame's view of a `session.json`. The offset is read from
    /// `window_start`, as the recordings page does (UTC when it has none).
    /// `fallback_my_calls` stands in for `my_callsigns` when the session has
    /// none (a stage-1 `session.json`), as the page's `fallback_my_calls`
    /// does.
    ///
    /// # Errors
    /// `started_at` is not an RFC 3339 time, or the sample rate is 0.
    pub fn from_meta(meta: &SessionMeta, fallback_my_calls: &[String]) -> Result<Self, String> {
        let started_at: Timestamp = meta
            .started_at
            .parse()
            .map_err(|_| "session.json's started_at is not an RFC 3339 time".to_owned())?;
        if meta.sample_rate == 0 {
            return Err("session.json's sample_rate is 0".to_owned());
        }
        let offset = Pieces::parse(&meta.window_start)
            .ok()
            .and_then(|p| p.offset())
            .map_or(Offset::UTC, |o| o.to_numeric_offset());
        Ok(Self {
            net: cap(&meta.net),
            network: cap(&meta.network),
            target: cap(&meta.target),
            offset,
            started_at,
            sample_rate: meta.sample_rate,
            my_side: meta.my_side,
            my_callsigns: if meta.my_callsigns.is_empty() {
                fallback_my_calls
            } else {
                &meta.my_callsigns
            }
            .iter()
            .map(|c| cap(c))
            .collect(),
        })
    }
}

/// Live stream or recorded video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The LIVE badge replaces the clock; the ring times the current over.
    Live,
    /// The clock shows; the ring shows progress through `total_ms`.
    Recorded { total_ms: u64 },
}

/// The "who's talking" panel while someone is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Talking {
    /// The base callsign, or [`UNKNOWN_STATION`].
    pub call: String,
    pub suffix: Option<String>,
    /// `"talking · 0:14"`.
    pub sub: String,
    /// The orb's six level bars, oldest first, in mockup px (22..=60).
    pub bars: [u8; 6],
}

/// One over in the conversation column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bubble {
    /// The base callsign, or [`UNKNOWN_STATION`].
    pub call: String,
    /// On the right, in the blue gradient.
    pub mine: bool,
    /// `"19:00:12 · 1:42"`, or `"19:02:24 · talking"` while it is open.
    pub meta: String,
    /// [`PEAK_BINS`] bars, 0..=100.
    pub peaks: Vec<u8>,
    /// Bars lit, 0..=[`PEAK_BINS`].
    pub played: u8,
    /// The over playing now: magenta border and glow.
    pub on: bool,
}

/// Everything one frame shows. Equal states draw identical frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameState {
    pub live: bool,
    /// Always `"ASTAR"`.
    pub brand: &'static str,
    /// The network chip: `"M17"`.
    pub chip: String,
    pub target: String,
    /// The net's name.
    pub title: String,
    /// Recorded: the wall clock `HH:MM:SS`. Live: empty (the LIVE badge).
    pub clock: String,
    /// Live: the wall clock. Recorded: `"19:02 / 1:31:02"`.
    pub footer_time: String,
    /// `"14 check-ins so far"`, or `"N overs so far"` on a network that
    /// names nobody.
    pub footer_count: String,
    pub talking: Option<Talking>,
    /// Between overs: who spoke last, drawn dimmed. `None` while someone
    /// talks.
    pub last_talker: Option<String>,
    /// The orb's ring, 0..=1000 thousandths of a turn.
    pub ring: u16,
    /// Oldest first; at most the column's capacity.
    pub bubbles: Vec<Bubble>,
    /// How far below its resting place the column is drawn, in mockup px;
    /// eases to 0 over [`SCROLL_MS`] after a bubble arrives.
    pub scroll: u16,
}

#[derive(Clone, Debug)]
struct Over {
    seq: u32,
    talker: Option<Call>,
    start_ms: u64,
}

#[derive(Clone, Debug)]
struct ClosedOver {
    over: Over,
    duration_ms: u64,
    peaks: Vec<u8>,
}

/// The loudest sample per chunk of the open over.
#[derive(Clone, Debug)]
struct PeakAcc {
    chunk_len: u64,
    cur_max: u16,
    cur_n: u64,
    chunks: Vec<u16>,
}

impl PeakAcc {
    fn new(rate: u32) -> Self {
        Self {
            chunk_len: u64::from(rate / 100).max(1),
            cur_max: 0,
            cur_n: 0,
            chunks: Vec::new(),
        }
    }

    fn push(&mut self, s: i16) {
        let a = u16::try_from(i32::from(s).unsigned_abs()).unwrap_or(u16::MAX);
        self.cur_max = self.cur_max.max(a);
        self.cur_n += 1;
        if self.cur_n >= self.chunk_len {
            self.chunks.push(self.cur_max);
            self.cur_max = 0;
            self.cur_n = 0;
            if self.chunks.len() >= MAX_CHUNKS {
                self.chunks = self
                    .chunks
                    .chunks(2)
                    .map(|p| p.iter().copied().max().unwrap_or(0))
                    .collect();
                self.chunk_len *= 2;
            }
        }
    }

    fn bins(&self) -> Vec<u8> {
        if self.cur_n == 0 {
            peak_bins(&self.chunks)
        } else {
            let mut frames = self.chunks.clone();
            frames.push(self.cur_max);
            peak_bins(&frames)
        }
    }
}

/// The last [`LEVEL_KEEP_MS`] of audio, by absolute sample index.
#[derive(Clone, Debug)]
struct Level {
    rate: u64,
    start: u64,
    buf: VecDeque<i16>,
}

impl Level {
    fn keep(&self) -> u64 {
        self.rate * LEVEL_KEEP_MS / 1_000
    }

    fn push(&mut self, at: u64, samples: &[i16]) {
        let end = self.start + self.buf.len() as u64;
        let mut samples = samples;
        if self.buf.is_empty() {
            self.start = at;
        } else if at > end {
            let gap = at - end;
            if gap > self.keep() {
                self.buf.clear();
                self.start = at;
            } else {
                self.buf
                    .extend(std::iter::repeat_n(0, usize::try_from(gap).unwrap_or(0)));
            }
        } else if at < end {
            let skip = usize::try_from(end - at).unwrap_or(usize::MAX);
            samples = samples.get(skip..).unwrap_or(&[]);
        }
        self.buf.extend(samples);
        let keep = usize::try_from(self.keep()).unwrap_or(usize::MAX);
        while self.buf.len() > keep {
            self.buf.pop_front();
            self.start += 1;
        }
    }

    /// RMS over samples `from..to`; samples not held count as silence.
    fn rms(&self, from: u64, to: u64) -> f64 {
        if to <= from {
            return 0.0;
        }
        let lo = from.max(self.start);
        let hi = to.min(self.start + self.buf.len() as u64);
        let mut sum = 0.0_f64;
        for i in lo..hi {
            let s = f64::from(self.buf[usize::try_from(i - self.start).unwrap_or(0)]);
            sum += s * s;
        }
        #[allow(clippy::cast_precision_loss)] // a window is a few hundred samples
        let n = (to - from) as f64;
        (sum / n).sqrt()
    }
}

/// Builds [`FrameState`]s from a session's events and audio.
#[derive(Clone, Debug)]
pub struct SceneBuilder {
    info: SessionInfo,
    mode: Mode,
    chip: String,
    my_bases: Vec<String>,
    capacity: usize,
    /// Closed overs long enough to be bubbles, oldest first.
    closed: VecDeque<ClosedOver>,
    open: Option<(Over, PeakAcc)>,
    /// The label of the last closed over, short ones included.
    last: Option<String>,
    seen: BTreeSet<String>,
    overs: u32,
    level: Level,
}

impl SceneBuilder {
    #[must_use]
    pub fn new(info: SessionInfo, mode: Mode) -> Self {
        let my_bases = info
            .my_callsigns
            .iter()
            .filter_map(|c| base_call(c))
            .collect();
        let chip = network_label(&info.network);
        let rate = u64::from(info.sample_rate.max(1));
        Self {
            info,
            mode,
            chip,
            my_bases,
            capacity: DEFAULT_CAPACITY,
            closed: VecDeque::new(),
            open: None,
            last: None,
            seen: BTreeSet::new(),
            overs: 0,
            level: Level {
                rate,
                start: 0,
                buf: VecDeque::new(),
            },
        }
    }

    /// How many bubbles fit the column (the renderer's
    /// `column_capacity`). Older ones are dropped.
    pub fn set_capacity(&mut self, bubbles: usize) {
        self.capacity = bubbles.min(MAX_KEPT);
    }

    /// Mono PCM at the session's rate, starting `offset_ms` into it.
    pub fn audio(&mut self, offset_ms: u64, samples: &[i16]) {
        let at = offset_ms * self.level.rate / 1_000;
        if let Some((over, acc)) = &mut self.open {
            let over_at = over.start_ms * self.level.rate / 1_000;
            let skip = usize::try_from(over_at.saturating_sub(at)).unwrap_or(usize::MAX);
            for &s in samples.get(skip..).unwrap_or(&[]) {
                acc.push(s);
            }
        }
        self.level.push(at, samples);
    }

    /// An over begins. One still open is closed where this one starts.
    pub fn over_start(&mut self, seq: u32, talker: Option<&str>, offset_ms: u64) {
        if let Some((prev, _)) = &self.open {
            let (prev_seq, dur) = (prev.seq, offset_ms.saturating_sub(prev.start_ms));
            self.over_end(prev_seq, dur);
        }
        let talker = talker.and_then(call_of);
        if let Some(c) = &talker {
            self.seen.insert(c.base.clone());
        }
        self.overs = self.overs.saturating_add(1);
        self.open = Some((
            Over {
                seq,
                talker,
                start_ms: offset_ms,
            },
            PeakAcc::new(self.info.sample_rate),
        ));
    }

    /// The talker of over `seq` became known after it started.
    pub fn over_talker(&mut self, seq: u32, talker: &str) {
        let Some(call) = call_of(talker) else { return };
        let target = match &mut self.open {
            Some((o, _)) if o.seq == seq => Some(o),
            _ => self
                .closed
                .iter_mut()
                .rev()
                .find(|c| c.over.seq == seq)
                .map(|c| &mut c.over),
        };
        if let Some(o) = target {
            self.seen.insert(call.base.clone());
            o.talker = Some(call);
        }
    }

    /// Over `seq` ended after `duration_ms`. Its peaks are the ones the
    /// archive would write, from the audio fed while it was open.
    pub fn over_end(&mut self, seq: u32, duration_ms: u64) {
        let Some((over, acc)) = self.open.take_if(|(o, _)| o.seq == seq) else {
            return;
        };
        self.last = Some(label(over.talker.as_ref()));
        if duration_ms < SHORT_OVER_MS {
            return;
        }
        self.closed.push_back(ClosedOver {
            over,
            duration_ms,
            peaks: acc.bins(),
        });
        while self.closed.len() > MAX_KEPT {
            self.closed.pop_front();
        }
    }

    /// Replace a closed over's peaks with the archive's own (`peaks.json`).
    pub fn over_peaks(&mut self, seq: u32, peaks: &[u8]) {
        if let Some(c) = self.closed.iter_mut().rev().find(|c| c.over.seq == seq) {
            c.peaks = peaks.iter().take(PEAK_BINS).map(|&p| p.min(100)).collect();
        }
    }

    /// What the frame `offset_ms` into the session shows.
    #[must_use]
    pub fn frame_at(&self, offset_ms: u64) -> FrameState {
        let live = matches!(self.mode, Mode::Live);
        let wall = self.clock_at(offset_ms);
        let open = self.open.as_ref().filter(|(o, _)| o.start_ms <= offset_ms);
        let talking = open.map(|(o, _)| Talking {
            call: label(o.talker.as_ref()),
            suffix: o.talker.as_ref().and_then(|c| c.suffix.clone()),
            sub: format!("talking · {}", span(offset_ms - o.start_ms)),
            bars: self.bars(offset_ms),
        });
        let ring = match self.mode {
            Mode::Live => open.map_or(0, |(o, _)| {
                thousandths((offset_ms - o.start_ms) % LIVE_TURN_MS, LIVE_TURN_MS)
            }),
            Mode::Recorded { total_ms } => thousandths(offset_ms.min(total_ms), total_ms),
        };
        let (bubbles, scroll) = self.column(offset_ms, open);
        FrameState {
            live,
            brand: "ASTAR",
            chip: self.chip.clone(),
            target: self.info.target.clone(),
            title: self.info.net.clone(),
            clock: if live { String::new() } else { wall.clone() },
            footer_time: match self.mode {
                Mode::Live => wall,
                Mode::Recorded { total_ms } => {
                    format!("{} / {}", span(offset_ms.min(total_ms)), span(total_ms))
                }
            },
            footer_count: self.footer_count(),
            last_talker: if talking.is_some() {
                None
            } else {
                self.last.clone()
            },
            talking,
            ring,
            bubbles,
            scroll,
        }
    }

    fn column(&self, t: u64, open: Option<&(Over, PeakAcc)>) -> (Vec<Bubble>, u16) {
        let appears = |o: &Over| o.start_ms + SHORT_OVER_MS;
        let mut all: Vec<(u64, Bubble)> = self
            .closed
            .iter()
            .filter(|c| appears(&c.over) <= t)
            .map(|c| {
                let b = Bubble {
                    call: label(c.over.talker.as_ref()),
                    mine: self.is_mine(c.over.talker.as_ref()),
                    meta: format!(
                        "{} · {}",
                        self.clock_at(c.over.start_ms),
                        span(c.duration_ms)
                    ),
                    peaks: c.peaks.clone(),
                    played: 0,
                    on: false,
                };
                (appears(&c.over), b)
            })
            .collect();
        if let Some((o, acc)) = open.filter(|(o, _)| appears(o) <= t) {
            all.push((
                appears(o),
                Bubble {
                    call: label(o.talker.as_ref()),
                    mine: self.is_mine(o.talker.as_ref()),
                    meta: format!("{} · talking", self.clock_at(o.start_ms)),
                    peaks: acc.bins(),
                    played: u8::try_from(PEAK_BINS).unwrap_or(u8::MAX),
                    on: true,
                },
            ));
        }
        let keep = all.len().saturating_sub(self.capacity);
        let all = all.split_off(keep);
        let scroll = all.last().map_or(0, |(at, _)| scroll_at(t - at));
        (all.into_iter().map(|(_, b)| b).collect(), scroll)
    }

    fn is_mine(&self, talker: Option<&Call>) -> bool {
        // As on the recordings page: a network that names nobody has no
        // "my side", whatever the session says.
        self.info.my_side
            && carries_callsigns(&self.info.network)
            && talker.is_some_and(|c| self.my_bases.contains(&c.base))
    }

    fn footer_count(&self) -> String {
        if carries_callsigns(&self.info.network) {
            let n = self.seen.len();
            format!("{n} check-in{} so far", plural(n))
        } else {
            let n = self.overs as usize;
            format!("{n} over{} so far", plural(n))
        }
    }

    /// The six level bars ending at `t`.
    fn bars(&self, t: u64) -> [u8; 6] {
        let rate = self.level.rate;
        let end = t * rate / 1_000;
        let w = (BAR_WINDOW_MS * rate / 1_000).max(1);
        let mut bars = [0u8; 6];
        for (k, bar) in bars.iter_mut().enumerate() {
            let back = (6 - k as u64) * w;
            let from = end.saturating_sub(back);
            let rms = self.level.rms(from, from + w);
            *bar = bar_px(rms);
        }
        bars
    }

    /// `HH:MM:SS` at the net's offset, `offset_ms` after the start.
    fn clock_at(&self, offset_ms: u64) -> String {
        let ms = i64::try_from(offset_ms).unwrap_or(i64::MAX);
        self.info
            .started_at
            .checked_add(SignedDuration::from_millis(ms))
            .unwrap_or(self.info.started_at)
            .to_zoned(TimeZone::fixed(self.info.offset))
            .strftime("%H:%M:%S")
            .to_string()
    }
}

/// The scroll `dt` ms after a bubble arrived: an ease-out cubic from one
/// bubble pitch down to 0.
fn scroll_at(dt: u64) -> u16 {
    if dt >= SCROLL_MS {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)] // dt < SCROLL_MS
    let x = dt as f64 / SCROLL_MS as f64;
    let left = (1.0 - x).powi(3);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..=pitch
    let px = (f64::from(BUBBLE_PITCH_PX) * left).round() as u16;
    px
}

/// −50…0 dBFS onto 22…60 px.
fn bar_px(rms: f64) -> u8 {
    const MIN: u8 = 22;
    if rms <= 0.0 {
        return MIN;
    }
    let db = (20.0 * (rms / 32_768.0).log10()).clamp(BAR_FLOOR_DB, 0.0);
    let px = BAR_MIN_PX + (BAR_MAX_PX - BAR_MIN_PX) * (db - BAR_FLOOR_DB) / -BAR_FLOOR_DB;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 22..=60
    let px = px.round() as u8;
    px
}

fn thousandths(part: u64, whole: u64) -> u16 {
    if whole == 0 {
        return 0;
    }
    u16::try_from((u128::from(part) * 1_000 / u128::from(whole)).min(1_000)).unwrap_or(1_000)
}

/// `0:14`, `1:42`, `1:31:02`.
fn span(ms: u64) -> String {
    let s = ms / 1_000;
    let (h, m, s) = (s / 3_600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn cap(s: &str) -> String {
    s.chars().take(MAX_TEXT_CHARS).collect()
}

/// An off-air talker, capped before and after splitting (uppercasing can
/// lengthen it).
fn call_of(raw: &str) -> Option<Call> {
    split_call(&cap(raw)).map(|c| Call {
        base: cap(&c.base),
        suffix: c.suffix.map(|s| cap(&s)),
    })
}

fn label(talker: Option<&Call>) -> String {
    talker.map_or_else(|| UNKNOWN_STATION.to_owned(), |c| c.base.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_netarchive::{Ended, SessionMeta};

    fn meta(network: &str, my_side: bool, mine: &[&str]) -> SessionMeta {
        SessionMeta {
            format: 1,
            net: "Tuesday hub net".into(),
            network: network.into(),
            target: "M17-XXX C".into(),
            window_start: "2026-11-03T18:55:00-07:00".into(),
            window_end: "2026-11-03T20:30:00-07:00".into(),
            visibility: "public".into(),
            my_side,
            sample_rate: 8_000,
            // 19:00:00 at -07:00.
            started_at: "2026-11-04T02:00:00Z".into(),
            ended_at: None,
            ended: None::<Ended>,
            rejoins: 0,
            dropped_frames: 0,
            overs: 0,
            astar_version: "0.0.0".into(),
            my_callsigns: mine.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn live() -> SceneBuilder {
        SceneBuilder::new(
            SessionInfo::from_meta(&meta("m17", false, &[]), &[]).expect("meta"),
            Mode::Live,
        )
    }

    /// `ms` of audio from `from` in 20 ms blocks, each sample from `f`.
    fn feed(b: &mut SceneBuilder, from: u64, ms: u64, f: impl Fn(u64) -> i16) {
        let mut t = from;
        while t < from + ms {
            let block: Vec<i16> = (0..160).map(|i| f(t * 8 + i)).collect();
            b.audio(t, &block);
            t += 20;
        }
    }

    fn over(b: &mut SceneBuilder, seq: u32, call: &str, at: u64, dur: u64) {
        b.over_start(seq, Some(call), at);
        b.over_end(seq, dur);
    }

    #[test]
    fn silence_gives_resting_bars() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        feed(&mut b, 0, 1_000, |_| 0);
        let t = b.frame_at(1_000).talking.expect("talking");
        assert_eq!(t.bars, [22; 6]);
    }

    #[test]
    fn full_scale_gives_tall_bars() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        feed(&mut b, 0, 1_000, |i| {
            if (i / 4) % 2 == 0 {
                i16::MAX
            } else {
                -i16::MAX
            }
        });
        let t = b.frame_at(1_000).talking.expect("talking");
        assert_eq!(t.bars, [60; 6]);
    }

    #[test]
    fn bars_follow_the_level_window_by_window() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        // Silent to 850 ms, then full scale: only the newest three windows
        // (850..1000) are loud.
        feed(
            &mut b,
            0,
            1_000,
            |i| if i >= 850 * 8 { i16::MAX } else { 0 },
        );
        let t = b.frame_at(1_000).talking.expect("talking");
        assert_eq!(t.bars, [22, 22, 22, 60, 60, 60]);
    }

    #[test]
    fn talking_sub_counts_seconds() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA  D"), 10_000);
        let f = b.frame_at(24_000);
        let t = f.talking.expect("talking");
        assert_eq!(t.sub, "talking · 0:14");
        assert_eq!(t.call, "KX0AAA");
        assert_eq!(t.suffix.as_deref(), Some("D"));
        assert_eq!(f.last_talker, None);
    }

    #[test]
    fn between_overs_shows_last_talker_dimmed() {
        let mut b = live();
        over(&mut b, 1, "N0CALL", 1_000, 5_000);
        over(&mut b, 2, "KX0AAA", 8_000, 4_000);
        let f = b.frame_at(20_000);
        assert_eq!(f.talking, None);
        assert_eq!(f.last_talker.as_deref(), Some("KX0AAA"));
        assert_eq!(f.ring, 0);
    }

    #[test]
    fn talker_named_late_is_picked_up() {
        let mut b = live();
        b.over_start(1, None, 0);
        assert_eq!(
            b.frame_at(500).talking.expect("talking").call,
            "Unknown station"
        );
        b.over_talker(1, "KX0AAA");
        assert_eq!(b.frame_at(600).talking.expect("talking").call, "KX0AAA");
        assert_eq!(b.frame_at(600).footer_count, "1 check-in so far");
    }

    #[test]
    fn recorded_ring_is_progress() {
        let info = SessionInfo::from_meta(&meta("m17", false, &[]), &[]).expect("meta");
        let b = SceneBuilder::new(info, Mode::Recorded { total_ms: 60_000 });
        let f = b.frame_at(15_000);
        assert_eq!(f.ring, 250);
        assert!(!f.live);
        assert_eq!(f.clock, "19:00:15");
        assert_eq!(f.footer_time, "0:15 / 1:00");
    }

    #[test]
    fn recorded_footer_reads_like_the_mockup() {
        let info = SessionInfo::from_meta(&meta("m17", false, &[]), &[]).expect("meta");
        let b = SceneBuilder::new(
            info,
            Mode::Recorded {
                total_ms: 5_462_000,
            },
        );
        assert_eq!(b.frame_at(1_142_000).footer_time, "19:02 / 1:31:02");
    }

    #[test]
    fn live_ring_wraps_at_two_minutes() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 1_000);
        let f = b.frame_at(131_000);
        assert_eq!(f.ring, 83);
        assert!(f.live);
        assert_eq!(f.clock, "");
        assert_eq!(f.footer_time, "19:02:11");
    }

    #[test]
    fn short_over_is_not_a_bubble() {
        let mut b = live();
        over(&mut b, 1, "KX0AAA", 1_000, 1_400);
        let f = b.frame_at(10_000);
        assert_eq!(f.bubbles.len(), 0);
        assert_eq!(f.last_talker.as_deref(), Some("KX0AAA"));
    }

    #[test]
    fn open_over_becomes_a_bubble_after_the_short_limit() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        assert_eq!(b.frame_at(1_000).bubbles, Vec::new());
        let f = b.frame_at(2_000);
        assert_eq!(f.bubbles.len(), 1);
        assert!(f.bubbles[0].on);
        assert_eq!(f.bubbles[0].meta, "19:00:00 · talking");
    }

    #[test]
    fn a_new_bubble_scrolls_in_then_settles() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        let start = b.frame_at(SHORT_OVER_MS).scroll;
        let mid = b.frame_at(SHORT_OVER_MS + 200).scroll;
        let end = b.frame_at(SHORT_OVER_MS + 400).scroll;
        assert_eq!(start, BUBBLE_PITCH_PX);
        assert!(mid > 0 && mid < start, "{mid}");
        assert_eq!(end, 0);
    }

    #[test]
    fn only_the_bubbles_that_fit_are_kept() {
        let mut b = live();
        b.set_capacity(3);
        for i in 0..6u32 {
            over(&mut b, i + 1, "KX0AAA", u64::from(i) * 10_000, 5_000);
        }
        let f = b.frame_at(100_000);
        assert_eq!(f.bubbles.len(), 3);
        assert_eq!(f.bubbles[2].meta, "19:00:50 · 0:05");
    }

    #[test]
    fn open_over_peaks_grow_and_closed_ones_keep_theirs() {
        let mut b = live();
        b.over_start(1, Some("KX0AAA"), 0);
        // Loud for the first second, silent for the second.
        feed(&mut b, 0, 2_000, |i| if i < 8_000 { i16::MAX } else { 0 });
        let open = b.frame_at(2_000).bubbles[0].clone();
        assert_eq!(open.peaks.len(), PEAK_BINS);
        assert_eq!(open.peaks[0], 100);
        assert_eq!(open.peaks[PEAK_BINS - 1], 0);
        assert_eq!(usize::from(open.played), PEAK_BINS);
        b.over_end(1, 2_000);
        let closed = b.frame_at(3_000).bubbles[0].clone();
        assert_eq!(closed.peaks, open.peaks);
        assert!(!closed.on);
        assert_eq!(closed.played, 0);
        let archive: Vec<u8> = (0..PEAK_BINS)
            .map(|i| u8::try_from(i).unwrap_or(0))
            .collect();
        b.over_peaks(1, &archive);
        assert_eq!(b.frame_at(3_000).bubbles[0].peaks, archive);
    }

    #[test]
    fn checkins_count_distinct_base_calls() {
        let mut b = live();
        // `KX0AAA  D` is KX0AAA on module D: the same station. (A `/P`
        // portable call is its own base call in the archive, so it is not
        // used here.)
        over(&mut b, 1, "KX0AAA", 0, 5_000);
        over(&mut b, 2, "KX0AAA  D", 10_000, 5_000);
        b.over_start(3, Some("N0CALL"), 20_000);
        assert_eq!(b.frame_at(21_000).footer_count, "2 check-ins so far");
    }

    #[test]
    fn a_network_without_callsigns_counts_overs() {
        let info = SessionInfo::from_meta(&meta("allstar", false, &[]), &[]).expect("meta");
        let mut b = SceneBuilder::new(info, Mode::Live);
        b.over_start(1, None, 0);
        b.over_end(1, 5_000);
        b.over_start(2, None, 6_000);
        let f = b.frame_at(7_000);
        assert_eq!(f.footer_count, "2 overs so far");
        assert_eq!(f.chip, "AllStar");
    }

    #[test]
    fn my_side_puts_my_call_right() {
        let info = SessionInfo::from_meta(&meta("m17", true, &["n0call"]), &[]).expect("meta");
        let mut b = SceneBuilder::new(info, Mode::Live);
        over(&mut b, 1, "KX0AAA", 0, 5_000);
        over(&mut b, 2, "N0CALL-1", 10_000, 5_000);
        let f = b.frame_at(20_000);
        assert!(!f.bubbles[0].mine);
        assert!(f.bubbles[1].mine);

        let info = SessionInfo::from_meta(&meta("m17", false, &["N0CALL"]), &[]).expect("meta");
        let mut b = SceneBuilder::new(info, Mode::Live);
        over(&mut b, 1, "N0CALL", 0, 5_000);
        assert!(!b.frame_at(10_000).bubbles[0].mine);
    }

    #[test]
    fn fallback_my_calls_stand_in_when_the_session_has_none() {
        let fallback = vec!["N0CALL".to_owned()];
        let info = SessionInfo::from_meta(&meta("m17", true, &[]), &fallback).expect("meta");
        assert_eq!(info.my_callsigns, fallback);
        let mut b = SceneBuilder::new(info, Mode::Live);
        over(&mut b, 1, "N0CALL", 0, 5_000);
        assert!(b.frame_at(10_000).bubbles[0].mine);

        // The session's own calls win over the fallback.
        let info =
            SessionInfo::from_meta(&meta("m17", true, &["KX0AAA"]), &fallback).expect("meta");
        let mut b = SceneBuilder::new(info, Mode::Live);
        over(&mut b, 1, "N0CALL", 0, 5_000);
        over(&mut b, 2, "KX0AAA", 10_000, 5_000);
        let f = b.frame_at(20_000);
        assert!(!f.bubbles[0].mine);
        assert!(f.bubbles[1].mine);
    }

    #[test]
    fn a_network_without_callsigns_has_no_my_side() {
        let info = SessionInfo::from_meta(&meta("allstar", true, &["N0CALL"]), &[]).expect("meta");
        let mut b = SceneBuilder::new(info, Mode::Live);
        over(&mut b, 1, "N0CALL", 0, 5_000);
        assert!(!b.frame_at(10_000).bubbles[0].mine);
    }

    #[test]
    fn identical_moments_compare_equal() {
        let mut b = live();
        over(&mut b, 1, "KX0AAA", 1_000, 5_000);
        feed(&mut b, 0, 51_000, |_| 0);
        assert_eq!(b.frame_at(50_100), b.frame_at(50_400));
    }

    #[test]
    fn meta_reads_like_the_mockup() {
        let mut b = live();
        over(&mut b, 1, "NC0NTL", 12_000, 102_000);
        let f = b.frame_at(200_000);
        assert_eq!(f.bubbles[0].meta, "19:00:12 · 1:42");
        assert_eq!(f.bubbles[0].call, "NC0NTL");
        assert_eq!(f.brand, "ASTAR");
        assert_eq!(f.chip, "M17");
        assert_eq!(f.target, "M17-XXX C");
        assert_eq!(f.title, "Tuesday hub net");
    }

    #[test]
    fn off_air_strings_are_capped() {
        let mut b = live();
        let long = "K".repeat(10_000);
        b.over_start(1, Some(&long), 0);
        let t = b.frame_at(100).talking.expect("talking");
        assert_eq!(t.call.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn from_meta_refuses_a_bad_start() {
        let mut m = meta("m17", false, &[]);
        m.started_at = "yesterday".into();
        assert!(SessionInfo::from_meta(&m, &[]).is_err());
    }
}
