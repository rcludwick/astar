// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Throwaway blocking-`UdpSocket` driver wrapping `Fsm` + `Reliability`.
//!
//! Exists only to feed the iax-6813 harness. When iax-612e lands a real
//! high-level `Client` API, delete this module and rewrite scenarios to
//! use it.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use smallvec::SmallVec;

use astar_iax_core::frame::{Frame, OwnedFrame, OwnedFullFrame, encode, parse_lenient};
use astar_iax_core::session::auth::Credentials;
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{Action, AppCommand, Event, Fsm, HangupData, SessionState};
use astar_iax_core::session::reliability::{Reliability, ReliabilityConfig, RxOutcome};
use astar_iax_core::subclass::IaxCommand;

pub struct Session {
    pub(crate) sock: UdpSocket,
    pub(crate) fsm: Fsm,
    pub(crate) rel: Reliability,
    pub(crate) peer: SocketAddr,
    pub(crate) creds: Credentials,
}

#[derive(Debug)]
pub enum DriverError {
    Timeout {
        last_state: String,
        last_event: Option<String>,
    },
    Io(io::Error),
    FsmRejected {
        state: String,
        reason: &'static str,
    },
}

impl From<io::Error> for DriverError {
    fn from(e: io::Error) -> Self {
        DriverError::Io(e)
    }
}

impl Session {
    /// Bind a fresh UDP socket on a free loopback port and prepare an
    /// `Fsm` + `Reliability` pair targeting `peer`.
    pub fn connect(peer: SocketAddr, creds: Credentials) -> io::Result<Self> {
        let sock = UdpSocket::bind("127.0.0.1:0")?;
        sock.set_read_timeout(Some(Duration::from_millis(50)))?;
        let our_call = CallNo::new(1).expect("CallNo::new(1) is valid");
        let fsm = Fsm::new(creds.clone(), our_call);
        let rel = Reliability::new(our_call, ReliabilityConfig::default());
        Ok(Self {
            sock,
            fsm,
            rel,
            peer,
            creds,
        })
    }

    #[must_use]
    pub fn state(&self) -> &SessionState {
        self.fsm.state()
    }

