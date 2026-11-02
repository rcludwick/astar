// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The tap thread and the [`StreamHub`] it feeds.
//!
//! The tap is a non-contributing conference member: it hears the relay mix
//! (every link's received audio, never an announcement played to a member)
//! and can put nothing on the air. One thread enrols it, encodes each 20 ms
//! frame once per format into that format's [`ChunkRing`], and publishes
//! levels every 100 ms. When the member goes away — a switch to handset,
//! to parrot, a dropped channel — it withdraws and keeps trying to
//! re-enrol every two seconds, so the stream comes back on its own.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::conference_tap::TapSource;
use crate::config::StreamCfg;
use crate::public::http::{Format, HubRefusal, Levels, Listener, StreamSource};
use crate::public::status::StreamAvailability;
use crate::stream::encode::{Encoder, OpusEnc};
use crate::stream::levels::{rms_dbfs, voice_spectrum};
use crate::stream::ring::ChunkRing;

/// How far behind a listener may fall before it skips forward.
pub const RING_WINDOW: Duration = Duration::from_secs(2);
/// The MP3 ring keeps more, so a new listener can be handed a burst.
pub const MP3_WINDOW: Duration = Duration::from_secs(10);
/// What a new MP3 listener starts with: enough bytes at 32 kbit/s for a
/// browser to finish probing and start playing at once, at the price of
/// that much delay behind live.
pub const MP3_BACKLOG: Duration = Duration::from_secs(6);
/// How often the tap retries enrolling while there is no conference.
pub const ENROL_RETRY: Duration = Duration::from_secs(2);
/// How often an enrolled tap checks its member still exists.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);
/// One conference tick.
const FRAME: Duration = Duration::from_millis(20);
/// How long the mix may go quiet before the tap fills with silence: a
/// little over two ticks, so scheduling jitter on a live over never inserts
/// a silent frame mid-speech.
const FILL_AFTER: Duration = Duration::from_millis(60);
/// Levels older than this are not reported.
const LEVELS_FRESH: Duration = Duration::from_secs(1);

/// What the tap is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapState {
    /// Enrolled; frames flow at this rate.
    Live { sample_rate: u32 },
    /// No conference to tap: handset or parrot mode, or not started yet.
    NoConference,
}

struct Rings {
    sample_rate: u32,
    opus: Option<Arc<ChunkRing>>,
    mp3: Option<Arc<ChunkRing>>,
}

