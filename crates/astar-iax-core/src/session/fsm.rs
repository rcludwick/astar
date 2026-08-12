// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure session FSM. No I/O, no time, no threads — drive with `handle()`.

use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::frame::{Frame, OwnedFullFrame, Subclass};
use crate::subclass::VoiceFormat;
use crate::text::OwnedTextEvent;

use super::auth::{AuthMethods, Credentials};
use super::call_no::CallNo;
use super::call_profile::CallProfile;
use super::codec_policy::CodecPolicy;
use super::keepalive::KeepaliveState;

/// Returned by [`CallToken::new`] when the supplied bytes cannot be carried in
/// an IE (the length field is a single `u8`, RFC 5456 §8.1). The CALLTOKEN IE
/// (§8.6) is an adversary-controlled anti-spoof nonce, so an over-length token
/// is rejected at construction rather than silently truncated downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTokenTooLong {
    /// The offending length (always `> 255`).
    pub len: usize,
}

impl std::fmt::Display for CallTokenTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CALLTOKEN is {} bytes; IE length field caps it at 255",
            self.len
        )
    }
}

impl std::error::Error for CallTokenTooLong {}

/// Opaque token bytes from RFC 5456 §8.6 (no internal structure mandated).
///
/// A newtype rather than a bare `Vec<u8>` so the IE-length invariant
/// (`len <= 255`) is enforced once, at the wire boundary, by [`CallToken::new`].
/// The bytes are an anti-spoof nonce, not a credential: `Debug` shows the
/// length and a short hex prefix so logs never dump the full (peer-chosen)
/// token, and never anything secret.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct CallToken(Vec<u8>);

impl CallToken {
    /// Validate and wrap token bytes. `Err` ⇒ the bytes exceed the 255-byte
    /// IE length limit and must not reach the encoder.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, CallTokenTooLong> {
        let bytes = bytes.into();
        if bytes.len() > 255 {
            return Err(CallTokenTooLong { len: bytes.len() });
        }
        Ok(Self(bytes))
    }

    /// The raw token bytes, for serialisation into a CALLTOKEN IE.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Number of token bytes (always `<= 255`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` for a present-but-empty token (the bootstrap CALLTOKEN IE).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for CallToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Length + short hex prefix only; never the full nonce.
        write!(f, "CallToken({} bytes", self.0.len())?;
        if let Some(head) = self.0.get(..self.0.len().min(4))
            && !head.is_empty()
        {
            write!(f, ", {:02x}…", FmtHex(head))?;
        }
        write!(f, ")")
    }
}

struct FmtHex<'a>(&'a [u8]);

impl std::fmt::LowerHex for FmtHex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// `AST_FORMAT_*` bitmask of supported voice codecs (advertised in CAPABILITY).
///
/// A newtype over the raw `u32` so call sites query format membership through
/// [`CodecMask::contains`] / mutate through [`CodecMask::set`] instead of
/// open-coding `mask & fmt.as_u32() != 0`. [`CodecMask::from_u32`] /
/// [`CodecMask::get`] bridge the wire (CAPABILITY/FORMAT IEs are raw `u32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CodecMask(u32);

impl CodecMask {
    /// The empty mask (no codecs advertised).
    pub const EMPTY: Self = Self(0);

    /// Wrap a raw `AST_FORMAT_*` bitmask straight off the wire.
    #[must_use]
    pub const fn from_u32(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bitmask, for serialisation into a CAPABILITY IE.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `true` if `fmt`'s format flag is set in this mask.
    #[must_use]
    pub fn contains(self, fmt: VoiceFormat) -> bool {
        self.0 & fmt.as_u32() != 0
    }

    /// Set `fmt`'s format flag.
    pub fn set(&mut self, fmt: VoiceFormat) {
        self.0 |= fmt.as_u32();
    }

    /// `true` if no format flags are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Bitwise intersection of two masks (codecs common to both).
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl std::iter::FromIterator<VoiceFormat> for CodecMask {
    fn from_iter<I: IntoIterator<Item = VoiceFormat>>(iter: I) -> Self {
        let mut mask = Self::EMPTY;
        for fmt in iter {
            mask.set(fmt);
        }
        mask
    }
}

/// Parsed caller-ID context from an inbound NEW, preserved through the
/// handshake so the `IncomingCall` event carries full context (design
/// §"Caller-ID IEs"). Owned (no borrow of the datagram buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingOffer {
    pub called_number: Option<String>,
    pub calling_number: Option<String>,
    pub calling_name: Option<String>,
    pub username: Option<String>,
    pub offered_codecs: CodecMask,
    pub preferred_codec: Option<VoiceFormat>,
    pub language: Option<String>,
    /// The peer's CALLTOKEN IE as received (empty slice = present-but-empty,
    /// `None` = IE absent). Drives the CALLTOKEN policy branch.
    pub peer_calltoken: Option<Vec<u8>>,
}

/// Why an inbound NEW was rejected before an FSM leg was spawned. The
/// `cause()` text is what goes into the REJECT CAUSE IE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferReject {
    MandatoryIeMissing,
    UnsupportedVersion,
}

impl OfferReject {
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::MandatoryIeMissing => "Mandatory IE missing",
            Self::UnsupportedVersion => "Unsupported version",
        }
    }
}

impl IncomingOffer {
    /// Parse + validate the IEs of an inbound NEW. `Err` ⇒ REJECT inline,
    /// never spawn an FSM (design §"Error / edge cases").
    pub fn from_new_ies(ies: &crate::ie::Ies<'_>) -> Result<Self, OfferReject> {
        // R-8.6-02: CALLED_NUMBER required and must be non-empty.
        let called = ies.called_number.filter(|s| !s.is_empty());
        if called.is_none() {
            return Err(OfferReject::MandatoryIeMissing);
        }
        // R-8.6-02: VERSION required and must equal 2.
        match ies.version {
            Some(2) => {}
            _ => return Err(OfferReject::UnsupportedVersion),
        }
        let offered_codecs = CodecMask::from_u32(ies.capability.unwrap_or(0));
        let preferred_codec = ies.format.and_then(VoiceFormat::from_u32);
        Ok(Self {
            called_number: called.map(str::to_string),
            calling_number: ies.calling_number.map(str::to_string),
            calling_name: ies.calling_name.map(str::to_string),
            username: ies.username.map(str::to_string),
            offered_codecs,
            preferred_codec,
            language: ies.language.map(str::to_string),
            peer_calltoken: ies.calltoken.map(<[u8]>::to_vec),
        })
    }
}

/// Vendor-neutral inbound-call policy the pure FSM holds (iax-8baf). The
/// runtime `IncomingCallPolicy` (Phase F) lowers to this; the core never
/// bakes in `AllStar`'s `auth=Required` / `CALLTOKEN=Always` (those are
/// iax-6461's job). Defaults are generic IAX2: no auth, no CALLTOKEN,
/// `AppDecide`, no auto-answer, 30 s decision window, G.711µ.
///
/// The four bool flags are independent IAX2 policy knobs the runtime lowers
/// from its `IncomingCallPolicy` enums; they are not a hidden state machine, so
/// the `struct_excessive_bools` lint does not apply.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct InboundPolicy {
    /// Demand a CALLTOKEN round-trip before proceeding. Neutral default: `false`.
    pub calltoken_required: bool,
    /// Demand MD5 authentication (AUTHREQ/AUTHREP). Neutral default: `false`.
    pub auth_required: bool,
    /// Skip the app decision and ANSWER immediately. Neutral default: `false`.
    pub auto_answer: bool,
    /// `true` ⇒ wait for the app to answer/reject (arm the decision timer);
    /// `false` ⇒ no decision timer is armed. Neutral default: `true` (`AppDecide`).
    pub decision_is_app: bool,
    /// How long to wait for the app's answer/reject before auto-rejecting.
    /// Neutral default: 30 s.
    pub accept_decision_timeout: Duration,
    /// Codec negotiation policy for calls we accept (iax-31f7). Neutral
    /// default: `UlawOnly` (G.711µ preferred), the pre-slin behavior.
    pub codec_policy: CodecPolicy,
}

