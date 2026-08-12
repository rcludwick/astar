// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Adaptive jitter buffer ported from Steve Kann's `jitterbuf.c` (Horizon
//! Wimba, 2004-2005, later folded into Asterisk and the vendored iaxclient).
//!
//! This is a verbatim port of the C algorithm — the same delay history,
//! the same percentile-based jitter estimate, the same grow/shrink heuristics
//! — but in safe, generic Rust. The payload type is parameterised: callers
//! typically use `Vec<u8>` for encoded audio frames, but anything works.
//!
//! # Algorithm sketch
//!
//! Each incoming voice frame's transit delay `now - ts` is pushed into a
//! 500-slot ring history. A sorted "top/bottom 3%" view of that history
//! gives min/max; jitter = max - min. The target buffer depth is
//! `jitter + min + target_extra`, clamped by `max_jitterbuf`. On each
//! `get`, the buffer compares its current depth against the target and
//! decides whether to deliver the next frame, interpolate (lost/late),
//! drop (shrink), or wait.
//!
//! See `jitterbuf.c` for the historical rationale on each magic number.

use std::collections::VecDeque;

/// Number of historical delays kept for jitter estimation. Matches the
/// vendored `JB_HISTORY_SZ`.
const HISTORY_SZ: usize = 500;
/// Percentile of outliers to drop from each end of the delay history when
/// computing min/max. Matches `JB_HISTORY_DROPPCT` (3%).
const HISTORY_DROPPCT: usize = 3;
/// Hard cap on the percentile, controlling the size of the sorted side
/// buffers. Matches `JB_HISTORY_DROPPCT_MAX` (4%).
const HISTORY_DROPPCT_MAX: usize = 4;
/// Size of the sorted top/bottom history buffers.
const HISTORY_MAXBUF_SZ: usize = HISTORY_SZ * HISTORY_DROPPCT_MAX / 100;
/// Default `target_extra` (ms of slack on top of measured jitter).
const TARGET_EXTRA_DEFAULT: i64 = 40;
/// Minimum ms between successive grow/shrink adjustments in voice mode.
const ADJUST_DELAY: i64 = 40;

/// Kind of frame held in the buffer.
///
/// Mirrors the vendored `jb_frame_type` enum. Only `Voice` participates in
/// jitter/drift history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// IAX2 control frame; bypasses jitter accounting, delivered when due.
    Control,
    /// Voice payload — the only kind that updates jitter history.
    Voice,
    /// Video payload (reserved; included for parity with the C enum).
    Video,
    /// Comfort-noise / silence marker that toggles silence mode.
    Silence,
    /// DTMF event frame. Treated like Control for buffer scheduling.
    Dtmf,
}

/// One queued frame.
///
/// `ts` is the sender's timestamp (ms). `ms` is the frame duration in ms.
#[derive(Debug, Clone)]
pub struct Frame<T> {
    /// Opaque payload (encoded audio, a control event, …).
    pub data: T,
    /// Sender timestamp in ms.
    pub ts: i64,
    /// Frame duration in ms (sample count / sample rate × 1000).
    pub ms: i64,
    /// Frame kind — controls scheduling and history accounting.
    pub frame_type: FrameType,
}

/// Tunable parameters for the jitter buffer.
///
/// All times in ms. `target_extra = -1` is reserved by `set_config` to
/// mean "reset to default"; use [`JitterConfig::default`] in normal code.
#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// Hard upper bound on the adaptive buffer depth (ms). 0 = unbounded.
    pub max_jitterbuf: i64,
    /// Delay jump threshold (ms) above which we count discontinuities; after
    /// three in a row we resync the timestamp base. -1 disables resync.
    pub resync_threshold: i64,
    /// Maximum consecutive interpolated frames before we drop into silence
    /// mode. 0 = no cap.
    pub max_contig_interp: i64,
    /// Extra slack added on top of measured jitter when picking the target
    /// depth. Larger = more latency, fewer underruns.
    pub target_extra: i64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            max_jitterbuf: 0,
            resync_threshold: 1000,
            max_contig_interp: 0,
            target_extra: TARGET_EXTRA_DEFAULT,
        }
    }
}

