// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The picture transmit path: a whole SSTV picture's audio sent on a call in
//! place of the microphone.
//!
//! A picture is fragile in a way speech is not. Its pixels are carried by
//! exact tone frequencies at exact times, so a dropped 20 ms frame, a
//! repeated one, or a jump in the time-stamps the far end schedules playout
//! by, shears or smears every line after it. Voice tolerates all three, and
//! the voice path is built to recover from them: its time-stamp ladder
//! (`runtime::voice_ts_ladder`) re-anchors to the wall clock after an 80 ms
//! disagreement. A picture must never do that.
//!
//! So a picture has its own path, with three parts:
//!
//! * **A pacer thread** owned by the picture. It cuts the audio into 20 ms
//!   frames and tells the call's run loop to send frame `n` at
//!   `t0 + n × 20 ms`, by absolute deadline. If it wakes late it sends that
//!   one frame at once and catches up on the frames after it at no more than
//!   one per half frame; it never skips a frame and never sends a burst.
//!   Holding the absolute schedule keeps the picture's time-stamps level with
//!   the wall clock, so voice afterwards resumes without the ladder
//!   re-anchoring.
//! * **Time-stamps counted from samples sent**, never from the clock:
//!   [`picture_ts`] anchors once, at the first frame, and every later frame is
//!   exactly 20 later. A pacing stall shows on the wire as a late frame with
//!   its proper time-stamp.
//! * **The run loop's side** ([`ActivePicture`]): while a picture is on, it
//!   sends the picture's frames through the same codec edge as voice and
//!   throws away whatever the microphone produces, and goes on throwing it
//!   away until the call has been unkeyed and keyed again (the run loop's
//!   `MicHold`). When the picture ends it hands the voice ladder the
//!   time-stamp after the picture's last frame, so the next over's voice
//!   carries on without a step.
//!
//! The first two parts are not tied to IAX2. [`PicturePacing`] holds the
//! cut-up audio and the pacer, and the `EchoLink` link in astar-console
//! drives one directly, so a picture on either network is paced by the same
//! code and reported through the same [`PictureTx`].
//!
//! The audio never passes through the microphone lane, so it never meets
//! input gain, noise reduction, the compressor or transmit trim. This path
//! neither keys nor unkeys: the caller keys first, and if the call unkeys or
//! ends mid-picture the picture is cut and its handle says why.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::audio_bridge::PttGate;

/// One frame's length on the wire, in time-stamp units (milliseconds).
const FRAME_MS: u32 = 20;
const FRAME: Duration = Duration::from_millis(FRAME_MS as u64);

/// Time-stamp of a picture's frame `n`: anchored once, never re-anchored.
pub(crate) fn picture_ts(first: u32, n: u32) -> u32 {
    first.wrapping_add(n.wrapping_mul(FRAME_MS))
}

/// How a picture ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureEnd {
    /// Every frame went out.
    Sent,
    /// [`PictureTx::cancel`] stopped it.
    Cancelled,
    /// Something outside the picture stopped it.
    Cut(CutReason),
}

/// Why a picture was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutReason {
    /// The call ended (either side hung up, or the call failed).
    CallEnded,
    /// The call was not keyed, or unkeyed mid-picture.
    Unkeyed,
    /// The call's own limit on one continuous over stopped it: the
    /// `EchoLink` link's transmit cap. A time limit, like a transmit
    /// time-out, so a caller reports it as one.
    TransmitCap,
}

// `Shared::end` encoding. `RUNNING` until the first writer wins.
const RUNNING: u8 = 0;
const SENT: u8 = 1;
const CANCELLED: u8 = 2;
const CUT_CALL_ENDED: u8 = 3;
const CUT_UNKEYED: u8 = 4;
const CUT_TRANSMIT_CAP: u8 = 5;

fn end_code(end: PictureEnd) -> u8 {
    match end {
        PictureEnd::Sent => SENT,
        PictureEnd::Cancelled => CANCELLED,
        PictureEnd::Cut(CutReason::CallEnded) => CUT_CALL_ENDED,
        PictureEnd::Cut(CutReason::Unkeyed) => CUT_UNKEYED,
        PictureEnd::Cut(CutReason::TransmitCap) => CUT_TRANSMIT_CAP,
    }
}

/// The cells a picture's handle, its pacer and the call's run loop share.
/// Atomics only: nothing here is ever locked.
struct Shared {
    sent: AtomicUsize,
    total: usize,
    end: AtomicU8,
    /// Test hook: milliseconds the pacer sleeps before its next frame.
    stall_ms: AtomicU64,
}

