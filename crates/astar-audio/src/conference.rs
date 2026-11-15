// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Mix-minus conference bridge: every member hears everyone else (iax-647d).
//!
//! Phase 2 over the iax-42e9 audio layer. Where [`crate::Mixer`] sums N call RX
//! channels onto ONE output bus (the local speaker), a [`Conference`] is the
//! routing piece that was missing: it feeds every member's RX into every OTHER
//! member's TX so connected users hear each other, not just the local radio.
//!
//! Topology (decided in the design doc):
//! - Each member's TX = sum of all OTHER members' RX (+ optional local mic),
//!   clamped to [-1, 1]. That is **mix-minus**: a member never hears itself.
//! - `mix_minus = false` produces a full mix (members hear themselves too) —
//!   useful for parrot/loopback behavior.
//! - Doubletalk is full summing (simultaneous talkers mix and clamp), exactly
//!   like [`crate::Mixer`]; no priority arbitration.
//! - `local_mic` / `local_out` (the "include local radio" option) add the local
//!   mic as an extra conference source on every member's TX and feed the local
//!   speaker the sum of all members. Both default to absent (pure bridge).
//!
//! A starved member (no frame ready this tick) contributes silence and never
//! blocks the others. A member whose frames carry the sender's clock (IAX2
//! voice) plays them out of its own jitter cushion ([`crate::dejitter`],
//! iax-confjb): held 40 ms and played by timestamp, so a frame a few ms late
//! still makes its tick. Bare PCM (the local mic, M17, `EchoLink`, tests) goes
//! through the per-source `residual` as before.
//!
//! The mixing runs on a dedicated 20 ms-clocked thread owned by the
//! [`Conference`]; [`Conference::start`] launches it and `Drop` joins it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::dejitter::Dejitter;
use crate::dtmf::DtmfDetector;
use crate::router::frame_samples;

/// 20 ms per conference tick (one 160-sample PCM frame @ 8 kHz; scales with
/// the station rate — 320 samples @ 16 kHz, iax-4348).
const TICK: Duration = Duration::from_millis(20);

/// Ticks a member's TX stays open after the last tick on which anyone it
/// hears delivered audio (100 ms): enough to ride out a late frame without
/// dropping the over, short enough that the far node unkeys promptly.
const TX_HANG_TICKS: u32 = 5;

/// Opaque per-member slot id within a [`Conference`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemberId(u64);

impl MemberId {
    /// Construct a `MemberId` from a raw slot value. For tests and FFI
    /// round-trips; the conference assigns real ids itself.
    #[must_use]
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }

    /// The raw slot value.
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

/// One audio source feeding a conference tick: a PCM `rx` plus the jitter
/// `residual` that normalizes into. Shared by conference members and the optional
/// local-mic source so both normalize identically (the shared DSP helper).
struct Source<T: AsPcm> {
    rx: Receiver<T>,
    residual: VecDeque<f32>,
    /// Jitter cushion for frames that carry the sender's clock (iax-confjb).
    cushion: Dejitter,
}

/// What a conference source can pull PCM out of. Members carry
/// [`crate::RxFrame`] (the output bus's frame, sender clock and all — the
/// conference bridges rather than plays, so it uses only the samples); the
/// local mic lane sends bare `Vec<i16>`.
trait AsPcm {
    fn pcm(&self) -> &[i16];
    /// The sender's clock, when the frame carries one.
    fn clock(&self) -> Option<crate::RxClock> {
        None
    }
    fn into_pcm(self) -> Vec<i16>;
}

impl AsPcm for Vec<i16> {
    fn pcm(&self) -> &[i16] {
        self
    }
    fn into_pcm(self) -> Vec<i16> {
        self
    }
}

impl AsPcm for crate::RxFrame {
    fn pcm(&self) -> &[i16] {
        &self.pcm
    }
    fn clock(&self) -> Option<crate::RxClock> {
        self.clock
    }
    fn into_pcm(self) -> Vec<i16> {
        self.pcm
    }
}

impl<T: AsPcm> Source<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self {
            rx,
            residual: VecDeque::new(),
            cushion: Dejitter::default(),
        }
    }

    fn push_pcm(&mut self, pcm: &[i16]) {
        for &s in pcm {
            self.residual.push_back(f32::from(s) / 32768.0);
        }
    }

    /// Drain whatever PCM frames are waiting on `rx` into `residual` (normalized
    /// to f32 in [-1, 1)), then pop up to `n` samples into `buf` (zero-padding
    /// the tail when starved). This is the normalize + jitter-residual logic
    /// shared with [`crate::Mixer`]; the conference's only new behavior is how
    /// the resulting per-source buffers are summed (mix-minus). Returns the
    /// number of real (non-padding) samples produced.
    ///
    /// With `cushioned`, a frame that carries the sender's clock and is one
    /// tick long goes through the jitter cushion instead (iax-confjb), which
    /// hands back at most one frame per tick, on the sender's schedule.
    fn decode_into(&mut self, buf: &mut [f32], cushioned: bool) -> usize {
        // Drain whatever is waiting; Empty or Disconnected ends the loop and the
        // source contributes silence for the rest of the tick.
        while let Ok(frame) = self.rx.try_recv() {
            match frame.clock() {
                Some(clock) if cushioned && frame.pcm().len() == buf.len() => {
                    self.cushion.put(frame.into_pcm(), clock.ts_ms, clock.ms);
                }
                _ => self.push_pcm(frame.pcm()),
            }
        }
        if let Some(pcm) = self.cushion.tick() {
            self.push_pcm(&pcm);
        }
        let real = buf.len().min(self.residual.len());
        for slot in buf.iter_mut().take(real) {
            *slot = self.residual.pop_front().unwrap_or(0.0);
        }
        for slot in buf.iter_mut().skip(real) {
            *slot = 0.0;
        }
        real
    }
}

/// One conference member: its RX source (decoded into the mix) plus the TX
/// `Sender` its personal mix-minus output is encoded onto.
struct Member {
    id: MemberId,
    src: Source<crate::RxFrame>,
    tx: Sender<Vec<i16>>,
    /// Per-member private announcement stream (iax-c4ea): a queue of 160-sample
    /// PCM frames played to THIS member's leg only (e.g. the node-id join
    /// greeting). While non-empty the member's TX carries one announcement frame
    /// per tick INSTEAD of the conference mix, so the greeting reaches one user
    /// without touching the bus. Drains front-to-back; empty ⇒ normal mix.
    announce: VecDeque<Vec<i16>>,
    /// Live PTT-key gate for parrot mode (iax-feab): `true` while this
    /// member is keyed. Ignored outside parrot mode. Defaults to a fresh
    /// unkeyed flag for members added via [`Conference::add_member`].
    key: Arc<AtomicBool>,
    /// Parrot record/replay/report state machine for this member (iax-feab);
    /// inert outside parrot mode.
    parrot: ParrotState,
    /// Optional nudge for the member's call run-loop (iax-feab), set via
    /// [`Conference::set_member_wake`]. Called every tick right after `tx.send`
    /// so a call whose peer has gone quiet — e.g. a parrot's replay, which by
    /// definition only starts once the RECORD side has already detected
    /// silence — notices new TX audio promptly instead of waiting on its own
    /// periodic poll-timeout fallback (tens of ms, worst case ~100 ms), which
    /// can otherwise race a fast-following hangup and silently drop the tail
    /// of the replay. Mirrors `TxFrames::send`'s explicit wake on the
    /// device-free raw-dial path. `None` (e.g. bare test doubles) is a
    /// harmless no-op.
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Relay eligibility (iax-42ce): whether this member's RX joins the relay
    /// sum sent to other members' TX. `false` for a `LocalMonitor` link (heard
    /// on the local speaker bus only). Default `true` (pre-link behavior).
    contributes: bool,
    /// Whether the conference mix is sent to this member's TX at all. `false`
    /// for Monitor/LocalMonitor links — we never transmit to them, and a
    /// non-receiving member is sent NOTHING rather than silence (radio gap
    /// semantics, as in parrot idle). Default `true`.
    receives: bool,
    /// Per-member in-band DTMF detector (iax-8ca0): fed this member's decoded
    /// RX block each tick (when [`ConferenceConfig::dtmf_squelch`] is on).
    /// While it reports a tone (+ tail), the member is excluded from the
    /// relay sum; registered digits land in `Shared::dtmf_digits`.
    dtmf: DtmfDetector,
    /// Ticks this member's TX stays open with nobody it hears talking
    /// ([`TX_HANG_TICKS`] after the last audio). At 0 it is sent nothing.
    tx_hang: u32,
}

impl Member {
    /// Nudge this member's run-loop after enqueueing new TX audio, if a wake
    /// callback is set. See the `wake` field doc for why this matters.
    fn wake_if_set(&self) {
        if let Some(w) = &self.wake {
            w();
        }
    }
}

/// Per-member parrot state machine (iax-feab): VOX/PTT-gated record, then
/// replay the take back on the same leg privately, then hold the
/// [`crate::SignalReport`] until the replay has fully drained.
#[derive(Default)]
struct ParrotState {
    /// Set the first time this member's key transitions; once set, PTT (not
    /// VOX level) decides voice activity for the rest of this member's life.
    ptt_latched: bool,
    /// The key value observed on the previous tick (edge detection for
    /// `ptt_latched`).
    prev_key: bool,
    /// Consecutive sub-threshold ticks since voice activity ended (VOX mode);
    /// reaching `silence_gap_ticks` triggers replay.
    quiet_ticks: u32,
    /// How many of those quiet ticks put a frame into the take (VOX mode):
    /// the trailing silence dropped when the take ends.
    quiet_recorded: usize,
    /// Countdown to replay start once triggered (`playback_delay_ticks`),
    /// `None` when no replay is pending.
    replay_in: Option<u32>,
    /// Frames recorded so far in the current take.
    recording: Vec<Vec<i16>>,
    /// Analysis of the take, held until the replay has fully drained.
    pending_report: Option<crate::SignalReport>,
}

