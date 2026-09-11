// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Summing output mixer: one bus, N call RX channels (iax-42e9 phase 2).
//!
//! Each call routed to a bus contributes a `Receiver<RxFrame>` of PCM RX
//! frames + its own cushion (`residual`, mirroring `SpeakerSource`).
//! `read` normalizes i16 → f32, sums sample-aligned across calls, and hard
//! clamps to [-1, 1] so two loud calls can't wrap. A starved call contributes
//! silence and never blocks the others. Codec transcode happens at the network
//! edge (iax-31f7), not here.
//!
//! # The jitter buffer (iax-rxjb)
//!
//! A lane whose frames carry a SENDER clock ([`RxClock`]) is played out of an
//! adaptive [`JitterBuf`] — Asterisk's `jitterbuf.c`, ported in
//! `astar_codec::jitter` — pulled by the playback clock on every [`Mixer::read`]:
//! arrivals go in with `put`, and `get` hands back whatever is due *now*.
//! That is what absorbs a network that delivers 20 ms frames 0/20/40 ms apart;
//! without it the lane's depth target is zero and any inter-arrival gap longer
//! than one device callback is an audible hole.
//!
//! A lane whose frames carry NO sender clock (`RxFrame::from(Vec<i16>)` — the
//! parrot, announcements, M17/D-Star/DMR/YSF/NXDN decoders, tests) bypasses the
//! buffer entirely and behaves exactly as it did before: straight into the
//! residual, byte-identical. There is one jitter buffer per lane, created on
//! that lane's first timestamped frame.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use astar_codec::jitter::{Frame, FrameType, GetResult, JitterBuf, JitterConfig};

/// Jitter-buffer tuning for the `AllStarLink` RX path.
///
/// These are Asterisk `chan_iax2`'s shipped defaults (`iax.conf`
/// `jitterbuffer`/`maxjitterbuffer`/`resyncthreshold`/`maxjitterinterps`,
/// via `jitterbuf.c`), and they are the numbers on purpose: the node at the
/// other end of an `AllStarLink` call IS Asterisk, so matching its buffer is
/// what makes our playout behave like the thing operators already have a feel
/// for. 40 ms of slack over measured jitter, a 200 ms ceiling, a 1 s
/// resynchronisation threshold, and at most 10 consecutive interpolations
/// (200 ms) before the buffer decides the talk spurt is over.
///
/// [`RxJitterConfig`] moves the first two of those (`target_extra` and
/// `max_jitterbuf`); the resync threshold and the interpolation cap are not
/// knobs — they are what makes the port behave like the thing it is a port of.
pub const IAX_JITTER_CONFIG: JitterConfig = JitterConfig {
    max_jitterbuf: 200,
    resync_threshold: 1000,
    max_contig_interp: 10,
    target_extra: 40,
};

/// Frame duration assumed when we have not yet seen one (20 ms = one IAX2
/// voice frame).
const DEFAULT_FRAME_MS: i64 = 20;
/// Samples of silence synthesised for an interpolation before any real frame
/// has set the length (160 = 20 ms at 8 kHz).
const DEFAULT_FRAME_SAMPLES: usize = 160;
/// How long after a timestamped frame a lane still counts as mid-talk-spurt
/// when the jitter buffer is switched OFF. Matches the buffer's own
/// `max_contig_interp × 20 ms` — the point at which the enabled path decides a
/// spurt is over — so `rx_underruns` means the same thing either way.
const DIRECT_VOICE_IDLE_MS: i64 = 200;

/// The RX jitter buffer as an operator sees it: on or off, and the window the
/// adaptive depth is allowed to live in.
///
/// `min_ms` is the floor of slack the adaptive target sits on (`jitterbuf.c`'s
/// `target_extra`) and `max_ms` the ceiling it may not grow past
/// (`max_jitterbuf`). Bigger `min_ms` = more latency, fewer holes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxJitterConfig {
    /// `false` plays received audio straight through, as astar did before the
    /// buffer existed.
    pub enabled: bool,
    /// Floor of the adaptive depth, ms.
    pub min_ms: u32,
    /// Ceiling of the adaptive depth, ms.
    pub max_ms: u32,
}

impl Default for RxJitterConfig {
    /// On, with Asterisk `chan_iax2`'s numbers (see [`IAX_JITTER_CONFIG`]).
    fn default() -> Self {
        Self {
            enabled: true,
            min_ms: 40,
            max_ms: 200,
        }
    }
}

impl RxJitterConfig {
    /// Hard ceiling on either bound. Half a second of buffered audio is
    /// already past the point where a conversation works.
    pub const LIMIT_MS: u32 = 500;

    /// Bring a caller's numbers into range: both bounds clamped to
    /// `0..=LIMIT_MS`, and a `max_ms` below `min_ms` raised to meet it. Never
    /// an error — a setting the operator typed is repaired, not refused.
    #[must_use]
    pub fn clamped(self) -> Self {
        let min_ms = self.min_ms.min(Self::LIMIT_MS);
        let max_ms = self.max_ms.min(Self::LIMIT_MS).max(min_ms);
        Self {
            enabled: self.enabled,
            min_ms,
            max_ms,
        }
    }

