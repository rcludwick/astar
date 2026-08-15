// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure client session state machine for an M17 reflector link.
//!
//! This module does no I/O: [`SessionFsm`] takes time explicitly ([`Instant`]
//! arguments) and returns bytes to send or events to act on via
//! [`FsmAction`]. The caller owns the socket and the clock.

use std::time::{Duration, Instant};

use crate::control::ControlPacket;
use crate::frame::StreamPacket;

/// How long the client waits without hearing from the reflector before
/// declaring the link dead.
const LINK_TIMEOUT: Duration = Duration::from_secs(30);

/// How often [`SessionFsm::tick`] resends `CONN` while [`LinkState::Connecting`]
/// (iax-f2b8-fix Fix 3): one lost UDP packet (CONN or ACKN) previously meant a
/// silent 30 s wait for the keepalive timeout before the link ever declared
/// itself `Failed`. Bounded by `keepalive_timeout` either way — a resend never
/// postpones the eventual `Failed` declaration, it just gives the reflector
/// more chances to hear the CONN (or the client more chances to hear the
/// ACKN/NACK) inside that same window.
const CONN_RESEND_INTERVAL: Duration = Duration::from_secs(1);

/// Current state of the reflector link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected; no CONN has been sent.
    Idle,
    /// CONN sent, waiting for ACKN/NACK.
    Connecting,
    /// ACKN received; the link is up.
    Linked,
    /// Rejected, disconnected, or timed out.
    Failed,
}

/// What the caller should do in response to [`SessionFsm::on_packet`] or
/// [`SessionFsm::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsmAction {
    /// Nothing to do.
    None,
    /// Send these bytes (e.g. a PONG reply) to the reflector.
    Send(Vec<u8>),
    /// A voice stream packet was received while linked; hand it to the codec.
    Voice(StreamPacket),
    /// The link has gone down (NACK, DISC, or timeout); tear down the session.
    Unlinked,
}

/// Pure client-side session state machine for one reflector link.
///
/// Holds no clock and does no I/O: `now` is passed in by the caller for every
/// time-sensitive operation, which makes this fully unit-testable without
/// sleeping.
#[derive(Debug, Clone)]
pub struct SessionFsm {
    callsign: [u8; 6],
    module: u8,
    state: LinkState,
    last_rx: Option<Instant>,
    /// How long to wait without hearing from the reflector before declaring
    /// the link dead. [`SessionFsm::new`] uses [`LINK_TIMEOUT`] (30 s); a
    /// caller that needs a shorter window (e.g. a test) uses
    /// [`SessionFsm::with_keepalive_timeout`] instead.
    keepalive_timeout: Duration,
    /// The CONN bytes last sent (from [`SessionFsm::connect`] or a resend),
    /// and when. `None` once no longer [`LinkState::Connecting`] (there is
    /// nothing left to resend). See [`CONN_RESEND_INTERVAL`] / Fix 3.
    pending_conn: Option<(Vec<u8>, Instant)>,
}

impl SessionFsm {
    /// Creates a new, unconnected session for `callsign` on reflector
    /// `module`, using the default 30 s keepalive timeout.
    #[must_use]
    pub fn new(callsign: [u8; 6], module: u8) -> Self {
        Self::with_keepalive_timeout(callsign, module, LINK_TIMEOUT)
    }

    /// Like [`SessionFsm::new`], but with a caller-supplied keepalive
    /// timeout instead of the default 30 s (iax-f2b8 Task 3: lets a host
    /// thread `M17Session` — or a test — surface/shorten the silence-timeout
    /// window without changing the wire protocol). [`SessionFsm::new`]'s
    /// default path stays byte-identical to before this constructor existed.
    #[must_use]
    pub fn with_keepalive_timeout(
        callsign: [u8; 6],
        module: u8,
        keepalive_timeout: Duration,
    ) -> Self {
        Self {
            callsign,
            module,
            state: LinkState::Idle,
            last_rx: None,
            keepalive_timeout,
            pending_conn: None,
        }
    }