impl Member {
    /// Advance this member's parrot state machine by one tick (iax-feab).
    /// `frame` is this tick's already-encoded i16 PCM (the member's own
    /// audio), or `None` when none of it arrived this tick; `level_db` is its dBFS peak (the VOX gate input). Returns the
    /// completed [`crate::SignalReport`] on the first tick the replay has
    /// fully drained from `self.announce`, `None` otherwise.
    ///
    /// `voice_now` is gated on `pending_report.is_none()` so a caller talking
    /// over their own replay doesn't start a second take before the report
    /// goes out — the Manager hangs the leg up right after.
    ///
    /// `recording.len()` never approaches `u32::MAX` (`max_record_ticks`
    /// bounds it, and the 10 s default cap is 500) — same rationale as the
    /// sample-count casts elsewhere in this crate.
    #[allow(clippy::cast_possible_truncation)]
    fn parrot_tick(
        &mut self,
        t: &ParrotTuning,
        keyed: bool,
        level_db: f32,
        frame: Option<Vec<i16>>,
        sample_rate: u32,
    ) -> Option<crate::SignalReport> {
        let p = &mut self.parrot;
        if keyed != p.prev_key {
            p.ptt_latched = true;
        }
        p.prev_key = keyed;

        let voice_now = if p.ptt_latched {
            keyed
        } else {
            level_db > t.vox_threshold_db
        };

        // The take is what the caller sent: a tick its frame missed (it
        // arrived just after the tick and plays one tick later) adds nothing,
        // rather than a hole of silence.
        if voice_now && p.pending_report.is_none() && p.replay_in.is_none() {
            p.quiet_ticks = 0;
            p.quiet_recorded = 0;
            p.recording.extend(frame);
            if p.recording.len() as u32 >= t.max_record_ticks {
                p.replay_in = Some(t.playback_delay_ticks);
            }
        } else if !p.recording.is_empty() && p.replay_in.is_none() && p.pending_report.is_none() {
            if p.ptt_latched {
                p.replay_in = Some(t.playback_delay_ticks);
            } else {
                // A quiet frame inside a VOX take is a pause between words:
                // keep it in place. If the quiet lasts the whole silence gap,
                // the take is over, and the trailing silence is dropped.
                if let Some(frame) = frame {
                    p.recording.push(frame);
                    p.quiet_recorded += 1;
                }
                p.quiet_ticks += 1;
                if p.quiet_ticks >= t.silence_gap_ticks {
                    let spoken = p.recording.len() - p.quiet_recorded;
                    p.recording.truncate(spoken);
                    p.quiet_recorded = 0;
                    p.replay_in = Some(t.playback_delay_ticks);
                }
            }
        }

        if let Some(left) = p.replay_in {
            if left == 0 {
                let flat: Vec<i16> = p.recording.iter().flatten().copied().collect();
                p.pending_report = Some(crate::analyze_signal(&flat, sample_rate));
                self.announce.extend(p.recording.drain(..));
                p.replay_in = None;
                p.quiet_ticks = 0;
            } else {
                p.replay_in = Some(left - 1);
            }
        }

        if p.pending_report.is_some() && self.announce.is_empty() {
            return p.pending_report.take();
        }
        None
    }
}

/// Tuning knobs for [`ConferenceConfig::parrot`] mode (iax-feab): each
/// member's leg privately records what it sends and replays it back on its
/// own leg (VOX- or PTT-gated), then a [`crate::SignalReport`] surfaces via
/// [`Conference::take_parrot_reports`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParrotTuning {
    /// Ticks (20 ms each) the state machine waits after deciding to replay
    /// before it actually starts (lets a trailing PTT unkey settle).
    pub playback_delay_ticks: u32,
    /// Ticks of sub-threshold silence (VOX mode only) that end a take and
    /// trigger replay.
    pub silence_gap_ticks: u32,
    /// VOX level threshold in dBFS; ignored once the member's PTT key has
    /// latched (PTT wins over VOX for that member from then on).
    pub vox_threshold_db: f32,
    /// Hard cap on take length, in 20 ms ticks; forces replay even mid-talk.
    /// Default 500 ticks = 10 s.
    pub max_record_ticks: u32,
}

impl Default for ParrotTuning {
    fn default() -> Self {
        Self {
            playback_delay_ticks: 150,
            silence_gap_ticks: 40,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        }
    }
}

/// Configuration for a [`Conference`]. `local_mic` / `local_out` are the
/// "include local radio" option: present ⇒ the local mic is an extra source on
/// every member's TX and the local speaker bus gets the sum of all members.
pub struct ConferenceConfig {
    /// `true` (default) ⇒ each member hears everyone but itself (mix-minus);
    /// `false` ⇒ full mix (members hear themselves too).
    pub mix_minus: bool,
    /// Optional local-mic source folded into every member's TX (include local
    /// radio). `None` ⇒ pure bridge among remote members.
    pub local_mic: Option<Receiver<Vec<i16>>>,
    /// Optional local-speaker sink that receives the sum of all members
    /// (include local radio). `None` ⇒ no local monitor.
    pub local_out: Option<Sender<Vec<i16>>>,
    /// Station bus sample rate; sizes the 20 ms mix buffers (iax-4348).
    pub sample_rate: u32,
    /// `Some` ⇒ parrot mode (iax-feab): every member's TX phase replaces the
    /// mix-minus mix with that member's own private record/replay/report
    /// cycle (see [`ParrotTuning`]). `None` (default) ⇒ ordinary mix-minus
    /// bridge, byte-identical to pre-parrot behavior.
    pub parrot: Option<ParrotTuning>,
    /// In-band DTMF handling (iax-8ca0): when `true`, each member's decoded
    /// RX runs through a per-member [`DtmfDetector`] every tick; a member
    /// sounding a touch-tone is EXCLUDED from the relay sum (command digits
    /// are for THIS node, never rebroadcast to the other legs) for the tone
    /// duration plus a short tail, and each registered digit is queued for
    /// [`Conference::drain_dtmf_digits`]. The local speaker bus (`local_out`)
    /// stays unfiltered either way.
    ///
    /// Default `true` — squelch-on is the desired node behavior, and the
    /// detector's validity checks (level floor, dominance, twist,
    /// tone-vs-total energy) mean non-DTMF audio (speech, constant test
    /// levels, noise) never trips it, so pre-existing conference behavior is
    /// unchanged. Set `false` to relay member audio verbatim (no detection).
    pub dtmf_squelch: bool,
}

impl Default for ConferenceConfig {
    fn default() -> Self {
        Self {
            mix_minus: true,
            local_mic: None,
            local_out: None,
            sample_rate: 8_000,
            parrot: None,
            dtmf_squelch: true,
        }
    }
}

/// The mutable mixing state shared between the public handle and the mixing
/// thread (mutex-guarded so `add_member`/`remove_member` mirror
/// `Mixer::add_call`/`remove_call`).
struct Shared {
    members: Vec<Member>,
    next_id: u64,
    mix_minus: bool,
    local_mic: Option<Source<Vec<i16>>>,
    local_out: Option<Sender<Vec<i16>>>,
    /// Samples per 20 ms tick at the configured station rate (iax-4348).
    frame_samples: usize,
    /// `Some` ⇒ parrot mode is active for every member on this tick
    /// (iax-feab); see [`ConferenceConfig::parrot`].
    parrot: Option<ParrotTuning>,
    /// Completed parrot [`crate::SignalReport`]s awaiting
    /// [`Conference::take_parrot_reports`].
    parrot_reports: Vec<(MemberId, crate::SignalReport)>,
    /// Whether per-member in-band DTMF detection + relay squelch runs each
    /// tick (iax-8ca0); see [`ConferenceConfig::dtmf_squelch`].
    dtmf_squelch: bool,
    /// Registered in-band DTMF digits awaiting
    /// [`Conference::drain_dtmf_digits`] (iax-8ca0), mirroring the
    /// `parrot_reports` drain pattern.
    dtmf_digits: Vec<(MemberId, char)>,
}

