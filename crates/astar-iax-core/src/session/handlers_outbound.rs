// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Per-state event handlers for the outbound call FSM (iax-19ad).
//! Each `on_<state>` is the verbatim logic of the former `handle()` match arm,
//! returning the next state instead of assigning `self.state`. No semantic
//! change.
//!
//! The handlers share one uniform signature (`&mut self`, owned state data,
//! owned `Event`) so the dispatcher can call them through `self.on_<state>(…)`.
//! A few of them (the passive `Closed`/`Failed`/`*Received` arms) consequently
//! don't touch `self` or consume `event`, and a few (notably `on_active`) are
//! long verbatim ports — hence the module-level pedantic allows below. These
//! are inherent to the dispatcher shape, not new behaviour. `too_many_lines` is
//! kept per-method (not module-wide) so it stays a tripwire for future handlers.
#![allow(clippy::unused_self, clippy::needless_pass_by_value)]

use std::time::Duration;

use smallvec::SmallVec;

use super::auth::AuthMethods;
use super::builders::{
    build_authrep, build_dtmf_begin, build_dtmf_end, build_full_voice, build_hangup, build_lagrp,
    build_lagrq, build_mini_voice, build_new, build_ping, build_pong, build_radio_key,
    build_radio_unkey, build_text,
};
use super::call_no::CallNo;
use super::fsm::{DTMF_RATE_LIMIT, full_subclass, invalid_reason, is_valid_dtmf_digit};
use super::keepalive::{KeepaliveConfig, KeepaliveState};
use super::{
    Action, ActiveData, AppCommand, AppEvent, AuthRepSentData, AuthReqReceivedData, CallProgress,
    CallToken, CallTokenReceivedData, CodecMask, CodecPolicy, Event, FailReason, Fsm, HangupData,
    HangupOrigin, NewResentData, NewSentData, SessionState, TimerKind,
};
use crate::frame::{Frame, Subclass};
use crate::ie::Ies;
use crate::subclass::{FrameType, IaxCommand, VoiceFormat};

