// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Sending an SSTV picture on a call (feature `sstv-tx`, which no shipped
//! build turns on).
//!
//! SSTV (slow-scan television) carries a picture as a run of audio tones:
//! each pixel's brightness is a frequency held for a fixed time. Sending one
//! takes three steps, and [`Station::send_picture`](crate::Station::send_picture)
//! does them in order:
//!
//! 1. **Encode** the whole picture up front with the vendored `sstv` crate
//!    (MIT, `vendor/sstv`), at the call's bus rate. The encoder peaks at full
//!    scale, so the audio is scaled to [`PICTURE_PEAK_DBFS`], and
//!    [`PICTURE_LEAD`] and [`PICTURE_TAIL`] of silence go either side: the
//!    lead lets the far node's jitter buffer settle before the header, and
//!    the tail keeps its hang time off the last line.
//! 2. **Key** by pressing [`PttSource::Picture`] in the PTT arbiter. That is
//!    what puts the picture under the one transmit time-out, and what makes
//!    the rest of the engine see an ordinary key-down.
//! 3. **Hand the audio** to the call's picture path, which paces it out
//!    20 ms at a time on its own clock: `astar-iax`'s for an `AllStarLink`
//!    call, the `EchoLink` link's (GSM, four frames to a packet) for an
//!    `EchoLink` call. Both use the same pacer and report through the same
//!    handle. It never goes through the microphone lane, so input gain,
//!    noise reduction, the compressor and transmit trim never touch it, and
//!    no microphone is needed.
//!
//! When the picture ends for any reason, the station's PTT watchdog lets go
//! of [`PttSource::Picture`] (and so unkeys), and the handle says why:
//! [`SendEnd`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use astar_console::ConsoleSession;
use astar_iax::{CutReason, PictureEnd, PictureTx};

use crate::ptt_arbiter::{ForcedReason, ForcedRecord, PttArbiter, PttEdge, PttSource};

/// A picture's mode: Robot 36, Scottie 2, PD 120 and the rest. The modes are
/// the constants in [`sstv_tx_modes`].
pub use sstv::Mode as SstvTxMode;
/// Every mode the encoder can send, as constants (`ROBOT_36`, `SCOTTIE_2`,
/// `PD_120`, …), so a caller needs no direct `sstv` dependency.
pub use sstv::modes as sstv_tx_modes;

/// A picture to send: `width` × `height` pixels, row-major, 3 bytes (red,
/// green, blue) per pixel. It must already be the mode's own size
/// ([`SstvTxMode::resolution`]); the station does not scale it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstvPicture {
    /// Pixels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height * 3` bytes of RGB.
    pub rgb: Vec<u8>,
}

/// The loudest sample of a sent picture, in dB below full scale. The
/// encoder itself peaks at full scale; 6 dB of headroom keeps a codec or a
/// node's level stage from clipping the tones.
pub const PICTURE_PEAK_DBFS: f32 = -6.0;

/// Silence sent before the picture's header: four seconds, so the far
/// node's jitter buffer has settled before the header starts.
///
/// A stock `AllStarLink` node's jitter buffer grows when a call's first
/// frames arrive late, then shrinks again by dropping 20 ms of audio at a
/// time, at about 1.4, 1.9, 2.6 and 3.3 s after key-up. The header's VIS
/// code, the eight bits that name the mode, lasts only 0.3 s, and a drop
/// inside it loses the whole picture at every decoder. With 0.3 s of lead
/// the header fell on the 1.9 s drop on every run through the bench's
/// `net-1118` network profile; from 3 s of lead on it never did there. On
/// the harsher `harsh` profile a drop still met the header once in five runs
/// at 3 s (that picture decoded anyway) and never at 4 s. Measured on
/// nodebench (`harness/nodebench/RIG.md`, "SSTV through the node").
pub const PICTURE_LEAD: Duration = Duration::from_secs(4);

/// Silence sent after the picture's last line, so the far node's hang time
/// cannot eat its end.
pub const PICTURE_TAIL: Duration = Duration::from_millis(300);