/// Read-only counters and instantaneous state.
///
/// Field names follow the C `jb_info` struct verbatim so the regression
/// suite (and operators reading logs) can map them 1:1.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    /// Total frames passed to [`JitterBuf::put`].
    pub frames_in: i64,
    /// Total frames returned from [`JitterBuf::get`] as `Ok` or `Drop`.
    pub frames_out: i64,
    /// Frames that arrived after their scheduled play time and were dropped.
    pub frames_late: i64,
    /// Frames we expected but never saw (counted on interpolation).
    pub frames_lost: i64,
    /// Frames discarded to shrink the buffer.
    pub frames_dropped: i64,
    /// Frames received out of timestamp order.
    pub frames_ooo: i64,
    /// Frames currently held in the queue.
    pub frames_cur: i64,
    /// Estimated jitter (max-min over the history window) in ms.
    pub jitter: i64,
    /// Estimated minimum one-way delay in ms.
    pub min: i64,
    /// Current buffer depth target in ms.
    pub current: i64,
    /// Where we'd like the buffer depth to be (`jitter + min + target_extra`).
    pub target: i64,
    /// Recent loss percentage × 1000 (exponential moving average).
    pub losspct: i64,
    /// Next voice frame's expected receiver timestamp.
    pub next_voice_ts: i64,
    /// Duration of the most recently observed voice frame.
    pub last_voice_ms: i64,
    /// Sender ts of the most recent silence frame, or 0 in voice mode.
    pub silence_begin_ts: i64,
    /// Receiver time of the last grow/shrink event.
    pub last_adjustment: i64,
    /// Most recently observed delay (used for discontinuity detection).
    pub last_delay: i64,
    /// Running count of consecutive discontinuous delays.
    pub cnt_delay_discont: i64,
    /// Offset applied to incoming timestamps after a resync.
    pub resync_offset: i64,
    /// Consecutive interpolated frames returned.
    pub cnt_contig_interp: i64,
}

/// Outcome of a [`JitterBuf::get`] call.
#[derive(Debug)]
pub enum GetResult<T> {
    /// Hand this frame to the codec/sink now.
    Ok(Frame<T>),
    /// No frame for this slot — synthesise `interpl` ms of silence/PLC.
    Interpolate,
    /// This frame is being thrown away (late arrival or shrink). Caller may
    /// inspect/log it but should not play it. Ask again for `now`.
    Drop(Frame<T>),
    /// Buffer holds nothing right now.
    Empty,
    /// Nothing scheduled for `now`; try again later (see [`JitterBuf::next`]).
    NoFrame,
}

/// Errors returned by [`JitterBuf::put`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JitterError {
    /// Frame's delay was so far out of band that the discontinuity counter
    /// rejected it (resync still building up).
    #[error("frame dropped: delay discontinuity exceeded threshold")]
    DiscontinuityDrop,
}

/// The adaptive jitter buffer.
///
/// Generic over payload `T`; for audio you typically want `T = Vec<u8>` or
/// `T = bytes::Bytes`. All times are in milliseconds.
pub struct JitterBuf<T> {
    info: JitterStats,
    conf: JitterConfig,

    /// Ring of recent delays. `hist_ptr` indexes the next slot to write.
    history: Box<[i64; HISTORY_SZ]>,
    hist_ptr: usize,
    /// Top-N delays from `history`, sorted descending. Lazily rebuilt.
    hist_maxbuf: [i64; HISTORY_MAXBUF_SZ],
    /// Bottom-N delays from `history`, sorted ascending. Lazily rebuilt.
    hist_minbuf: [i64; HISTORY_MAXBUF_SZ],
    hist_maxbuf_valid: bool,

    /// Queued frames, kept sorted by `ts` ascending. The C code uses a
    /// circular doubly-linked list with a `free` pool; `VecDeque` is
    /// simpler and gives us O(log n) lookup with O(n) shift on insert,
    /// which is fine for a ≤ few-dozen-element buffer.
    frames: VecDeque<Frame<T>>,
}