    /// The `jitterbuf.c` configuration this maps onto.
    #[must_use]
    pub fn to_jitter_config(self) -> JitterConfig {
        let c = self.clamped();
        JitterConfig {
            target_extra: i64::from(c.min_ms),
            max_jitterbuf: i64::from(c.max_ms),
            ..IAX_JITTER_CONFIG
        }
    }
}

/// The live [`RxJitterConfig`] cell a mixer reads on every callback, so a
/// change applies to a call in progress without reconnecting.
#[derive(Debug)]
pub struct RxJitterSettings {
    enabled: AtomicBool,
    min_ms: AtomicU32,
    max_ms: AtomicU32,
    /// Bumped on every [`RxJitterSettings::set`] so a lane can tell, cheaply
    /// and on the audio thread, that it has to re-read the config.
    generation: AtomicU64,
}

impl Default for RxJitterSettings {
    fn default() -> Self {
        Self::new(RxJitterConfig::default())
    }
}

impl RxJitterSettings {
    /// A cell seeded with `cfg` (clamped).
    #[must_use]
    pub fn new(cfg: RxJitterConfig) -> Self {
        let cfg = cfg.clamped();
        Self {
            enabled: AtomicBool::new(cfg.enabled),
            min_ms: AtomicU32::new(cfg.min_ms),
            max_ms: AtomicU32::new(cfg.max_ms),
            generation: AtomicU64::new(0),
        }
    }

    /// Replace the live configuration. Clamped, never refused.
    pub fn set(&self, cfg: RxJitterConfig) {
        let cfg = cfg.clamped();
        self.enabled.store(cfg.enabled, Ordering::Relaxed);
        self.min_ms.store(cfg.min_ms, Ordering::Relaxed);
        self.max_ms.store(cfg.max_ms, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// The configuration the mixer is actually using.
    #[must_use]
    pub fn get(&self) -> RxJitterConfig {
        RxJitterConfig {
            enabled: self.enabled.load(Ordering::Relaxed),
            min_ms: self.min_ms.load(Ordering::Relaxed),
            max_ms: self.max_ms.load(Ordering::Relaxed),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// The sender's clock for one received frame: the wire timestamp and the
/// frame's own duration, both in ms.
///
/// Present only on frames that came off a protocol carrying a media clock
/// (IAX2 voice). Everything else sends bare PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxClock {
    /// Sender timestamp in ms, extended past any wire-level wrap.
    pub ts_ms: i64,
    /// Duration of this frame in ms.
    pub ms: i64,
}

/// One decoded RX frame on its way to an output bus.
///
/// `clock: None` means "no sender clock": the lane plays it straight through,
/// exactly as the mixer did before the jitter buffer existed. Use
/// `RxFrame::from(pcm)` for that path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RxFrame {
    /// Decoded PCM at the bus rate.
    pub pcm: Vec<i16>,
    /// The sender's clock, when the source has one.
    pub clock: Option<RxClock>,
}

impl RxFrame {
    /// A frame carrying the sender's clock — the jitter-buffered path.
    #[must_use]
    pub fn timed(pcm: Vec<i16>, ts_ms: i64, ms: i64) -> Self {
        Self {
            pcm,
            clock: Some(RxClock { ts_ms, ms }),
        }
    }

    /// A frame with no sender clock — played straight through.
    #[must_use]
    pub fn untimed(pcm: Vec<i16>) -> Self {
        Self { pcm, clock: None }
    }
}

impl From<Vec<i16>> for RxFrame {
    /// Bare PCM: no sender clock, so the lane bypasses the jitter buffer.
    fn from(pcm: Vec<i16>) -> Self {
        Self::untimed(pcm)
    }
}

/// The playback-side ms clock a [`Mixer`] pulls its jitter buffers with.
///
/// Monotonic in production; a test hands the mixer a cell it advances by hand
/// so a lane's playout can be driven frame by frame without sleeping.
#[derive(Debug, Clone)]
pub enum MixerClock {
    /// Milliseconds since the mixer was built.
    Monotonic(Instant),
    /// A cell the caller advances (tests).
    Fake(Arc<AtomicI64>),
}

impl MixerClock {
    /// A hand-driven clock plus the cell that advances it.
    #[must_use]
    pub fn fake() -> (Self, Arc<AtomicI64>) {
        let cell = Arc::new(AtomicI64::new(0));
        (Self::Fake(Arc::clone(&cell)), cell)
    }

    /// Current reading in ms.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        match self {
            Self::Monotonic(start) => {
                i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
            }
            Self::Fake(cell) => cell.load(Ordering::Relaxed),
        }
    }
}

impl Default for MixerClock {
    fn default() -> Self {
        Self::Monotonic(Instant::now())
    }
}

/// Live per-lane jitter-buffer health, published for the call snapshot.
///
/// Plain counters — no identity, no secret. Zero on a lane that has no sender
/// clock (nothing is jitter-buffered, so there is nothing to report).
#[derive(Debug, Default)]
pub struct RxJitterCells {
    /// Estimated network jitter in ms (max-min over the delay history).
    pub jitter_ms: AtomicU32,
    /// Current buffer depth in ms.
    pub depth_ms: AtomicU32,
    /// Frames the buffer expected and never saw (each one an interpolation).
    pub frames_lost: AtomicU64,
    /// Frames that arrived after their play time and were thrown away.
    pub frames_late: AtomicU64,
    /// Frames that arrived out of timestamp order (reordered in place).
    pub frames_ooo: AtomicU64,
}

impl RxJitterCells {
    /// Back to zero — what a lane with no live jitter buffer reports.
    fn clear(&self) {
        self.jitter_ms.store(0, Ordering::Relaxed);
        self.depth_ms.store(0, Ordering::Relaxed);
        self.frames_lost.store(0, Ordering::Relaxed);
        self.frames_late.store(0, Ordering::Relaxed);
        self.frames_ooo.store(0, Ordering::Relaxed);
    }