/// Why [`Station::send_picture`](crate::Station::send_picture) would not
/// start a picture. Nothing was keyed for any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendPictureError {
    /// No answered call to send it on. Also returned if the engine cannot
    /// start the picture at all (its watchdog or pacer thread would not
    /// start, or the call went away while the picture was being handed over).
    NoCall,
    /// The call is on a network that cannot carry a picture, named as a
    /// person would: `"M17"`, `"D-Star"`, `"System Fusion"`, `"NXDN"` and
    /// `"DMR"` send voice through a vocoder, which cannot carry SSTV tones.
    /// `AllStarLink` and `EchoLink` calls both carry pictures.
    WrongNetwork(&'static str),
    /// The picture is not the mode's size, or `rgb` is not
    /// `width * height * 3` bytes long.
    WrongSize {
        /// The mode's size.
        expected: (u32, u32),
        /// The picture's `width` and `height`.
        got: (u32, u32),
    },
    /// The picture, lead-in and tail included, lasts longer than the
    /// transmit time-out, so it could not finish. The time-out is never
    /// changed for a picture: raise it, or pick a shorter mode.
    TooLong {
        /// [`Station::picture_duration`](crate::Station::picture_duration).
        picture: Duration,
        /// The transmit time-out in force.
        timeout: Duration,
    },
    /// Something is already transmitting: a picture, PTT, VOX or an
    /// announcement.
    OnAir,
}

impl std::fmt::Display for SendPictureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCall => f.write_str("no call to send the picture on"),
            Self::WrongNetwork(net) => write!(f, "{net} cannot carry a picture"),
            Self::WrongSize { expected, got } => write!(
                f,
                "the picture is {}x{}, the mode needs {}x{}",
                got.0, got.1, expected.0, expected.1
            ),
            Self::TooLong { picture, timeout } => write!(
                f,
                "the picture lasts {:.0} s, longer than the {:.0} s transmit time-out",
                picture.as_secs_f64(),
                timeout.as_secs_f64()
            ),
            Self::OnAir => f.write_str("already transmitting"),
        }
    }
}

impl std::error::Error for SendPictureError {}

/// How a picture ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendEnd {
    /// Every frame went out.
    Sent,
    /// [`PictureSend::cancel`], or something let go of the key (the
    /// universal stop, a `set_ptt(false)`).
    Cancelled,
    /// The transmit time-out fired while the picture was on the air.
    TimedOut,
    /// The call ended under it.
    CallEnded,
}

/// The whole transmission of `picture` in `mode` at `rate` samples a second,
/// exactly as [`Station::send_picture`](crate::Station::send_picture) hands
/// it to the call: [`PICTURE_LEAD`] of silence, the header and picture
/// scaled to peak at [`PICTURE_PEAK_DBFS`], then [`PICTURE_TAIL`] of
/// silence.
///
/// # Errors
/// [`SendPictureError::WrongSize`] if the picture is not the mode's size.
#[doc(hidden)]
pub fn encode_picture(
    picture: &SstvPicture,
    mode: SstvTxMode,
    rate: u32,
) -> Result<Vec<i16>, SendPictureError> {
    check_size(picture, mode)?;
    let pixels = picture
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| sstv::RgbPixel::new(r, g, b));
    let encoder = sstv::Encoder::new(mode, pixels).map_err(|_| wrong_size(picture, mode))?;
    let raw: Vec<i16> = sstv::Synthesizer::new(encoder, rate).collect();
    let peak = raw.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let target = f32::from(i16::MAX) * 10f32.powf(PICTURE_PEAK_DBFS / 20.0);
    let gain = if peak == 0 {
        1.0
    } else {
        target / f32::from(peak)
    };
    let lead = samples_in(PICTURE_LEAD, rate);
    let tail = samples_in(PICTURE_TAIL, rate);
    let mut out = Vec::with_capacity(lead + raw.len() + tail);
    out.resize(lead, 0);
    // The gain is below 1, so every scaled sample fits in an i16.
    #[allow(clippy::cast_possible_truncation)]
    out.extend(raw.iter().map(|&s| (f32::from(s) * gain).round() as i16));
    out.resize(out.len() + tail, 0);
    Ok(out)
}

/// How long `mode` takes on the air, lead-in and tail included.
pub(crate) fn picture_duration(mode: SstvTxMode) -> Duration {
    mode.duration() + PICTURE_LEAD + PICTURE_TAIL
}

/// `Ok` when `picture` is the mode's size and holds 3 bytes per pixel.
pub(crate) fn check_size(picture: &SstvPicture, mode: SstvTxMode) -> Result<(), SendPictureError> {
    let bytes = u64::from(picture.width) * u64::from(picture.height) * 3;
    if (picture.width, picture.height) == mode.resolution()
        && u64::try_from(picture.rgb.len()).is_ok_and(|len| len == bytes)
    {
        Ok(())
    } else {
        Err(wrong_size(picture, mode))
    }
}