impl<T> JitterBuf<T> {
    /// Build a fresh buffer with the given configuration.
    #[must_use]
    pub fn new(config: JitterConfig) -> Self {
        let mut jb = Self {
            info: JitterStats::default(),
            conf: config,
            history: Box::new([0_i64; HISTORY_SZ]),
            hist_ptr: 0,
            hist_maxbuf: [0; HISTORY_MAXBUF_SZ],
            hist_minbuf: [0; HISTORY_MAXBUF_SZ],
            hist_maxbuf_valid: false,
            frames: VecDeque::new(),
        };
        jb.reset_internal();
        jb
    }

    /// Drop all queued frames and reset stats. Configuration is preserved.
    pub fn reset(&mut self) {
        self.frames.clear();
        self.reset_internal();
    }

    fn reset_internal(&mut self) {
        let saved = self.conf;
        self.info = JitterStats::default();
        self.history.fill(0);
        self.hist_ptr = 0;
        self.hist_maxbuf = [0; HISTORY_MAXBUF_SZ];
        self.hist_minbuf = [0; HISTORY_MAXBUF_SZ];
        self.hist_maxbuf_valid = false;
        self.conf = saved;
        // The C code seeds current/target with target_extra and uses
        // silence_begin_ts = -1 to indicate "no silence in progress".
        self.info.current = saved.target_extra;
        self.info.target = saved.target_extra;
        self.info.silence_begin_ts = -1;
    }

    /// Snapshot of current counters/state.
    #[must_use]
    pub fn info(&self) -> &JitterStats {
        &self.info
    }

    /// Replace the live configuration. Mirrors `jb_setconf`: a `target_extra`
    /// of `-1` resets to the default. `current` and `target` are reseeded
    /// from the new `target_extra`.
    pub fn set_config(&mut self, mut config: JitterConfig) {
        if config.target_extra == -1 {
            config.target_extra = TARGET_EXTRA_DEFAULT;
        }
        self.conf = config;
        self.info.current = config.target_extra;
        self.info.target = config.target_extra;
    }

    /// When is the next frame due? `None` indicates an empty buffer
    /// (the C code returned `LONG_MAX`; `Option` is the Rust-y equivalent).
    #[must_use]
    pub fn next(&self) -> Option<i64> {
        // C semantics: `if (silence_begin_ts)` — any non-zero value (including
        // the post-reset sentinel of -1) puts us on the silent schedule.
        if self.info.silence_begin_ts != 0 {
            // Silent mode: schedule against the queued frames.
            if let Some(front) = self.frames.front() {
                // Mirror the C "shrink during silence" early-return.
                if self.info.target - self.info.current < -self.conf.target_extra {
                    return Some(self.info.last_adjustment + 10);
                }
                return Some(front.ts + self.info.target);
            }
            return None;
        }
        // Voice mode: next_voice_ts is the scheduled receiver-time.
        Some(self.info.next_voice_ts)
    }

    // ---- history ----

