// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! One `EchoLink` connection to one station: the SDES/BYE handshake, the
//! keepalive and time-out clocks, and the far end's talk state. Sans-IO.

use std::time::Instant;

use crate::Timings;
use crate::rtcp::{self, Rtcp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Five SDES went unanswered.
    NoAnswer,
    /// No SDES for `connection_timeout` while connected.
    TimedOut,
    ByeReceived,
    Hangup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsoState {
    Connecting,
    Connected,
    Closed(CloseReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QsoEvent {
    Connected { callsign: String },
    Closed(CloseReason),
    TalkStart,
    TalkStop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QsoAction {
    /// A datagram for the peer's control port.
    SendControl(Vec<u8>),
    Event(QsoEvent),
}

#[derive(Debug)]
pub struct Qso {
    timings: Timings,
    own_callsign: String,
    state: QsoState,
    sdes_sent: u32,
    last_sdes_sent: Instant,
    last_sdes_received: Instant,
    last_audio: Option<Instant>,
    talking: bool,
}

impl Qso {
    /// Begin connecting as `callsign`; the first SDES goes out at once.
    #[must_use]
    pub fn start(callsign: &str, timings: Timings, now: Instant) -> (Qso, Vec<QsoAction>) {
        let qso = Qso {
            timings,
            own_callsign: callsign.to_owned(),
            state: QsoState::Connecting,
            sdes_sent: 1,
            last_sdes_sent: now,
            last_sdes_received: now,
            last_audio: None,
            talking: false,
        };
        let first = vec![qso.sdes_action()];
        (qso, first)
    }

    #[must_use]
    pub fn state(&self) -> &QsoState {
        &self.state
    }

    /// Whether the far end is talking.
    #[must_use]
    pub fn talking(&self) -> bool {
        self.talking
    }

    fn sdes_action(&self) -> QsoAction {
        QsoAction::SendControl(rtcp::sdes(&self.own_callsign, ""))
    }

    fn close(&mut self, reason: CloseReason, send_bye: bool) -> Vec<QsoAction> {
        let mut out = Vec::new();
        if send_bye {
            out.push(QsoAction::SendControl(rtcp::bye().to_vec()));
        }
        if self.talking {
            self.talking = false;
            out.push(QsoAction::Event(QsoEvent::TalkStop));
        }
        self.state = QsoState::Closed(reason);
        out.push(QsoAction::Event(QsoEvent::Closed(reason)));
        out
    }

    /// A datagram arrived on the control port.
    pub fn on_control(&mut self, bytes: &[u8], now: Instant) -> Vec<QsoAction> {
        if matches!(self.state, QsoState::Closed(_)) {
            return Vec::new();
        }
        match rtcp::parse(bytes) {
            Ok(Rtcp::Sdes { callsign, .. }) => {
                self.last_sdes_received = now;
                if self.state == QsoState::Connecting {
                    self.state = QsoState::Connected;
                    return vec![QsoAction::Event(QsoEvent::Connected { callsign })];
                }
                Vec::new()
            }
            Ok(Rtcp::Bye) => self.close(CloseReason::ByeReceived, false),
            Ok(Rtcp::Other) | Err(_) => Vec::new(),
        }
    }

    /// An audio datagram arrived from the peer.
    pub fn on_audio(&mut self, now: Instant) -> Vec<QsoAction> {
        if self.state != QsoState::Connected {
            return Vec::new();
        }
        self.last_audio = Some(now);
        if self.talking {
            return Vec::new();
        }
        self.talking = true;
        vec![QsoAction::Event(QsoEvent::TalkStart)]
    }

    /// Advance the clocks; call at least as often as the shortest interval.
    pub fn tick(&mut self, now: Instant) -> Vec<QsoAction> {
        let mut out = Vec::new();
        match self.state {
            QsoState::Closed(_) => return out,
            QsoState::Connecting => {
                if now.saturating_duration_since(self.last_sdes_sent) >= self.timings.keepalive {
                    if self.sdes_sent >= self.timings.give_up_after {
                        return self.close(CloseReason::NoAnswer, true);
                    }
                    self.sdes_sent += 1;
                    self.last_sdes_sent = now;
                    out.push(self.sdes_action());
                }
            }
            QsoState::Connected => {
                if now.saturating_duration_since(self.last_sdes_received)
                    >= self.timings.connection_timeout
                {
                    return self.close(CloseReason::TimedOut, true);
                }
                if now.saturating_duration_since(self.last_sdes_sent) >= self.timings.keepalive {
                    self.last_sdes_sent = now;
                    out.push(self.sdes_action());
                }
            }
        }
        if self.talking
            && self
                .last_audio
                .is_some_and(|t| now.saturating_duration_since(t) >= self.timings.talk_hold)
        {
            self.talking = false;
            out.push(QsoAction::Event(QsoEvent::TalkStop));
        }
        out
    }

    /// End the contact from our side.
    pub fn hang_up(&mut self) -> Vec<QsoAction> {
        if matches!(self.state, QsoState::Closed(_)) {
            return Vec::new();
        }
        self.close(CloseReason::Hangup, true)
    }
}