    fn publish(&self, info: &astar_codec::jitter::JitterStats) {
        let clamp_u32 = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
        let clamp_u64 = |v: i64| u64::try_from(v.max(0)).unwrap_or(u64::MAX);
        self.jitter_ms
            .store(clamp_u32(info.jitter), Ordering::Relaxed);
        self.depth_ms
            .store(clamp_u32(info.current), Ordering::Relaxed);
        self.frames_lost
            .store(clamp_u64(info.frames_lost), Ordering::Relaxed);
        self.frames_late
            .store(clamp_u64(info.frames_late), Ordering::Relaxed);
        self.frames_ooo
            .store(clamp_u64(info.frames_ooo), Ordering::Relaxed);
    }
}

/// Opaque per-call slot id within a [`Mixer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixCallId(u64);

/// A lane's jitter buffer plus the bookkeeping needed to drive it.
struct LaneJitter {
    buf: JitterBuf<Vec<i16>>,
    /// Sender ts of this lane's first frame; subtracted from every ts so the
    /// buffer's timestamp axis starts at 0 whenever the lane joined.
    ts_base: i64,
    /// Playback clock at that first frame; subtracted from every `now` for the
    /// same reason. Together these make `now - ts` the transit delay relative
    /// to the first frame, which is all the buffer ever needs.
    now_base: i64,
    /// Most recent frame duration (ms) — what we interpolate with.
    frame_ms: i64,
    /// Most recent frame length in samples — how much silence one
    /// interpolation is worth on this bus.
    frame_samples: usize,
}

impl LaneJitter {
    fn new(cfg: RxJitterConfig, ts_base: i64, now_base: i64) -> Self {
        Self {
            buf: JitterBuf::new(cfg.to_jitter_config()),
            ts_base,
            now_base,
            frame_ms: DEFAULT_FRAME_MS,
            frame_samples: DEFAULT_FRAME_SAMPLES,
        }
    }

    /// Is the buffer out of audio mid-talk-spurt?
    ///
    /// `silence_begin_ts == 0` is `jitterbuf.c`'s literal "voice mode" flag
    /// (the post-reset sentinel is -1, and the end of a spurt stores a real
    /// timestamp). The empty-queue half matters as much: a buffer that is
    /// holding frames back is deepening its cushion on purpose, which is the
    /// job, not a hole.
    fn starving(&self) -> bool {
        let info = self.buf.info();
        info.silence_begin_ts == 0 && info.frames_cur == 0
    }
}

struct Lane {
    id: MixCallId,
    inbound: Receiver<RxFrame>,
    residual: VecDeque<f32>,
    /// Present once this lane has seen a frame with a sender clock.
    jb: Option<LaneJitter>,
    /// Health counters this lane publishes for the call snapshot.
    stats: Arc<RxJitterCells>,
    /// Generation of [`RxJitterSettings`] this lane has already applied.
    applied_gen: Option<u64>,
    /// Playback time up to which this lane counts as mid-talk-spurt while the
    /// buffer is switched OFF (the enabled path asks the buffer instead).
    timed_until: i64,
    /// Set once for a finite announcement lane; flipped + removed when the
    /// inbound channel is closed and the residual is drained (iax-e30d).
    done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Audio-thread record that the inbound sender has hung up.
    disconnected: bool,
}

impl Lane {
    fn new(id: MixCallId, inbound: Receiver<RxFrame>, stats: Arc<RxJitterCells>) -> Self {
        Self {
            id,
            inbound,
            residual: VecDeque::new(),
            jb: None,
            stats,
            applied_gen: None,
            timed_until: i64::MIN,
            done: None,
            disconnected: false,
        }
    }

    fn push_pcm(&mut self, pcm: &[i16]) {
        for &s in pcm {
            self.residual.push_back(f32::from(s) / 32768.0);
        }
    }

    /// Adopt a changed [`RxJitterConfig`] mid-call.
    ///
    /// Switching the buffer OFF drains whatever it is holding into the
    /// residual — the audio is already received and not yet played, so it
    /// comes out once, in order — and zeroes the counters. Switching it back
    /// ON leaves `jb` empty, so the next timestamped frame starts a fresh
    /// buffer. A min/max change is pushed into the live buffer.
    fn sync_config(&mut self, cfg: RxJitterConfig, generation: u64) {
        if self.applied_gen == Some(generation) {
            return;
        }
        self.applied_gen = Some(generation);
        if cfg.enabled {
            if let Some(jb) = self.jb.as_mut() {
                jb.buf.set_config(cfg.to_jitter_config());
            }
            return;
        }
        if let Some(mut jb) = self.jb.take() {
            for frame in jb.buf.drain() {
                self.push_pcm(&frame.data);
            }
        }
        self.stats.clear();
    }