    fn history_put(&mut self, ts: i64, now: i64) -> Result<(), JitterError> {
        let mut delay = now - (ts - self.info.resync_offset);
        let threshold = 2 * self.info.jitter + self.conf.resync_threshold;

        // The C code rejects ts <= 0 silently. Skip history but don't error.
        if ts <= 0 {
            return Ok(());
        }

        if self.conf.resync_threshold != -1 {
            if (delay - self.info.last_delay).abs() > threshold {
                self.info.cnt_delay_discont += 1;
                if self.info.cnt_delay_discont > 3 {
                    // Resync: zero out history and rebase the timestamp axis.
                    tracing::debug!(
                        last_delay = self.info.last_delay,
                        delay,
                        threshold,
                        new_offset = ts - now,
                        "jitter: resync"
                    );
                    self.info.cnt_delay_discont = 0;
                    self.hist_ptr = 0;
                    self.hist_maxbuf_valid = false;
                    self.info.resync_offset = ts - now;
                    self.info.last_delay = 0;
                    delay = 0;
                } else {
                    return Err(JitterError::DiscontinuityDrop);
                }
            } else {
                self.info.last_delay = delay;
                self.info.cnt_delay_discont = 0;
            }
        }

        let slot = self.hist_ptr % HISTORY_SZ;
        let kicked = self.history[slot];
        self.history[slot] = delay;
        self.hist_ptr += 1;

        // Invalidate the sorted side buffers only if this frame might
        // perturb them. Same fast-path as the C version.
        if !self.hist_maxbuf_valid {
            return Ok(());
        }
        if self.hist_ptr < HISTORY_SZ {
            self.hist_maxbuf_valid = false;
            return Ok(());
        }
        let max_low = self.hist_maxbuf[HISTORY_MAXBUF_SZ - 1];
        let min_high = self.hist_minbuf[HISTORY_MAXBUF_SZ - 1];
        if delay < min_high || delay > max_low || kicked <= min_high || kicked >= max_low {
            self.hist_maxbuf_valid = false;
        }
        Ok(())
    }

    fn history_calc_maxbuf(&mut self) {
        if self.hist_ptr == 0 {
            return;
        }
        self.hist_maxbuf = [i64::MIN; HISTORY_MAXBUF_SZ];
        self.hist_minbuf = [i64::MAX; HISTORY_MAXBUF_SZ];

        let start = self.hist_ptr.saturating_sub(HISTORY_SZ);
        for i in start..self.hist_ptr {
            let toins = self.history[i % HISTORY_SZ];

            if toins > self.hist_maxbuf[HISTORY_MAXBUF_SZ - 1] {
                for j in 0..HISTORY_MAXBUF_SZ {
                    if toins > self.hist_maxbuf[j] {
                        // Shift right, insert.
                        for k in (j + 1..HISTORY_MAXBUF_SZ).rev() {
                            self.hist_maxbuf[k] = self.hist_maxbuf[k - 1];
                        }
                        self.hist_maxbuf[j] = toins;
                        break;
                    }
                }
            }
            if toins < self.hist_minbuf[HISTORY_MAXBUF_SZ - 1] {
                for j in 0..HISTORY_MAXBUF_SZ {
                    if toins < self.hist_minbuf[j] {
                        for k in (j + 1..HISTORY_MAXBUF_SZ).rev() {
                            self.hist_minbuf[k] = self.hist_minbuf[k - 1];
                        }
                        self.hist_minbuf[j] = toins;
                        break;
                    }
                }
            }
        }
        self.hist_maxbuf_valid = true;
    }

    fn history_get(&mut self) {
        if !self.hist_maxbuf_valid {
            self.history_calc_maxbuf();
        }
        let count = self.hist_ptr.min(HISTORY_SZ);
        let mut index = count * HISTORY_DROPPCT / 100;
        if index > HISTORY_MAXBUF_SZ - 1 {
            index = HISTORY_MAXBUF_SZ - 1;
        }
        if count == 0 {
            self.info.min = 0;
            self.info.jitter = 0;
            return;
        }
        let max = self.hist_maxbuf[index];
        let min = self.hist_minbuf[index];
        self.info.min = min;
        self.info.jitter = max - min;
    }

    // ---- queue ----

    /// Insert `frame` into the queue, sorted by `ts`. Returns `true` if it
    /// became the new head (caller should reschedule its wake-up). Mirrors
    /// the C `queue_put` exactly, including out-of-order accounting.
    fn queue_put(&mut self, mut frame: Frame<T>) -> bool {
        let resync_ts = frame.ts - self.info.resync_offset;
        frame.ts = resync_ts;
        self.info.frames_cur += 1;

        if self.frames.is_empty() {
            self.frames.push_back(frame);
            return true;
        }
        // New head?
        if resync_ts < self.frames.front().expect("non-empty").ts {
            self.info.frames_ooo += 1;
            self.frames.push_front(frame);
            return true;
        }
        // Out-of-order vs current tail?
        if resync_ts < self.frames.back().expect("non-empty").ts {
            self.info.frames_ooo += 1;
        }
        // Insert in sorted position. Linear from the back since most frames
        // arrive in order and end up at the tail.
        let mut idx = self.frames.len();
        while idx > 0 && self.frames[idx - 1].ts > resync_ts {
            idx -= 1;
        }
        self.frames.insert(idx, frame);
        false
    }

