// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Inbound-call `Listener`: a shared UDP socket demuxes peer-originated NEW
//! frames by `dest_call`, spawns a per-leg inbound [`Fsm`] + [`Reliability`]
//! runtime, and surfaces [`IncomingCall`]s to the application (iax-8baf).
//!
//! Architecture (mirrors [`crate::registration`] / [`crate::manager`]): one
//! blocking `mio` thread owns the shared socket (`Arc<dyn LinkSocket>`, bound
//! from the transport seam's [`NetStack`], iax-b6f5) and does the demux +
//! inline `DoS` hardening (`INVAL` / `POKE`→`PONG` / malformed-or-unknown `REJECT`). Each
//! accepted NEW gets its own per-leg thread driving the inbound FSM; the leg
//! sends via the shared socket with `send_to(peer_addr)` (the FSM stays
//! address-implicit — the Listener supplies `peer_addr` per leg).
//!
//! VENDOR-NEUTRAL: defaults are generic `IAX2` (auth=Optional, CALLTOKEN
//! Never-in-debug / Always-in-release, `AppDecide`, no auto-answer). `AllStar`'s
//! `auth=Required` / `CALLTOKEN=Always` / echo node belong to iax-6461, NOT
//! here.
//!
//! SECRET-FREE: the per-username credentials map is supplied at runtime, never
//! logged or exported; auth uses constant-time compares (core `md5_verify`).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::{Frame, OwnedFrame, OwnedFullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::CodecPolicy;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::{CallNo, CallNoAllocator};
use astar_iax_core::session::fsm::{
    Action, AppCommand, AppEvent, Event, Fsm, InboundPolicy, IncomingOffer, SessionState, TimerKind,
};
use astar_iax_core::session::reliability::{Reliability, ReliabilityConfig, RxOutcome};
use astar_iax_core::subclass::{FrameType, IaxCommand};
use mio::{Events, Poll, Token, Waker};
use rand::RngCore;

use crate::call::{Call, CallEvent, CallId, CallSnapshotMode, STATE_ACTIVE, STATE_HUNGUP};
use crate::error::IaxError;
use crate::runtime::RuntimeCommand;
use crate::transport::{LinkSocket, NetStack, OsNetStack};

// --- Policy types (Task 14) ------------------------------------------------

/// When the Listener demands a CALLTOKEN round-trip before proceeding with an
/// inbound NEW (RFC 5456 §8.6 anti-spoof). Vendor-neutral default: `Never` in
/// debug builds, `Always` in release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingCallTokenPolicy {
    /// Never demand a CALLTOKEN (test/dev convenience).
    Never,
    /// Demand a CALLTOKEN only from sources not in a configured allowlist; this
    /// MVP treats it as "demand" at the leg boundary (full allowlist is iax-b764).
    OpportunisticBackend,
    /// Always demand a CALLTOKEN round-trip.
    Always,
}

/// Whether the Listener challenges inbound NEWs for MD5 authentication.
/// Vendor-neutral default: `Optional` (challenge only when the peer offered a
/// username we hold credentials for). `Required` is iax-6461 territory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingAuthPolicy {
    /// Every inbound NEW must authenticate (unknown user → REJECT).
    Required,
    /// Challenge only if the peer's username maps to a held credential.
    Optional,
    /// Never challenge (accept anonymous).
    Off,
}

/// How the accept/answer decision is made once a NEW passes auth/calltoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingDecisionPolicy {
    /// Surface an [`IncomingCall`]; wait for the app to answer/reject.
    AppDecide,
    /// Auto-answer every call.
    AutoAccept,
    /// Auto-reject every call.
    AutoReject,
}

/// Dynamic inbound credential lookup: offered username → plaintext secret
/// (`""` = no credential). See [`IncomingCallPolicy::credential_resolver`].
pub type CredentialResolver = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Vendor-neutral inbound-call policy. Lowers to the core [`InboundPolicy`] the
/// pure FSM holds (the booleans), keeping the secret map and the runtime enums
/// out of `astar-iax-core`.
///
/// SECRET-FREE: `credentials` is never logged or exported; [`Secret`]'s `Debug`
/// already prints `Secret(***)`, so the derived `Debug` here cannot leak the
/// password (usernames are not secret).
#[allow(clippy::struct_excessive_bools)]
pub struct IncomingCallPolicy {
    /// CALLTOKEN demand policy.
    pub calltoken: IncomingCallTokenPolicy,
    /// MD5 auth policy.
    pub auth: IncomingAuthPolicy,
    /// Accept/answer decision policy.
    pub decision: IncomingDecisionPolicy,
    /// Skip the app decision and ANSWER immediately once accepted.
    pub auto_answer: bool,
    /// How long to wait for the app's answer/reject before auto-rejecting.
    pub accept_decision_timeout: Duration,
    /// Permit PLAINTEXT in AUTHMETHODS (we never advertise it regardless; this
    /// is a forward knob and stays `false` by default).
    pub allow_plaintext: bool,
    /// Per-username credential map. NEVER logged / exported. `Arc<Secret>` so
    /// the policy (and this map) can be cloned without copying any plaintext
    /// out of its zeroizing wrapper (iax-f755 L5).
    pub credentials: HashMap<String, Arc<Secret>>,
    /// Dynamic credential fallback consulted when `credentials` misses (iax-99cd).
    /// Given the offered username, returns the plaintext secret, or `""` for
    /// "no credential". Lets a runtime store (e.g. the node's `SecretProvider`
    /// fed by `ALLSTAR_PEER_*` env + `POST /secrets`) authenticate inbound calls
    /// without pre-populating the static map. NEVER logged — its [`fmt::Debug`]
    /// prints `<resolver>` only. `None` = no fallback (byte-identical to before).
    pub credential_resolver: Option<CredentialResolver>,
    /// Codec negotiation policy for accepted calls (iax-31f7).
    pub codec_policy: CodecPolicy,
}

impl std::fmt::Debug for IncomingCallPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print the credential *count* only — never the usernames or secrets.
        f.debug_struct("IncomingCallPolicy")
            .field("calltoken", &self.calltoken)
            .field("auth", &self.auth)
            .field("decision", &self.decision)
            .field("auto_answer", &self.auto_answer)
            .field("accept_decision_timeout", &self.accept_decision_timeout)
            .field("allow_plaintext", &self.allow_plaintext)
            .field(
                "credentials",
                &format_args!("<{} entries>", self.credentials.len()),
            )
            .field(
                "credential_resolver",
                &format_args!(
                    "{}",
                    if self.credential_resolver.is_some() {
                        "<resolver>"
                    } else {
                        "None"
                    }
                ),
            )
            .field("codec_policy", &self.codec_policy)
            .finish()
    }
}

impl Default for IncomingCallPolicy {
    fn default() -> Self {
        Self {
            calltoken: if cfg!(debug_assertions) {
                IncomingCallTokenPolicy::Never
            } else {
                IncomingCallTokenPolicy::Always
            },
            auth: IncomingAuthPolicy::Optional,
            decision: IncomingDecisionPolicy::AppDecide,
            auto_answer: false,
            accept_decision_timeout: Duration::from_secs(30),
            allow_plaintext: false,
            credentials: HashMap::new(),
            credential_resolver: None,
            codec_policy: CodecPolicy::default(),
        }
    }
}

impl IncomingCallPolicy {
    /// Lower to the core [`InboundPolicy`] the pure FSM consumes. The runtime
    /// enums collapse to the FSM's policy booleans. `OpportunisticBackend`
    /// lowers to `calltoken_required = true` for the MVP (per-source allowlist
    /// bypass is iax-b764).
    #[must_use]
    pub fn lower(&self) -> InboundPolicy {
        InboundPolicy {
            calltoken_required: matches!(
                self.calltoken,
                IncomingCallTokenPolicy::Always | IncomingCallTokenPolicy::OpportunisticBackend
            ),
            auth_required: matches!(self.auth, IncomingAuthPolicy::Required),
            auto_answer: self.auto_answer
                || matches!(self.decision, IncomingDecisionPolicy::AutoAccept),
            decision_is_app: matches!(self.decision, IncomingDecisionPolicy::AppDecide),
            accept_decision_timeout: self.accept_decision_timeout,
            codec_policy: self.codec_policy,
        }
    }
}

