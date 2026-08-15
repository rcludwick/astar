// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Per-state event handlers for the inbound (NEW-received) call FSM.
//!
//! iax-19ad created this as an empty stub; iax-8baf adds the inbound states
//! (`NewReceived`, `CallTokenIssued`, `AuthReqSent`, `AcceptSent`,
//! `AnswerSent`) as `on_<state>` methods here, matching the outbound handler
//! shape in `handlers_outbound.rs`.
#![allow(clippy::unused_self, clippy::needless_pass_by_value)]

use std::time::{Duration, Instant};

use smallvec::SmallVec;

use super::builders::{
    build_accept, build_answer, build_authreq, build_calltoken, build_hangup, build_reject,
};
use super::call_no::CallNo;
use super::fsm::{CodecMask, full_subclass, invalid_reason};
use super::keepalive::{KeepaliveConfig, KeepaliveState};
use super::{
    AcceptSentData, Action, ActiveData, AnswerSentData, AppCommand, AppEvent, AuthReqSentData,
    CallTokenIssuedData, CodecPolicy, Event, FailReason, Fsm, HangupData, HangupOrigin,
    IncomingOffer, NewReceivedData, SessionState, TimerKind,
};
use crate::frame::Subclass;
use crate::ie::Ies;
use crate::subclass::{IaxCommand, VoiceFormat};

/// Pick the codec for the ACCEPT FORMAT IE. A deferential policy
/// (`UlawOnly`/`AllowSlin`) honors the peer's stated FORMAT if we support it
/// and it was offered in CAPABILITY; an asserting policy (`PreferSlin`/
/// `PreferSlin16`, iax-d0cc) instead takes its own most-preferred codec common
/// to both, so a wideband node pulls a capable caller up even when the caller
/// prefers ulaw. Either way, fall to the first codec in our preference order
/// common to both, else our preference (the peer offered nothing usable —
/// ACCEPT still names a single format per RFC 5456 design decision 2).
fn choose_codec(
    offered: CodecMask,
    peer_pref: Option<VoiceFormat>,
    policy: CodecPolicy,
) -> VoiceFormat {
    let ours = policy.capability_mask();
    // If the peer advertised no CAPABILITY (offered empty) treat every codec
    // we support as acceptable — many peers send only a FORMAT.
    let common = if offered.is_empty() {
        ours
    } else {
        offered.intersect(ours)
    };

    // iax-d0cc: a Prefer* policy asserts its own preference over a capable
    // caller's stated FORMAT (so a prefer_slin16 node pulls a slin16-capable
    // but ulaw-preferring caller up to wideband). Deferential policies
    // (UlawOnly/AllowSlin) honor the caller's FORMAT when we can.
    if !policy.asserts_preference()
        && let Some(p) = peer_pref
        && common.contains(p)
    {
        return p;
    }
    for &fmt in policy.preference_order() {
        if common.contains(fmt) {
            return fmt;
        }
    }
    policy.preferred()
}

impl Fsm {
    pub(super) fn on_new_received(
        &mut self,
        s: NewReceivedData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let NewReceivedData {
            peer_call,
            our_call,
            offered,
            received_at,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        let now = match &event {
            Event::App(AppCommand::DriveInbound { now }) => *now,
            ev => {
                let st = SessionState::NewReceived(NewReceivedData {
                    peer_call,
                    our_call,
                    offered,
                    received_at,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&st, ev),
                });
                return (st, out);
            }
        };

        // CALLTOKEN policy: demand a token round-trip if required and the peer
        // did not already echo one (IE absent or present-but-empty).
        let token_absent = offered
            .peer_calltoken
            .as_deref()
            .is_none_or(<[u8]>::is_empty);
        if self.inbound_policy.calltoken_required && token_absent {
            let token = self.pending_token.clone().unwrap_or_default();
            out.push(Action::SendReliable(build_calltoken(
                our_call,
                peer_call,
                token.as_bytes(),
            )));
            out.push(Action::SetTimer(
                TimerKind::InboundTokenExpiry,
                Duration::from_secs(10),
            ));
            return (
                SessionState::CallTokenIssued(CallTokenIssuedData {
                    peer_call,
                    our_call,
                    offered,
                    token,
                    issued_at: now,
                }),
                out,
            );
        }

        if self.inbound_policy.auth_required {
            return self.emit_authreq(our_call, peer_call, offered, now, &mut out);
        }
        self.emit_accept(our_call, peer_call, offered, now, &mut out)
    }

    /// Emit AUTHREQ (MD5 challenge) and move to `AuthReqSent`. Shared by
    /// `on_new_received` and `on_calltoken_issued`.
    fn emit_authreq(
        &mut self,
        our_call: CallNo,
        peer_call: CallNo,
        offered: IncomingOffer,
        now: Instant,
        out: &mut SmallVec<[Action; 4]>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let challenge = self.pending_challenge.clone().unwrap_or_default();
        out.push(Action::SetPeerCall(peer_call));
        out.push(Action::SendReliable(build_authreq(
            our_call, peer_call, &challenge,
        )));
        out.push(Action::SetTimer(
            TimerKind::AuthReqRetry,
            Duration::from_secs(1),
        ));
        (
            SessionState::AuthReqSent(AuthReqSentData {
                peer_call,
                our_call,
                challenge: challenge.into_bytes(),
                attempts: 1,
                sent_at: now,
                offered,
            }),
            std::mem::take(out),
        )
    }