    #[must_use]
    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    /// Local bound address — handy for fake-server tests that need to
    /// know where to send replies.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub fn send_app_command(&mut self, cmd: AppCommand) {
        let actions = self.fsm.handle(Event::App(cmd));
        self.dispatch_actions(actions);
    }

    /// Drive the loop until `done(&Fsm)` returns true or `deadline` elapses.
    pub fn run_event_loop_until<P>(
        &mut self,
        done: P,
        deadline: Duration,
    ) -> Result<(), DriverError>
    where
        P: Fn(&Fsm) -> bool,
    {
        let start = Instant::now();
        let mut buf = [0u8; 4096];

        loop {
            if done(&self.fsm) {
                return Ok(());
            }
            if start.elapsed() >= deadline {
                return Err(DriverError::Timeout {
                    last_state: format!("{:?}", self.fsm.state()),
                    last_event: None,
                });
            }

            // Drain everything pending in the socket buffer in one iteration so
            // an in-burst ACCEPT doesn't get delayed behind retransmits and a
            // 50ms recv timeout.
            loop {
                match self.sock.recv_from(&mut buf) {
                    Ok((n, _src)) => {
                        let now = Instant::now();
                        let bytes = buf[..n].to_vec();
                        let Ok(frame) = parse_lenient(&bytes) else {
                            continue;
                        };
                        // Capture frame metadata for the Hangup→Closed shortcut
                        // below: Reliability consumes ACKs, but the FSM needs to
                        // see Event::Frame(ACK) to close cleanly.
                        let frame_was_ack_in_hangup = match (&frame, self.fsm.state()) {
                            (
                                astar_iax_core::frame::Frame::Full(ff),
                                SessionState::Hangup(HangupData { .. }),
                            ) => matches!(
                                ff.subclass,
                                astar_iax_core::Subclass::Iax(IaxCommand::Ack)
                            ),
                            _ => false,
                        };
                        match self.rel.on_frame_in(frame, now) {
                            RxOutcome::Deliver { frame, send_ack } => {
                                if let Some(ack_bytes) = send_ack {
                                    let _ = self.sock.send_to(&ack_bytes, self.peer);
                                }
                                let pre = self.fsm.state().name();
                                let actions = self.fsm.handle(Event::Frame { frame, now });
                                let post = self.fsm.state().name();
                                // RFC 5456 §8.6 / iax-c333 follow-up: the CALLTOKEN
                                // handshake replaces the initial NEW; sequence numbers
                                // restart from 0. The FSM signals this by moving from
                                // NewSent to NewResent in response to CALLTOKEN. Reset
                                // before dispatching so the resent NEW gets oseqno=0.
                                if pre == "NewSent" && post == "NewResent" {
                                    self.rel.reset();
                                }
                                self.dispatch_actions(actions);
                            }
                            RxOutcome::Consumed => {
                                // Hangup→Closed shortcut: re-parse the bytes and
                                // hand the ACK to the FSM so it closes the call.
                                // Reliability has already cleared in_flight.
                                if frame_was_ack_in_hangup
                                    && let Ok(reparsed) = parse_lenient(&bytes)
                                {
                                    let actions = self.fsm.handle(Event::Frame {
                                        frame: reparsed,
                                        now,
                                    });
                                    self.dispatch_actions(actions);
                                }
                            }
                            RxOutcome::Duplicate { resend_ack } => {
                                if let Some(b) = resend_ack {
                                    let _ = self.sock.send_to(&b, self.peer);
                                }
                            }
                            RxOutcome::Vnak(iseqno) => {
                                // RFC 5456 §6.9.3: the peer wants a resend from
                                // `iseqno`. iax-3b9d (mirroring iax-a307):
                                // answer it instead of mapping to
                                // DeliveryFailed, which killed the call.
                                for bytes in self.rel.resend_from(iseqno) {
                                    let _ = self.sock.send_to(&bytes, self.peer);
                                }
                            }
                            RxOutcome::GaveUp { oseqno } => {
                                let actions = self.fsm.handle(Event::DeliveryFailed { oseqno });
                                self.dispatch_actions(actions);
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(e) => return Err(DriverError::Io(e)),
                }
            }

            // Drive retransmits.
            let tick = self.rel.tick(Instant::now());
            for bytes in tick.retransmit {
                let _ = self.sock.send_to(&bytes, self.peer);
            }
            for oseqno in tick.gave_up {
                let actions = self.fsm.handle(Event::DeliveryFailed { oseqno });
                self.dispatch_actions(actions);
            }
        }
    }

    /// Encode + send a `Frame` directly over the socket, bypassing
    /// FSM/Reliability. Used by scenarios for sub-flows the FSM does not
    /// model (registration, today).
    pub fn send_raw_frame(&self, frame: &Frame<'_>) -> io::Result<()> {
        let mut out = Vec::with_capacity(128);
        // Scenario-supplied test frames are expected to have well-formed
        // IEs (≤ 255 bytes each); panic loudly if not so the scenario
        // author sees the bug rather than silently emitting unparseable
        // bytes (iax-545b).
        encode(frame, &mut out).expect("test scenario frame IEs must be ≤ 255 bytes");
        self.sock.send_to(&out, self.peer).map(|_| ())
    }

    /// Block until a single full frame arrives or `deadline` elapses.
    /// Auto-ACKs every received full frame (oseqno >= 0) so the peer
    /// stops retransmitting. Returns the first full frame received.
    pub fn recv_one_frame(&self, deadline: Duration) -> Result<OwnedFullFrame, DriverError> {
        let start = Instant::now();
        let mut buf = [0u8; 4096];
        loop {
            if start.elapsed() >= deadline {
                return Err(DriverError::Timeout {
                    last_state: format!("{:?}", self.fsm.state()),
                    last_event: Some("recv_one_frame".into()),
                });
            }
            match self.sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let frame = parse_lenient(&buf[..n]).map_err(|_| DriverError::FsmRejected {
                        state: format!("{:?}", self.fsm.state()),
                        reason: "parse failed",
                    })?;
                    if let OwnedFrame::Full(ff) = frame.into_owned() {
                        // Auto-ACK so the peer stops retransmitting. M8
                        // (iax-f755): the ACK shape lives in core's
                        // `encode_ack`. We mirror call numbers (their source →
                        // our dest) and use their OSeqno+1 as our ISeqno.
                        let ack = astar_iax_core::session::encode_ack(
                            ff.dest_call,
                            ff.source_call,
                            ff.timestamp,
                            ff.iseqno,
                            ff.oseqno.wrapping_add(1),
                        );
                        let _ = self.sock.send_to(&ack, self.peer);
                        return Ok(ff);
                    }
                    // Mini frames don't need ACKing.
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(DriverError::Io(e)),
            }
        }
    }

    fn dispatch_actions(&mut self, actions: SmallVec<[Action; 4]>) {
        let now = Instant::now();
        for action in actions {
            match action {
                Action::SendReliable(frame) => {
                    let bytes = self.rel.enqueue(frame, now);
                    let _ = self.sock.send_to(&bytes, self.peer);
                }
                Action::SendUnreliable(bytes) => {
                    let _ = self.sock.send_to(&bytes, self.peer);
                }
                Action::SetPeerCall(peer) => {
                    // iax-e402: FSM learned the peer's chosen scallno; plumb
                    // it down so Reliability stamps it into dest_call on
                    // subsequent enqueues and ACKs (RFC 5456 §8.6.1).
                    self.rel.set_peer_call(peer);
                }
                // iax-8baf: inbound CALLTOKEN resend resets the reliability ladder.
                Action::ResetReliability => {
                    self.rel.reset();
                }
                Action::SetTimer(_, _)
                | Action::CancelTimer(_)
                | Action::AppEvent(_)
                | Action::LogInvalid { .. } => {
                    // Timers fire from `tick()`; AppEvents and logs are
                    // ignored by the harness driver.
                }
            }
        }
    }
}