// --- Pure demux peek (Task 15) ---------------------------------------------

/// The low 15 bits of bytes 2-3 of a full frame, i.e. the `dest_call` the
/// datagram is addressed to. `None` for mini frames (top bit of byte 0 clear)
/// and for buffers too short to contain a full-frame header prefix.
///
/// Pure — no allocation, no socket — so the demux is unit-testable and
/// fuzzable (Task 19) without binding a port.
#[must_use]
pub fn peek_dest_call(buf: &[u8]) -> Option<u16> {
    if buf.len() < 4 {
        return None;
    }
    // Full frame iff the high bit of byte 0 (IAX_FLAG_FULL) is set.
    if buf[0] & 0x80 == 0 {
        return None;
    }
    Some(u16::from_be_bytes([buf[2], buf[3]]) & 0x7fff)
}

/// Whether the datagram is a full-frame IAX `NEW` command. Cheap parse via
/// `parse_lenient`; `false` on any parse error or non-NEW frame.
#[must_use]
pub fn is_new(buf: &[u8]) -> bool {
    matches!(
        parse_lenient(buf),
        Ok(Frame::Full(f)) if f.subclass == Subclass::Iax(IaxCommand::New)
    )
}

// --- Inline frame helpers (no Reliability; pre-spawn / DoS replies) ---------

/// Encode an owned full frame to wire bytes for an inline (unreliable) send.
fn encode_inline(frame: &OwnedFullFrame) -> Option<Vec<u8>> {
    let owned = OwnedFrame::Full(frame.clone());
    let borrowed = owned.as_frame().ok()?;
    let mut bytes = Vec::new();
    encode(&borrowed, &mut bytes).ok()?;
    Some(bytes)
}

/// Build an inline REJECT (CAUSE IE) addressed to `peer_call`, `our_call=0`
/// (we never allocated a leg). Used for malformed/unknown-user/pool-exhaustion.
fn inline_reject(peer_call: u16, cause: &str) -> Option<Vec<u8>> {
    let ies = Ies {
        cause: Some(cause),
        ..Ies::empty()
    };
    let frame = OwnedFullFrame::with_ies(
        0,
        peer_call,
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Reject),
        &ies,
    )
    .ok()?;
    encode_inline(&frame)
}

/// Build an inline INVAL for an unknown `dest_call` (R-6.9-02). `our_call=0`.
fn inline_inval(peer_call: u16) -> Option<Vec<u8>> {
    let frame = OwnedFullFrame::with_ies(
        0,
        peer_call,
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Inval),
        &Ies::empty(),
    )
    .ok()?;
    encode_inline(&frame)
}

/// Build an inline PONG reply to a POKE (no per-call state needed).
fn inline_pong(peer_call: u16) -> Option<Vec<u8>> {
    let frame = OwnedFullFrame::with_ies(
        0,
        peer_call,
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Pong),
        &Ies::empty(),
    )
    .ok()?;
    encode_inline(&frame)
}

// --- Per-leg runtime (Task 16/18) ------------------------------------------

/// What the Listener forwards to a leg's frame channel: the raw datagram plus
/// the source address (for the per-leg source-addr pin, R-10.0-07).
type LegDatagram = (Vec<u8>, SocketAddr);

/// Parameters for spawning an inbound leg.
struct LegSpawn {
    sock: Arc<dyn LinkSocket>,
    peer_addr: SocketAddr,
    our_call: CallNo,
    peer_call: CallNo,
    offer: IncomingOffer,
    creds: Credentials,
    policy: InboundPolicy,
    token: [u8; 16],
    challenge_hex: String,
    poll: Poll,
    frame_rx: mpsc::Receiver<LegDatagram>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    event_tx: mpsc::Sender<CallEvent>,
    mic_rx: mpsc::Receiver<Vec<i16>>,
    spk_tx: mpsc::Sender<Vec<i16>>,
    rtt_micros: Arc<AtomicU32>,
    state: Arc<AtomicU8>,
    format_bits: Arc<AtomicU32>,
    /// Station bus sample rate (iax-4348) = the listener policy's codec cap. The
    /// leg's codec edge resamples between it and the negotiated wire rate.
    sample_rate: u32,
    done_tx: mpsc::Sender<CallNo>,
    /// Live remote-PTT-key cell (iax-feab parrot mode): stored into whenever
    /// `AppEvent::RemotePtt` fires, mirroring `state`. Carried into
    /// `Call::new_inbound` alongside `rx_source`/`tx_sender`.
    remote_keyed: Arc<AtomicBool>,
}