impl Shared {
    fn new(total: usize, end: u8) -> Self {
        Self {
            sent: AtomicUsize::new(0),
            total,
            end: AtomicU8::new(end),
            stall_ms: AtomicU64::new(0),
        }
    }

    /// Record how the picture ended. The first caller wins; later ones are
    /// ignored, so a cancel and a hangup racing each other settle on one.
    fn finish(&self, end: PictureEnd) {
        let _ =
            self.end
                .compare_exchange(RUNNING, end_code(end), Ordering::AcqRel, Ordering::Acquire);
    }

    fn running(&self) -> bool {
        self.end.load(Ordering::Acquire) == RUNNING
    }
}

/// Handle to a picture being sent: progress, cancel, and how it ended.
/// Cloning it shares the same picture.
#[derive(Clone)]
pub struct PictureTx {
    shared: Arc<Shared>,
}

impl PictureTx {
    /// Frames sent so far.
    #[must_use]
    pub fn frames_sent(&self) -> usize {
        self.shared.sent.load(Ordering::Acquire)
    }

    /// Frames in the whole picture (the last one zero-padded to 20 ms).
    #[must_use]
    pub fn frames_total(&self) -> usize {
        self.shared.total
    }

    /// Stop the picture. At most one more frame goes out (one the run loop
    /// was already sending). Does nothing if the picture has already ended.
    pub fn cancel(&self) {
        self.shared.finish(PictureEnd::Cancelled);
    }

    /// `None` while frames are still going out.
    #[must_use]
    pub fn ended(&self) -> Option<PictureEnd> {
        match self.shared.end.load(Ordering::Acquire) {
            SENT => Some(PictureEnd::Sent),
            CANCELLED => Some(PictureEnd::Cancelled),
            CUT_CALL_ENDED => Some(PictureEnd::Cut(CutReason::CallEnded)),
            CUT_UNKEYED => Some(PictureEnd::Cut(CutReason::Unkeyed)),
            CUT_TRANSMIT_CAP => Some(PictureEnd::Cut(CutReason::TransmitCap)),
            _ => None,
        }
    }

    /// Test hook: the pacer sleeps `stall` before its next frame, as if its
    /// thread had been descheduled.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn stall_pacer(&self, stall: Duration) {
        let ms = u64::try_from(stall.as_millis()).unwrap_or(u64::MAX);
        self.shared.stall_ms.store(ms, Ordering::Release);
    }

    /// A handle for a picture that ended before its first frame: one of
    /// `total` frames refused because the call was not keyed, or an empty
    /// picture that is already [`PictureEnd::Sent`].
    #[must_use]
    pub fn already_ended(total: usize, end: PictureEnd) -> Self {
        Self {
            shared: Arc::new(Shared::new(total, end_code(end))),
        }
    }
}

/// `pcm` cut into whole frames of `frame_len` samples: the audio itself when
/// it already divides evenly, otherwise one copy with the last frame
/// zero-padded. Returns the audio and the frame count. Done once, up front,
/// so the run loop never allocates for a picture frame.
pub(crate) fn pad_to_frames(pcm: Arc<[i16]>, frame_len: usize) -> (Arc<[i16]>, usize) {
    let total = pcm.len().div_ceil(frame_len);
    if pcm.len() == total * frame_len {
        return (pcm, total);
    }
    let mut padded = Vec::with_capacity(total * frame_len);
    padded.extend_from_slice(&pcm);
    padded.resize(total * frame_len, 0);
    (Arc::from(padded), total)
}

/// Frame `n` of audio already cut by [`pad_to_frames`].
pub(crate) fn frame(pcm: &[i16], frame_len: usize, n: usize) -> &[i16] {
    &pcm[n * frame_len..(n + 1) * frame_len]
}

/// A picture's audio, cut into 20 ms frames, with its pacer running: the
/// sending side of a picture, for whichever transport carries it. The IAX2
/// run loop drives one through `ActivePicture`; the `EchoLink` link in
/// astar-console drives one directly, so both share one pacer and one
/// [`PictureTx`].
///
/// The transport polls [`PicturePacing::next_due`] at least as often as a
/// frame falls due, sends each frame it returns and counts it with
/// [`PicturePacing::mark_sent`]. Frames come out in order, each exactly
/// once; the transport stamps them from the frame number, never the clock.
///
/// Dropping it while the picture is still running means the transport is
/// gone, or never took it: the call has ended, and the picture says so
/// ([`CutReason::CallEnded`]).
pub struct PicturePacing {
    pcm: Arc<[i16]>,
    frame_len: usize,
    /// One tick per frame from the pacer, carrying the frame's number.
    ticks: mpsc::Receiver<u32>,
    shared: Arc<Shared>,
}

