// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Client-side state machine for one `YSFReflector` link.
//!
//! YSF has no handshake. A client links by sending a poll and stays linked
//! by continuing to; the reflector's own poll coming back is the only
//! acknowledgement there is. So "linked" here means *a reflector has
//! answered*, not *a reflector has agreed*, and the difference matters when
//! reporting state to an operator: a link that never answers is
//! indistinguishable from a wrong address, and both should say so rather
//! than sit on "connecting" forever.
//!
//! Holds no clock and does no I/O: `now` is passed in for every
//! time-sensitive operation, exactly as `astar_dstar`'s `DextraFsm` does.

use std::time::{Duration, Instant};

use crate::wire::{self, Callsign, CallsignError, DataPacket, Packet};

/// How often to poll once linked. The reflector expects to keep hearing
/// from a client; going quiet is how it decides one has gone away.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How many polls to fire when opening a link.
///
/// One would do on a clean network. Three is what every deployed client
/// sends, and the reason is UDP: the first poll is also the packet that
/// punches the NAT hole, and losing it costs five seconds of an operator
/// staring at "connecting".
pub const INITIAL_POLLS: usize = 3;

/// Silence after which a link is declared dead.
///
/// The reflector answers every poll, so five seconds of quiet is already
/// odd; thirty is three missed polls and no ambiguity. Same number
/// `astar_dstar` uses, for the same reason.
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
    /// This is an ABI string, not a debug convenience: it crosses the C ABI
    /// in the YSF link state and is matched by name in front-ends. Renaming
    /// a variant is free; changing one of these strings is not.
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

/// What the caller should do in response to [`YsfFsm::on_packet`] or
/// [`YsfFsm::tick`].
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
    /// A radio frame arrived.
    Data(Box<DataPacket>),
    /// Nothing has been heard for [`LINK_TIMEOUT`]; tear the session down.
    Timeout,
}

/// Pure client-side state machine for one `YSFReflector` link.
#[derive(Debug, Clone)]
pub struct YsfFsm {
    callsign: Callsign,
    options: Option<String>,
    state: LinkState,
    last_rx: Option<Instant>,
    last_poll: Option<Instant>,
    poll_interval: Duration,
    link_timeout: Duration,
}

impl YsfFsm {
    /// A new, unlinked session identifying itself as `callsign`.
    pub fn new(callsign: &str) -> Result<YsfFsm, CallsignError> {
        Self::with_timing(callsign, POLL_INTERVAL, LINK_TIMEOUT)
    }

    /// Like [`YsfFsm::new`], but with caller-supplied timings — lets tests
    /// shrink the poll and silence windows without touching the wire
    /// protocol.
    pub fn with_timing(
        callsign: &str,
        poll_interval: Duration,
        link_timeout: Duration,
    ) -> Result<YsfFsm, CallsignError> {
        Ok(YsfFsm {
            callsign: Callsign::new(callsign)?,
            options: None,
            state: LinkState::Idle,
            last_rx: None,
            last_poll: None,
            poll_interval,
            link_timeout,
        })
    }

