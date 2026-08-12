// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure registration FSM. No I/O, no time, no threads — drive with `handle()`.
//!
//! Structural twin of [`super::fsm`]. Same `Event`/`Action` discipline,
//! same per-state field layout, same `LogInvalid` catch-all on the bottom.
//! See `docs/superpowers/specs/2026-06-05-iax-bc14-registration-design.md`.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::frame::{Frame, OwnedFullFrame};
use crate::session::auth::{AuthMethods, Credentials};
use crate::session::call_no::CallNo;
use crate::session::fsm::{CallToken, TimerKind};

/// Reason a registration FSM transitioned to `Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegFailReason {
    /// Registrar sent REGREJ with optional CAUSE / CAUSECODE.
    Rejected {
        cause: Option<String>,
        code: Option<u8>,
    },
    /// Retry budget exhausted in the named state.
    Timeout { in_state: &'static str },
    /// REGAUTH advertised an auth method we do not support (RSA-only today).
    UnsupportedAuthMethod { advertised: AuthMethods },
    /// REGAUTH advertised plaintext only, and `RegisterOptions::allow_plaintext` is false.
    PlaintextDeclined { advertised: AuthMethods },
    /// Surfaced by the runtime when the UDP layer fails.
    NetworkError(std::io::ErrorKind),
    /// Caller dropped the `Registration` before `Registered`.
    Aborted,
}

/// Per-call options for a registration round.
#[derive(Debug, Clone)]
pub struct RegisterOptions {
    /// Refresh interval requested in the REGREQ REFRESH IE. Default 60s.
    /// The server-returned REFRESH in the REGACK overrides this.
    pub refresh_request: Duration,
    /// Permit PLAINTEXT auth if the registrar advertises only PLAINTEXT.
    pub allow_plaintext: bool,
    /// Maximum REGREQ / REGAUTH / REGREL retries before giving up. Default 5.
    pub max_retries: u8,
}

