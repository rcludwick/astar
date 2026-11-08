// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The only part of the daemon that holds a [`Station`].
//!
//! [`ListenOnlyStation`] owns its station privately and offers an allow-list:
//! join a net receive-only, read how the link is doing, leave. There is no
//! way through this type to key a radio, and nothing else in the crate may
//! hold a station to try. `ci/guard-netrec-receive-only.sh` is the backstop.
//!
//! Two more things stand behind that:
//!
//! - The pull backend has no input device, so a D-Star route opens with no
//!   capture lane and its keying path refuses.
//! - The M17 side uses the monitor, which has a control-only socket, and its
//!   conference member's contribution channel is dropped the moment it exists.

// RECEIVE-ONLY DENY LIST
// `ci/guard-netrec-receive-only.sh` fails on any of these anywhere under
// crates/astar-netrec, and this comment is the only place they may be named:
//   set_ptt() link_key() link_set_mode() sync_link_keying() send_dtmf*()
//   announce*() echolink_send_chat() answer() enable_inbound() set_mode()
//   connect() connect_wt() connect_wt_at() m17_connect() link_connect()
//   link_connect_at() echolink_connect() echolink_node_connect()
//   ysf_connect() nxdn_connect() dmr_connect() start_inbound*()
//   answer_pending() connect_with_*()
// add_bridge_member(), set_bridge_config() and set_dstar_vocoder() are
// allowed in this file and nowhere else, as are the words Station,
// ConsoleSession and DstarSession outside a comment.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use astar_audio::MemberId;
use astar_audio::pull_backend::{PullBackend, PullSink};
use astar_console::ConsoleSession;
use astar_iax::{BridgeConfig, BridgeMode};
use astar_station::{
    DstarLinkState as LinkState, DstarVocoder, M17MonitorEdge, M17MonitorRequest, Station,
    StationConfig, VocoderTrust,
};

use crate::feed::{AudioFeed, BusTap, ConferenceFeed, Pump};

/// Why the recorder could not listen.
#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    /// The station refused.
    #[error(transparent)]
    Station(#[from] astar_station::StationError),
    /// This station is already joined to a net.
    #[error("already joined; leave first")]
    AlreadyJoined,
    /// The callsign cannot be logged in with on this network.
    #[error("{0:?} cannot be used as a login callsign")]
    BadCallsign(String),
    /// The engine has no conference for the monitor to feed.
    #[error("the engine has no conference to listen to")]
    NoConference,
    /// The thread that reads the conference could not start.
    #[error("could not start the audio pump: {0}")]
    Pump(std::io::Error),
}

/// How a join is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    /// Waiting for the reflector.
    Linking,
    /// The reflector accepted the link.
    Linked,
    /// The link failed. The `String` says why.
    Failed(String),
    /// The link is over, or there never was one. The `String` says why.
    Ended(String),
}

/// What is joined right now.
enum Link {
    /// An M17 monitor, heard through this conference member, with the pump
    /// that reads it.
    M17 { member: MemberId, pump: Pump },
    /// A D-Star session, heard on the output bus.
    Dstar,
}

/// A station that can listen to a reflector and do nothing else.
///
/// One join at a time. After [`Self::leave`], or once [`Self::status`] reads
/// [`LinkStatus::Failed`] or [`LinkStatus::Ended`], a join may be made again.
pub struct ListenOnlyStation {
    station: Station,
    session: Arc<Mutex<ConsoleSession>>,
    rate: u32,
    link: Option<Link>,
    /// Why the M17 monitor ended. Polling reaps a monitor, so its reason has
    /// to be kept here to stay readable until the next join.
    ended: Mutex<Option<String>>,
    tap: Arc<BusTap>,
    dropped: Arc<AtomicU64>,
}