fn wrong_size(picture: &SstvPicture, mode: SstvTxMode) -> SendPictureError {
    SendPictureError::WrongSize {
        expected: mode.resolution(),
        got: (picture.width, picture.height),
    }
}

fn samples_in(d: Duration, rate: u32) -> usize {
    usize::try_from(d.as_millis() * u128::from(rate) / 1000).unwrap_or(usize::MAX)
}

fn relock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One picture on its way out, shared by its handle and the station's slot.
pub(crate) struct Sending {
    tx: PictureTx,
    /// The arbiter's forced-stop count when the picture keyed, so a time-out
    /// during this picture can be told apart from an earlier one.
    forced_seq: u32,
    /// The call the picture was keyed on went away under it.
    call_lost: AtomicBool,
    /// How it ended, fixed the first time anyone looks after it has.
    end: OnceLock<SendEnd>,
}

impl Sending {
    pub(crate) fn new(tx: PictureTx, forced_seq: u32) -> Self {
        Self {
            tx,
            forced_seq,
            call_lost: AtomicBool::new(false),
            end: OnceLock::new(),
        }
    }

    /// How the picture ended, or `None` while it runs. `forced` is the
    /// arbiter's last forced stop, read after the picture ended: a picture
    /// cut by an unkey was timed out exactly when that stop is a transmit
    /// time-out that came after it keyed and released [`PttSource::Picture`].
    fn settle(&self, forced: ForcedRecord) -> Option<SendEnd> {
        if let Some(end) = self.end.get() {
            return Some(*end);
        }
        let end = match self.tx.ended()? {
            PictureEnd::Sent => SendEnd::Sent,
            PictureEnd::Cancelled => SendEnd::Cancelled,
            PictureEnd::Cut(CutReason::CallEnded) => SendEnd::CallEnded,
            PictureEnd::Cut(CutReason::Unkeyed) => {
                if self.call_lost.load(Ordering::Acquire) {
                    SendEnd::CallEnded
                } else if forced.seq != self.forced_seq
                    && forced.reason == Some(ForcedReason::TxTimeout)
                    && forced.sources & PttSource::Picture.bit() != 0
                {
                    SendEnd::TimedOut
                } else {
                    SendEnd::Cancelled
                }
            }
        };
        Some(*self.end.get_or_init(|| end))
    }
}

/// The station's one picture slot. The watchdog and the handles reap from
/// it. Lock order: the arbiter, then this, then the session; it is never
/// held while either of the others is taken.
#[derive(Clone, Default)]
pub(crate) struct PictureSlot(Arc<Mutex<Option<Arc<Sending>>>>);

impl PictureSlot {
    /// With the arbiter held: if the picture in the slot has ended, fix how,
    /// empty the slot and let go of [`PttSource::Picture`] (which also clears
    /// a latch a time-out or hangup left on it). Returns the arbiter's edge,
    /// which the caller forwards to the console.
    pub(crate) fn reap(&self, arbiter: &mut PttArbiter) -> PttEdge {
        let mut slot = relock(&self.0);
        let Some(sending) = slot.as_ref() else {
            return PttEdge::None;
        };
        if sending.settle(arbiter.snapshot().forced).is_none() {
            return PttEdge::None;
        }
        *slot = None;
        drop(slot);
        arbiter.release(PttSource::Picture)
    }

    /// The call is going away: a picture cut by the unkey that follows ended
    /// because the call did.
    pub(crate) fn call_lost(&self) {
        if let Some(sending) = relock(&self.0).as_ref() {
            sending.call_lost.store(true, Ordering::Release);
        }
    }

    /// A picture is in the slot and still running.
    pub(crate) fn busy(&self) -> bool {
        relock(&self.0)
            .as_ref()
            .is_some_and(|s| s.tx.ended().is_none())
    }

    pub(crate) fn put(&self, sending: Arc<Sending>) {
        *relock(&self.0) = Some(sending);
    }
}

/// A picture being sent: how far it has got, a way to stop it, and how it
/// ended. Dropping the handle does not stop the picture.
pub struct PictureSend {
    pub(crate) sending: Arc<Sending>,
    pub(crate) slot: PictureSlot,
    pub(crate) ptt: Weak<Mutex<PttArbiter>>,
    pub(crate) session: Weak<Mutex<ConsoleSession>>,
}