/// Per-leg blocking loop. Owns the inbound `Fsm` + `Reliability`; receives
/// demuxed datagrams over `frame_rx` (the shared socket is read by the
/// Listener) and sends via the shared socket with `send_to(peer_addr)` (the FSM
/// stays address-implicit). Mirrors `client::run_loop`'s frame/timer/tick shape.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn leg_run_loop(p: LegSpawn) {
    let LegSpawn {
        sock,
        peer_addr,
        our_call,
        peer_call,
        offer,
        creds,
        policy,
        token,
        challenge_hex,
        mut poll,
        frame_rx,
        cmd_rx,
        event_tx,
        mic_rx,
        spk_tx,
        rtt_micros,
        state,
        format_bits,
        sample_rate,
        done_tx,
        remote_keyed,
    } = p;

    let now = Instant::now();
    let mut fsm =
        Fsm::for_inbound(creds, our_call, peer_call, offer, now).with_inbound_policy(policy);
    fsm.seed_entropy(token, challenge_hex);
    let mut rel = Reliability::new(our_call, ReliabilityConfig::default());
    rel.set_peer_call(peer_call);
    let mut timers: Vec<(Instant, TimerKind)> = Vec::new();
    let mut pending_answer_oseqno: Option<u8> = None;
    // iax-7bdc: the app's answer/reject can land while the calltoken/auth
    // handshake is still in flight (the offer surfaces at spawn; over the
    // internet the token echo takes one RTT). The FSM drops those commands
    // outside AcceptSent, so park the decision here and replay it the moment
    // the handshake reaches AcceptSent.
    let mut parked_decision: Option<AppCommand> = None;
    let mut next_voice_ts: u32 = 0;
    // RX codec edge (iax-31f7): warn once per leg on an undecodable RX payload.
    let mut rx_decode_warned = false;
    // Rate-adapting codec edge (iax-4348): resamples bus-rate PCM ↔ the
    // negotiated wire rate. `sample_rate == 8000` builds no resampler and the
    // edge is byte-identical to the pure encode/decode.
    let mut edge = crate::codec_edge::EdgeAudio::new(sample_rate);
    let start = Instant::now();

    // Kick: produce NewReceived's first actions (the NEW was consumed for demux).
    {
        let mut ctx = leg_ctx(
            sock.as_ref(),
            peer_addr,
            &mut rel,
            &mut timers,
            &spk_tx,
            &event_tx,
            &state,
            &remote_keyed,
            &mut pending_answer_oseqno,
            &mut rx_decode_warned,
        );
        let actions = fsm.handle(Event::App(AppCommand::DriveInbound {
            now: Instant::now(),
        }));
        if ctx.dispatch(
            &mut edge,
            actions,
            fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
        ) {
            state.store(STATE_HUNGUP, Ordering::Relaxed);
            let _ = done_tx.send(our_call);
            return;
        }
    }

    let mut events = Events::with_capacity(8);
    loop {
        let now = Instant::now();
        let next_timer = timers.iter().map(|(at, _)| *at).min();
        let timeout = match next_timer {
            Some(t) if t > now => t - now,
            Some(_) => Duration::from_millis(0),
            None => Duration::from_millis(100),
        };
        if let Err(e) = poll.poll(&mut events, Some(timeout)) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        let mut terminated = false;

        // Drain demuxed datagrams. The Listener already verified src == peer_addr
        // before forwarding (source-addr pin), so we trust the channel here.
        while let Ok((bytes, _src)) = frame_rx.try_recv() {
            let Ok(frame) = parse_lenient(&bytes) else {
                continue;
            };
            let now = Instant::now();
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            match ctx.rel.on_frame_in(frame, now) {
                RxOutcome::Deliver { frame, send_ack } => {
                    if let Some(ack) = send_ack {
                        let _ = ctx.sock.send_to(&ack, peer_addr);
                    }
                    let actions = fsm.handle(Event::Frame { frame, now });
                    if ctx.dispatch(
                        &mut edge,
                        actions,
                        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                    ) {
                        terminated = true;
                        break;
                    }
                }
                RxOutcome::Consumed => {}
                RxOutcome::Duplicate { resend_ack } => {
                    if let Some(b) = resend_ack {
                        let _ = ctx.sock.send_to(&b, peer_addr);
                    }
                }
                RxOutcome::Vnak(iseqno) => {
                    for b in ctx.rel.resend_from(iseqno) {
                        let _ = ctx.sock.send_to(&b, peer_addr);
                    }
                }
                RxOutcome::GaveUp { oseqno } => {
                    let actions = fsm.handle(Event::DeliveryFailed { oseqno });
                    if ctx.dispatch(
                        &mut edge,
                        actions,
                        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                    ) {
                        terminated = true;
                        break;
                    }
                }
            }
        }
        if terminated {
            break;
        }

        // Drain control commands (answer/reject from the IncomingCall handle).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RuntimeCommand::App(app) => {
                    // iax-7bdc: an answer/reject arriving before the
                    // calltoken/auth handshake has reached AcceptSent would be
                    // dropped by the FSM's catch-all. Park it; the replay
                    // below fires once the handshake completes.
                    if matches!(
                        app,
                        AppCommand::AnswerIncoming { .. } | AppCommand::RejectIncoming { .. }
                    ) && matches!(
                        fsm.state(),
                        SessionState::NewReceived(_)
                            | SessionState::CallTokenIssued(_)
                            | SessionState::AuthReqSent(_)
                    ) {
                        parked_decision = Some(app);
                        continue;
                    }
                    let mut ctx = leg_ctx(
                        sock.as_ref(),
                        peer_addr,
                        &mut rel,
                        &mut timers,
                        &spk_tx,
                        &event_tx,
                        &state,
                        &remote_keyed,
                        &mut pending_answer_oseqno,
                        &mut rx_decode_warned,
                    );
                    let actions = fsm.handle(Event::App(app));
                    if ctx.dispatch(
                        &mut edge,
                        actions,
                        fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
                    ) {
                        terminated = true;
                        break;
                    }
                }
                RuntimeCommand::Shutdown => {
                    terminated = true;
                    break;
                }
            }
        }
        if terminated {
            break;
        }

        // iax-7bdc: replay a parked answer/reject once the handshake reaches
        // AcceptSent (the frame drain above is where the token/auth NEW
        // advances the state, so the parked decision applies within the same
        // loop pass it becomes valid).
        if parked_decision.is_some() && matches!(fsm.state(), SessionState::AcceptSent(_)) {
            let app = parked_decision.take().expect("checked is_some");
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            let actions = fsm.handle(Event::App(app));
            if ctx.dispatch(
                &mut edge,
                actions,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
            ) {
                break;
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
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            let actions = fsm.handle(Event::Timer { kind, now });
            if ctx.dispatch(
                &mut edge,
                actions,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
            ) {
                terminated = true;
                break;
            }
        }
        if terminated {
            break;
        }

        // AnswerAcked seam (Decision §2): when Reliability releases the in-flight
        // ANSWER's oseqno, drive AnswerSent -> Active. The FSM never sees the
        // bare ACK (Reliability consumed it).
        if let Some(oseqno) = pending_answer_oseqno
            && !rel.has_inflight(oseqno)
        {
            pending_answer_oseqno = None;
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            let actions = fsm.handle(Event::App(AppCommand::AnswerAcked {
                now: Instant::now(),
            }));
            if ctx.dispatch(
                &mut edge,
                actions,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
            ) {
                break;
            }
        }

        // Pump captured mic audio as voice frames (20 ms ladder). Outbound codec
        // edge (iax-31f7): the bus carries PCM; the leg encodes to the format it
        // chose in its own ACCEPT (this is where inbound slin TX happens).
        while let Ok(pcm) = mic_rx.try_recv() {
            next_voice_ts = next_voice_ts.wrapping_add(20);
            let format = fsm.negotiated_format().unwrap_or(VoiceFormat::G711U);
            // Outbound codec edge (iax-31f7 / iax-4348): encode to the leg's
            // chosen format, resampling to the wire rate when it differs.
            let payload = edge.encode(format, &pcm);
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            let actions = fsm.handle(Event::App(AppCommand::SendVoice {
                format,
                payload,
                ts: next_voice_ts,
            }));
            if ctx.dispatch(
                &mut edge,
                actions,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
            ) {
                terminated = true;
                break;
            }
        }
        if terminated {
            break;
        }

        // Reliability retransmits / give-ups.
        let tick = rel.tick(Instant::now());
        for bytes in tick.retransmit {
            let _ = sock.send_to(&bytes, peer_addr);
        }
        for oseqno in tick.gave_up {
            let mut ctx = leg_ctx(
                sock.as_ref(),
                peer_addr,
                &mut rel,
                &mut timers,
                &spk_tx,
                &event_tx,
                &state,
                &remote_keyed,
                &mut pending_answer_oseqno,
                &mut rx_decode_warned,
            );
            let actions = fsm.handle(Event::DeliveryFailed { oseqno });
            if ctx.dispatch(
                &mut edge,
                actions,
                fsm.negotiated_format().unwrap_or(VoiceFormat::G711U),
            ) {
                terminated = true;
                break;
            }
        }
        if terminated {
            break;
        }

        let micros = fsm.rtt().map_or(u32::MAX, |d| {
            u32::try_from(d.as_micros()).unwrap_or(u32::MAX - 1)
        });
        rtt_micros.store(micros, Ordering::Relaxed);
        // iax-31f7: publish the negotiated codec once per event-handling pass
        // for Call::snapshot. `0` = not yet negotiated.
        format_bits.store(
            fsm.negotiated_format().map_or(0, VoiceFormat::as_u32),
            Ordering::Relaxed,
        );
        let _ = start;
    }

    state.store(STATE_HUNGUP, Ordering::Relaxed);
    let _ = done_tx.send(our_call);
}

/// Per-leg dispatch context (groups the args `dispatch` needs, mirroring
/// `client::dispatch_actions` but with `send_to(peer_addr)`).
struct LegCtx<'a> {
    sock: &'a dyn LinkSocket,
    peer_addr: SocketAddr,
    rel: &'a mut Reliability,
    timers: &'a mut Vec<(Instant, TimerKind)>,
    spk_tx: &'a mpsc::Sender<Vec<i16>>,
    event_tx: &'a mpsc::Sender<CallEvent>,
    state: &'a Arc<AtomicU8>,
    /// Live remote-PTT-key cell (iax-feab parrot mode), mirroring `state`.
    remote_keyed: &'a Arc<AtomicBool>,
    pending_answer_oseqno: &'a mut Option<u8>,
    rx_decode_warned: &'a mut bool,
}

#[allow(clippy::too_many_arguments)]
fn leg_ctx<'a>(
    sock: &'a dyn LinkSocket,
    peer_addr: SocketAddr,
    rel: &'a mut Reliability,
    timers: &'a mut Vec<(Instant, TimerKind)>,
    spk_tx: &'a mpsc::Sender<Vec<i16>>,
    event_tx: &'a mpsc::Sender<CallEvent>,
    state: &'a Arc<AtomicU8>,
    remote_keyed: &'a Arc<AtomicBool>,
    pending_answer_oseqno: &'a mut Option<u8>,
    rx_decode_warned: &'a mut bool,
) -> LegCtx<'a> {
    LegCtx {
        sock,
        peer_addr,
        rel,
        timers,
        spk_tx,
        event_tx,
        state,
        remote_keyed,
        pending_answer_oseqno,
        rx_decode_warned,
    }
}