    /// Emit ACCEPT (chosen FORMAT), surface `IncomingCall`, and move to
    /// `AcceptSent` (or straight to `AnswerSent` under `auto_answer`). Shared by
    /// `on_new_received`, `on_calltoken_issued`, and `on_auth_req_sent`.
    fn emit_accept(
        &mut self,
        our_call: CallNo,
        peer_call: CallNo,
        offered: IncomingOffer,
        now: Instant,
        out: &mut SmallVec<[Action; 4]>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let chosen = choose_codec(
            offered.offered_codecs,
            offered.preferred_codec,
            self.inbound_policy.codec_policy,
        );
        out.push(Action::SetPeerCall(peer_call));
        out.push(Action::SendReliable(build_accept(
            our_call, peer_call, chosen,
        )));
        out.push(Action::AppEvent(AppEvent::IncomingCall {
            our_call,
            peer_call,
            calling_number: offered.calling_number.clone(),
            calling_name: offered.calling_name.clone(),
            called_number: offered.called_number.clone(),
            username: offered.username.clone(),
            offered_codecs: offered.offered_codecs,
            preferred_codec: offered.preferred_codec,
            language: offered.language.clone(),
        }));
        if self.inbound_policy.auto_answer {
            out.push(Action::SendReliable(build_answer(our_call, peer_call)));
            out.push(Action::SetTimer(
                TimerKind::AnswerRetry,
                Duration::from_secs(1),
            ));
            return (
                SessionState::AnswerSent(AnswerSentData {
                    peer_call,
                    our_call,
                    chosen_format: chosen,
                    attempts: 1,
                    sent_at: now,
                }),
                std::mem::take(out),
            );
        }
        // Always arm the ACCEPT retransmit; arm the decision-timeout only when
        // the app is the one deciding (AppDecide).
        out.push(Action::SetTimer(
            TimerKind::AcceptRetry,
            Duration::from_secs(1),
        ));
        if self.inbound_policy.decision_is_app {
            out.push(Action::SetTimer(
                TimerKind::AcceptDecisionTimeout,
                self.inbound_policy.accept_decision_timeout,
            ));
        }
        (
            SessionState::AcceptSent(AcceptSentData {
                peer_call,
                our_call,
                chosen_format: chosen,
                attempts: 1,
                sent_at: now,
                awaiting_app_decision: self.inbound_policy.decision_is_app,
            }),
            std::mem::take(out),
        )
    }

    /// Shared peer-HANGUP teardown for the inbound setup states (`AuthReqSent`,
    /// `AcceptSent`, `AnswerSent`): cancel keepalive (harmless if unarmed), arm
    /// the `HangupRetry` timer, surface `Disconnected { RemoteHangup }`, and move
    /// to `Hangup { Peer }`. Mirrors the outbound peer-HANGUP arm.
    fn peer_hangup_during_setup(
        &mut self,
        our_call: CallNo,
        peer_call: CallNo,
        ies_bytes: &[u8],
        now: Instant,
        out: &mut SmallVec<[Action; 4]>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let ies = Ies::parse(ies_bytes).unwrap_or_else(|_| Ies::empty());
        let cause = ies.cause.map(str::to_string);
        out.push(Action::SetTimer(
            TimerKind::HangupRetry,
            Duration::from_secs(1),
        ));
        out.push(Action::AppEvent(AppEvent::Disconnected {
            reason: FailReason::RemoteHangup { cause },
        }));
        (
            SessionState::Hangup(HangupData {
                our_call,
                peer_call,
                initiated_by: HangupOrigin::Peer,
                sent_at: now,
                attempts: 1,
            }),
            std::mem::take(out),
        )
    }

