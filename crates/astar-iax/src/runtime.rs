// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The per-call runtime: spawns one blocking mio thread per call and pumps the FSM. Shared by the WT dial path and the Manager (iax-64b6 P2 — extracted from client.rs).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self};
use std::time::{Duration, Instant};

use astar_audio::CallAudio;
use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::parse_lenient;
use astar_iax_core::session::CallProfile;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{Action, AppCommand, AppEvent, Event, Fsm, TimerKind};
use astar_iax_core::session::reliability::{Reliability, ReliabilityConfig, RxOutcome};
use mio::{Poll, Waker};

use crate::audio_bridge::PttGate;
use crate::call::{Call, CallEvent};
use crate::transport::{LinkSocket, NetStack};

/// Command sent from a [`Call`] handle into its runtime thread.
pub(crate) enum RuntimeCommand {
    /// Forward an FSM-level command (PTT, DTMF, text, hangup, …).
    App(AppCommand),
    /// Tear the call down and exit the runtime thread.
    Shutdown,
}

/// Parameters for [`spawn_call_runtime`]. The audio (`CallAudio`) and the
/// `poll`/`waker`/`gate` are built by the caller (so the µ-law sink can capture
/// the per-call waker before the runtime spawns); `call_no` is the per-socket
/// source-call number (Q4: stays `CallNo(1)` — one socket per call).
pub(crate) struct SpawnParams {
    pub peer: SocketAddr,
    pub caller_id: String,
    pub dest: String,
    pub secret: String,
    pub profile: CallProfile,
    pub call_no: CallNo,
    pub poll: Poll,
    pub waker: Arc<Waker>,
    pub gate: PttGate,
    pub audio: CallAudio,
    pub frame_observer: Option<std::sync::mpsc::Sender<crate::trace::TracedFrame>>,
    /// Manager-assigned identity (keystone). `CallId(0)` for the non-pooled
    /// `Client::dial` path.
    pub id: crate::call::CallId,
    /// Node number for the snapshot (empty for the client path).
    pub node: String,
    /// Snapshot mode (secret-free).
    pub mode: crate::call::CallSnapshotMode,
    /// Whether to build a pooled `Call` (carrying `id`/`node`/`mode`/state) or
    /// the plain client-path handle.
    pub pooled: bool,
    /// Station bus sample rate (iax-4348). The codec edge resamples between this
    /// and the negotiated wire rate; `8000` keeps the edge byte-identical to the
    /// pre-slin16 paths (no resampler ever constructed).
    pub sample_rate: u32,
    /// Transport seam (iax-b6f5): the socket factory the runtime binds its
    /// per-call socket from. [`crate::transport::OsNetStack`] everywhere today.
    pub net: Arc<dyn NetStack>,
}

/// Spawn the per-call runtime thread and return the canonical [`Call`] handle
/// plus its event receiver. Shared by `Client::dial` (non-pooled) and
/// `Manager::dial` (pooled). Threads an explicit `call_no` and the caller's
/// `CallAudio` ends straight into `run_loop`.
pub(crate) fn spawn_call_runtime(
    p: SpawnParams,
) -> Result<(Call, mpsc::Receiver<CallEvent>), crate::error::IaxError> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<RuntimeCommand>();
    let (event_tx, event_rx) = mpsc::channel::<CallEvent>();

    // iax-a307: shared smoothed-RTT cell (micros; u32::MAX = no sample yet).
    let rtt_micros = Arc::new(AtomicU32::new(u32::MAX));
    let rtt_for_loop = Arc::clone(&rtt_micros);

    // iax-9e55: shared cumulative count of voice-ts-ladder re-anchors (>80 ms
    // drift events). Published by the run-loop, read by Call::snapshot as
    // `tx_reanchors`. A plain TX health counter, credential-free.
    let tx_reanchors = Arc::new(AtomicU64::new(0));
    let tx_reanchors_for_loop = Arc::clone(&tx_reanchors);

    // iax-31f7: shared negotiated-codec cell (format bits; 0 = not yet
    // negotiated). Published once per event-handling pass by the run-loop,
    // read by Call::snapshot as `negotiated_format`.
    let format_bits = Arc::new(AtomicU32::new(0));
    let format_bits_for_loop = Arc::clone(&format_bits);

    // iax-42e9 keystone: shared connection-state cell published by the runtime
    // for Call::snapshot (Connecting → Active → Hungup).
    let state = Arc::new(std::sync::atomic::AtomicU8::new(
        crate::call::STATE_CONNECTING,
    ));
    let state_for_loop = Arc::clone(&state);

    // iax-feab: shared remote-PTT-key cell, stored into whenever
    // AppEvent::RemotePtt fires (parrot mode's per-leg key gate).
    let remote_keyed = Arc::new(AtomicBool::new(false));
    let remote_keyed_for_loop = Arc::clone(&remote_keyed);

    let waker_for_caller = Arc::clone(&p.waker);
    let SpawnParams {
        peer,
        caller_id,
        dest,
        secret,
        profile,
        call_no,
        poll,
        waker,
        gate,
        audio,
        frame_observer,
        id,
        node,
        mode,
        pooled,
        sample_rate,
        net,
    } = p;
    let mic_rx = audio.tx_frames;
    let spk_tx = audio.rx_frames;
    let preroll_lead = audio.preroll_lead;

    let handle = thread::Builder::new()
        .name(format!("iax-call-{peer}"))
        .spawn(move || {
            run_loop(
                poll,
                waker,
                net,
                peer,
                caller_id,
                dest,
                secret,
                profile,
                call_no,
                cmd_rx,
                event_tx,
                mic_rx,
                spk_tx,
                rtt_for_loop,
                state_for_loop,
                frame_observer,
                preroll_lead,
                tx_reanchors_for_loop,
                format_bits_for_loop,
                sample_rate,
                remote_keyed_for_loop,
            );
        })?;

    let call = if pooled {
        Call::new_pooled(
            cmd_tx,
            waker_for_caller,
            handle,
            gate,
            rtt_micros,
            id,
            node,
            mode,
            state,
            tx_reanchors,
            format_bits,
            remote_keyed,
        )
    } else {
        Call::new(
            cmd_tx,
            waker_for_caller,
            handle,
            gate,
            rtt_micros,
            tx_reanchors,
            format_bits,
            remote_keyed,
        )
    };
    Ok((call, event_rx))
}