impl LegCtx<'_> {
    /// Apply FSM actions: send via `send_to(peer_addr)`, arm/cancel timers, route
    /// decoded voice to the speaker, forward lifecycle events. Returns `true`
    /// when the call has terminated. Captures the ANSWER oseqno for the
    /// `AnswerAcked` seam.
    ///
    /// `negotiated` is the format the leg's FSM settled on (iax-31f7): callers
    /// pass `fsm.negotiated_format().unwrap_or(VoiceFormat::G711U)` read fresh
    /// at the call site, so `Answered` reports what was actually negotiated
    /// (mirrors `runtime.rs::translate`).
    fn dispatch(
        &mut self,
        edge: &mut crate::codec_edge::EdgeAudio,
        actions: smallvec::SmallVec<[Action; 4]>,
        negotiated: VoiceFormat,
    ) -> bool {
        let now = Instant::now();
        let mut terminated = false;
        for action in actions {
            match action {
                Action::SendReliable(frame) => {
                    let is_answer = frame.frame_type == FrameType::Control
                        && frame.subclass
                            == Subclass::Control(astar_iax_core::subclass::ControlSubclass::Answer);
                    let oseqno = self.rel.next_oseqno();
                    let bytes = self.rel.enqueue(frame, now);
                    let _ = self.sock.send_to(&bytes, self.peer_addr);
                    if is_answer {
                        *self.pending_answer_oseqno = Some(oseqno);
                    }
                }
                Action::SendUnreliable(bytes) => {
                    let _ = self.sock.send_to(&bytes, self.peer_addr);
                }
                Action::SetPeerCall(peer) => {
                    self.rel.set_peer_call(peer);
                }
                Action::SetTimer(kind, dur) => {
                    self.timers.retain(|(_, k)| *k != kind);
                    self.timers.push((now + dur, kind));
                }
                Action::CancelTimer(kind) => {
                    self.timers.retain(|(_, k)| *k != kind);
                }
                Action::ResetReliability => {
                    self.rel.reset();
                }
                Action::AppEvent(ev) => {
                    match &ev {
                        AppEvent::Connected { .. } => {
                            self.state.store(STATE_ACTIVE, Ordering::Relaxed);
                        }
                        AppEvent::Disconnected { .. } => {
                            self.state.store(STATE_HUNGUP, Ordering::Relaxed);
                        }
                        // iax-feab: publish the peer's PTT-key state BEFORE
                        // translating to a CallEvent (mirrors runtime.rs).
                        AppEvent::RemotePtt(b) => {
                            self.remote_keyed.store(*b, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    if let AppEvent::VoiceReceived {
                        format, payload, ..
                    } = &ev
                    {
                        // RX codec edge (iax-31f7 / iax-4348): decode on the
                        // received frame's own format into bus-rate PCM
                        // (resampling if the wire rate differs); drop undecodable
                        // frames (warn once per leg).
                        match edge.decode(*format, payload) {
                            Some(pcm) => {
                                let _ = self.spk_tx.send(pcm);
                            }
                            None if !*self.rx_decode_warned => {
                                *self.rx_decode_warned = true;
                                tracing::warn!(
                                    ?format,
                                    len = payload.len(),
                                    "dropping undecodable RX voice"
                                );
                            }
                            None => {}
                        }
                    } else if let Some(ce) = translate_leg(&ev, negotiated) {
                        let is_hangup = matches!(ce, CallEvent::Hangup { .. });
                        let _ = self.event_tx.send(ce);
                        if is_hangup {
                            terminated = true;
                        }
                    }
                }
                Action::LogInvalid { reason } => {
                    tracing::debug!(target: "astar_iax::listener", reason);
                }
            }
        }
        terminated
    }
}

/// Translate an FSM [`AppEvent`] into the public [`CallEvent`] (leg copy of
/// `client::translate`; inbound legs additionally never see `IncomingCall`).
///
/// `negotiated` is the format the leg's FSM settled on (iax-31f7), so
/// `Answered` reports the actual negotiation (e.g. slin) instead of a
/// hardcoded G.711µ.
fn translate_leg(event: &AppEvent, negotiated: VoiceFormat) -> Option<CallEvent> {
    match event {
        AppEvent::Connected { .. } => Some(CallEvent::Answered { format: negotiated }),
        AppEvent::Disconnected { reason } => Some(CallEvent::Hangup {
            reason: reason.clone(),
        }),
        AppEvent::DtmfReceived(d) => Some(CallEvent::Dtmf(*d)),
        AppEvent::RemotePtt(b) => Some(CallEvent::RemotePtt(*b)),
        AppEvent::TextReceived(t) => Some(CallEvent::Text(match t {
            astar_iax_core::OwnedTextEvent::Raw(s) => s.clone(),
            astar_iax_core::OwnedTextEvent::KStatus {
                src,
                name,
                keyed,
                since,
            } => format!("K {src} {name} {} {since}", u8::from(*keyed)),
        })),
        AppEvent::ConnectionLost => Some(CallEvent::ConnectionLost),
        AppEvent::ConnectionRestored => Some(CallEvent::ConnectionRestored),
        // iax-85e7: call-progress stays on the internal AppEvent channel +
        // console for now; not surfaced through the public CallEvent (no FFI
        // balloon). Promote to a CallEvent variant in a follow-up if wanted.
        AppEvent::CallProgress(_)
        | AppEvent::VoiceReceived { .. }
        | AppEvent::IncomingCall { .. } => None,
    }
}

// --- Public API: events + the IncomingCall handle (Task 18) -----------------

/// An inbound call awaiting an accept/reject decision (or already auto-answered).
/// Carries the parsed caller-ID context plus the pinned `peer_addr`; the
/// `answer()` path yields the UNIFIED [`Call`] (iax-42e9 keystone) so
/// `Manager::adopt` pools it exactly like a dialed leg.
pub struct IncomingCall {
    /// Our freshly-allocated wire call number for this leg.
    pub our_call: CallNo,
    /// The peer's source call number.
    pub peer_call: CallNo,
    /// The source address the leg is pinned to (source-addr validation,
    /// R-10.0-07). Datagrams from any other source are dropped by the Listener.
    pub peer_addr: SocketAddr,
    /// Caller-ID context from the inbound NEW.
    pub calling_number: Option<String>,
    pub calling_name: Option<String>,
    pub called_number: Option<String>,
    pub username: Option<String>,
    pub offered_codecs: u32,
    pub preferred_codec: Option<VoiceFormat>,
    pub language: Option<String>,
    /// Single-use handle the public methods consume to drive the leg + build
    /// the unified `Call`. `None` after `answer`/`reject`/`ignore`.
    handle: Option<LegHandle>,
}

/// The per-leg plumbing the `IncomingCall` consumes on a decision.
struct LegHandle {
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    waker: Arc<Waker>,
    join: JoinHandle<()>,
    event_rx: mpsc::Receiver<CallEvent>,
    ptt: crate::audio_bridge::PttGate,
    rtt_micros: Arc<AtomicU32>,
    state: Arc<AtomicU8>,
    /// Negotiated-codec cell published by the leg run-loop (iax-31f7); carried
    /// into `Call::new_inbound` alongside `rx_source`/`tx_sender`.
    format_bits: Arc<AtomicU32>,
    /// The unified-Call audio ends (keystone): the RX `Receiver` the Manager
    /// joins to an output bus, and the parked TX `Sender` a mic lane binds to.
    rx_source: mpsc::Receiver<Vec<i16>>,
    tx_sender: mpsc::Sender<Vec<i16>>,
    /// The leg's bus sample rate (iax-4348) = its listener policy's codec cap.
    /// Carried into `Call::new_inbound` so `Manager::adopt` can reject a leg
    /// whose rate differs from the station bus.
    sample_rate: u32,
    /// Live remote-PTT-key cell (iax-feab parrot mode), mirroring `state`.
    /// Carried into `Call::new_inbound`.
    remote_keyed: Arc<AtomicBool>,
}

impl IncomingCall {
    fn wake(handle: &LegHandle, cmd: RuntimeCommand) {
        let _ = handle.cmd_tx.send(cmd);
        let _ = handle.waker.wake();
    }

    /// Consume a `LegHandle` into the unified `Call` (keystone) + its event
    /// stream. Used by both `answer()` and the auto-answer path.
    fn into_unified_call(handle: LegHandle) -> (Call, mpsc::Receiver<CallEvent>) {
        let LegHandle {
            cmd_tx,
            waker,
            join,
            event_rx,
            ptt,
            rtt_micros,
            state,
            format_bits,
            rx_source,
            tx_sender,
            sample_rate,
            remote_keyed,
        } = handle;
        let call = Call::new_inbound(
            cmd_tx,
            waker,
            join,
            ptt,
            rtt_micros,
            CallId::from_raw(0),
            String::new(),
            CallSnapshotMode::Direct,
            state,
            rx_source,
            tx_sender,
            format_bits,
            sample_rate,
            remote_keyed,
        );
        (call, event_rx)
    }

    /// Accept + answer the call. Drives the leg to ANSWER (the leg fires
    /// `AnswerAcked` itself when Reliability releases the ANSWER), and returns
    /// the UNIFIED [`Call`] keystone plus its [`CallEvent`] stream.
    /// `Manager::adopt(call, &output)` pools it monitor-only like a dialed leg.
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if the handle was already consumed.
    pub fn answer(mut self) -> Result<(Call, mpsc::Receiver<CallEvent>), IaxError> {
        let handle = self.handle.take().ok_or(IaxError::NoActiveCall)?;
        Self::wake(
            &handle,
            RuntimeCommand::App(AppCommand::AnswerIncoming {
                now: Instant::now(),
            }),
        );
        Ok(Self::into_unified_call(handle))
    }

    /// Reject the call with an optional CAUSE. Drives the leg to HANGUP and
    /// joins its thread.
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if the handle was already consumed.
    pub fn reject(mut self, cause: Option<String>) -> Result<(), IaxError> {
        let handle = self.handle.take().ok_or(IaxError::NoActiveCall)?;
        Self::wake(
            &handle,
            RuntimeCommand::App(AppCommand::RejectIncoming {
                cause,
                now: Instant::now(),
            }),
        );
        Self::wake(&handle, RuntimeCommand::Shutdown);
        let LegHandle { join, .. } = handle;
        join.join()
            .map_err(|_| IaxError::Io(io::Error::other("leg thread panicked")))
    }

    /// Ignore the call: tear the leg down silently (no REJECT on the wire).
    ///
    /// # Errors
    /// [`IaxError::NoActiveCall`] if the handle was already consumed.
    pub fn ignore(mut self) -> Result<(), IaxError> {
        let handle = self.handle.take().ok_or(IaxError::NoActiveCall)?;
        Self::wake(&handle, RuntimeCommand::Shutdown);
        let LegHandle { join, .. } = handle;
        join.join()
            .map_err(|_| IaxError::Io(io::Error::other("leg thread panicked")))
    }
}

impl Drop for IncomingCall {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.cmd_tx.send(RuntimeCommand::Shutdown);
            let _ = handle.waker.wake();
            let _ = handle.join.join();
        }
    }
}