impl Shared {
    /// Run one 20 ms mix-minus tick over the current membership. Factored out so
    /// unit tests can drive ticks deterministically without the wall-clock
    /// thread.
    #[allow(clippy::too_many_lines)]
    fn tick(&mut self) {
        if self.members.is_empty() && self.local_mic.is_none() {
            return;
        }

        let n = self.frame_samples;

        // 1. Decode each member's RX frame into a per-member f32 buffer, and
        //    note which members delivered audio at all this tick.
        let mut bufs: Vec<Vec<f32>> = vec![vec![0.0; n]; self.members.len()];
        let mut delivered = vec![false; self.members.len()];
        // The parrot records what the caller sent, as it arrives (90efb10):
        // it has no listener whose timing a cushion would protect, and its
        // PTT gate would cut off the frames a cushion still held at the unkey.
        let cushioned = self.parrot.is_none();
        for ((member, buf), got) in self
            .members
            .iter_mut()
            .zip(bufs.iter_mut())
            .zip(delivered.iter_mut())
        {
            *got = member.src.decode_into(buf, cushioned) > 0;
        }

        // In-band DTMF (iax-8ca0): run each member's detector over this
        // tick's decoded block. A registered digit queues for
        // [`Conference::drain_dtmf_digits`]; a member currently sounding a
        // tone (+ a short tail) is EXCLUDED from the relay sum below —
        // command digits are for THIS node, never rebroadcast — while
        // `total_all` (the local speaker bus) stays unfiltered.
        let mut squelched = vec![false; self.members.len()];
        if self.dtmf_squelch {
            for (i, (member, buf)) in self.members.iter_mut().zip(bufs.iter()).enumerate() {
                if let Some(digit) = member.dtmf.feed(buf) {
                    self.dtmf_digits.push((member.id, digit));
                }
                squelched[i] = member.dtmf.squelching();
            }
        }

        // Parrot mode (iax-feab) replaces the rest of the tick: each member's
        // TX carries its own private announce queue (either the normal
        // announce-greeting slot or the parrot replay) instead of the
        // mix-minus mix; there is no local-mic/local-out path in parrot mode.
        if let Some(tuning) = self.parrot {
            for (i, member) in self.members.iter_mut().enumerate() {
                // An idle parrot leg is unkeyed — it sends NOTHING (no
                // silence filler; iax-a54e). Radio semantics: callers detect
                // end-of-transmission by the frame GAP, so continuous silence
                // frames would make frame-arrival-gated clients record
                // forever, and they waste bandwidth. Only replay/announce
                // frames go out; the wake nudge fires only when audio was
                // actually sent.
                if let Some(ann) = member.announce.pop_front() {
                    let _ = member.tx.send(ann);
                    member.wake_if_set();
                }
                let keyed = member.key.load(Ordering::Relaxed);
                let frame = delivered[i]
                    .then(|| bufs[i].iter().map(|&s| encode_sample_rounded(s)).collect());
                let level_db = frame_dbfs(&bufs[i]);
                if let Some(report) = member.parrot_tick(
                    &tuning,
                    keyed,
                    level_db,
                    frame,
                    sample_rate_of(self.frame_samples),
                ) {
                    self.parrot_reports.push((member.id, report));
                }
            }
            return;
        }

        // 2. Decode the optional local mic into its own source buffer.
        let mut mic_buf = vec![0.0_f32; n];
        let mut mic_delivered = false;
        let have_mic = if let Some(mic) = self.local_mic.as_mut() {
            mic_delivered = mic.decode_into(&mut mic_buf, false) > 0;
            true
        } else {
            false
        };

        // The relay sum (iax-42ce): only relay-eligible members join the mix
        // other members are sent. `total_all` still sums EVERY member for the
        // local speaker bus, so a LocalMonitor member is heard locally but
        // never relayed onward. With all-default flags the two sums are
        // identical and this is byte-for-byte the pre-link behavior.
        let mut total_relay = vec![0.0_f32; n];
        let mut total_all = vec![0.0_f32; n];
        for (i, (member, buf)) in self.members.iter().zip(bufs.iter()).enumerate() {
            for (j, &s) in buf.iter().enumerate() {
                total_all[j] += s;
                if member.contributes && !squelched[i] {
                    total_relay[j] += s;
                }
            }
        }

        // 3. For each member i: sum(relay-eligible members) + mic, minus self
        //    if mix_minus (self was only in the sum if it contributes).
        //    A member with a queued private announcement (iax-c4ea) gets that
        //    announcement frame on its TX this tick INSTEAD of the mix, so the
        //    node-id join greeting plays to that one user only — announcements
        //    are per-leg private audio, delivered even to non-receiving
        //    members. A non-receiving member (Monitor/LocalMonitor link) is
        //    otherwise sent NOTHING — not silence — mirroring the parrot idle
        //    radio-gap semantics above.
        //
        //    A member is sent the mix only while someone it hears is talking:
        //    a relay-eligible member other than itself (or itself, in a full
        //    mix) delivered audio this tick, or did within the hangover. Then
        //    it is sent NOTHING, not silence. An app_rpt node treats a link's
        //    voice frames as its key once the NEWKEY handshake is done, so
        //    continuous silence held every linked node's transmitter up for
        //    good (nodebench finding 6). app_rpt sends link voice only while
        //    keyed, and so does this.
        let talking: Vec<bool> = self
            .members
            .iter()
            .zip(delivered.iter())
            .zip(squelched.iter())
            .map(|((m, &d), &q)| d && m.contributes && !q)
            .collect();
        let talkers = talking.iter().filter(|&&t| t).count();
        let mut frame = vec![0_i16; n];
        for (i, member) in self.members.iter_mut().enumerate() {
            let others_talking = if self.mix_minus && talking[i] {
                talkers > 1
            } else {
                talkers > 0
            };
            if others_talking || mic_delivered {
                member.tx_hang = TX_HANG_TICKS;
            }
            let tx_open = member.tx_hang > 0;
            member.tx_hang = member.tx_hang.saturating_sub(1);
            if let Some(ann) = member.announce.pop_front() {
                let _ = member.tx.send(ann);
                member.wake_if_set();
                continue;
            }
            if !member.receives || !tx_open {
                continue;
            }
            for (j, slot) in frame.iter_mut().enumerate() {
                let mut s = total_relay[j] + if have_mic { mic_buf[j] } else { 0.0 };
                // Subtract self only if self actually joined the relay sum —
                // a squelched member's audio was never added (iax-8ca0), so
                // subtracting it would inject inverted tone.
                if self.mix_minus && member.contributes && !squelched[i] {
                    s -= bufs[i][j];
                }
                *slot = encode_sample(s);
            }
            let _ = member.tx.send(frame.clone());
            member.wake_if_set();
        }

        // 4. Local speaker bus gets the sum of ALL members — LocalMonitor
        //    included (no minus; the local mic is not in the speaker path).
        if let Some(out) = self.local_out.as_ref() {
            for (slot, &s) in frame.iter_mut().zip(total_all.iter()) {
                *slot = encode_sample(s);
            }
            let _ = out.send(frame.clone());
        }
    }
}

/// Clamp a summed f32 sample to [-1, 1] and quantize it to i16 PCM (shared with
/// [`crate::Mixer`]'s clamp contract; codec transcode is at the network edge).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn encode_sample(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Peak dBFS of a decoded f32 tick buffer (the parrot VOX gate input;
/// iax-feab). Thin wrapper over the shared [`crate::peak`] /
/// [`crate::peak_to_dbfs`] meter helpers.
fn frame_dbfs(buf: &[f32]) -> f32 {
    crate::peak_to_dbfs(crate::peak(buf))
}

/// Quantize a normalized f32 sample to i16 PCM with rounding rather than
/// [`encode_sample`]'s truncation (iax-feab). A parrot recording is captured
/// once and then both analyzed ([`crate::analyze_signal`]) and replayed
/// verbatim, so it needs the 1-LSB-tighter round-trip fidelity round-to-
/// nearest gives; the live mix path keeps `encode_sample` exactly as-is so
/// mix-minus output stays byte-identical to pre-parrot behavior.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn encode_sample_rounded(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Sample rate implied by a 20 ms tick's sample count (`frame_samples * 50`;
/// iax-feab), for feeding [`crate::analyze_signal`] a parrot take.
#[allow(clippy::cast_possible_truncation)]
fn sample_rate_of(frame_samples: usize) -> u32 {
    (frame_samples * 50) as u32
}

/// The longest a member's jitter cushion can still hold audio after the
/// member's last frame arrived (iax-confjb): 140 ms. Whatever keys other
/// legs on a member's behalf may wait this long for its over to play out.
pub const MAX_CUSHION_DRAIN: Duration = crate::dejitter::MAX_DRAIN;

/// How far behind its schedule the mixing thread may fall and still catch up
/// (100 ms, five ticks). Past this it resynchronises on the present.
const MAX_CATCH_UP: Duration = Duration::from_millis(100);

/// When the tick after one due at `due` should run, given that it is `now`.
///
/// The schedule advances one [`TICK`] per tick whatever time the tick
/// actually ran, so a tick that woke late is followed by one that runs at
/// once: the bridge keeps 50 ticks a second, and no member's input falls a
/// frame behind (iax-confjb, S6 under host load). Only a stall longer than
/// [`MAX_CATCH_UP`] gives up the ticks it missed, rather than bursting them.
fn next_tick_after(due: Instant, now: Instant) -> Instant {
    if now > due + MAX_CATCH_UP {
        now + TICK
    } else {
        due + TICK
    }
}