    pub(super) fn on_calltoken_issued(
        &mut self,
        s: CallTokenIssuedData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let CallTokenIssuedData {
            peer_call,
            our_call,
            offered,
            token,
            issued_at,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, now } => {
                if let Some((Subclass::Iax(IaxCommand::New), _src, ies_bytes)) =
                    full_subclass(&frame)
                {
                    let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                    let received = ies.calltoken.unwrap_or(&[]);
                    if super::auth::token_eq(token.as_bytes(), received) {
                        // Valid resent NEW: the real call leg begins. Reset the
                        // runtime's Reliability seqno bookkeeping (single-use:
                        // the token is consumed by not re-storing it). Re-parse
                        // the offer from this NEW so the post-token IEs win.
                        out.push(Action::ResetReliability);
                        let offer = IncomingOffer::from_new_ies(&ies).unwrap_or(offered);
                        if self.inbound_policy.auth_required {
                            return self.emit_authreq(our_call, peer_call, offer, now, &mut out);
                        }
                        self.emit_accept(our_call, peer_call, offer, now, &mut out)
                    } else {
                        // Mismatched/absent token: anti-spoof reject.
                        let cause = "Call Token Invalid";
                        out.push(Action::SendReliable(build_reject(
                            our_call,
                            peer_call,
                            Some(cause),
                        )));
                        out.push(Action::CancelTimer(TimerKind::InboundTokenExpiry));
                        (
                            SessionState::Failed(FailReason::Rejected {
                                cause: Some(cause.to_string()),
                            }),
                            out,
                        )
                    }
                } else {
                    let state = SessionState::CallTokenIssued(CallTokenIssuedData {
                        peer_call,
                        our_call,
                        offered,
                        token,
                        issued_at,
                    });
                    out.push(Action::LogInvalid {
                        reason: "unexpected_frame_in_calltoken_issued",
                    });
                    (state, out)
                }
            }
            Event::Timer {
                kind: TimerKind::InboundTokenExpiry,
                ..
            } => {
                // Drop silently: no REJECT (no point alerting a scanner).
                (
                    SessionState::Failed(FailReason::Timeout {
                        in_state: "CallTokenIssued",
                    }),
                    out,
                )
            }
            event => {
                let state = SessionState::CallTokenIssued(CallTokenIssuedData {
                    peer_call,
                    our_call,
                    offered,
                    token,
                    issued_at,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // mirrors on_auth_rep_sent's shape
    pub(super) fn on_auth_req_sent(
        &mut self,
        s: AuthReqSentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let AuthReqSentData {
            peer_call,
            our_call,
            challenge,
            attempts,
            sent_at,
            offered,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            Event::Frame { frame, now } => {
                if let Some((subclass, _src, ies_bytes)) = full_subclass(&frame) {
                    match subclass {
                        Subclass::Iax(IaxCommand::AuthRep) => {
                            let ies = Ies::parse(&ies_bytes).unwrap_or_else(|_| Ies::empty());
                            let candidate = ies.md5_result.unwrap_or("");
                            let challenge_str = std::str::from_utf8(&challenge).unwrap_or("");
                            if super::auth::md5_verify(
                                challenge_str,
                                self.credentials.password.expose(),
                                candidate,
                            ) {
                                out.push(Action::CancelTimer(TimerKind::AuthReqRetry));
                                self.emit_accept(our_call, peer_call, offered, now, &mut out)
                            } else {
                                let cause = "Authentication failed";
                                out.push(Action::CancelTimer(TimerKind::AuthReqRetry));
                                out.push(Action::SendReliable(build_reject(
                                    our_call,
                                    peer_call,
                                    Some(cause),
                                )));
                                (
                                    SessionState::Failed(FailReason::Rejected {
                                        cause: Some(cause.to_string()),
                                    }),
                                    out,
                                )
                            }
                        }
                        Subclass::Control(crate::subclass::ControlSubclass::Hangup)
                        | Subclass::Iax(IaxCommand::Hangup) => self.peer_hangup_during_setup(
                            our_call, peer_call, &ies_bytes, now, &mut out,
                        ),
                        _ => {
                            out.push(Action::LogInvalid {
                                reason: "unexpected_frame_in_auth_req_sent",
                            });
                            (
                                SessionState::AuthReqSent(AuthReqSentData {
                                    peer_call,
                                    our_call,
                                    challenge,
                                    attempts,
                                    sent_at,
                                    offered,
                                }),
                                out,
                            )
                        }
                    }
                } else {
                    out.push(Action::LogInvalid {
                        reason: "unexpected_frame_in_auth_req_sent",
                    });
                    (
                        SessionState::AuthReqSent(AuthReqSentData {
                            peer_call,
                            our_call,
                            challenge,
                            attempts,
                            sent_at,
                            offered,
                        }),
                        out,
                    )
                }
            }
            Event::Timer {
                kind: TimerKind::AuthReqRetry,
                ..
            } => {
                if attempts >= 5 {
                    let reason = FailReason::Timeout {
                        in_state: "AuthReqSent",
                    };
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    (SessionState::Failed(reason), out)
                } else {
                    let challenge_str = std::str::from_utf8(&challenge).unwrap_or("");
                    out.push(Action::SendReliable(build_authreq(
                        our_call,
                        peer_call,
                        challenge_str,
                    )));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::AuthReqRetry, backoff));
                    (
                        SessionState::AuthReqSent(AuthReqSentData {
                            peer_call,
                            our_call,
                            challenge,
                            attempts: attempts + 1,
                            sent_at,
                            offered,
                        }),
                        out,
                    )
                }
            }
            event => {
                let state = SessionState::AuthReqSent(AuthReqSentData {
                    peer_call,
                    our_call,
                    challenge,
                    attempts,
                    sent_at,
                    offered,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one arm per inbound event, like on_active
    pub(super) fn on_accept_sent(
        &mut self,
        s: AcceptSentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let AcceptSentData {
            peer_call,
            our_call,
            chosen_format,
            attempts,
            sent_at,
            awaiting_app_decision,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        let keep = |out: &mut SmallVec<[Action; 4]>, reason: &'static str| {
            out.push(Action::LogInvalid { reason });
        };
        match event {
            Event::App(AppCommand::AnswerIncoming { now }) => {
                out.push(Action::CancelTimer(TimerKind::AcceptDecisionTimeout));
                out.push(Action::CancelTimer(TimerKind::AcceptRetry));
                out.push(Action::SendReliable(build_answer(our_call, peer_call)));
                out.push(Action::SetTimer(
                    TimerKind::AnswerRetry,
                    Duration::from_secs(1),
                ));
                (
                    SessionState::AnswerSent(AnswerSentData {
                        peer_call,
                        our_call,
                        chosen_format,
                        attempts: 1,
                        sent_at: now,
                    }),
                    out,
                )
            }
            Event::App(AppCommand::RejectIncoming { cause, now }) => {
                self.reject_accepted_leg(our_call, peer_call, cause, now, &mut out)
            }
            Event::Timer {
                kind: TimerKind::AcceptDecisionTimeout,
                now,
            } => {
                if awaiting_app_decision {
                    // App never answered: auto-reject the already-ACCEPTed leg.
                    self.reject_accepted_leg(
                        our_call,
                        peer_call,
                        Some("No answer".to_string()),
                        now,
                        &mut out,
                    )
                } else {
                    keep(&mut out, "decision_timeout_without_app_decision");
                    (
                        SessionState::AcceptSent(AcceptSentData {
                            peer_call,
                            our_call,
                            chosen_format,
                            attempts,
                            sent_at,
                            awaiting_app_decision,
                        }),
                        out,
                    )
                }
            }
            Event::Timer {
                kind: TimerKind::AcceptRetry,
                ..
            } => {
                if attempts >= 5 {
                    let reason = FailReason::Timeout {
                        in_state: "AcceptSent",
                    };
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    (SessionState::Failed(reason), out)
                } else {
                    out.push(Action::SendReliable(build_accept(
                        our_call,
                        peer_call,
                        chosen_format,
                    )));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::AcceptRetry, backoff));
                    (
                        SessionState::AcceptSent(AcceptSentData {
                            peer_call,
                            our_call,
                            chosen_format,
                            attempts: attempts + 1,
                            sent_at,
                            awaiting_app_decision,
                        }),
                        out,
                    )
                }
            }
            Event::Frame { frame, now } => {
                if let Some((subclass, _src, ies_bytes)) = full_subclass(&frame) {
                    match subclass {
                        Subclass::Control(crate::subclass::ControlSubclass::Hangup)
                        | Subclass::Iax(IaxCommand::Hangup) => {
                            out.push(Action::CancelTimer(TimerKind::AcceptDecisionTimeout));
                            out.push(Action::CancelTimer(TimerKind::AcceptRetry));
                            self.peer_hangup_during_setup(
                                our_call, peer_call, &ies_bytes, now, &mut out,
                            )
                        }
                        Subclass::Iax(IaxCommand::Ack) => {
                            // Stray full-frame ACK of our ACCEPT: cancel the
                            // retransmit, stay awaiting the app's decision.
                            out.push(Action::CancelTimer(TimerKind::AcceptRetry));
                            (
                                SessionState::AcceptSent(AcceptSentData {
                                    peer_call,
                                    our_call,
                                    chosen_format,
                                    attempts,
                                    sent_at,
                                    awaiting_app_decision,
                                }),
                                out,
                            )
                        }
                        _ => {
                            keep(&mut out, "unexpected_frame_in_accept_sent");
                            (
                                SessionState::AcceptSent(AcceptSentData {
                                    peer_call,
                                    our_call,
                                    chosen_format,
                                    attempts,
                                    sent_at,
                                    awaiting_app_decision,
                                }),
                                out,
                            )
                        }
                    }
                } else {
                    keep(&mut out, "unexpected_frame_in_accept_sent");
                    (
                        SessionState::AcceptSent(AcceptSentData {
                            peer_call,
                            our_call,
                            chosen_format,
                            attempts,
                            sent_at,
                            awaiting_app_decision,
                        }),
                        out,
                    )
                }
            }
            event => {
                let state = SessionState::AcceptSent(AcceptSentData {
                    peer_call,
                    our_call,
                    chosen_format,
                    attempts,
                    sent_at,
                    awaiting_app_decision,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }

    /// Tear down an already-ACCEPTed inbound leg via HANGUP (`RejectIncoming` /
    /// decision-timeout). Cancels the ACCEPT timers, sends HANGUP with the
    /// cause, arms `HangupRetry`, surfaces `Disconnected { Rejected }`, →
    /// `Hangup { Local }`.
    fn reject_accepted_leg(
        &mut self,
        our_call: CallNo,
        peer_call: CallNo,
        cause: Option<String>,
        now: Instant,
        out: &mut SmallVec<[Action; 4]>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        out.push(Action::CancelTimer(TimerKind::AcceptDecisionTimeout));
        out.push(Action::CancelTimer(TimerKind::AcceptRetry));
        out.push(Action::SendReliable(build_hangup(
            our_call,
            peer_call,
            cause.as_deref(),
        )));
        out.push(Action::SetTimer(
            TimerKind::HangupRetry,
            Duration::from_secs(1),
        ));
        out.push(Action::AppEvent(AppEvent::Disconnected {
            reason: FailReason::Rejected { cause },
        }));
        (
            SessionState::Hangup(HangupData {
                our_call,
                peer_call,
                initiated_by: HangupOrigin::Local,
                sent_at: now,
                attempts: 1,
            }),
            std::mem::take(out),
        )
    }

    pub(super) fn on_answer_sent(
        &mut self,
        s: AnswerSentData,
        event: Event<'_>,
    ) -> (SessionState, SmallVec<[Action; 4]>) {
        let AnswerSentData {
            peer_call,
            our_call,
            chosen_format,
            attempts,
            sent_at,
        } = s;
        let mut out: SmallVec<[Action; 4]> = SmallVec::new();
        match event {
            // Decision §2: the bare ACK confirming our ANSWER is consumed by the
            // runtime's Reliability, so the FSM never sees it as a Frame. The
            // runtime fires AnswerAcked when the in-flight ANSWER's oseqno is
            // released; that is the earliest correct trigger to go Active
            // (transitioning on *send* would forfeit ANSWER retransmits).
            Event::App(AppCommand::AnswerAcked { now }) => {
                out.push(Action::CancelTimer(TimerKind::AnswerRetry));
                out.push(Action::AppEvent(AppEvent::Connected { peer_call }));
                let keepalive = KeepaliveState::new(KeepaliveConfig::default(), now);
                out.push(Action::SetTimer(
                    TimerKind::Keepalive,
                    keepalive.config().ping_interval,
                ));
                (
                    SessionState::Active(ActiveData {
                        our_call,
                        peer_call,
                        established_at: now,
                        pending_dtmf: None,
                        last_dtmf_at: None,
                        keepalive,
                        last_full_voice: None,
                        last_ptt: None,
                        last_rx_voice_format: None, // iax-a422: no voice rx yet
                        negotiated_format: chosen_format,
                    }),
                    out,
                )
            }
            Event::Timer {
                kind: TimerKind::AnswerRetry,
                ..
            } => {
                if attempts >= 5 {
                    let reason = FailReason::Timeout {
                        in_state: "AnswerSent",
                    };
                    out.push(Action::AppEvent(AppEvent::Disconnected {
                        reason: reason.clone(),
                    }));
                    (SessionState::Failed(reason), out)
                } else {
                    out.push(Action::SendReliable(build_answer(our_call, peer_call)));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(Action::SetTimer(TimerKind::AnswerRetry, backoff));
                    (
                        SessionState::AnswerSent(AnswerSentData {
                            peer_call,
                            our_call,
                            chosen_format,
                            attempts: attempts + 1,
                            sent_at,
                        }),
                        out,
                    )
                }
            }
            Event::Frame { frame, now } => {
                if let Some((subclass, _src, ies_bytes)) = full_subclass(&frame)
                    && matches!(
                        subclass,
                        Subclass::Control(crate::subclass::ControlSubclass::Hangup)
                            | Subclass::Iax(IaxCommand::Hangup)
                    )
                {
                    out.push(Action::CancelTimer(TimerKind::AnswerRetry));
                    return self
                        .peer_hangup_during_setup(our_call, peer_call, &ies_bytes, now, &mut out);
                }
                let state = SessionState::AnswerSent(AnswerSentData {
                    peer_call,
                    our_call,
                    chosen_format,
                    attempts,
                    sent_at,
                });
                out.push(Action::LogInvalid {
                    reason: "unexpected_frame_in_answer_sent",
                });
                (state, out)
            }
            event => {
                let state = SessionState::AnswerSent(AnswerSentData {
                    peer_call,
                    our_call,
                    chosen_format,
                    attempts,
                    sent_at,
                });
                out.push(Action::LogInvalid {
                    reason: invalid_reason(&state, &event),
                });
                (state, out)
            }
        }
    }
}

#[cfg(test)]
mod inbound_handler_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::session::auth::{AuthMethods, Credentials, Secret};
    use crate::session::call_no::CallNo;
    use crate::session::{AppCommand, InboundPolicy};
    use crate::subclass::IaxCommand;

    fn creds() -> Credentials {
        Credentials {
            username: "rob".into(),
            password: Arc::new(Secret::new("hunter2".into())),
            allowed_methods: AuthMethods::MD5,
        }
    }

    fn offer(token: Option<&[u8]>) -> IncomingOffer {
        IncomingOffer {
            called_number: Some("s".into()),
            calling_number: Some("1001".into()),
            calling_name: Some("Rob".into()),
            username: Some("rob".into()),
            offered_codecs: [VoiceFormat::G711U, VoiceFormat::G711A]
                .into_iter()
                .collect(),
            preferred_codec: Some(VoiceFormat::G711U),
            language: None,
            peer_calltoken: token.map(<[u8]>::to_vec),
        }
    }

    fn inbound(policy: InboundPolicy, off: IncomingOffer) -> (Fsm, Instant) {
        let now = Instant::now();
        let mut f = Fsm::for_inbound(
            creds(),
            CallNo::new(16379).unwrap(),
            CallNo::new(13885).unwrap(),
            off,
            now,
        )
        .with_inbound_policy(policy);
        f.seed_entropy([0xAB; 16], "c0ffeebabec0ffeebabec0ffeebabe00".into());
        (f, now)
    }

    #[test]
    fn new_received_no_auth_no_token_transitions_to_accept_sent() {
        let (mut f, now) = inbound(InboundPolicy::default(), offer(Some(b"")));
        let actions = f.handle(Event::App(AppCommand::DriveInbound { now }));
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::IncomingCall { .. })))
        );
        assert!(actions.iter().any(|a| matches!(a, Action::SetPeerCall(_))));
        assert!(matches!(f.state(), SessionState::AcceptSent(_)));
    }

    #[test]
    fn new_received_calltoken_policy_emits_calltoken_and_transitions() {
        let pol = InboundPolicy {
            calltoken_required: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b""))); // empty token => challenge
        let actions = f.handle(Event::App(AppCommand::DriveInbound { now }));
        let ct = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::CallToken)) =>
                {
                    Some(fr)
                }
                _ => None,
            })
            .expect("CALLTOKEN issued");
        let ies = Ies::parse(&ct.ie_bytes).unwrap();
        assert_eq!(
            ies.calltoken,
            Some(&[0xAB; 16][..]),
            "seeded token bytes echoed"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SetTimer(TimerKind::InboundTokenExpiry, _)))
        );
        assert!(matches!(f.state(), SessionState::CallTokenIssued(_)));
    }

    #[test]
    fn new_received_authreq_policy_emits_authreq_with_md5_challenge() {
        let pol = InboundPolicy {
            auth_required: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let actions = f.handle(Event::App(AppCommand::DriveInbound { now }));
        let req = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::AuthReq)) =>
                {
                    Some(fr)
                }
                _ => None,
            })
            .expect("AUTHREQ issued");
        let ies = Ies::parse(&req.ie_bytes).unwrap();
        assert_eq!(ies.authmethods, Some(2), "MD5 only");
        assert_eq!(ies.challenge, Some("c0ffeebabec0ffeebabec0ffeebabe00"));
        assert!(matches!(f.state(), SessionState::AuthReqSent(_)));
    }

    #[test]
    fn new_received_auto_answer_goes_straight_to_answer_sent() {
        let pol = InboundPolicy {
            auto_answer: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let actions = f.handle(Event::App(AppCommand::DriveInbound { now }));
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))));
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if fr.frame_type == crate::subclass::FrameType::Control)));
        assert!(matches!(f.state(), SessionState::AnswerSent(_)));
    }

    #[test]
    fn new_received_rejects_non_drive_event() {
        let (mut f, now) = inbound(InboundPolicy::default(), offer(Some(b"")));
        let actions = f.handle(Event::Timer {
            kind: TimerKind::AcceptRetry,
            now,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::LogInvalid { .. }))
        );
        assert!(matches!(f.state(), SessionState::NewReceived(_)));
    }

    #[test]
    fn choose_codec_prefers_peer_format_when_supported() {
        use crate::session::CodecPolicy;

        let both: CodecMask = [VoiceFormat::G711U, VoiceFormat::G711A]
            .into_iter()
            .collect();
        // Peer prefers A and both support it -> A.
        assert_eq!(
            choose_codec(both, Some(VoiceFormat::G711A), CodecPolicy::UlawOnly),
            VoiceFormat::G711A
        );
        // Peer prefers an unsupported codec -> our preference (offered).
        assert_eq!(
            choose_codec(both, None, CodecPolicy::UlawOnly),
            VoiceFormat::G711U
        );
        // Peer only offers A; our pref is U -> first common (A).
        assert_eq!(
            choose_codec(
                CodecMask::from_u32(VoiceFormat::G711A.as_u32()),
                None,
                CodecPolicy::UlawOnly
            ),
            VoiceFormat::G711A
        );
        // Peer offered nothing (no CAPABILITY) -> our pref.
        assert_eq!(
            choose_codec(CodecMask::EMPTY, None, CodecPolicy::UlawOnly),
            VoiceFormat::G711U
        );
    }

    #[test]
    fn choose_codec_negotiation_matrix() {
        use crate::session::CodecPolicy;
        use VoiceFormat::{G711A, G711U, Slin, Slin16};
        let slin_peer: CodecMask = [Slin, G711U, G711A].into_iter().collect();
        let ulaw_peer: CodecMask = [G711U, G711A].into_iter().collect();
        let wide_peer: CodecMask = [Slin16, Slin, G711U].into_iter().collect();

        // PreferSlin picks slin from a slin-capable peer even if peer prefers ulaw.
        assert_eq!(choose_codec(slin_peer, None, CodecPolicy::PreferSlin), Slin);
        // Peer's explicit preference wins when we allow it.
        assert_eq!(
            choose_codec(slin_peer, Some(Slin), CodecPolicy::AllowSlin),
            Slin
        );
        // AllowSlin without peer insistence stays on ulaw.
        assert_eq!(choose_codec(slin_peer, None, CodecPolicy::AllowSlin), G711U);
        // UlawOnly never yields slin, even when the peer prefers it.
        assert_eq!(
            choose_codec(slin_peer, Some(Slin), CodecPolicy::UlawOnly),
            G711U
        );
        // Mixed-capability fallback: PreferSlin against a ulaw-only peer.
        assert_eq!(
            choose_codec(ulaw_peer, None, CodecPolicy::PreferSlin),
            G711U
        );
        // Empty CAPABILITY (peer sent only FORMAT): honor peer pref if we can.
        assert_eq!(
            choose_codec(CodecMask::EMPTY, Some(Slin), CodecPolicy::AllowSlin),
            Slin
        );
        // PreferSlin16 picks slin16 from a wideband-capable peer.
        assert_eq!(
            choose_codec(wide_peer, None, CodecPolicy::PreferSlin16),
            Slin16
        );
        // Wideband policy against a narrowband peer falls back down the order.
        assert_eq!(
            choose_codec(ulaw_peer, None, CodecPolicy::PreferSlin16),
            G711U
        );

        // iax-d0cc: a Prefer* node ASSERTS its preference over a capable
        // caller's stated FORMAT. A slin16-capable caller that PREFERS ulaw
        // must still be pulled up to slin16 by a PreferSlin16 node — otherwise
        // the node yields to ulaw and the audio is needlessly narrowband/8-bit
        // (astar's echo bug). AllowSlin/UlawOnly stay deferential (asserted
        // above).
        assert_eq!(
            choose_codec(wide_peer, Some(G711U), CodecPolicy::PreferSlin16),
            Slin16,
            "PreferSlin16 must override a capable caller's ulaw preference"
        );
        assert_eq!(
            choose_codec(slin_peer, Some(G711U), CodecPolicy::PreferSlin),
            Slin,
            "PreferSlin must override a capable caller's ulaw preference"
        );
    }

    // --- shared frame helpers for Tasks 8-11 ------------------------------
    use crate::frame::{Frame, FullFrame};
    use crate::subclass::FrameType;
    use std::time::Duration;

    /// Build a borrowed peer→callee full frame on the leg (peer scallno 13885,
    /// our scallno 16379).
    fn peer_full(subclass: Subclass, frame_type: FrameType, ies: Ies<'static>) -> Frame<'static> {
        Frame::Full(Box::new(FullFrame {
            source_call: 13885,
            dest_call: 16379,
            retransmission: false,
            timestamp: 0,
            oseqno: 0,
            iseqno: 0,
            frame_type,
            subclass,
            ies,
            payload: &[],
        }))
    }

    fn new_frame_with_token(token: &'static [u8]) -> Frame<'static> {
        let ies = Ies {
            called_number: Some("s"),
            version: Some(2),
            capability: Some(VoiceFormat::G711U.as_u32()),
            calltoken: Some(token),
            ..Ies::empty()
        };
        peer_full(Subclass::Iax(IaxCommand::New), FrameType::Iax, ies)
    }

    fn drive_to_calltoken_issued() -> (Fsm, Instant) {
        let pol = InboundPolicy {
            calltoken_required: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let _ = f.handle(Event::App(AppCommand::DriveInbound { now }));
        (f, now)
    }

    #[test]
    fn calltoken_issued_matching_token_resets_and_accepts() {
        let (mut f, now) = drive_to_calltoken_issued();
        // seeded token is [0xAB;16].
        let frame = new_frame_with_token(&[0xAB; 16]);
        let actions = f.handle(Event::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::ResetReliability))
        );
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))));
        assert!(matches!(f.state(), SessionState::AcceptSent(_)));
    }

    #[test]
    fn calltoken_issued_mismatched_token_emits_reject() {
        let (mut f, now) = drive_to_calltoken_issued();
        let frame = new_frame_with_token(b"WRONGWRONGWRONG!");
        let actions = f.handle(Event::Frame { frame, now });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Reject)))));
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Rejected { .. })
        ));
    }

    #[test]
    fn calltoken_issued_token_expiry_fails_silently() {
        let (mut f, now) = drive_to_calltoken_issued();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::InboundTokenExpiry,
            now: now + Duration::from_secs(10),
        });
        assert!(
            !actions.iter().any(|a| matches!(a, Action::SendReliable(_))),
            "no REJECT on expiry"
        );
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "CallTokenIssued"
            })
        ));
    }

    #[test]
    fn calltoken_issued_matching_token_with_auth_emits_authreq() {
        // calltoken + auth both required: after the token validates, AUTHREQ.
        let pol = InboundPolicy {
            calltoken_required: true,
            auth_required: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let _ = f.handle(Event::App(AppCommand::DriveInbound { now }));
        let frame = new_frame_with_token(&[0xAB; 16]);
        let actions = f.handle(Event::Frame { frame, now });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::AuthReq)))));
        assert!(matches!(f.state(), SessionState::AuthReqSent(_)));
    }

    // --- Task 9: on_auth_req_sent ----------------------------------------

    fn drive_to_auth_req_sent() -> (Fsm, Instant) {
        let pol = InboundPolicy {
            auth_required: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let _ = f.handle(Event::App(AppCommand::DriveInbound { now }));
        (f, now)
    }

    /// The seeded challenge ("c0ffee…00") with the test password "hunter2".
    fn authrep_frame(md5_hex: &'static str) -> Frame<'static> {
        let ies = Ies {
            md5_result: Some(md5_hex),
            ..Ies::empty()
        };
        peer_full(Subclass::Iax(IaxCommand::AuthRep), FrameType::Iax, ies)
    }

    #[test]
    fn authreq_sent_valid_md5_response_transitions_to_accept_sent_and_emits_incoming_call_event() {
        let (mut f, now) = drive_to_auth_req_sent();
        let good =
            crate::session::auth::md5_response("c0ffeebabec0ffeebabec0ffeebabe00", "hunter2");
        let good: &'static str = Box::leak(good.into_boxed_str());
        let actions = f.handle(Event::Frame {
            frame: authrep_frame(good),
            now,
        });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::IncomingCall { .. })))
        );
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))));
        assert!(matches!(f.state(), SessionState::AcceptSent(_)));
    }

    #[test]
    fn authreq_sent_invalid_md5_response_emits_reject_with_cause() {
        let (mut f, now) = drive_to_auth_req_sent();
        let actions = f.handle(Event::Frame {
            frame: authrep_frame("00000000000000000000000000000000"),
            now,
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Reject)))));
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Rejected { .. })
        ));
    }

    #[test]
    fn authreq_sent_retry_budget_exhausted_fails_with_timeout() {
        let (mut f, now) = drive_to_auth_req_sent();
        for i in 1..=5 {
            f.handle(Event::Timer {
                kind: TimerKind::AuthReqRetry,
                now: now + Duration::from_secs(i),
            });
        }
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "AuthReqSent"
            })
        ));
    }

    #[test]
    fn authreq_sent_retry_resends_authreq() {
        let (mut f, now) = drive_to_auth_req_sent();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::AuthReqRetry,
            now: now + Duration::from_secs(1),
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::AuthReq)))));
        assert!(matches!(
            f.state(),
            SessionState::AuthReqSent(AuthReqSentData { attempts: 2, .. })
        ));
    }

    #[test]
    fn authreq_sent_peer_hangup_transitions_to_hangup_peer() {
        let (mut f, now) = drive_to_auth_req_sent();
        let hangup = peer_full(
            Subclass::Iax(IaxCommand::Hangup),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame { frame: hangup, now });
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::AppEvent(AppEvent::Disconnected {
                reason: FailReason::RemoteHangup { .. }
            })
        )));
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Peer,
                ..
            })
        ));
    }

    // --- Task 10: on_accept_sent -----------------------------------------

    fn drive_to_accept_sent() -> (Fsm, Instant) {
        let (mut f, now) = inbound(InboundPolicy::default(), offer(Some(b"")));
        let _ = f.handle(Event::App(AppCommand::DriveInbound { now }));
        (f, now) // now in AcceptSent (AppDecide)
    }

    #[test]
    fn accept_sent_app_answer_emits_answer_frame_and_transitions_to_answer_sent() {
        let (mut f, now) = drive_to_accept_sent();
        let actions = f.handle(Event::App(AppCommand::AnswerIncoming {
            now: now + Duration::from_secs(1),
        }));
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if fr.frame_type == FrameType::Control
            && fr.subclass == Subclass::Control(crate::subclass::ControlSubclass::Answer))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SetTimer(TimerKind::AnswerRetry, _)))
        );
        assert!(matches!(f.state(), SessionState::AnswerSent(_)));
    }

    #[test]
    fn accept_sent_app_reject_emits_hangup_with_cause() {
        let (mut f, now) = drive_to_accept_sent();
        let actions = f.handle(Event::App(AppCommand::RejectIncoming {
            cause: Some("busy".to_string()),
            now,
        }));
        let hangup = actions
            .iter()
            .find_map(|a| match a {
                Action::SendReliable(fr)
                    if matches!(fr.subclass, Subclass::Iax(IaxCommand::Hangup)) =>
                {
                    Some(fr)
                }
                _ => None,
            })
            .expect("HANGUP emitted on reject");
        let ies = Ies::parse(&hangup.ie_bytes).unwrap();
        assert_eq!(ies.cause, Some("busy"));
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Local,
                ..
            })
        ));
    }

    #[test]
    fn accept_sent_decision_timeout_auto_rejects() {
        let (mut f, now) = drive_to_accept_sent();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::AcceptDecisionTimeout,
            now: now + Duration::from_secs(30),
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Hangup)))));
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Local,
                ..
            })
        ));
    }

    #[test]
    fn accept_sent_retry_resends_accept_then_times_out() {
        let (mut f, now) = drive_to_accept_sent();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::AcceptRetry,
            now: now + Duration::from_secs(1),
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))));
        // Exhaust the budget.
        for i in 2..=5 {
            f.handle(Event::Timer {
                kind: TimerKind::AcceptRetry,
                now: now + Duration::from_secs(i),
            });
        }
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "AcceptSent"
            })
        ));
    }

    #[test]
    fn accept_sent_peer_hangup_transitions_to_hangup_peer() {
        let (mut f, now) = drive_to_accept_sent();
        let hangup = peer_full(
            Subclass::Iax(IaxCommand::Hangup),
            FrameType::Iax,
            Ies::empty(),
        );
        let actions = f.handle(Event::Frame { frame: hangup, now });
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::AppEvent(AppEvent::Disconnected {
                reason: FailReason::RemoteHangup { .. }
            })
        )));
        assert!(matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Peer,
                ..
            })
        ));
    }

    #[test]
    fn accept_sent_stray_ack_stays_in_accept_sent() {
        let (mut f, now) = drive_to_accept_sent();
        let ack = peer_full(Subclass::Iax(IaxCommand::Ack), FrameType::Iax, Ies::empty());
        let actions = f.handle(Event::Frame { frame: ack, now });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::CancelTimer(TimerKind::AcceptRetry)))
        );
        assert!(matches!(f.state(), SessionState::AcceptSent(_)));
    }

    // --- Task 11: on_answer_sent -----------------------------------------

    fn drive_to_answer_sent() -> (Fsm, Instant) {
        let pol = InboundPolicy {
            auto_answer: true,
            ..InboundPolicy::default()
        };
        let (mut f, now) = inbound(pol, offer(Some(b"")));
        let _ = f.handle(Event::App(AppCommand::DriveInbound { now }));
        (f, now)
    }

    #[test]
    fn answer_sent_ack_transitions_to_active() {
        let (mut f, now) = drive_to_answer_sent();
        let actions = f.handle(Event::App(AppCommand::AnswerAcked {
            now: now + Duration::from_millis(5),
        }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. })))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::SetTimer(TimerKind::Keepalive, _)))
        );
        assert!(matches!(f.state(), SessionState::Active(_)));
    }

    #[test]
    fn answer_sent_retry_resends_answer() {
        let (mut f, now) = drive_to_answer_sent();
        let actions = f.handle(Event::Timer {
            kind: TimerKind::AnswerRetry,
            now: now + Duration::from_secs(1),
        });
        assert!(actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if fr.frame_type == FrameType::Control
            && fr.subclass == Subclass::Control(crate::subclass::ControlSubclass::Answer))));
        assert!(matches!(
            f.state(),
            SessionState::AnswerSent(AnswerSentData { attempts: 2, .. })
        ));
    }

    #[test]
    fn answer_sent_retry_budget_exhausted_fails_with_timeout() {
        let (mut f, now) = drive_to_answer_sent();
        for i in 1..=5 {
            f.handle(Event::Timer {
                kind: TimerKind::AnswerRetry,
                now: now + Duration::from_secs(i),
            });
        }
        assert!(matches!(
            f.state(),
            SessionState::Failed(FailReason::Timeout {
                in_state: "AnswerSent"
            })
        ));
    }

    #[test]
    fn answer_sent_peer_hangup_transitions_to_hangup_peer() {
        let (mut f, now) = drive_to_answer_sent();
        let hangup = peer_full(
            Subclass::Iax(IaxCommand::Hangup),
            FrameType::Iax,
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
}