impl Default for RegisterOptions {
    fn default() -> Self {
        Self {
            refresh_request: Duration::from_secs(60),
            allow_plaintext: false,
            max_retries: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegState {
    Idle,
    RegReqSent {
        sent_at: Instant,
        our_call: CallNo,
        attempts: u8,
        refresh_request: Duration,
    },
    RegReqResent {
        sent_at: Instant,
        our_call: CallNo,
        token: CallToken,
        attempts: u8,
        refresh_request: Duration,
    },
    RegAuthRecv {
        challenge: Vec<u8>,
        methods: AuthMethods,
        our_call: CallNo,
        peer_call: CallNo,
    },
    RegPending {
        sent_at: Instant,
        our_call: CallNo,
        peer_call: CallNo,
        attempts: u8,
        challenge: Vec<u8>,
    },
    Registered {
        our_call: CallNo,
        peer_call: CallNo,
        refresh: Duration,
        apparent_addr: Option<SocketAddr>,
        registered_at: Instant,
    },
    RegRelSent {
        our_call: CallNo,
        peer_call: CallNo,
        sent_at: Instant,
        attempts: u8,
    },
    Closed,
    Failed(RegFailReason),
}

impl RegState {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::RegReqSent { .. } => "RegReqSent",
            Self::RegReqResent { .. } => "RegReqResent",
            Self::RegAuthRecv { .. } => "RegAuthRecv",
            Self::RegPending { .. } => "RegPending",
            Self::Registered { .. } => "Registered",
            Self::RegRelSent { .. } => "RegRelSent",
            Self::Closed => "Closed",
            Self::Failed(_) => "Failed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum RegAppCommand {
    /// Begin a registration: emit REGREQ with empty CALLTOKEN.
    StartRegister { now: Instant },
    /// Tear down a live registration with REGREL. Silent no-op on Failed/Closed/Idle.
    Deregister { now: Instant },
}

#[derive(Debug, Clone)]
pub enum RegAppEvent {
    /// Reached `RegReqSent` after `StartRegister`.
    Registering,
    /// Reached `Registered`.
    Registered {
        refresh: Duration,
        apparent_addr: Option<SocketAddr>,
    },
    /// Refresh round started (an in-flight refresh REGREQ was emitted).
    Refreshing,
    /// Refresh round completed successfully.
    Refreshed,
    /// Reached `Failed(_)`.
    Failed(RegFailReason),
    /// Reached `Closed` after REGREL.
    Released,
}

#[derive(Debug)]
pub enum RegEvent<'a> {
    App(RegAppCommand),
    Frame {
        frame: Frame<'a>,
        now: Instant,
    },
    Timer {
        kind: TimerKind,
        now: Instant,
        jitter_salt: u32,
    },
    DeliveryFailed {
        oseqno: u8,
    },
}

#[derive(Debug)]
pub enum RegAction {
    SendReliable(OwnedFullFrame),
    /// Signal to the runtime that the registrar's chosen `source_call` is now
    /// known. The runtime must call `Reliability::set_peer_call(_)` before
    /// enqueueing the next reliable frame so `OSeqno` / ACK / `dest_call`
    /// bookkeeping addresses the right registrar call. Emitted ahead of any
    /// `SendReliable` produced by the same transition.
    ///
    /// Mirrors [`super::fsm::Action::SetPeerCall`] on the call path. Crucially
    /// it is **not** emitted on the CALLTOKEN round: the resent REGREQ keeps
    /// `dest_call = 0` (the CALLTOKEN's `source_call` is a throwaway anti-spoof
    /// scallno; the registrar picks its real scallno for the REGAUTH that
    /// follows). The peer call is learned from REGAUTH / no-auth REGACK only.
    SetPeerCall(CallNo),
    /// Reset the runtime's `Reliability` for a brand-new transaction (fresh
    /// seqnos, no peer call → `dest_call = 0`). Emitted by the refresh round
    /// (iax-177d): the registrar destroys its side of the call after REGACK,
    /// so the refresh REGREQ must open a fresh transaction and re-run the full
    /// handshake. Always emitted BEFORE the `SendReliable` it applies to.
    ResetReliability,
    SetTimer(TimerKind, Duration),
    CancelTimer(TimerKind),
    AppEvent(RegAppEvent),
    LogInvalid {
        reason: &'static str,
    },
}

pub struct RegFsm {
    state: RegState,
    credentials: Credentials,
    our_call: CallNo,
    options: RegisterOptions,
}

impl RegFsm {
    #[must_use]
    pub fn new(credentials: Credentials, our_call: CallNo, options: RegisterOptions) -> Self {
        Self {
            state: RegState::Idle,
            credentials,
            our_call,
            options,
        }
    }

    #[must_use]
    pub fn state(&self) -> &RegState {
        &self.state
    }

    /// Test-only constructor. Not part of the stable API.
    #[doc(hidden)]
    #[must_use]
    pub fn with_state(
        state: RegState,
        credentials: Credentials,
        our_call: CallNo,
        options: RegisterOptions,
    ) -> Self {
        Self {
            state,
            credentials,
            our_call,
            options,
        }
    }

    /// Drive the FSM. Default behavior: unknown (state, event) pairs leave
    /// state unchanged and emit a single `LogInvalid` action. Subsequent
    /// tasks layer real transitions over this default.
    #[allow(clippy::too_many_lines, clippy::single_match_else)]
    pub fn handle(&mut self, event: RegEvent<'_>) -> SmallVec<[RegAction; 4]> {
        let mut out: SmallVec<[RegAction; 4]> = SmallVec::new();
        match (std::mem::replace(&mut self.state, RegState::Idle), event) {
            (RegState::Idle, RegEvent::App(RegAppCommand::StartRegister { now })) => {
                let refresh = self.options.refresh_request;
                #[allow(clippy::cast_possible_truncation)]
                let refresh_secs = refresh.as_secs().min(u64::from(u16::MAX)) as u16;
                let regreq = build_regreq(
                    self.our_call,
                    &self.credentials.username,
                    refresh_secs,
                    None,
                    None,
                    None,
                );
                out.push(RegAction::SendReliable(regreq));
                out.push(RegAction::SetTimer(
                    TimerKind::RegReqRetry,
                    Duration::from_secs(1),
                ));
                out.push(RegAction::AppEvent(RegAppEvent::Registering));
                self.state = RegState::RegReqSent {
                    sent_at: now,
                    our_call: self.our_call,
                    attempts: 1,
                    refresh_request: refresh,
                };
            }
            (
                RegState::RegReqSent {
                    our_call,
                    refresh_request,
                    sent_at,
                    attempts,
                },
                RegEvent::Frame { frame, now },
            ) => match full_subclass(&frame) {
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::RegAck),
                    peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    let refresh = ies
                        .refresh
                        .map_or(refresh_request, |s| Duration::from_secs(u64::from(s)));
                    let apparent_addr = parse_apparent_addr(ies.apparent_addr);
                    let peer_call = CallNo::new(peer).unwrap_or(our_call);
                    out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                    // No-auth registrar: learn its scallno so a retransmitted
                    // REGACK gets ACKed (and any future refresh addresses it).
                    out.push(RegAction::SetPeerCall(peer_call));
                    let next = jittered_refresh(refresh, 0);
                    out.push(RegAction::SetTimer(TimerKind::RegRefresh, next));
                    out.push(RegAction::AppEvent(RegAppEvent::Registered {
                        refresh,
                        apparent_addr,
                    }));
                    self.state = RegState::Registered {
                        our_call,
                        peer_call,
                        refresh,
                        apparent_addr,
                        registered_at: now,
                    };
                }
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::RegRej),
                    _peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    let reason = RegFailReason::Rejected {
                        cause: ies.cause.map(str::to_string),
                        code: ies.causecode,
                    };
                    out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                    out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                    self.state = RegState::Failed(reason);
                }
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::RegAuth),
                    peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    let methods = AuthMethods::from_bits_truncate(ies.authmethods.unwrap_or(0));
                    let challenge = ies.challenge.unwrap_or("").to_string();
                    let peer_call = CallNo::new(peer).unwrap_or(our_call);