/// Translate an FSM [`AppEvent`] into the public [`CallEvent`] surface.
/// Returns `None` for events handled out-of-band (voice → speaker).
///
/// `negotiated` is the format the FSM settled on for this call (iax-31f7):
/// the caller passes `fsm.negotiated_format().unwrap_or(VoiceFormat::G711U)`,
/// so `Answered` reports what was actually negotiated instead of a hardcoded
/// G.711µ. TX/RX payloads are transcoded at the network edge (`codec_edge`).
fn translate(event: &AppEvent, negotiated: VoiceFormat) -> Option<CallEvent> {
    match event {
        AppEvent::Connected { .. } => Some(CallEvent::Answered { format: negotiated }),
        AppEvent::Disconnected { reason } => Some(CallEvent::Hangup {
            reason: reason.clone(),
        }),
        AppEvent::DtmfReceived(d) => Some(CallEvent::Dtmf(*d)),
        AppEvent::RemotePtt(b) => Some(CallEvent::RemotePtt(*b)),
        AppEvent::TextReceived(t) => Some(CallEvent::Text(match t {
            astar_iax_core::OwnedTextEvent::Raw(s) => s.clone(),
            // Structured K-status: reconstruct its `K <src> <name> <keyed> <since>`
            // wire form (matching `TextEvent::encode`).
            astar_iax_core::OwnedTextEvent::KStatus {
                src,
                name,
                keyed,
                since,
            } => format!("K {src} {name} {} {since}", u8::from(*keyed)),
        })),
        AppEvent::ConnectionLost => Some(CallEvent::ConnectionLost),
        AppEvent::ConnectionRestored => Some(CallEvent::ConnectionRestored),
        // iax-85e7: RFC 5456 §6.3 call-progress. RINGING maps to the existing
        // public `CallEvent::Ringing` (previously never produced). PROCEEDING
        // and ANSWER stay on the internal AppEvent channel for now — ANSWER is
        // redundant with `Connected` (ACCEPT) on the servers we target (parrot/
        // ASL3 echo never send a separate CONTROL ANSWER), and there is no
        // CallEvent for PROCEEDING. Promote in a follow-up if the UX needs them.
        AppEvent::CallProgress(astar_iax_core::session::CallProgress::Ringing) => {
            Some(CallEvent::Ringing)
        }
        // PROCEEDING/ANSWER stay internal; voice goes to the speaker
        // out-of-band; IncomingCall is inbound-only (iax-8baf) and an outbound
        // Call leg never sees an offer.
        AppEvent::CallProgress(_)
        | AppEvent::VoiceReceived { .. }
        | AppEvent::IncomingCall { .. } => None,
    }
}