/// A mix-minus conference bridge owning all members and a dedicated 20 ms
/// mixing thread. Add members as calls join ([`Conference::add_member`]) and
/// remove them as calls leave ([`Conference::remove_member`]); the thread feeds
/// every member's RX into every other member's TX each tick.
pub struct Conference {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Conference {
    /// Build a conference from `config` WITHOUT starting the mixing thread.
    /// Used by tests that drive ticks manually; production code calls
    /// [`Conference::start`].
    #[must_use]
    pub fn new(config: ConferenceConfig) -> Self {
        let shared = Shared {
            members: Vec::new(),
            next_id: 0,
            mix_minus: config.mix_minus,
            local_mic: config.local_mic.map(Source::new),
            local_out: config.local_out,
            frame_samples: frame_samples(config.sample_rate),
            parrot: config.parrot,
            parrot_reports: Vec::new(),
            dtmf_squelch: config.dtmf_squelch,
            dtmf_digits: Vec::new(),
        };
        Self {
            shared: Arc::new(Mutex::new(shared)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Build a conference from `config` and start its dedicated 20 ms mixing
    /// thread. `Drop` stops and joins the thread.
    #[must_use]
    pub fn start(config: ConferenceConfig) -> Self {
        let mut conf = Self::new(config);
        conf.spawn();
        conf
    }

    /// Launch the mixing thread (idempotent-ish: only call once).
    fn spawn(&mut self) {
        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop);
        let handle = std::thread::Builder::new()
            .name("iax-conference".into())
            .spawn(move || {
                let mut next = Instant::now() + TICK;
                while !stop.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if now < next {
                        std::thread::sleep(next - now);
                    }
                    next = next_tick_after(next, Instant::now());
                    if let Ok(mut s) = shared.lock() {
                        s.tick();
                    }
                }
            })
            .expect("spawn conference mixing thread");
        self.handle = Some(handle);
    }

    /// Register a member: hand the conference the call's RX `Receiver` (decoded
    /// into the mix) and TX `Sender` (its personal mix-minus output). Returns
    /// the member's slot id for later [`Conference::remove_member`]. Mutex-guarded,
    /// mirroring [`crate::Mixer::add_call`].
    #[must_use]
    pub fn add_member(&self, rx: Receiver<crate::RxFrame>, tx: Sender<Vec<i16>>) -> MemberId {
        self.add_member_keyed(rx, tx, Arc::new(AtomicBool::new(false)))
    }

    /// Register a member with an external PTT-key gate (parrot PTT mode,
    /// iax-feab): `key` reflects this member's live PTT state, sampled once
    /// per tick by the parrot state machine — once it edges, PTT (not VOX
    /// level) decides voice activity for this member from then on. Ignored
    /// outside parrot mode. Returns the member's slot id for later
    /// [`Conference::remove_member`]. Mutex-guarded, mirroring
    /// [`Conference::add_member`].
    #[must_use]
    pub fn add_member_keyed(
        &self,
        rx: Receiver<crate::RxFrame>,
        tx: Sender<Vec<i16>>,
        key: Arc<AtomicBool>,
    ) -> MemberId {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        let id = MemberId(s.next_id);
        s.next_id += 1;
        let member_rate = sample_rate_of(s.frame_samples);
        s.members.push(Member {
            id,
            src: Source::new(rx),
            tx,
            announce: VecDeque::new(),
            key,
            parrot: ParrotState::default(),
            wake: None,
            contributes: true,
            receives: true,
            dtmf: DtmfDetector::new(member_rate),
            tx_hang: 0,
        });
        id
    }

    /// Set a member's relay flags (iax-42ce): `contributes` = its RX joins the
    /// relay sum other members are sent; `receives` = the conference mix is
    /// sent to its TX at all. Both default `true` (the pre-link full-relay
    /// behavior). The link-mode mapping is the CALLER'S policy — the `Manager`
    /// maps `LinkMode::relays_onward()` / `is_transmit_capable()` here:
    /// `Transceive` = (true, true); `Monitor` = (true, false); `LocalMonitor` =
    /// (false, false). Live-settable; a no-op if `id` already left.
    ///
    /// Works on a poisoned lock too: the node pump closes link TX through
    /// this, and closing must not fail because the mixing thread died.
    pub fn set_member_relay(&self, id: MemberId, contributes: bool, receives: bool) {
        let mut s = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(member) = s.members.iter_mut().find(|m| m.id == id) {
            member.contributes = contributes;
            member.receives = receives;
        }
    }

    /// Set (or replace) the wake callback for a member already added via
    /// [`Conference::add_member`] / [`Conference::add_member_keyed`] (iax-feab).
    /// A no-op if `id` is no longer present (e.g. it already left). See
    /// `Member::wake` for why this matters — without it, a member's replay can
    /// silently lose its tail to a fast-following hangup.
    pub fn set_member_wake(&self, id: MemberId, wake: Arc<dyn Fn() + Send + Sync>) {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        if let Some(member) = s.members.iter_mut().find(|m| m.id == id) {
            member.wake = Some(wake);
        }
    }

    /// Drain and return parrot [`crate::SignalReport`]s completed since the
    /// last call (iax-feab): `(member id, analysis of that member's last
    /// take)`, one entry per finished record→replay cycle, emitted only
    /// after the replay has fully drained from that member's private
    /// announce queue. Mutex-guarded.
    #[must_use]
    pub fn take_parrot_reports(&self) -> Vec<(MemberId, crate::SignalReport)> {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        std::mem::take(&mut s.parrot_reports)
    }

    /// Drain in-band DTMF digits registered since the last call (iax-8ca0):
    /// `(member id, digit)` in detection order, one entry per key-down on
    /// that member's leg. Digits are only produced while
    /// [`ConferenceConfig::dtmf_squelch`] is on. Mutex-guarded, mirroring
    /// [`Conference::take_parrot_reports`].
    #[must_use]
    pub fn drain_dtmf_digits(&self) -> Vec<(MemberId, char)> {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        std::mem::take(&mut s.dtmf_digits)
    }

    /// Queue a private announcement for ONE member (iax-c4ea): the 160-sample
    /// PCM `frames` are played to that member's leg only — one frame per tick,
    /// INSTEAD of the conference mix — until the queue drains. Used for the
    /// node-id join greeting so each joining user hears the node they reached
    /// without the audio touching any other member or the bus. No-op if `id`
    /// isn't a current member. Mutex-guarded.
    pub fn announce_to_member(&self, id: MemberId, frames: Vec<Vec<i16>>) {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        if let Some(m) = s.members.iter_mut().find(|m| m.id == id) {
            m.announce.extend(frames);
        }
    }

    /// Current depth of a member's private announce queue (iax-feab): the
    /// Manager's parrot report pump polls this to detect when a spoken report
    /// has fully drained onto the member's TX (so it's safe to hang the leg
    /// up). `0` if `id` isn't a current member (already left) — treated as
    /// "done" by the caller. Mutex-guarded, mirroring
    /// [`Conference::announce_to_member`]'s lookup.
    #[must_use]
    pub fn member_queue_len(&self, id: MemberId) -> usize {
        let s = self.shared.lock().expect("conference mutex poisoned");
        s.members
            .iter()
            .find(|m| m.id == id)
            .map_or(0, |m| m.announce.len())
    }

    /// Whether a member's jitter cushion still holds audio it received and
    /// has not yet played (iax-confjb). The bridge plays a member's frames
    /// 40-60 ms after they arrive, so its PTT can drop while the end of its
    /// over is still to come; whatever keys other legs on its behalf
    /// (`Manager::sync_link_keying`) waits for this to clear. `false` if `id`
    /// isn't a current member.
    ///
    /// A poisoned lock (the mixing thread panicked) reads as `false`: the node
    /// pump calls this every pass, and on any internal failure the safe
    /// direction for keying is unkeyed.
    #[must_use]
    pub fn member_holds_audio(&self, id: MemberId) -> bool {
        let Ok(s) = self.shared.lock() else {
            return false;
        };
        s.members
            .iter()
            .find(|m| m.id == id)
            .is_some_and(|m| m.src.cushion.holds_audio())
    }

    /// Remove a member (hangup / leave). The mix thread stops summing it next
    /// tick. No-op if `id` isn't a current member. Mutex-guarded, mirroring
    /// [`crate::Mixer::remove_call`].
    pub fn remove_member(&self, id: MemberId) {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        s.members.retain(|m| m.id != id);
    }

    /// Detach a member and return its RX `Receiver` so the caller can re-home it
    /// (the conference→handset live switch, iax-647d). The per-member jitter
    /// residual and cushion are dropped (a glitch of up to the cushion's
    /// depth, 40-80 ms, on a mode change). `None` if `id` isn't
    /// a current member. The member's TX `Sender` is dropped with the slot.
    #[must_use]
    pub fn take_member(&self, id: MemberId) -> Option<Receiver<crate::RxFrame>> {
        let mut s = self.shared.lock().expect("conference mutex poisoned");
        let pos = s.members.iter().position(|m| m.id == id)?;
        Some(s.members.remove(pos).src.rx)
    }

    /// Current member count.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.shared.lock().map_or(0, |s| s.members.len())
    }

    /// Live-toggle mix-minus (the `POST /bridge` re-wire path).
    pub fn set_mix_minus(&self, mix_minus: bool) {
        if let Ok(mut s) = self.shared.lock() {
            s.mix_minus = mix_minus;
        }
    }

    /// Run one mixing tick synchronously (test hook; the thread calls the same
    /// inner logic on its 20 ms clock).
    #[cfg(test)]
    fn tick_for_test(&self) {
        self.shared.lock().expect("conference mutex").tick();
    }

    /// Run one mixing tick now, for deterministic tests in crates that build
    /// a [`Conference`] with [`Conference::new`] (which starts no thread).
    /// Never call it on a started conference.
    #[doc(hidden)]
    pub fn tick_once(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.tick();
        }
    }
}

