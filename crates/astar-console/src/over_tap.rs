// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The codec-frame recording tap the System Fusion, NXDN and DMR links
//! share.
//!
//! None of the three networks carries a sequence number the link keeps, so
//! every `Frame` goes out with `seq: None`. What differs between them is only
//! the bytes (each link converts its own) and the edges of an over, which
//! each link reads off its own wire and reports through [`OverTap::note_rx`]
//! and [`OverTap::close`].
//!
//! Every method only ever *offers* to the tap: a full or absent tap costs
//! the link thread nothing.

use std::time::{Duration, Instant, SystemTime};

use astar_record::frames::{CaptureMode, Dir, FRAME_IDLE_CLOSE, FrameEvent};
use astar_record::tap::{StartedOn, TapSlot};

/// A received over that goes this long without a packet is treated as
/// ended, and the next packet opens a new recording — even from the same
/// talker. Half the writer's [`FRAME_IDLE_CLOSE`], so this side always
/// closes first: a talker who keys again after a lost end-of-transmission
/// gets a fresh `Start` instead of frames the writer, having idled the
/// stream out, would drop for arriving without one.
const RX_QUIET: Duration = FRAME_IDLE_CLOSE.checked_div(2).expect("non-zero divisor");

/// One open recorded stream.
struct Open {
    talker: Option<String>,
    last: Instant,
    /// Whether the `Start` reached the tap installed now; until it has,
    /// the next packet or frame offers it again, so a recording begun
    /// mid-over gets the rest of the over.
    started: StartedOn,
}

impl Open {
    /// Whether a packet from `talker` at `now` belongs to this over.
    fn continues(&self, talker: &str, now: Instant) -> bool {
        self.talker.as_deref().unwrap_or("") == talker
            && now.saturating_duration_since(self.last) < RX_QUIET
    }
}

/// The frame tap of one link, plus the over it has open in each direction.
pub(crate) struct OverTap {
    slot: TapSlot<FrameEvent>,
    network: &'static str,
    mode: CaptureMode,
    vocoder: &'static str,
    rx: Option<Open>,
    tx: Option<Open>,
}

impl OverTap {
    pub(crate) fn new(network: &'static str, mode: CaptureMode, vocoder: &'static str) -> Self {
        Self {
            slot: TapSlot::default(),
            network,
            mode,
            vocoder,
            rx: None,
            tx: None,
        }
    }

    /// The shared slot; setting it reaches the link thread.
    pub(crate) fn slot(&self) -> TapSlot<FrameEvent> {
        self.slot.clone()
    }

    fn open_mut(&mut self, dir: Dir) -> &mut Option<Open> {
        match dir {
            Dir::Rx => &mut self.rx,
            Dir::Tx => &mut self.tx,
        }
    }

    /// Whether [`Self::note_rx`] with these arguments would close the open
    /// received recording and start another. A link that sees `true` flushes
    /// its decode pipeline first, so the old talker's frames still queued or
    /// in flight finish inside the old recording rather than the new one.
    pub(crate) fn rotates(&self, talker: &str, now: Instant) -> bool {
        self.rx
            .as_ref()
            .is_some_and(|open| !open.continues(talker, now))
    }

    /// A received packet from `talker` (empty for none) is about to be
    /// decoded at `now`: keep the open recording when it is the same talker
    /// and not gone quiet, otherwise close it and open a new one. Call it
    /// only once the packet is known to carry voice, so an over with none
    /// leaves no recording behind.
    pub(crate) fn note_rx(&mut self, talker: &str, now: Instant) {
        if let Some(open) = self.rx.as_mut()
            && open.continues(talker, now)
        {
            open.last = now;
            self.ensure_started(Dir::Rx);
            return;
        }
        self.start(
            Dir::Rx,
            (!talker.is_empty()).then(|| talker.to_owned()),
            now,
        );
    }

    /// Open a recording in `dir`, closing whatever was open there.
    pub(crate) fn start(&mut self, dir: Dir, talker: Option<String>, now: Instant) {
        self.close(dir);
        *self.open_mut(dir) = Some(Open {
            talker,
            last: now,
            started: StartedOn::default(),
        });
        self.ensure_started(dir);
    }

