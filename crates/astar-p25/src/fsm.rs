// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Client-side state machine for one `P25Reflector` link.
//!
//! P25 has no handshake. A client links by sending polls and stays linked by
//! continuing to; the reflector echoes each poll back verbatim
//! (`DVReflectors/P25Reflector/P25Reflector.cpp`), and that echo is the only
//! acknowledgement there is. So "linked" here means *a reflector has
//! answered*. A link that never answers looks the same as a wrong address,
//! which is why both end at [`LinkState::Failed`] rather than sitting on
//! "linking" forever.
//!
//! This is `astar_nxdn::fsm::NxdnFsm` without the talkgroup. A P25 poll
//! carries no talkgroup, a reflector is one talkgroup, and this machine never
//! filters on the one a record names: that number is a label for the person
//! listening, nothing more.
//!
//! Holds no clock and does no I/O: `now` is passed in to every
//! time-sensitive call, so tests run on a fake clock.

use std::time::{Duration, Instant};

use crate::ldu::VoiceRecord;
use crate::wire::{self, Callsign, CallsignError, Packet};

/// How often to poll once linked.
///
/// `P25Clients/P25Gateway/P25Gateway.cpp`: `CTimer pollTimer(1000U, 5U);`.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Silence after which the link is declared lost.
///
/// The reflector's own peer timeout: it forgets a peer it has not heard from
/// for 120 s (`P25Reflector.h`, `CP25Repeater::m_timer(1000U, 120U)`). After
/// that long with nothing from the reflector, no echo and no audio, there is
/// no link left to keep.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(120);

/// How many polls to send on linking, and unlinks on leaving.
///
/// `P25Gateway.cpp` writes three of each. It is UDP: one lost poll would
/// otherwise cost five seconds of "linking", and one lost unlink would leave
/// the reflector sending audio to a client that has gone.
pub const INITIAL_POLLS: usize = 3;

/// Current state of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected.
    Idle,
    /// Polls sent, nothing has answered yet.
    Linking,
    /// The reflector is answering.
    Linked,
    /// Unlinks sent; nothing more is expected.
    Unlinking,
    /// Nothing was heard for [`LINK_TIMEOUT`].
    Failed,
}

impl LinkState {
    /// A stable lowercase name.
    ///
    /// This is an ABI string: it is what a link state is called when it
    /// crosses to a front end. Renaming a variant is free; changing one of
    /// these strings is not.
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

/// What the caller should do in response to [`P25Fsm::on_packet`] or
/// [`P25Fsm::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsmAction {
    /// Nothing to do.
    None,
    /// Send these bytes to the reflector.
    Send(Vec<u8>),
    /// The link just came up.
    Linked,
    /// A voice record arrived on a live link.
    Voice(Box<VoiceRecord>),
    /// The talker finished: the end record arrived on a live link.
    End,
    /// Nothing has been heard for the link timeout; the state is now
    /// [`LinkState::Failed`].
    Timeout,
}

/// Pure client-side state machine for one `P25Reflector` link.
#[derive(Debug, Clone)]
pub struct P25Fsm {
    callsign: Callsign,
    state: LinkState,
    last_rx: Option<Instant>,
    last_poll: Option<Instant>,
    poll_interval: Duration,
    link_timeout: Duration,
}

impl P25Fsm {
    /// A new, idle link identifying itself as `callsign`.
    pub fn new(callsign: &str) -> Result<P25Fsm, CallsignError> {
        Self::with_timing(callsign, POLL_INTERVAL, LINK_TIMEOUT)
    }