impl Drop for PicturePacing {
    fn drop(&mut self) {
        self.shared.finish(PictureEnd::Cut(CutReason::CallEnded));
    }
}

impl PicturePacing {
    /// Start a picture: cut `pcm` into `frame_len`-sample frames (20 ms at
    /// the transport's rate; the last frame zero-padded) and spawn its pacer,
    /// which calls `wake` each time a frame falls due so a transport that
    /// sleeps can be woken. Until the returned pacing reaches the transport,
    /// ticks wait in its channel.
    ///
    /// # Errors
    /// The pacer thread could not be spawned.
    ///
    /// # Panics
    /// If `frame_len` is zero.
    pub fn start(
        pcm: Arc<[i16]>,
        frame_len: usize,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<(PictureTx, PicturePacing)> {
        assert!(frame_len > 0, "a picture frame holds at least one sample");
        let (pcm, total) = pad_to_frames(pcm, frame_len);
        let shared = Arc::new(Shared::new(total, RUNNING));
        let (tick_tx, ticks) = mpsc::channel();
        let pacing = PicturePacing {
            pcm,
            frame_len,
            ticks,
            shared: Arc::clone(&shared),
        };
        let pacer_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("picture-pacer".into())
            .spawn(move || pace(&pacer_shared, &tick_tx, &wake))?;
        Ok((PictureTx { shared }, pacing))
    }

    /// The number of the next frame due out, or `None` when none is due yet
    /// or the picture is over.
    #[must_use]
    pub fn next_due(&self) -> Option<usize> {
        if !self.shared.running() {
            return None;
        }
        let tick = self.ticks.try_recv().ok()?;
        let n = self.shared.sent.load(Ordering::Acquire);
        debug_assert_eq!(usize::try_from(tick).ok(), Some(n), "pacer out of step");
        Some(n)
    }

    /// Frame `n`'s audio: `frame_len` samples.
    #[must_use]
    pub fn frame(&self, n: usize) -> &[i16] {
        frame(&self.pcm, self.frame_len, n)
    }

    /// Count the frame just sent; the last one ends the picture as sent.
    pub fn mark_sent(&self) {
        let sent = self.shared.sent.fetch_add(1, Ordering::AcqRel) + 1;
        if sent == self.shared.total {
            self.shared.finish(PictureEnd::Sent);
        }
    }

    /// Stop the picture because something outside it stopped the call's
    /// transmit side. Does nothing once the picture has ended.
    pub fn cut(&self, reason: CutReason) {
        self.shared.finish(PictureEnd::Cut(reason));
    }

    /// Whether the picture has ended, for any reason.
    #[must_use]
    pub fn is_over(&self) -> bool {
        !self.shared.running()
    }

    /// Frames sent so far.
    #[must_use]
    pub fn frames_sent(&self) -> usize {
        self.shared.sent.load(Ordering::Acquire)
    }
}

/// What the call's run loop needs to send one picture. Built by
/// [`start`] and carried into the run loop by a runtime command. Dropping
/// it unsent cuts the picture ([`PicturePacing`]'s drop).
pub(crate) struct PictureJob {
    pacing: PicturePacing,
    gate: PttGate,
}

/// Start a picture on an IAX2 call: [`PicturePacing::start`], waking the
/// call's run loop through `waker` for each frame. The returned job must
/// reach that run loop.
///
/// # Errors
/// The pacer thread could not be spawned.
pub(crate) fn start(
    pcm: Arc<[i16]>,
    frame_len: usize,
    gate: PttGate,
    waker: Arc<mio::Waker>,
) -> std::io::Result<(PictureTx, PictureJob)> {
    let (tx, pacing) = PicturePacing::start(pcm, frame_len, move || {
        let _ = waker.wake();
    })?;
    Ok((tx, PictureJob { pacing, gate }))
}

/// The shortest gap between two ticks while the pacer is catching up after a
/// late wake: half a frame, so a stall's backlog drains at twice real time
/// rather than as a burst.
const CATCH_UP_GAP: Duration = Duration::from_millis(FRAME_MS as u64 / 2);

/// The pacer: tick frame `n` at `t0 + n × 20 ms`, one frame per wake.
///
/// The schedule is absolute and is never moved. A wake that comes late sends
/// its frame at once; the frames after it then go out [`CATCH_UP_GAP`] apart
/// until the schedule is met again, never in a burst. No frame is skipped,
/// and the time-stamps come from the frame number, not from this clock. Since
/// the schedule never slips, the picture ends level with the wall clock, and
/// the voice ladder that takes over after it has nothing to re-anchor.
fn pace(shared: &Shared, ticks: &mpsc::Sender<u32>, wake: &dyn Fn()) {
    let t0 = Instant::now();
    let mut earliest = t0;
    for n in 0..shared.total {
        let stall = shared.stall_ms.swap(0, Ordering::AcqRel);
        if stall > 0 {
            thread::sleep(Duration::from_millis(stall));
        }
        let Ok(n) = u32::try_from(n) else { return };
        let due = (t0 + FRAME * n).max(earliest);
        let now = Instant::now();
        if due > now {
            thread::sleep(due - now);
        }
        if !shared.running() {
            return;
        }
        if ticks.send(n).is_err() {
            return;
        }
        wake();
        earliest = Instant::now() + CATCH_UP_GAP;
    }
}

/// The run loop's state for the picture it is sending.
pub(crate) struct ActivePicture {
    job: PictureJob,
    /// The first frame's time-stamp, fixed when that frame goes out.
    first: Option<u32>,
}

impl ActivePicture {
    pub(crate) fn new(job: PictureJob) -> Self {
        Self { job, first: None }
    }