/// Media-clock timestamp ladder for outbound voice (iax-86d7). Every frame
/// is exactly 160 samples = 20 ms, so consecutive frames advance the media
/// clock by exactly 20 regardless of send-time jitter (resampler chunking,
/// loop-wake bunching). Re-anchor to the wall clock when the ladder and the
/// wall diverge by more than 80 ms — an unkey pause, or accumulated drift.
/// The peer's jitterbuffer schedules playout by these timestamps, so a clean
/// 20 ms ladder is what makes our audio reassemble smoothly.
///
/// `media_lead` (ms, iax-2733) is the deliberate amount by which the media
/// ladder is allowed to run AHEAD of the wall clock during a VOX pre-roll flush.
/// On key-up the mic lane drains its look-back ring (the buffered speech onset)
/// in one loop pass, racing `next` ~250 ms ahead of a barely-advanced wall clock.
/// Without compensation the >80 ms divergence would be read as drift and the
/// ladder would re-anchor backward mid-burst, corrupting the onset. The runtime
/// instead grows `media_lead` by 20 ms per pre-roll frame laid, so the ladder's
/// reference becomes `wall_ms + media_lead` and the intentional lead is never
/// seen as drift. `media_lead == 0` (pre-roll disabled OR no pre-roll this over)
/// makes the reference exactly `wall_ms`, i.e. byte-identical to the legacy
/// two-argument ladder — the non-negotiable no-regression path.
///
/// Returns `(ts, reanchored)`. `reanchored` is `true` when the ladder was
/// already running (`next` was `Some`) but an unkey gap / drift of >80 ms forced
/// it back to the wall clock — a TX-choppiness instrumentation signal (iax-5530):
/// a re-anchor is a timestamp discontinuity the peer's jitter buffer may resync
/// on. It does not affect the returned `ts`. (The first frame anchors with
/// `next == None`; that is initialization, not a re-anchor.) A deliberate
/// pre-roll lead must NOT count as a re-anchor — see the guardrail test.
fn voice_ts_ladder(next: &mut Option<u32>, wall_ms: u32, media_lead: u32) -> (u32, bool) {
    let wall_ref = wall_ms.wrapping_add(media_lead);
    let reanchored = matches!(*next, Some(t) if (i64::from(wall_ref) - i64::from(t)).abs() > 80);
    let ts = match *next {
        Some(t) if (i64::from(wall_ref) - i64::from(t)).abs() <= 80 => t,
        _ => wall_ref,
    };
    *next = Some(ts.wrapping_add(20));
    (ts, reanchored)
}

/// Observes wire frames in/out when an observer channel is installed.
/// Zero-cost (single `Option` check) when no observer is set.
struct Tracer {
    obs: Option<mpsc::Sender<crate::trace::TracedFrame>>,
    seq: u64,
    start: Instant,
}

impl Tracer {
    fn record(&mut self, dir: crate::trace::Direction, raw: &[u8]) {
        let Some(obs) = self.obs.as_ref() else {
            return;
        };
        let Ok(frame) = parse_lenient(raw) else {
            return;
        };
        let _ = obs.send(crate::trace::TracedFrame {
            seq: self.seq,
            dir,
            at_ms: u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX),
            summary: astar_iax_core::trace::summarize(&frame),
            raw: raw.to_vec(),
        });
        self.seq += 1;
    }
}

/// Upper bound on the mio poll timeout: one voice-frame interval. Keeps the
/// run-loop waking at least every 20 ms so outbound mic frames are drained
/// promptly and the voice ts ladder can't spuriously re-anchor mid-TX
/// (iax-2b05). The Waker still wakes the loop sooner for real work.
const MAX_POLL_TIMEOUT: Duration = Duration::from_millis(20);

/// The mio poll timeout for one loop iteration: time until the next timer, but
/// **capped at [`MAX_POLL_TIMEOUT`]** so a far timer (e.g. the keepalive) can't
/// let the loop sleep long enough to bunch outbound mic frames or drift
/// `wall_ms` >80 ms ahead of the voice ts ladder (which would spuriously
/// re-anchor it → a discontinuity the peer jitter buffer resyncs on, i.e. choppy
/// TX, iax-5530). A timer already due yields `0`; re-anchor after a genuine
/// PTT/VOX silence gap is unaffected.
fn poll_timeout(next_timer: Option<Instant>, now: Instant) -> Duration {
    match next_timer {
        Some(t) if t > now => (t - now).min(MAX_POLL_TIMEOUT),
        Some(_) => Duration::from_millis(0),
        None => MAX_POLL_TIMEOUT,
    }
}