impl Fsm {
    pub(super) fn on_init(&mut self, event: Event<'_>) -> (SessionState, SmallVec<[Action; 4]>) {
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::App(AppCommand::StartCall { dest, now }) => {
                let capabilities: CodecMask = self.call_profile.codec_policy.capability_mask();
                let ping_seq = 0u8;
                // First NEW: peer scallno is unknown, dest_call = 0 (RFC 5456
                // §8.6.1). iax-e402: subsequent NEWs (after CALLTOKEN) carry
                // the peer's chosen scallno.
                let new_frame = build_new(
                    self.our_call,
                    0,
                    &dest,
                    &self.credentials.username,
                    capabilities,
                    None,
                    &self.call_profile,
                );
                out.push(Action::SendReliable(new_frame));
                out.push(Action::SetTimer(
                    TimerKind::NewRetry,
                    Duration::from_secs(1),
                ));
                (
                    SessionState::NewSent(NewSentData {
                        sent_at: now,
                        our_call: self.our_call,
                        attempts: 1,
                        capabilities,
                        ping_seq,
                        dest,
                    }),
                    out,
                )
            }
            event => {
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&SessionState::Init, &event),
                });
                (SessionState::Init, out)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // verbatim port of former handle() arm — iax-19ad
    pub(super) fn on_new_sent(
        &mut self,
        s: NewSentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let NewSentData {
            our_call,
            attempts,
            capabilities,
            ping_seq,
            sent_at,
            dest,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, now } => match full_subclass(&frame) {
                Some((Subclass::Iax(IaxCommand::CallToken), _peer_call, ies_bytes)) => {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    // The CALLTOKEN bytes came off the wire through `Ies::parse`,
                    // so they already fit a u8 length prefix; `new` cannot fail.
                    let token = CallToken::new(ies.calltoken.unwrap_or(&[]))
                        .expect("wire CALLTOKEN IE <= 255 bytes");
                    // iax-ff7b: the resent NEW MUST carry dest_call=0. A NEW is a
                    // call-setup frame; the callee has not committed a real call
                    // leg yet. The CALLTOKEN's `source_call` is only a temporary,
                    // throwaway scallno (anti-spoof) — Asterisk/ASL3 picks a
                    // DIFFERENT real scallno for the AUTHREQ that follows. Adopting
                    // the temp scallno here (and plumbing it to Reliability via
                    // SetPeerCall) made enqueue stamp a non-zero dest_call, which
                    // ASL3 REJECTS — breaking every web-transceiver call. So: no
                    // SetPeerCall, and build with dest_call=0. The peer's real
                    // scallno is learned later, from the AUTHREQ in `on_new_resent`.
                    // (Reverses iax-e402's misreading of RFC 5456 §8.6.1.)
                    let new_frame = build_new(
                        our_call,
                        0,
                        &dest,
                        &self.credentials.username,
                        capabilities,
                        Some(token.as_bytes()),
                        &self.call_profile,
                    );
                    out.push(Action::SendReliable(new_frame));
                    out.push(Action::SetTimer(
                        TimerKind::TokenExpiry,
                        Duration::from_secs(10),
                    ));
                    out.push(Action::SetTimer(
                        TimerKind::NewRetry,
                        Duration::from_secs(1),
                    ));
                    (
                        SessionState::NewResent(NewResentData {
                            sent_at: now,
                            our_call,
                            token,
                            attempts: 1,
                            capabilities,
                            ping_seq,
                            dest,
                        }),
                        out,
                    )
                }
                Some((Subclass::Iax(IaxCommand::AuthReq), peer_call, ies_bytes)) => {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let challenge = ies.challenge.unwrap_or("").to_string();
                    let methods = AuthMethods::from_bits_truncate(ies.authmethods.unwrap_or(0));
                    let peer = CallNo::new(peer_call).unwrap_or(our_call);
                    out.push(Action::CancelTimer(TimerKind::NewRetry));
                    // iax-e402: AUTHREQ-without-CALLTOKEN path. Plumb the
                    // peer's scallno down to Reliability before AUTHREP so
                    // retransmits address the right peer call.
                    out.push(Action::SetPeerCall(peer));
                    let response =
                        super::auth::md5_response(&challenge, self.credentials.password.expose());
                    let authrep = build_authrep(our_call, peer, &response);
                    out.push(Action::SendReliable(authrep));
                    out.push(Action::SetTimer(
                        TimerKind::AuthRepRetry,
                        Duration::from_secs(1),
                    ));
                    let next = SessionState::AuthRepSent(AuthRepSentData {
                        sent_at: now,
                        our_call,
                        peer_call: peer,
                        attempts: 1,
                        challenge: challenge.into_bytes(),
                    });
                    let _ = methods;
                    (next, out)
                }
                Some((Subclass::Iax(IaxCommand::Accept), peer_call, ies_bytes)) => {
                    // No-auth, no-CALLTOKEN accept: the peer accepted the initial
                    // NEW directly, so ACCEPT is the first reliable peer frame we
                    // see (auth=Off / calltoken=Never servers — e.g. a plain
                    // node-as-handset listener). Previously this fell through to
                    // the invalid-frame arm and the call timed out in NewSent
                    // (iax-64b6). Mirrors the AuthRepSent ACCEPT transition.
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let negotiated_format =
                        accepted_format(&ies, self.call_profile.codec_policy, &mut out);
                    let peer = CallNo::new(peer_call).unwrap_or(our_call);
                    out.push(Action::CancelTimer(TimerKind::NewRetry));
                    out.push(Action::SetPeerCall(peer));
                    out.push(Action::AppEvent(AppEvent::Connected { peer_call: peer }));
                    let keepalive = KeepaliveState::new(KeepaliveConfig::default(), now);
                    out.push(Action::SetTimer(
                        TimerKind::Keepalive,
                        keepalive.config().ping_interval,
                    ));
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call: peer,
                            established_at: now,
                            pending_dtmf: None,
                            last_dtmf_at: None,
                            keepalive,
                            last_full_voice: None,
                            last_ptt: None,
                            last_rx_voice_format: None, // iax-a422: no voice rx yet
                            negotiated_format,
                        }),
                        out,
                    )
                }
                Some((Subclass::Iax(IaxCommand::Reject), _, ies_bytes)) => {
                    // A peer rejecting the initial NEW (no auth): fail the call
                    // fast instead of retrying NEW until the NewSent timeout.
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let cause = ies.cause.map(str::to_string);
                    let reason = FailReason::Rejected { cause };
                    out.push(Action::CancelTimer(TimerKind::NewRetry));
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    (SessionState::Failed(reason), out)
                }
                _ => {
                    out.push(Action::LogInvalid {
                        reason: "unexpected_frame_in_new_sent",
                    });
                    (
                        SessionState::NewSent(NewSentData {
                            sent_at: now,
                            our_call,
                            attempts,
                            capabilities,
                            ping_seq,
                            dest,
                        }),
                        out,
                    )
                }
            },
            Event::Timer {
                kind: TimerKind::NewRetry,
                now,
            } => {
                if attempts >= 5 {
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: FailReason::Timeout {
                            in_state: "NewSent",
                        },
                    }));
                    (
                        SessionState::Failed(FailReason::Timeout {
                            in_state: "NewSent",
                        }),
                        out,
                    )
                } else {
                    // Still in NewSent: peer scallno not yet learned, so
                    // dest_call stays 0. The Reliability layer also has
                    // peer_call == None and so won't overwrite it.
                    let new_frame = build_new(
                        our_call,
                        0,
                        &dest,
                        &self.credentials.username,
                        capabilities,
                        None,
                        &self.call_profile,
                    );
                    out.push(Action::SendReliable(new_frame));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::NewRetry, backoff));
                    let next = SessionState::NewSent(NewSentData {
                        sent_at,
                        our_call,
                        attempts: attempts + 1,
                        capabilities,
                        ping_seq,
                        dest,
                    });
                    let _ = now;
                    (next, out)
                }
            }
            event => {
                out.push(Action::LogInvalid {
                    reason: invalid_reason(
                        &SessionState::NewSent(NewSentData {
                            sent_at,
                            our_call,
                            attempts,
                            capabilities,
                            ping_seq,
                            dest: dest.clone(),
                        }),
                        &event,
                    ),
                });
                (
                    SessionState::NewSent(NewSentData {
                        sent_at,
                        our_call,
                        attempts,
                        capabilities,
                        ping_seq,
                        dest,
                    }),
                    out,
                )
            }
        }
    }

    pub(super) fn on_calltoken_received(
        &mut self,
        s: CallTokenReceivedData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        // No explicit arm existed for CallTokenReceived in the original
        // handle(); every event fell through to the invalid-transition
        // catch-all leaving state unchanged.
        let state = SessionState::CallTokenReceived(s);
        out.push(Action::LogInvalid {
            reason: invalid_reason(&state, &event),
        });
        (state, out)
    }

    #[allow(clippy::too_many_lines)] // verbatim port of former handle() arm — iax-19ad
    pub(super) fn on_new_resent(
        &mut self,
        s: NewResentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let NewResentData {
            our_call,
            token,
            attempts,
            capabilities,
            ping_seq,
            sent_at,
            dest,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, now } => match full_subclass(&frame) {
                Some((Subclass::Iax(IaxCommand::AuthReq), peer_call, ies_bytes)) => {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let challenge = ies.challenge.unwrap_or("").to_string();
                    let peer = CallNo::new(peer_call).unwrap_or(our_call);
                    out.push(Action::CancelTimer(TimerKind::NewRetry));
                    out.push(Action::CancelTimer(TimerKind::TokenExpiry));
                    // iax-e402: re-assert peer_call on AUTHREQ. Even on the
                    // CALLTOKEN path the server *might* pick a different
                    // scallno for AUTHREQ than it used for CALLTOKEN (Asterisk
                    // does not in practice, but the RFC permits it), so we
                    // refresh Reliability's view from the AUTHREQ source_call.
                    out.push(Action::SetPeerCall(peer));
                    let response =
                        super::auth::md5_response(&challenge, self.credentials.password.expose());
                    let authrep = build_authrep(our_call, peer, &response);
                    out.push(Action::SendReliable(authrep));
                    out.push(Action::SetTimer(
                        TimerKind::AuthRepRetry,
                        Duration::from_secs(1),
                    ));
                    (
                        SessionState::AuthRepSent(AuthRepSentData {
                            sent_at: now,
                            our_call,
                            peer_call: peer,
                            attempts: 1,
                            challenge: challenge.into_bytes(),
                        }),
                        out,
                    )
                }
                Some((Subclass::Iax(IaxCommand::Accept), peer_call, ies_bytes)) => {
                    // auth=Off + requirecalltoken=Yes: the peer demands a
                    // CALLTOKEN but does NOT challenge, so it ACCEPTs the
                    // token'd NEW directly — ACCEPT is the first reliable peer
                    // frame after CALLTOKEN, with no intervening AUTHREQ. This
                    // is the default `astar-server` listener path. Previously
                    // this fell through to the invalid-frame arm and the call
                    // re-sent NEW until the NewResent timeout. Mirrors the
                    // NewSent / AuthRepSent ACCEPT transitions.
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let negotiated_format =
                        accepted_format(&ies, self.call_profile.codec_policy, &mut out);
                    let peer = CallNo::new(peer_call).unwrap_or(our_call);
                    out.push(Action::CancelTimer(TimerKind::NewRetry));
                    out.push(Action::CancelTimer(TimerKind::TokenExpiry));
                    out.push(Action::SetPeerCall(peer));
                    out.push(Action::AppEvent(AppEvent::Connected { peer_call: peer }));
                    let keepalive = KeepaliveState::new(KeepaliveConfig::default(), now);
                    out.push(Action::SetTimer(
                        TimerKind::Keepalive,
                        keepalive.config().ping_interval,
                    ));
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call: peer,
                            established_at: now,
                            pending_dtmf: None,
                            last_dtmf_at: None,
                            keepalive,
                            last_full_voice: None,
                            last_ptt: None,
                            last_rx_voice_format: None,
                            negotiated_format,
                        }),
                        out,
                    )
                }
                _ => {
                    out.push(Action::LogInvalid {
                        reason: "unexpected_frame_in_new_resent",
                    });
                    (
                        SessionState::NewResent(NewResentData {
                            sent_at,
                            our_call,
                            token,
                            attempts,
                            capabilities,
                            ping_seq,
                            dest,
                        }),
                        out,
                    )
                }
            },
            Event::Timer {
                kind: TimerKind::TokenExpiry,
                ..
            } => {
                out.push(Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::Timeout {
                        in_state: "NewResent",
                    },
                }));
                let next = SessionState::Failed(FailReason::Timeout {
                    in_state: "NewResent",
                });
                let _ = (
                    our_call,
                    token,
                    attempts,
                    capabilities,
                    ping_seq,
                    sent_at,
                    dest,
                );
                (next, out)
            }
            Event::Timer {
                kind: TimerKind::NewRetry,
                ..
            } => {
                if attempts >= 5 {
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: FailReason::Timeout {
                            in_state: "NewResent",
                        },
                    }));
                    (
                        SessionState::Failed(FailReason::Timeout {
                            in_state: "NewResent",
                        }),
                        out,
                    )
                } else {
                    // iax-ff7b: a NEW always carries dest_call=0. The peer's real
                    // scallno is not yet known in NewResent (it arrives with the
                    // AUTHREQ), so Reliability's peer_call is still unset and
                    // `enqueue` leaves this 0 — exactly what ASL3 requires for a
                    // NEW. Resent with the same token until AUTHREQ or give-up.
                    let new_frame = build_new(
                        our_call,
                        0,
                        &dest,
                        &self.credentials.username,
                        capabilities,
                        Some(token.as_bytes()),
                        &self.call_profile,
                    );
                    out.push(Action::SendReliable(new_frame));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::NewRetry, backoff));
                    (
                        SessionState::NewResent(NewResentData {
                            sent_at,
                            our_call,
                            token,
                            attempts: attempts + 1,
                            capabilities,
                            ping_seq,
                            dest,
                        }),
                        out,
                    )
                }
            }
            event => {
                let state = SessionState::NewResent(NewResentData {
                    sent_at,
                    our_call,
                    token,
                    attempts,
                    capabilities,
                    ping_seq,
                    dest,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    pub(super) fn on_auth_req_received(
        &mut self,
        s: AuthReqReceivedData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        // No explicit arm existed for AuthReqReceived in the original
        // handle(); every event fell through to the invalid-transition
        // catch-all leaving state unchanged.
        let state = SessionState::AuthReqReceived(s);
        out.push(Action::LogInvalid {
            reason: invalid_reason(&state, &event),
        });
        (state, out)
    }

    #[allow(clippy::too_many_lines)] // verbatim port of former handle() arm — iax-19ad
    pub(super) fn on_auth_rep_sent(
        &mut self,
        s: AuthRepSentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let AuthRepSentData {
            our_call,
            peer_call,
            attempts,
            sent_at,
            challenge,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, now } => match full_subclass(&frame) {
                Some((Subclass::Iax(IaxCommand::Accept), _, ies_bytes)) => {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let negotiated_format =
                        accepted_format(&ies, self.call_profile.codec_policy, &mut out);
                    out.push(Action::CancelTimer(TimerKind::AuthRepRetry));
                    // iax-e402: re-assert peer_call. Defence-in-depth — the
                    // value was already plumbed at AUTHREQ/CALLTOKEN time,
                    // but the AUTHREQ-less server path (rare but spec-legal)
                    // makes ACCEPT the first reliable peer frame we see.
                    out.push(Action::SetPeerCall(peer_call));
                    out.push(Action::AppEvent(AppEvent::Connected { peer_call }));
                    // iax-a307: start the keepalive engine with the call.
                    let keepalive = KeepaliveState::new(KeepaliveConfig::default(), now);
                    out.push(Action::SetTimer(
                        TimerKind::Keepalive,
                        keepalive.config().ping_interval,
                    ));
                    let _ = challenge;
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at: now,
                            pending_dtmf: None,
                            last_dtmf_at: None,
                            keepalive,
                            last_full_voice: None, // iax-a116: no voice sent yet
                            last_ptt: None,        // iax-d4e9: no PTT signalled yet
                            last_rx_voice_format: None, // iax-a422: no voice rx yet
                            negotiated_format,
                        }),
                        out,
                    )
                }
                Some((Subclass::Iax(IaxCommand::Reject), _, ies_bytes)) => {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let cause = ies.cause.map(str::to_string);
                    let reason = FailReason::Rejected { cause };
                    out.push(Action::CancelTimer(TimerKind::AuthRepRetry));
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    let _ = challenge;
                    (SessionState::Failed(reason), out)
                }
                _ => {
                    out.push(Action::LogInvalid {
                        reason: "unexpected_frame_in_authrep_sent",
                    });
                    (
                        SessionState::AuthRepSent(AuthRepSentData {
                            sent_at,
                            our_call,
                            peer_call,
                            attempts,
                            challenge,
                        }),
                        out,
                    )
                }
            },
            Event::Timer {
                kind: TimerKind::AuthRepRetry,
                ..
            } => {
                if attempts >= 5 {
                    let reason = FailReason::Timeout {
                        in_state: "AuthRepSent",
                    };
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    (SessionState::Failed(reason), out)
                } else {
                    let challenge_str = std::str::from_utf8(&challenge).unwrap_or("");
                    let response = super::auth::md5_response(
                        challenge_str,
                        self.credentials.password.expose(),
                    );
                    let authrep = build_authrep(our_call, peer_call, &response);
                    out.push(Action::SendReliable(authrep));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::AuthRepRetry, backoff));
                    (
                        SessionState::AuthRepSent(AuthRepSentData {
                            our_call,
                            peer_call,
                            attempts: attempts + 1,
                            sent_at,
                            challenge,
                        }),
                        out,
                    )
                }
            }
            event => {
                let state = SessionState::AuthRepSent(AuthRepSentData {
                    sent_at,
                    our_call,
                    peer_call,
                    attempts,
                    challenge,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // verbatim port of former handle() arm — iax-19ad
    pub(super) fn on_active(
        &mut self,
        s: ActiveData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let ActiveData {
            our_call,
            peer_call,
            established_at,
            pending_dtmf,
            last_dtmf_at,
            mut keepalive,
            last_full_voice,
            last_ptt,
            last_rx_voice_format,
            negotiated_format,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::App(cmd) => match cmd {
                AppCommand::SendVoice {
                    format,
                    payload,
                    ts,
                } => {
                    // RFC 5456 §6.4: the FIRST media frame of a call — and any
                    // frame where the codec or the high 16 bits of the 32-bit
                    // timestamp change — MUST be a FULL Voice frame. Mini frames
                    // carry only the low 16 ts bits and inherit the codec +
                    // high-ts context from the last full voice frame; without
                    // that context the peer floods VNAK and hangs up. `ts` is
                    // monotonic per call (a fixed-origin elapsed-ms clock; see
                    // client.rs run_loop), so it never moves backwards within an
                    // Active session — a re-key builds a fresh FSM with
                    // last_full_voice=None, so the first frame is full again,
                    // which is why no backwards-ts guard is needed here. (iax-a116)
                    let need_full = match last_full_voice {
                        None => true, // first voice frame
                        Some((prev_fmt, prev_ts)) => {
                            prev_fmt != format ||              // codec changed
                            (ts & 0xFFFF_0000) != (prev_ts & 0xFFFF_0000)
                        } // high 16 bits changed
                    };
                    let updated_last_full_voice = if need_full {
                        let frame = build_full_voice(our_call, peer_call, format, ts, &payload);
                        out.push(Action::SendReliable(frame));
                        Some((format, ts))
                    } else {
                        let bytes = build_mini_voice(our_call, ts, &payload);
                        out.push(Action::SendUnreliable(bytes));
                        last_full_voice
                    };
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at,
                            keepalive,
                            last_full_voice: updated_last_full_voice,
                            last_ptt,
                            last_rx_voice_format,
                            negotiated_format,
                        }),
                        out,
                    )
                }
                AppCommand::Hangup { cause, now } => {
                    let hangup = build_hangup(our_call, peer_call, cause.as_deref());
                    out.push(Action::CancelTimer(TimerKind::Keepalive));
                    out.push(Action::SendReliable(hangup));
                    out.push(Action::SetTimer(
                        TimerKind::HangupRetry,
                        Duration::from_secs(1),
                    ));
                    let _ = (pending_dtmf, last_dtmf_at, keepalive, last_full_voice);
                    (
                        SessionState::Hangup(HangupData {
                            our_call,
                            peer_call,
                            initiated_by: HangupOrigin::Local,
                            sent_at: now,
                            attempts: 1,
                        }),
                        out,
                    )
                }
                AppCommand::SendDtmf { digit, now } => {
                    if !is_valid_dtmf_digit(digit) {
                        out.push(Action::LogInvalid {
                            reason: "invalid_dtmf_digit",
                        });
                        return (
                            SessionState::Active(ActiveData {
                                our_call,
                                peer_call,
                                established_at,
                                pending_dtmf,
                                last_dtmf_at,
                                keepalive,
                                last_full_voice,
                                last_ptt,
                                last_rx_voice_format,
                                negotiated_format,
                            }),
                            out,
                        );
                    }
                    if let Some(prev) = last_dtmf_at
                        && now.duration_since(prev) < DTMF_RATE_LIMIT {
                            out.push(Action::LogInvalid {
                                reason: "dtmf_rate_limited",
                            });
                            return (
                                SessionState::Active(ActiveData {
                                    our_call,
                                    peer_call,
                                    established_at,
                                    pending_dtmf,
                                    last_dtmf_at,
                                    keepalive,
                                    last_full_voice,
                                    last_ptt,
                                    last_rx_voice_format,
                                    negotiated_format,
                                }),
                                out,
                            );
                        }
                    #[allow(clippy::cast_possible_truncation)]
                    // 32-bit ms wraps ~49 days; calls won't survive that long
                    // before reliability gives up.
                    let ts_ms = now.duration_since(established_at).as_millis() as u32;
                    let begin = build_dtmf_begin(our_call, peer_call, digit, ts_ms);
                    let end = build_dtmf_end(our_call, peer_call, digit, ts_ms.saturating_add(100));
                    out.push(Action::SendReliable(begin));
                    out.push(Action::SendReliable(end));
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at: Some(now),
                            keepalive,
                            last_full_voice,
                            last_ptt,
                            last_rx_voice_format,
                            negotiated_format,
                        }),
                        out,
                    )
                }
                AppCommand::SendText(body) => {
                    out.push(Action::SendReliable(build_text(our_call, peer_call, &body)));
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at,
                            keepalive,
                            last_full_voice,
                            last_ptt,
                            last_rx_voice_format,
                            negotiated_format,
                        }),
                        out,
                    )
                }
                AppCommand::SendPtt(engaged) => {
                    // Coalesce: only signal on an actual change of keyed state (iax-d4e9).
                    if last_ptt == Some(engaged) {
                        return (
                            SessionState::Active(ActiveData {
                                our_call,
                                peer_call,
                                established_at,
                                pending_dtmf,
                                last_dtmf_at,
                                keepalive,
                                last_full_voice,
                                last_ptt,
                                last_rx_voice_format,
                                negotiated_format,
                            }),
                            out,
                        );
                    }
                    let frame = if engaged {
                        build_radio_key(our_call, peer_call)
                    } else {
                        build_radio_unkey(our_call, peer_call)
                    };
                    out.push(Action::SendReliable(frame));
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at,
                            keepalive,
                            last_full_voice,
                            last_ptt: Some(engaged),
                            last_rx_voice_format,
                            negotiated_format,
                        }),
                        out,
                    )
                }
                AppCommand::StartCall { .. }
                // iax-8baf inbound seams are never valid in an Active outbound
                // call leg; treat them like the unimplemented StartCall path.
                | AppCommand::DriveInbound { .. }
                | AppCommand::AcceptIncoming { .. }
                | AppCommand::AnswerIncoming { .. }
                | AppCommand::RejectIncoming { .. }
                | AppCommand::AnswerAcked { .. } => {
                    out.push(Action::LogInvalid {
                        reason: "command_not_implemented_in_c333",
                    });
                    (
                        SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at,
                            keepalive,
                            last_full_voice,
                            last_ptt,
                            last_rx_voice_format,
                            negotiated_format,
                        }),
                        out,
                    )
                }
            },
            Event::Frame { frame, now } => {
                // iax-a307: any inbound frame is proof of life. Edge-restores
                // before subclass dispatch so the Restored event precedes
                // whatever the frame itself produces.
                if keepalive.on_inbound(now) {
                    out.push(Action::AppEvent(AppEvent::ConnectionRestored));
                }
                match &frame {
                    Frame::Mini(m) => {
                        // RFC 5456 §6.4: a mini frame carries no subclass, so its
                        // implicit codec is that of the last full voice frame
                        // received on this leg (iax-a422). Before any full voice
                        // frame, fall back to the NEGOTIATED format (iax-408b):
                        // real peers do open with minis (ASL3 node 2002 sends its
                        // announcement that way), and the old hardcoded G.711µ
                        // fallback ran 16-bit slin16 payloads through the µ-law
                        // expander — loud digital noise.
                        let format = last_rx_voice_format.unwrap_or(negotiated_format);
                        out.push(Action::AppEvent(AppEvent::VoiceReceived {
                            format,
                            payload: m.payload.to_vec(),
                            ts: u32::from(m.timestamp),
                        }));
                        let next = SessionState::Active(ActiveData {
                            our_call,
                            peer_call,
                            established_at,
                            pending_dtmf,
                            last_dtmf_at,
                            keepalive,
                            last_full_voice,
                            last_ptt,
                            last_rx_voice_format,
                            negotiated_format,
                        });
                        let _ = now;
                        (next, out)
                    }
                    Frame::Full(full) => {
                        // DTMF dispatch checks frame_type first; the subclass
                        // carries the validated digit as `Subclass::Dtmf(char)`
                        // (RFC 5456 §6.7).
                        if matches!(full.frame_type, crate::subclass::FrameType::DtmfBegin) {
                            let digit = match full.subclass {
                                Subclass::Dtmf(c) => c,
                                _ => '\0',
                            };
                            return (
                                SessionState::Active(ActiveData {
                                    our_call,
                                    peer_call,
                                    established_at,
                                    pending_dtmf: Some(digit),
                                    last_dtmf_at,
                                    keepalive,
                                    last_full_voice,
                                    last_ptt,
                                    last_rx_voice_format,
                                    negotiated_format,
                                }),
                                out,
                            );
                        }
                        if matches!(full.frame_type, crate::subclass::FrameType::DtmfEnd) {
                            let digit = match full.subclass {
                                Subclass::Dtmf(c) => c,
                                _ => '\0',
                            };
                            out.push(Action::AppEvent(AppEvent::DtmfReceived(digit)));
                            return (
                                SessionState::Active(ActiveData {
                                    our_call,
                                    peer_call,
                                    established_at,
                                    pending_dtmf: None,
                                    last_dtmf_at,
                                    keepalive,
                                    last_full_voice,
                                    last_ptt,
                                    last_rx_voice_format,
                                    negotiated_format,
                                }),
                                out,
                            );
                        }
                        // TEXT frames carry app_rpt link-control strings in the
                        // payload (raw UTF-8, not IEs). Dispatch on body content
                        // before the subclass match (Text uses Subclass::Raw(0)).
                        if matches!(full.frame_type, FrameType::Text) {
                            let body = String::from_utf8_lossy(full.payload);
                            if body == "!NEWKEY1!" {
                                // app_rpt newkey handshake — echo back.
                                out.push(Action::SendReliable(build_text(
                                    our_call,
                                    peer_call,
                                    "!NEWKEY1!",
                                )));
                                return (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                );
                            } else if body == "!!DISCONNECT!!" {
                                // app_rpt peer teardown — mirror the peer-HANGUP
                                // arm: cancel keepalive, set HangupRetry timer,
                                // emit Disconnected, go to Hangup with Peer origin.
                                out.push(Action::CancelTimer(TimerKind::Keepalive));
                                out.push(Action::SetTimer(
                                    TimerKind::HangupRetry,
                                    Duration::from_secs(1),
                                ));
                                out.push(Action::AppEvent(AppEvent::Disconnected {
                                    reason: FailReason::RemoteHangup {
                                        cause: Some("disconnect".into()),
                                    },
                                }));
                                let _ = (pending_dtmf, last_dtmf_at, keepalive, last_full_voice);
                                return (
                                    SessionState::Hangup(HangupData {
                                        our_call,
                                        peer_call,
                                        initiated_by: HangupOrigin::Peer,
                                        sent_at: now,
                                        attempts: 1,
                                    }),
                                    out,
                                );
                            }
                            // Periodic status frames (e.g. "L <linklist>") —
                            // parse and surface to the application layer.
                            {
                                let event = crate::text::TextEvent::parse(full.payload)
                                    .unwrap_or(crate::text::TextEvent::Raw(""))
                                    .to_owned();
                                out.push(Action::AppEvent(AppEvent::TextReceived(event)));
                                return (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                );
                            }
                        }
                        // iax-408b: some peers (app_rpt-2026 chan_iax2, e.g. ASL
                        // node 2002) send full VOICE frames with subclass byte 0 —
                        // no format signaled — which parse_lenient surfaces as
                        // `Raw(0)`. Treat that as "the negotiated format": ignoring
                        // the frame both dropped its payload and left
                        // `last_rx_voice_format` unseeded, so every following mini
                        // fell back to the wrong codec.
                        let voice_format = match full.subclass {
                            Subclass::Voice(format) => Some(format),
                            Subclass::Raw(0)
                                if matches!(full.frame_type, crate::subclass::FrameType::Voice) =>
                            {
                                Some(negotiated_format)
                            }
                            _ => None,
                        };
                        if let Some(format) = voice_format {
                            // RFC 5456 §6.4: voice full frames carry the codec
                            // samples in the bytes after the 12-byte header.
                            // iax-bf0b: previously we emitted an empty payload,
                            // dropping the first 20ms of every G.711U stream.
                            out.push(Action::AppEvent(AppEvent::VoiceReceived {
                                format,
                                payload: full.payload.to_vec(),
                                ts: full.timestamp,
                            }));
                            // iax-a422: remember this leg's current voice codec
                            // so subsequent mini frames (which carry no subclass)
                            // are reported in the right format (RFC 5456 §6.4).
                            return (
                                SessionState::Active(ActiveData {
                                    our_call,
                                    peer_call,
                                    established_at,
                                    pending_dtmf,
                                    last_dtmf_at,
                                    keepalive,
                                    last_full_voice,
                                    last_ptt,
                                    last_rx_voice_format: Some(format),
                                    negotiated_format,
                                }),
                                out,
                            );
                        }
                        match full.subclass {
                            Subclass::Iax(IaxCommand::Ping) => {
                                // RFC 5456 §6.7.3: PONG echoes the PING timestamp.
                                out.push(Action::SendReliable(build_pong(
                                    our_call,
                                    peer_call,
                                    full.timestamp,
                                )));
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Iax(IaxCommand::LagRq) => {
                                // RFC 5456 §6.7.5: LAGRP echoes the LAGRQ timestamp.
                                out.push(Action::SendReliable(build_lagrp(
                                    our_call,
                                    peer_call,
                                    full.timestamp,
                                )));
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Iax(IaxCommand::Pong) => {
                                keepalive.on_pong(full.timestamp, now);
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Iax(IaxCommand::LagRp) => {
                                keepalive.on_lagrp(full.timestamp, now);
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Iax(IaxCommand::Inval) => {
                                // RFC 5456 §6.9.2: the peer has no state for this
                                // call (e.g. it restarted). Locked Q4: hard-fail,
                                // no automatic re-establish — recovery is the
                                // consumer's job (re-dial).
                                out.push(Action::CancelTimer(TimerKind::Keepalive));
                                out.push(Action::AppEvent(AppEvent::Disconnected {
                                    reason: FailReason::PeerInval,
                                }));
                                let _ = (pending_dtmf, last_dtmf_at, keepalive, last_full_voice);
                                (SessionState::Failed(FailReason::PeerInval), out)
                            }
                            Subclass::Control(crate::subclass::ControlSubclass::Hangup)
                            | Subclass::Iax(IaxCommand::Hangup) => {
                                // Peer-initiated teardown. Asterisk emits
                                // Iax(Hangup) (IAX_COMMAND_HANGUP=5); some peers use
                                // the Control variant. RFC 5456 §8.4.7: we ACK and
                                // close. The HangupRetry timer drives our own
                                // outbound HANGUP/ACK cycle.
                                let cause = full.ies.cause.map(str::to_string);
                                out.push(Action::CancelTimer(TimerKind::Keepalive));
                                out.push(Action::SetTimer(
                                    TimerKind::HangupRetry,
                                    Duration::from_secs(1),
                                ));
                                out.push(Action::AppEvent(AppEvent::Disconnected {
                                    reason: FailReason::RemoteHangup { cause },
                                }));
                                let _ = (pending_dtmf, last_dtmf_at, keepalive, last_full_voice);
                                (
                                    SessionState::Hangup(HangupData {
                                        our_call,
                                        peer_call,
                                        initiated_by: HangupOrigin::Peer,
                                        sent_at: now,
                                        attempts: 1,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Control(crate::subclass::ControlSubclass::RadioKey) => {
                                out.push(Action::AppEvent(AppEvent::RemotePtt(true)));
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            Subclass::Control(crate::subclass::ControlSubclass::RadioUnkey) => {
                                out.push(Action::AppEvent(AppEvent::RemotePtt(false)));
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            // RFC 5456 §6.3 call-progress CONTROL frames. The
                            // transport is already up (ACCEPT moved us to
                            // Active); these surface intermediate progress.
                            // ANSWER is the real "far end picked up" signal.
                            // iax-85e7.
                            Subclass::Control(
                                progress @ (crate::subclass::ControlSubclass::Proceeding
                                | crate::subclass::ControlSubclass::Progress
                                | crate::subclass::ControlSubclass::Ringing
                                | crate::subclass::ControlSubclass::Answer),
                            ) => {
                                let event = match progress {
                                    crate::subclass::ControlSubclass::Ringing => {
                                        CallProgress::Ringing
                                    }
                                    crate::subclass::ControlSubclass::Answer => {
                                        CallProgress::Answered
                                    }
                                    // Proceeding + Progress both map to Proceeding.
                                    _ => CallProgress::Proceeding,
                                };
                                out.push(Action::AppEvent(AppEvent::CallProgress(event)));
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                            _ => {
                                out.push(Action::LogInvalid {
                                    reason: "unhandled_subclass_in_active",
                                });
                                (
                                    SessionState::Active(ActiveData {
                                        our_call,
                                        peer_call,
                                        established_at,
                                        pending_dtmf,
                                        last_dtmf_at,
                                        keepalive,
                                        last_full_voice,
                                        last_ptt,
                                        last_rx_voice_format,
                                        negotiated_format,
                                    }),
                                    out,
                                )
                            }
                        }
                    }
                }
            }
            Event::Timer {
                kind: TimerKind::Keepalive,
                now,
            } => {
                #[allow(clippy::cast_possible_truncation)]
                // 32-bit ms wraps ~49 days; calls won't survive that long.
                let ts_ms = now.duration_since(established_at).as_millis() as u32;
                let tick = keepalive.on_ping_timer(ts_ms, now);
                // PING for network RTT, LAGRQ for end-to-end lag — same
                // cadence, same echoed timestamp (spec: locked Q2/Q3).
                out.push(Action::SendReliable(build_ping(our_call, peer_call, ts_ms)));
                out.push(Action::SendReliable(build_lagrq(
                    our_call, peer_call, ts_ms,
                )));
                out.push(Action::SetTimer(
                    TimerKind::Keepalive,
                    keepalive.config().ping_interval,
                ));
                if tick.connection_lost {
                    out.push(Action::AppEvent(AppEvent::ConnectionLost));
                }
                (
                    SessionState::Active(ActiveData {
                        our_call,
                        peer_call,
                        established_at,
                        pending_dtmf,
                        last_dtmf_at,
                        keepalive,
                        last_full_voice,
                        last_ptt,
                        last_rx_voice_format,
                        negotiated_format,
                    }),
                    out,
                )
            }
            event => {
                let state = SessionState::Active(ActiveData {
                    our_call,
                    peer_call,
                    established_at,
                    pending_dtmf,
                    last_dtmf_at,
                    keepalive,
                    last_full_voice,
                    last_ptt,
                    last_rx_voice_format,
                    negotiated_format,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    pub(super) fn on_hangup(
        &mut self,
        s: HangupData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let HangupData {
            our_call,
            peer_call,
            initiated_by,
            sent_at,
            attempts,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, .. } => {
                if let Some((Subclass::Iax(IaxCommand::Ack), _, _)) = full_subclass(&frame) {
                    out.push(Action::CancelTimer(TimerKind::HangupRetry));
                    if matches!(initiated_by, HangupOrigin::Local) {
                        out.push(Action::AppEvent(AppEvent::Disconnected {
                            reason: FailReason::Aborted,
                        }));
                    }
                    (SessionState::Closed, out)
                } else {
                    out.push(Action::LogInvalid {
                        reason: "non_ack_in_hangup",
                    });
                    (
                        SessionState::Hangup(HangupData {
                            our_call,
                            peer_call,
                            initiated_by,
                            sent_at,
                            attempts,
                        }),
                        out,
                    )
                }
            }
            Event::Timer {
                kind: TimerKind::HangupRetry,
                ..
            } => {
                if attempts >= 3 {
                    if matches!(initiated_by, HangupOrigin::Local) {
                        out.push(Action::AppEvent(AppEvent::Disconnected {
                            reason: FailReason::Aborted,
                        }));
                    }
                    (SessionState::Closed, out)
                } else {
                    let hangup = build_hangup(our_call, peer_call, None);
                    out.push(Action::SendReliable(hangup));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(2)));
                    out.push(Action::SetTimer(TimerKind::HangupRetry, backoff));
                    (
                        SessionState::Hangup(HangupData {
                            our_call,
                            peer_call,
                            initiated_by,
                            sent_at,
                            attempts: attempts + 1,
                        }),
                        out,
                    )
                }
            }
            event => {
                let state = SessionState::Hangup(HangupData {
                    our_call,
                    peer_call,
                    initiated_by,
                    sent_at,
                    attempts,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    pub(super) fn on_closed(&mut self, event: Event<'_>) -> (SessionState, SmallVec<[Action; 4]>) {
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        // No explicit arm existed for Closed in the original handle(); every
        // event fell through to the invalid-transition catch-all leaving
        // state unchanged.
        out.push(Action::LogInvalid {
            reason: invalid_reason(&SessionState::Closed, &event),
        });
        (SessionState::Closed, out)
    }

    pub(super) fn on_failed(
        &mut self,
        reason: FailReason,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        // No explicit arm existed for Failed in the original handle(); every
        // event fell through to the invalid-transition catch-all leaving
        // state unchanged.
        let state = SessionState::Failed(reason);
        out.push(Action::LogInvalid {
            reason: invalid_reason(&state, &event),
        });
        (state, out)
    }

    /// iax-6c21: a reliable frame exhausted its retransmits
    /// (`RxOutcome::GaveUp`) — the peer is unreachable. Terminate the call.
    ///
    /// Semantics by state class:
    /// * Pre-Active handshake states (outbound NEW/AUTH dance and the inbound
    ///   NEW/ACCEPT/ANSWER dance): the call never reached `Active`, so this is a
    ///   setup failure — FAIL to `Failed(Timeout { in_state })` and emit
    ///   `Disconnected`. Retransmit exhaustion is, semantically, a timeout.
    /// * `Active`: tear down to a clean terminal state. Because delivery already
    ///   gave up, the link is dead — sending another reliable HANGUP would only
    ///   fail again — so we go straight to `Closed`, cancel the keepalive timer,
    ///   and emit `Disconnected` (no `HangupRetry` handshake).
    /// * `Hangup`: a HANGUP we were retransmitting was abandoned. The teardown is
    ///   effectively done — settle into `Closed`, emitting `Disconnected` only for
    ///   a locally-initiated hangup (mirrors `on_hangup`'s ack/give-up arms).
    /// * Terminal/`Init` states have no in-flight reliable frame of their own; a
    ///   stray `DeliveryFailed` is a no-op `LogInvalid`.
    ///
    /// Timers are cancelled per state so the runtime does not keep firing retries
    /// at a call that has already terminated.
    pub(super) fn on_delivery_failed(
        &mut self,
        state: SessionState,
        oseqno: u8,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let _ = oseqno; // identifies which frame failed; the call dies regardless.
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match state {
            // --- Pre-Active handshake states: FAIL the call. ---
            SessionState::NewSent(_) => {
                out.push(Action::CancelTimer(TimerKind::NewRetry));
                Self::fail_on_delivery(&mut out, "NewSent")
            }
            SessionState::CallTokenReceived(_) => {
                Self::fail_on_delivery(&mut out, "CallTokenReceived")
            }
            SessionState::NewResent(_) => {
                out.push(Action::CancelTimer(TimerKind::NewRetry));
                out.push(Action::CancelTimer(TimerKind::TokenExpiry));
                Self::fail_on_delivery(&mut out, "NewResent")
            }
            SessionState::AuthReqReceived(_) => Self::fail_on_delivery(&mut out, "AuthReqReceived"),
            SessionState::AuthRepSent(_) => {
                out.push(Action::CancelTimer(TimerKind::AuthRepRetry));
                Self::fail_on_delivery(&mut out, "AuthRepSent")
            }
            SessionState::NewReceived(_) => Self::fail_on_delivery(&mut out, "NewReceived"),
            SessionState::CallTokenIssued(_) => {
                out.push(Action::CancelTimer(TimerKind::InboundTokenExpiry));
                Self::fail_on_delivery(&mut out, "CallTokenIssued")
            }
            SessionState::AuthReqSent(_) => {
                out.push(Action::CancelTimer(TimerKind::AuthReqRetry));
                Self::fail_on_delivery(&mut out, "AuthReqSent")
            }
            SessionState::AcceptSent(_) => {
                out.push(Action::CancelTimer(TimerKind::AcceptRetry));
                out.push(Action::CancelTimer(TimerKind::AcceptDecisionTimeout));
                Self::fail_on_delivery(&mut out, "AcceptSent")
            }
            SessionState::AnswerSent(_) => {
                out.push(Action::CancelTimer(TimerKind::AnswerRetry));
                Self::fail_on_delivery(&mut out, "AnswerSent")
            }
            // --- Active: clean teardown to Closed. ---
            SessionState::Active(_) => {
                out.push(Action::CancelTimer(TimerKind::Keepalive));
                out.push(Action::AppEvent(AppEvent::Disconnected {
                    reason: FailReason::Timeout { in_state: "Active" },
                }));
                (SessionState::Closed, out)
            }
            // --- Hangup: the teardown frame was abandoned; settle into Closed. ---
            SessionState::Hangup(h) => {
                out.push(Action::CancelTimer(TimerKind::HangupRetry));
                if matches!(h.initiated_by, HangupOrigin::Local) {
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: FailReason::Aborted,
                    }));
                }
                (SessionState::Closed, out)
            }
            // --- Terminal / Init: no in-flight reliable frame of our own. ---
            SessionState::Init | SessionState::Closed | SessionState::Failed(_) => {
                out.push(Action::LogInvalid {
                    reason: "delivery_failed_in_terminal_state",
                });
                (state, out)
            }
        }
    }

    /// Shared tail for the pre-Active `DeliveryFailed` arms: emit `Disconnected`
    /// and transition to `Failed(Timeout { in_state })`.
    fn fail_on_delivery(
        out: &mut SmallVec<[Action; 4]>,
        in_state: &'static str,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let reason = FailReason::Timeout { in_state };
        out.push(Action::AppEvent(AppEvent::Disconnected {
            reason: reason.clone(),
        }));
        (SessionState::Failed(reason), std::mem::take(out))
    }
}

/// The FORMAT the peer's ACCEPT names, if we can actually encode it; else our
/// policy preference. A peer naming a format we never offered violates the
/// exchange — trace it, don't hang up (degraded beats dead on RF links).
fn accepted_format(ies: &Ies, policy: CodecPolicy, out: &mut SmallVec<[Action; 4]>) -> VoiceFormat {
    match ies.format.and_then(VoiceFormat::from_u32) {
        Some(f) if CodecPolicy::is_encodable(f) => f,
        Some(_) => {
            out.push(Action::LogInvalid {
                reason: "accept_format_unsupported",
            });
            policy.preferred()
        }
        None => policy.preferred(),
    }
}