    fn queue_next_ts(&self) -> Option<i64> {
        self.frames.front().map(|f| f.ts)
    }

    fn queue_last_ts(&self) -> Option<i64> {
        self.frames.back().map(|f| f.ts)
    }

    /// Pop the head if its ts is at or before `ts`. Mirrors `queue_get`.
    fn queue_get(&mut self, ts: i64) -> Option<Frame<T>> {
        if self.frames.front()?.ts <= ts {
            let f = self.frames.pop_front();
            if f.is_some() {
                self.info.frames_cur -= 1;
            }
            f
        } else {
            None
        }
    }

    // ---- loss percentage EMA ----

    fn increment_losspct(&mut self) {
        self.info.losspct = (100_000 + 499 * self.info.losspct) / 500;
    }

    fn decrement_losspct(&mut self) {
        self.info.losspct = 499 * self.info.losspct / 500;
    }

    // ---- put / get ----

    /// Queue a frame.
    ///
    /// `now` is the receiver's clock in ms. Errors when the buffer detects
    /// a delay discontinuity (and doesn't yet trust it enough to resync).
    pub fn put(&mut self, frame: Frame<T>, now: i64) -> Result<(), JitterError> {
        self.info.frames_in += 1;
        if frame.frame_type == FrameType::Voice {
            self.history_put(frame.ts, now)?;
        }
        let _was_head = self.queue_put(frame);
        Ok(())
    }

    /// Pull a frame for time `now`.
    ///
    /// `interpl` is the duration in ms to assume when interpolating a
    /// missing frame (the next codec frame length, typically 20).
    pub fn get(&mut self, now: i64, interpl: i64) -> GetResult<T> {
        // Empty + nothing scheduled → really empty.
        if self.frames.is_empty() && self.info.next_voice_ts == 0 {
            return GetResult::Empty;
        }

        self.history_get();

        // target = jitter + min + target_extra, clamped by max_jitterbuf.
        self.info.target = self.info.jitter + self.info.min + self.conf.target_extra;
        if self.conf.max_jitterbuf != 0
            && (self.info.target - self.info.min) > self.conf.max_jitterbuf
        {
            self.info.target = self.info.min + self.conf.max_jitterbuf;
        }
        let diff = self.info.target - self.info.current;

        // C: `if (!silence_begin_ts)` — only literal zero is voice mode.
        // After reset the C sentinel is -1, which sends the first calls
        // through the silent path; voice mode is entered when a voice frame
        // resumes (silent path snaps `silence_begin_ts = 0`).
        if self.info.silence_begin_ts == 0 {
            self.get_voice(now, interpl, diff)
        } else {
            self.get_silent(now, interpl, diff)
        }
    }