/// Per-call thread loop. mio UDP socket + std mpsc command channel + a
/// vector-of-deadlines timer wheel. Single-thread, no async.
///
/// Promoted from `astar-conformance`'s `driver.rs` (`run_event_loop_until` +
/// `dispatch_actions`), with `AppEvent` harvesting wired to the `CallEvent`
/// channel and mic/speaker voice routing added.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value
)]
fn run_loop(
    mut poll: mio::Poll,
    waker: Arc<Waker>,
    net: Arc<dyn NetStack>,
    peer: SocketAddr,
    caller_id: String,
    dest: String,
    secret: String,
    profile: CallProfile,
    our_call: CallNo,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    event_tx: mpsc::Sender<CallEvent>,
    mic_rx: mpsc::Receiver<Vec<i16>>,
    spk_tx: mpsc::Sender<Vec<i16>>,
    rtt_micros: Arc<AtomicU32>,
    state: Arc<std::sync::atomic::AtomicU8>,
    frame_observer: Option<mpsc::Sender<crate::trace::TracedFrame>>,
    preroll_lead: Arc<AtomicU32>,
    tx_reanchors_total: Arc<AtomicU64>,
    format_bits: Arc<AtomicU32>,
    sample_rate: u32,
    remote_keyed: Arc<AtomicBool>,
) {
    use mio::{Events, Token};

    const SOCK: Token = Token(0);

    let Ok(mut sock) = net.bind("0.0.0.0:0".parse().unwrap()) else {
        return;
    };
    if sock.connect(peer).is_err() {
        return;
    }
    if sock.register(poll.registry(), SOCK, waker).is_err() {
        return;
    }

    // iax-3fca: real secret from the builder; empty string when unset.
    let creds = Credentials {
        username: caller_id,
        password: Arc::new(Secret::new(secret)),
        allowed_methods: AuthMethods::MD5,
    };
    let mut fsm = Fsm::new(creds, our_call).with_call_profile(profile);
    let mut rel = Reliability::new(our_call, ReliabilityConfig::default());
    let mut timers: Vec<(Instant, TimerKind)> = Vec::new();
    let mut buf = [0u8; 4096];

    let start = Instant::now();

    let mut tracer = Tracer {
        obs: frame_observer,
        seq: 0,
        start,
    };

    // RX codec edge (iax-31f7): warn once per call if a received voice payload
    // is undecodable for its format, then drop silently — never log-spam.
    let mut rx_decode_warned = false;

    // Rate-adapting codec edge (iax-4348): resamples bus-rate PCM ↔ the
    // negotiated wire rate. On an 8 kHz station (`sample_rate == 8000`) no
    // resampler is ever built and both paths reduce to the pure encode/decode.
    let mut edge = crate::codec_edge::EdgeAudio::new(sample_rate);

    // Kick the call.
    let actions = fsm.handle(Event::App(AppCommand::StartCall {
        dest,
        now: Instant::now(),
    }));
    if dispatch_actions(
        actions,
        sock.as_ref(),
        &mut rel,
        &mut timers,
        &spk_tx,
        &event_tx,
        &state,
        &remote_keyed,
        &mut tracer,
        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
        &mut rx_decode_warned,
        &mut edge,
    ) {
        return;
    }

    let mut events = Events::with_capacity(8);
    // Media-clock timestamp ladder for outbound voice (iax-86d7): each
    // 160-byte frame advances exactly 20 ms; re-anchored to the wall clock
    // after an unkey gap / drift. `None` until the first frame.
    let mut next_voice_ts: Option<u32> = None;
    // VOX pre-roll lead state (iax-2733). On key-up the mic lane drains its
    // look-back ring AHEAD of the live stream and bumps `preroll_lead` by the
    // frame count. We drain that signal per loop pass into `preroll_remaining`
    // (frames still to be timestamped as pre-roll) and grow `media_lead` (ms)
    // by 20 per pre-roll frame so `wall_ms + media_lead` tracks the ladder
    // racing ahead — the deliberate lead is NOT counted as drift, so the burst
    // lays a clean monotonic ladder with ZERO re-anchors. `media_lead` resets
    // to 0 at the start of each new over (next pre-roll burst). With pre-roll
    // disabled, `preroll_lead` never fires, `media_lead` stays 0, and the
    // ladder is byte-identical to today (the non-negotiable guardrail).
    let mut media_lead: u32 = 0;
    let mut preroll_remaining: u32 = 0;
    // TX-audio pacing instrumentation (iax-5530): per-window peaks to confirm
    // outbound choppiness (bunching / ts re-anchors) when it recurs. Emitted on
    // target "astar_iax::tx_audio" every ~5 s of an active TX. Cheap (a few
    // integers + an Instant compare per frame); no effect on the wire.
    let mut tx_frames = 0u64; // frames sent this window
    let mut tx_reanchors = 0u64; // ts re-anchors this window
    let mut tx_max_burst = 0usize; // largest mic_rx drain in one loop pass
    let mut tx_max_gap_ms = 0u64; // largest inter-frame send spacing
    let mut tx_last_send: Option<Instant> = None;
    let mut tx_last_log = Instant::now();
    loop {
        let now = Instant::now();
        let next_timer = timers.iter().map(|(at, _)| *at).min();
        let timeout = poll_timeout(next_timer, now);

        if let Err(e) = poll.poll(&mut events, Some(timeout)) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        // Drain commands woken via the waker (Token(1)).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RuntimeCommand::App(app) => {
                    let actions = fsm.handle(Event::App(app));
                    if dispatch_actions(
                        actions,
                        sock.as_ref(),
                        &mut rel,
                        &mut timers,
                        &spk_tx,
                        &event_tx,
                        &state,
                        &remote_keyed,
                        &mut tracer,
                        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                        &mut rx_decode_warned,
                        &mut edge,
                    ) {
                        return;
                    }
                }
                RuntimeCommand::Shutdown => {
                    let actions = fsm.handle(Event::App(AppCommand::Hangup {
                        cause: None,
                        now: Instant::now(),
                    }));
                    let _ = dispatch_actions(
                        actions,
                        sock.as_ref(),
                        &mut rel,
                        &mut timers,
                        &spk_tx,
                        &event_tx,
                        &state,
                        &remote_keyed,
                        &mut tracer,
                        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                        &mut rx_decode_warned,
                        &mut edge,
                    );
                    return;
                }
            }
        }

        // Drain inbound datagrams. Attempt a non-blocking drain on ANY wakeup
        // (not just when the socket token fired): for OS UDP this is
        // behaviorally identical (an empty socket returns `WouldBlock`
        // immediately), and it is what a waker-driven [`LinkSocket`] impl
        // (whose readiness arrives via the waker, not a token) requires
        // (iax-b6f5).
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    let bytes = buf[..n].to_vec();
                    tracer.record(crate::trace::Direction::In, &bytes);
                    let Ok(frame) = parse_lenient(&bytes) else {
                        continue;
                    };
                    let now = Instant::now();
                    match rel.on_frame_in(frame, now) {
                        RxOutcome::Deliver { frame, send_ack } => {
                            if let Some(ack) = send_ack {
                                let _ = sock.send(&ack);
                            }
                            let pre = fsm.state().name();
                            let actions = fsm.handle(Event::Frame { frame, now });
                            let post = fsm.state().name();
                            // RFC 5456 §8.6: the CALLTOKEN handshake replaces the
                            // initial NEW; sequence numbers restart from 0. The FSM
                            // signals this by moving NewSent -> NewResent in response
                            // to CALLTOKEN. Reset before dispatching so the resent NEW
                            // gets oseqno=0. Mirrors driver.rs.
                            if pre == "NewSent" && post == "NewResent" {
                                rel.reset();
                            }
                            if dispatch_actions(
                                actions,
                                sock.as_ref(),
                                &mut rel,
                                &mut timers,
                                &spk_tx,
                                &event_tx,
                                &state,
                                &remote_keyed,
                                &mut tracer,
                                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                                &mut rx_decode_warned,
                                &mut edge,
                            ) {
                                return;
                            }
                        }
                        RxOutcome::Consumed => {}
                        RxOutcome::Duplicate { resend_ack } => {
                            if let Some(b) = resend_ack {
                                let _ = sock.send(&b);
                            }
                        }
                        RxOutcome::Vnak(iseqno) => {
                            // RFC 5456 §6.9.3: the peer wants a resend
                            // from `iseqno`. iax-a307: answer it — this
                            // used to map to DeliveryFailed and killed
                            // the call on a VNAK.
                            for bytes in rel.resend_from(iseqno) {
                                let _ = sock.send(&bytes);
                            }
                        }
                        RxOutcome::GaveUp { oseqno } => {
                            let actions = fsm.handle(Event::DeliveryFailed { oseqno });
                            if dispatch_actions(
                                actions,
                                sock.as_ref(),
                                &mut rel,
                                &mut timers,
                                &spk_tx,
                                &event_tx,
                                &state,
                                &remote_keyed,
                                &mut tracer,
                                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                                &mut rx_decode_warned,
                                &mut edge,
                            ) {
                                return;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return,
            }
        }

        // Fire expired timers.
        let now = Instant::now();
        let mut fired = Vec::new();
        timers.retain(|(at, kind)| {
            if *at <= now {
                fired.push(*kind);
                false
            } else {
                true
            }
        });
        for kind in fired {
            let actions = fsm.handle(Event::Timer { kind, now });
            if dispatch_actions(
                actions,
                sock.as_ref(),
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut tracer,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                &mut rx_decode_warned,
                &mut edge,
            ) {
                return;
            }
        }

        // Pump captured mic audio (fixed 160-sample / 20 ms PCM frames) out as
        // voice frames on the media-clock ladder (iax-86d7), encoding to the
        // negotiated wire format at the edge.
        //
        // VOX pre-roll (iax-2733): the mic lane bumps `preroll_lead` by the
        // number of look-back frames it flushed AHEAD of the live stream on
        // key-up. Read-and-clear it here. A non-zero value is the start of a new
        // over: reset `media_lead` to 0 and arm `preroll_remaining` so the burst
        // (and the live frames after it) lay one clean ladder. When pre-roll is
        // disabled this never fires, so `media_lead` stays 0 and the ladder is
        // byte-identical to today.
        let lead = preroll_lead.swap(0, Ordering::Relaxed);
        if lead > 0 {
            media_lead = 0;
            preroll_remaining = lead;
        }
        let mut burst = 0usize;
        while let Ok(pcm) = mic_rx.try_recv() {
            // TX instrumentation (iax-5530): drain-burst size (bunching) +
            // inter-frame send spacing.
            burst += 1;
            let send_now = Instant::now();
            if let Some(prev) = tx_last_send {
                #[allow(clippy::cast_possible_truncation)]
                let gap = send_now.duration_since(prev).as_millis() as u64;
                tx_max_gap_ms = tx_max_gap_ms.max(gap);
            }
            tx_last_send = Some(send_now);
            #[allow(clippy::cast_possible_truncation)]
            let wall_ms = start.elapsed().as_millis() as u32;
            let (ts, reanchored) = voice_ts_ladder(&mut next_voice_ts, wall_ms, media_lead);
            let is_preroll = preroll_remaining > 0;
            if is_preroll {
                // Grow the lead 20 ms per pre-roll frame so the next frame's
                // `wall_ms + media_lead` tracks the ladder racing ahead.
                media_lead = media_lead.saturating_add(20);
                preroll_remaining -= 1;
            }
            tx_frames += 1;
            // A deliberate pre-roll lead must NOT inflate the choppy-TX re-anchor
            // counter (iax-5530); with the media_lead fix it shouldn't re-anchor
            // anyway, but never charge a pre-roll frame to `tx_reanchors`.
            if reanchored && !is_preroll {
                tx_reanchors += 1;
                // iax-9e55: also bump the cumulative cell read by the snapshot.
                tx_reanchors_total.fetch_add(1, Ordering::Relaxed);
            }
            // Outbound codec edge (iax-31f7 / iax-4348): the bus carries PCM;
            // encode into the negotiated format here, resampling to the wire rate
            // first when it differs from the bus rate. Default policy negotiates
            // G711U at 8 kHz, so the wire byte stream is unchanged.
            let format = fsm.negotiated_format().unwrap_or(VoiceFormat::G711U);
            let payload = edge.encode(format, &pcm);
            let actions = fsm.handle(Event::App(AppCommand::SendVoice {
                format,
                payload,
                ts,
            }));
            if dispatch_actions(
                actions,
                sock.as_ref(),
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut tracer,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                &mut rx_decode_warned,
                &mut edge,
            ) {
                return;
            }
        }
        tx_max_burst = tx_max_burst.max(burst);

        // Drive retransmits / give-ups.
        let tick = rel.tick(Instant::now());
        for bytes in tick.retransmit {
            let _ = sock.send(&bytes);
        }
        for oseqno in tick.gave_up {
            let actions = fsm.handle(Event::DeliveryFailed { oseqno });
            if dispatch_actions(
                actions,
                sock.as_ref(),
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut tracer,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                &mut rx_decode_warned,
                &mut edge,
            ) {
                return;
            }
        }

        // iax-a307: publish the smoothed RTT for Call::rtt(). u32::MAX is
        // the no-sample sentinel; a real RTT that overflows u32 micros
        // (~71 min) clamps just below it.
        let micros = fsm.rtt().map_or(u32::MAX, |d| {
            u32::try_from(d.as_micros()).unwrap_or(u32::MAX - 1)
        });
        rtt_micros.store(micros, Ordering::Relaxed);

        // iax-31f7: publish the negotiated codec once per event-handling pass
        // for Call::snapshot. `0` = not yet negotiated (VoiceFormat::from_u32
        // maps 0 to None).
        format_bits.store(
            fsm.negotiated_format().map_or(0, VoiceFormat::as_u32),
            Ordering::Relaxed,
        );

        // TX-audio pacing summary (iax-5530): emit per ~5 s window while TX is
        // active. `max_burst > 1` = mic frames bunched (run-loop fell behind);
        // `reanchors > 0` = ts discontinuities; `max_send_gap_ms` >> 20 = irregular
        // send cadence. Steady clean TX shows ~250 frames, max_burst 1, 0 reanchors.
        if tx_frames > 0 && tx_last_log.elapsed() >= Duration::from_secs(5) {
            tracing::debug!(
                target: "astar_iax::tx_audio",
                frames = tx_frames,
                reanchors = tx_reanchors,
                max_burst = tx_max_burst,
                max_send_gap_ms = tx_max_gap_ms,
                "TX pacing (5s window)"
            );
            tx_frames = 0;
            tx_reanchors = 0;
            tx_max_burst = 0;
            tx_max_gap_ms = 0;
            tx_last_log = Instant::now();
        }
    }
}