                    if methods.contains(AuthMethods::MD5) {
                        out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                        // Plumb the registrar's scallno to Reliability BEFORE the
                        // post-auth REGREQ so it is addressed (dest_call) to the
                        // registrar's call. Without this the REGREQ goes out with
                        // dest_call=0 and Asterisk/ASL silently drops it → timeout.
                        out.push(RegAction::SetPeerCall(peer_call));
                        let response = crate::session::auth::md5_response(
                            &challenge,
                            self.credentials.password.expose(),
                        );
                        #[allow(clippy::cast_possible_truncation)]
                        let refresh_secs =
                            refresh_request.as_secs().min(u64::from(u16::MAX)) as u16;
                        let regreq = build_regreq(
                            our_call,
                            &self.credentials.username,
                            refresh_secs,
                            None,
                            Some(&response),
                            None,
                        );
                        out.push(RegAction::SendReliable(regreq));
                        out.push(RegAction::SetTimer(
                            TimerKind::RegAuthRetry,
                            Duration::from_secs(1),
                        ));
                        self.state = RegState::RegPending {
                            sent_at: now,
                            our_call,
                            peer_call,
                            attempts: 1,
                            challenge: challenge.into_bytes(),
                        };
                    } else if methods.contains(AuthMethods::PLAINTEXT) {
                        if self.options.allow_plaintext {
                            out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                            out.push(RegAction::SetPeerCall(peer_call));
                            #[allow(clippy::cast_possible_truncation)]
                            let refresh_secs =
                                refresh_request.as_secs().min(u64::from(u16::MAX)) as u16;
                            let regreq = build_regreq(
                                our_call,
                                &self.credentials.username,
                                refresh_secs,
                                None,
                                None,
                                Some(self.credentials.password.expose()),
                            );
                            out.push(RegAction::SendReliable(regreq));
                            out.push(RegAction::SetTimer(
                                TimerKind::RegAuthRetry,
                                Duration::from_secs(1),
                            ));
                            self.state = RegState::RegPending {
                                sent_at: now,
                                our_call,
                                peer_call,
                                attempts: 1,
                                challenge: challenge.into_bytes(),
                            };
                        } else {
                            let reason = RegFailReason::PlaintextDeclined {
                                advertised: methods,
                            };
                            out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                            out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                            self.state = RegState::Failed(reason);
                        }
                    } else {
                        let reason = RegFailReason::UnsupportedAuthMethod {
                            advertised: methods,
                        };
                        out.push(RegAction::CancelTimer(TimerKind::RegReqRetry));
                        out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                        self.state = RegState::Failed(reason);
                    }
                }
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::CallToken),
                    _peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    // Wire CALLTOKEN already fits a u8 length prefix; cannot fail.
                    let token = CallToken::new(ies.calltoken.unwrap_or(&[]))
                        .expect("wire CALLTOKEN IE <= 255 bytes");
                    #[allow(clippy::cast_possible_truncation)]
                    let refresh_secs = refresh_request.as_secs().min(u64::from(u16::MAX)) as u16;
                    let regreq = build_regreq(
                        our_call,
                        &self.credentials.username,
                        refresh_secs,
                        Some(token.as_bytes()),
                        None,
                        None,
                    );
                    out.push(RegAction::SendReliable(regreq));
                    out.push(RegAction::SetTimer(
                        TimerKind::RegTokenExpiry,
                        Duration::from_secs(10),
                    ));
                    out.push(RegAction::SetTimer(
                        TimerKind::RegReqRetry,
                        Duration::from_secs(1),
                    ));
                    self.state = RegState::RegReqResent {
                        sent_at: now,
                        our_call,
                        token,
                        attempts: 1,
                        refresh_request,
                    };
                }
                _ => {
                    self.state = RegState::RegReqSent {
                        sent_at,
                        our_call,
                        attempts,
                        refresh_request,
                    };
                    out.push(RegAction::LogInvalid {
                        reason: "unexpected_frame_in_regreq_sent",
                    });
                }
            },
            (
                RegState::RegPending {
                    our_call,
                    peer_call,
                    challenge,
                    sent_at,
                    attempts,
                },
                RegEvent::Frame { frame, now },
            ) => match full_subclass(&frame) {
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::RegAck),
                    _peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    let refresh = ies.refresh.map_or(self.options.refresh_request, |s| {
                        Duration::from_secs(u64::from(s))
                    });
                    let apparent_addr = parse_apparent_addr(ies.apparent_addr);
                    out.push(RegAction::CancelTimer(TimerKind::RegAuthRetry));
                    // Defence-in-depth: re-assert the peer call (already set on
                    // the REGAUTH round) so a retransmitted REGACK is ACKed.
                    out.push(RegAction::SetPeerCall(peer_call));
                    let next = jittered_refresh(refresh, 0);
                    out.push(RegAction::SetTimer(TimerKind::RegRefresh, next));
                    out.push(RegAction::AppEvent(RegAppEvent::Registered {
                        refresh,
                        apparent_addr,
                    }));
                    self.state = RegState::Registered {
                        our_call,
                        peer_call,
                        refresh,
                        apparent_addr,
                        registered_at: now,
                    };
                    let _ = challenge;
                }
                Some((
                    crate::frame::Subclass::Iax(crate::subclass::IaxCommand::RegRej),
                    _peer,
                    ies_bytes,
                )) => {
                    let ies = crate::ie::Ies::parse(&ies_bytes)
                        .unwrap_or_else(|_| crate::ie::Ies::empty());
                    let reason = RegFailReason::Rejected {
                        cause: ies.cause.map(str::to_string),
                        code: ies.causecode,
                    };
                    out.push(RegAction::CancelTimer(TimerKind::RegAuthRetry));
                    out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                    self.state = RegState::Failed(reason);
                }
                _ => {
                    self.state = RegState::RegPending {
                        our_call,
                        peer_call,
                        challenge,
                        sent_at,
                        attempts,
                    };
                    out.push(RegAction::LogInvalid {
                        reason: "unexpected_frame_in_regpending",
                    });
                }
            },
            (
                RegState::Registered {
                    our_call,
                    peer_call,
                    refresh,
                    apparent_addr,
                    registered_at,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegRefresh,
                    now,
                    jitter_salt: _,
                },
            ) => {
                // iax-177d: the registrar destroyed its side of the original
                // transaction after REGACK — a refresh is a BRAND-NEW
                // registration transaction. Reset the runtime's Reliability
                // (fresh seqnos, dest_call=0) and re-run the full REGREQ
                // handshake; the registrar re-challenges and `RegReqSent`
                // handles it exactly like the initial round. Reusing the old
                // peer call / seqnos made Asterisk drop the refresh REGREQ and
                // the registration died at its first refresh.
                out.push(RegAction::ResetReliability);
                #[allow(clippy::cast_possible_truncation)]
                let refresh_secs = refresh.as_secs().min(u64::from(u16::MAX)) as u16;
                let regreq = build_regreq(
                    our_call,
                    &self.credentials.username,
                    refresh_secs,
                    None,
                    None,
                    None,
                );
                out.push(RegAction::SendReliable(regreq));
                out.push(RegAction::SetTimer(
                    TimerKind::RegReqRetry,
                    Duration::from_secs(1),
                ));
                out.push(RegAction::AppEvent(RegAppEvent::Refreshing));
                self.state = RegState::RegReqSent {
                    sent_at: now,
                    our_call,
                    attempts: 1,
                    refresh_request: refresh,
                };
                let _ = (peer_call, apparent_addr, registered_at);
            }
            (
                RegState::Registered {
                    our_call,
                    peer_call,
                    ..
                },
                RegEvent::App(RegAppCommand::Deregister { now }),
            ) => {
                let regrel = build_regrel(our_call, peer_call, &self.credentials.username);
                out.push(RegAction::SendReliable(regrel));
                out.push(RegAction::CancelTimer(TimerKind::RegRefresh));
                out.push(RegAction::SetTimer(
                    TimerKind::RegRelRetry,
                    Duration::from_secs(1),
                ));
                self.state = RegState::RegRelSent {
                    our_call,
                    peer_call,
                    sent_at: now,
                    attempts: 1,
                };
            }
            (
                RegState::RegRelSent {
                    our_call,
                    peer_call,
                    sent_at,
                    attempts,
                },
                RegEvent::Frame { frame, .. },
            ) => match full_subclass(&frame) {
                Some((
                    crate::frame::Subclass::Iax(
                        crate::subclass::IaxCommand::Ack | crate::subclass::IaxCommand::RegAck,
                    ),
                    _,
                    _,
                )) => {
                    out.push(RegAction::CancelTimer(TimerKind::RegRelRetry));
                    out.push(RegAction::AppEvent(RegAppEvent::Released));
                    self.state = RegState::Closed;
                }
                _ => {
                    self.state = RegState::RegRelSent {
                        our_call,
                        peer_call,
                        sent_at,
                        attempts,
                    };
                    out.push(RegAction::LogInvalid {
                        reason: "unexpected_frame_in_regrel_sent",
                    });
                }
            },
            (
                RegState::RegReqSent {
                    our_call,
                    attempts,
                    refresh_request,
                    sent_at,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegReqRetry,
                    ..
                },
            ) => {
                if attempts >= self.options.max_retries {
                    let reason = RegFailReason::Timeout {
                        in_state: "RegReqSent",
                    };
                    out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                    self.state = RegState::Failed(reason);
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    let refresh_secs = refresh_request.as_secs().min(u64::from(u16::MAX)) as u16;
                    let regreq = build_regreq(
                        our_call,
                        &self.credentials.username,
                        refresh_secs,
                        None,
                        None,
                        None,
                    );
                    out.push(RegAction::SendReliable(regreq));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(RegAction::SetTimer(TimerKind::RegReqRetry, backoff));
                    self.state = RegState::RegReqSent {
                        sent_at,
                        our_call,
                        attempts: attempts + 1,
                        refresh_request,
                    };
                }
            }
            (
                RegState::RegReqResent {
                    our_call,
                    token,
                    attempts,
                    refresh_request,
                    sent_at,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegTokenExpiry,
                    ..
                },
            ) => {
                let reason = RegFailReason::Timeout {
                    in_state: "RegReqResent",
                };
                out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                self.state = RegState::Failed(reason);
                let _ = (our_call, token, attempts, refresh_request, sent_at);
            }
            (
                RegState::RegReqResent {
                    our_call,
                    token,
                    attempts,
                    refresh_request,
                    sent_at,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegReqRetry,
                    ..
                },
            ) => {
                if attempts >= self.options.max_retries {
                    let reason = RegFailReason::Timeout {
                        in_state: "RegReqResent",
                    };
                    out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                    self.state = RegState::Failed(reason);
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    let refresh_secs = refresh_request.as_secs().min(u64::from(u16::MAX)) as u16;
                    let regreq = build_regreq(
                        our_call,
                        &self.credentials.username,
                        refresh_secs,
                        Some(token.as_bytes()),
                        None,
                        None,
                    );
                    out.push(RegAction::SendReliable(regreq));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(RegAction::SetTimer(TimerKind::RegReqRetry, backoff));
                    self.state = RegState::RegReqResent {
                        sent_at,
                        our_call,
                        token,
                        attempts: attempts + 1,
                        refresh_request,
                    };
                }
            }
            (
                RegState::RegPending {
                    our_call,
                    peer_call,
                    challenge,
                    attempts,
                    sent_at,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegAuthRetry,
                    ..
                },
            ) => {
                if attempts >= self.options.max_retries {
                    let reason = RegFailReason::Timeout {
                        in_state: "RegPending",
                    };
                    out.push(RegAction::AppEvent(RegAppEvent::Failed(reason.clone())));
                    self.state = RegState::Failed(reason);
                } else {
                    let challenge_str = std::str::from_utf8(&challenge).unwrap_or("");
                    let response = crate::session::auth::md5_response(
                        challenge_str,
                        self.credentials.password.expose(),
                    );
                    #[allow(clippy::cast_possible_truncation)]
                    let refresh_secs = self
                        .options
                        .refresh_request
                        .as_secs()
                        .min(u64::from(u16::MAX)) as u16;
                    let regreq = build_regreq(
                        our_call,
                        &self.credentials.username,
                        refresh_secs,
                        None,
                        Some(&response),
                        None,
                    );
                    out.push(RegAction::SendReliable(regreq));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(3)));
                    out.push(RegAction::SetTimer(TimerKind::RegAuthRetry, backoff));
                    self.state = RegState::RegPending {
                        our_call,
                        peer_call,
                        challenge,
                        attempts: attempts + 1,
                        sent_at,
                    };
                }
            }
            (
                RegState::RegRelSent {
                    our_call,
                    peer_call,
                    sent_at,
                    attempts,
                },
                RegEvent::Timer {
                    kind: TimerKind::RegRelRetry,
                    ..
                },
            ) => {
                if attempts >= 3 {
                    out.push(RegAction::AppEvent(RegAppEvent::Released));
                    self.state = RegState::Closed;
                } else {
                    let regrel = build_regrel(our_call, peer_call, &self.credentials.username);
                    out.push(RegAction::SendReliable(regrel));
                    let backoff = Duration::from_secs(1 << u32::from(attempts.min(2)));
                    out.push(RegAction::SetTimer(TimerKind::RegRelRetry, backoff));
                    self.state = RegState::RegRelSent {
                        our_call,
                        peer_call,
                        sent_at,
                        attempts: attempts + 1,
                    };
                }
            }
            // Decision §4: Deregister on Failed/Closed/Idle is a silent no-op.
            (state @ RegState::Failed(_), RegEvent::App(RegAppCommand::Deregister { .. }))
            | (state @ RegState::Closed, RegEvent::App(RegAppCommand::Deregister { .. }))
            | (state @ RegState::Idle, RegEvent::App(RegAppCommand::Deregister { .. })) => {
                self.state = state;
            }
            (state, _event) => {
                out.push(RegAction::LogInvalid {
                    reason: "invalid_transition",
                });
                self.state = state;
            }
        }
        out
    }
}