    /// Like [`P25Fsm::new`], with caller-supplied timings, so tests can
    /// shrink the windows without touching the wire protocol.
    pub fn with_timing(
        callsign: &str,
        poll_interval: Duration,
        link_timeout: Duration,
    ) -> Result<P25Fsm, CallsignError> {
        Ok(P25Fsm {
            callsign: Callsign::new(callsign)?,
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

    fn live(&self) -> bool {
        matches!(self.state, LinkState::Linking | LinkState::Linked)
    }

    /// Opens the link: three polls to send, in order. Enters
    /// [`LinkState::Linking`].
    pub fn connect(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Linking;
        self.last_rx = Some(now);
        self.last_poll = Some(now);
        vec![wire::poll(&self.callsign).to_vec(); INITIAL_POLLS]
    }

    /// Closes the link: three unlinks to send, in order. Enters
    /// [`LinkState::Unlinking`]; no more polls follow.
    pub fn unlink(&mut self, _now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Unlinking;
        self.last_poll = None;
        vec![wire::unlink(&self.callsign).to_vec(); INITIAL_POLLS]
    }

    /// Feeds one received datagram in.
    ///
    /// Any datagram that parses refreshes the silence clock while the link
    /// is live. The echo of our own poll links us; another station's poll
    /// does not. A voice record while linking also links us, since a
    /// reflector sending audio plainly has us. Voice and the end record are
    /// passed up only while linked.
    pub fn on_packet(&mut self, datagram: &[u8], now: Instant) -> FsmAction {
        let Some(packet) = wire::parse(datagram) else {
            return FsmAction::None;
        };
        if !self.live() {
            return FsmAction::None;
        }
        self.last_rx = Some(now);
        match packet {
            Packet::Poll(callsign) if callsign == self.callsign => {
                if self.state == LinkState::Linking {
                    self.state = LinkState::Linked;
                    FsmAction::Linked
                } else {
                    FsmAction::None
                }
            }
            Packet::Voice(record) => {
                self.state = LinkState::Linked;
                FsmAction::Voice(Box::new(record))
            }
            Packet::End if self.state == LinkState::Linked => FsmAction::End,
            Packet::Poll(_) | Packet::Unlink(_) | Packet::End | Packet::Unknown(_) => {
                FsmAction::None
            }
        }
    }

    /// Drives the clock: sends a poll when one is due, or gives up on a link
    /// that has heard nothing for the link timeout.
    pub fn tick(&mut self, now: Instant) -> FsmAction {
        if !self.live() {
            return FsmAction::None;
        }
        if self
            .last_rx
            .is_some_and(|last| now.saturating_duration_since(last) >= self.link_timeout)
        {
            self.state = LinkState::Failed;
            return FsmAction::Timeout;
        }
        let due = self
            .last_poll
            .is_none_or(|last| now.saturating_duration_since(last) >= self.poll_interval);
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
    use crate::ldu::VoiceRecord;
    use crate::wire::{end_record, poll, unlink as unlink_datagram};

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn voice(index: u8) -> Vec<u8> {
        VoiceRecord {
            index,
            imbe: [1; 11],
            fields: [0; 3],
        }
        .encode()
    }

    fn linked(t0: Instant) -> P25Fsm {
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        f.connect(t0);
        assert_eq!(f.on_packet(&poll(f.callsign()), t0), FsmAction::Linked);
        f
    }

    #[test]
    fn connect_sends_three_polls_and_enters_linking() {
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        assert_eq!(f.state(), LinkState::Idle);
        let out = f.connect(Instant::now());
        assert_eq!(out.len(), INITIAL_POLLS);
        assert!(out.iter().all(|d| d.as_slice() == b"\xF0AJ7HR     "));
        assert_eq!(f.state(), LinkState::Linking);
    }

    #[test]
    fn a_bad_callsign_is_refused_at_construction() {
        assert_eq!(P25Fsm::new("").err(), Some(CallsignError::Empty));
        assert_eq!(
            P25Fsm::new("AJ7HR/MOBIL").err(),
            Some(CallsignError::TooLong)
        );
    }

    #[test]
    fn the_poll_echo_links_and_the_poll_timer_runs() {
        let t0 = Instant::now();
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        assert_eq!(f.connect(t0).len(), 3);
        assert_eq!(f.on_packet(&poll(f.callsign()), t0), FsmAction::Linked);
        assert_eq!(f.state(), LinkState::Linked);
        assert_eq!(f.tick(t0 + secs(4)), FsmAction::None);
        assert!(matches!(f.tick(t0 + secs(5)), FsmAction::Send(p) if p[0] == 0xF0));
        assert_eq!(f.tick(t0 + secs(6)), FsmAction::None);
        assert!(matches!(f.tick(t0 + secs(10)), FsmAction::Send(p) if p[0] == 0xF0));
    }

    #[test]
    fn a_second_echo_while_linked_is_quiet() {
        let t0 = Instant::now();
        let mut f = linked(t0);
        assert_eq!(
            f.on_packet(&poll(f.callsign()), t0 + secs(5)),
            FsmAction::None
        );
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn the_link_is_lost_after_120_s_of_silence_on_a_fake_clock() {
        let t0 = Instant::now();
        let mut f = linked(t0);
        let mut t = t0;
        while t < t0 + secs(119) {
            t += secs(1);
            assert_ne!(f.tick(t), FsmAction::Timeout, "at {:?}", t - t0);
        }
        assert_eq!(f.state(), LinkState::Linked);
        assert_eq!(f.tick(t0 + secs(120)), FsmAction::Timeout);
        assert_eq!(f.state(), LinkState::Failed);
        // A failed link stays failed and quiet.
        assert_eq!(f.tick(t0 + secs(125)), FsmAction::None);
        assert_eq!(
            f.on_packet(&poll(f.callsign()), t0 + secs(126)),
            FsmAction::None
        );
        assert_eq!(f.state(), LinkState::Failed);
    }

    #[test]
    fn anything_heard_refreshes_the_silence_clock() {
        let t0 = Instant::now();
        let mut f = linked(t0);
        f.on_packet(&[0x42, 0x00], t0 + secs(100));
        assert_ne!(f.tick(t0 + secs(120)), FsmAction::Timeout);
        assert_ne!(f.tick(t0 + secs(219)), FsmAction::Timeout);
        assert_eq!(f.tick(t0 + secs(220)), FsmAction::Timeout);
    }

    #[test]
    fn a_link_that_never_hears_an_echo_also_fails_at_120_s() {
        let t0 = Instant::now();
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        f.connect(t0);
        assert_ne!(f.tick(t0 + secs(119)), FsmAction::Timeout);
        assert_eq!(f.state(), LinkState::Linking);
        assert_eq!(f.tick(t0 + secs(120)), FsmAction::Timeout);
        assert_eq!(f.state(), LinkState::Failed);
    }

    #[test]
    fn with_timing_shrinks_the_windows() {
        let t0 = Instant::now();
        let mut f = P25Fsm::with_timing("AJ7HR", Duration::from_millis(100), secs(1)).unwrap();
        f.connect(t0);
        assert!(matches!(
            f.tick(t0 + Duration::from_millis(100)),
            FsmAction::Send(_)
        ));
        assert_eq!(f.tick(t0 + secs(1)), FsmAction::Timeout);
    }

    #[test]
    fn another_stations_poll_echo_is_not_ours() {
        let t0 = Instant::now();
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        f.connect(t0);
        let other = poll(&Callsign::new("N0CALL").unwrap());
        assert_eq!(f.on_packet(&other, t0), FsmAction::None);
        assert_eq!(f.state(), LinkState::Linking);
    }

    #[test]
    fn voice_while_linking_counts_as_linked() {
        let t0 = Instant::now();
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        f.connect(t0);
        let action = f.on_packet(&voice(4), t0);
        assert!(
            matches!(action, FsmAction::Voice(ref r) if r.index == 4),
            "{action:?}"
        );
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn voice_and_end_pass_up_only_while_linked() {
        let t0 = Instant::now();
        let mut idle = P25Fsm::new("AJ7HR").unwrap();
        assert_eq!(idle.on_packet(&voice(0), t0), FsmAction::None);
        assert_eq!(idle.on_packet(&end_record(), t0), FsmAction::None);
        assert_eq!(idle.state(), LinkState::Idle);

        let mut linking = P25Fsm::new("AJ7HR").unwrap();
        linking.connect(t0);
        assert_eq!(linking.on_packet(&end_record(), t0), FsmAction::None);
        assert_eq!(linking.state(), LinkState::Linking);

        let mut f = linked(t0);
        assert!(matches!(f.on_packet(&voice(17), t0), FsmAction::Voice(_)));
        assert_eq!(f.on_packet(&end_record(), t0), FsmAction::End);

        f.unlink(t0);
        assert_eq!(f.on_packet(&voice(1), t0), FsmAction::None);
        assert_eq!(f.on_packet(&end_record(), t0), FsmAction::None);
    }

    #[test]
    fn the_fsm_never_filters_on_talkgroup() {
        let t0 = Instant::now();
        let mut f = linked(t0);
        for tg in [0u8, 0x27, 0xFF] {
            let mut d = voice(3);
            d[1..4].copy_from_slice(&[tg, tg, tg]);
            assert!(matches!(f.on_packet(&d, t0), FsmAction::Voice(_)));
        }
    }

    #[test]
    fn unlink_sends_three_and_enters_unlinking() {
        let t0 = Instant::now();
        let mut f = linked(t0);
        let out = f.unlink(t0);
        assert_eq!(out.len(), INITIAL_POLLS);
        assert!(
            out.iter()
                .all(|d| d.as_slice() == unlink_datagram(f.callsign()))
        );
        assert_eq!(f.state(), LinkState::Unlinking);
        // No more polls, and no timeout, once unlinking.
        assert_eq!(f.tick(t0 + secs(5)), FsmAction::None);
        assert_eq!(f.tick(t0 + secs(500)), FsmAction::None);
    }

    #[test]
    fn garbage_never_moves_the_state() {
        let t0 = Instant::now();
        let mut f = P25Fsm::new("AJ7HR").unwrap();
        f.connect(t0);
        for d in [
            &[][..],
            &[0xF0][..],
            &[0x65; 10][..],
            &[0x80; 3][..],
            &[0xFF; 64][..],
        ] {
            assert_eq!(f.on_packet(d, t0), FsmAction::None);
            assert_eq!(f.state(), LinkState::Linking);
        }
    }

    #[test]
    fn link_state_names_are_stable() {
        let names: Vec<_> = [
            LinkState::Idle,
            LinkState::Linking,
            LinkState::Linked,
            LinkState::Unlinking,
            LinkState::Failed,
        ]
        .iter()
        .map(|s| s.as_str())
        .collect();
        assert_eq!(names, ["idle", "linking", "linked", "unlinking", "failed"]);
    }

    #[test]
    fn the_constants_are_the_gateways() {
        assert_eq!(POLL_INTERVAL, secs(5));
        assert_eq!(LINK_TIMEOUT, secs(120));
        assert_eq!(INITIAL_POLLS, 3);
    }
}