    fn get_voice(&mut self, now: i64, interpl: i64, diff: i64) -> GetResult<T> {
        // Grow path: target says we want more depth than we have. Either
        // we've waited long enough since the last adjustment, or we're so
        // far behind that there's no room left in the queue anyway.
        let need_grow_room = match (self.queue_next_ts(), self.queue_last_ts()) {
            (Some(first), Some(last)) => diff > last - first,
            _ => false,
        };
        if diff > 0 && (self.info.last_adjustment + ADJUST_DELAY < now || need_grow_room) {
            self.info.current += interpl;
            self.info.next_voice_ts += interpl;
            self.info.last_voice_ms = interpl;
            self.info.last_adjustment = now;
            self.info.cnt_contig_interp += 1;
            if self.conf.max_contig_interp != 0
                && self.info.cnt_contig_interp >= self.conf.max_contig_interp
            {
                self.info.silence_begin_ts = self.info.next_voice_ts - self.info.current;
            }
            return GetResult::Interpolate;
        }

        let wanted = self.info.next_voice_ts - self.info.current;
        let frame = self.queue_get(wanted);

        // Non-voice frame: deliver as-is.
        if let Some(f) = frame {
            if f.frame_type != FrameType::Voice {
                if f.frame_type == FrameType::Silence {
                    self.info.silence_begin_ts = f.ts;
                    self.info.cnt_contig_interp = 0;
                }
                self.info.frames_out += 1;
                return GetResult::Ok(f);
            }

            // Voice frame in hand. Is it late?
            if f.ts + self.info.current < self.info.next_voice_ts {
                if f.ts + self.info.current > self.info.next_voice_ts - self.info.last_voice_ms {
                    // Either we interpolated past it on the last call, or
                    // it came a hair early. Accept it and resync.
                    self.info.next_voice_ts = f.ts + self.info.current + f.ms;
                    self.info.frames_out += 1;
                    self.decrement_losspct();
                    self.info.cnt_contig_interp = 0;
                    return GetResult::Ok(f);
                }
                // Genuinely late: deliver as Drop. The C code marks it
                // late and adjusts frames_lost too.
                self.info.frames_out += 1;
                self.decrement_losspct();
                self.info.frames_late += 1;
                self.info.frames_lost -= 1;
                return GetResult::Drop(f);
            }

            if f.ms > 0 {
                self.info.last_voice_ms = f.ms;
            }

            // Normal voice delivery.
            self.info.next_voice_ts += f.ms;
            self.info.frames_out += 1;
            self.info.cnt_contig_interp = 0;
            self.decrement_losspct();
            return GetResult::Ok(f);
        }

        // No frame ready. Shrink? Lost?
        let shrink_window =
            self.info.last_adjustment + 80 < now || self.info.last_adjustment + 500 < now;
        if diff < -self.conf.target_extra && shrink_window {
            self.info.last_adjustment = now;
            self.info.cnt_contig_interp = 0;
            // No frame to drop — shrink by last_voice_ms.
            self.info.current -= self.info.last_voice_ms;
            self.info.frames_lost += 1;
            self.increment_losspct();
            return GetResult::NoFrame;
        }

        // Just a lost frame.
        self.info.frames_lost += 1;
        self.increment_losspct();
        self.info.next_voice_ts += interpl;
        self.info.last_voice_ms = interpl;
        self.info.cnt_contig_interp += 1;
        if self.conf.max_contig_interp != 0
            && self.info.cnt_contig_interp >= self.conf.max_contig_interp
        {
            self.info.silence_begin_ts = self.info.next_voice_ts - self.info.current;
        }
        GetResult::Interpolate
    }

    fn get_silent(&mut self, now: i64, interpl: i64, diff: i64) -> GetResult<T> {
        // Shrink interpl every 10 ms during silence.
        if diff < -self.conf.target_extra && self.info.last_adjustment + 10 <= now {
            self.info.current -= interpl;
            self.info.last_adjustment = now;
        }
        let Some(frame) = self.queue_get(now - self.info.current) else {
            return GetResult::NoFrame;
        };
        if frame.frame_type != FrameType::Voice {
            self.info.frames_out += 1;
            return GetResult::Ok(frame);
        }
        if frame.ts < self.info.silence_begin_ts {
            // Late voice in silence mode.
            self.info.frames_out += 1;
            self.decrement_losspct();
            self.info.frames_late += 1;
            self.info.frames_lost -= 1;
            return GetResult::Drop(frame);
        }
        // Voice resumes. Snap current to target and re-enter voice mode.
        self.info.current = self.info.target;
        self.info.silence_begin_ts = 0;
        self.info.next_voice_ts = frame.ts + self.info.current + frame.ms;
        self.info.last_voice_ms = frame.ms;
        self.info.frames_out += 1;
        self.decrement_losspct();
        GetResult::Ok(frame)
    }
}