/// Pure jitter math. Spec §"Jitter strategy":
/// - refresh >= 30s: subtract a salt-derived offset in [0, min(refresh/8, 5s)).
/// - refresh  < 30s: refresh at max(refresh - 2s, refresh * 3/4), still jittered.
#[must_use]
pub fn jittered_refresh(refresh: Duration, salt: u32) -> Duration {
    if refresh >= Duration::from_secs(30) {
        let max_jitter = (refresh / 8).min(Duration::from_secs(5));
        let max_nanos = u64::try_from(max_jitter.as_nanos().max(1)).unwrap_or(1);
        let jitter = Duration::from_nanos(u64::from(salt) % max_nanos);
        refresh.saturating_sub(jitter)
    } else {
        let early = refresh.saturating_sub(Duration::from_secs(2));
        let three_quarters = refresh * 3 / 4;
        let base = early.max(three_quarters);
        let max_jitter = (refresh / 8).min(Duration::from_secs(1));
        let max_nanos = u64::try_from(max_jitter.as_nanos().max(1)).unwrap_or(1);
        let jitter = Duration::from_nanos(u64::from(salt) % max_nanos);
        base.saturating_sub(jitter)
    }
}

fn parse_apparent_addr(ie: Option<&[u8]>) -> Option<SocketAddr> {
    // RFC 5456: APPARENT_ADDR is a `sockaddr_in` (16 bytes) on Asterisk.
    // We do a minimal IPv4 parse; failure returns None.
    let bytes = ie?;
    if bytes.len() < 8 {
        return None;
    }
    // sin_family(2) + sin_port(2) + sin_addr(4) — Asterisk stuffs raw struct.
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    let ip = std::net::Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]);
    Some(SocketAddr::new(std::net::IpAddr::V4(ip), port))
}