impl ListenOnlyStation {
    /// Build the station and its engine. Opens no socket and no device.
    ///
    /// `trusted_serials` is `[vocoder] trusted_serials`, the only sticks a
    /// D-Star join may open with [`DstarVocoder::Dongle`]. With
    /// [`DstarVocoder::Software`] it is ignored and the trust list stays
    /// empty, so nothing ever opens a serial port.
    ///
    /// # Errors
    /// [`ListenError::Station`] if the bridge topology cannot be set.
    pub fn new(vocoder: DstarVocoder, trusted_serials: &[String]) -> Result<Self, ListenError> {
        let dropped = Arc::new(AtomicU64::new(0));
        let tap = Arc::new(BusTap::new(Arc::clone(&dropped)));
        let session = Arc::new(Mutex::new(ConsoleSession::new()));

        let sink_tap = Arc::clone(&tap);
        let sink: PullSink = Arc::new(move |frame| sink_tap.push(frame));
        let factory_sink = Arc::clone(&sink);
        let station = Station::with_shared_session(
            StationConfig {
                vocoder_trust: trust_for(vocoder, trusted_serials),
                ..StationConfig::default()
            },
            Arc::clone(&session),
            Box::new(move || Box::new(PullBackend::new(Arc::clone(&factory_sink)))),
        );
        station.set_dstar_vocoder(vocoder);
        station.set_bridge_config(BridgeConfig {
            mode: BridgeMode::Bridge,
            ..BridgeConfig::default()
        })?;
        // netrec.toml has no codec directories yet, so M17 uses the system
        // search, as astar-server does with no `[m17] codec_dirs`.
        station.set_codec_dirs(Vec::new());

        // The M17 monitor needs a conference, and a conference needs the
        // engine, so it is built here rather than by the first join. The
        // conference and the output bus both run at the engine's one rate.
        let rate = session
            .lock()
            .expect("session poisoned")
            .ensure_engine(|| Box::new(PullBackend::new(sink)))
            .pipeline_sample_rate();

        Ok(Self {
            station,
            session,
            rate,
            link: None,
            ended: Mutex::new(None),
            tap,
            dropped,
        })
    }

    /// The sample rate of every frame this station's feeds give, in Hz.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Frames lost to a slow consumer since this station was built.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Listen to an M17 reflector module. Returns once the monitor has
    /// started; watch [`Self::status`] for the link.
    ///
    /// # Errors
    /// [`ListenError::AlreadyJoined`] while a link is up or on its way;
    /// [`ListenError::Station`] for a bad module or callsign;
    /// [`ListenError::NoConference`] if the engine has no conference;
    /// [`ListenError::Pump`] if its reader thread cannot start.
    pub fn join_m17(
        &mut self,
        host: &str,
        port: u16,
        module: char,
        callsign: &str,
    ) -> Result<Box<dyn AudioFeed>, ListenError> {
        self.make_way()?;
        self.station.m17_monitor_connect(M17MonitorRequest {
            host: host.to_string(),
            port,
            module,
            callsign: callsign.to_string(),
        })?;
        let Some(handle) = self.station.add_bridge_member() else {
            self.station.m17_monitor_disconnect();
            return Err(ListenError::NoConference);
        };
        // The contribution channel goes first: this member can hear the
        // conference, and from here it has no way to speak to it.
        drop(handle.to_conference);

        // One short lock per frame, over a read of the monitor's atomics.
        let session = Arc::clone(&self.session);
        let receiving = Box::new(move || {
            session
                .lock()
                .expect("session poisoned")
                .m17_monitor_status()
                .is_some_and(|s| s.receiving)
        });
        let started = ConferenceFeed::start(
            handle.from_conference,
            handle.sample_rate,
            receiving,
            Arc::clone(&self.dropped),
        );
        match started {
            Ok((feed, pump)) => {
                let member = handle.id;
                self.link = Some(Link::M17 { member, pump });
                Ok(Box::new(feed))
            }
            Err(e) => {
                self.station.m17_monitor_disconnect();
                self.station.remove_bridge_member(handle.id);
                Err(ListenError::Pump(e))
            }
        }
    }

    /// Listen to a D-Star reflector module. `reflector` is the reflector's
    /// name when `host` does not give it away, as with a bare IP address.
    ///
    /// # Errors
    /// [`ListenError::AlreadyJoined`] while a link is up or on its way;
    /// [`ListenError::Station`] for a bad module or callsign, or when the
    /// chosen vocoder cannot be opened.
    pub fn join_dstar(
        &mut self,
        host: &str,
        port: u16,
        module: char,
        callsign: &str,
        reflector: Option<&str>,
    ) -> Result<Box<dyn AudioFeed>, ListenError> {
        self.make_way()?;
        let feed = self.tap.open(self.rate);
        if let Err(e) = self
            .station
            .dstar_connect(host, port, module, callsign, reflector)
        {
            self.tap.close();
            return Err(e.into());
        }
        self.link = Some(Link::Dstar);
        Ok(Box::new(feed))
    }