// ---------- tests ----------

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::*;

    fn voice(ts: i64, payload: u8) -> Frame<Vec<u8>> {
        Frame {
            data: vec![payload],
            ts,
            ms: 20,
            frame_type: FrameType::Voice,
        }
    }

    #[test]
    fn new_buffer_is_empty() {
        let jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        if let Some(t) = jb.next() {
            assert_eq!(t, 0, "next_voice_ts seeded to 0");
        }
        assert_eq!(jb.info().frames_cur, 0);
    }

    #[test]
    fn empty_get_returns_empty() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        assert!(matches!(jb.get(0, 20), GetResult::Empty));
    }

    #[test]
    fn put_then_get_in_order() {
        // Drive enough frames through to exit the initial "grow" phase,
        // then verify that an Ok-typed frame matches what we put in.
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        // Steady 20ms delay, in-order. Use a smallish target_extra so the
        // buffer settles quickly.
        let mut got_ok = 0_u32;
        for i in 0..50_i64 {
            let ts = i * 20;
            let now = ts + 20;
            jb.put(voice(ts, (i & 0xFF) as u8), now).unwrap();
            // Pull at a "now" comfortably after the put.
            if let GetResult::Ok(_) = jb.get(now + 60, 20) {
                got_ok += 1;
            }
        }
        assert!(got_ok > 0, "expected at least one Ok delivery, got none");
        assert_eq!(jb.info().frames_in, 50);
    }

    #[test]
    fn out_of_order_inserts_are_reordered() {
        // Verify the internal sort by checking the front-ts after inserts;
        // we don't need to round-trip through get() to prove order.
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        jb.put(voice(40, 2), 80).unwrap();
        jb.put(voice(0, 1), 40).unwrap();
        jb.put(voice(20, 3), 60).unwrap();
        // The queue must hold them in ts-ascending order.
        let ordered: Vec<i64> = jb.frames.iter().map(|f| f.ts).collect();
        assert_eq!(ordered, vec![0, 20, 40]);
        assert!(jb.info().frames_ooo >= 1, "expected out-of-order count");
    }

    #[test]
    fn late_frame_is_dropped() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig {
            target_extra: 20,
            ..JitterConfig::default()
        });
        // Steady stream to build history.
        for i in 0..10_i64 {
            let ts = i * 20;
            let now = ts + 20;
            jb.put(voice(ts, i as u8), now).unwrap();
            let _ = jb.get(now + 20, 20);
        }
        // Now inject a frame whose ts is way in the past relative to
        // next_voice_ts (i.e. should arrive late).
        let next = jb.info().next_voice_ts;
        // Insert a frame with ts well below what next_voice_ts - current expects.
        let stale_ts = next - jb.info().current - 200;
        jb.put(voice(stale_ts, 99), next + 200).unwrap();
        let r = jb.get(next + 200, 20);
        // Late frames come back as Drop, not Ok.
        match r {
            GetResult::Drop(f) => assert_eq!(f.data, vec![99]),
            other => panic!("expected Drop, got {other:?}"),
        }
        assert!(jb.info().frames_late >= 1);
    }

    #[test]
    fn interpolation_when_no_frame_available() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        // Put one frame, then ask for a much later slot. No frame available
        // there → Interpolate (lost frame path).
        jb.put(voice(0, 1), 40).unwrap();
        let _ = jb.get(80, 20); // consume the first
        let r = jb.get(200, 20);
        assert!(
            matches!(r, GetResult::Interpolate | GetResult::NoFrame),
            "got {r:?}"
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        for i in 0..20_i64 {
            jb.put(voice(i * 20, i as u8), i * 20 + 40).unwrap();
        }
        let before = jb.info().frames_in;
        assert!(before > 0);
        jb.reset();
        assert_eq!(jb.info().frames_in, 0);
        assert_eq!(jb.info().frames_cur, 0);
        assert_eq!(jb.info().jitter, 0);
        assert!(matches!(jb.get(0, 20), GetResult::Empty));
    }

    #[test]
    fn growing_jitter_increases_target() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        // Feed enough frames with growing delays to populate history.
        // Use 200 frames so the percentile drop-cut has room.
        for i in 0..200_i64 {
            let ts = i * 20;
            // Add jitter: every 7th frame is delayed by 60ms.
            let extra = if i % 7 == 0 { 60 } else { 0 };
            let now = ts + 20 + extra;
            jb.put(voice(ts, (i & 0xFF) as u8), now).unwrap();
        }
        // Trigger a target recompute by calling get.
        let _ = jb.get(5000, 20);
        let info = jb.info();
        assert!(
            info.jitter > 0,
            "jitter should be measurable after varied delays, got {info:?}"
        );
        assert!(info.target >= info.jitter + info.min);
    }

    #[test]
    fn shrinking_jitter_lowers_target_over_time() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        // Start with variable delays.
        for i in 0..100_i64 {
            let ts = i * 20;
            let now = ts + 20 + (i % 5) * 10;
            jb.put(voice(ts, (i & 0xFF) as u8), now).unwrap();
        }
        let _ = jb.get(3000, 20);
        let varied_jitter = jb.info().jitter;
        // Reset and feed steady stream.
        jb.reset();
        for i in 0..500_i64 {
            let ts = i * 20;
            let now = ts + 20; // constant delay → zero jitter
            jb.put(voice(ts, (i & 0xFF) as u8), now).unwrap();
        }
        let _ = jb.get(20_000, 20);
        let steady_jitter = jb.info().jitter;
        assert!(
            steady_jitter <= varied_jitter,
            "steady jitter {steady_jitter} should be <= varied {varied_jitter}"
        );
        assert_eq!(steady_jitter, 0);
    }

    #[test]
    fn config_target_extra_minus_one_resets_to_default() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        jb.set_config(JitterConfig {
            target_extra: -1,
            ..JitterConfig::default()
        });
        assert_eq!(jb.info().current, TARGET_EXTRA_DEFAULT);
        assert_eq!(jb.info().target, TARGET_EXTRA_DEFAULT);
    }

    #[test]
    fn frames_in_counter_advances_on_put() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        for i in 0..5 {
            jb.put(voice(i * 20, i as u8), i * 20 + 40).unwrap();
        }
        assert_eq!(jb.info().frames_in, 5);
        assert_eq!(jb.info().frames_cur, 5);
    }

    #[test]
    fn control_frames_bypass_history() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        let f = Frame {
            data: vec![0xAA],
            ts: 100,
            ms: 0,
            frame_type: FrameType::Control,
        };
        jb.put(f, 200).unwrap();
        assert_eq!(jb.info().jitter, 0); // history untouched
        assert_eq!(jb.info().frames_cur, 1);
    }

    #[test]
    fn silence_frame_enters_silent_mode() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig::default());
        // Seed a voice frame first so next_voice_ts advances.
        jb.put(voice(0, 1), 20).unwrap();
        let _ = jb.get(100, 20);
        // Push a silence frame at a non-zero ts (0 is the C sentinel for
        // "not in silence", so a Silence frame at ts=0 is ambiguous by
        // design of the original algorithm).
        let f = Frame {
            data: vec![0],
            ts: 100,
            ms: 20,
            frame_type: FrameType::Silence,
        };
        jb.put(f, 120).unwrap();
        // Pull repeatedly to consume any interp/grow steps, then the silence.
        for now in (140..400).step_by(20) {
            if let GetResult::Ok(f) = jb.get(now, 20)
                && f.frame_type == FrameType::Silence
            {
                break;
            }
        }
        // After consuming the silence frame, silence_begin_ts should be > 0.
        assert!(
            jb.info().silence_begin_ts > 0,
            "silence_begin_ts not set, info = {:?}",
            jb.info()
        );
    }

    #[test]
    fn resync_disabled_accepts_huge_delay_jump() {
        let mut jb: JitterBuf<Vec<u8>> = JitterBuf::new(JitterConfig {
            resync_threshold: -1,
            ..JitterConfig::default()
        });
        // Massive delay - would normally trigger discontinuity.
        jb.put(voice(0, 1), 10_000).unwrap();
        jb.put(voice(20, 2), 20).unwrap();
        assert_eq!(jb.info().frames_in, 2);
    }
}