    /// Offer the open over's `Start` in `dir` if it has not reached the
    /// tap installed now. Costs a lock-free read once it has.
    fn ensure_started(&mut self, dir: Dir) {
        let (network, mode, vocoder) = (self.network, self.mode, self.vocoder);
        let slot = self.slot.clone();
        let Some(open) = self.open_mut(dir).as_mut() else {
            return;
        };
        let talker = &open.talker;
        open.started.ensure(&slot, || FrameEvent::Start {
            dir,
            network,
            mode,
            talker: talker.clone(),
            at: SystemTime::now(),
            vocoder,
        });
    }

    /// One codec frame, as it goes into (or comes out of) the vocoder.
    /// `bytes` runs only when a tap is installed, so a conversion inside it
    /// costs nothing on a link nobody is recording. An open over whose
    /// `Start` has not reached the tap offers it first.
    pub(crate) fn frame_with(&mut self, dir: Dir, bytes: impl FnOnce() -> Vec<u8>) {
        self.ensure_started(dir);
        self.slot.offer_with(|| FrameEvent::Frame {
            dir,
            bytes: bytes(),
            seq: None,
        });
    }

    /// The 160 samples paired with the oldest unpaired frame.
    pub(crate) fn pcm(&self, dir: Dir, pcm: &[i16; 160]) {
        self.slot.offer_with(|| FrameEvent::Pcm { dir, pcm: *pcm });
    }

    /// Close the recording in `dir`, if one is open.
    pub(crate) fn close(&mut self, dir: Dir) {
        if self
            .open_mut(dir)
            .take()
            .is_some_and(|open| open.started.started())
        {
            self.slot.offer_with(|| FrameEvent::End { dir });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rx: &std::sync::mpsc::Receiver<FrameEvent>) -> Vec<&'static str> {
        rx.try_iter()
            .map(|e| match e {
                FrameEvent::Start { .. } => "start",
                FrameEvent::Frame { .. } => "frame",
                FrameEvent::Pcm { .. } => "pcm",
                FrameEvent::End { .. } => "end",
            })
            .collect()
    }

    /// The producer gives up on a quiet over before the writer does.
    #[test]
    fn rx_quiet_is_inside_the_writers_idle_close() {
        assert_eq!(RX_QUIET * 2, FRAME_IDLE_CLOSE);
    }

    /// One talker keeps one recording; a new talker, or the same one after
    /// a quiet second, opens a new one; closing twice ends once.
    #[test]
    fn note_rx_opens_one_recording_per_over() {
        let (tap, events) = astar_record::tap::tap_channel(32);
        let mut t = OverTap::new("nxdn", CaptureMode::Nxdn, "hardware");
        t.slot().set(Some(tap));
        let t0 = Instant::now();
        assert!(!t.rotates("1234", t0), "nothing open, nothing to rotate");
        t.note_rx("1234", t0);
        assert!(!t.rotates("1234", t0 + Duration::from_millis(80)));
        t.note_rx("1234", t0 + Duration::from_millis(80));
        assert_eq!(kinds(&events), ["start"]);
        assert!(t.rotates("5678", t0 + Duration::from_millis(160)));
        t.note_rx("5678", t0 + Duration::from_millis(160));
        assert_eq!(kinds(&events), ["end", "start"]);
        assert!(t.rotates("5678", t0 + Duration::from_millis(160) + RX_QUIET));
        t.note_rx("5678", t0 + Duration::from_millis(160) + RX_QUIET);
        assert_eq!(kinds(&events), ["end", "start"]);
        t.close(Dir::Rx);
        t.close(Dir::Rx);
        assert_eq!(kinds(&events), ["end"]);
    }

    /// Record pressed mid-over: the tap appears after the over began, and
    /// the next packet (or, transmitting, the next frame) opens the
    /// recording for the rest of it.
    #[test]
    fn a_tap_set_mid_over_records_the_rest_of_the_over() {
        let mut t = OverTap::new("nxdn", CaptureMode::Nxdn, "hardware");
        let t0 = Instant::now();
        t.note_rx("1234", t0);
        t.start(Dir::Tx, Some("W1ABC".into()), t0);
        t.frame_with(Dir::Tx, || vec![1; 7]);
        let (tap, events) = astar_record::tap::tap_channel(32);
        t.slot().set(Some(tap));
        t.note_rx("1234", t0 + Duration::from_millis(80));
        t.frame_with(Dir::Rx, || vec![2; 7]);
        t.frame_with(Dir::Tx, || vec![3; 7]);
        t.frame_with(Dir::Tx, || vec![4; 7]);
        t.close(Dir::Rx);
        t.close(Dir::Tx);
        assert_eq!(
            kinds(&events),
            ["start", "frame", "start", "frame", "frame", "end", "end"]
        );
    }
}