/// Events surfaced on the Listener's event channel.
pub enum IncomingCallEvent {
    /// A new inbound call awaiting a decision (`AppDecide` policy).
    Incoming(IncomingCall),
    /// An auto-answered inbound call: the unified [`Call`] is delivered directly
    /// (`AutoAccept` / `auto_answer` policy), poolable identically to a dialed leg.
    Answered {
        call: Call,
        events: mpsc::Receiver<CallEvent>,
    },
}

// --- The Listener actor + builder (Task 16/17) ------------------------------

/// Builder for an [`IncomingCallListener`].
pub struct IncomingCallListenerBuilder {
    bind: SocketAddr,
    policy: IncomingCallPolicy,
    /// Transport seam (iax-b6f5): the socket factory the shared socket is bound
    /// from. [`OsNetStack`] (plain OS UDP) by default — byte-identical behavior.
    net: Arc<dyn NetStack>,
    /// Optional plain OS UDP address bound ALONGSIDE the primary socket
    /// (iax-927a: `WireGuard` mode's `also_bind_udp` for direct/LAN peers).
    also_bind_udp: Option<SocketAddr>,
}

impl IncomingCallListenerBuilder {
    /// Bind address. Use `127.0.0.1:0` for an ephemeral test port; `0.0.0.0:4569`
    /// for the standard IAX2 port in production.
    #[must_use]
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = addr;
        self
    }

    /// Set the inbound-call policy (auth/calltoken/decision/credentials).
    #[must_use]
    pub fn policy(mut self, policy: IncomingCallPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Transport seam (iax-927a): bind the shared socket from `net` instead of
    /// the OS default — e.g. [`crate::Manager::net_stack`] so the listener
    /// rides the Manager's `WireGuard` tunnel.
    #[must_use]
    pub fn net(mut self, net: Arc<dyn NetStack>) -> Self {
        self.net = net;
        self
    }

    /// Additionally bind a plain OS UDP socket on `addr` alongside the primary
    /// one (iax-927a: `WireGuard` mode's `also_bind_udp`, from
    /// [`crate::Manager::also_bind_udp`]). Both sockets run the same demux
    /// under the same policy and feed the same event channel — i.e. the same
    /// adopt/route path. `None` (the default) binds nothing extra.
    #[must_use]
    pub fn also_bind_udp(mut self, addr: Option<SocketAddr>) -> Self {
        self.also_bind_udp = addr;
        self
    }

    /// Bind the shared socket(s) and spawn the demux thread(s). Returns the
    /// listener handle (carrying the bound local addr) and the
    /// [`IncomingCallEvent`] receiver.
    ///
    /// # Errors
    /// [`IaxError::Io`] if the socket / poll setup fails.
    pub fn start(
        self,
    ) -> Result<(IncomingCallListener, mpsc::Receiver<IncomingCallEvent>), IaxError> {
        let policy = Arc::new(self.policy);
        let (event_tx, event_rx) = mpsc::channel();
        let primary = start_listener_socket(
            self.net.as_ref(),
            self.bind,
            Arc::clone(&policy),
            event_tx.clone(),
        )?;
        // The extra plain-UDP socket (iax-927a) is always an OS socket — the
        // point is reaching the node NEXT TO the tunnel.
        let extra = self
            .also_bind_udp
            .map(|addr| start_listener_socket(&OsNetStack, addr, policy, event_tx))
            .transpose()?;
        Ok((IncomingCallListener { primary, extra }, event_rx))
    }
}

/// Bind one listener socket from `net` and spawn its demux thread.
fn start_listener_socket(
    net: &dyn NetStack,
    bind: SocketAddr,
    policy: Arc<IncomingCallPolicy>,
    event_tx: mpsc::Sender<IncomingCallEvent>,
) -> Result<ListenerSocket, IaxError> {
    let mut sock = net.bind(bind).map_err(IaxError::Io)?;
    let local_addr = sock.local_addr().map_err(IaxError::Io)?;
    let poll = Poll::new().map_err(IaxError::Io)?;
    let waker = Arc::new(Waker::new(poll.registry(), Token(1)).map_err(IaxError::Io)?);
    sock.register(poll.registry(), Token(0), Arc::clone(&waker))
        .map_err(IaxError::Io)?;
    let sock: Arc<dyn LinkSocket> = Arc::from(sock);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ListenerCommand>();
    let waker_for_caller = Arc::clone(&waker);
    let handle = thread::Builder::new()
        .name(format!("iax-listen-{local_addr}"))
        .spawn(move || {
            listener_run_loop(poll, sock, policy, event_tx, cmd_rx);
        })
        .map_err(|e| IaxError::Io(io::Error::other(e)))?;
    Ok(ListenerSocket {
        local_addr,
        cmd_tx,
        waker: waker_for_caller,
        handle: Some(handle),
    })
}