    /// Take one arrival. Timestamped frames go into the jitter buffer;
    /// bare PCM — and everything, when the buffer is switched off — goes
    /// straight into the residual (the pre-jitter-buffer path, unchanged).
    fn accept(&mut self, frame: RxFrame, now: i64, cfg: RxJitterConfig) {
        let Some(clock) = frame.clock else {
            self.push_pcm(&frame.pcm);
            return;
        };
        self.timed_until = now + DIRECT_VOICE_IDLE_MS;
        if !cfg.enabled {
            self.push_pcm(&frame.pcm);
            return;
        }
        let jb = self
            .jb
            .get_or_insert_with(|| LaneJitter::new(cfg, clock.ts_ms, now));
        if clock.ms > 0 {
            jb.frame_ms = clock.ms;
        }
        if !frame.pcm.is_empty() {
            jb.frame_samples = frame.pcm.len();
        }
        let ms = jb.frame_ms;
        // A discontinuity drop is `jitterbuf.c`'s own behaviour (three in a
        // row and it resyncs the timestamp axis instead); the frame is gone
        // either way, and `frames_lost` will show for it.
        let _ = jb.buf.put(
            Frame {
                data: frame.pcm,
                ts: clock.ts_ms - jb.ts_base,
                ms,
                frame_type: FrameType::Voice,
            },
            now - jb.now_base,
        );
    }

    /// Pull everything the jitter buffer says is due by `now`, up to `need`
    /// samples of residual. No-op on a lane with no sender clock.
    fn pull(&mut self, need: usize, now: i64) {
        let Some(jb) = self.jb.as_mut() else { return };
        let now = now - jb.now_base;
        while self.residual.len() < need {
            // Pace against the buffer's own schedule: `get` hands frames back
            // as fast as it is asked, so without this the first callback of a
            // talk spurt would drain the whole cushion it just built.
            match jb.buf.next() {
                Some(due) if due <= now => {}
                _ => break,
            }
            match jb.buf.get(now, jb.frame_ms) {
                GetResult::Ok(frame) => {
                    if !frame.data.is_empty() {
                        jb.frame_samples = frame.data.len();
                    }
                    for &s in &frame.data {
                        self.residual.push_back(f32::from(s) / 32768.0);
                    }
                }
                // A hole the buffer could not fill: exactly one frame of
                // silence, and no more. No packet-loss concealment — a
                // fabricated waveform would be a lie about what was received.
                GetResult::Interpolate => {
                    for _ in 0..jb.frame_samples {
                        self.residual.push_back(0.0);
                    }
                }
                // Late or shrunk: discard and ask again for the same `now`.
                GetResult::Drop(_) => {}
                GetResult::Empty | GetResult::NoFrame => break,
            }
        }
        self.stats.publish(jb.buf.info());
    }

    /// Has this lane run dry with somebody still talking? The buffer's own
    /// state when it is running; "a timestamped frame arrived recently" when it
    /// is switched off, so `rx_underruns` stays comparable across the switch.
    fn starving(&self, now: i64) -> bool {
        self.jb
            .as_ref()
            .map_or_else(|| now < self.timed_until, LaneJitter::starving)
    }
}

/// Sums several calls' decoded RX audio onto one output bus.
pub struct Mixer {
    lanes: Vec<Lane>,
    next_id: u64,
    clock: MixerClock,
    /// Live jitter-buffer configuration, shared with the control side.
    settings: Arc<RxJitterSettings>,
    /// Cumulative device callbacks that got nothing to play while a
    /// jitter-buffered lane was still mid-talk-spurt (iax-rxjb).
    underruns: Arc<AtomicU64>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            lanes: Vec::new(),
            next_id: 0,
            clock: MixerClock::default(),
            settings: Arc::new(RxJitterSettings::default()),
            underruns: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Mixer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive this mixer's jitter buffers from a caller-supplied clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: MixerClock) -> Self {
        self.clock = clock;
        self
    }

    /// Share a live [`RxJitterSettings`] cell with the control side, so
    /// `set_rx_jitter` reaches this bus without a reconnect.
    #[must_use]
    pub fn with_settings(mut self, settings: Arc<RxJitterSettings>) -> Self {
        self.settings = settings;
        self
    }

    /// The jitter-buffer configuration this bus is running.
    #[must_use]
    pub fn rx_jitter(&self) -> RxJitterConfig {
        self.settings.get()
    }