    /// How the join is doing. With nothing joined this is
    /// [`LinkStatus::Ended`].
    #[must_use]
    pub fn status(&self) -> LinkStatus {
        match self.link {
            None => LinkStatus::Ended("not joined".to_string()),
            Some(Link::M17 { .. }) => self.m17_status(),
            Some(Link::Dstar) => self.dstar_status(),
        }
    }

    fn m17_status(&self) -> LinkStatus {
        let mut ended = self.ended.lock().expect("ended poisoned");
        while let Some(edge) = self.station.m17_monitor_poll() {
            if let M17MonitorEdge::Ended(reason) = edge {
                *ended = Some(reason);
            }
        }
        if let Some(reason) = ended.as_ref() {
            return LinkStatus::Ended(reason.clone());
        }
        match self.station.m17_monitor_status() {
            None => LinkStatus::Ended("monitor gone".to_string()),
            Some(s) => match s.link {
                "linked" => LinkStatus::Linked,
                "failed" => {
                    LinkStatus::Failed(s.fail_reason.unwrap_or_else(|| "link failed".to_string()))
                }
                _ => LinkStatus::Linking,
            },
        }
    }

    fn dstar_status(&self) -> LinkStatus {
        match self.station.dstar_state().map(|s| s.link) {
            None => LinkStatus::Ended("session gone".to_string()),
            Some(LinkState::Idle | LinkState::Linking) => LinkStatus::Linking,
            Some(LinkState::Linked) => LinkStatus::Linked,
            Some(LinkState::Unlinking) => LinkStatus::Ended("unlinking".to_string()),
            // The state carries no reason: a refusal, a timeout and a drop
            // all read the same.
            Some(LinkState::Failed) => LinkStatus::Failed("link failed".to_string()),
        }
    }

    /// The last callsign heard. It comes off the air, so treat it as
    /// untrusted text.
    #[must_use]
    pub fn talker(&self) -> Option<String> {
        match self.link {
            None => None,
            Some(Link::M17 { .. }) => self.station.m17_monitor_status()?.talker,
            Some(Link::Dstar) => self.station.dstar_state()?.talker,
        }
    }

    /// Drop the link, if there is one. Safe to call at any time.
    pub fn leave(&mut self) {
        match self.link.take() {
            None => {}
            Some(Link::M17 { member, pump }) => {
                pump.stop();
                self.station.m17_monitor_disconnect();
                self.station.remove_bridge_member(member);
            }
            Some(Link::Dstar) => {
                self.station.dstar_disconnect();
                self.tap.close();
            }
        }
        *self.ended.lock().expect("ended poisoned") = None;
    }

    /// Clear a dead link so a join can go ahead, or refuse if one is alive.
    /// Nothing reaps a failed D-Star link on its own, and it holds the audio
    /// route until something does.
    fn make_way(&mut self) -> Result<(), ListenError> {
        if matches!(self.status(), LinkStatus::Linking | LinkStatus::Linked) {
            return Err(ListenError::AlreadyJoined);
        }
        self.leave();
        Ok(())
    }
}

impl Drop for ListenOnlyStation {
    /// The station's own drop leaves an M17 monitor running.
    fn drop(&mut self) {
        self.leave();
    }
}

/// The sticks this station may open: the configured ones for a dongle, and
/// none at all for the software vocoder.
fn trust_for(vocoder: DstarVocoder, trusted_serials: &[String]) -> VocoderTrust {
    match vocoder {
        DstarVocoder::Dongle => VocoderTrust::new(trusted_serials),
        DstarVocoder::Software => VocoderTrust::none(),
    }
}

/// The networks a recorder can log in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// M17.
    M17,
    /// D-Star.
    Dstar,
}

