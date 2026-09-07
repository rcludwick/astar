// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Client-side state machine for one `NXDNReflector` link.
//!
//! NXDN has no handshake. A client links by sending a poll and stays linked
//! by continuing to; the reflector echoes that same poll back, and that echo
//! is the only acknowledgement there is (`NXDNReflector.cpp`: `// Return the
//! poll`). So "linked" here means *a reflector has answered*, not *a
//! reflector has agreed* — and a link that never answers is
//! indistinguishable from a wrong address, which is why both end at
//! [`LinkState::Failed`] rather than sitting on "connecting" forever.
//!
//! This is `astar_ysf::fsm::YsfFsm` with three differences, and every one of
//! them is the talkgroup:
//!
//! 1. A poll and an unlink **carry** one — `NXDNGateway/NXDNNetwork.cpp:
//!    CNXDNNetwork::writePoll` puts it in `data[15]`/`data[16]` — so an
//!    `NxdnFsm` is built for a talkgroup and cannot change it.
//! 2. An echoed poll for a *different* talkgroup is not our acknowledgement.
//!    The reflector registers a client only when the poll's talkgroup is its
//!    own (`NXDNReflector.cpp`: `if (id == tg)`), so a poll that came back
//!    naming another one says nothing about our link.
//! 3. A frame whose `dst_id` is not our talkgroup, or that is not a group
//!    call, is dropped — the same test the reflector applies before relaying
//!    (`NXDNReflector.cpp`: `if (grp && dstId == tg)`).
//!
//! Holds no clock and does no I/O: `now` is passed in for every
//! time-sensitive operation, exactly as `astar_ysf`'s `YsfFsm` and
//! `astar_dstar`'s `DextraFsm` do.

use std::time::{Duration, Instant};

use crate::wire::{self, Callsign, CallsignError, DataPacket, Packet};

/// How often to poll once linked.
///
/// `NXDNGateway.cpp: CTimer pollTimer(1000U, 5U);` — five seconds, and the
/// reflector's own client timeout is set expecting it.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How many polls to fire at every link point.
///
/// `NXDNGateway.cpp` calls `writePoll` three times in a row when linking and
/// again when unlinking, and the reason is UDP: the first poll is also the
/// packet that punches the NAT hole, and losing it costs five seconds of an
/// operator staring at "connecting".
pub const INITIAL_POLLS: usize = 3;

/// Silence after which a link is declared dead.
///
/// astar's own number, not the reference implementation's: three missed
/// polls and no ambiguity. The reflector is far more patient — 120 s before
/// it forgets a client (`NXDNReflector.h: CNXDNRepeater::m_timer(1000U,
/// 120U)`, and [`crate::reflector::DEFAULT_CLIENT_TIMEOUT`]) — which is the
/// right asymmetry: the side with a user watching gives up first.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(30);

/// Current state of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected.
    Idle,
    /// Polls sent, nothing has answered yet.
    Linking,
    /// A reflector is answering.
    Linked,
    /// Unlink sent; nothing more is expected.
    Unlinking,
    /// Timed out.
    Failed,
}

impl LinkState {
    /// A stable lowercase name.
    ///
    /// This is an ABI string, not a debug convenience: it is what a link
    /// state is called when it crosses to a front-end. Renaming a variant is
    /// free; changing one of these strings is not.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LinkState::Idle => "idle",
            LinkState::Linking => "linking",
            LinkState::Linked => "linked",
            LinkState::Unlinking => "unlinking",
            LinkState::Failed => "failed",
        }
    }
}

/// What the caller should do in response to [`NxdnFsm::on_packet`] or
/// [`NxdnFsm::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsmAction {
    /// Nothing to do.
    None,
    /// Send these bytes to the reflector.
    Send(Vec<u8>),
    /// The link just came up.
    Linked,
    /// The link just went down at the far end.
    Unlinked,
    /// A network frame arrived.
    Data(Box<DataPacket>),
    /// Nothing has been heard for [`LINK_TIMEOUT`]; tear the session down.
    Timeout,
}

/// Pure client-side state machine for one `NXDNReflector` link.
#[derive(Debug, Clone)]
pub struct NxdnFsm {
    callsign: Callsign,
    talkgroup: u16,
    state: LinkState,
    last_rx: Option<Instant>,
    last_poll: Option<Instant>,
    poll_interval: Duration,
    link_timeout: Duration,
}