    /// Current [`LinkState`].
    #[must_use]
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Begins connecting: moves to [`LinkState::Connecting`] and returns the
    /// CONN packet bytes to send.
    #[must_use]
    pub fn connect(&mut self, now: Instant) -> Vec<u8> {
        self.state = LinkState::Connecting;
        self.last_rx = Some(now);
        let bytes = ControlPacket::Conn {
            callsign: self.callsign,
            module: self.module,
        }
        .to_bytes();
        self.pending_conn = Some((bytes.clone(), now));
        bytes
    }

    /// Feeds a received packet into the session, advancing the state machine
    /// and returning the resulting [`FsmAction`].
    #[must_use]
    pub fn on_packet(&mut self, buf: &[u8], now: Instant) -> FsmAction {
        self.last_rx = Some(now);

        if self.state == LinkState::Linked
            && let Some(packet) = StreamPacket::parse(buf)
        {
            return FsmAction::Voice(packet);
        }

        match ControlPacket::parse(buf) {
            Some(ControlPacket::Ping { .. }) => FsmAction::Send(
                ControlPacket::Pong {
                    callsign: self.callsign,
                }
                .to_bytes(),
            ),
            Some(ControlPacket::Ackn) => {
                self.state = LinkState::Linked;
                self.pending_conn = None;
                FsmAction::None
            }
            Some(ControlPacket::Nack | ControlPacket::Disc { .. }) => {
                self.state = LinkState::Failed;
                self.pending_conn = None;
                FsmAction::Unlinked
            }
            _ => FsmAction::None,
        }
    }

    /// Advances time to `now`, declaring the link [`LinkState::Failed`] if
    /// the configured keepalive timeout ([`LINK_TIMEOUT`] by default; see
    /// [`SessionFsm::with_keepalive_timeout`]) has elapsed with no received
    /// packet since the last [`SessionFsm::connect`] or
    /// [`SessionFsm::on_packet`] call.
    #[must_use]
    pub fn tick(&mut self, now: Instant) -> FsmAction {
        let Some(last) = self.last_rx else {
            return FsmAction::None;
        };
        if self.state != LinkState::Failed && now.duration_since(last) >= self.keepalive_timeout {
            self.state = LinkState::Failed;
            self.pending_conn = None;
            return FsmAction::Unlinked;
        }
        // Fix 3: while still waiting on ACKN/NACK, resend CONN roughly every
        // CONN_RESEND_INTERVAL rather than staying silent for the whole
        // keepalive window on one lost packet. Bounded by the timeout check
        // above, which always runs first and wins.
        if self.state == LinkState::Connecting
            && let Some((bytes, sent_at)) = self.pending_conn.clone()
            && now.duration_since(sent_at) >= CONN_RESEND_INTERVAL
        {
            self.pending_conn = Some((bytes.clone(), now));
            return FsmAction::Send(bytes);
        }
        FsmAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{BROADCAST, encode_callsign};
    use crate::frame::Lsf;
    use std::time::Duration;

    fn cs() -> [u8; 6] {
        encode_callsign("N0CALL").unwrap()
    }

    #[test]
    fn connect_then_ackn_reaches_linked() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let bytes = fsm.connect(now);
        assert_eq!(fsm.state(), LinkState::Connecting);
        assert_eq!(
            ControlPacket::parse(&bytes).unwrap(),
            ControlPacket::Conn {
                callsign: cs(),
                module: b'A',
            }
        );

        let action = fsm.on_packet(b"ACKN", now);
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn ping_is_answered_with_pong() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let _ = fsm.connect(now);
        let _ = fsm.on_packet(b"ACKN", now);

        let mut ping = b"PING".to_vec();
        ping.extend_from_slice(&BROADCAST); // reflector callsign, contents irrelevant here
        let action = fsm.on_packet(&ping, now);
        assert_eq!(
            action,
            FsmAction::Send(ControlPacket::Pong { callsign: cs() }.to_bytes())
        );
    }

    #[test]
    fn thirty_seconds_of_silence_fails_the_link() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let _ = fsm.connect(now);
        let _ = fsm.on_packet(b"ACKN", now);
        assert_eq!(fsm.state(), LinkState::Linked);