/// The streams' shared state: rings, levels, the tap's state, the listener
/// count, and the tap thread.
pub struct StreamHub {
    counter: Arc<AtomicU32>,
    max_listeners: u32,
    opus_kbps: u32,
    #[cfg_attr(not(feature = "stream-mp3"), allow(dead_code))]
    mp3_kbps: u32,
    rings: Mutex<Option<Rings>>,
    levels: Mutex<Option<(Instant, Levels)>>,
    state: Mutex<TapState>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl StreamHub {
    /// Start the tap thread against `source`.
    ///
    /// # Panics
    /// If the thread cannot be spawned.
    #[must_use]
    pub fn start(cfg: &StreamCfg, source: Arc<dyn TapSource>) -> Arc<Self> {
        let hub = Arc::new(Self {
            counter: Arc::new(AtomicU32::new(0)),
            max_listeners: cfg.max_listeners,
            opus_kbps: cfg.opus_kbps,
            mp3_kbps: cfg.mp3_kbps,
            rings: Mutex::new(None),
            levels: Mutex::new(None),
            state: Mutex::new(TapState::NoConference),
            stop: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        });
        let worker = Arc::clone(&hub);
        let handle = std::thread::Builder::new()
            .name("stream-tap".into())
            .spawn(move || worker.run(source.as_ref()))
            .expect("spawn the stream tap");
        *hub.thread.lock().expect("hub poisoned") = Some(handle);
        hub
    }

    /// A listener on `format`.
    ///
    /// # Errors
    /// [`HubRefusal`]: not built, no audio now, or full.
    pub fn reader(&self, format: Format) -> Result<crate::stream::ring::RingReader, HubRefusal> {
        if format == Format::Mp3 && !cfg!(feature = "stream-mp3") {
            return Err(HubRefusal::NotBuilt);
        }
        if !matches!(self.state(), TapState::Live { .. }) {
            return Err(HubRefusal::NoAudio);
        }
        let ring = {
            let g = self.rings.lock().expect("hub poisoned");
            let r = g.as_ref().ok_or(HubRefusal::NoAudio)?;
            match format {
                Format::Opus => r.opus.clone(),
                Format::Mp3 => r.mp3.clone(),
            }
            .ok_or(HubRefusal::NoAudio)?
        };
        ring.reader().map_err(|_| HubRefusal::Full)
    }

    /// The latest levels, while fresh.
    #[must_use]
    pub fn latest_levels(&self) -> Option<Levels> {
        self.levels
            .lock()
            .expect("hub poisoned")
            .as_ref()
            .filter(|(at, _)| at.elapsed() < LEVELS_FRESH)
            .map(|(_, l)| l.clone())
    }

    #[must_use]
    pub fn state(&self) -> TapState {
        *self.state.lock().expect("hub poisoned")
    }

    #[must_use]
    pub fn listener_count(&self) -> u32 {
        self.counter.load(Ordering::SeqCst)
    }

    /// Stop the tap thread, withdraw its member, and close the rings.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().expect("hub poisoned").take() {
            let _ = t.join();
        }
        self.close_rings();
    }

    fn close_rings(&self) {
        if let Some(r) = self.rings.lock().expect("hub poisoned").take() {
            for ring in [r.opus, r.mp3].into_iter().flatten() {
                ring.close();
            }
        }
    }

    fn set_state(&self, s: TapState) {
        *self.state.lock().expect("hub poisoned") = s;
    }

    /// Rings for `sample_rate`: kept when the rate is unchanged, replaced
    /// (old listeners told to reconnect) when it changed.
    fn ensure_rings(&self, sample_rate: u32) -> (Option<Arc<ChunkRing>>, Option<Arc<ChunkRing>>) {
        let mut g = self.rings.lock().expect("hub poisoned");
        if g.as_ref().is_none_or(|r| r.sample_rate != sample_rate) {
            if let Some(old) = g.take() {
                for ring in [old.opus, old.mp3].into_iter().flatten() {
                    ring.close();
                }
            }
            let mk = |window, backlog| {
                Some(ChunkRing::with_backlog(
                    window,
                    backlog,
                    self.max_listeners,
                    Arc::clone(&self.counter),
                ))
            };
            *g = Some(Rings {
                sample_rate,
                opus: mk(RING_WINDOW, Duration::ZERO),
                mp3: if cfg!(feature = "stream-mp3") {
                    mk(MP3_WINDOW, MP3_BACKLOG)
                } else {
                    None
                },
            });
        }
        let r = g.as_ref().expect("just set");
        (r.opus.clone(), r.mp3.clone())
    }

    fn sleep(&self, d: Duration) {
        let until = Instant::now() + d;
        while Instant::now() < until && !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn run(&self, source: &dyn TapSource) {
        while !self.stop.load(Ordering::Relaxed) {
            let Some(handle) = source.enrol() else {
                self.set_state(TapState::NoConference);
                self.sleep(ENROL_RETRY);
                continue;
            };
            let rate = handle.sample_rate;
            let (opus_ring, mp3_ring) = self.ensure_rings(rate);
            self.set_state(TapState::Live { sample_rate: rate });
            tracing::info!("stream: tap enrolled at {rate} Hz");
            self.pump(source, &handle, opus_ring.as_ref(), mp3_ring.as_ref());
            source.withdraw(handle.id);
            self.set_state(TapState::NoConference);
            if !self.stop.load(Ordering::Relaxed) {
                tracing::info!("stream: tap withdrawn; re-enrolling when a conference returns");
            }
        }
    }

    /// Encode frames until the member goes away or the hub stops.
    fn pump(
        &self,
        source: &dyn TapSource,
        handle: &astar_iax::BridgeMemberHandle,
        opus_ring: Option<&Arc<ChunkRing>>,
        mp3_ring: Option<&Arc<ChunkRing>>,
    ) {
        let rate = handle.sample_rate;
        let mut opus = Slot::new("opus", {
            let kbps = self.opus_kbps;
            move || OpusEnc::new(rate, kbps).map(|e| Box::new(e) as Box<dyn Encoder>)
        });
        #[cfg(feature = "stream-mp3")]
        let mut mp3 = Slot::new("mp3", {
            let kbps = self.mp3_kbps;
            move || {
                crate::stream::encode::Mp3Enc::new(rate, kbps)
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
            }
        });
        #[cfg(not(feature = "stream-mp3"))]
        let _ = mp3_ring;
        let mut frames = 0u64;
        let mut take = |frame: &[i16]| {
            frames += 1;
            if let Some(r) = opus_ring {
                opus.feed(frame, r);
            }
            #[cfg(feature = "stream-mp3")]
            if let Some(r) = mp3_ring {
                mp3.feed(frame, r);
            }
            if frames.is_multiple_of(5) {
                *self.levels.lock().expect("hub poisoned") = Some((
                    Instant::now(),
                    Levels {
                        dbfs: rms_dbfs(frame),
                        spectrum: voice_spectrum(frame, rate).to_vec(),
                        rate,
                    },
                ));
            }
        };
        // The bridge sends a member nothing, not silence, while nobody it
        // hears is talking (an app_rpt link keys on any voice frame). A
        // stream must not stall between overs, so once the mix has been
        // quiet for FILL_AFTER the tap encodes its own silent frames, paced
        // at one per 20 ms tick.
        let silence = vec![0i16; usize::try_from(rate / 50).unwrap_or(160)];
        let mut next_fill = Instant::now() + FILL_AFTER;
        let mut checked = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            let wait = next_fill
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            match handle.from_conference.recv_timeout(wait) {
                Ok(frame) => {
                    take(&frame);
                    next_fill = Instant::now() + FILL_AFTER;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if now >= next_fill {
                        take(&silence);
                        next_fill += FRAME;
                        // After a stall (a suspended thread), resume from
                        // now rather than bursting the missed frames.
                        if next_fill + FILL_AFTER < now {
                            next_fill = now + FRAME;
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if checked.elapsed() >= CHECK_EVERY {
                checked = Instant::now();
                if !source.still_enrolled(handle.id) {
                    return;
                }
            }
        }
    }
}

/// One format's encoder: built lazily, rebuilt after a panic, and given up
/// on (with one log line) when it cannot be built at all.
struct Slot<F: Fn() -> Result<Box<dyn Encoder>, String>> {
    name: &'static str,
    make: F,
    enc: Option<Box<dyn Encoder>>,
    broken: bool,
}

impl<F: Fn() -> Result<Box<dyn Encoder>, String>> Slot<F> {
    fn new(name: &'static str, make: F) -> Self {
        Self {
            name,
            make,
            enc: None,
            broken: false,
        }
    }

    fn feed(&mut self, frame: &[i16], ring: &ChunkRing) {
        if self.broken {
            return;
        }
        if self.enc.is_none() {
            match (self.make)() {
                Ok(e) => self.enc = Some(e),
                Err(e) => {
                    tracing::warn!("stream: the {} encoder did not start: {e}", self.name);
                    self.broken = true;
                    return;
                }
            }
        }
        let enc = self.enc.as_mut().expect("just built");
        match catch_unwind(AssertUnwindSafe(|| enc.encode(frame))) {
            Ok(bytes) if !bytes.is_empty() => ring.push(Arc::from(bytes)),
            Ok(_) => {}
            Err(_) => {
                tracing::warn!("stream: the {} encoder panicked; rebuilding it", self.name);
                self.enc = None;
            }
        }
    }
}

impl StreamSource for StreamHub {
    fn open(&self, format: Format) -> Result<Box<dyn Listener>, HubRefusal> {
        self.reader(format)
            .map(|r| Box::new(r) as Box<dyn Listener>)
    }
    fn levels(&self) -> Option<Levels> {
        self.latest_levels()
    }
    fn availability(&self) -> StreamAvailability {
        match self.state() {
            TapState::Live { .. } => StreamAvailability::Live,
            TapState::NoConference => StreamAvailability::Unavailable,
        }
    }
    fn listeners(&self) -> u32 {
        self.listener_count()
    }
    fn sample_rate(&self) -> Option<u32> {
        match self.state() {
            TapState::Live { sample_rate } => Some(sample_rate),
            TapState::NoConference => None,
        }
    }
    fn mp3_built(&self) -> bool {
        cfg!(feature = "stream-mp3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public::http::Pull;
    use crate::stream::encode::tone;
    use std::sync::mpsc;

    /// A fake station: hands out members whose mix the test feeds.
    struct Fake {
        enrolments: AtomicU32,
        allow: AtomicBool,
        enrolled: AtomicBool,
        feeds: Mutex<Vec<mpsc::Sender<Vec<i16>>>>,
        rate: u32,
    }

    impl Fake {
        fn new(rate: u32, allow: bool) -> Arc<Self> {
            Arc::new(Self {
                enrolments: AtomicU32::new(0),
                allow: AtomicBool::new(allow),
                enrolled: AtomicBool::new(false),
                feeds: Mutex::new(Vec::new()),
                rate,
            })
        }

        fn feed(&self, frame: &[i16]) {
            if let Some(tx) = self.feeds.lock().unwrap().last() {
                let _ = tx.send(frame.to_vec());
            }
        }
    }

    impl TapSource for Fake {
        fn enrol(&self) -> Option<astar_iax::BridgeMemberHandle> {
            if !self.allow.load(Ordering::SeqCst) {
                return None;
            }
            let n = self.enrolments.fetch_add(1, Ordering::SeqCst);
            let (to_conference, _rx) = mpsc::channel();
            let (tx, from_conference) = mpsc::channel();
            self.feeds.lock().unwrap().push(tx);
            self.enrolled.store(true, Ordering::SeqCst);
            Some(astar_iax::BridgeMemberHandle {
                id: astar_audio::MemberId::from_raw(u64::from(n)),
                to_conference,
                from_conference,
                sample_rate: self.rate,
            })
        }
        fn still_enrolled(&self, _id: astar_audio::MemberId) -> bool {
            self.enrolled.load(Ordering::SeqCst)
        }
        fn withdraw(&self, _id: astar_audio::MemberId) {
            self.enrolled.store(false, Ordering::SeqCst);
        }
    }

    fn cfg() -> StreamCfg {
        StreamCfg {
            enabled: true,
            ..StreamCfg::default()
        }
    }

    fn wait(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn frames_become_opus_chunks_and_levels() {
        let fake = Fake::new(16_000, true);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        wait("live", || {
            hub.state()
                == TapState::Live {
                    sample_rate: 16_000,
                }
        });
        let mut reader = hub.reader(Format::Opus).unwrap();
        assert_eq!(hub.listener_count(), 1);
        let pcm = tone(16_000, 1000.0, 1000);
        for f in pcm.chunks(320) {
            fake.feed(f);
        }
        let mut chunks = 0;
        while let Pull::Chunk(_) = reader.pull(Duration::from_millis(300)) {
            chunks += 1;
            if chunks == 50 {
                break;
            }
        }
        assert!(chunks >= 45, "{chunks} Opus packets for 50 frames");
        // Levels while a tone is playing, fed at the real 20 ms pace: once
        // the mix goes quiet the tap's silence fill takes the levels over.
        let toner = {
            let fake = Arc::clone(&fake);
            let pcm = pcm.clone();
            std::thread::spawn(move || {
                for f in pcm.chunks(320).cycle().take(150) {
                    fake.feed(f);
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        wait("tone levels", || {
            hub.latest_levels().is_some_and(|l| l.dbfs > -30.0)
        });
        let l = hub.latest_levels().expect("levels published");
        assert!(l.spectrum.len() == 32 && l.rate == 16_000, "{l:?}");
        toner.join().unwrap();
        drop(reader);
        assert_eq!(hub.listener_count(), 0);
        hub.stop();
    }

    #[test]
    fn an_idle_conference_still_streams_silence() {
        // The bridge sends an idle member nothing at all (not silence), so
        // the tap fills the gap itself: a listener hears quiet, not a stall.
        let fake = Fake::new(8_000, true);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        wait("live", || {
            hub.state() == TapState::Live { sample_rate: 8_000 }
        });
        let mut reader = hub.reader(Format::Opus).unwrap();
        let start = Instant::now();
        let mut chunks = 0;
        while start.elapsed() < Duration::from_secs(1) {
            if let Pull::Chunk(_) = reader.pull(Duration::from_millis(300)) {
                chunks += 1;
            }
        }
        assert!(chunks >= 35, "{chunks} Opus packets in 1 s of silence");
        let l = hub.latest_levels().expect("silence publishes levels too");
        assert!(l.dbfs < -60.0, "{l:?}");
        hub.stop();
    }

    #[test]
    fn no_conference_means_no_audio_and_retries() {
        let fake = Fake::new(8_000, false);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hub.state(), TapState::NoConference);
        assert_eq!(hub.reader(Format::Opus).err(), Some(HubRefusal::NoAudio));
        assert_eq!(hub.availability(), StreamAvailability::Unavailable);
        fake.allow.store(true, Ordering::SeqCst);
        wait("live after a retry", || {
            matches!(hub.state(), TapState::Live { .. })
        });
        assert_eq!(hub.sample_rate(), Some(8_000));
        hub.stop();
    }

    #[test]
    fn dropped_member_is_re_enrolled() {
        let fake = Fake::new(16_000, true);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        wait("live", || matches!(hub.state(), TapState::Live { .. }));
        let mut reader = hub.reader(Format::Opus).unwrap();
        // A Handset switch: the member disappears underneath the tap.
        fake.enrolled.store(false, Ordering::SeqCst);
        wait("re-enrolled", || {
            fake.enrolments.load(Ordering::SeqCst) >= 2
        });
        wait("live again", || {
            matches!(hub.state(), TapState::Live { .. })
        });
        // Same rate, same rings: the listener carries on.
        for f in tone(16_000, 1000.0, 200).chunks(320) {
            fake.feed(f);
        }
        assert!(matches!(
            reader.pull(Duration::from_secs(1)),
            Pull::Chunk(_)
        ));
        hub.stop();
        assert!(matches!(
            reader.pull(Duration::from_millis(100)),
            Pull::Closed
        ));
    }

    #[test]
    fn the_cap_counts_both_streams() {
        let fake = Fake::new(16_000, true);
        let hub = StreamHub::start(
            &StreamCfg {
                enabled: true,
                max_listeners: 1,
                ..StreamCfg::default()
            },
            Arc::clone(&fake) as Arc<dyn TapSource>,
        );
        wait("live", || matches!(hub.state(), TapState::Live { .. }));
        let _one = hub.reader(Format::Opus).unwrap();
        assert_eq!(hub.reader(Format::Opus).err(), Some(HubRefusal::Full));
        #[cfg(feature = "stream-mp3")]
        assert_eq!(hub.reader(Format::Mp3).err(), Some(HubRefusal::Full));
        hub.stop();
    }

    #[cfg(not(feature = "stream-mp3"))]
    #[test]
    fn mp3_not_built_is_reported() {
        let fake = Fake::new(16_000, true);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        assert_eq!(hub.reader(Format::Mp3).err(), Some(HubRefusal::NotBuilt));
        assert!(!hub.mp3_built());
        hub.stop();
    }

    #[cfg(feature = "stream-mp3")]
    #[test]
    fn mp3_chunks_carry_frame_sync() {
        let fake = Fake::new(16_000, true);
        let hub = StreamHub::start(&cfg(), Arc::clone(&fake) as Arc<dyn TapSource>);
        wait("live", || matches!(hub.state(), TapState::Live { .. }));
        let mut reader = hub.reader(Format::Mp3).unwrap();
        for f in tone(16_000, 1000.0, 1000).chunks(320) {
            fake.feed(f);
        }
        // The stream never goes quiet (the tap fills idle with silence), so
        // read for a bounded time rather than until it stops.
        let mut bytes = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if let Pull::Chunk(c) = reader.pull(Duration::from_millis(300)) {
                bytes.extend_from_slice(&c);
            }
        }
        assert!(
            bytes
                .windows(2)
                .any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
        );
        hub.stop();
    }
}