impl Drop for Conference {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)] // test bindings like a_in/a_id are intentionally parallel
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// A constant-level 20 ms (8 kHz) PCM frame.
    fn frame(level: i16) -> Vec<i16> {
        vec![level; frame_samples(8_000)]
    }

    /// Normalize the first sample of a PCM frame to f32.
    fn first_sample(frame: &[i16]) -> f32 {
        f32::from(frame[0]) / 32768.0
    }

    /// The normalized f32 value a given i16 level carries (exact PCM now).
    fn level_norm(level: i16) -> f32 {
        f32::from(level) / 32768.0
    }

    /// The normalized f32 a conference output carries for a mix of the given
    /// input levels: the mix sums the normalized inputs then re-quantizes to i16
    /// PCM, so the expected value is that same normalize→sum→quantize round-trip.
    fn mixed_norm(levels: &[i16]) -> f32 {
        let sum: f32 = levels.iter().map(|&l| level_norm(l)).sum();
        first_sample(&[encode_sample(sum)])
    }

    /// Helper: wire a member into the conference, returning its tx-feeder
    /// (we send its RX here) and its tx-output (we receive its mix here).
    fn join(conf: &Conference) -> (Sender<crate::RxFrame>, Receiver<Vec<i16>>, MemberId) {
        let (rx_tx, rx_rx) = channel(); // RX: we send into the conference
        let (tx_tx, tx_rx) = channel(); // TX: conference sends out, we receive
        let id = conf.add_member(rx_rx, tx_tx);
        (rx_tx, tx_rx, id)
    }

    #[test]
    fn two_members_a_tx_equals_b_rx_with_mix_minus() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        // A speaks; B is silent.
        a_in.send(frame(8000).into()).unwrap();
        b_in.send(frame(0).into()).unwrap();
        conf.tick_for_test();
        // B hears A (the only other member).
        let b_heard = b_out.recv().unwrap();
        assert!(
            (first_sample(&b_heard) - level_norm(8000)).abs() < 1e-3,
            "B's TX == A's RX"
        );
        // A hears only B (silence) — never itself (mix-minus).
        let a_heard = a_out.recv().unwrap();
        assert!(
            first_sample(&a_heard).abs() < 1e-3,
            "A never hears itself under mix-minus"
        );
    }

    #[test]
    fn three_members_each_hears_sum_of_other_two() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        let (c_in, c_out, _c) = join(&conf);
        a_in.send(frame(5000).into()).unwrap();
        b_in.send(frame(7000).into()).unwrap();
        c_in.send(frame(3000).into()).unwrap();
        conf.tick_for_test();
        // A hears B+C.
        let a_heard = first_sample(&a_out.recv().unwrap());
        assert!(
            (a_heard - mixed_norm(&[7000, 3000])).abs() < 1e-3,
            "A hears B+C, got {a_heard}"
        );
        // B hears A+C.
        let b_heard = first_sample(&b_out.recv().unwrap());
        assert!(
            (b_heard - mixed_norm(&[5000, 3000])).abs() < 1e-3,
            "B hears A+C, got {b_heard}"
        );
        // C hears A+B.
        let c_heard = first_sample(&c_out.recv().unwrap());
        assert!(
            (c_heard - mixed_norm(&[5000, 7000])).abs() < 1e-3,
            "C hears A+B, got {c_heard}"
        );
    }

    #[test]
    fn mix_minus_false_members_hear_themselves() {
        let conf = Conference::new(ConferenceConfig {
            mix_minus: false,
            ..ConferenceConfig::default()
        });
        let (a_in, a_out, _a) = join(&conf);
        let (b_in, _b_out, _b) = join(&conf);
        a_in.send(frame(6000).into()).unwrap();
        b_in.send(frame(0).into()).unwrap();
        conf.tick_for_test();
        // Full mix: A hears the full sum, which includes itself.
        let a_heard = first_sample(&a_out.recv().unwrap());
        assert!(
            (a_heard - level_norm(6000)).abs() < 2e-3,
            "full mix: A hears itself, got {a_heard}"
        );
    }

    #[test]
    fn include_local_radio_mic_in_every_tx_and_speaker_gets_sum() {
        let (mic_tx, mic_rx) = channel();
        let (spk_tx, spk_rx) = channel();
        let conf = Conference::new(ConferenceConfig {
            mix_minus: true,
            local_mic: Some(mic_rx),
            local_out: Some(spk_tx),
            ..ConferenceConfig::default()
        });
        let (a_in, a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        a_in.send(frame(4000).into()).unwrap();
        b_in.send(frame(2000).into()).unwrap();
        mic_tx.send(frame(1000)).unwrap();
        conf.tick_for_test();
        // A hears B + local mic (not itself).
        let a_heard = first_sample(&a_out.recv().unwrap());
        assert!(
            (a_heard - mixed_norm(&[2000, 1000])).abs() < 1e-3,
            "A hears B + mic, got {a_heard}"
        );
        // B hears A + local mic (not itself).
        let b_heard = first_sample(&b_out.recv().unwrap());
        assert!(
            (b_heard - mixed_norm(&[4000, 1000])).abs() < 1e-3,
            "B hears A + mic, got {b_heard}"
        );
        // The local speaker gets the sum of all members (no mic, no minus).
        let spk = first_sample(&spk_rx.recv().unwrap());
        assert!(
            (spk - mixed_norm(&[4000, 2000])).abs() < 1e-3,
            "speaker gets A+B, got {spk}"
        );
    }

    #[test]
    fn nobody_talking_sends_nothing() {
        // Radio-gap semantics on every member (nodebench finding 6): with no
        // member delivering audio, nothing goes out, not silence.
        let conf = Conference::new(ConferenceConfig::default());
        let (_a_in, a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        for _ in 0..3 {
            conf.tick_for_test();
        }
        assert!(a_out.try_recv().is_err(), "A is sent nothing");
        assert!(b_out.try_recv().is_err(), "B is sent nothing");
    }

    #[test]
    fn a_talker_opens_the_others_tx_for_the_over_plus_hangover_only() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        for _ in 0..3 {
            a_in.send(frame(4000).into()).unwrap();
            conf.tick_for_test();
        }
        // A's own TX stays shut: under mix-minus it hears nobody.
        assert!(a_out.try_recv().is_err(), "the talker hears no one");
        assert_eq!(b_out.try_iter().count(), 3, "B hears each tick of A");
        // A stops: B gets the hangover, then nothing.
        for _ in 0..20 {
            conf.tick_for_test();
        }
        let tail = b_out.try_iter().count();
        assert_eq!(
            tail,
            usize::try_from(TX_HANG_TICKS).unwrap() - 1,
            "hangover after the last frame"
        );
    }

    #[test]
    fn monitor_member_relays_onward_but_is_never_transmitted_to() {
        // Monitor link semantics (iax-42ce): its RX reaches the Transceive
        // members' TX, but nothing is ever sent to it — not even silence.
        let conf = Conference::new(ConferenceConfig::default());
        let (t_in, t_out, _t) = join(&conf); // Transceive
        let (m_in, m_out, m) = join(&conf); // Monitor
        conf.set_member_relay(m, true, false);
        t_in.send(frame(4000).into()).unwrap();
        m_in.send(frame(2000).into()).unwrap();
        conf.tick_for_test();
        let t_heard = first_sample(&t_out.recv().unwrap());
        assert!(
            (t_heard - level_norm(2000)).abs() < 1e-3,
            "Transceive member hears the Monitor member's RX, got {t_heard}"
        );
        assert!(
            m_out.try_recv().is_err(),
            "a Monitor member is sent NOTHING (no mix, no silence filler)"
        );
    }

    #[test]
    fn local_monitor_member_is_heard_on_the_speaker_only() {
        // LocalMonitor link semantics (iax-42ce): heard on the local speaker
        // bus, never relayed to other members, never transmitted to.
        let (spk_tx, spk_rx) = channel();
        let conf = Conference::new(ConferenceConfig {
            local_out: Some(spk_tx),
            ..ConferenceConfig::default()
        });
        let (t_in, t_out, _t) = join(&conf); // Transceive
        let (lm_in, lm_out, lm) = join(&conf); // LocalMonitor
        conf.set_member_relay(lm, false, false);
        t_in.send(frame(4000).into()).unwrap();
        lm_in.send(frame(2000).into()).unwrap();
        conf.tick_for_test();
        // The only other member is not relayed, so nobody T hears is
        // talking: T is sent nothing at all (finding 6 radio gap).
        assert!(
            t_out.try_recv().is_err(),
            "LocalMonitor RX is never relayed to other members"
        );
        assert!(
            lm_out.try_recv().is_err(),
            "a LocalMonitor member is sent nothing"
        );
        let spk = first_sample(&spk_rx.recv().unwrap());
        assert!(
            (spk - mixed_norm(&[4000, 2000])).abs() < 1e-3,
            "the local speaker still hears everyone, LocalMonitor included, got {spk}"
        );
    }

    #[test]
    fn set_member_relay_is_live_and_restores_full_relay() {
        // Flags are live-settable (a link mode change mid-call): drop a member
        // to Monitor, then restore Transceive — the mix follows on the next tick.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        conf.set_member_relay(a, true, false); // a → Monitor
        a_in.send(frame(4000).into()).unwrap();
        b_in.send(frame(2000).into()).unwrap();
        conf.tick_for_test();
        assert!(a_out.try_recv().is_err(), "Monitor a receives nothing");
        let b_heard = first_sample(&b_out.recv().unwrap());
        assert!(
            (b_heard - level_norm(4000)).abs() < 1e-3,
            "b still hears Monitor a, got {b_heard}"
        );
        conf.set_member_relay(a, true, true); // a → back to Transceive
        a_in.send(frame(4000).into()).unwrap();
        b_in.send(frame(2000).into()).unwrap();
        conf.tick_for_test();
        let a_heard = first_sample(&a_out.recv().unwrap());
        assert!(
            (a_heard - level_norm(2000)).abs() < 1e-3,
            "restored a hears b again, got {a_heard}"
        );
    }

    #[test]
    fn announcement_still_reaches_a_non_receiving_member() {
        // The join greeting is per-leg private audio (iax-c4ea), not conference
        // mix — a Monitor member still gets it.
        let conf = Conference::new(ConferenceConfig::default());
        let (_m_in, m_out, m) = join(&conf);
        conf.set_member_relay(m, true, false);
        conf.announce_to_member(m, vec![frame(3000)]);
        conf.tick_for_test();
        let heard = first_sample(&m_out.recv().unwrap());
        assert!(
            (heard - level_norm(3000)).abs() < 1e-3,
            "announcement bypasses the receives gate, got {heard}"
        );
    }

    #[test]
    fn starved_member_contributes_silence() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf); // B never sends — starved.
        a_in.send(frame(8000).into()).unwrap();
        conf.tick_for_test();
        // B (starved) still gets a frame, and it carries A's audio — A's
        // contribution wasn't blocked by B's empty channel.
        let b_heard = first_sample(&b_out.recv().unwrap());
        assert!(
            (b_heard - level_norm(8000)).abs() < 2e-3,
            "B hears A even though B is starved, got {b_heard}"
        );
    }

    #[test]
    fn removed_member_is_dropped_from_the_mix() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, alpha) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        assert_eq!(conf.member_count(), 2);
        conf.remove_member(alpha);
        assert_eq!(conf.member_count(), 1);
        // A's RX receiver was dropped with its member slot, so this send has
        // nowhere to land — the failed send is itself evidence A left the mix.
        let _ = a_in.send(frame(8000).into());
        b_in.send(frame(0).into()).unwrap();
        conf.tick_for_test();
        // B is now alone: nobody it hears is talking, so it is sent nothing.
        assert!(b_out.try_recv().is_err(), "removed A no longer contributes");
    }

    #[test]
    fn doubletalk_sums_and_clamps() {
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a, _aid) = join(&conf);
        let (b_in, _b, _bid) = join(&conf);
        let (c_in, c_out, _cid) = join(&conf);
        // Two near-full-scale talkers: their sum would exceed 1.0 → must clamp.
        a_in.send(frame(30000).into()).unwrap();
        b_in.send(frame(30000).into()).unwrap();
        c_in.send(frame(0).into()).unwrap();
        conf.tick_for_test();
        let heard = c_out.recv().unwrap();
        for &b in &heard {
            let s = f32::from(b) / 32768.0;
            assert!((-1.0..=1.0).contains(&s), "clamped to [-1,1]");
        }
    }

    #[test]
    fn announce_to_member_plays_to_that_member_only() {
        // iax-c4ea: a private announcement queued for one member plays on that
        // member's TX (overriding the mix for that tick) and does NOT appear on
        // any other member's TX.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, a_id) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        // Both speak so the normal mix is non-trivial.
        a_in.send(frame(5000).into()).unwrap();
        b_in.send(frame(7000).into()).unwrap();
        // Queue a one-frame greeting for A only.
        conf.announce_to_member(a_id, vec![frame(9000)]);
        conf.tick_for_test();
        // A hears its greeting (9000), NOT the mix-minus of B (7000).
        let a_heard = first_sample(&a_out.recv().unwrap());
        assert!(
            (a_heard - level_norm(9000)).abs() < 1e-3,
            "A hears its private greeting, got {a_heard}"
        );
        // B is unaffected: still hears A's RX (mix-minus), never the greeting.
        let b_heard = first_sample(&b_out.recv().unwrap());
        assert!(
            (b_heard - level_norm(5000)).abs() < 1e-3,
            "B hears A's RX, not A's private greeting, got {b_heard}"
        );
    }

    #[test]
    fn announce_to_member_drains_then_returns_to_mix() {
        // After the queued frames are consumed, the member returns to the mix.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, a_out, a_id) = join(&conf);
        let (b_in, _b_out, _b) = join(&conf);
        conf.announce_to_member(a_id, vec![frame(9000)]); // one greeting frame
        // Tick 1: A gets the greeting.
        a_in.send(frame(1000).into()).unwrap();
        b_in.send(frame(7000).into()).unwrap();
        conf.tick_for_test();
        let t1 = first_sample(&a_out.recv().unwrap());
        assert!((t1 - level_norm(9000)).abs() < 1e-3, "tick1 greeting");
        // Tick 2: queue drained → A is back on the mix (hears B = 7000).
        a_in.send(frame(1000).into()).unwrap();
        b_in.send(frame(7000).into()).unwrap();
        conf.tick_for_test();
        let t2 = first_sample(&a_out.recv().unwrap());
        assert!(
            (t2 - level_norm(7000)).abs() < 1e-3,
            "tick2 back to mix (hears B), got {t2}"
        );
    }

    #[test]
    fn announce_to_member_unknown_id_is_noop() {
        let conf = Conference::new(ConferenceConfig::default());
        let (_a_in, _a_out, a_id) = join(&conf);
        conf.remove_member(a_id);
        // Must not panic on an unknown member id.
        conf.announce_to_member(a_id, vec![frame(9000)]);
        assert_eq!(conf.member_count(), 0);
    }

    fn parrot_conf(t: ParrotTuning) -> Conference {
        Conference::new(ConferenceConfig {
            parrot: Some(t),
            ..ConferenceConfig::default()
        })
    }

    #[test]
    fn parrot_vox_records_replays_privately_then_reports() {
        let t = ParrotTuning {
            playback_delay_ticks: 2,
            silence_gap_ticks: 2,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        };
        let conf = parrot_conf(t);
        let (a_rx_tx, a_rx) = channel();
        let (a_tx_tx, a_tx_rx) = channel();
        let a_id = conf.add_member(a_rx, a_tx_tx);
        let (b_rx_tx, b_rx) = channel();
        let (b_tx_tx, b_tx_rx) = channel();
        let _b = conf.add_member(b_rx, b_tx_tx);
        let _b_keep = b_rx_tx;
        for _ in 0..3 {
            a_rx_tx.send(frame(9000).into()).unwrap();
            conf.tick_for_test();
        }
        for _ in 0..10 {
            conf.tick_for_test(); // gap(2) + delay(2) + 3 replay frames + slack
        }
        let a_frames: Vec<Vec<i16>> = std::iter::from_fn(|| a_tx_rx.try_recv().ok()).collect();
        assert!(
            a_frames.iter().any(|f| f[0] == 9000),
            "A hears its recording"
        );
        let b_frames: Vec<Vec<i16>> = std::iter::from_fn(|| b_tx_rx.try_recv().ok()).collect();
        assert!(
            b_frames.is_empty(),
            "idle parrot leg B receives NO frames (unkeyed = silent air), got {}",
            b_frames.len()
        );
        // Report emitted for A only, AFTER the replay drained.
        let reports = conf.take_parrot_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, a_id);
        assert!(
            !reports[0].1.underdriven,
            "9000-amplitude audio is not underdriven"
        );
    }

    #[test]
    fn parrot_vox_take_keeps_the_pauses_between_words() {
        // A spoken over has quiet frames between its words. In VOX mode a
        // pause shorter than the silence gap is part of the take: the replay
        // must carry it in place, or the echo comes back with its pauses
        // squeezed out (seen against a stock ASL3 node in nodebench's
        // `parrot_echoes_speech`). Trailing silence that ended the take is
        // not replayed.
        let t = ParrotTuning {
            playback_delay_ticks: 1,
            silence_gap_ticks: 4,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        };
        let conf = parrot_conf(t);
        let (rx_tx, rx) = channel();
        let (tx_tx, tx_rx) = channel();
        let _m = conf.add_member(rx, tx_tx);
        let over = [9000, 9000, 0, 0, 7000, 7000, 7000, 0, 5000];
        for level in over {
            rx_tx.send(frame(level).into()).unwrap();
            conf.tick_for_test();
        }
        for _ in 0..30 {
            conf.tick_for_test(); // gap + delay + replay + slack
        }
        let replay: Vec<i16> = std::iter::from_fn(|| tx_rx.try_recv().ok())
            .map(|f| f[0])
            .collect();
        assert_eq!(replay, over, "the take replays whole, pauses included");
    }

    #[test]
    fn parrot_take_has_no_hole_where_a_frame_was_late() {
        // A frame that arrives just after its tick plays one tick later:
        // that tick delivers nothing, the next delivers two frames. The take
        // records what the caller sent, so the replay has no 20 ms of
        // silence where the bridge's tick and the network disagreed (seen
        // against a stock ASL3 node in nodebench's `parrot_echoes_speech`).
        for keyed in [false, true] {
            let t = ParrotTuning {
                playback_delay_ticks: 1,
                silence_gap_ticks: 4,
                vox_threshold_db: -40.0,
                max_record_ticks: 500,
            };
            let conf = parrot_conf(t);
            let key = Arc::new(AtomicBool::new(false));
            let (rx_tx, rx) = channel();
            let (tx_tx, tx_rx) = channel();
            let _m = conf.add_member_keyed(rx, tx_tx, Arc::clone(&key));
            if keyed {
                key.store(true, Ordering::Relaxed); // latches the PTT gate
            }
            rx_tx.send(frame(9000).into()).unwrap();
            conf.tick_for_test();
            conf.tick_for_test(); // the next frame is late
            rx_tx.send(frame(8000).into()).unwrap();
            rx_tx.send(frame(7000).into()).unwrap();
            conf.tick_for_test();
            conf.tick_for_test();
            key.store(false, Ordering::Relaxed);
            for _ in 0..30 {
                conf.tick_for_test();
            }
            let replay: Vec<i16> = std::iter::from_fn(|| tx_rx.try_recv().ok())
                .map(|f| f[0])
                .collect();
            assert_eq!(replay, [9000, 8000, 7000], "keyed: {keyed}");
        }
    }

    #[test]
    fn parrot_ptt_gate_wins_over_vox_once_latched() {
        let t = ParrotTuning {
            playback_delay_ticks: 1,
            silence_gap_ticks: 1,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        };
        let conf = parrot_conf(t);
        let key = Arc::new(AtomicBool::new(false));
        let (rx_tx, rx) = channel();
        let (tx_tx, tx_rx) = channel();
        let _m = conf.add_member_keyed(rx, tx_tx, Arc::clone(&key));
        key.store(true, Ordering::Relaxed);
        for _ in 0..3 {
            rx_tx.send(frame(0).into()).unwrap(); // SILENT but keyed: PTT gate records
            conf.tick_for_test();
        }
        key.store(false, Ordering::Relaxed);
        for _ in 0..8 {
            conf.tick_for_test();
        }
        assert_eq!(
            conf.take_parrot_reports().len(),
            1,
            "keyed-silence take reports"
        );
        let n = std::iter::from_fn(|| tx_rx.try_recv().ok()).count();
        assert_eq!(n, 3, "exactly the 3 recorded frames replayed, no fillers");
    }

    #[test]
    fn parrot_idle_member_receives_no_frames() {
        // Radio semantics (iax-a54e): an unkeyed leg sends NOTHING. An idle
        // parrot member (no voice, no announce queued) must receive zero
        // frames — continuous silence filler would defeat frame-arrival-gated
        // clients' end-of-transmission detection.
        let conf = parrot_conf(ParrotTuning::default());
        let (_rx_tx, rx) = channel();
        let (tx_tx, tx_rx) = channel();
        let _m = conf.add_member(rx, tx_tx);
        for _ in 0..10 {
            conf.tick_for_test();
        }
        assert_eq!(
            std::iter::from_fn(|| tx_rx.try_recv().ok()).count(),
            0,
            "idle parrot member got frames"
        );
    }

    #[test]
    fn parrot_cap_forces_replay_at_10s() {
        let t = ParrotTuning {
            playback_delay_ticks: 1,
            silence_gap_ticks: 100,
            vox_threshold_db: -40.0,
            max_record_ticks: 3,
        };
        let conf = parrot_conf(t);
        let (rx_tx, rx) = channel();
        let (tx_tx, tx_rx) = channel();
        let _m = conf.add_member(rx, tx_tx);
        for _ in 0..10 {
            rx_tx.send(frame(9000).into()).unwrap();
            conf.tick_for_test();
        }
        let frames: Vec<Vec<i16>> = std::iter::from_fn(|| tx_rx.try_recv().ok()).collect();
        assert!(
            frames.iter().any(|f| f[0] == 9000),
            "capped recording replays"
        );
    }

    #[test]
    fn default_cap_is_ten_seconds() {
        assert_eq!(ParrotTuning::default().max_record_ticks, 500);
    }

    /// Synthesize `count` consecutive 20 ms (8 kHz) i16 PCM frames of a
    /// two-tone DTMF pair, phase-continuous across frames.
    #[allow(clippy::cast_possible_truncation)]
    fn dtmf_frames(row_hz: f32, col_hz: f32, count: usize) -> Vec<Vec<i16>> {
        let n = frame_samples(8_000);
        #[allow(clippy::cast_precision_loss)]
        (0..count * n)
            .map(|i| {
                let t = i as f32 / 8_000.0;
                let s = 0.25 * (std::f32::consts::TAU * row_hz * t).sin()
                    + 0.25 * (std::f32::consts::TAU * col_hz * t).sin();
                encode_sample_rounded(s)
            })
            .collect::<Vec<i16>>()
            .chunks(n)
            .map(<[i16]>::to_vec)
            .collect()
    }

    /// Peak normalized magnitude of a PCM frame.
    fn peak_of(frame: &[i16]) -> f32 {
        frame
            .iter()
            .map(|&s| (f32::from(s) / 32768.0).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn dtmf_member_is_squelched_from_relay_but_heard_locally_and_digit_drains() {
        // iax-8ca0: A sounds a touch-tone ('5' = 770+1336 Hz). The tone must
        // NOT reach B's TX (relay squelch), MUST still reach the local
        // speaker bus (unfiltered), and the digit must drain with A's id.
        let (spk_tx, spk_rx) = channel();
        let conf = Conference::new(ConferenceConfig {
            local_out: Some(spk_tx),
            ..ConferenceConfig::default()
        });
        let (a_in, _a_out, a_id) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        for tone in dtmf_frames(770.0, 1336.0, 4) {
            a_in.send(tone.into()).unwrap();
            b_in.send(frame(0).into()).unwrap();
            conf.tick_for_test();
            // A squelched member is not talking, so B is sent nothing.
            let b_peak = b_out.try_recv().map_or(0.0, |f| peak_of(&f));
            assert!(
                b_peak < 1e-3,
                "B never hears A's tone (relay squelch), peak {b_peak}"
            );
            let spk = spk_rx.recv().unwrap();
            assert!(
                peak_of(&spk) > 0.1,
                "the local speaker still hears the tone, peak {}",
                peak_of(&spk)
            );
        }
        let digits = conf.drain_dtmf_digits();
        assert_eq!(digits, vec![(a_id, '5')], "digit drains with A's id");
        assert!(
            conf.drain_dtmf_digits().is_empty(),
            "drain empties the queue"
        );
    }

    #[test]
    fn audio_resumes_after_tone_plus_tail() {
        // After the tone ends the squelch holds for SQUELCH_TAIL_BLOCKS more
        // ticks, then A's normal audio relays to B again.
        use crate::dtmf::SQUELCH_TAIL_BLOCKS;
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        for tone in dtmf_frames(697.0, 1209.0, 3) {
            a_in.send(tone.into()).unwrap();
            b_in.send(frame(0).into()).unwrap();
            conf.tick_for_test();
            let _ = b_out.try_recv();
        }
        // Tail ticks: A speaks normally but is still squelched.
        for i in 0..SQUELCH_TAIL_BLOCKS {
            a_in.send(frame(6000).into()).unwrap();
            b_in.send(frame(0).into()).unwrap();
            conf.tick_for_test();
            let b_peak = b_out.try_recv().map_or(0.0, |f| peak_of(&f));
            assert!(
                b_peak < 1e-3,
                "tail tick {i}: still squelched, peak {b_peak}"
            );
        }
        // Tail expired: the relay resumes.
        a_in.send(frame(6000).into()).unwrap();
        b_in.send(frame(0).into()).unwrap();
        conf.tick_for_test();
        let b_heard = b_out.recv().unwrap();
        assert!(
            (first_sample(&b_heard) - level_norm(6000)).abs() < 1e-3,
            "relay resumes after the tail, got {}",
            first_sample(&b_heard)
        );
    }

    #[test]
    fn dtmf_squelch_off_relays_the_tone_and_registers_no_digit() {
        // dtmf_squelch = false disables the whole in-band path: the tone
        // relays verbatim and no digit is queued.
        let conf = Conference::new(ConferenceConfig {
            dtmf_squelch: false,
            ..ConferenceConfig::default()
        });
        let (a_in, _a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        for tone in dtmf_frames(770.0, 1336.0, 3) {
            a_in.send(tone.into()).unwrap();
            b_in.send(frame(0).into()).unwrap();
            conf.tick_for_test();
            let b_heard = b_out.recv().unwrap();
            assert!(
                peak_of(&b_heard) > 0.1,
                "squelch off: B hears the tone, peak {}",
                peak_of(&b_heard)
            );
        }
        assert!(
            conf.drain_dtmf_digits().is_empty(),
            "squelch off: no digits registered"
        );
    }

    #[test]
    fn speech_level_frames_are_not_squelched() {
        // The existing constant-level test frames must never read as DTMF —
        // this pins the dtmf_squelch=true default to pre-iax-8ca0 behavior
        // for non-tone audio.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (b_in, b_out, _b) = join(&conf);
        for _ in 0..5 {
            a_in.send(frame(8000).into()).unwrap();
            b_in.send(frame(0).into()).unwrap();
            conf.tick_for_test();
            let b_heard = b_out.recv().unwrap();
            assert!(
                (first_sample(&b_heard) - level_norm(8000)).abs() < 1e-3,
                "non-tone audio relays untouched"
            );
        }
        assert_eq!(
            conf.drain_dtmf_digits(),
            [] as [(crate::conference::MemberId, char); 0]
        );
    }

    /// A member's over, as the network delivered it: frame `k` carries the
    /// sender's timestamp `20 k` ms and level `100 (k + 1)`, and arrives at
    /// `arrive(k)` ms. The bridge ticks at `20 t + 10` ms (the tick edge sits
    /// mid-frame, so a frame 3-9 ms late makes its tick and one 11-15 ms
    /// late misses it). Returns what the other member was sent each tick:
    /// `(tick time, Some(frame index) | Some(-1) for silence | None)`.
    fn bridge_jittered_over(
        conf: &Conference,
        a_in: &Sender<crate::RxFrame>,
        b_out: &Receiver<Vec<i16>>,
        frames: &[(i64, i64)],
        ticks: std::ops::Range<i64>,
    ) -> Vec<(i64, Option<i64>)> {
        let mut arrivals = frames.to_vec();
        arrivals.sort_unstable();
        let mut next = 0;
        let mut heard = Vec::new();
        for t in ticks {
            let now = 20 * t + 10;
            while next < arrivals.len() && arrivals[next].0 <= now {
                let (_, k) = arrivals[next];
                let level = i16::try_from(100 * (k % 300 + 1)).unwrap();
                a_in.send(crate::RxFrame::timed(frame(level), 20 * k, 20))
                    .unwrap();
                next += 1;
            }
            conf.tick_for_test();
            let got: Vec<Vec<i16>> = b_out.try_iter().collect();
            assert!(got.len() <= 1, "one frame per tick at most");
            let idx = got.first().map(|f| {
                if f[0] == 0 {
                    -1
                } else {
                    // The mix re-quantizes 100 (k + 1) to a level a hair under it.
                    (i64::from(f[0]) + 50) / 100 - 1
                }
            });
            heard.push((now, idx));
        }
        heard
    }

    #[test]
    fn bridge_output_has_no_stall_repeat_or_gap_under_arrival_jitter() {
        // nodebench `speech_server_to_node`, S6 (iax-confjb): a member's
        // frames arrive a few ms late, now and then two together, and one is
        // lost. What the other member is sent must be the over in order, one
        // frame per tick, with one frame of silence where the lost frame
        // was: no stall, repeat or gap of the bridge's own making.
        const N: i64 = 60;
        const LOST: i64 = 31;
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        let late = [3, 8, 15, 4, 12, 6, 15, 3, 11, 9, 14, 5, 13, 7, 10];
        let frames: Vec<(i64, i64)> = (0..N)
            .filter(|&k| k != LOST)
            .map(|k| {
                let at = match k {
                    // 22 ms late, arriving together with frame 21.
                    20 | 21 => 20 * 21 + 2,
                    // 18 ms late, together with 45.
                    44 | 45 => 20 * 45,
                    _ => 20 * k + late[usize::try_from(k).unwrap() % late.len()],
                };
                (at, k)
            })
            .collect();
        let heard = bridge_jittered_over(&conf, &a_in, &b_out, &frames, 0..N + 20);
        let sent: Vec<&(i64, Option<i64>)> = heard.iter().filter(|(_, f)| f.is_some()).collect();
        let first = heard
            .iter()
            .position(|(_, f)| *f == Some(0))
            .expect("the over was bridged");
        let last = heard
            .iter()
            .rposition(|(_, f)| *f == Some(N - 1))
            .expect("the over's last frame was bridged");
        let over: Vec<i64> = heard[first..=last]
            .iter()
            .map(|(_, f)| f.unwrap_or(-2))
            .collect();
        let want: Vec<i64> = (0..N).map(|k| if k == LOST { -1 } else { k }).collect();
        assert_eq!(over, want, "the over, tick by tick ({sent:?})");
        // Latency: frame 0 was due at 0 ms and arrived at 3 ms. The cushion
        // holds two frames (40 ms), and the tick edge adds under 20 ms.
        let delay = heard[first].0 - frames[0].0;
        assert!(
            (40..60).contains(&delay),
            "added delay {delay} ms, want 40-60"
        );
    }

    #[test]
    fn each_over_starts_on_a_fresh_cushion() {
        // Over 1 has its sender's path step 62 ms slower halfway through:
        // one frame misses its tick, the schedule slips once and the cushion
        // deepens a tick. Over 2 (clean) starts on that deeper hold; over 3
        // is back on the base hold. The delay never carries further.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        let mut frames: Vec<(i64, i64)> = (0..20)
            .map(|k| (20 * k + if k < 10 { 5 } else { 62 }, k))
            .collect();
        frames.extend((100..120).chain(200..220).map(|k| (20 * k + 5, k)));
        let heard = bridge_jittered_over(&conf, &a_in, &b_out, &frames, 0..240);
        let delay = |k: i64| {
            let at = heard
                .iter()
                .find(|(_, f)| *f == Some(k))
                .unwrap_or_else(|| panic!("frame {k} bridged"))
                .0;
            at - (20 * k + 5)
        };
        assert!((40..60).contains(&delay(0)), "over 1: {}", delay(0));
        assert!((60..80).contains(&delay(100)), "over 2: {}", delay(100));
        assert!((40..60).contains(&delay(200)), "over 3: {}", delay(200));
        // Every frame of every over was bridged, and between the overs B was
        // sent nothing once the hangover ran out.
        for k in (0..20).chain(100..120).chain(200..220) {
            delay(k);
        }
        let between = heard
            .iter()
            .filter(|(at, f)| (1000..1900).contains(at) && f.is_some())
            .count();
        assert_eq!(between, 0, "B's TX closed between the overs");
    }

    #[test]
    fn a_late_member_never_holds_up_the_others() {
        // A and C talk at once; A's frames 10-19 are held up 200 ms and then
        // arrive in a burst. B hears C's over without a break, on C's own
        // schedule, whatever A's cushion is doing.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        let (c_in, _c_out, _c) = join(&conf);
        let mut heard = Vec::new();
        for t in 0..40_i64 {
            let now = 20 * t + 10;
            for k in 0..30_i64 {
                let a_at = 20 * k + 4 + if (10..20).contains(&k) { 200 } else { 0 };
                if (now - 20..now).contains(&a_at) {
                    a_in.send(crate::RxFrame::timed(frame(1000), 20 * k, 20))
                        .unwrap();
                }
                if (now - 20..now).contains(&(20 * k + 4)) {
                    c_in.send(crate::RxFrame::timed(frame(3000), 20 * k, 20))
                        .unwrap();
                }
            }
            conf.tick_for_test();
            heard.push(b_out.try_iter().next().map(|f| f[0]));
        }
        // C's first frame arrived at 4 ms and plays at the third tick (50 ms);
        // its last plays 29 ticks later. B hears C at every one of them.
        let c_only = first_sample(&[encode_sample(level_norm(3000))]);
        for (t, got) in heard.iter().enumerate().take(32).skip(2) {
            let got = got.unwrap_or_else(|| panic!("tick {t}: B was sent nothing"));
            assert!(
                f32::from(got) / 32768.0 >= c_only - 1e-3,
                "tick {t}: B hears at least C, got {got}"
            );
        }
    }

    #[test]
    fn parrot_take_of_timestamped_frames_is_whole_at_the_unkey() {
        // The parrot reads its caller with no cushion: under the PTT gate,
        // every frame that arrived while keyed is in the take, the last one
        // included.
        let t = ParrotTuning {
            playback_delay_ticks: 1,
            silence_gap_ticks: 4,
            vox_threshold_db: -40.0,
            max_record_ticks: 500,
        };
        let conf = parrot_conf(t);
        let key = Arc::new(AtomicBool::new(false));
        let (rx_tx, rx) = channel();
        let (tx_tx, tx_rx) = channel();
        let _m = conf.add_member_keyed(rx, tx_tx, Arc::clone(&key));
        key.store(true, Ordering::Relaxed);
        for k in 0..5_i16 {
            rx_tx
                .send(crate::RxFrame::timed(
                    frame(1000 * (k + 1)),
                    20 * i64::from(k),
                    20,
                ))
                .unwrap();
            conf.tick_for_test();
        }
        key.store(false, Ordering::Relaxed);
        for _ in 0..30 {
            conf.tick_for_test();
        }
        let replay: Vec<i16> = std::iter::from_fn(|| tx_rx.try_recv().ok())
            .map(|f| f[0])
            .collect();
        assert_eq!(replay, [1000, 2000, 3000, 4000, 5000]);
    }

    #[test]
    fn a_late_tick_is_caught_up_not_lost() {
        // nodebench, S6 under host load: a tick that woke 18-23 ms late used
        // to push the whole schedule back a tick, so the bridge sent 49
        // frames in that second and every member's input fell 20 ms further
        // behind. A late tick is caught up; only a long stall is abandoned.
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // On time: the next tick is one TICK on.
        assert_eq!(next_tick_after(ms(20), ms(21)), ms(40));
        // 23 ms late: the next tick is still due at 40, so it runs at once.
        assert_eq!(next_tick_after(ms(20), ms(43)), ms(40));
        // 90 ms late: still within the catch-up window.
        assert_eq!(next_tick_after(ms(20), ms(110)), ms(40));
        // A stall past the window resynchronises on now.
        assert_eq!(next_tick_after(ms(20), ms(500)), ms(520));
    }

    #[test]
    fn a_poisoned_conference_reads_as_holding_nothing_and_still_closes_tx() {
        // The node pump asks every 50 ms whether a member's cushion still
        // holds audio, and closes or opens link TX. If the mixing thread ever
        // panicked holding the lock, those calls must not panic the pump too
        // (links would stay keyed in their last state): the safe answer is
        // "nothing held", and closing a member's TX must still work.
        let conf = Conference::new(ConferenceConfig::default());
        let (a_in, _a_out, a) = join(&conf);
        for k in 0..3 {
            a_in.send(crate::RxFrame::timed(frame(4000), 20 * k, 20))
                .unwrap();
        }
        conf.tick_for_test();
        assert!(conf.member_holds_audio(a));
        let shared = Arc::clone(&conf.shared);
        let _ = std::thread::spawn(move || {
            let _guard = shared.lock().unwrap();
            panic!("the mixing thread dies holding the lock");
        })
        .join();
        assert!(conf.shared.is_poisoned());
        assert!(
            !conf.member_holds_audio(a),
            "poisoned reads as nothing held"
        );
        conf.set_member_relay(a, true, false);
        let s = conf
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!s.members[0].receives, "TX closed despite the poison");
    }

    #[test]
    fn started_thread_mixes_on_its_own_clock() {
        // Smoke test the live mixing thread end-to-end: two members, A speaks,
        // B should receive A's audio within a few ticks. Drop joins the thread.
        let conf = Conference::start(ConferenceConfig::default());
        let (a_in, _a_out, _a) = join(&conf);
        let (_b_in, b_out, _b) = join(&conf);
        for _ in 0..10 {
            a_in.send(frame(8000).into()).unwrap();
        }
        let heard = b_out
            .recv_timeout(Duration::from_millis(500))
            .expect("the mixing thread delivered a frame");
        assert!(
            (first_sample(&heard) - level_norm(8000)).abs() < 2e-3,
            "live thread bridged A→B"
        );
    }
}