enum ListenerCommand {
    Shutdown,
}

/// One bound listener socket + its demux thread (the primary or the
/// `also_bind_udp` extra).
struct ListenerSocket {
    local_addr: SocketAddr,
    cmd_tx: mpsc::Sender<ListenerCommand>,
    waker: Arc<Waker>,
    handle: Option<JoinHandle<()>>,
}

impl ListenerSocket {
    fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(ListenerCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A running inbound-call listener. Drops shut down the demux thread(s).
pub struct IncomingCallListener {
    primary: ListenerSocket,
    extra: Option<ListenerSocket>,
}

impl IncomingCallListener {
    /// Start configuring a listener.
    #[must_use]
    pub fn builder() -> IncomingCallListenerBuilder {
        IncomingCallListenerBuilder {
            bind: "0.0.0.0:4569".parse().expect("valid default bind"),
            policy: IncomingCallPolicy::default(),
            net: Arc::new(OsNetStack),
            also_bind_udp: None,
        }
    }

    /// The actually-bound local address (resolves the ephemeral `:0` port).
    /// In `WireGuard` mode this is the tunnel-inner address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.primary.local_addr
    }

    /// The actually-bound address of the extra plain-UDP socket, when
    /// [`IncomingCallListenerBuilder::also_bind_udp`] was set (iax-927a).
    #[must_use]
    pub fn extra_local_addr(&self) -> Option<SocketAddr> {
        self.extra.as_ref().map(|s| s.local_addr)
    }
}

impl Drop for IncomingCallListener {
    fn drop(&mut self) {
        self.primary.shutdown();
        if let Some(extra) = self.extra.as_mut() {
            extra.shutdown();
        }
    }
}

/// State a spawned leg's demux entry holds in the Listener.
struct LegEntry {
    peer_addr: SocketAddr,
    peer_call: CallNo,
    frame_tx: mpsc::Sender<LegDatagram>,
    waker: Arc<Waker>,
}

/// The Listener's blocking demux loop. Owns the shared socket; reads datagrams,
/// demuxes by `dest_call`, applies inline `DoS` hardening, spawns legs on `NEW`, and
/// forwards in-call datagrams to per-leg channels (source-addr-pinned).
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn listener_run_loop(
    mut poll: Poll,
    sock: Arc<dyn LinkSocket>,
    policy: Arc<IncomingCallPolicy>,
    event_tx: mpsc::Sender<IncomingCallEvent>,
    cmd_rx: mpsc::Receiver<ListenerCommand>,
) {
    let mut allocator = CallNoAllocator::new();
    // dest_call (our_call) -> leg entry.
    let mut by_local_call: HashMap<CallNo, LegEntry> = HashMap::new();
    // (peer_addr, peer's source_call) -> our_call, for NEW-retransmit dedupe.
    let mut by_peer: HashMap<(SocketAddr, u16), CallNo> = HashMap::new();
    // Legs signal their own teardown here so we can free the call number + maps.
    let (done_tx, done_rx) = mpsc::channel::<CallNo>();
    let mut buf = [0u8; 4096];
    let mut events = Events::with_capacity(16);

    loop {
        if let Err(e) = poll.poll(&mut events, Some(Duration::from_millis(200))) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }

        // Shutdown?
        if let Ok(ListenerCommand::Shutdown) = cmd_rx.try_recv() {
            return;
        }

        // Reap terminated legs.
        while let Ok(our_call) = done_rx.try_recv() {
            if let Some(entry) = by_local_call.remove(&our_call) {
                by_peer.remove(&(entry.peer_addr, entry.peer_call.value()));
                allocator.free(our_call);
            }
        }

        // Drain inbound datagrams. Attempt a non-blocking drain on ANY wakeup
        // (not just when the socket token fired): behaviorally identical for
        // OS UDP (an empty socket returns `WouldBlock` immediately), and
        // required by a waker-driven [`LinkSocket`] impl whose readiness
        // arrives via the waker, not a token (iax-b6f5).
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return,
            };
            let bytes = buf[..n].to_vec();
            demux_one(
                &sock,
                &policy,
                &mut allocator,
                &mut by_local_call,
                &mut by_peer,
                &done_tx,
                &event_tx,
                &bytes,
                src,
            );
        }
    }
}

/// Resolve the inbound credential for `username`: the static `credentials` map
/// wins; on a miss, fall back to the dynamic `credential_resolver` (iax-99cd) so
/// a runtime store (e.g. the node `SecretProvider`) can authenticate without
/// pre-populating the map. `None` = no credential (empty resolver result too).
///
/// L5 (iax-f755): clone the `Arc`, never copy the plaintext out of `Secret`.
fn resolve_credential(policy: &IncomingCallPolicy, username: &str) -> Option<Arc<Secret>> {
    if let Some(s) = policy.credentials.get(username) {
        return Some(Arc::clone(s));
    }
    let resolved = policy.credential_resolver.as_ref()?(username);
    if resolved.is_empty() {
        None
    } else {
        Some(Arc::new(Secret::new(resolved)))
    }
}