    /// Sets the options string sent once the link comes up.
    ///
    /// This is how a YCS reflector is told which DG-ID room to join. Plain
    /// `YSFReflector` ignores it, so setting it is never wrong — but astar
    /// leaves it unset until something asks for a room, because an empty
    /// options packet is a packet nobody needed.
    pub fn set_options(&mut self, options: Option<String>) {
        self.options = options.filter(|o| !o.is_empty());
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

    /// Opens the link: the datagrams to send, in order.
    pub fn connect(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Linking;
        self.last_rx = Some(now);
        self.last_poll = Some(now);
        let poll = wire::poll(&self.callsign);
        vec![poll.to_vec(); INITIAL_POLLS]
    }

    /// Closes the link: the datagrams to send, in order.
    ///
    /// Sent more than once for the same reason the polls are: an unlink
    /// that is lost leaves the reflector still sending audio at a client
    /// that has gone.
    pub fn unlink(&mut self, _now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Unlinking;
        self.last_poll = None;
        let unlink = wire::unlink(&self.callsign);
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
            Packet::Poll { .. } => {
                if self.state == LinkState::Linking {
                    self.state = LinkState::Linked;
                    if let Some(options) = &self.options {
                        // Linked *and* there is a room to ask for: say so
                        // now, while the reflector is listening.
                        let bytes = wire::options(&self.callsign, options);
                        return FsmAction::Send(bytes.to_vec());
                    }
                    return FsmAction::Linked;
                }
                FsmAction::None
            }
            Packet::Unlink { .. } => {
                if matches!(self.state, LinkState::Linking | LinkState::Linked) {
                    self.state = LinkState::Idle;
                    return FsmAction::Unlinked;
                }
                FsmAction::None
            }
            Packet::Data(data) => {
                // A reflector that sends audio has plainly accepted the
                // link, whether or not its poll reply arrived first.
                if self.state == LinkState::Linking {
                    self.state = LinkState::Linked;
                }
                if self.state == LinkState::Linked {
                    FsmAction::Data(data)
                } else {
                    FsmAction::None
                }
            }
            Packet::Options | Packet::Status | Packet::Info | Packet::Unknown(_) => FsmAction::None,
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
            return FsmAction::Send(wire::poll(&self.callsign).to_vec());
        }
        FsmAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FRAME_LEN;

    fn fsm() -> YsfFsm {
        YsfFsm::with_timing("KC0ABC", Duration::from_secs(5), Duration::from_secs(30))
            .expect("a legal callsign")
    }

    fn reflector_poll() -> Vec<u8> {
        wire::poll(&Callsign::new("REFLECT").expect("a legal callsign")).to_vec()
    }

    fn reflector_data() -> Vec<u8> {
        wire::data(&DataPacket {
            gateway: Callsign::new("REFLECT").expect("a legal callsign"),
            source: Callsign::new("W1AW").expect("a legal callsign"),
            destination: Callsign::new("ALL").expect("a legal callsign"),
            counter: 1,
            end: false,
            frame: [0u8; FRAME_LEN],
        })
        .to_vec()
    }

    #[test]
    fn a_bad_callsign_is_refused_before_anything_is_sent() {
        assert_eq!(
            YsfFsm::new("TOO LONG A CALL").err(),
            Some(CallsignError::TooLong)
        );
    }

    #[test]
    fn connecting_sends_three_polls() {
        let mut fsm = fsm();
        let out = fsm.connect(Instant::now());
        assert_eq!(out.len(), INITIAL_POLLS);
        assert!(out.iter().all(|p| &p[..4] == b"YSFP"));
        assert_eq!(fsm.state(), LinkState::Linking);
    }

    #[test]
    fn a_returned_poll_is_what_linked_means() {
        let mut fsm = fsm();
        let now = Instant::now();
        fsm.connect(now);
        assert_eq!(fsm.on_packet(&reflector_poll(), now), FsmAction::Linked);
        assert_eq!(fsm.state(), LinkState::Linked);
        // Later polls are the reflector keeping itself alive, not a second
        // link coming up.
        assert_eq!(fsm.on_packet(&reflector_poll(), now), FsmAction::None);
    }

    #[test]
    fn audio_before_the_poll_reply_still_counts_as_linked() {
        // Reflectors under load have been seen sending the first audio
        // frame before their poll acknowledgement. Dropping it because a
        // state machine had not caught up would lose the first word of a
        // transmission.
        let mut fsm = fsm();
        let now = Instant::now();
        fsm.connect(now);
        let action = fsm.on_packet(&reflector_data(), now);
        assert!(matches!(action, FsmAction::Data(_)));
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn data_before_connecting_is_ignored() {
        let mut fsm = fsm();
        assert_eq!(
            fsm.on_packet(&reflector_data(), Instant::now()),
            FsmAction::None
        );
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn the_options_string_goes_out_the_moment_the_link_answers() {
        let mut fsm = fsm();
        fsm.set_options(Some("room 42".to_string()));
        let now = Instant::now();
        fsm.connect(now);
        let FsmAction::Send(bytes) = fsm.on_packet(&reflector_poll(), now) else {
            panic!("expected the options packet");
        };
        assert_eq!(&bytes[..4], b"YSFO");
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn an_empty_options_string_is_not_a_packet() {
        let mut fsm = fsm();
        fsm.set_options(Some(String::new()));
        let now = Instant::now();
        fsm.connect(now);
        assert_eq!(fsm.on_packet(&reflector_poll(), now), FsmAction::Linked);
    }

    #[test]
    fn polls_go_out_on_the_interval_and_not_before() {
        let mut fsm = fsm();
        let start = Instant::now();
        fsm.connect(start);
        fsm.on_packet(&reflector_poll(), start);

        assert_eq!(fsm.tick(start + Duration::from_secs(4)), FsmAction::None);
        let FsmAction::Send(bytes) = fsm.tick(start + Duration::from_secs(5)) else {
            panic!("a poll was due");
        };
        assert_eq!(&bytes[..4], b"YSFP");
        assert_eq!(fsm.tick(start + Duration::from_secs(6)), FsmAction::None);
    }

    #[test]
    fn silence_times_the_link_out() {
        let mut fsm = fsm();
        let start = Instant::now();
        fsm.connect(start);
        fsm.on_packet(&reflector_poll(), start);
        assert_eq!(
            fsm.tick(start + Duration::from_secs(30)),
            FsmAction::Timeout
        );
        assert_eq!(fsm.state(), LinkState::Failed);
        // Once failed it stays failed: no more polls at a reflector that
        // has stopped answering.
        assert_eq!(fsm.tick(start + Duration::from_secs(60)), FsmAction::None);
    }

    #[test]
    fn a_reflector_that_never_answers_times_out_rather_than_hanging() {
        let mut fsm = fsm();
        let start = Instant::now();
        fsm.connect(start);
        assert_eq!(
            fsm.tick(start + Duration::from_secs(30)),
            FsmAction::Timeout
        );
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn any_traffic_keeps_the_link_alive() {
        let mut fsm = fsm();
        let start = Instant::now();
        fsm.connect(start);
        fsm.on_packet(&reflector_poll(), start);
        // Audio, not polls, for a minute: the link is plainly fine.
        for second in 1..60 {
            let now = start + Duration::from_secs(second);
            fsm.on_packet(&reflector_data(), now);
            assert_ne!(fsm.tick(now), FsmAction::Timeout, "at {second}s");
        }
    }

    #[test]
    fn unlinking_stops_the_polls() {
        let mut fsm = fsm();
        let start = Instant::now();
        fsm.connect(start);
        fsm.on_packet(&reflector_poll(), start);
        let out = fsm.unlink(start);
        assert_eq!(out.len(), INITIAL_POLLS);
        assert!(out.iter().all(|p| &p[..4] == b"YSFU"));
        assert_eq!(fsm.state(), LinkState::Unlinking);
        assert_eq!(fsm.tick(start + Duration::from_secs(60)), FsmAction::None);
    }

    #[test]
    fn an_unlink_from_the_far_end_takes_the_link_down() {
        let mut fsm = fsm();
        let now = Instant::now();
        fsm.connect(now);
        fsm.on_packet(&reflector_poll(), now);
        let bytes = wire::unlink(&Callsign::new("REFLECT").expect("a legal callsign"));
        assert_eq!(fsm.on_packet(&bytes, now), FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn chatter_is_ignored_without_disturbing_the_link() {
        let mut fsm = fsm();
        let now = Instant::now();
        fsm.connect(now);
        fsm.on_packet(&reflector_poll(), now);
        for chatter in [&b"YSFI hello"[..], b"YSFS", b"WXYZ...."] {
            assert_eq!(fsm.on_packet(chatter, now), FsmAction::None);
            assert_eq!(fsm.state(), LinkState::Linked);
        }
    }

    #[test]
    fn every_state_has_a_stable_name() {
        assert_eq!(LinkState::Idle.as_str(), "idle");
        assert_eq!(LinkState::Linking.as_str(), "linking");
        assert_eq!(LinkState::Linked.as_str(), "linked");
        assert_eq!(LinkState::Unlinking.as_str(), "unlinking");
        assert_eq!(LinkState::Failed.as_str(), "failed");
    }
}