impl std::fmt::Debug for PictureSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (sent, total) = self.progress();
        f.debug_struct("PictureSend")
            .field("sent", &sent)
            .field("total", &total)
            .field("ended", &self.sending.end.get())
            .finish_non_exhaustive()
    }
}

impl PictureSend {
    /// `(sent, total)`: how much of the transmission has gone out, and how
    /// long it is, lead-in and tail included, in whole 20 ms frames.
    #[must_use]
    pub fn progress(&self) -> (Duration, Duration) {
        let frames = |n: usize| Duration::from_millis(20 * u64::try_from(n).unwrap_or(u64::MAX));
        let total = self.sending.tx.frames_total();
        (
            frames(self.sending.tx.frames_sent().min(total)),
            frames(total),
        )
    }

    /// Stop the picture and unkey. At most one more 20 ms frame goes out.
    /// Does nothing once the picture has ended. When it returns, the station
    /// can send another picture.
    pub fn cancel(&self) {
        self.sending.tx.cancel();
        reap_now(&self.slot, &self.ptt, &self.session);
    }

    /// Test hook: the picture's pacer sleeps `stall` before its next frame,
    /// as if its thread had been descheduled.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn stall_pacer_for_test(&self, stall: Duration) {
        self.sending.tx.stall_pacer(stall);
    }

    /// `None` while the picture is going out; afterwards, how it ended.
    #[must_use]
    pub fn ended(&self) -> Option<SendEnd> {
        if let Some(end) = self.sending.end.get() {
            return Some(*end);
        }
        self.sending.tx.ended()?;
        let forced = self
            .ptt
            .upgrade()
            .map(|ptt| relock(&ptt).snapshot().forced)
            .unwrap_or_default();
        self.sending.settle(forced)
    }
}

/// Reap the slot now, from a thread that holds no lock: take the arbiter,
/// reap (which takes the slot), and forward an unkey to the console. The
/// same lock order as the PTT watchdog's tick: arbiter, slot, session.
fn reap_now(
    slot: &PictureSlot,
    ptt: &Weak<Mutex<PttArbiter>>,
    session: &Weak<Mutex<ConsoleSession>>,
) {
    let Some(ptt) = ptt.upgrade() else {
        return;
    };
    let mut arbiter = relock(&ptt);
    if slot.reap(&mut arbiter) == PttEdge::Unkey
        && let Some(session) = session.upgrade()
    {
        let _ = relock(&session).set_ptt(false);
    }
}

/// How often the end watch looks once the picture is in its last frames.
const END_POLL: Duration = Duration::from_millis(1);
/// How often it looks before then: one frame.
const FAR_POLL: Duration = Duration::from_millis(20);

/// Spawn `astar-picture-end`, which lets go of [`PttSource::Picture`] and
/// unkeys the moment the picture ends, instead of on the PTT watchdog's
/// next 50 ms tick. Without it, a station with a live microphone would send
/// up to 50 ms of microphone audio after the picture's tail, since the call
/// is still keyed when the pacer runs out of frames.
///
/// It polls one frame at a time until the picture is in its last two
/// frames, then every millisecond. It holds only `Weak` references to the
/// arbiter and the session, takes them in the watchdog's order, and never
/// holds a lock while it sleeps, so it adds no lock-order hazard; a hangup
/// that joins the call's run loop while holding the session simply makes
/// it wait. It exits when the picture has ended, or when the station is
/// gone. If the thread cannot start, the watchdog still reaps the picture
/// on its next tick.
pub(crate) fn spawn_end_watch(
    sending: Arc<Sending>,
    slot: PictureSlot,
    ptt: Weak<Mutex<PttArbiter>>,
    session: Weak<Mutex<ConsoleSession>>,
) {
    let _ = std::thread::Builder::new()
        .name("astar-picture-end".into())
        .spawn(move || {
            loop {
                if sending.tx.ended().is_some() {
                    reap_now(&slot, &ptt, &session);
                    return;
                }
                if ptt.strong_count() == 0 {
                    return;
                }
                let left = sending
                    .tx
                    .frames_total()
                    .saturating_sub(sending.tx.frames_sent());
                std::thread::sleep(if left > 2 { FAR_POLL } else { END_POLL });
            }
        });
}