impl Default for InboundPolicy {
    fn default() -> Self {
        Self {
            calltoken_required: false,
            auth_required: false,
            auto_answer: false,
            decision_is_app: true,
            accept_decision_timeout: Duration::from_secs(30),
            codec_policy: CodecPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HangupOrigin {
    Local,
    Peer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    Rejected {
        cause: Option<String>,
    },
    Timeout {
        in_state: &'static str,
    },
    Aborted,
    /// Peer sent a HANGUP frame. `cause` carries the human-readable CAUSE IE
    /// text when present (RFC 5456 §8.4.7 / Q.931 cause codes). Distinct from
    /// `Aborted` which is reserved for locally-initiated teardowns.
    RemoteHangup {
        cause: Option<String>,
    },
    /// Peer answered with INVAL — it has no state for this call, e.g. it
    /// restarted (RFC 5456 §6.9.2). Hard-fail; no automatic re-establish.
    PeerInval,
    InvalidTransition {
        from: &'static str,
        on: &'static str,
    },
}

impl std::fmt::Display for FailReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { cause: Some(c) } => write!(f, "rejected: {c}"),
            Self::Rejected { cause: None } => write!(f, "rejected"),
            Self::Timeout { in_state } => write!(f, "peer timed out (in {in_state})"),
            Self::Aborted => write!(f, "call aborted locally"),
            Self::RemoteHangup { cause: Some(c) } => write!(f, "peer hung up: {c}"),
            Self::RemoteHangup { cause: None } => write!(f, "peer hung up"),
            Self::PeerInval => write!(f, "peer has no state for this call (INVAL)"),
            Self::InvalidTransition { from, on } => {
                write!(f, "invalid transition from {from} on {on}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub attempts: u8,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    /// Dialled extension from `StartCall`. Preserved so NEW retransmits
    /// (timer or post-CALLTOKEN) re-use the same called-number IE.
    pub dest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTokenReceivedData {
    pub token: CallToken,
    pub received_at: Instant,
    pub our_call: CallNo,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    pub dest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewResentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub token: CallToken,
    pub attempts: u8,
    pub capabilities: CodecMask,
    pub ping_seq: u8,
    pub dest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthReqReceivedData {
    pub challenge: Vec<u8>,
    pub methods: AuthMethods,
    pub our_call: CallNo,
    pub peer_call: CallNo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRepSentData {
    pub sent_at: Instant,
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub attempts: u8,
    /// Cached so the `AuthRepRetry` timer can re-send AUTHREP per the spec
    /// table without re-receiving the challenge.
    pub challenge: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveData {
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub established_at: Instant,
    /// Set on inbound `DtmfBegin`; cleared on inbound `DtmfEnd` (or on
    /// a second `DtmfBegin` superseding the first). Receive-only state.
    pub pending_dtmf: Option<char>,
    /// Last outbound `SendDtmf` instant. Drives the 50 ms per-call
    /// rate limit. `None` immediately after entering `Active`.
    pub last_dtmf_at: Option<Instant>,
    /// Liveness / RTT bookkeeping (iax-a307). Constructed on entry to
    /// `Active`; driven by `TimerKind::Keepalive` and inbound frames.
    pub keepalive: KeepaliveState,
    /// Timestamp (full 32-bit, ms) of the last outbound FULL voice frame, and
    /// the codec it carried. `None` until the first voice frame is sent. Mini
    /// frames inherit the high 16 ts bits + codec from this; a full frame must
    /// be re-sent when either would change. (iax-a116)
    pub last_full_voice: Option<(VoiceFormat, u32)>,
    /// Last PTT state we emitted a `RADIO_KEY`/`UNKEY` control frame for, so we
    /// coalesce — don't re-send when the app sets the same state again.
    /// `None` until the first `SendPtt` in this Active session. (iax-d4e9)
    pub last_ptt: Option<bool>,
    /// Codec of the last INBOUND full voice frame on this leg. RFC 5456 §6.4:
    /// a mini voice frame carries no subclass, so its implicit format is the
    /// format of the most recent full voice frame received on the same leg.
    /// `None` until the first inbound full voice frame; the mini handler falls
    /// back to G.711µ in that (spec-degenerate) case. Distinct from
    /// `last_full_voice`, which tracks the OUTBOUND send-side format/ts. (iax-a422)
    pub last_rx_voice_format: Option<VoiceFormat>,
    /// TX format negotiated for this call: outbound = the ACCEPT's FORMAT IE,
    /// inbound = the format we chose for our own ACCEPT (iax-31f7).
    pub negotiated_format: VoiceFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HangupData {
    pub our_call: CallNo,
    pub peer_call: CallNo,
    pub initiated_by: HangupOrigin,
    pub sent_at: Instant,
    pub attempts: u8,
}

// --- Inbound (callee) state data (iax-8baf) -------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReceivedData {
    pub peer_call: CallNo,
    pub our_call: CallNo,
    pub offered: IncomingOffer,
    pub received_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTokenIssuedData {
    pub peer_call: CallNo,
    pub our_call: CallNo,
    pub offered: IncomingOffer,
    pub token: CallToken,
    pub issued_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthReqSentData {
    pub peer_call: CallNo,
    pub our_call: CallNo,
    pub challenge: Vec<u8>,
    pub attempts: u8,
    pub sent_at: Instant,
    pub offered: IncomingOffer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptSentData {
    pub peer_call: CallNo,
    pub our_call: CallNo,
    pub chosen_format: VoiceFormat,
    pub attempts: u8,
    pub sent_at: Instant,
    /// true if policy=AppDecide and we sent ACCEPT but the app hasn't
    /// answered/rejected yet. Drives the `AcceptDecisionTimeout` auto-reject.
    pub awaiting_app_decision: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerSentData {
    pub peer_call: CallNo,
    pub our_call: CallNo,
    pub chosen_format: VoiceFormat,
    pub attempts: u8,
    pub sent_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Init,
    NewSent(NewSentData),
    CallTokenReceived(CallTokenReceivedData),
    NewResent(NewResentData),
    AuthReqReceived(AuthReqReceivedData),
    AuthRepSent(AuthRepSentData),
    Active(ActiveData),
    Hangup(HangupData),
    Closed,
    Failed(FailReason),
    // Inbound (callee) handshake states (iax-8baf).
    NewReceived(NewReceivedData),
    CallTokenIssued(CallTokenIssuedData),
    AuthReqSent(AuthReqSentData),
    AcceptSent(AcceptSentData),
    AnswerSent(AnswerSentData),
}

impl SessionState {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init => "Init",
            Self::NewSent(_) => "NewSent",
            Self::CallTokenReceived(_) => "CallTokenReceived",
            Self::NewResent(_) => "NewResent",
            Self::AuthReqReceived(_) => "AuthReqReceived",
            Self::AuthRepSent(_) => "AuthRepSent",
            Self::Active(_) => "Active",
            Self::Hangup(_) => "Hangup",
            Self::Closed => "Closed",
            Self::Failed(_) => "Failed",
            Self::NewReceived(_) => "NewReceived",
            Self::CallTokenIssued(_) => "CallTokenIssued",
            Self::AuthReqSent(_) => "AuthReqSent",
            Self::AcceptSent(_) => "AcceptSent",
            Self::AnswerSent(_) => "AnswerSent",
        }
    }
}

#[derive(Debug, Clone)]
pub enum AppCommand {
    StartCall {
        dest: String,
        now: Instant,
    },
    Hangup {
        cause: Option<String>,
        now: Instant,
    },
    SendVoice {
        format: VoiceFormat,
        payload: Vec<u8>,
        ts: u32,
    },
    SendDtmf {
        digit: char,
        now: Instant,
    },
    SendPtt(bool),
    SendText(String),
    // Inbound (callee) seams (iax-8baf).
    /// Listener kick: produce `NewReceived`'s first actions. The NEW datagram
    /// was already consumed by the Listener for demux, so there is no inbound
    /// frame to feed; this drives the entropy-seeded first transition.
    DriveInbound {
        now: Instant,
    },
    AcceptIncoming {
        now: Instant,
    },
    AnswerIncoming {
        now: Instant,
    },
    RejectIncoming {
        cause: Option<String>,
        now: Instant,
    },
    /// Reliability released our ANSWER -> `AnswerSent` -> `Active` (Decision §2).
    AnswerAcked {
        now: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    NewRetry,
    HangupRetry,
    AuthRepRetry,
    TokenExpiry,
    /// PING/LAGRQ cadence + inbound-silence check while Active (iax-a307).
    Keepalive,
    /// Retry the initial REGREQ (no auth yet).
    RegReqRetry,
    /// Retry the post-auth REGREQ (carries `MD5_RESULT` or `PASSWORD`).
    RegAuthRetry,
    /// RFC 5456 §8.6: 10s ceiling on the CALLTOKEN dance for a registration.
    RegTokenExpiry,
    /// Server-returned REFRESH interval (jittered) fires this to start a refresh round.
    RegRefresh,
    /// Retry an outbound REGREL during best-effort deregistration.
    RegRelRetry,
    // Inbound (callee) timers (iax-8baf).
    InboundTokenExpiry,
    AuthReqRetry,
    AcceptRetry,
    AnswerRetry,
    AcceptDecisionTimeout,
}

#[derive(Debug)]
pub enum Event<'a> {
    App(AppCommand),
    Frame { frame: Frame<'a>, now: Instant },
    Timer { kind: TimerKind, now: Instant },
    DeliveryFailed { oseqno: u8 },
}

#[derive(Debug)]
pub enum Action {
    SendReliable(OwnedFullFrame),
    SendUnreliable(Vec<u8>),
    SetTimer(TimerKind, Duration),
    CancelTimer(TimerKind),
    AppEvent(AppEvent),
    /// iax-e402: signal to the runtime that the peer's chosen scallno is now
    /// known. The runtime must call `Reliability::set_peer_call(_)` before
    /// enqueueing the next reliable frame so `OSeqno` / ACK / `dest_call`
    /// bookkeeping addresses the right peer call (RFC 5456 §8.6.1). Emitted
    /// ahead of any `SendReliable` produced by the same transition.
    SetPeerCall(CallNo),
    /// iax-8baf: tell the runtime to reset its `Reliability` sequence-number
    /// state (`Reliability::reset`) for this leg. Emitted on the inbound
    /// CALLTOKEN path: once the peer's resent NEW carries the echoed token, the
    /// real call leg begins, so the throwaway seqno bookkeeping from the token
    /// dance must be cleared before the AUTHREQ/ACCEPT that follows. The runtime
    /// calls `rel.reset()` on it ahead of any `SendReliable` from the same
    /// transition (mirrors `SetPeerCall`).
    ResetReliability,
    LogInvalid {
        reason: &'static str,
    },
}

/// Call-setup progress signalled by the callee via CONTROL frames
/// (RFC 5456 §6.3). Surfaced to the app on the [`AppEvent`] channel between
/// `Connected` (the transport handshake completed on ACCEPT) and steady-state
/// media. `Answered` is the real "the far end picked up" signal — `Connected`
/// only means "the callee accepted our NEW".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallProgress {
    /// `AST_CONTROL_PROCEEDING` (15): the call is being processed.
    Proceeding,
    /// `AST_CONTROL_RINGING` (3): the far end is alerting (ringback).
    Ringing,
    /// `AST_CONTROL_ANSWER` (4): the far end answered — the call is up.
    Answered,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    Connected {
        peer_call: CallNo,
    },
    /// Intermediate call-setup progress (RFC 5456 §6.3 PROCEEDING / RINGING /
    /// ANSWER). Edge-triggered: emitted once per CONTROL frame received.
    /// Internal-channel + console only for now (no FFI surfacing — iax-85e7).
    CallProgress(CallProgress),
    Disconnected {
        reason: FailReason,
    },
    VoiceReceived {
        format: VoiceFormat,
        payload: Vec<u8>,
        ts: u32,
    },
    DtmfReceived(char),
    RemotePtt(bool),
    TextReceived(OwnedTextEvent),
    /// Keepalive: inbound silence exceeded `lost_after`. Edge-triggered —
    /// emitted once per loss episode; the call is NOT torn down (iax-a307).
    ConnectionLost,
    /// Keepalive: first inbound frame after a `ConnectionLost` (iax-a307).
    ConnectionRestored,
    /// An inbound NEW was demuxed and parsed; surface the offer to the app
    /// so it can accept/answer/reject (iax-8baf). `peer_addr` is intentionally
    /// NOT on the FSM event — the runtime Listener holds it per leg and stamps
    /// it onto the public `IncomingCall` struct, keeping the FSM I/O-free.
    IncomingCall {
        our_call: CallNo,
        peer_call: CallNo,
        calling_number: Option<String>,
        calling_name: Option<String>,
        called_number: Option<String>,
        username: Option<String>,
        offered_codecs: CodecMask,
        preferred_codec: Option<VoiceFormat>,
        language: Option<String>,
    },
}

pub struct Fsm {
    state: SessionState,
    pub(super) credentials: Credentials,
    pub(super) our_call: CallNo,
    pub(super) call_profile: CallProfile,
    /// Per-leg CALLTOKEN bytes the runtime seeds via [`Fsm::seed_entropy`]
    /// before the first inbound `handle` (the FSM is pure and never calls
    /// `OsRng` itself — design Accepted decision §3). Consumed by the inbound
    /// `NewReceived` handler in Phase D. Not a secret (opaque anti-spoof
    /// nonce) and never logged: `Fsm` derives no `Debug`/serialization.
    ///
    /// Written by `seed_entropy`; read by the Phase D inbound handlers.
    pub(super) pending_token: Option<CallToken>,
    /// Per-leg AUTHREQ challenge (hex) seeded the same way as `pending_token`.
    /// A public nonce, not a credential.
    pub(super) pending_challenge: Option<String>,
    /// Vendor-neutral inbound policy (iax-8baf). Defaults to a generic IAX2
    /// callee (no auth, no CALLTOKEN, `AppDecide`). The runtime lowers its
    /// `IncomingCallPolicy` to this via [`Fsm::with_inbound_policy`].
    pub(super) inbound_policy: InboundPolicy,
}

impl Fsm {
    #[must_use]
    pub fn new(credentials: Credentials, our_call: CallNo) -> Self {
        Self {
            state: SessionState::Init,
            credentials,
            our_call,
            call_profile: CallProfile::default(),
            pending_token: None,
            pending_challenge: None,
            inbound_policy: InboundPolicy::default(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Smoothed round-trip estimate from keepalive PONG/LAGRP echoes.
    /// `None` unless the call is `Active` and at least one echo arrived.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        match &self.state {
            SessionState::Active(s) => s.keepalive.rtt(),
            _ => None,
        }
    }

    /// The voice format negotiated for this call, once known. Outbound: the
    /// peer ACCEPT's FORMAT. Inbound: the format we chose for our ACCEPT.
    /// `None` until negotiation completes (iax-31f7).
    #[must_use]
    pub fn negotiated_format(&self) -> Option<VoiceFormat> {
        match &self.state {
            SessionState::Active(d) => Some(d.negotiated_format),
            SessionState::AcceptSent(d) => Some(d.chosen_format),
            SessionState::AnswerSent(d) => Some(d.chosen_format),
            _ => None,
        }
    }

    /// Test-only constructor. Not part of the stable API.
    #[doc(hidden)]
    #[must_use]
    pub fn with_state(state: SessionState, credentials: Credentials, our_call: CallNo) -> Self {
        Self {
            state,
            credentials,
            our_call,
            call_profile: CallProfile::default(),
            pending_token: None,
            pending_challenge: None,
            inbound_policy: InboundPolicy::default(),
        }
    }

    /// Set the outbound-NEW call profile (web-transceiver vs standard). The
    /// high-level client lowers its `CallMode` to this. Call before the first
    /// `StartCall`. iax-3fca.
    #[must_use]
    pub fn with_call_profile(mut self, profile: CallProfile) -> Self {
        self.call_profile = profile;
        self
    }

    /// Construct an inbound (callee) FSM seeded in `NewReceived`. The Listener
    /// calls this after a syntactically-valid NEW is demuxed and the
    /// `IncomingOffer` is parsed (design §Refactors item 2). `peer_call` is the
    /// peer's `source_call`; `our_call` is freshly allocated.
    #[must_use]
    pub fn for_inbound(
        credentials: Credentials,
        our_call: CallNo,
        peer_call: CallNo,
        offered: IncomingOffer,
        now: Instant,
    ) -> Self {
        Self {
            state: SessionState::NewReceived(NewReceivedData {
                peer_call,
                our_call,
                offered,
                received_at: now,
            }),
            credentials,
            our_call,
            call_profile: CallProfile::default(),
            pending_token: None,
            pending_challenge: None,
            inbound_policy: InboundPolicy::default(),
        }
    }

    /// Override the vendor-neutral inbound policy (iax-8baf). The runtime lowers
    /// its `IncomingCallPolicy` to an [`InboundPolicy`] and calls this right
    /// after [`Fsm::for_inbound`], before the first `handle`. Mirrors
    /// [`Fsm::with_call_profile`].
    #[must_use]
    pub fn with_inbound_policy(mut self, policy: InboundPolicy) -> Self {
        self.inbound_policy = policy;
        self
    }

    /// Seed the per-leg randomness the inbound handlers consume (CALLTOKEN
    /// bytes + AUTHREQ challenge). The runtime (Listener) calls this **once**,
    /// right after [`Fsm::for_inbound`] and before the first `handle`, so the
    /// pure FSM never touches `OsRng` (design Accepted decision §3). Keeps
    /// unit tests deterministic (fixed seeded bytes). Neither value is a
    /// credential; both are public anti-spoof / challenge nonces.
    pub fn seed_entropy(&mut self, token16: [u8; 16], challenge_hex: String) {
        // A 16-byte nonce is always within the IE limit; `new` cannot fail.
        self.pending_token = Some(CallToken::new(token16).expect("16 bytes <= 255"));
        self.pending_challenge = Some(challenge_hex);
    }

    /// Drive the FSM. Default behavior: unknown (state, event) pairs leave
    /// state unchanged and emit a single `LogInvalid` action. Subsequent
    /// tasks layer real transitions over this default.
    pub fn handle(&mut self, event: Event<'_>) -> SmallVec<[Action; 4]> {
        let st = std::mem::replace(&mut self.state, SessionState::Init);
        // iax-6c21: reliable delivery gave up (RxOutcome::GaveUp). The peer is
        // unreachable, so no per-state handler can usefully act on it — handle it
        // centrally and terminate the call. Pre-Active states FAIL; Active tears
        // down to a clean terminal state. Done before per-state dispatch so a new
        // handler can never accidentally swallow it into a LogInvalid catch-all.
        if let Event::DeliveryFailed { oseqno } = event {
            let (next, actions) = self.on_delivery_failed(st, oseqno);
            self.state = next;
            return actions;
        }
        let (next, actions) = match st {
            SessionState::Init => self.on_init(event),
            SessionState::NewSent(s) => self.on_new_sent(s, event),
            SessionState::CallTokenReceived(s) => self.on_calltoken_received(s, event),
            SessionState::NewResent(s) => self.on_new_resent(s, event),
            SessionState::AuthReqReceived(s) => self.on_auth_req_received(s, event),
            SessionState::AuthRepSent(s) => self.on_auth_rep_sent(s, event),
            SessionState::Active(s) => self.on_active(s, event),
            SessionState::Hangup(s) => self.on_hangup(s, event),
            SessionState::Closed => self.on_closed(event),
            SessionState::Failed(reason) => self.on_failed(reason, event),
            SessionState::NewReceived(s) => self.on_new_received(s, event),
            SessionState::CallTokenIssued(s) => self.on_calltoken_issued(s, event),
            SessionState::AuthReqSent(s) => self.on_auth_req_sent(s, event),
            SessionState::AcceptSent(s) => self.on_accept_sent(s, event),
            SessionState::AnswerSent(s) => self.on_answer_sent(s, event),
        };
        self.state = next;
        actions
    }
}

pub(super) fn invalid_reason(_state: &SessionState, _event: &Event<'_>) -> &'static str {
    "invalid_transition"
}

pub(super) fn full_subclass(frame: &Frame<'_>) -> Option<(Subclass, u16, Vec<u8>)> {
    if let Frame::Full(f) = frame {
        let mut bytes = Vec::new();
        // Frame came from `parse`, so every IE payload is bounded by the
        // wire's u8 length prefix and re-encoding cannot overflow.
        f.ies
            .encode(&mut bytes)
            .expect("parsed Frame IEs are bounded by u8 wire length");
        Some((f.subclass, f.source_call, bytes))
    } else {
        None
    }
}

/// Per-call outbound DTMF rate-limit floor (20 Hz ceiling).
///
/// Defensive cap protecting against runaway UI loops that would otherwise
/// spam `app_rpt` and trigger hub-side anti-spam quarantine. The 50 ms floor
/// aligns with the 100 ms BEGIN→END hold so back-to-back digits' frame
/// pairs do not overlap.
pub(super) const DTMF_RATE_LIMIT: Duration = Duration::from_millis(50);

/// RFC 5456 §6.7 valid DTMF subclass digits.
pub(super) const fn is_valid_dtmf_digit(c: char) -> bool {
    matches!(c, '0'..='9' | '*' | '#' | 'A'..='D')
}

/// Truncate `s` to the longest codepoint-aligned prefix of at most
/// `max_bytes` bytes. Used when constructing IEs from user-supplied
/// strings (cause, dest, username) so we never feed `Ies::encode` a
/// value that would overflow the 255-byte wire limit (iax-545b).
///
/// `str::char_indices` yields `(byte_index, char)` pairs; the last
/// `byte_index` ≤ `max_bytes` is the largest valid split point. We then
/// slice up to that index plus the length of the character that *starts*
/// there only if it still fits — otherwise we stop before it.
pub(super) fn truncate_to_codepoint_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk codepoints; track the largest prefix end that still fits.
    let mut end = 0;
    for (idx, ch) in s.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &s[..end]
}

#[cfg(test)]
mod fsm_tests {
    use super::*;
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::session::auth::{AuthMethods, Credentials, Secret};
    use crate::session::builders::{build_dtmf_begin, build_dtmf_end, build_radio_key};
    use crate::subclass::{ControlSubclass, IaxCommand};

    // iax-994b: CALLTOKEN must reject bytes that won't fit a u8 length prefix.
    #[test]
    fn calltoken_rejects_over_255_bytes() {
        assert!(CallToken::new(vec![0u8; 255]).is_ok());
        let err = CallToken::new(vec![0u8; 256]).unwrap_err();
        assert_eq!(err.len, 256);
        let empty = CallToken::new(Vec::new()).unwrap();
        assert!(empty.is_empty());
        let tok = CallToken::new(vec![1, 2, 3]).unwrap();
        assert_eq!(tok.as_bytes(), &[1, 2, 3]);
        assert_eq!(tok.len(), 3);
        // Debug never dumps the full nonce.
        assert!(format!("{tok:?}").starts_with("CallToken(3 bytes"));
    }

    // iax-994b: CodecMask membership/mutation without raw bit-twiddling.
    #[test]
    fn codec_mask_set_get_contains() {
        let mut m = CodecMask::EMPTY;
        assert!(m.is_empty());
        m.set(VoiceFormat::G711U);
        assert!(m.contains(VoiceFormat::G711U));
        assert!(!m.contains(VoiceFormat::G711A));
        assert_eq!(m.get(), VoiceFormat::G711U.as_u32());
        let both: CodecMask = [VoiceFormat::G711U, VoiceFormat::G711A]
            .into_iter()
            .collect();
        assert_eq!(both.intersect(m), m);
        assert_eq!(CodecMask::from_u32(0b1100).get(), 0b1100);
    }

    // iax-545b: `truncate_to_codepoint_boundary` must never split a
    // codepoint, so the resulting `&str` is always valid UTF-8 and short
    // enough to feed `Ies::encode` without overflow.
    #[test]
    fn truncate_codepoint_boundary_does_not_split_multibyte() {
        // 254 ASCII + one 4-byte codepoint = 258 bytes. With max=255 we
        // must drop the 4-byte codepoint (it can't fit) — never split it.
        let mut s = "a".repeat(254);
        s.push('\u{1F600}');
        let truncated = truncate_to_codepoint_boundary(&s, 255);
        assert_eq!(truncated.len(), 254);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    // iax-1c24: FailReason has a human-readable Display so consumers stop
    // leaking the Rust Debug repr into user-facing status / error strings.
    #[test]
    fn fail_reason_display_is_human_readable() {
        assert_eq!(
            FailReason::Rejected {
                cause: Some("congestion".into())
            }
            .to_string(),
            "rejected: congestion"
        );
        assert_eq!(FailReason::Rejected { cause: None }.to_string(), "rejected");
        assert_eq!(
            FailReason::Timeout {
                in_state: "NewSent"
            }
            .to_string(),
            "peer timed out (in NewSent)"
        );
        assert_eq!(FailReason::Aborted.to_string(), "call aborted locally");
        assert_eq!(
            FailReason::RemoteHangup {
                cause: Some("normal clearing".into())
            }
            .to_string(),
            "peer hung up: normal clearing"
        );
        assert_eq!(
            FailReason::RemoteHangup { cause: None }.to_string(),
            "peer hung up"
        );
        assert_eq!(
            FailReason::PeerInval.to_string(),
            "peer has no state for this call (INVAL)"
        );
        assert_eq!(
            FailReason::InvalidTransition {
                from: "Active",
                on: "App"
            }
            .to_string(),
            "invalid transition from Active on App"
        );
    }

    #[test]
    fn truncate_codepoint_boundary_passes_through_short_strings() {
        let s = "hello";
        assert_eq!(truncate_to_codepoint_boundary(s, 255), "hello");
    }

    #[test]
    fn truncate_codepoint_boundary_keeps_codepoint_that_fits_at_limit() {
        // 252 ASCII + one 3-byte codepoint (snowman \u{2603}) = 255 bytes
        // exactly. Whole codepoint must be kept.
        let mut s = "a".repeat(252);
        s.push('\u{2603}');
        assert_eq!(s.len(), 255);
        let truncated = truncate_to_codepoint_boundary(&s, 255);
        assert_eq!(truncated.len(), 255);
        assert!(truncated.ends_with('\u{2603}'));
    }

    #[test]
    fn for_inbound_starts_in_new_received_with_offer() {
        let our = CallNo::new(16379).unwrap();
        let peer = CallNo::new(13885).unwrap();
        let offer = IncomingOffer {
            called_number: Some("s".into()),
            calling_number: None,
            calling_name: None,
            username: Some("rob".into()),
            offered_codecs: CodecMask::from_u32(VoiceFormat::G711U.as_u32()),
            preferred_codec: Some(VoiceFormat::G711U),
            language: None,
            peer_calltoken: Some(Vec::new()),
        };
        let now = Instant::now();
        let f = Fsm::for_inbound(creds(), our, peer, offer.clone(), now);
        match f.state() {
            SessionState::NewReceived(d) => {
                assert_eq!(d.our_call, our);
                assert_eq!(d.peer_call, peer);
                assert_eq!(d.offered, offer);
            }
            other => panic!("expected NewReceived, got {other:?}"),
        }
    }

    #[test]
    fn incoming_offer_parses_caller_id_and_validates_required_ies() {
        let ies = Ies {
            called_number: Some("s"),
            calling_number: Some("1001"),
            calling_name: Some("Rob"),
            username: Some("astartest_notok"),
            capability: Some(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            language: Some("en"),
            calltoken: Some(b""),
            ..Ies::empty()
        };
        let offer = IncomingOffer::from_new_ies(&ies).expect("valid NEW");
        assert_eq!(offer.called_number.as_deref(), Some("s"));
        assert_eq!(offer.calling_name.as_deref(), Some("Rob"));
        assert_eq!(offer.username.as_deref(), Some("astartest_notok"));
        assert_eq!(offer.preferred_codec, Some(VoiceFormat::G711U));
        assert_eq!(offer.peer_calltoken.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn incoming_offer_rejects_missing_called_number() {
        let ies = Ies {
            version: Some(2),
            capability: Some(4),
            ..Ies::empty()
        };
        assert!(matches!(
            IncomingOffer::from_new_ies(&ies),
            Err(OfferReject::MandatoryIeMissing)
        ));
    }

    #[test]
    fn incoming_offer_rejects_bad_version() {
        let ies = Ies {
            called_number: Some("s"),
            version: Some(3),
            capability: Some(4),
            ..Ies::empty()
        };
        assert!(matches!(
            IncomingOffer::from_new_ies(&ies),
            Err(OfferReject::UnsupportedVersion)
        ));
    }

    fn creds() -> Credentials {
        Credentials {
            username: "rob".to_string(),
            password: std::sync::Arc::new(Secret::new("hunter2".to_string())),
            allowed_methods: AuthMethods::MD5,
        }
    }

    fn fsm() -> Fsm {
        Fsm::new(creds(), CallNo::new(1).unwrap())
    }

    use crate::frame::{Frame, FullFrame};
    use crate::subclass::FrameType;

    fn peer_frame(
        oseqno: u8,
        iseqno: u8,
        subclass: Subclass,
        frame_type: FrameType,
        ies: Ies<'static>,
    ) -> Frame<'static> {
        Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno,
            iseqno,
            frame_type,
            subclass,
            ies,
            payload: &[],
        }))
    }

    fn drive_to_new_sent() -> (Fsm, Instant) {
        let mut f = fsm();
        let now = Instant::now();
        let _ = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".to_string(),
            now,
        }));
        (f, now)
    }

    #[test]
    fn new_sent_accept_transitions_to_active() {
        // No-auth, no-CALLTOKEN server: ACCEPT arrives directly in NewSent and
        // must connect the call (iax-64b6). Previously it fell through to the
        // invalid-frame arm and the call timed out in NewSent — a plain
        // node-as-handset listener (auth=Off) was unreachable from the caller.
        let (mut f, now) = drive_to_new_sent();
        let accept = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::Accept),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame {
            frame: accept,
            now: now + Duration::from_millis(5),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. }))),
            "no-auth ACCEPT in NewSent must emit Connected"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn accept_format_is_retained_and_exposed() {
        // iax-31f7: the peer's ACCEPT names the format it will send/expect.
        // Drive an Fsm configured to prefer Slin through StartCall, then feed
        // it an ACCEPT whose FORMAT IE carries Slin; the FSM must remember it
        // and expose it via Fsm::negotiated_format().
        use crate::session::CodecPolicy;
        use crate::session::call_profile::CallProfile;
        let profile = CallProfile {
            codec_policy: CodecPolicy::PreferSlin,
            ..CallProfile::default()
        };
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap()).with_call_profile(profile);
        let now = Instant::now();
        let _ = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".to_string(),
            now,
        }));
        let ies = Ies {
            format: Some(VoiceFormat::Slin.as_u32()),
            ..Ies::empty()
        };
        let accept = peer_frame(0, 1, Subclass::Iax(IaxCommand::Accept), FrameType::Iax, ies);
        let _ = f.handle(Event::Frame {
            frame: accept,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(f.state(), SessionState::Active(_)));
        assert_eq!(f.negotiated_format(), Some(VoiceFormat::Slin));
    }

    #[test]
    fn accept_with_unsupported_format_falls_back_to_preference() {
        // A peer ACCEPTing with a format we never offered (or can't encode)
        // violates the exchange, but we degrade instead of hanging up: fall
        // back to our policy preference and trace the anomaly.
        use crate::session::CodecPolicy;
        use crate::session::call_profile::CallProfile;
        let profile = CallProfile {
            codec_policy: CodecPolicy::PreferSlin,
            ..CallProfile::default()
        };
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap()).with_call_profile(profile);
        let now = Instant::now();
        let _ = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".to_string(),
            now,
        }));
        let ies = Ies {
            format: Some(VoiceFormat::Gsm.as_u32()),
            ..Ies::empty()
        };
        let accept = peer_frame(0, 1, Subclass::Iax(IaxCommand::Accept), FrameType::Iax, ies);
        let actions = f.handle(Event::Frame {
            frame: accept,
            now: now + Duration::from_millis(5),
        });
        assert_eq!(
            f.negotiated_format(),
            Some(VoiceFormat::Slin),
            "unsupported ACCEPT format falls back to policy preference"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::LogInvalid {
                    reason: "accept_format_unsupported"
                }
            )),
            "unsupported ACCEPT format must be traced"
        );
    }

    #[test]
    fn new_sent_reject_fails_fast() {
        // A peer rejecting the initial (no-auth) NEW fails the call immediately
        // instead of retrying NEW until the NewSent timeout (iax-64b6).
        let (mut f, now) = drive_to_new_sent();
        let ies = Ies {
            cause: Some("congestion"),
            ..Ies::empty()
        };
        let reject = peer_frame(0, 1, Subclass::Iax(IaxCommand::Reject), FrameType::Iax, ies);
        let _ = f.handle(Event::Frame { frame: reject, now });
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Rejected { cause: Some(_) })
        ));
    }

    #[test]
    fn new_sent_calltoken_transitions_to_new_resent_with_token_in_frame() {
        let (mut f, now) = drive_to_new_sent();
        let token = b"opaque-token";
        let ies = Ies {
            calltoken: Some(token),
            ..Ies::empty()
        };
        let frame = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::CallToken),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });
        let mut saw_new = false;
        for a in &actions {
            if let Action::SendReliable(f2) = a
                && matches!(f2.subclass, Subclass::Iax(IaxCommand::New))
            {
                let ies = Ies::parse(&f2.ie_bytes).unwrap();
                assert_eq!(ies.calltoken, Some(&token[..]));
                saw_new = true;
            }
        }
        assert!(saw_new, "NEW resent with populated CALLTOKEN");
        assert!(matches!(f.state(), SessionState::NewResent(_)));
    }

    #[test]
    fn new_sent_authreq_transitions_to_authrep_sent_with_md5() {
        let (mut f, now) = drive_to_new_sent();
        let ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let frame = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame,
            now: now + Duration::from_millis(60),
        });
        let mut saw_authrep = false;
        for a in &actions {
            if let Action::SendReliable(f2) = a
                && matches!(f2.subclass, Subclass::Iax(IaxCommand::AuthRep))
            {
                let ies = Ies::parse(&f2.ie_bytes).unwrap();
                assert_eq!(
                    ies.md5_result,
                    Some("10630b248e56142621b9910fbe08b6f9"),
                    "md5(c0ffee || hunter2) hex"
                );
                saw_authrep = true;
            }
        }
        assert!(saw_authrep);
        assert!(matches!(f.state(), SessionState::AuthRepSent(_)));
    }

    #[test]
    fn new_sent_retry_after_timer() {
        let (mut f, now) = drive_to_new_sent();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::NewRetry,
            now: now + Duration::from_secs(1),
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(f2)
            if matches!(f2.subclass, Subclass::Iax(IaxCommand::New)))));
        assert!(matches!(
            f.state(),
            SessionState::NewSent(NewSentData { attempts: 2, .. })
        ));
    }

    #[test]
    fn new_sent_timeout_after_max_attempts() {
        let (mut f, now) = drive_to_new_sent();
        for i in 1..=5 {
            f.handle(Event::Timer {
                kind: TimerKind::NewRetry,
                now: now + Duration::from_secs(i),
            });
        }
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "NewSent"
            })
        ));
    }

    #[test]
    fn start_call_from_init_emits_new_with_empty_calltoken_and_timer() {
        let mut f = fsm();
        let now = Instant::now();
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".to_string(),
            now,
        }));
        // Must have at least: SendReliable(NEW), SetTimer(NewRetry, 1s).
        let mut saw_new = false;
        let mut saw_timer = false;
        for a in &actions {
            match a {
                Action::SendReliable(frame) => {
                    assert!(matches!(frame.subclass, Subclass::Iax(IaxCommand::New)));
                    let ies = Ies::parse(&frame.ie_bytes).expect("parse ies");
                    assert_eq!(ies.calltoken, Some(&[][..]), "empty CALLTOKEN IE present");
                    assert_eq!(ies.username, Some("rob"));
                    saw_new = true;
                }
                Action::SetTimer(TimerKind::NewRetry, d) => {
                    assert_eq!(*d, Duration::from_secs(1));
                    saw_timer = true;
                }
                _ => {}
            }
        }
        assert!(saw_new && saw_timer);
        assert!(matches!(f.state(), SessionState::NewSent(_)));
    }

    fn drive_to_new_resent() -> (Fsm, Instant) {
        let (mut f, now) = drive_to_new_sent();
        let token_ies = Ies {
            calltoken: Some(b"tok"),
            ..Ies::empty()
        };
        let token_frame = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::CallToken),
            FrameType::Iax,
            token_ies,
        );
        let _ = f.handle(Event::Frame {
            frame: token_frame,
            now: now + Duration::from_millis(10),
        });
        (f, now + Duration::from_millis(10))
    }

    #[test]
    fn new_resent_token_expiry_fails() {
        let (mut f, now) = drive_to_new_resent();
        let _ = f.handle(Event::Timer {
            kind: TimerKind::TokenExpiry,
            now: now + Duration::from_secs(10),
        });
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "NewResent"
            })
        ));
    }

    #[test]
    fn new_resent_authreq_clears_both_timers() {
        let (mut f, now) = drive_to_new_resent();
        let ies = Ies {
            challenge: Some("x"),
            authmethods: Some(2),
            ..Ies::empty()
        };
        let auth = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame: auth,
            now: now + Duration::from_millis(5),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::NewRetry)))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::TokenExpiry)))
        );
        assert!(matches!(f.state(), SessionState::AuthRepSent(_)));
    }

    fn drive_to_authrep_sent() -> (Fsm, Instant) {
        let (mut f, now) = drive_to_new_sent();
        let ies = Ies {
            challenge: Some("x"),
            authmethods: Some(2),
            ..Ies::empty()
        };
        let auth = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
            ies,
        );
        let _ = f.handle(Event::Frame { frame: auth, now });
        (f, now)
    }

    #[test]
    fn authrep_sent_accept_transitions_to_active() {
        let (mut f, now) = drive_to_authrep_sent();
        let accept = peer_frame(
            1,
            2,
            Subclass::Iax(IaxCommand::Accept),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame {
            frame: accept,
            now: now + Duration::from_millis(5),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. })))
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn authrep_sent_reject_fails_with_cause() {
        let (mut f, now) = drive_to_authrep_sent();
        let ies = Ies {
            cause: Some("bad password"),
            ..Ies::empty()
        };
        let reject = peer_frame(1, 2, Subclass::Iax(IaxCommand::Reject), FrameType::Iax, ies);
        let _ = f.handle(Event::Frame { frame: reject, now });
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Rejected { cause: Some(_) })
        ));
    }

    fn drive_to_active() -> (Fsm, Instant) {
        let (mut f, now) = drive_to_authrep_sent();
        let accept = peer_frame(
            1,
            2,
            Subclass::Iax(IaxCommand::Accept),
            FrameType::Iax,
            Ies::empty(),
        );
        let _ = f.handle(Event::Frame { frame: accept, now });
        (f, now)
    }

    #[test]
    fn active_voice_full_frame_emits_voice_received() {
        let (mut f, now) = drive_to_active();
        // RFC 5456 §6.4: voice full frames carry raw codec samples
        // immediately after the 12-byte header — no IEs. iax-bf0b: the
        // FSM previously dropped the bytes on the floor (payload = empty),
        // losing the first 20ms of every G.711U stream and *all* audio for
        // codecs (e.g. G.722) that only ever ride on full frames.
        let samples: Vec<u8> = (0..160u8).collect();
        let voice = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Voice,
            subclass: Subclass::Voice(VoiceFormat::G711U),
            ies: Ies::empty(),
            payload: &samples,
        }));
        let actions = f.handle(Event::Frame { frame: voice, now });
        let event = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived {
                    format,
                    payload,
                    ts,
                }) => Some((*format, payload.clone(), *ts)),
                _ => None,
            })
            .expect("VoiceReceived event must be emitted");
        assert_eq!(event.0, VoiceFormat::G711U);
        assert_eq!(event.1, samples, "audio payload must survive the FSM");
        assert_eq!(event.2, 0);
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-a422: RFC 5456 §6.4 — a mini frame carries no subclass, so its
    // implicit codec is that of the last full voice frame received on the leg.
    // A full frame in a NON-G.711µ format (here G.711a) followed by a mini frame
    // must report the tracked G.711a format, not the old hardcoded G.711µ.
    #[test]
    fn mini_frame_inherits_last_full_frame_voice_format() {
        let (mut f, now) = drive_to_active();
        // Full voice frame in G.711a establishes the leg's current codec.
        let full_samples: Vec<u8> = (0..160u8).collect();
        let full = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Voice,
            subclass: Subclass::Voice(VoiceFormat::G711A),
            ies: Ies::empty(),
            payload: &full_samples,
        }));
        let actions = f.handle(Event::Frame { frame: full, now });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::VoiceReceived {
                    format: VoiceFormat::G711A,
                    ..
                })
            )),
            "full frame must be reported in its own G711A format"
        );

        // A subsequent mini frame must inherit the tracked G.711a format.
        let mini_samples = vec![0x55u8; 160];
        let mini = Frame::Mini(crate::frame::MiniFrame {
            source_call: 7,
            timestamp: 20,
            payload: &mini_samples,
        });
        let actions = f.handle(Event::Frame {
            frame: mini,
            now: now + Duration::from_millis(20),
        });
        let reported = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived { format, .. }) => Some(*format),
                _ => None,
            })
            .expect("mini frame must emit VoiceReceived");
        assert_eq!(
            reported,
            VoiceFormat::G711A,
            "mini frame must inherit the last full frame's G711A format (RFC 5456 §6.4)"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-a422/iax-408b: before any full voice frame is received, a mini frame
    // falls back to the NEGOTIATED format (spec-degenerate — a conformant peer
    // sends a full frame first). In this harness the ACCEPT carries no FORMAT,
    // so the negotiated format is the UlawOnly policy default, G.711µ.
    #[test]
    fn mini_frame_before_any_full_frame_falls_back_to_negotiated_default() {
        let (mut f, now) = drive_to_active();
        let mini_samples = vec![0x11u8; 160];
        let mini = Frame::Mini(crate::frame::MiniFrame {
            source_call: 7,
            timestamp: 0,
            payload: &mini_samples,
        });
        let actions = f.handle(Event::Frame { frame: mini, now });
        let reported = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived { format, .. }) => Some(*format),
                _ => None,
            })
            .expect("mini frame must emit VoiceReceived");
        assert_eq!(
            reported,
            VoiceFormat::G711U,
            "fallback codec is the negotiated format (G711U in this harness)"
        );
    }

    /// Drive to Active with an ACCEPT carrying FORMAT=slin16, so the
    /// negotiated format differs from the G.711µ default and the iax-408b
    /// fallbacks below are observable.
    fn drive_to_active_slin16() -> (Fsm, Instant) {
        let (mut f, now) = drive_to_authrep_sent();
        let ies = Ies {
            format: Some(VoiceFormat::Slin16.as_u32()),
            ..Ies::empty()
        };
        let accept = peer_frame(1, 2, Subclass::Iax(IaxCommand::Accept), FrameType::Iax, ies);
        let _ = f.handle(Event::Frame { frame: accept, now });
        (f, now)
    }

    // iax-408b: the pre-full-frame mini fallback must be the NEGOTIATED format,
    // not hardcoded G.711µ. ASL3 node 2002 (app_rpt 2026) delivers its
    // announcement almost entirely as minis while its full voice frames carry
    // subclass 0 (see the next test) — under the old µ-law fallback every
    // 16-bit slin16 payload was run through the µ-law expander: loud digital
    // noise.
    #[test]
    fn mini_frame_before_any_full_frame_falls_back_to_negotiated_format() {
        let (mut f, now) = drive_to_active_slin16();
        let mini_samples = vec![0x11u8; 640];
        let mini = Frame::Mini(crate::frame::MiniFrame {
            source_call: 7,
            timestamp: 0,
            payload: &mini_samples,
        });
        let actions = f.handle(Event::Frame { frame: mini, now });
        let reported = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived { format, .. }) => Some(*format),
                _ => None,
            })
            .expect("mini frame must emit VoiceReceived");
        assert_eq!(
            reported,
            VoiceFormat::Slin16,
            "mini fallback must be the negotiated slin16, not G.711µ (iax-408b)"
        );
    }

    // iax-408b: node 2002's chan_iax2 sends full VOICE frames with subclass
    // byte 0 — no format signaled (observed live 2026-07-14, header
    // `96 18 00 01 | ts | 03 01 02 00`). parse_lenient surfaces that as
    // `Subclass::Raw(0)`. The frame must be treated as the negotiated format:
    // payload delivered, and `last_rx_voice_format` seeded so following minis
    // decode correctly instead of falling back.
    #[test]
    fn voice_full_frame_with_zero_subclass_uses_negotiated_format() {
        let (mut f, now) = drive_to_active_slin16();
        let samples = vec![0x22u8; 640];
        let voice = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Voice,
            subclass: Subclass::Raw(0),
            ies: Ies::empty(),
            payload: &samples,
        }));
        let actions = f.handle(Event::Frame { frame: voice, now });
        let (fmt, payload) = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived {
                    format, payload, ..
                }) => Some((*format, payload.clone())),
                _ => None,
            })
            .expect("zero-subclass voice frame must emit VoiceReceived (iax-408b)");
        assert_eq!(
            fmt,
            VoiceFormat::Slin16,
            "format 0 means the negotiated format"
        );
        assert_eq!(payload, samples, "payload must not be dropped");

        // The zero-subclass frame seeds the leg's format for subsequent minis.
        let mini_samples = vec![0x33u8; 640];
        let mini = Frame::Mini(crate::frame::MiniFrame {
            source_call: 7,
            timestamp: 20,
            payload: &mini_samples,
        });
        let actions = f.handle(Event::Frame {
            frame: mini,
            now: now + Duration::from_millis(20),
        });
        let reported = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::VoiceReceived { format, .. }) => Some(*format),
                _ => None,
            })
            .expect("mini after zero-subclass full frame must emit VoiceReceived");
        assert_eq!(reported, VoiceFormat::Slin16);
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn voice_full_frame_round_trips_with_payload_on_wire() {
        // RFC 5456 §6.4: a full voice frame's audio bytes follow the
        // 12-byte header directly; there are no IEs. parse(encode(f))
        // must preserve the payload byte-for-byte.
        let samples: Vec<u8> = (0..200u8).collect();
        let f = FullFrame {
            source_call: 1,
            dest_call: 2,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 0,
            frame_type: FrameType::Voice,
            subclass: Subclass::Voice(VoiceFormat::G711U),
            ies: Ies::empty(),
            payload: &samples,
        };
        let frame = Frame::Full(Box::new(f));
        let mut wire = Vec::new();
        crate::frame::encode(&frame, &mut wire).expect("voice frame has no IEs");
        let parsed = crate::frame::parse(&wire).expect("voice frame must parse");
        let Frame::Full(p) = parsed else {
            panic!("expected full frame");
        };
        assert_eq!(p.payload, &samples[..]);
        assert_eq!(p.ies, Ies::empty());
    }

    // iax-6c21: Event::DeliveryFailed (RxOutcome::GaveUp) in a pre-Active
    // handshake state FAILs the call — it never connected, so retransmit
    // exhaustion is a setup timeout. Previously this fell through the
    // LogInvalid catch-all and the call hung forever in NewSent.
    #[test]
    fn delivery_failed_in_new_sent_fails_the_call() {
        let (mut f, _now) = drive_to_new_sent();
        let actions = f.handle(Event::DeliveryFailed { oseqno: 0 });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::Timeout {
                        in_state: "NewSent"
                    }
                })
            )),
            "must emit Disconnected(Timeout {{ in_state: NewSent }})"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::NewRetry))),
            "must cancel the NewRetry timer"
        );
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "NewSent"
            })
        ));
    }

    // iax-6c21: in Active, DeliveryFailed tears the call down to a clean
    // terminal state (Closed) without a doomed reliable HANGUP, cancels the
    // keepalive timer, and notifies the app.
    #[test]
    fn delivery_failed_in_active_tears_down_to_closed() {
        let (mut f, _now) = drive_to_active();
        let actions = f.handle(Event::DeliveryFailed { oseqno: 3 });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::Timeout { in_state: "Active" }
                })
            )),
            "must emit Disconnected for the torn-down Active call"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::Keepalive))),
            "must cancel the keepalive timer"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "no doomed reliable HANGUP — the link already gave up"
        );
        assert!(matches!(f.state(), SessionState::Closed));
    }

    // iax-6c21: in Hangup, an abandoned HANGUP retransmit settles into Closed,
    // emitting Disconnected for a locally-initiated teardown.
    #[test]
    fn delivery_failed_in_hangup_settles_to_closed() {
        let (mut f, now) = drive_to_active();
        // Local hangup -> Hangup(initiated_by = Local).
        let _ = f.handle(Event::App(AppCommand::Hangup {
            cause: Some("user".into()),
            now: now + Duration::from_secs(1),
        }));
        assert!(matches!(f.state(), SessionState::Hangup(_)));
        let actions = f.handle(Event::DeliveryFailed { oseqno: 4 });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::HangupRetry))),
            "must cancel HangupRetry"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::Aborted
                })
            )),
            "local hangup must still emit Disconnected(Aborted)"
        );
        assert!(matches!(f.state(), SessionState::Closed));
    }

    // iax-6c21: a stray DeliveryFailed in a terminal state is an inert
    // LogInvalid — it must not resurrect or re-fail the call.
    #[test]
    fn delivery_failed_in_terminal_state_is_noop() {
        let mut f = Fsm::with_state(SessionState::Closed, creds(), CallNo::new(1).unwrap());
        let actions = f.handle(Event::DeliveryFailed { oseqno: 0 });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::LogInvalid { .. })),
            "terminal-state DeliveryFailed logs invalid"
        );
        assert!(matches!(f.state(), SessionState::Closed));
    }

    #[test]
    fn active_peer_hangup_transitions_to_hangup_peer() {
        let (mut f, now) = drive_to_active();
        let hangup = peer_frame(
            2,
            2,
            Subclass::Control(crate::subclass::ControlSubclass::Hangup),
            FrameType::Control,
            Ies::empty(),
        );
        let _ = f.handle(Event::Frame { frame: hangup, now });
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Peer,
                ..
            })
        ));
    }

    // iax-a116: the FIRST outbound voice frame in Active MUST be a FULL Voice
    // frame (SendReliable), NOT a mini (SendUnreliable). The peer cannot decode
    // mini frames without the codec + high-ts context established by a full
    // frame first. Updated from the old mini-only assumption.
    #[test]
    fn active_first_send_voice_emits_reliable_full_frame() {
        let (mut f, _now) = drive_to_active();
        let payload = vec![0xab; 160];
        let ts: u32 = 0x0000_1234;
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: payload.clone(),
            ts,
        }));
        // Must emit exactly one SendReliable, no SendUnreliable.
        let reliable_frames: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(reliable_frames.len(), 1, "exactly one SendReliable");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SendUnreliable(_))),
            "no SendUnreliable on first voice frame"
        );
        let fr = reliable_frames[0];
        assert_eq!(fr.frame_type, FrameType::Voice, "frame_type == Voice");
        assert_eq!(
            fr.subclass,
            Subclass::Voice(VoiceFormat::G711U),
            "subclass == Voice(G711U)"
        );
        assert_eq!(fr.timestamp, ts, "timestamp preserved (iax-a116)");
        assert_eq!(fr.payload, payload, "audio payload preserved byte-for-byte");
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-a116: second SendVoice in the SAME 16-bit timestamp window and same
    // codec MUST use a mini frame (SendUnreliable), not a full frame.
    #[test]
    fn active_second_send_voice_same_window_emits_mini() {
        let (mut f, _now) = drive_to_active();
        // First frame — full.
        let _ = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xaa; 160],
            ts: 0,
        }));
        // Second frame: same format, ts=20 — still in the same high-16 window (0..0xFFFF).
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xbb; 160],
            ts: 20,
        }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SendUnreliable(_))),
            "same-window second frame must be mini"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "no full frame when high-16 ts bits and codec are unchanged"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-a116: a SendVoice whose ts crosses the 16-bit boundary relative to
    // the last full frame MUST emit a new FULL frame (high-16 bits changed).
    #[test]
    fn active_send_voice_ts_crosses_16bit_boundary_emits_full_frame() {
        let (mut f, _now) = drive_to_active();
        // First frame: ts=0, high-16 = 0x0000.
        let _ = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xaa; 160],
            ts: 0,
        }));
        // Second frame: ts crosses into the next 0x1_0000 window.
        let ts2: u32 = 0x0001_0008;
        let payload2 = vec![0xcc; 160];
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: payload2.clone(),
            ts: ts2,
        }));
        let reliable_frames: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(
            reliable_frames.len(),
            1,
            "boundary crossing emits full frame"
        );
        let fr = reliable_frames[0];
        assert_eq!(fr.frame_type, FrameType::Voice);
        assert_eq!(fr.subclass, Subclass::Voice(VoiceFormat::G711U));
        assert_eq!(fr.timestamp, ts2, "new ts preserved in full frame");
        assert_eq!(fr.payload, payload2);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SendUnreliable(_))),
            "no mini when high-16 ts bits changed"
        );
    }

    // iax-a116: the rule is an equality test on the high 16 ts bits, NOT a
    // delta-of-one — a jump spanning MULTIPLE 16-bit windows (e.g. a long
    // silence/PTT gap > 65536 ms) must still force a full frame.
    #[test]
    fn active_send_voice_multi_window_jump_emits_full_frame() {
        let (mut f, _now) = drive_to_active();
        // First frame: ts=0, high-16 = 0x0000.
        let _ = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xaa; 160],
            ts: 0,
        }));
        // Jump three windows ahead (high-16 changes by >1).
        let ts2: u32 = 0x0003_0008;
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xee; 160],
            ts: ts2,
        }));
        let reliable_frames: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(
            reliable_frames.len(),
            1,
            "multi-window jump emits a full frame (equality test, not delta-of-one)"
        );
        assert_eq!(reliable_frames[0].timestamp, ts2, "new ts preserved");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SendUnreliable(_))),
            "no mini when high-16 ts bits changed by more than one window"
        );
    }

    // iax-a116: a SendVoice with a different codec than the last full frame
    // MUST emit a new FULL frame.
    #[test]
    fn active_send_voice_codec_change_emits_full_frame() {
        let (mut f, _now) = drive_to_active();
        // First frame: G711U.
        let _ = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0xaa; 160],
            ts: 0,
        }));
        // Second frame: G711A (codec changed, same ts window).
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711A,
            payload: vec![0xdd; 160],
            ts: 20,
        }));
        let reliable_frames: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(reliable_frames.len(), 1, "codec change emits full frame");
        let fr = reliable_frames[0];
        assert_eq!(fr.frame_type, FrameType::Voice);
        assert_eq!(fr.subclass, Subclass::Voice(VoiceFormat::G711A));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::SendUnreliable(_))),
            "no mini when codec changed"
        );
    }

    // iax-a116: last_full_voice is None on entry to Active — demonstrated by
    // the fact that the very first SendVoice always emits a full frame.
    #[test]
    fn active_last_full_voice_none_on_entry_first_frame_is_full() {
        let (mut f, _now) = drive_to_active();
        // The only way to know last_full_voice == None is that the first frame
        // triggers the "need_full" path; we assert that here.
        let actions = f.handle(Event::App(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: vec![0u8; 160],
            ts: 42,
        }));
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SendReliable(fr)
                    if fr.frame_type == FrameType::Voice
                    && fr.timestamp == 42
            )),
            "fresh Active: first voice frame must be full (last_full_voice was None)"
        );
    }

    #[test]
    fn active_app_hangup_transitions_to_hangup_local_and_sends_frame() {
        let (mut f, now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::Hangup {
            cause: Some("user".into()),
            now: now + Duration::from_secs(1),
        }));
        let mut saw_hangup_frame = false;
        for a in &actions {
            if let Action::SendReliable(f2) = a
                && matches!(f2.subclass, Subclass::Iax(IaxCommand::Hangup))
            {
                saw_hangup_frame = true;
            }
        }
        assert!(saw_hangup_frame);
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Local,
                ..
            })
        ));
    }

    #[test]
    fn hangup_ack_transitions_to_closed() {
        let (mut f, now) = drive_to_active();
        let _ = f.handle(Event::App(AppCommand::Hangup { cause: None, now }));
        let ack = peer_frame(
            3,
            3,
            Subclass::Iax(IaxCommand::Ack),
            FrameType::Iax,
            Ies::empty(),
        );
        let _ = f.handle(Event::Frame {
            frame: ack,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(f.state(), SessionState::Closed));
    }

    #[test]
    fn active_send_dtmf_emits_begin_then_end_with_digit_in_subclass() {
        let (mut f, now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::SendDtmf {
            digit: '*',
            now: now + Duration::from_millis(100),
        }));

        let mut iter = actions.iter().filter_map(|a| match a {
            Action::SendReliable(frame) => Some(frame),
            _ => None,
        });
        let begin = iter.next().expect("BEGIN frame");
        let end = iter.next().expect("END frame");
        assert!(iter.next().is_none(), "no extra reliable frames");

        assert_eq!(begin.frame_type, FrameType::DtmfBegin);
        assert_eq!(begin.subclass, Subclass::Dtmf('*'));
        assert_eq!(end.frame_type, FrameType::DtmfEnd);
        assert_eq!(end.subclass, Subclass::Dtmf('*'));

        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn active_receives_dtmf_begin_then_end_emits_one_event_at_end() {
        let (mut f, now) = drive_to_active();

        // Inbound BEGIN — no AppEvent yet.
        let begin = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 5,
            iseqno: 5,
            frame_type: FrameType::DtmfBegin,
            subclass: Subclass::Dtmf('9'),
            ies: Ies::empty(),
            payload: &[],
        }));
        let actions = f.handle(Event::Frame {
            frame: begin,
            now: now + Duration::from_millis(50),
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::DtmfReceived(_)))),
            "BEGIN alone does not emit DtmfReceived"
        );
        if let SessionState::Active(ActiveData { pending_dtmf, .. }) = f.state() {
            assert_eq!(*pending_dtmf, Some('9'), "BEGIN stashes digit");
        } else {
            panic!("state should still be Active");
        }

        // Inbound END — emits DtmfReceived and clears pending.
        let end = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 100,
            oseqno: 6,
            iseqno: 5,
            frame_type: FrameType::DtmfEnd,
            subclass: Subclass::Dtmf('9'),
            ies: Ies::empty(),
            payload: &[],
        }));
        let actions = f.handle(Event::Frame {
            frame: end,
            now: now + Duration::from_millis(150),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::DtmfReceived('9')))),
            "END emits DtmfReceived('9')"
        );
        if let SessionState::Active(ActiveData { pending_dtmf, .. }) = f.state() {
            assert_eq!(*pending_dtmf, None, "END clears pending");
        } else {
            panic!("state should still be Active");
        }
    }

    #[test]
    fn active_receives_legacy_single_frame_dtmf_emits_one_event() {
        let (mut f, now) = drive_to_active();
        // Legacy: DTMF_END with no preceding BEGIN.
        let end = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 5,
            iseqno: 5,
            frame_type: FrameType::DtmfEnd,
            subclass: Subclass::Dtmf('7'),
            ies: Ies::empty(),
            payload: &[],
        }));
        let actions = f.handle(Event::Frame { frame: end, now });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::DtmfReceived('7')))),
            "legacy single-frame DTMF emits one DtmfReceived"
        );
    }

    // iax-85e7: RFC 5456 §6.3 call-progress CONTROL frames received while
    // Active surface a `CallProgress` AppEvent and leave the call up.
    #[test]
    fn active_receives_call_progress_control_frames() {
        let cases = [
            (ControlSubclass::Proceeding, CallProgress::Proceeding),
            (ControlSubclass::Progress, CallProgress::Proceeding),
            (ControlSubclass::Ringing, CallProgress::Ringing),
            (ControlSubclass::Answer, CallProgress::Answered),
        ];
        for (control, expected) in cases {
            let (mut f, now) = drive_to_active();
            let frame = Frame::Full(Box::new(FullFrame {
                source_call: 7,
                dest_call: 1,
                retransmission: false,
                timestamp: 0,
                oseqno: 5,
                iseqno: 5,
                frame_type: FrameType::Control,
                subclass: Subclass::Control(control),
                ies: Ies::empty(),
                payload: &[],
            }));
            let actions = f.handle(Event::Frame { frame, now });
            assert!(
                actions.iter().any(|a| matches!(
                    a,
                    Action::AppEvent(AppEvent::CallProgress(p)) if *p == expected
                )),
                "{control:?} should emit CallProgress::{expected:?}"
            );
            assert!(
                matches!(f.state(), SessionState::Active(_)),
                "{control:?} keeps the call Active"
            );
        }
    }

    #[test]
    fn active_send_dtmf_rate_limited_within_50ms() {
        let (mut f, now) = drive_to_active();
        let _ = f.handle(Event::App(AppCommand::SendDtmf {
            digit: '1',
            now: now + Duration::from_millis(100),
        }));
        let actions = f.handle(Event::App(AppCommand::SendDtmf {
            digit: '2',
            now: now + Duration::from_millis(120),
        }));
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::LogInvalid {
                    reason: "dtmf_rate_limited"
                }
            )),
            "expected dtmf_rate_limited log"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "no wire traffic when rate limited"
        );
    }

    #[test]
    fn active_send_dtmf_admitted_after_50ms() {
        let (mut f, now) = drive_to_active();
        let _ = f.handle(Event::App(AppCommand::SendDtmf {
            digit: '1',
            now: now + Duration::from_millis(100),
        }));
        let actions = f.handle(Event::App(AppCommand::SendDtmf {
            digit: '2',
            now: now + Duration::from_millis(160),
        }));
        let reliables: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::SendReliable(_)))
            .collect();
        assert_eq!(reliables.len(), 2, "BEGIN + END for the second digit");
    }

    #[test]
    fn active_send_dtmf_invalid_digit_emits_log_invalid_and_no_frame() {
        let (mut f, now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::SendDtmf {
            digit: 'Z',
            now: now + Duration::from_millis(100),
        }));
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::LogInvalid {
                    reason: "invalid_dtmf_digit"
                }
            )),
            "expected invalid_dtmf_digit log"
        );
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "no wire traffic for invalid digit"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn build_dtmf_begin_round_trips_with_digit_in_subclass() {
        use crate::frame::{Frame, OwnedFrame, encode, parse};
        let our = CallNo::new(1).unwrap();
        let peer = CallNo::new(7).unwrap();
        let frame = build_dtmf_begin(our, peer, '5', 1000);
        assert_eq!(frame.frame_type, FrameType::DtmfBegin);
        assert_eq!(frame.subclass, Subclass::Dtmf('5'));

        // Wire round-trip.
        let owned = OwnedFrame::Full(frame.clone());
        let borrowed = owned.as_frame().expect("re-borrow");
        let mut bytes = Vec::new();
        encode(&borrowed, &mut bytes).expect("test frame must encode");
        let parsed = parse(&bytes).expect("parse");
        let Frame::Full(p) = parsed else {
            panic!("expected full frame")
        };
        assert_eq!(p.frame_type, FrameType::DtmfBegin);
        assert_eq!(p.subclass, Subclass::Dtmf('5'));
    }

    #[test]
    fn build_dtmf_end_round_trips_with_digit_in_subclass() {
        use crate::frame::{Frame, OwnedFrame, encode, parse};
        let our = CallNo::new(1).unwrap();
        let peer = CallNo::new(7).unwrap();
        let frame = build_dtmf_end(our, peer, '*', 1100);
        assert_eq!(frame.frame_type, FrameType::DtmfEnd);
        assert_eq!(frame.subclass, Subclass::Dtmf('*'));

        let owned = OwnedFrame::Full(frame.clone());
        let borrowed = owned.as_frame().expect("re-borrow");
        let mut bytes = Vec::new();
        encode(&borrowed, &mut bytes).expect("test frame must encode");
        let parsed = parse(&bytes).expect("parse");
        let Frame::Full(p) = parsed else {
            panic!("expected full frame")
        };
        assert_eq!(p.frame_type, FrameType::DtmfEnd);
        assert_eq!(p.subclass, Subclass::Dtmf('*'));
    }

    // iax-d4e9: keying PTT in Active emits a single reliable RADIO_KEY control
    // frame (FrameType::Control, subclass Control(RadioKey)=12), no IEs.
    #[test]
    fn active_send_ptt_key_emits_radio_key_frame() {
        let (mut f, _now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::SendPtt(true)));
        let reliables: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(reliables.len(), 1, "exactly one RADIO_KEY frame");
        assert_eq!(reliables[0].frame_type, FrameType::Control);
        assert_eq!(
            reliables[0].subclass,
            Subclass::Control(ControlSubclass::RadioKey)
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-d4e9: a key-down followed by a key-up emits RADIO_UNKEY (=13).
    #[test]
    fn active_send_ptt_unkey_emits_radio_unkey_frame() {
        let (mut f, _now) = drive_to_active();
        let _ = f.handle(Event::App(AppCommand::SendPtt(true)));
        let actions = f.handle(Event::App(AppCommand::SendPtt(false)));
        let reliables: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some(fr),
                _ => None,
            })
            .collect();
        assert_eq!(reliables.len(), 1, "exactly one RADIO_UNKEY frame");
        assert_eq!(reliables[0].frame_type, FrameType::Control);
        assert_eq!(
            reliables[0].subclass,
            Subclass::Control(ControlSubclass::RadioUnkey)
        );
    }

    // iax-d4e9: re-asserting the SAME keyed state is coalesced — no frame.
    #[test]
    fn active_send_ptt_coalesces_repeat() {
        let (mut f, _now) = drive_to_active();
        let first = f.handle(Event::App(AppCommand::SendPtt(true)));
        assert_eq!(
            first
                .iter()
                .filter(|a| matches!(a, Action::SendReliable(_)))
                .count(),
            1,
            "first key-down emits one frame"
        );
        let second = f.handle(Event::App(AppCommand::SendPtt(true)));
        assert!(
            !second.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "repeated key-down is coalesced (no second frame)"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-d4e9: a built RADIO_KEY frame survives a wire encode/parse round-trip.
    #[test]
    fn build_radio_key_round_trips_on_the_wire() {
        use crate::frame::{Frame, OwnedFrame, encode, parse};
        let our = CallNo::new(1).unwrap();
        let peer = CallNo::new(7).unwrap();
        let frame = build_radio_key(our, peer);
        assert_eq!(frame.frame_type, FrameType::Control);
        assert_eq!(frame.subclass, Subclass::Control(ControlSubclass::RadioKey));

        let owned = OwnedFrame::Full(frame.clone());
        let borrowed = owned.as_frame().expect("re-borrow");
        let mut bytes = Vec::new();
        encode(&borrowed, &mut bytes).expect("test frame must encode");
        let parsed = parse(&bytes).expect("parse");
        let Frame::Full(p) = parsed else {
            panic!("expected full frame")
        };
        assert_eq!(p.frame_type, FrameType::Control);
        assert_eq!(p.subclass, Subclass::Control(ControlSubclass::RadioKey));
    }

    #[test]
    fn hangup_retry_gives_up_after_three_attempts() {
        let (mut f, now) = drive_to_active();
        let _ = f.handle(Event::App(AppCommand::Hangup { cause: None, now }));
        let mut saw_disconnect = false;
        for i in 1..=3 {
            let actions = f.handle(Event::Timer {
                kind: TimerKind::HangupRetry,
                now: now + Duration::from_secs(i),
            });
            if actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::Disconnected { .. })))
            {
                saw_disconnect = true;
            }
        }
        assert!(matches!(f.state(), SessionState::Closed));
        assert!(saw_disconnect, "local hangup give-up emits Disconnected");
    }

    // iax-ff7b: a NEW frame MUST carry dest_call=0 — the callee has not
    // committed a real call leg yet. The CALLTOKEN's source_call is only a
    // temporary, throwaway scallno (Asterisk/ASL3 picks a DIFFERENT real
    // scallno for the subsequent AUTHREQ), so the resent NEW must NOT adopt
    // it. Wire-proven 2026-06-10: a resent NEW with dest_call != 0 is
    // REJECTED by ASL3, breaking every web-transceiver call. The peer's real
    // scallno is learned later, from the AUTHREQ in NewResent. This reverses
    // iax-e402, which misread RFC 5456 §8.6.1.
    #[test]
    fn calltoken_resent_new_uses_dest_call_zero_and_emits_no_set_peer_call() {
        let (mut f, now) = drive_to_new_sent();
        let token = b"opaque-token";
        let ies = Ies {
            calltoken: Some(token),
            ..Ies::empty()
        };
        // `peer_frame` uses server scallno = 7 (the CALLTOKEN's temp scallno).
        let frame = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::CallToken),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });

        // No SetPeerCall on the CALLTOKEN transition: adopting the temp scallno
        // would make Reliability::enqueue stamp a non-zero dest_call onto the
        // resent NEW, which the node rejects.
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SetPeerCall(_))),
            "no SetPeerCall must be emitted on CALLTOKEN (peer call is learned from AUTHREQ)"
        );

        // The resent NEW must carry dest_call=0.
        let new_frame = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(f2)
                    if matches!(f2.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(f2)
                }
                _ => None,
            })
            .expect("resent NEW must be emitted on CALLTOKEN");
        assert_eq!(
            new_frame.dest_call, 0,
            "resent NEW dest_call must be 0; ASL3 rejects a NEW with non-zero dest_call"
        );
        // The token must still be echoed back in the resent NEW.
        let ies = Ies::parse(&new_frame.ie_bytes).expect("resent NEW IEs parse");
        assert_eq!(
            ies.calltoken,
            Some(&token[..]),
            "token echoed in resent NEW"
        );
    }

    #[test]
    fn authreq_from_new_sent_emits_set_peer_call_before_authrep() {
        let (mut f, now) = drive_to_new_sent();
        let ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let frame = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame,
            now: now + Duration::from_millis(60),
        });
        let set_idx = actions
            .iter()
            .position(|a| matches!(a, Action::SetPeerCall(p) if p.value() == 7))
            .expect("SetPeerCall(7) on AUTHREQ");
        let authrep_idx = actions
            .iter()
            .position(|a| matches!(
                a,
                Action::SendReliable(f2) if matches!(f2.subclass, Subclass::Iax(IaxCommand::AuthRep))
            ))
            .expect("AUTHREP emitted");
        assert!(set_idx < authrep_idx);
    }

    #[test]
    fn authreq_from_new_resent_emits_set_peer_call_before_authrep() {
        let (mut f, now) = drive_to_new_resent();
        let ies = Ies {
            challenge: Some("x"),
            authmethods: Some(2),
            ..Ies::empty()
        };
        let auth = peer_frame(
            0,
            1,
            Subclass::Iax(IaxCommand::AuthReq),
            FrameType::Iax,
            ies,
        );
        let actions = f.handle(Event::Frame {
            frame: auth,
            now: now + Duration::from_millis(5),
        });
        let set_idx = actions
            .iter()
            .position(|a| matches!(a, Action::SetPeerCall(p) if p.value() == 7))
            .expect("SetPeerCall(7) on AUTHREQ from NewResent");
        let authrep_idx = actions
            .iter()
            .position(|a| matches!(
                a,
                Action::SendReliable(f2) if matches!(f2.subclass, Subclass::Iax(IaxCommand::AuthRep))
            ))
            .expect("AUTHREP emitted");
        assert!(set_idx < authrep_idx);
    }

    #[test]
    fn accept_emits_set_peer_call_redundantly() {
        // Defence-in-depth: even though peer_call was already plumbed at
        // CALLTOKEN/AUTHREQ time, the ACCEPT arm re-asserts it. Real Asterisk
        // peers are observed to keep the same scallno for the whole leg, so
        // this is idempotent but cheap insurance against the AUTHREQ-less
        // server path (where ACCEPT is the first reliable peer frame).
        let (mut f, now) = drive_to_authrep_sent();
        let accept = peer_frame(
            1,
            2,
            Subclass::Iax(IaxCommand::Accept),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame {
            frame: accept,
            now: now + Duration::from_millis(5),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SetPeerCall(p) if p.value() == 7)),
            "ACCEPT must (re-)assert peer_call so Reliability is in sync"
        );
    }

    #[test]
    fn start_call_standard_emits_capability_and_no_calling_id() {
        use crate::ie::Ies;
        use crate::subclass::IaxCommand;
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap());
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".into(),
            now: Instant::now(),
        }));
        let new = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(fr.clone())
                }
                _ => None,
            })
            .expect("StartCall emits a NEW");
        let ies = Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(ies.called_number, Some("1234"));
        assert!(
            ies.capability.is_some(),
            "standard mode advertises CAPABILITY"
        );
        assert_eq!(ies.calling_number, None);
        assert_eq!(ies.calling_name, None);
    }

    #[test]
    fn start_call_web_transceiver_omits_capability_and_sets_calling_id() {
        use crate::ie::Ies;
        use crate::session::CodecPolicy;
        use crate::session::call_profile::CallProfile;
        use crate::subclass::IaxCommand;
        let profile = CallProfile {
            calling_number: Some("55553".into()),
            calling_name: Some("astar".into()),
            send_capability: false,
            codec_policy: CodecPolicy::default(),
        };
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap()).with_call_profile(profile);
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "s".into(),
            now: Instant::now(),
        }));
        let new = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(fr.clone())
                }
                _ => None,
            })
            .expect("StartCall emits a NEW");
        let ies = Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(ies.called_number, Some("s"));
        assert_eq!(ies.calling_number, Some("55553"));
        assert_eq!(ies.calling_name, Some("astar"));
        assert_eq!(ies.capability, None, "WT mode omits CAPABILITY");
        assert_eq!(ies.format, Some(VoiceFormat::G711U.as_u32()));
    }

    /// Build a WT-shaped profile with `policy` and return the NEW frame that
    /// `StartCall` emits (owned, so tests can parse its IEs locally).
    fn wt_new_frame(policy: crate::session::CodecPolicy) -> crate::frame::OwnedFullFrame {
        use crate::session::call_profile::CallProfile;
        use crate::subclass::IaxCommand;
        let profile = CallProfile {
            calling_number: Some("55553".into()),
            calling_name: Some("astar".into()),
            send_capability: false,
            codec_policy: policy,
        };
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap()).with_call_profile(profile);
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "s".into(),
            now: Instant::now(),
        }));
        actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(fr.clone())
                }
                _ => None,
            })
            .expect("StartCall emits a NEW")
    }

    #[test]
    fn start_call_web_transceiver_prefer_slin_sends_capability_with_fallbacks() {
        // iax-866f: a no-caps NEW advertises only FORMAT, so any non-ulaw
        // preference is unnegotiable on nodes that don't allow it (live
        // REJECT "Unable to negotiate codec" from HamVoIP, 2026-07-11). Any
        // policy beyond UlawOnly must therefore ship the CAPABILITY mask so
        // the peer can fall back.
        use crate::session::CodecPolicy;
        let new = wt_new_frame(CodecPolicy::PreferSlin);
        let ies = crate::ie::Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(
            ies.capability,
            Some(CodecPolicy::PreferSlin.capability_mask().get()),
            "WT + non-ulaw policy must advertise CAPABILITY"
        );
        assert_eq!(ies.format, Some(VoiceFormat::Slin.as_u32()));
    }

    #[test]
    fn start_call_web_transceiver_prefer_slin16_capability_includes_ulaw_fallback() {
        // iax-866f regression: the astar wideband default (PreferSlin16) must
        // keep µ-law reachable in the WT shape or narrowband-only nodes reject
        // the call outright.
        use crate::session::CodecPolicy;
        let new = wt_new_frame(CodecPolicy::PreferSlin16);
        let ies = crate::ie::Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        let caps = ies.capability.expect("WT + PreferSlin16 sends CAPABILITY");
        assert_ne!(
            caps & VoiceFormat::G711U.as_u32(),
            0,
            "capability must include the µ-law fallback"
        );
        assert_eq!(ies.format, Some(VoiceFormat::Slin16.as_u32()));
    }

    #[test]
    fn start_call_web_transceiver_ulaw_only_still_omits_capability() {
        // iax-3fca compatibility pin: the proven legacy WT wire shape (FORMAT
        // = µ-law, no CAPABILITY) is preserved byte-for-byte for the default
        // policy.
        use crate::session::CodecPolicy;
        let new = wt_new_frame(CodecPolicy::UlawOnly);
        let ies = crate::ie::Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(ies.capability, None, "UlawOnly keeps the no-caps WT shape");
        assert_eq!(ies.format, Some(VoiceFormat::G711U.as_u32()));
    }

    #[test]
    fn prefer_slin_advertises_and_names_slin() {
        use crate::ie::Ies;
        use crate::session::CodecPolicy;
        use crate::session::call_profile::CallProfile;
        use crate::subclass::IaxCommand;
        let profile = CallProfile {
            codec_policy: CodecPolicy::PreferSlin,
            ..CallProfile::default()
        };
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap()).with_call_profile(profile);
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".into(),
            now: Instant::now(),
        }));
        let new = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(fr.clone())
                }
                _ => None,
            })
            .expect("StartCall emits a NEW");
        let ies = Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(
            ies.format,
            Some(VoiceFormat::Slin.as_u32()),
            "PreferSlin names Slin in FORMAT"
        );
        let capability = ies.capability.expect("PreferSlin advertises CAPABILITY");
        assert_ne!(
            capability & VoiceFormat::Slin.as_u32(),
            0,
            "CAPABILITY includes Slin"
        );
        assert_ne!(
            capability & VoiceFormat::G711U.as_u32(),
            0,
            "CAPABILITY includes G711U"
        );
        assert_ne!(
            capability & VoiceFormat::G711A.as_u32(),
            0,
            "CAPABILITY includes G711A"
        );
    }

    #[test]
    fn default_policy_new_frame_is_unchanged() {
        // Pre-slin regression guard: the default profile's NEW frame must be
        // byte-identical to today (iax-31f7).
        use crate::ie::Ies;
        use crate::subclass::IaxCommand;
        let mut f = Fsm::new(creds(), CallNo::new(1).unwrap());
        let actions = f.handle(Event::App(AppCommand::StartCall {
            dest: "1234".into(),
            now: Instant::now(),
        }));
        let new = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::New)) =>
                {
                    Some(fr.clone())
                }
                _ => None,
            })
            .expect("StartCall emits a NEW");
        let ies = Ies::parse(new.ie_bytes()).expect("parse NEW ies");
        assert_eq!(ies.format, Some(VoiceFormat::G711U.as_u32()));
        let capability = ies
            .capability
            .expect("default policy advertises CAPABILITY");
        assert_eq!(
            capability,
            VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32(),
            "default policy advertises exactly G711U|G711A"
        );
    }

    #[test]
    fn timer_kind_has_registration_variants() {
        let _: TimerKind = TimerKind::RegReqRetry;
        let _: TimerKind = TimerKind::RegAuthRetry;
        let _: TimerKind = TimerKind::RegTokenExpiry;
        let _: TimerKind = TimerKind::RegRefresh;
        let _: TimerKind = TimerKind::RegRelRetry;
    }

    #[test]
    fn accept_arms_keepalive_timer_at_ping_interval() {
        let (mut f, now) = drive_to_authrep_sent();
        let accept = peer_frame(
            1,
            2,
            Subclass::Iax(IaxCommand::Accept),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame { frame: accept, now });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SetTimer(TimerKind::Keepalive, d) if *d == Duration::from_secs(2)
            )),
            "entering Active must arm TimerKind::Keepalive at ping_interval"
        );
    }

    #[test]
    fn rtt_is_none_outside_active_and_before_any_echo() {
        let f = fsm();
        assert_eq!(f.rtt(), None, "Init has no RTT");
        let (f2, _) = drive_to_active();
        assert_eq!(f2.rtt(), None, "Active but no PONG/LAGRP yet");
    }

    #[test]
    fn keepalive_timer_sends_ping_and_lagrq_and_rearms() {
        let (mut f, now) = drive_to_active();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::Keepalive,
            now: now + Duration::from_secs(2),
        });
        let sent: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::SendReliable(fr) => Some((fr.subclass, fr.timestamp)),
                _ => None,
            })
            .collect();
        assert!(
            sent.contains(&(Subclass::Iax(IaxCommand::Ping), 2_000)),
            "PING stamped with ms-since-establishment; got {sent:?}"
        );
        assert!(
            sent.contains(&(Subclass::Iax(IaxCommand::LagRq), 2_000)),
            "LAGRQ rides the same cadence; got {sent:?}"
        );
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::SetTimer(TimerKind::Keepalive, d) if *d == Duration::from_secs(2)
            )),
            "timer must re-arm"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn inbound_ping_replies_pong_echoing_timestamp() {
        let (mut f, now) = drive_to_active();
        let inbound_ping = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 4242,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Ping),
            ies: Ies::empty(),
            payload: &[],
        }));
        let actions = f.handle(Event::Frame {
            frame: inbound_ping,
            now: now + Duration::from_millis(500),
        });
        let reply_pong = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::Pong)) =>
                {
                    Some(fr)
                }
                _ => None,
            })
            .expect("PING must be answered with PONG");
        assert_eq!(
            reply_pong.timestamp, 4242,
            "PONG echoes the PING timestamp (RFC 5456 §6.7.3)"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn inbound_lagrq_replies_lagrp_echoing_timestamp() {
        let (mut f, now) = drive_to_active();
        let inbound_lagrq = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 777,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::LagRq),
            ies: Ies::empty(),
            payload: &[],
        }));
        let actions = f.handle(Event::Frame {
            frame: inbound_lagrq,
            now: now + Duration::from_millis(500),
        });
        let reply_lagrp = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::LagRp)) =>
                {
                    Some(fr)
                }
                _ => None,
            })
            .expect("LAGRQ must be answered with LAGRP");
        assert_eq!(
            reply_lagrp.timestamp, 777,
            "LAGRP echoes the LAGRQ timestamp (RFC 5456 §6.7.5)"
        );
    }

    #[test]
    fn pong_after_keepalive_timer_populates_rtt() {
        let (mut f, now) = drive_to_active();
        assert_eq!(f.rtt(), None);
        let _ = f.handle(Event::Timer {
            kind: TimerKind::Keepalive,
            now: now + Duration::from_secs(2),
        });
        // The PING went out stamped ts=2000; peer echoes it 80ms later.
        let pong = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 2_000,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(IaxCommand::Pong),
            ies: Ies::empty(),
            payload: &[],
        }));
        let _ = f.handle(Event::Frame {
            frame: pong,
            now: now + Duration::from_secs(2) + Duration::from_millis(80),
        });
        assert_eq!(f.rtt(), Some(Duration::from_millis(80)));
    }

    #[test]
    fn silence_trips_connection_lost_once_and_inbound_restores_once() {
        let (mut f, now) = drive_to_active();
        let lost_count = |actions: &smallvec::SmallVec<[Action; 4]>| {
            actions
                .iter()
                .filter(|a| matches!(a, Action::AppEvent(AppEvent::ConnectionLost)))
                .count()
        };
        // +2s: 2s silent, below the 4s deadline.
        let a = f.handle(Event::Timer {
            kind: TimerKind::Keepalive,
            now: now + Duration::from_secs(2),
        });
        assert_eq!(lost_count(&a), 0);
        // +4s: exactly at the deadline — the one and only ConnectionLost.
        let a = f.handle(Event::Timer {
            kind: TimerKind::Keepalive,
            now: now + Duration::from_secs(4),
        });
        assert_eq!(lost_count(&a), 1);
        // +6s: still silent — edge-triggered, no repeat.
        let a = f.handle(Event::Timer {
            kind: TimerKind::Keepalive,
            now: now + Duration::from_secs(6),
        });
        assert_eq!(lost_count(&a), 0);
        assert!(
            matches!(f.state(), SessionState::Active(_)),
            "loss must never tear the call down"
        );
        // First inbound frame afterwards: exactly one ConnectionRestored.
        let mk_pong = |ts: u32, oseq: u8| {
            Frame::Full(Box::new(FullFrame {
                source_call: 7,
                dest_call: 1,
                retransmission: false,
                timestamp: ts,
                oseqno: oseq,
                iseqno: 2,
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Pong),
                ies: Ies::empty(),
                payload: &[],
            }))
        };
        let a = f.handle(Event::Frame {
            frame: mk_pong(6_000, 2),
            now: now + Duration::from_secs(7),
        });
        assert_eq!(
            a.iter()
                .filter(|x| matches!(x, Action::AppEvent(AppEvent::ConnectionRestored)))
                .count(),
            1
        );
        let a = f.handle(Event::Frame {
            frame: mk_pong(6_000, 3),
            now: now + Duration::from_secs(7) + Duration::from_millis(20),
        });
        assert!(
            !a.iter()
                .any(|x| matches!(x, Action::AppEvent(AppEvent::ConnectionRestored))),
            "restored is edge-triggered"
        );
    }

    #[test]
    fn active_inval_hard_fails_with_peer_inval() {
        let (mut f, now) = drive_to_active();
        let inval = peer_frame(
            0,
            0,
            Subclass::Iax(IaxCommand::Inval),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame {
            frame: inval,
            now: now + Duration::from_secs(1),
        });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::PeerInval
                })
            )),
            "INVAL must surface Disconnected{{PeerInval}}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::Keepalive))),
            "keepalive must stop with the call"
        );
        assert!(
            matches!(f.state(), SessionState::Failed(FailReason::PeerInval)),
            "INVAL is terminal (locked Q4: no auto re-establish)"
        );
    }

    // iax-a195: the CAUSE IE from a peer HANGUP must surface in the
    // Disconnected event — not be silently replaced by the generic Aborted.
    #[test]
    fn peer_hangup_surfaces_cause_not_aborted() {
        let (mut f, now) = drive_to_active();
        // Build an inbound IAX HANGUP full frame carrying a CAUSE IE.
        // Call numbers / seq must match what drive_to_active establishes:
        // source_call=7 (peer), dest_call=1 (our), oseqno/iseqno=2.
        let ies = Ies {
            cause: Some("Normal Clearing"),
            causecode: Some(16),
            ..Ies::empty()
        };
        let hangup = peer_frame(2, 2, Subclass::Iax(IaxCommand::Hangup), FrameType::Iax, ies);
        let actions = f.handle(Event::Frame { frame: hangup, now });
        let reason = actions
            .iter()
            .find_map(|a| match a {
                Action::AppEvent(AppEvent::Disconnected { reason }) => Some(reason.clone()),
                _ => None,
            })
            .expect("Disconnected must be emitted on peer hangup");
        match reason {
            FailReason::RemoteHangup { cause } => {
                assert_eq!(cause.as_deref(), Some("Normal Clearing"));
            }
            other => panic!("expected RemoteHangup, got {other:?}"),
        }
    }

    #[test]
    fn leaving_active_cancels_the_keepalive_timer() {
        // Local hangup.
        let (mut f, now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::Hangup {
            cause: None,
            now: now + Duration::from_secs(1),
        }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::Keepalive))),
            "local hangup must cancel the keepalive timer"
        );
        // Peer hangup.
        let (mut f, now) = drive_to_active();
        let hangup = peer_frame(
            2,
            2,
            Subclass::Control(crate::subclass::ControlSubclass::Hangup),
            FrameType::Control,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame {
            frame: hangup,
            now: now + Duration::from_secs(1),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::Keepalive))),
            "peer hangup must cancel the keepalive timer"
        );
    }

    // iax-a195: SendText must emit a reliable TEXT full frame carrying the
    // body in the payload field (subclass = Raw(0), frame_type = Text).
    #[test]
    fn send_text_emits_a_text_full_frame() {
        let (mut f, _now) = drive_to_active();
        let actions = f.handle(Event::App(AppCommand::SendText("!NEWKEY1!".into())));
        let sent = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr) => Some(fr.clone()),
                _ => None,
            })
            .expect("a reliable frame is sent");
        assert_eq!(sent.frame_type, FrameType::Text);
        assert_eq!(sent.payload, b"!NEWKEY1!");
    }

    // iax-a195: inbound !NEWKEY1! TEXT frame must be echoed back.
    #[test]
    fn inbound_newkey1_text_is_echoed() {
        let (mut f, now) = drive_to_active();
        let body: &'static [u8] = b"!NEWKEY1!";
        let frame = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Text,
            subclass: Subclass::Raw(0),
            ies: Ies::empty(),
            payload: body,
        }));
        let actions = f.handle(Event::Frame { frame, now });
        assert!(
            actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
                if fr.frame_type == FrameType::Text && fr.payload == b"!NEWKEY1!")),
            "node should echo !NEWKEY1!"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    // iax-a195: inbound !!DISCONNECT!! TEXT frame must trigger peer hangup teardown.
    #[test]
    fn inbound_disconnect_text_triggers_hangup() {
        let (mut f, now) = drive_to_active();
        let body: &'static [u8] = b"!!DISCONNECT!!";
        let frame = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Text,
            subclass: Subclass::Raw(0),
            ies: Ies::empty(),
            payload: body,
        }));
        let actions = f.handle(Event::Frame { frame, now });
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::RemoteHangup { .. }
                })
            )),
            "!!DISCONNECT!! must emit RemoteHangup Disconnected"
        );
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Peer,
                ..
            })
        ));
    }

    // iax-a195: an arbitrary inbound TEXT frame (e.g. "L <list>") must surface
    // as a TextReceived app event.
    #[test]
    fn inbound_text_other_emits_text_received() {
        let (mut f, now) = drive_to_active();
        let body: &'static [u8] = b"L 27225";
        let frame = Frame::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno: 2,
            iseqno: 2,
            frame_type: FrameType::Text,
            subclass: Subclass::Raw(0),
            ies: Ies::empty(),
            payload: body,
        }));
        let actions = f.handle(Event::Frame { frame, now });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::TextReceived(_)))),
            "generic text must surface as TextReceived"
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }
}
