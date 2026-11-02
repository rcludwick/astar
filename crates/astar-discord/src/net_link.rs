// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Where the bridge pushes its audio and pulls the net's.

/// One 20 ms mono i16 frame in each direction, at [`NetLink::sample_rate`].
///
/// Two implementations are possible and only one is built: a conference
/// member ([`ConferenceLink`]) and a test double ([`TestLink`]). A future
/// IAX-peer host would fit the same trait — `RawDial`'s `rx_frames`/
/// `tx_frames` are the same pair with the types mirrored.
pub trait NetLink: Send {
    /// The net's audio, one 20 ms frame; `None` when nothing is queued.
    fn recv(&mut self) -> Option<Vec<i16>>;

    /// Contribute one 20 ms frame to the net.
    ///
    /// **Never called.** It completes the trait so that a two-way host can be
    /// added without reshaping it. Adding a caller is a deliberate act that
    /// must also add the arming and the `dstar_active` refusal that a
    /// transmit path requires.
    fn send(&mut self, pcm: &[i16]);

    /// Pipeline rate: 8000, or 16000 under a slin16 link.
    fn sample_rate(&self) -> u32;
}

/// A [`NetLink`] backed by a receive-only conference member.
pub struct ConferenceLink {
    handle: astar_iax::BridgeMemberHandle,
}

impl ConferenceLink {
    #[must_use]
    pub fn new(handle: astar_iax::BridgeMemberHandle) -> Self {
        Self { handle }
    }

    /// The enrolled member, for withdrawal on leave.
    #[must_use]
    pub fn member_id(&self) -> astar_audio::MemberId {
        self.handle.id
    }
}

impl NetLink for ConferenceLink {
    fn recv(&mut self) -> Option<Vec<i16>> {
        self.handle.from_conference.try_recv().ok()
    }

    fn send(&mut self, pcm: &[i16]) {
        // Never called (see `NetLink::send`), and harmless if it ever were —
        // but for a precise reason, not the loose "the mixer discards it".
        // The member is registered `contributes = false`, so the conference
        // leaves it out of `total_relay`, the sum that every other member's
        // TX and every outbound link is built from: this audio cannot reach
        // the air. It IS summed into `total_all`, which feeds only the local
        // speaker bus — silent today solely because `ensure_conference`
        // passes `local_out: None` and `include_local_radio` has no consumer.
        // Kept symmetric on purpose.
        let _ = self
            .handle
            .to_conference
            .send(astar_audio::RxFrame::from(pcm.to_vec()));
    }

    fn sample_rate(&self) -> u32 {
        self.handle.sample_rate
    }
}

/// A [`NetLink`] double: `push` queues a frame for `recv`, and everything
/// `send` receives is recorded for assertions.
pub struct TestLink {
    sample_rate: u32,
    inbound: std::sync::mpsc::Receiver<Vec<i16>>,
    inbound_tx: std::sync::mpsc::Sender<Vec<i16>>,
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<i16>>>>,
}

impl TestLink {
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let (inbound_tx, inbound) = std::sync::mpsc::channel();
        Self {
            sample_rate,
            inbound,
            inbound_tx,
            sent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Queue a frame for the next `recv`.
    pub fn push(&self, frame: Vec<i16>) {
        let _ = self.inbound_tx.send(frame);
    }

    /// Everything `send` has been handed, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Vec<i16>> {
        self.sent
            .lock()
            .expect("TestLink sent mutex poisoned")
            .clone()
    }

    /// A sender for this link's inbound queue, so a test can keep feeding it
    /// after the link itself has been moved into a [`Bridge`](crate::Bridge).
    /// The mirror of [`TestLink::clone_probe`] on the receive side.
    #[must_use]
    pub fn sender(&self) -> std::sync::mpsc::Sender<Vec<i16>> {
        self.inbound_tx.clone()
    }

    /// A handle sharing this link's `sent` record, so a test can assert on
    /// it after the link itself has been moved into a `Bridge`.
    #[must_use]
    pub fn clone_probe(&self) -> TestProbe {
        TestProbe {
            sent: std::sync::Arc::clone(&self.sent),
        }
    }
}

/// A handle onto a [`TestLink`]'s `sent` record that outlives the link being
/// moved elsewhere (for example, into a [`Bridge`](crate::Bridge)).
pub struct TestProbe {
    sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<i16>>>>,
}

impl TestProbe {
    /// Everything the linked `TestLink`'s `send` has been handed, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Vec<i16>> {
        self.sent
            .lock()
            .expect("TestLink sent mutex poisoned")
            .clone()
    }
}

impl NetLink for TestLink {
    fn recv(&mut self) -> Option<Vec<i16>> {
        self.inbound.try_recv().ok()
    }

    fn send(&mut self, pcm: &[i16]) {
        self.sent
            .lock()
            .expect("TestLink sent mutex poisoned")
            .push(pcm.to_vec());
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_link_round_trips_frames_and_records_sends() {
        let mut link = TestLink::new(8000);
        assert_eq!(link.sample_rate(), 8000);
        assert!(link.recv().is_none(), "empty to start");

        link.push(vec![7i16; 160]);
        assert_eq!(link.recv(), Some(vec![7i16; 160]));

        link.send(&[1i16, 2, 3]);
        assert_eq!(link.sent(), vec![vec![1i16, 2, 3]]);
    }

    #[test]
    fn conference_link_reports_the_handle_rate() {
        // Built from a hand-made handle so the test needs no engine.
        let (to_conference, _member_rx) = std::sync::mpsc::channel();
        let (member_tx, from_conference) = std::sync::mpsc::channel();
        let h = astar_iax::BridgeMemberHandle {
            id: astar_audio::MemberId::from_raw(1),
            to_conference,
            from_conference,
            sample_rate: 16000,
        };
        let mut link = ConferenceLink::new(h);
        assert_eq!(link.sample_rate(), 16000);
        assert!(link.recv().is_none());
        member_tx.send(vec![3i16; 320]).unwrap();
        assert_eq!(link.recv(), Some(vec![3i16; 320]));
    }
}