impl NxdnFsm {
    /// A new, unlinked session identifying itself as `callsign` and asking
    /// for `talkgroup`.
    pub fn new(callsign: &str, talkgroup: u16) -> Result<NxdnFsm, CallsignError> {
        Self::with_timing(callsign, talkgroup, POLL_INTERVAL, LINK_TIMEOUT)
    }

    /// Like [`NxdnFsm::new`], but with caller-supplied timings — lets tests
    /// shrink the poll and silence windows without touching the wire
    /// protocol.
    pub fn with_timing(
        callsign: &str,
        talkgroup: u16,
        poll_interval: Duration,
        link_timeout: Duration,
    ) -> Result<NxdnFsm, CallsignError> {
        Ok(NxdnFsm {
            callsign: Callsign::new(callsign)?,
            talkgroup,
            state: LinkState::Idle,
            last_rx: None,
            last_poll: None,
            poll_interval,
            link_timeout,
        })
    }

    /// Current [`LinkState`].
    #[must_use]
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// The callsign this link identifies itself with.
    #[must_use]
    pub fn callsign(&self) -> &Callsign {
        &self.callsign
    }

    /// The talkgroup this link is for.
    #[must_use]
    pub fn talkgroup(&self) -> u16 {
        self.talkgroup
    }

    /// Opens the link: the datagrams to send, in order.
    pub fn connect(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Linking;
        self.last_rx = Some(now);
        self.last_poll = Some(now);
        let poll = wire::poll(&self.callsign, self.talkgroup);
        vec![poll.to_vec(); INITIAL_POLLS]
    }

    /// Closes the link: the datagrams to send, in order.
    ///
    /// Sent three times for the same reason the polls are: an unlink that is
    /// lost leaves the reflector sending audio at a client that has gone,
    /// for the full 120 s of its client timeout.
    pub fn unlink(&mut self, _now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Unlinking;
        self.last_poll = None;
        let unlink = wire::unlink(&self.callsign, self.talkgroup);
        vec![unlink.to_vec(); INITIAL_POLLS]
    }

    /// Feeds one received datagram in.
    pub fn on_packet(&mut self, datagram: &[u8], now: Instant) -> FsmAction {
        let Some(packet) = wire::parse(datagram) else {
            return FsmAction::None;
        };
        // Anything at all from the far end counts as it being alive — the
        // silence timeout is about the reflector, not about polls
        // specifically.
        if self.state != LinkState::Idle {
            self.last_rx = Some(now);
        }
        match packet {
            Packet::Poll { talkgroup, .. } => {
                // Difference 2: the reflector registers a client only when
                // the poll's talkgroup is its own, so an echo naming another
                // one is not our acknowledgement.
                if talkgroup != self.talkgroup {
                    return FsmAction::None;
                }
                if self.state == LinkState::Linking {
                    self.state = LinkState::Linked;
                    return FsmAction::Linked;
                }
                FsmAction::None
            }
            Packet::Unlink { talkgroup, .. } => {
                if talkgroup != self.talkgroup {
                    return FsmAction::None;
                }
                if matches!(self.state, LinkState::Linking | LinkState::Linked) {
                    self.state = LinkState::Idle;
                    return FsmAction::Unlinked;
                }
                FsmAction::None
            }
            Packet::Data(data) => {
                // Difference 3: `NXDNReflector.cpp` relays only when `grp &&
                // dstId == tg`. Applying the same test here means a frame
                // that reaches us is one we asked for.
                if !data.group || data.dst_id != self.talkgroup {
                    return FsmAction::None;
                }
                // A reflector sending our talkgroup's audio has plainly
                // accepted the link, whether or not the echoed poll arrived
                // first.
                if self.state == LinkState::Linking {
                    self.state = LinkState::Linked;
                }
                if self.state == LinkState::Linked {
                    FsmAction::Data(data)
                } else {
                    FsmAction::None
                }
            }
            Packet::Unknown(_) => FsmAction::None,
        }
    }