/// The callsign a reflector sees for `call` on `network`. M17 takes the
/// suffix after a space. D-Star's is the eighth character of the field, so
/// the call is padded to seven first.
///
/// # Errors
/// [`ListenError::BadCallsign`] for an empty call, a non-ASCII one, or one
/// too long for the network's field (nine characters on M17, eight on
/// D-Star, suffix included). Nothing is cut short: a shortened call would be
/// someone else's.
pub fn login_callsign(network: Network, call: &str, suffix: char) -> Result<String, ListenError> {
    let (login, field) = match network {
        Network::M17 => (format!("{call} {suffix}"), 9),
        Network::Dstar => (format!("{call:<7}{suffix}"), 8),
    };
    if call.is_empty() || !login.is_ascii() || login.len() > field {
        return Err(ListenError::BadCallsign(call.to_string()));
    }
    Ok(login)
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(feature = "dstar-soft-ambe")]
    use astar_dstar::{DsvtPacket, Reflector, RfHeader};
    use astar_m17::ControlPacket;

    use super::*;

    const CALL: &str = "N0CALL R";

    fn software() -> ListenOnlyStation {
        ListenOnlyStation::new(DstarVocoder::Software, &[]).expect("station")
    }

    fn wait_for(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    /// A loopback socket that answers each login with the next scripted
    /// reply, repeating the last one forever.
    struct ScriptedReflector {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl ScriptedReflector {
        fn start(is_login: fn(&[u8]) -> bool, replies: Vec<Vec<u8>>) -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
            sock.set_read_timeout(Some(Duration::from_millis(20)))
                .expect("read timeout");
            let addr = sock.local_addr().expect("fake reflector addr");
            let stop = Arc::new(AtomicBool::new(false));
            let t_stop = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let mut buf = [0u8; 2_048];
                let mut next = 0;
                while !t_stop.load(Ordering::Acquire) {
                    if let Ok((n, from)) = sock.recv_from(&mut buf)
                        && is_login(&buf[..n])
                    {
                        let reply = &replies[next.min(replies.len() - 1)];
                        next += 1;
                        let _ = sock.send_to(reply, from);
                    }
                }
            });
            Self {
                addr,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for ScriptedReflector {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn m17_reflector(replies: &[ControlPacket]) -> ScriptedReflector {
        ScriptedReflector::start(
            |d| matches!(ControlPacket::parse(d), Some(ControlPacket::Conn { .. })),
            replies.iter().map(ControlPacket::to_bytes).collect(),
        )
    }

    /// A `DExtra` reply: ten bytes of callsign and modules, then the verdict.
    #[cfg(feature = "dstar-soft-ambe")]
    fn dextra_reply(verdict: [u8; 4]) -> Vec<u8> {
        let mut reply = b"XRF757 G A".to_vec();
        reply.extend_from_slice(&verdict);
        reply
    }

    #[cfg(feature = "dstar-soft-ambe")]
    fn dstar_reflector(replies: &[[u8; 4]]) -> ScriptedReflector {
        // A login names a module; an unlink blanks it.
        ScriptedReflector::start(
            |d| d.len() == 11 && d[9] != b' ',
            replies.iter().copied().map(dextra_reply).collect(),
        )
    }

    fn join_m17(
        l: &mut ListenOnlyStation,
        at: SocketAddr,
    ) -> Result<Box<dyn AudioFeed>, ListenError> {
        l.join_m17("127.0.0.1", at.port(), 'A', CALL)
    }

    fn join_dstar(
        l: &mut ListenOnlyStation,
        at: SocketAddr,
    ) -> Result<Box<dyn AudioFeed>, ListenError> {
        l.join_dstar("127.0.0.1", at.port(), 'A', CALL, Some("XRF757"))
    }

    fn linked(l: &ListenOnlyStation) -> bool {
        wait_for(|| l.status() == LinkStatus::Linked, 3_000)
    }

    #[test]
    fn login_callsign_forms() {
        let login = |n, c| login_callsign(n, c, 'R').expect("valid");
        assert_eq!(login(Network::M17, "N0CALL"), "N0CALL R");
        assert_eq!(login(Network::M17, "KX0AAAA"), "KX0AAAA R");
        assert_eq!(login(Network::Dstar, "N0CALL"), "N0CALL R");
        assert_eq!(login(Network::Dstar, "KX0AAAA"), "KX0AAAAR");
        assert_eq!(login(Network::Dstar, "W1AW"), "W1AW   R");
    }

    #[test]
    fn login_callsign_refuses_what_the_field_cannot_hold() {
        for (network, call) in [
            (Network::M17, "KX0AAAAA"),
            (Network::Dstar, "KX0AAAAA"),
            (Network::M17, ""),
            (Network::Dstar, ""),
            (Network::Dstar, "N0CÄLL"),
        ] {
            assert!(
                matches!(
                    login_callsign(network, call, 'R'),
                    Err(ListenError::BadCallsign(_))
                ),
                "{network:?} {call:?}"
            );
        }
    }

    #[test]
    fn longest_m17_login_still_encodes() {
        let login = login_callsign(Network::M17, "KX0AAAA", 'R').expect("valid");
        astar_m17::encode_callsign(&login).expect("nine characters fit");
    }

    #[test]
    fn software_keeps_trust_empty() {
        let serials = ["DV012345".to_string()];
        let l = ListenOnlyStation::new(DstarVocoder::Software, &serials).expect("station");
        assert!(l.station.vocoder_trust().is_empty());
    }

    /// The list only. Nothing here builds a station, so no port is near it.
    #[test]
    fn dongle_trusts_exactly_the_configured_serials() {
        let serials = ["DV012345".to_string(), " DV012345 ".to_string()];
        assert_eq!(
            trust_for(DstarVocoder::Dongle, &serials).serials,
            vec!["DV012345".to_string()]
        );
        assert!(trust_for(DstarVocoder::Dongle, &[]).is_empty());
    }

    #[cfg(not(feature = "dstar-soft-ambe"))]
    #[test]
    fn software_vocoder_missing_from_the_build_is_refused_cleanly() {
        let refl = m17_reflector(&[ControlPacket::Ackn]);
        let mut l = software();
        assert!(matches!(
            join_dstar(&mut l, refl.addr),
            Err(ListenError::Station(_))
        ));
        assert!(matches!(l.status(), LinkStatus::Ended(_)));
        let _feed = join_m17(&mut l, refl.addr).expect("the audio route was given back");
        assert!(linked(&l));
    }

    #[test]
    fn building_the_station_opens_no_listener() {
        assert!(!software().station.is_listening());
    }

    #[test]
    fn rate_is_the_engine_rate_straight_after_new() {
        let l = software();
        assert_eq!(l.rate(), 8_000);
        let manager_rate = l
            .session
            .lock()
            .unwrap()
            .ensure_engine(|| unreachable!("new built the engine"))
            .pipeline_sample_rate();
        assert_eq!(l.rate(), manager_rate);
    }

    #[test]
    fn nothing_joined_reads_as_ended() {
        let l = software();
        assert!(matches!(l.status(), LinkStatus::Ended(_)));
        assert_eq!(l.talker(), None);
        assert_eq!(l.dropped_frames(), 0);
    }

    #[test]
    fn leave_is_safe_never_joined_and_twice() {
        let mut l = software();
        l.leave();
        l.leave();
        let refl = m17_reflector(&[ControlPacket::Ackn]);
        let _feed = join_m17(&mut l, refl.addr).expect("join");
        l.leave();
        l.leave();
        assert!(matches!(l.status(), LinkStatus::Ended(_)));
    }

    #[test]
    fn m17_second_join_refused_and_rejoin_after_leave_works() {
        let refl = m17_reflector(&[ControlPacket::Ackn]);
        let mut l = software();
        let _feed = join_m17(&mut l, refl.addr).expect("join");
        assert!(linked(&l));
        assert!(matches!(
            join_m17(&mut l, refl.addr),
            Err(ListenError::AlreadyJoined)
        ));
        assert!(matches!(
            join_dstar(&mut l, refl.addr),
            Err(ListenError::AlreadyJoined)
        ));
        assert_eq!(
            l.status(),
            LinkStatus::Linked,
            "a refused join changes nothing"
        );
        l.leave();
        let _feed = join_m17(&mut l, refl.addr).expect("rejoin");
        assert!(linked(&l));
    }

    #[test]
    fn m17_refused_link_ends_and_rejoin_works() {
        let refl = m17_reflector(&[ControlPacket::Nack, ControlPacket::Ackn]);
        let mut l = software();
        let _feed = join_m17(&mut l, refl.addr).expect("join");
        assert!(wait_for(
            || matches!(l.status(), LinkStatus::Failed(_) | LinkStatus::Ended(_)),
            3_000
        ));
        assert!(
            matches!(l.status(), LinkStatus::Failed(_) | LinkStatus::Ended(_)),
            "the end of a link stays readable until the next join"
        );
        let _feed = join_m17(&mut l, refl.addr).expect("rejoin after the refusal");
        assert!(linked(&l));
        assert_eq!(
            l.station.conference_member_count(),
            2,
            "monitor and tap, once each"
        );
    }

    #[test]
    fn m17_leave_withdraws_the_conference_member() {
        let refl = m17_reflector(&[ControlPacket::Ackn]);
        let mut l = software();
        let _feed = join_m17(&mut l, refl.addr).expect("join");
        assert!(linked(&l));
        l.leave();
        assert_eq!(l.station.conference_member_count(), 0);
    }

    #[cfg(feature = "dstar-soft-ambe")]
    #[test]
    fn dstar_route_opens_with_no_way_to_transmit() {
        let refl = dstar_reflector(&[*b"ACK\0"]);
        let mut l = software();
        let _feed = join_dstar(&mut l, refl.addr).expect("join");
        assert!(linked(&l));
        let state = l.station.dstar_state().expect("session");
        assert!(!state.tx_capable, "the pull backend has no capture side");
    }

    #[cfg(feature = "dstar-soft-ambe")]
    #[test]
    fn dstar_second_join_refused_and_rejoin_after_leave_works() {
        let refl = dstar_reflector(&[*b"ACK\0"]);
        let mut l = software();
        let _feed = join_dstar(&mut l, refl.addr).expect("join");
        assert!(linked(&l));
        assert!(matches!(
            join_dstar(&mut l, refl.addr),
            Err(ListenError::AlreadyJoined)
        ));
        assert!(matches!(
            join_m17(&mut l, refl.addr),
            Err(ListenError::AlreadyJoined)
        ));
        l.leave();
        l.leave();
        let _feed = join_dstar(&mut l, refl.addr).expect("rejoin");
        assert!(linked(&l));
    }

    #[cfg(feature = "dstar-soft-ambe")]
    #[test]
    fn dstar_failed_link_reads_failed_and_rejoin_works() {
        let refl = dstar_reflector(&[*b"NAK\0", *b"ACK\0"]);
        let mut l = software();
        let _feed = join_dstar(&mut l, refl.addr).expect("join");
        assert!(wait_for(
            || matches!(l.status(), LinkStatus::Failed(_)),
            3_000
        ));
        let _feed = join_dstar(&mut l, refl.addr).expect("rejoin after the failure");
        assert!(linked(&l));
    }

    /// A far station on a real in-process reflector, feeding the software
    /// vocoder real packets. Proves the audio comes off the bus at [`rate`].
    #[cfg(feature = "dstar-soft-ambe")]
    #[test]
    fn dstar_feed_gives_20_ms_frames_at_the_engine_rate() {
        let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
        let addr = reflector.local_addr();
        let handle = reflector.run();
        let mut l = software();
        let mut feed = join_dstar(&mut l, addr).expect("join");
        assert!(linked(&l));

        let far = UdpSocket::bind("127.0.0.1:0").expect("bind far station");
        far.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout");
        let mut login = b"N0FAR   ".to_vec();
        login.extend_from_slice(&[b' ', b'A', 0]);
        far.send_to(&login, addr).expect("send login");
        let mut buf = [0u8; 128];
        let (n, _) = far.recv_from(&mut buf).expect("login reply");
        assert!(buf[..n].ends_with(b"ACK\0"));

        let header = RfHeader {
            flags: [0; 3],
            rpt2: *b"XRF757 G",
            rpt1: *b"XRF757 A",
            ur: *b"CQCQCQ  ",
            my: *b"N0FAR   ",
            suffix: *b"    ",
        };
        let stream_id = 0x1313;
        far.send_to(&DsvtPacket::Header { stream_id, header }.encode(), addr)
            .expect("send header");
        let mut frames = Vec::new();
        for k in 0..60u8 {
            let voice = DsvtPacket::Voice {
                stream_id,
                seq: k % 21,
                end: k == 59,
                ambe: [0xA5 ^ k, 0x3C, 0x91, 0x77, 0x2E, 0xC4, 0x58, 0xDA, 0x0F ^ k],
                slow_data: [0; 3],
            };
            far.send_to(&voice.encode(), addr).expect("send voice");
            thread::sleep(Duration::from_millis(20));
            frames.extend(feed.take());
        }
        assert!(wait_for(
            || {
                frames.extend(feed.take());
                frames.len() >= 20
            },
            2_000
        ));
        assert!(frames.iter().all(|f| f.rate == l.rate()));
        assert!(
            frames
                .iter()
                .all(|f| f.samples.len() == (l.rate() / 50) as usize)
        );
        assert_eq!(l.talker().as_deref().map(str::trim), Some("N0FAR"));
        assert_eq!(l.dropped_frames(), 0);
        l.leave();
        handle.shutdown();
    }
}