fn full_subclass(frame: &Frame<'_>) -> Option<(crate::frame::Subclass, u16, Vec<u8>)> {
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

fn build_regreq(
    our_call: CallNo,
    username: &str,
    refresh_secs: u16,
    token: Option<&[u8]>,
    md5_result: Option<&str>,
    password: Option<&str>,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    let ies = Ies {
        username: Some(username),
        refresh: Some(refresh_secs),
        calltoken: Some(token.unwrap_or(&[])),
        md5_result,
        password,
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        0,
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::RegReq),
        &ies,
    )
    .expect("REGREQ IEs fit within wire limits")
}

fn build_regrel(our_call: CallNo, peer_call: CallNo, username: &str) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    let ies = Ies {
        username: Some(username),
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::RegRel),
        &ies,
    )
    .expect("REGREL IEs fit within wire limits")
}

#[cfg(test)]
mod reg_tests {
    use super::*;
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::session::auth::{AuthMethods, Secret};
    use crate::subclass::IaxCommand;

    fn creds() -> Credentials {
        Credentials {
            username: "u".into(),
            password: std::sync::Arc::new(Secret::new("p".into())),
            allowed_methods: AuthMethods::MD5,
        }
    }

    fn fsm() -> RegFsm {
        RegFsm::new(creds(), CallNo::new(1).unwrap(), RegisterOptions::default())
    }

    #[test]
    fn new_starts_in_idle() {
        let f = fsm();
        assert!(matches!(f.state(), RegState::Idle));
    }