        let later = now + Duration::from_secs(31);
        let action = fsm.tick(later);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn with_keepalive_timeout_shortens_the_silence_window() {
        // iax-f2b8 Task 3: M17Session threads a shortened `keepalive_timeout`
        // through for tests; confirm the custom constructor actually uses it
        // (rather than silently falling back to the 30 s default).
        let mut fsm = SessionFsm::with_keepalive_timeout(cs(), b'A', Duration::from_millis(200));
        let now = Instant::now();
        let _ = fsm.connect(now);
        let _ = fsm.on_packet(b"ACKN", now);
        assert_eq!(fsm.state(), LinkState::Linked);

        // Well under 30s, but past the configured 200ms window.
        let later = now + Duration::from_millis(250);
        let action = fsm.tick(later);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    // --- iax-f2b8-fix Fix 3: CONN retry while Connecting ---

    #[test]
    fn tick_resends_conn_about_once_a_second_while_connecting() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let conn_bytes = fsm.connect(now);
        assert_eq!(fsm.state(), LinkState::Connecting);

        // Well under the 1s resend interval: no resend yet.
        let action = fsm.tick(now + Duration::from_millis(400));
        assert_eq!(
            action,
            FsmAction::None,
            "must not resend CONN before the 1s interval elapses"
        );

        // Past the 1s resend interval: a fresh CONN goes out, byte-identical
        // to the original.
        let action = fsm.tick(now + Duration::from_millis(1_100));
        assert_eq!(
            action,
            FsmAction::Send(conn_bytes.clone()),
            "must resend the same CONN bytes once ~1s has passed with no ACKN/NACK"
        );
        assert_eq!(
            fsm.state(),
            LinkState::Connecting,
            "a resend must not itself change link state"
        );
    }

    #[test]
    fn ackn_stops_conn_resends() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let _ = fsm.connect(now);

        // First resend at +1.1s.
        let action = fsm.tick(now + Duration::from_millis(1_100));
        assert!(matches!(action, FsmAction::Send(_)), "expected a resend");

        // ACKN arrives right after: link is Linked.
        let ackn_at = now + Duration::from_millis(1_150);
        let action = fsm.on_packet(b"ACKN", ackn_at);
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Linked);

        // Even well past another 1s window, tick must never resend CONN once
        // linked (nothing pending — no packet received in this window, but the
        // link is up so any further tick is a plain keepalive check).
        let action = fsm.tick(ackn_at + Duration::from_millis(1_200));
        assert_eq!(
            action,
            FsmAction::None,
            "must not resend CONN once ACKN has linked the session"
        );
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn conn_resends_stop_once_the_link_fails() {
        let mut fsm = SessionFsm::with_keepalive_timeout(cs(), b'A', Duration::from_millis(500));
        let now = Instant::now();
        let _ = fsm.connect(now);

        // First resend at +1.1s would normally fire, but the 500ms keepalive
        // timeout elapses first: tick must report Unlinked/Failed, not a CONN
        // resend, and every tick after must keep reporting no further resend.
        let action = fsm.tick(now + Duration::from_millis(600));
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);

        let action = fsm.tick(now + Duration::from_secs(2));
        assert_eq!(
            action,
            FsmAction::None,
            "must not resend CONN (or do anything else) once Failed"
        );
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn nack_while_connecting_fails_the_link() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let _ = fsm.connect(now);
        assert_eq!(fsm.state(), LinkState::Connecting);

        let action = fsm.on_packet(b"NACK", now);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn voice_packet_while_linked_yields_voice_action() {
        let mut fsm = SessionFsm::new(cs(), b'A');
        let now = Instant::now();
        let _ = fsm.connect(now);
        let _ = fsm.on_packet(b"ACKN", now);

        let packet = StreamPacket {
            stream_id: 42,
            lsf: Lsf {
                dst: BROADCAST,
                src: cs(),
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number: 0,
            payload: [0xAA; 16],
        };
        let action = fsm.on_packet(&packet.to_bytes(), now);
        assert_eq!(action, FsmAction::Voice(packet));
    }
}