    /// Drives the clock: keeps the link registered, or gives up on it.
    pub fn tick(&mut self, now: Instant) -> FsmAction {
        if !matches!(self.state, LinkState::Linking | LinkState::Linked) {
            return FsmAction::None;
        }
        if self
            .last_rx
            .is_some_and(|last| now.duration_since(last) >= self.link_timeout)
        {
            self.state = LinkState::Failed;
            return FsmAction::Timeout;
        }
        let due = self
            .last_poll
            .is_none_or(|last| now.duration_since(last) >= self.poll_interval);
        if due {
            self.last_poll = Some(now);
            return FsmAction::Send(wire::poll(&self.callsign, self.talkgroup).to_vec());
        }
        FsmAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fsm() -> NxdnFsm {
        NxdnFsm::with_timing(
            "KC0ABC",
            31313,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
        .expect("a legal callsign")
    }

    #[test]
    fn connect_sends_three_polls() {
        // NXDNGateway.cpp writes writePoll three times at every link point:
        // the first poll also punches the NAT hole, and losing it costs five
        // seconds of an operator staring at "connecting".
        let mut f = fsm();
        let out = f.connect(Instant::now());
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|d| &d[..5] == b"NXDNP" && d.len() == 17));
        assert_eq!(f.state(), LinkState::Linking);
    }

    #[test]
    fn the_reflectors_echoed_poll_is_the_whole_acknowledgement() {
        // NXDNReflector.cpp: `// Return the poll` — there is no distinct ack.
        let mut f = fsm();
        let now = Instant::now();
        f.connect(now);
        let echo = wire::poll(f.callsign(), 31313);
        assert_eq!(f.on_packet(&echo, now), FsmAction::Linked);
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn a_poll_echoed_for_another_talkgroup_does_not_link_us() {
        let mut f = fsm();
        let now = Instant::now();
        f.connect(now);
        let other = wire::poll(f.callsign(), 100);
        assert_eq!(f.on_packet(&other, now), FsmAction::None);
        assert_eq!(f.state(), LinkState::Linking);
    }

    #[test]
    fn a_data_frame_for_our_talkgroup_links_us_and_is_delivered() {
        // NXDNReflector.cpp relays only when `grp && dstId == tg`, so a frame
        // arriving at all is proof the link was accepted.
        let mut f = fsm();
        let now = Instant::now();
        f.connect(now);
        let packet = wire::DataPacket {
            src_id: 12345,
            dst_id: 31313,
            group: true,
            data: false,
            start: true,
            end: false,
            frame: [0u8; wire::FRAME_LEN],
        };
        match f.on_packet(&wire::data(&packet), now) {
            FsmAction::Data(d) => assert_eq!(d.src_id, 12345),
            other => panic!("expected data, got {other:?}"),
        }
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn a_data_frame_for_another_talkgroup_is_dropped() {
        let mut f = fsm();
        let now = Instant::now();
        f.connect(now);
        let packet = wire::DataPacket {
            src_id: 1,
            dst_id: 100,
            group: true,
            data: false,
            start: false,
            end: false,
            frame: [0u8; wire::FRAME_LEN],
        };
        assert_eq!(f.on_packet(&wire::data(&packet), now), FsmAction::None);
    }

    #[test]
    fn a_poll_is_due_every_five_seconds() {
        let mut f = fsm();
        let t0 = Instant::now();
        f.connect(t0);
        f.on_packet(&wire::poll(f.callsign(), 31313), t0);
        assert_eq!(f.tick(t0 + Duration::from_secs(4)), FsmAction::None);
        match f.tick(t0 + Duration::from_secs(5)) {
            FsmAction::Send(d) => assert_eq!(&d[..5], b"NXDNP"),
            other => panic!("expected a poll, got {other:?}"),
        }
    }

    #[test]
    fn thirty_seconds_of_silence_fails_the_link() {
        let mut f = fsm();
        let t0 = Instant::now();
        f.connect(t0);
        f.on_packet(&wire::poll(f.callsign(), 31313), t0);
        assert_eq!(f.tick(t0 + Duration::from_secs(30)), FsmAction::Timeout);
        assert_eq!(f.state(), LinkState::Failed);
    }

    #[test]
    fn unlink_sends_three_and_leaves_unlinking() {
        let mut f = fsm();
        let now = Instant::now();
        f.connect(now);
        let out = f.unlink(now);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|d| &d[..5] == b"NXDNU"));
        assert_eq!(f.state(), LinkState::Unlinking);
    }
}