    #[test]
    fn idle_start_register_emits_regreq_with_empty_calltoken_and_timer() {
        let mut f = fsm();
        let actions = f.handle(RegEvent::App(RegAppCommand::StartRegister {
            now: Instant::now(),
        }));
        let mut saw_regreq = false;
        let mut saw_timer = false;
        for a in &actions {
            match a {
                RegAction::SendReliable(frame) => {
                    assert!(matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq)));
                    let ies = Ies::parse(frame.ie_bytes()).expect("parse ies");
                    assert_eq!(ies.username, Some("u"));
                    assert_eq!(ies.refresh, Some(60));
                    assert_eq!(ies.calltoken, Some(&[][..]), "empty CALLTOKEN IE present");
                    saw_regreq = true;
                }
                RegAction::SetTimer(TimerKind::RegReqRetry, d) => {
                    assert_eq!(*d, Duration::from_secs(1));
                    saw_timer = true;
                }
                _ => {}
            }
        }
        assert!(saw_regreq && saw_timer);
        assert!(matches!(
            f.state(),
            RegState::RegReqSent { attempts: 1, .. }
        ));
    }

    use crate::frame::{Frame as F, FullFrame};
    use crate::subclass::FrameType;

    fn peer_frame(oseqno: u8, iseqno: u8, subclass: Subclass, ies: Ies<'static>) -> Frame<'static> {
        F::Full(Box::new(FullFrame {
            source_call: 7,
            dest_call: 1,
            retransmission: false,
            timestamp: 0,
            oseqno,
            iseqno,
            frame_type: FrameType::Iax,
            subclass,
            ies,
            payload: &[],
        }))
    }

    fn drive_to_regreqsent() -> (RegFsm, Instant) {
        let mut f = fsm();
        let now = Instant::now();
        let _ = f.handle(RegEvent::App(RegAppCommand::StartRegister { now }));
        (f, now)
    }

    #[test]
    fn regreqsent_calltoken_transitions_to_regreqresent_with_token() {
        let (mut f, now) = drive_to_regreqsent();
        let token = b"opaque-token";
        let ies = Ies {
            calltoken: Some(token),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::CallToken), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });
        let mut saw_regreq = false;
        for a in &actions {
            if let RegAction::SendReliable(frame) = a
                && matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                let ies = Ies::parse(frame.ie_bytes()).unwrap();
                assert_eq!(ies.calltoken, Some(&token[..]));
                saw_regreq = true;
            }
        }
        assert!(saw_regreq, "REGREQ resent with populated CALLTOKEN");
        assert!(matches!(f.state(), RegState::RegReqResent { .. }));
    }

    #[test]
    fn regreqsent_regauth_md5_emits_post_auth_regreq_and_transitions_to_regpending() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });
        let mut saw_regreq_md5 = false;
        for a in &actions {
            if let RegAction::SendReliable(frame) = a
                && matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                let ies = Ies::parse(frame.ie_bytes()).unwrap();
                assert_eq!(
                    ies.md5_result,
                    Some("f5aa7ab908f6660d2bc19d73ff0e848c"),
                    "md5(c0ffee || p) hex"
                );
                saw_regreq_md5 = true;
            }
        }
        assert!(saw_regreq_md5);
        assert!(matches!(
            f.state(),
            RegState::RegPending { attempts: 1, .. }
        ));
    }

    #[test]
    fn regreqsent_regauth_emits_set_peer_call_before_post_auth_regreq() {
        // Regression (iax-64b6 live register): without SetPeerCall the post-auth
        // REGREQ goes out with dest_call=0 and Asterisk/ASL silently drops it →
        // RegPending timeout. The peer scallno must be plumbed to Reliability
        // ahead of the REGREQ so it is addressed to the registrar's call.
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        // peer_frame uses source_call: 7.
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });
        let set_peer_idx = actions
            .iter()
            .position(|a| matches!(a, RegAction::SetPeerCall(c) if *c == CallNo::new(7).unwrap()));
        let regreq_idx = actions.iter().position(|a| {
            matches!(a, RegAction::SendReliable(fr) if matches!(fr.subclass, Subclass::Iax(IaxCommand::RegReq)))
        });
        let set_peer_idx = set_peer_idx.expect("SetPeerCall(7) emitted on REGAUTH");
        let regreq_idx = regreq_idx.expect("post-auth REGREQ emitted");
        assert!(
            set_peer_idx < regreq_idx,
            "SetPeerCall must precede the post-auth REGREQ so enqueue stamps dest_call",
        );
    }

    #[test]
    fn regreqsent_calltoken_does_not_emit_set_peer_call() {
        // The CALLTOKEN's source_call is a throwaway anti-spoof scallno; the
        // resent REGREQ must keep dest_call=0. Emitting SetPeerCall here would
        // make enqueue stamp a non-zero dest_call (the WT iax-ff7b bug).
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            calltoken: Some(b"opaque-token"),
            ..Ies::empty()
        };
        let actions = f.handle(RegEvent::Frame {
            frame: peer_frame(0, 1, Subclass::Iax(IaxCommand::CallToken), ies),
            now: now + Duration::from_millis(50),
        });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, RegAction::SetPeerCall(_))),
            "CALLTOKEN round must NOT set peer call (dest_call stays 0)",
        );
    }

    #[test]
    fn regreqsent_regauth_rsa_only_fails_with_unsupported_auth_method() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            authmethods: Some(4),
            challenge: Some("x"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::UnsupportedAuthMethod { .. })
        ));
    }

    #[test]
    fn regreqsent_regauth_plaintext_only_with_decline_fails() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            authmethods: Some(1),
            challenge: Some("x"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::PlaintextDeclined { .. })
        ));
    }

    #[test]
    fn regreqsent_regauth_plaintext_only_with_opt_in_emits_password_regreq() {
        let mut f = RegFsm::new(
            creds(),
            CallNo::new(1).unwrap(),
            RegisterOptions {
                allow_plaintext: true,
                ..RegisterOptions::default()
            },
        );
        let now = Instant::now();
        let _ = f.handle(RegEvent::App(RegAppCommand::StartRegister { now }));
        let ies = Ies {
            authmethods: Some(1),
            challenge: Some("x"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        let mut saw_pw = false;
        for a in &actions {
            if let RegAction::SendReliable(frame) = a
                && matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                let ies = Ies::parse(frame.ie_bytes()).unwrap();
                if ies.password == Some("p") {
                    saw_pw = true;
                }
            }
        }
        assert!(saw_pw, "plaintext opt-in emits PASSWORD IE");
        assert!(matches!(f.state(), RegState::RegPending { .. }));
    }

    #[test]
    fn regreqsent_regack_no_auth_transitions_to_registered_with_server_refresh() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            refresh: Some(120),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAck), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(
            f.state(),
            RegState::Registered { refresh, .. } if *refresh == Duration::from_secs(120)
        ));
        assert!(actions.iter().any(|a| matches!(
            a,
            RegAction::AppEvent(RegAppEvent::Registered { refresh, .. })
                if *refresh == Duration::from_secs(120)
        )));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RegAction::CancelTimer(TimerKind::RegReqRetry)))
        );
        assert!(actions.iter().any(|a| matches!(
            a,
            RegAction::SetTimer(TimerKind::RegRefresh, d) if *d <= Duration::from_secs(120) && *d > Duration::from_secs(60)
        )));
    }

    #[test]
    fn refresh_timer_resets_reliability_and_opens_a_fresh_transaction() {
        // Reach Registered (no-auth REGACK, refresh granted 60s).
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            refresh: Some(60),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAck), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(f.state(), RegState::Registered { .. }));

        // Fire the refresh timer: the refresh must open a BRAND-NEW
        // transaction (iax-177d) — ResetReliability BEFORE the REGREQ, then
        // back to RegReqSent so the registrar's fresh challenge is handled
        // like the initial round.
        let actions = f.handle(RegEvent::Timer {
            kind: TimerKind::RegRefresh,
            now: now + Duration::from_secs(60),
            jitter_salt: 0,
        });
        let reset_pos = actions
            .iter()
            .position(|a| matches!(a, RegAction::ResetReliability));
        let send_pos = actions
            .iter()
            .position(|a| matches!(a, RegAction::SendReliable(_)));
        assert!(reset_pos.is_some(), "refresh emits ResetReliability");
        assert!(send_pos.is_some(), "refresh emits the REGREQ");
        assert!(reset_pos < send_pos, "reset precedes the REGREQ");
        assert!(matches!(
            f.state(),
            RegState::RegReqSent { attempts: 1, .. }
        ));
    }

    #[test]
    fn regreqsent_regack_without_refresh_ie_uses_caller_request() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies::empty();
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAck), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(
            f.state(),
            RegState::Registered { refresh, .. } if *refresh == Duration::from_secs(60)
        ));
    }

    #[test]
    fn regreqsent_regrej_transitions_to_failed_with_cause() {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            cause: Some("Authentication failed"),
            causecode: Some(29),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegRej), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::Rejected { cause: Some(c), code: Some(29) })
                if c == "Authentication failed"
        ));
    }

    fn drive_to_regpending() -> (RegFsm, Instant) {
        let (mut f, now) = drive_to_regreqsent();
        let ies = Ies {
            authmethods: Some(2),
            challenge: Some("c0ffee"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAuth), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(50),
        });
        assert!(matches!(f.state(), RegState::RegPending { .. }));
        (f, now + Duration::from_millis(50))
    }

    #[test]
    fn regpending_regack_transitions_to_registered() {
        let (mut f, now) = drive_to_regpending();
        let ies = Ies {
            refresh: Some(90),
            ..Ies::empty()
        };
        let frame = peer_frame(1, 2, Subclass::Iax(IaxCommand::RegAck), ies);
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(
            f.state(),
            RegState::Registered { refresh, .. } if *refresh == Duration::from_secs(90)
        ));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RegAction::CancelTimer(TimerKind::RegAuthRetry)))
        );
    }

    #[test]
    fn regpending_regrej_transitions_to_failed() {
        let (mut f, now) = drive_to_regpending();
        let ies = Ies {
            cause: Some("Bad password"),
            ..Ies::empty()
        };
        let frame = peer_frame(1, 2, Subclass::Iax(IaxCommand::RegRej), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::Rejected { cause: Some(_), .. })
        ));
    }

    fn drive_to_registered() -> (RegFsm, Instant) {
        let (mut f, now) = drive_to_regpending();
        let ies = Ies {
            refresh: Some(60),
            ..Ies::empty()
        };
        let frame = peer_frame(1, 2, Subclass::Iax(IaxCommand::RegAck), ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20),
        });
        assert!(matches!(f.state(), RegState::Registered { .. }));
        (f, now + Duration::from_millis(20))
    }

    #[test]
    fn registered_refresh_timer_restarts_from_regreqsent() {
        let (mut f, now) = drive_to_registered();
        let actions = f.handle(RegEvent::Timer {
            kind: TimerKind::RegRefresh,
            now: now + Duration::from_secs(55),
            jitter_salt: 42,
        });
        let mut saw_regreq = false;
        let mut saw_refreshing = false;
        for a in &actions {
            match a {
                RegAction::SendReliable(frame) => {
                    if matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq)) {
                        let ies = Ies::parse(frame.ie_bytes()).unwrap();
                        assert_eq!(ies.calltoken, Some(&[][..]));
                        saw_regreq = true;
                    }
                }
                RegAction::AppEvent(RegAppEvent::Refreshing) => saw_refreshing = true,
                _ => {}
            }
        }
        assert!(saw_regreq && saw_refreshing);
        assert!(matches!(
            f.state(),
            RegState::RegReqSent { attempts: 1, .. }
        ));
    }

    #[test]
    fn registered_deregister_emits_regrel_and_sets_retry_timer() {
        let (mut f, now) = drive_to_registered();
        let actions = f.handle(RegEvent::App(RegAppCommand::Deregister {
            now: now + Duration::from_secs(1),
        }));
        let mut saw_regrel = false;
        for a in &actions {
            if let RegAction::SendReliable(frame) = a
                && matches!(frame.subclass, Subclass::Iax(IaxCommand::RegRel))
            {
                let ies = Ies::parse(frame.ie_bytes()).unwrap();
                assert_eq!(ies.username, Some("u"));
                saw_regrel = true;
            }
        }
        assert!(saw_regrel);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RegAction::SetTimer(TimerKind::RegRelRetry, _)))
        );
        assert!(matches!(
            f.state(),
            RegState::RegRelSent { attempts: 1, .. }
        ));
    }

    #[test]
    fn regrelsent_ack_transitions_to_closed() {
        let (mut f, now) = drive_to_registered();
        let _ = f.handle(RegEvent::App(RegAppCommand::Deregister { now }));
        let frame = peer_frame(2, 3, Subclass::Iax(IaxCommand::Ack), Ies::empty());
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(f.state(), RegState::Closed));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RegAction::AppEvent(RegAppEvent::Released)))
        );
    }

    #[test]
    fn regrelsent_regack_also_transitions_to_closed() {
        let (mut f, now) = drive_to_registered();
        let _ = f.handle(RegEvent::App(RegAppCommand::Deregister { now }));
        let frame = peer_frame(2, 3, Subclass::Iax(IaxCommand::RegAck), Ies::empty());
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(5),
        });
        assert!(matches!(f.state(), RegState::Closed));
    }

    #[test]
    fn deregister_on_failed_is_silent_no_op() {
        let mut f = RegFsm::with_state(
            RegState::Failed(RegFailReason::Aborted),
            creds(),
            CallNo::new(1).unwrap(),
            RegisterOptions::default(),
        );
        let actions = f.handle(RegEvent::App(RegAppCommand::Deregister {
            now: Instant::now(),
        }));
        assert!(actions.is_empty(), "Deregister on Failed must be silent");
        assert!(matches!(f.state(), RegState::Failed(_)));
    }

    #[test]
    fn regreq_retry_within_budget_resends_and_increments_attempts() {
        let (mut f, now) = drive_to_regreqsent();
        let actions = f.handle(RegEvent::Timer {
            kind: TimerKind::RegReqRetry,
            now: now + Duration::from_secs(1),
            jitter_salt: 0,
        });
        assert!(actions.iter().any(|a| matches!(
            a,
            RegAction::SendReliable(frame) if matches!(frame.subclass, Subclass::Iax(IaxCommand::RegReq))
        )));
        assert!(matches!(
            f.state(),
            RegState::RegReqSent { attempts: 2, .. }
        ));
    }

    #[test]
    fn regreq_retry_budget_exhausted_fails_with_timeout() {
        let (mut f, now) = drive_to_regreqsent();
        for i in 1..=5 {
            f.handle(RegEvent::Timer {
                kind: TimerKind::RegReqRetry,
                now: now + Duration::from_secs(i),
                jitter_salt: 0,
            });
        }
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::Timeout {
                in_state: "RegReqSent"
            })
        ));
    }

    #[test]
    fn regrel_retry_budget_exhausted_closes_anyway() {
        let (mut f, now) = drive_to_registered();
        let _ = f.handle(RegEvent::App(RegAppCommand::Deregister { now }));
        for i in 1..=3 {
            f.handle(RegEvent::Timer {
                kind: TimerKind::RegRelRetry,
                now: now + Duration::from_secs(i),
                jitter_salt: 0,
            });
        }
        assert!(matches!(f.state(), RegState::Closed));
    }

    #[test]
    fn regreqresent_token_expiry_fails() {
        let (mut f, now) = drive_to_regreqsent();
        let token_ies = Ies {
            calltoken: Some(b"tok"),
            ..Ies::empty()
        };
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::CallToken), token_ies);
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(10),
        });
        let _ = f.handle(RegEvent::Timer {
            kind: TimerKind::RegTokenExpiry,
            now: now + Duration::from_secs(10),
            jitter_salt: 0,
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::Timeout {
                in_state: "RegReqResent"
            })
        ));
    }

    #[test]
    fn failed_absorbs_further_frames_without_state_change() {
        let mut f = RegFsm::with_state(
            RegState::Failed(RegFailReason::Aborted),
            creds(),
            CallNo::new(1).unwrap(),
            RegisterOptions::default(),
        );
        let frame = peer_frame(0, 1, Subclass::Iax(IaxCommand::RegAck), Ies::empty());
        let _ = f.handle(RegEvent::Frame {
            frame,
            now: Instant::now(),
        });
        assert!(matches!(
            f.state(),
            RegState::Failed(RegFailReason::Aborted)
        ));
    }

    #[test]
    fn closed_absorbs_further_timer_events_without_state_change() {
        let mut f = RegFsm::with_state(
            RegState::Closed,
            creds(),
            CallNo::new(1).unwrap(),
            RegisterOptions::default(),
        );
        let _ = f.handle(RegEvent::Timer {
            kind: TimerKind::RegRefresh,
            now: Instant::now(),
            jitter_salt: 0,
        });
        assert!(matches!(f.state(), RegState::Closed));
    }
}