    /// Cumulative RX underruns on this bus: device callbacks that got no audio
    /// at all while at least one jitter-buffered lane was mid-talk-spurt. A
    /// plain `u64` health counter, credential-free.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    /// Share this bus's cumulative RX-underrun cell so a consumer (the
    /// `Manager`, binding it into a call's snapshot) can read it live.
    #[must_use]
    pub fn underruns_cell(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.underruns)
    }

    /// This lane's live jitter-buffer counters, or `None` if the id is gone.
    #[must_use]
    pub fn jitter_cells(&self, id: MixCallId) -> Option<Arc<RxJitterCells>> {
        self.lanes
            .iter()
            .find(|l| l.id == id)
            .map(|l| Arc::clone(&l.stats))
    }

    /// Register a call's RX channel; returns its slot id for later removal.
    pub fn add_call(&mut self, inbound: Receiver<RxFrame>) -> MixCallId {
        self.add_call_with_stats(inbound, Arc::new(RxJitterCells::default()))
    }

    /// Register a call's RX channel against counters the caller already holds
    /// — the bus-move path, so a `set_output` doesn't reset a live call's
    /// jitter history in the UI.
    pub fn add_call_with_stats(
        &mut self,
        inbound: Receiver<RxFrame>,
        stats: Arc<RxJitterCells>,
    ) -> MixCallId {
        let id = MixCallId(self.next_id);
        self.next_id += 1;
        self.lanes.push(Lane::new(id, inbound, stats));
        id
    }

    /// Register a finite announcement source: a closed-ended `Receiver` whose
    /// `done` flag is flipped when its audio is fully drained. The lane removes
    /// itself from the bus at that point (iax-e30d).
    pub fn add_finite_call(
        &mut self,
        inbound: Receiver<RxFrame>,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> MixCallId {
        let id = MixCallId(self.next_id);
        self.next_id += 1;
        let mut lane = Lane::new(id, inbound, Arc::new(RxJitterCells::default()));
        lane.done = Some(done);
        self.lanes.push(lane);
        id
    }

    /// Remove a call from the bus (monitor-off / hangup).
    pub fn remove_call(&mut self, id: MixCallId) {
        self.lanes.retain(|l| l.id != id);
    }

    /// Detach a call from the bus and return its RX `Receiver` plus its live
    /// jitter counters, so both can be re-registered on another bus's mixer
    /// (the `set_output` / re-route path). The per-call cushion (`residual`
    /// and the jitter buffer itself) is dropped (Q3: accept a ≤20 ms glitch on
    /// a bus change). Returns `None` if the id isn't on this bus.
    pub fn take_call(&mut self, id: MixCallId) -> Option<(Receiver<RxFrame>, Arc<RxJitterCells>)> {
        let pos = self.lanes.iter().position(|l| l.id == id)?;
        let lane = self.lanes.remove(pos);
        Some((lane.inbound, lane.stats))
    }

    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lanes.len()
    }

    /// Fill `out` with the clamped sum of every lane. Returns the number of
    /// samples written (0 when there are no lanes).
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        if self.lanes.is_empty() {
            return 0;
        }
        let now = self.clock.now_ms();
        let cfg = self.settings.get();
        let generation = self.settings.generation();
        for slot in out.iter_mut() {
            *slot = 0.0;
        }
        let mut produced = 0usize;
        let mut voice_lane = false;
        for lane in &mut self.lanes {
            lane.sync_config(cfg, generation);
            loop {
                match lane.inbound.try_recv() {
                    Ok(frame) => lane.accept(frame, now, cfg),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        lane.disconnected = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
            lane.pull(out.len(), now);
            voice_lane |= lane.starving(now);
            let n = out.len().min(lane.residual.len());
            for slot in out.iter_mut().take(n) {
                *slot += lane.residual.pop_front().unwrap_or(0.0);
            }
            produced = produced.max(n);
        }
        // iax-rxjb: the output path pads silence exactly when this returns 0
        // (`fill_output` asks again only while it is still short, and pads the
        // moment a read comes back dry). A dry read while a lane is out of
        // audio mid-talk-spurt is therefore an audible hole, and the one thing
        // the jitter buffer exists to stop. A lane that is merely holding
        // frames back to deepen its cushion is NOT starving and does not count.
        if produced == 0 && voice_lane {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        for slot in out.iter_mut() {
            *slot = slot.clamp(-1.0, 1.0);
        }
        // iax-e30d: a finite lane whose sender closed and whose residual is
        // empty is finished — flip its done flag and drop it from the bus.
        self.lanes.retain(|l| {
            let finished = l.disconnected && l.residual.is_empty();
            if finished && let Some(d) = &l.done {
                d.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            !(finished && l.done.is_some())
        });
        // Report the largest lane fill so the cpal output path treats the
        // unfilled tail as silence, not as a starve-stall (mirrors
        // SpeakerSource which returns min(out, residual)).
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn frame(level: i16, n: usize) -> RxFrame {
        RxFrame::from(vec![level; n])
    }

    #[test]
    fn two_buses_sum_sample_aligned() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        let a = mixer.add_call(a_rx);
        let b = mixer.add_call(b_rx);
        // Each call sends a constant-level frame; the sum is louder than either.
        a_tx.send(frame(8000, 160)).unwrap();
        b_tx.send(frame(8000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(n, 160);
        let one = f32::from(8000i16) / 32768.0;
        assert!((out[0] - 2.0 * one).abs() < 1e-3, "two equal calls sum");
        let _ = (a, b);
    }

    #[test]
    fn overlap_is_hard_clamped_to_unit() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (b_tx, b_rx) = channel();
        mixer.add_call(a_rx);
        mixer.add_call(b_rx);
        // Two near-full-scale calls would sum past 1.0 — must clamp.
        a_tx.send(frame(30000, 160)).unwrap();
        b_tx.send(frame(30000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        mixer.read(&mut out);
        assert!(
            out.iter().all(|&s| (-1.0..=1.0).contains(&s)),
            "clamped to [-1,1]"
        );
    }

    #[test]
    fn one_starved_call_does_not_block_the_other() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let (_b_tx, b_rx) = channel(); // B never sends — starved
        mixer.add_call(a_rx);
        mixer.add_call(b_rx);
        a_tx.send(frame(8000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(
            n, 160,
            "A fills the buffer; B contributes silence, not a stall"
        );
        assert!(out[0].abs() > 0.0, "A's audio is present");
    }

    #[test]
    fn finite_lane_flips_done_and_self_removes_when_drained() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut mixer = Mixer::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        mixer.add_finite_call(rx, Arc::clone(&done));
        tx.send(frame(8000, 160)).unwrap();
        drop(tx); // sender closed → lane is finite and will end
        let mut out = [0.0_f32; 160];
        assert_eq!(mixer.read(&mut out), 160, "plays its one frame");
        let mut out2 = [0.0_f32; 160];
        mixer.read(&mut out2); // disconnected + drained → done + removed
        assert!(done.load(Ordering::Relaxed), "done flips when drained");
        assert_eq!(mixer.call_count(), 0, "finite lane self-removes");
    }

    #[test]
    fn dropping_a_call_removes_it_from_the_sum() {
        let mut mixer = Mixer::new();
        let (a_tx, a_rx) = channel();
        let id = mixer.add_call(a_rx);
        mixer.remove_call(id);
        // The lane (and its Receiver) is gone, so this send has nowhere to land
        // — the failed send is itself evidence the call was removed.
        let _ = a_tx.send(frame(8000, 160));
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(n, 0, "no calls → nothing to mix");
    }

    // ---- iax-rxjb: the jitter-buffered lane ----

    /// One 20 ms frame of 8 kHz PCM at a constant level, stamped on the
    /// sender's clock.
    fn timed(level: i16, ts_ms: i64) -> RxFrame {
        RxFrame::timed(vec![level; 160], ts_ms, 20)
    }

    /// Count the samples in `out` that carry audio (non-zero).
    fn voiced(out: &[f32]) -> usize {
        out.iter().filter(|s| s.abs() > 0.0).count()
    }

    /// A jittery sender — frames arriving 0/20/40 ms apart — plays out
    /// gap-free once the cushion has filled. Without the buffer every long
    /// inter-arrival gap is an audible hole.
    #[test]
    fn jittery_arrivals_play_back_gap_free() {
        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new().with_clock(clock);
        let (tx, rx) = channel();
        mixer.add_call(rx);

        // Arrival pattern in ms for frames stamped 0, 20, 40, …: a steady 20 ms
        // cadence smeared by ±20 ms, which is exactly the shape that stutters
        // today.
        let jitter = [0, 20, 40, 0, 20, 40, 0, 20, 40, 0];
        let mut out = [0.0_f32; 160];
        let mut played = 0usize;
        let mut holes = 0usize;
        // The first 20 slots are the buffer filling its cushion and adapting
        // its depth to the jitter it is measuring; that silence is the buffer
        // working, not a hole. Judge it once it has settled.
        let settled = 20_i64;
        for step in 0..60_i64 {
            let now = step * 20;
            cell.store(now, Ordering::Relaxed);
            // Deliver every frame whose arrival time (ts + jitter) is now.
            for (i, &smear) in jitter.iter().cycle().take(60).enumerate() {
                let ts = i64::try_from(i).unwrap() * 20;
                let arrive = ts + smear;
                if arrive == now {
                    tx.send(timed(8000, ts)).unwrap();
                }
            }
            let n = mixer.read(&mut out);
            let v = voiced(&out[..n]);
            if step >= settled {
                if v == 0 {
                    holes += 1;
                }
                played += v;
            }
        }
        assert_eq!(holes, 0, "no gaps once the buffer has settled");
        assert!(played > 30 * 160, "and the stream keeps playing: {played}");
        assert_eq!(mixer.underruns(), 0, "a working buffer underruns nothing");
    }

    /// A single frame that is 30 ms late is absorbed by the 40 ms of slack —
    /// it plays, and it is not counted late.
    #[test]
    fn a_late_frame_within_target_extra_is_absorbed() {
        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new().with_clock(clock);
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        let mut out = [0.0_f32; 160];

        for step in 0..30_i64 {
            let now = step * 20;
            cell.store(now, Ordering::Relaxed);
            // Frame 10 arrives 30 ms after its slot; everything else is on time.
            let ts = step * 20;
            if step == 10 {
                // held back
            } else if step == 11 {
                tx.send(timed(8000, 10 * 20)).unwrap(); // the late one
                tx.send(timed(8000, ts)).unwrap();
            } else {
                tx.send(timed(8000, ts)).unwrap();
            }
            mixer.read(&mut out);
        }
        assert_eq!(
            stats.frames_late.load(Ordering::Relaxed),
            0,
            "30 ms of lateness fits inside 40 ms of slack"
        );
        assert_eq!(mixer.underruns(), 0, "and costs the bus nothing");
    }

    /// A frame that never arrives costs exactly one 20 ms interpolation — one
    /// frame of silence, no more — and is counted as one lost frame.
    #[test]
    fn one_missing_frame_is_one_interpolation_and_one_loss() {
        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new().with_clock(clock);
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        let mut out = [0.0_f32; 160];
        let mut silent_after_start = 0usize;
        let mut started = false;

        for step in 0..30_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            if step != 15 {
                tx.send(timed(8000, step * 20)).unwrap();
            }
            let n = mixer.read(&mut out);
            let v = voiced(&out[..n]);
            if v > 0 {
                started = true;
            } else if started {
                silent_after_start += 160;
            }
        }
        assert_eq!(
            stats.frames_lost.load(Ordering::Relaxed),
            1,
            "exactly one frame was lost"
        );
        assert_eq!(
            silent_after_start, 160,
            "and it cost exactly one 20 ms frame of silence"
        );
    }

    /// The underrun counter is the "the device got nothing while somebody was
    /// still talking" line: zero while the buffer keeps up, non-zero the
    /// moment the bus asks for audio the stopped source cannot supply.
    #[test]
    fn underruns_count_only_a_dry_bus_mid_talk_spurt() {
        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new().with_clock(clock);
        let (tx, rx) = channel();
        mixer.add_call(rx);
        let mut out = [0.0_f32; 160];

        for step in 0..20_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            tx.send(timed(8000, step * 20)).unwrap();
            mixer.read(&mut out);
        }
        assert_eq!(mixer.underruns(), 0, "keeping up costs nothing");

        // The source stops. The cushion plays out, and once it is empty the
        // bus starts coming back dry while the spurt is still officially open
        // — the output path reads again whenever it is still short, so this is
        // exactly the callback that pads silence.
        for step in 20..30_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            mixer.read(&mut out);
            mixer.read(&mut out);
        }
        assert!(
            mixer.underruns() > 0,
            "a dry bus mid-spurt is an underrun, got {}",
            mixer.underruns()
        );
    }

    /// A lane fed bare PCM (no sender clock) never touches the jitter buffer:
    /// the samples land in the residual in arrival order, on the first read,
    /// exactly as they did before iax-rxjb — and its counters stay zero.
    #[test]
    fn bare_pcm_lanes_bypass_the_jitter_buffer_entirely() {
        let (clock, cell) = MixerClock::fake();
        let mut mixer = Mixer::new().with_clock(clock);
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        cell.store(0, Ordering::Relaxed);

        tx.send(frame(8000, 160)).unwrap();
        let mut out = [0.0_f32; 160];
        let n = mixer.read(&mut out);
        assert_eq!(n, 160, "played on the very first read — no cushion");
        let one = f32::from(8000i16) / 32768.0;
        assert!((out[0] - one).abs() < 1e-3, "and at the level it was sent");
        assert_eq!(stats.jitter_ms.load(Ordering::Relaxed), 0);
        assert_eq!(stats.depth_ms.load(Ordering::Relaxed), 0);
        assert_eq!(stats.frames_lost.load(Ordering::Relaxed), 0);
        assert_eq!(mixer.underruns(), 0, "and an idle bus is not an underrun");
    }

    // ---- iax-rxjb: the buffer is configurable and optional ----

    /// The clamp is a repair, not a refusal: out-of-range bounds come back in
    /// range, and a `max_ms` under `min_ms` is raised to meet it.
    #[test]
    fn config_is_clamped_and_repaired_never_refused() {
        let c = RxJitterConfig {
            enabled: true,
            min_ms: 900,
            max_ms: 900,
        }
        .clamped();
        assert_eq!((c.min_ms, c.max_ms), (500, 500), "both bounds clamped");

        let c = RxJitterConfig {
            enabled: true,
            min_ms: 120,
            max_ms: 60,
        }
        .clamped();
        assert_eq!((c.min_ms, c.max_ms), (120, 120), "max raised to min");

        let d = RxJitterConfig::default();
        assert!(d.enabled);
        assert_eq!((d.min_ms, d.max_ms), (40, 200), "chan_iax2's numbers");

        // And the mapping onto the port is the one documented.
        let j = RxJitterConfig {
            enabled: true,
            min_ms: 80,
            max_ms: 240,
        }
        .to_jitter_config();
        assert_eq!(j.target_extra, 80);
        assert_eq!(j.max_jitterbuf, 240);
        assert_eq!(j.resync_threshold, IAX_JITTER_CONFIG.resync_threshold);
        assert_eq!(j.max_contig_interp, IAX_JITTER_CONFIG.max_contig_interp);
    }

    /// With the buffer switched off a timestamped lane behaves exactly like a
    /// bare-PCM one: the frame plays on the very first read, with no cushion.
    #[test]
    fn disabled_plays_timestamped_frames_straight_through() {
        let (clock, cell) = MixerClock::fake();
        let settings = Arc::new(RxJitterSettings::new(RxJitterConfig {
            enabled: false,
            ..RxJitterConfig::default()
        }));
        let mut mixer = Mixer::new().with_clock(clock).with_settings(settings);
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        cell.store(0, Ordering::Relaxed);

        tx.send(timed(8000, 0)).unwrap();
        let mut out = [0.0_f32; 160];
        assert_eq!(mixer.read(&mut out), 160, "no cushion when disabled");
        assert_eq!(
            stats.depth_ms.load(Ordering::Relaxed),
            0,
            "no buffer, no depth"
        );
        assert_eq!(stats.frames_lost.load(Ordering::Relaxed), 0);
    }

    /// Switching the buffer OFF mid-call drains what it holds — every buffered
    /// frame comes out exactly once, none twice — and then keeps playing.
    #[test]
    fn disabling_mid_call_drains_without_replaying_anything() {
        let (clock, cell) = MixerClock::fake();
        let settings = Arc::new(RxJitterSettings::default());
        let mut mixer = Mixer::new()
            .with_clock(clock)
            .with_settings(Arc::clone(&settings));
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        let mut out = [0.0_f32; 160];

        // Each frame carries its own index as its level, so a replay is visible.
        for step in 0..8_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            let level = i16::try_from(step + 1).unwrap() * 1000;
            tx.send(RxFrame::timed(vec![level; 160], step * 20, 20))
                .unwrap();
            mixer.read(&mut out);
        }
        // Everything still queued in the buffer, in order.
        settings.set(RxJitterConfig {
            enabled: false,
            ..RxJitterConfig::default()
        });
        let mut played: Vec<i16> = Vec::new();
        for step in 8..20_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            let n = mixer.read(&mut out);
            for &s in &out[..n] {
                #[allow(clippy::cast_possible_truncation)]
                let level = (s * 32768.0).round() as i16;
                if played.last() != Some(&level) {
                    played.push(level);
                }
            }
        }
        let mut seen = played.clone();
        seen.retain(|&l| l != 0);
        seen.dedup();
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(seen, sorted, "levels came out in order, none replayed");
        assert_eq!(
            stats.frames_lost.load(Ordering::Relaxed),
            0,
            "and the counters zero out with the buffer"
        );
    }

    /// Switching the buffer back ON mid-call starts it empty — the new buffer
    /// builds its own cushion rather than inheriting a stale one.
    #[test]
    fn enabling_mid_call_starts_from_an_empty_buffer() {
        let (clock, cell) = MixerClock::fake();
        let settings = Arc::new(RxJitterSettings::new(RxJitterConfig {
            enabled: false,
            ..RxJitterConfig::default()
        }));
        let mut mixer = Mixer::new()
            .with_clock(clock)
            .with_settings(Arc::clone(&settings));
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        let mut out = [0.0_f32; 160];

        for step in 0..5_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            tx.send(timed(8000, step * 20)).unwrap();
            mixer.read(&mut out);
        }
        settings.set(RxJitterConfig::default());
        cell.store(5 * 20, Ordering::Relaxed);
        tx.send(timed(8000, 5 * 20)).unwrap();
        assert_eq!(
            mixer.read(&mut out),
            0,
            "a freshly enabled buffer is empty and fills before it plays"
        );
        assert_eq!(
            stats.frames_lost.load(Ordering::Relaxed),
            0,
            "and it starts without inherited losses"
        );
    }

    /// A `min_ms` of 120 holds a deeper cushion than the 40 ms default: once
    /// settled, the buffer reports at least 120 ms of depth.
    #[test]
    fn a_larger_min_ms_holds_a_deeper_cushion() {
        let (clock, cell) = MixerClock::fake();
        let settings = Arc::new(RxJitterSettings::new(RxJitterConfig {
            enabled: true,
            min_ms: 120,
            max_ms: 200,
        }));
        let mut mixer = Mixer::new().with_clock(clock).with_settings(settings);
        let (tx, rx) = channel();
        let id = mixer.add_call(rx);
        let stats = mixer.jitter_cells(id).expect("lane present");
        let mut out = [0.0_f32; 160];
        for step in 0..40_i64 {
            cell.store(step * 20, Ordering::Relaxed);
            tx.send(timed(8000, step * 20)).unwrap();
            mixer.read(&mut out);
        }
        assert!(
            stats.depth_ms.load(Ordering::Relaxed) >= 120,
            "depth settled at or above the configured floor, got {}",
            stats.depth_ms.load(Ordering::Relaxed)
        );
    }

    /// `rx_underruns` keeps counting with the buffer switched off — that is
    /// the whole point of being able to switch it off and compare.
    #[test]
    fn underruns_still_count_with_the_buffer_disabled() {
        let (clock, cell) = MixerClock::fake();
        let settings = Arc::new(RxJitterSettings::new(RxJitterConfig {
            enabled: false,
            ..RxJitterConfig::default()
        }));
        let mut mixer = Mixer::new().with_clock(clock).with_settings(settings);
        let (tx, rx) = channel();
        mixer.add_call(rx);
        let mut out = [0.0_f32; 160];

        cell.store(0, Ordering::Relaxed);
        tx.send(timed(8000, 0)).unwrap();
        assert_eq!(mixer.read(&mut out), 160, "the frame plays");
        // Still mid-spurt (a timestamped frame arrived 20 ms ago), nothing left.
        cell.store(20, Ordering::Relaxed);
        assert_eq!(mixer.read(&mut out), 0, "and the source is dry");
        assert_eq!(mixer.underruns(), 1, "which is exactly an underrun");
    }

    /// Moving a call between buses keeps its counters: `set_output` must not
    /// zero a live call's jitter history.
    #[test]
    fn taking_a_call_carries_its_counters_to_the_next_bus() {
        let mut a = Mixer::new();
        let (_tx, rx) = channel::<RxFrame>();
        let id = a.add_call(rx);
        let cells = a.jitter_cells(id).expect("lane present");
        cells.frames_lost.store(7, Ordering::Relaxed);

        let (moved_rx, moved_stats) = a.take_call(id).expect("lane taken");
        let mut b = Mixer::new();
        let new_id = b.add_call_with_stats(moved_rx, moved_stats);
        assert_eq!(
            b.jitter_cells(new_id)
                .expect("lane present")
                .frames_lost
                .load(Ordering::Relaxed),
            7,
            "the counters travel with the call"
        );
    }
}