    /// The number of the next frame due out, or `None` when none is due yet
    /// or the picture is over. Cuts the picture if the call has unkeyed.
    pub(crate) fn next_due(&mut self) -> Option<usize> {
        if !self.job.pacing.is_over() && !self.job.gate.engaged() {
            self.job.pacing.cut(CutReason::Unkeyed);
            return None;
        }
        self.job.pacing.next_due()
    }

    /// Frame `n`'s audio and time-stamp. The first call fixes the picture's
    /// first time-stamp from `anchor`: the voice ladder's next value.
    pub(crate) fn frame(&mut self, n: usize, anchor: impl FnOnce() -> u32) -> (&[i16], u32) {
        let first = *self.first.get_or_insert_with(anchor);
        let ts = picture_ts(first, u32::try_from(n).unwrap_or(u32::MAX));
        (self.job.pacing.frame(n), ts)
    }

    /// Count frame `n` as sent; the last one ends the picture as sent.
    pub(crate) fn mark_sent(&self) {
        self.job.pacing.mark_sent();
    }

    /// Whether the picture has ended, for any reason.
    pub(crate) fn is_over(&self) -> bool {
        self.job.pacing.is_over()
    }

    /// The time-stamp the voice ladder continues from: the one after the
    /// picture's last frame sent. `None` if no frame went out.
    pub(crate) fn resume_ts(&self) -> Option<u32> {
        let sent = self.job.pacing.frames_sent();
        self.first
            .map(|first| picture_ts(first, u32::try_from(sent).unwrap_or(u32::MAX)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_counts_frames_not_time() {
        assert_eq!(picture_ts(1000, 0), 1000);
        assert_eq!(picture_ts(1000, 250), 6000);
        assert_eq!(picture_ts(u32::MAX - 9, 1), 10);
    }

    #[test]
    fn frames_cover_the_audio_exactly() {
        let input: Vec<i16> = (1..=1000).map(|i| i16::try_from(i).unwrap()).collect();
        let (padded, total) = pad_to_frames(Arc::from(input.clone()), 160);
        assert_eq!(total, 7);
        let frames: Vec<&[i16]> = (0..total).map(|n| frame(&padded, 160, n)).collect();
        assert!(frames.iter().all(|f| f.len() == 160));
        assert_eq!(&frames[6][..40], &input[960..]);
        assert!(frames[6][40..].iter().all(|&s| s == 0));
        let joined: Vec<i16> = frames.concat();
        assert_eq!(&joined[..input.len()], &input[..]);
    }

    #[test]
    fn audio_that_divides_evenly_is_not_copied() {
        let input: Arc<[i16]> = Arc::from(vec![7i16; 320]);
        let (out, total) = pad_to_frames(Arc::clone(&input), 160);
        assert_eq!(total, 2);
        assert!(Arc::ptr_eq(&out, &input));
    }

    #[test]
    fn the_first_end_wins() {
        let tx = PictureTx::already_ended(3, PictureEnd::Cut(CutReason::Unkeyed));
        tx.cancel();
        assert_eq!(tx.ended(), Some(PictureEnd::Cut(CutReason::Unkeyed)));
    }
}