/// Demux a single datagram. Inline INVAL/POKE/REJECT; NEW spawns a leg;
/// in-call datagrams are forwarded to the pinned leg.
#[allow(clippy::too_many_arguments)]
fn demux_one(
    sock: &Arc<dyn LinkSocket>,
    policy: &IncomingCallPolicy,
    allocator: &mut CallNoAllocator,
    by_local_call: &mut HashMap<CallNo, LegEntry>,
    by_peer: &mut HashMap<(SocketAddr, u16), CallNo>,
    done_tx: &mpsc::Sender<CallNo>,
    event_tx: &mpsc::Sender<IncomingCallEvent>,
    bytes: &[u8],
    src: SocketAddr,
) {
    let Some(dest_call) = peek_dest_call(bytes) else {
        // Mini frame (no dest_call in the header) or truncated. A mini voice
        // frame still belongs to a call — route it by the pinned source address
        // (a leg is pinned 1:1 to its peer_addr, so `src` selects it). Without
        // this, every voice frame after the opening FULL frame (all of which are
        // mini frames) was dropped: node-as-handset RX played one frame then
        // went silent (iax-64b6).
        if let Some(entry) = by_local_call.values().find(|e| e.peer_addr == src) {
            let _ = entry.frame_tx.send((bytes.to_vec(), src));
            let _ = entry.waker.wake();
        }
        return;
    };

    // In-call datagram (dest_call != 0): forward to the pinned leg, or INVAL.
    if dest_call != 0 {
        if let Some(our) = CallNo::new(dest_call)
            && let Some(entry) = by_local_call.get(&our)
        {
            // Source-addr pin (R-10.0-07): drop datagrams from a different src.
            if entry.peer_addr != src {
                tracing::debug!(target: "astar_iax::listener", "drop frame: source addr mismatch");
                return;
            }
            let _ = entry.frame_tx.send((bytes.to_vec(), src));
            let _ = entry.waker.wake();
            return;
        }
        // Unknown dest_call: INVAL, drop (R-6.9-02) — but NEVER reply to a frame
        // that is itself unanswerable. An ACK is unreliable and generates no
        // reply (RFC 5456 §8.4); an INVAL likewise expects no response. Replying
        // INVAL to a stale ACK/INVAL deadlocks against a peer that ACKs our INVAL:
        // we INVAL its ACK, it ACKs our INVAL, forever. This happens right after
        // a leg is reaped (e.g. the allowlist/cap reject path): the peer's ACK of
        // our terminal HANGUP/REJECT lands on the now-unknown leg call number.
        if inval_warranted(bytes)
            && let Some(peer_call) = peek_source_call(bytes)
            && let Some(b) = inline_inval(peer_call)
        {
            let _ = sock.send_to(&b, src);
        }
        return;
    }

    // dest_call == 0: must be a NEW (or POKE). Parse to classify.
    let Ok(Frame::Full(f)) = parse_lenient(bytes) else {
        return;
    };
    let peer_source = f.source_call;
    match f.subclass {
        Subclass::Iax(IaxCommand::New) => {}
        Subclass::Iax(IaxCommand::Poke) => {
            // Inline PONG, no spawn (DoS: POKE floods must not allocate).
            if let Some(b) = inline_pong(peer_source) {
                let _ = sock.send_to(&b, src);
            }
            return;
        }
        _ => {
            // Any other dcallno=0 non-NEW: INVAL inline, drop.
            if let Some(b) = inline_inval(peer_source) {
                let _ = sock.send_to(&b, src);
            }
            return;
        }
    }

    // NEW. Dedupe retransmits keyed by (src, peer_source).
    if let Some(&our) = by_peer.get(&(src, peer_source)) {
        if let Some(entry) = by_local_call.get(&our) {
            let _ = entry.frame_tx.send((bytes.to_vec(), src));
            let _ = entry.waker.wake();
        }
        return;
    }

    let Some(peer_call) = CallNo::new(peer_source) else {
        return; // peer source_call 0 on a NEW is malformed; drop.
    };

    // Parse + validate the offer.
    let offer = match IncomingOffer::from_new_ies(&f.ies) {
        Ok(o) => o,
        Err(reject) => {
            reject_inline(sock, peer_source, reject.cause(), src);
            return;
        }
    };

    // Resolve credentials. If auth is Required, an unknown username is rejected
    // inline and never spawns. Otherwise we use the matched secret (or an empty
    // one for the no-auth path).
    let username = offer.username.clone().unwrap_or_default();
    // Resolve a credential (static map, then dynamic resolver). `None` = miss:
    // REJECT under `Required`, else the empty no-auth secret.
    let secret = match resolve_credential(policy, &username) {
        Some(s) => s,
        None if matches!(policy.auth, IncomingAuthPolicy::Required) => {
            reject_inline(sock, peer_source, "No such user", src);
            return;
        }
        None => Arc::new(Secret::new(String::new())),
    };

    // Allocate a call number; pool exhaustion → REJECT inline, no spawn.
    let Some(our_call) = allocator.alloc() else {
        reject_inline(sock, peer_source, "System busy", src);
        return;
    };

    match spawn_leg(
        sock, policy, our_call, peer_call, offer, secret, src, done_tx, event_tx,
    ) {
        Ok(entry) => {
            by_peer.insert((src, peer_source), our_call);
            by_local_call.insert(our_call, entry);
        }
        Err(()) => {
            allocator.free(our_call);
        }
    }
}

/// Read the peer's `source_call` (low 15 bits of bytes 0-1) for inline replies.
fn peek_source_call(buf: &[u8]) -> Option<u16> {
    if buf.len() < 2 || buf[0] & 0x80 == 0 {
        return None;
    }
    Some(u16::from_be_bytes([buf[0], buf[1]]) & 0x7fff)
}

/// Whether an unknown-`dest_call` datagram warrants an inline INVAL reply.
///
/// Per RFC 5456 a bare ACK is unreliable and elicits no response, and an INVAL
/// itself is terminal/unanswered. Replying INVAL to either creates a frame storm
/// against a peer that ACKs our INVAL (it ACKs our INVAL → we INVAL its ACK →
/// …), so those two subclasses are dropped silently. Everything else (a stray
/// HANGUP, voice, etc. on a dead call number) still gets the INVAL.
fn inval_warranted(bytes: &[u8]) -> bool {
    !matches!(
        parse_lenient(bytes),
        Ok(Frame::Full(f))
            if matches!(f.subclass, Subclass::Iax(IaxCommand::Ack | IaxCommand::Inval))
    )
}

fn reject_inline(sock: &Arc<dyn LinkSocket>, peer_call: u16, cause: &str, dst: SocketAddr) {
    if let Some(b) = inline_reject(peer_call, cause) {
        let _ = sock.send_to(&b, dst);
    }
}