/// Apply the FSM's actions: send frames, arm/cancel timers, route voice to the
/// speaker, and forward lifecycle [`AppEvent`]s to the [`CallEvent`] channel.
///
/// Returns `true` if the call has terminated (a `Hangup`/`Disconnected` event
/// was surfaced), signalling the caller to break the loop.
///
/// Mirrors `driver.rs::dispatch_actions` (`SendReliable` / `SendUnreliable` /
/// `SetPeerCall` / timer arming) with `AppEvent` harvesting added.
#[allow(clippy::too_many_arguments)]
fn dispatch_actions(
    actions: smallvec::SmallVec<[Action; 4]>,
    sock: &dyn LinkSocket,
    rel: &mut Reliability,
    timers: &mut Vec<(Instant, TimerKind)>,
    spk_tx: &mpsc::Sender<Vec<i16>>,
    event_tx: &mpsc::Sender<CallEvent>,
    state: &Arc<std::sync::atomic::AtomicU8>,
    remote_keyed: &Arc<AtomicBool>,
    tracer: &mut Tracer,
    negotiated_format: VoiceFormat,
    rx_decode_warned: &mut bool,
    edge: &mut crate::codec_edge::EdgeAudio,
) -> bool {
    let now = Instant::now();
    let mut terminated = false;
    for action in actions {
        match action {
            Action::SendReliable(frame) => {
                let bytes = rel.enqueue(frame, now);
                let _ = sock.send(&bytes);
                tracer.record(crate::trace::Direction::Out, &bytes);
            }
            Action::SendUnreliable(bytes) => {
                let _ = sock.send(&bytes);
                tracer.record(crate::trace::Direction::Out, &bytes);
            }
            Action::SetPeerCall(peer) => {
                // iax-e402: FSM learned the peer's chosen scallno; plumb it down
                // so Reliability stamps it into dest_call on subsequent enqueues.
                rel.set_peer_call(peer);
            }
            Action::SetTimer(kind, dur) => {
                timers.retain(|(_, k)| *k != kind);
                timers.push((now + dur, kind));
            }
            Action::CancelTimer(kind) => {
                timers.retain(|(_, k)| *k != kind);
            }
            Action::AppEvent(ev) => {
                // Keystone: publish coarse connection-state for Call::snapshot.
                match &ev {
                    AppEvent::Connected { .. } => {
                        state.store(crate::call::STATE_ACTIVE, Ordering::Relaxed);
                    }
                    AppEvent::Disconnected { .. } => {
                        state.store(crate::call::STATE_HUNGUP, Ordering::Relaxed);
                    }
                    // iax-feab: publish the peer's PTT-key state BEFORE
                    // translating to a CallEvent, so a parrot conference
                    // member's key gate (Call::remote_keyed_handle) is
                    // current the instant the event is observable.
                    AppEvent::RemotePtt(b) => {
                        remote_keyed.store(*b, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if let AppEvent::VoiceReceived {
                    format, payload, ..
                } = &ev
                {
                    // RX codec edge (iax-31f7 / iax-4348): decode the received
                    // payload on its OWN format into bus-rate PCM (resampling if
                    // the wire rate differs). Undecodable frames are dropped (warn
                    // once).
                    match edge.decode(*format, payload) {
                        Some(pcm) => {
                            let _ = spk_tx.send(pcm);
                        }
                        None if !*rx_decode_warned => {
                            *rx_decode_warned = true;
                            tracing::warn!(
                                ?format,
                                len = payload.len(),
                                "dropping undecodable RX voice"
                            );
                        }
                        None => {}
                    }
                } else if let Some(ce) = translate(&ev, negotiated_format) {
                    let is_hangup = matches!(ce, CallEvent::Hangup { .. });
                    let _ = event_tx.send(ce);
                    if is_hangup {
                        terminated = true;
                    }
                }
            }
            Action::LogInvalid { reason } => {
                tracing::debug!(target: "astar_iax::client", reason);
            }
            // iax-8baf: inbound CALLTOKEN resend resets the reliability ladder.
            // The outbound FSM doesn't emit this today; reset is the correct
            // semantics if/when it does (Phase F wires the inbound runtime).
            Action::ResetReliability => {
                rel.reset();
            }
        }
    }
    terminated
}

#[cfg(test)]
mod poll_timeout_tests {
    use super::{MAX_POLL_TIMEOUT, poll_timeout};
    use std::time::{Duration, Instant};

    #[test]
    fn caps_far_timer_and_idle_to_one_frame() {
        let now = Instant::now();
        // No timer pending → the cap.
        assert_eq!(poll_timeout(None, now), MAX_POLL_TIMEOUT);
        // A far timer (1 s out, e.g. keepalive) → capped to the cap, NOT 1 s.
        let far = now + Duration::from_secs(1);
        assert_eq!(poll_timeout(Some(far), now), MAX_POLL_TIMEOUT);
    }

    #[test]
    fn near_timer_kept_and_due_timer_is_zero() {
        let now = Instant::now();
        // A near timer (5 ms) is under the cap → kept as-is.
        let near = now + Duration::from_millis(5);
        assert_eq!(poll_timeout(Some(near), now), Duration::from_millis(5));
        // A timer already due → 0 (fire it this iteration).
        let past = now.checked_sub(Duration::from_millis(1)).unwrap();
        assert_eq!(poll_timeout(Some(past), now), Duration::ZERO);
    }
}

#[cfg(test)]
mod voice_ts_tests {
    use super::voice_ts_ladder;

    #[test]
    fn consecutive_frames_step_exactly_20ms_despite_send_jitter() {
        let mut next = None;
        // First frame anchors at the wall clock — initialization, not a re-anchor.
        assert_eq!(voice_ts_ladder(&mut next, 1000, 0), (1000, false));
        // Bunched sends (same wall instant) and late sends (within 80 ms)
        // stay on the ladder: 1020, 1040, 1060 — no re-anchor.
        assert_eq!(voice_ts_ladder(&mut next, 1000, 0), (1020, false));
        assert_eq!(voice_ts_ladder(&mut next, 1055, 0), (1040, false));
        assert_eq!(voice_ts_ladder(&mut next, 1070, 0), (1060, false));
    }

    #[test]
    fn unkey_gap_reanchors_to_wall_clock_and_flags_it() {
        let mut next = None;
        assert_eq!(voice_ts_ladder(&mut next, 1000, 0), (1000, false));
        // 2 s pause (unkeyed) → wall far ahead of the ladder → re-anchor (flagged).
        assert_eq!(voice_ts_ladder(&mut next, 3000, 0), (3000, true));
        // Back on the ladder afterwards — not a re-anchor.
        assert_eq!(voice_ts_ladder(&mut next, 3010, 0), (3020, false));
    }

    /// NON-NEGOTIABLE no-regression guardrail (iax-2733): with `media_lead == 0`
    /// (pre-roll disabled OR no pre-roll this over) the ladder MUST be
    /// byte-identical to the pre-iax-2733 two-argument ladder — same first-frame
    /// anchor, same +20 step, same >80 ms re-anchor edge (iax-86d7/2b05/5530).
    /// This pins the legacy behavior so a future change to the offset-aware
    /// reference cannot silently perturb the choppy-TX-sensitive lead=0 path.
    #[test]
    fn media_lead_zero_is_byte_identical_to_the_legacy_ladder() {
        // The legacy ladder, inlined exactly as it was before the `media_lead`
        // parameter existed (commit 87e5d66): wall_ms is the only reference.
        fn legacy_ladder(next: &mut Option<u32>, wall_ms: u32) -> (u32, bool) {
            let reanchored =
                matches!(*next, Some(t) if (i64::from(wall_ms) - i64::from(t)).abs() > 80);
            let ts = match *next {
                Some(t) if (i64::from(wall_ms) - i64::from(t)).abs() <= 80 => t,
                _ => wall_ms,
            };
            *next = Some(ts.wrapping_add(20));
            (ts, reanchored)
        }
        // Sweep a wall-clock trace that exercises every branch: first-frame
        // anchor, on-ladder steps, jitter within 80 ms, a >80 ms unkey gap
        // re-anchor, and a backward drift. Run both ladders frame-for-frame and
        // assert the (ts, reanchored) tuple AND the residual `next` cell match.
        let trace = [
            1000, 1000, 1010, 1055, 1070, 1075, 3000, 3010, 3030, 3008, 3050, 5000, 5020,
        ];
        let mut legacy_next = None;
        let mut lead0_next = None;
        for wall in trace {
            let legacy = legacy_ladder(&mut legacy_next, wall);
            let lead0 = voice_ts_ladder(&mut lead0_next, wall, 0);
            assert_eq!(lead0, legacy, "ts/reanchor diverged at wall={wall}");
            assert_eq!(
                lead0_next, legacy_next,
                "ladder cell diverged at wall={wall}"
            );
        }
    }

    /// A pre-roll flush (iax-2733) lays the buffered onset frames AHEAD of the
    /// live stream. The runtime feeds the ladder a per-frame `media_lead` that
    /// grows +20 ms per pre-roll frame, so `wall_ms + media_lead` tracks the
    /// racing ladder and the deliberate lead is NEVER mistaken for >80 ms drift.
    /// Result: a clean monotonic 20 ms ladder across the burst with ZERO
    /// re-anchors (the lead is deliberate, not choppy TX — it must not inflate
    /// `tx_reanchors`, iax-5530).
    #[test]
    fn preroll_lead_lays_a_monotonic_20ms_ladder_with_zero_reanchors() {
        let mut next = None;
        // 13 pre-roll frames (250 ms) all flush in ONE loop pass: wall is
        // ~constant (say 1000) while `media_lead` advances 0,20,40,…240 — one
        // per pre-roll frame already laid. The runtime applies the lead BEFORE
        // computing each frame's ts.
        let wall = 1000;
        let mut media_lead = 0u32;
        let mut expected_ts = 1000;
        for _ in 0..13 {
            let (ts, reanchored) = voice_ts_ladder(&mut next, wall, media_lead);
            assert_eq!(ts, expected_ts, "pre-roll ts off ladder");
            assert!(!reanchored, "pre-roll lead spuriously re-anchored");
            media_lead += 20;
            expected_ts += 20;
        }
        // Live frames continue seamlessly: media_lead stays fixed at 260 and the
        // wall clock catches up ~20 ms per frame. The ladder keeps stepping +20
        // with no re-anchor — the burst and the live stream are one ladder.
        assert_eq!(media_lead, 260);
        for step in 0..4 {
            let live_wall = 1000 + 20 * step; // wall advances ~20 ms / live frame
            let (ts, reanchored) = voice_ts_ladder(&mut next, live_wall, media_lead);
            assert_eq!(ts, expected_ts, "live ts off ladder after pre-roll");
            assert!(
                !reanchored,
                "live frame after pre-roll spuriously re-anchored"
            );
            expected_ts += 20;
        }
    }
}