/// Spawn a leg thread for an accepted NEW. Builds the unified-Call audio ends,
/// the leg's own poll/waker, and the per-leg channels, then either surfaces an
/// `Incoming` event (`AppDecide`) or delivers the unified `Call` (auto-answer path).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn spawn_leg(
    sock: &Arc<dyn LinkSocket>,
    policy: &IncomingCallPolicy,
    our_call: CallNo,
    peer_call: CallNo,
    offer: IncomingOffer,
    secret: Arc<Secret>,
    peer_addr: SocketAddr,
    done_tx: &mpsc::Sender<CallNo>,
    event_tx: &mpsc::Sender<IncomingCallEvent>,
) -> Result<LegEntry, ()> {
    let mut lowered = policy.lower();
    // Optional auth challenges iff we hold a (non-empty) credential for the
    // offered username (iax-99cd). `Required` already lowers to
    // `auth_required = true`; `Off` never challenges. For `Optional`, a
    // resolver/map hit (non-empty `secret`) promotes this leg to a challenge,
    // while a miss (empty `secret`) admits anonymously.
    if matches!(policy.auth, IncomingAuthPolicy::Optional) && !secret.expose().is_empty() {
        lowered.auth_required = true;
    }
    let auto_answer = lowered.auto_answer;

    // OsRng entropy at the runtime boundary (the FSM never calls OsRng).
    let mut token = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut token);
    let mut chal = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut chal);
    let mut challenge_hex = String::with_capacity(32);
    for b in chal {
        use std::fmt::Write as _;
        let _ = write!(challenge_hex, "{b:02x}");
    }

    let creds = Credentials {
        username: offer.username.clone().unwrap_or_default(),
        password: secret,
        allowed_methods: AuthMethods::MD5,
    };

    // Per-leg poll + waker (the Listener wakes it on forwarded frames + cmds).
    let Ok(poll) = Poll::new() else {
        return Err(());
    };
    let Ok(waker) = Waker::new(poll.registry(), Token(1)) else {
        return Err(());
    };
    let waker = Arc::new(waker);

    let (frame_tx, frame_rx) = mpsc::channel::<LegDatagram>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<RuntimeCommand>();
    let (event_call_tx, event_call_rx) = mpsc::channel::<CallEvent>();
    // Keystone audio ends: leg drains `mic_rx` (tx_sender side), fills `spk_tx`
    // (rx_source side).
    let (mic_tx, mic_rx) = mpsc::channel::<Vec<i16>>();
    let (spk_tx, spk_rx) = mpsc::channel::<Vec<i16>>();
    let rtt_micros = Arc::new(AtomicU32::new(u32::MAX));
    let state = Arc::new(AtomicU8::new(crate::call::STATE_CONNECTING));
    // iax-31f7: negotiated-codec cell, published by the leg run-loop.
    let format_bits = Arc::new(AtomicU32::new(0));
    // iax-feab: shared remote-PTT-key cell (parrot mode's per-leg key gate).
    let remote_keyed = Arc::new(AtomicBool::new(false));

    let spawn = LegSpawn {
        sock: Arc::clone(sock),
        peer_addr,
        our_call,
        peer_call,
        offer: offer.clone(),
        creds,
        policy: lowered,
        token,
        challenge_hex,
        poll,
        frame_rx,
        cmd_rx,
        event_tx: event_call_tx,
        mic_rx,
        spk_tx,
        rtt_micros: Arc::clone(&rtt_micros),
        state: Arc::clone(&state),
        format_bits: Arc::clone(&format_bits),
        // The leg's bus rate is the listener policy's codec cap (iax-4348): the
        // highest rate it can negotiate (16 kHz iff slin16 is offerable).
        sample_rate: policy.codec_policy.max_sample_rate(),
        done_tx: done_tx.clone(),
        remote_keyed: Arc::clone(&remote_keyed),
    };
    let Ok(join) = thread::Builder::new()
        .name(format!("iax-leg-{our_call}"))
        .spawn(move || leg_run_loop(spawn))
    else {
        return Err(());
    };

    let handle = LegHandle {
        cmd_tx,
        waker: Arc::clone(&waker),
        join,
        event_rx: event_call_rx,
        ptt: crate::audio_bridge::PttGate::new(),
        rtt_micros,
        state,
        format_bits,
        rx_source: spk_rx,
        tx_sender: mic_tx,
        sample_rate: policy.codec_policy.max_sample_rate(),
        remote_keyed,
    };

    if auto_answer {
        let (call, events) = IncomingCall::into_unified_call(handle);
        let _ = event_tx.send(IncomingCallEvent::Answered { call, events });
    } else {
        let incoming = IncomingCall {
            our_call,
            peer_call,
            peer_addr,
            calling_number: offer.calling_number,
            calling_name: offer.calling_name,
            called_number: offer.called_number,
            username: offer.username,
            offered_codecs: offer.offered_codecs.get(),
            preferred_codec: offer.preferred_codec,
            language: offer.language,
            handle: Some(handle),
        };
        let _ = event_tx.send(IncomingCallEvent::Incoming(incoming));
    }

    Ok(LegEntry {
        peer_addr,
        peer_call,
        frame_tx,
        waker,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_leg_answered_carries_the_negotiated_format_not_a_hardcode() {
        // iax-31f7: an inbound leg that negotiated slin must report slin in
        // CallEvent::Answered (previously hardcoded G.711µ).
        let ev = AppEvent::Connected {
            peer_call: CallNo::new(1).expect("valid call no"),
        };
        match translate_leg(&ev, VoiceFormat::Slin) {
            Some(CallEvent::Answered { format }) => assert_eq!(format, VoiceFormat::Slin),
            other => panic!("expected Answered, got {other:?}"),
        }
        match translate_leg(&ev, VoiceFormat::G711U) {
            Some(CallEvent::Answered { format }) => assert_eq!(format, VoiceFormat::G711U),
            other => panic!("expected Answered, got {other:?}"),
        }
    }

    #[test]
    fn incoming_call_policy_defaults_are_neutral() {
        let p = IncomingCallPolicy::default();
        assert!(matches!(p.decision, IncomingDecisionPolicy::AppDecide));
        assert!(matches!(p.auth, IncomingAuthPolicy::Optional)); // neutral: not Required
        assert!(!p.auto_answer);
        assert!(!p.allow_plaintext);
        assert_eq!(p.accept_decision_timeout, Duration::from_secs(30));
        assert_eq!(p.codec_policy, CodecPolicy::UlawOnly);
        // CALLTOKEN default: Never in debug, Always in release (design decision 5).
        #[cfg(debug_assertions)]
        assert!(matches!(p.calltoken, IncomingCallTokenPolicy::Never));
        #[cfg(not(debug_assertions))]
        assert!(matches!(p.calltoken, IncomingCallTokenPolicy::Always));
    }

    #[test]
    fn lower_maps_runtime_enums_to_core_booleans() {
        let p = IncomingCallPolicy {
            auth: IncomingAuthPolicy::Required,
            calltoken: IncomingCallTokenPolicy::Always,
            decision: IncomingDecisionPolicy::AppDecide,
            ..IncomingCallPolicy::default()
        };
        let lowered = p.lower();
        assert!(lowered.auth_required);
        assert!(lowered.calltoken_required);
        assert!(lowered.decision_is_app);
        assert!(!lowered.auto_answer);

        let auto = IncomingCallPolicy {
            decision: IncomingDecisionPolicy::AutoAccept,
            ..IncomingCallPolicy::default()
        };
        let lowered = auto.lower();
        assert!(lowered.auto_answer, "AutoAccept lowers to auto_answer");
        assert!(!lowered.decision_is_app);
    }

    #[test]
    fn policy_debug_never_leaks_credentials() {
        let mut creds = HashMap::new();
        creds.insert(
            "rob".to_string(),
            Arc::new(Secret::new("hunter2".to_string())),
        );
        let p = IncomingCallPolicy {
            credentials: creds,
            // A resolver that would leak if its Debug ever called it.
            credential_resolver: Some(Arc::new(|_u: &str| "topsecret".to_string())),
            ..IncomingCallPolicy::default()
        };
        let dump = format!("{p:?}");
        assert!(!dump.contains("hunter2"), "secret must not appear in Debug");
        assert!(!dump.contains("rob"), "username must not appear in Debug");
        assert!(
            !dump.contains("topsecret"),
            "resolver-provided secret must not appear in Debug"
        );
        assert!(dump.contains("1 entries"));
        assert!(
            dump.contains("<resolver>"),
            "Debug shows the resolver presence as <resolver> only"
        );
    }

    #[test]
    fn policy_debug_resolver_none_prints_none() {
        let p = IncomingCallPolicy::default();
        let dump = format!("{p:?}");
        assert!(dump.contains("credential_resolver: None"));
    }

    #[test]
    fn peek_dest_call_reads_low_15_bits_of_full_frame() {
        let mut buf = vec![0u8; 12];
        buf[0] = 0x80; // full-frame marker (top bit of source_call high byte).
        buf[2] = 0x36;
        buf[3] = 0x3d; // 0x363d & 0x7fff = 13885
        assert_eq!(peek_dest_call(&buf), Some(13885));
    }

    #[test]
    fn peek_dest_call_masks_retransmit_flag() {
        // dest_call bytes with the high bit set (RETRANS flag) must be masked.
        let mut buf = vec![0u8; 12];
        buf[0] = 0x80;
        buf[2] = 0xff;
        buf[3] = 0xff; // 0xffff & 0x7fff = 0x7fff
        assert_eq!(peek_dest_call(&buf), Some(0x7fff));
    }

    #[test]
    fn peek_dest_call_none_for_mini_frame() {
        let buf = vec![0x00, 0x07, 0x00, 0x00]; // top bit clear => mini
        assert_eq!(peek_dest_call(&buf), None);
    }

    #[test]
    fn peek_dest_call_none_for_short_buffer() {
        assert_eq!(peek_dest_call(&[0x80, 0x00]), None);
        assert_eq!(peek_dest_call(&[]), None);
    }

    /// Build a full IAX frame with the given subclass for `inval_warranted` tests.
    fn iax_full(dest_call: u16, source_call: u16, cmd: IaxCommand) -> Vec<u8> {
        let frame = OwnedFullFrame::with_ies(
            dest_call,
            source_call,
            false,
            0,
            0,
            0,
            FrameType::Iax,
            Subclass::Iax(cmd),
            &Ies::empty(),
        )
        .expect("encode full frame");
        encode_inline(&frame).expect("inline encode")
    }

    #[test]
    fn unknown_dest_ack_does_not_warrant_inval() {
        // A stale ACK landing on a reaped leg's call number must NOT draw an
        // INVAL: that creates the INVAL/ACK storm that deadlocked the reject
        // path (iax-1d7a). ACK is unreliable and elicits no reply (RFC 5456).
        let ack = iax_full(99, 7, IaxCommand::Ack);
        assert!(!inval_warranted(&ack));
    }

    #[test]
    fn unknown_dest_inval_does_not_warrant_inval() {
        // An INVAL is terminal/unanswered — never reply INVAL to an INVAL.
        let inval = iax_full(99, 7, IaxCommand::Inval);
        assert!(!inval_warranted(&inval));
    }

    #[test]
    fn unknown_dest_other_frame_still_warrants_inval() {
        // A stray HANGUP (or any non-ACK/INVAL full frame) on a dead call number
        // still gets the standard INVAL (R-6.9-02).
        let hangup = iax_full(99, 7, IaxCommand::Hangup);
        assert!(inval_warranted(&hangup));
        // Unparseable / mini garbage also warrants the default INVAL.
        assert!(inval_warranted(&[0x00, 0x01]));
    }
}
