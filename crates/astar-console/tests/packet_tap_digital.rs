// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The packet log's tap on the digital links, end to end over loopback.
//!
//! Each run links to the protocol crate's real fake reflector or master, so
//! the login and control traffic the tap must refuse really does cross the
//! tapped socket. The link dials a [`Tee`], a small relay in front of the
//! fake. It keeps a copy of everything the link was sent, which is how a test
//! reads the DMR salt, and it can put a datagram on the link's socket from
//! the address the link trusts.
//!
//! The fake isn't started until the tap is on the link's slot, so no reply
//! can beat the tap. No test opens a device: D-Star takes a silent stand-in
//! vocoder, and YSF, NXDN and DMR connect with no audio at all.
#![cfg(all(
    feature = "dstar",
    feature = "m17",
    feature = "ysf",
    feature = "nxdn",
    feature = "dmr"
))]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::{CallAudio, RxFrame};
use astar_codec::ambe::{AmbeBackend, AmbeStream, ChannelFrame};
use astar_console::{
    DmrConfig, DmrLink, DmrNetwork, DstarConfig, DstarSession, M17Config, M17Monitor,
    M17MonitorConfig, M17Session, NxdnLink, SourceSink, Timeslot, YsfLink,
};
use astar_dmr::{CallType, FrameType, Master};
use astar_dstar::{DsvtPacket, NULL_AMBE, RfHeader};
use astar_m17::{BROADCAST, ControlPacket, Lsf, StreamPacket, encode_callsign};
use astar_record::packet_tap::{
    CallMeta, PACKET_TAP_CAPACITY, PacketCallId, PacketEvent, PacketNetwork, PacketSlot,
    packet_tap_channel,
};

const CALLSIGN: &str = "KC0ABC";
const PASSWORD: &str = "canary-password";
const NXDN_TALKGROUP: u16 = 31337;
const DMR_TALKGROUP: u32 = 91;
const DMR_RADIO_ID: u32 = 3_153_591;
const WAIT: Duration = Duration::from_secs(5);

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------------------
// The relay in front of each fake.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TeeShared {
    link: Mutex<Option<SocketAddr>>,
    to_link: Mutex<Vec<Vec<u8>>>,
    stop: AtomicBool,
}

/// Relays between a link and `upstream`, keeping what the link was sent.
struct Tee {
    addr: SocketAddr,
    socket: UdpSocket,
    shared: Arc<TeeShared>,
    thread: Option<JoinHandle<()>>,
}

impl Tee {
    fn start(upstream: SocketAddr) -> Tee {
        let socket = UdpSocket::bind(loopback()).expect("bind tee");
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .expect("tee timeout");
        let addr = socket.local_addr().expect("tee addr");
        let shared = Arc::new(TeeShared::default());
        let thread = {
            let socket = socket.try_clone().expect("clone tee socket");
            let shared = Arc::clone(&shared);
            thread::spawn(move || relay(&socket, upstream, &shared))
        };
        Tee {
            addr,
            socket,
            shared,
            thread: Some(thread),
        }
    }

    /// Send `datagram` to the link from the address it trusts.
    fn inject(&self, datagram: &[u8]) {
        let link = self.shared.link.lock().unwrap().expect("the link dialled");
        self.shared.to_link.lock().unwrap().push(datagram.to_vec());
        self.socket.send_to(datagram, link).expect("inject");
    }

    fn inject_all(&self, datagrams: &[Vec<u8>]) {
        for datagram in datagrams {
            self.inject(datagram);
        }
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.shared.to_link.lock().unwrap().clone()
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn relay(socket: &UdpSocket, upstream: SocketAddr, shared: &TeeShared) {
    let mut buf = [0u8; 2_048];
    while !shared.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == upstream => {
                let link = *shared.link.lock().unwrap();
                if let Some(link) = link {
                    shared.to_link.lock().unwrap().push(buf[..n].to_vec());
                    let _ = socket.send_to(&buf[..n], link);
                }
            }
            Ok((n, from)) => {
                *shared.link.lock().unwrap() = Some(from);
                let _ = socket.send_to(&buf[..n], upstream);
            }
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// What a run leaves behind.
// ---------------------------------------------------------------------------

fn tapped(slot: &PacketSlot) -> Receiver<PacketEvent> {
    let (tap, events) = packet_tap_channel(PACKET_TAP_CAPACITY, PACKET_TAP_CAPACITY);
    slot.set(Some(tap));
    events
}

fn packets_in(events: &[PacketEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            PacketEvent::Packet { bytes, .. } => Some(bytes.to_vec()),
            _ => None,
        })
        .collect()
}

/// Read events until `count` packets have come through.
fn gather(events: &Receiver<PacketEvent>, count: usize) -> Vec<PacketEvent> {
    let mut seen = Vec::new();
    wait_until(&format!("{count} logged packets"), || {
        seen.extend(events.try_iter());
        packets_in(&seen).len() >= count
    });
    seen
}

struct Run {
    call: PacketCallId,
    events: Vec<PacketEvent>,
    /// The media the run put on the link's socket.
    media: Vec<Vec<u8>>,
    /// Everything the link's socket was sent, login and control included.
    sent: Vec<Vec<u8>>,
    tee_addr: SocketAddr,
}

impl Run {
    fn meta(&self) -> &CallMeta {
        match self.events.first() {
            Some(PacketEvent::Start { meta, .. }) => meta,
            other => panic!("a run opens with Start, not {other:?}"),
        }
    }

    fn packets(&self) -> Vec<Vec<u8>> {
        packets_in(&self.events)
    }

    fn was_sent(&self, tag: &[u8]) -> bool {
        self.sent.iter().any(|d| d.starts_with(tag))
    }

    /// The log holds the run's media, in order and nothing else, under the
    /// link's own call id, between one `Start` and one `End`.
    fn assert_logged_only_the_media(&self, network: PacketNetwork) {
        assert_eq!(self.meta().network, network);
        assert_eq!(self.meta().peer, self.tee_addr);
        assert_ne!(self.meta().local.port(), 0);
        assert_eq!(self.packets(), self.media);
        assert!(
            self.sent.len() > self.media.len(),
            "control traffic must really have crossed the socket"
        );
        for event in &self.events {
            match event {
                PacketEvent::Start { call, .. } | PacketEvent::End { call, .. } => {
                    assert_eq!(*call, self.call);
                }
                PacketEvent::Packet { call, from, .. } => {
                    assert_eq!(*call, self.call);
                    assert_eq!(*from, self.tee_addr);
                }
                PacketEvent::Jitter { .. } => panic!("no link samples jitter here"),
            }
        }
        assert!(
            matches!(self.events.last(), Some(PacketEvent::End { .. })),
            "the link's end closes the log"
        );
    }

    fn assert_no_packet_holds(&self, needle: &[u8]) {
        for packet in self.packets() {
            assert!(
                !packet.windows(needle.len()).any(|w| w == needle),
                "a logged packet holds {needle:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Audio ends for the two sessions that need them. No device is involved.
// ---------------------------------------------------------------------------

fn call_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<RxFrame>) {
    let (mic, tx_frames) = channel();
    let (rx_frames, speaker) = channel();
    (
        CallAudio {
            tx_frames,
            rx_frames,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        },
        mic,
        speaker,
    )
}

/// A vocoder that takes everything and says nothing.
struct SilentVocoder;

impl AmbeStream for SilentVocoder {
    fn submit_decode(&mut self, _frame: ChannelFrame) {}
    fn poll_decoded(&mut self) -> Option<[i16; 160]> {
        None
    }
    fn in_flight(&self) -> usize {
        0
    }
    fn submit_encode(&mut self, _pcm: [i16; 160]) {}
    fn poll_encoded(&mut self) -> Option<ChannelFrame> {
        None
    }
    fn in_flight_encoded(&self) -> usize {
        0
    }
}

// ---------------------------------------------------------------------------
// One run per link.
// ---------------------------------------------------------------------------

fn ysf_media(counter: u8) -> Vec<u8> {
    let call = astar_ysf::Callsign::new("W1AW").expect("a legal callsign");
    astar_ysf::wire::data(&astar_ysf::DataPacket {
        gateway: call,
        source: call,
        destination: call,
        counter,
        end: false,
        frame: [counter; astar_ysf::FRAME_LEN],
    })
    .to_vec()
}

fn ysf_run() -> Run {
    let reflector = astar_ysf::Reflector::bind(loopback()).expect("bind reflector");
    let tee = Tee::start(reflector.local_addr());
    let link = YsfLink::connect(&tee.addr.to_string(), CALLSIGN, None).expect("connect");
    let events = tapped(&link.packet_slot());
    let handle = reflector.run();
    wait_until("the YSF link", || link.snapshot().link_state == "linked");

    let call = astar_ysf::Callsign::new("W1AW").expect("a legal callsign");
    tee.inject(&astar_ysf::wire::poll(&call));
    let media: Vec<_> = (0..3).map(ysf_media).collect();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = link.packet_call();
    link.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

fn nxdn_media(fill: u8) -> Vec<u8> {
    astar_nxdn::wire::data(&astar_nxdn::DataPacket {
        src_id: 4242,
        dst_id: NXDN_TALKGROUP,
        group: true,
        data: false,
        start: false,
        end: false,
        frame: [fill; astar_nxdn::FRAME_LEN],
    })
    .to_vec()
}

fn nxdn_run() -> Run {
    let reflector =
        astar_nxdn::Reflector::bind(loopback(), NXDN_TALKGROUP).expect("bind reflector");
    let tee = Tee::start(reflector.local_addr());
    let link =
        NxdnLink::connect(&tee.addr.to_string(), CALLSIGN, 4243, NXDN_TALKGROUP).expect("connect");
    let events = tapped(&link.packet_slot());
    let handle = reflector.run();
    wait_until("the NXDN link", || link.snapshot().link_state == "linked");

    let call = astar_nxdn::Callsign::new("W1AW").expect("a legal callsign");
    tee.inject(&astar_nxdn::wire::poll(&call, NXDN_TALKGROUP));
    let media: Vec<_> = (1..4).map(nxdn_media).collect();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = link.packet_call();
    link.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

fn dmr_cfg(addr: SocketAddr, password: &str) -> DmrConfig {
    DmrConfig {
        system: "freedmr-network".into(),
        family: Some(DmrNetwork::FreeDmr),
        host: addr.ip().to_string(),
        port: addr.port(),
        radio_id: DMR_RADIO_ID,
        callsign: CALLSIGN.into(),
        talkgroup: DMR_TALKGROUP,
        timeslot: Timeslot::Ts2,
        password: password.into(),
    }
}

fn dmr_media(seq: u8) -> Vec<u8> {
    astar_dmr::wire::data(&astar_dmr::DataPacket {
        seq,
        src_id: 4242,
        dst_id: DMR_TALKGROUP,
        peer_id: 4242,
        slot: Timeslot::Ts2,
        call_type: CallType::Group,
        frame_type: FrameType::Voice { n: seq },
        stream_id: [1, 2, 3, 4],
        burst: [seq + 1; astar_dmr::BURST_LEN],
        ber: 0,
        rssi: 0,
    })
    .expect("a legal burst")
    .to_vec()
}

fn dmr_run() -> Run {
    let master = Master::bind(loopback(), PASSWORD).expect("bind master");
    let tee = Tee::start(master.local_addr());
    let link = DmrLink::connect(dmr_cfg(tee.addr, PASSWORD)).expect("connect");
    let events = tapped(&link.packet_slot());
    let handle = master.run();
    wait_until("the DMR link", || link.snapshot().link_state == "linked");

    let mut pong = b"MSTPONG".to_vec();
    pong.extend_from_slice(&DMR_RADIO_ID.to_be_bytes());
    tee.inject(&pong);
    let media: Vec<_> = (0..3).map(dmr_media).collect();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = link.packet_call();
    link.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

fn dstar_media() -> Vec<Vec<u8>> {
    let header = RfHeader {
        flags: [0, 0, 0],
        rpt2: *b"XRF757 G",
        rpt1: *b"XRF757 A",
        ur: *b"CQCQCQ  ",
        my: *b"W1AW    ",
        suffix: *b"    ",
    };
    let voice = |seq| DsvtPacket::Voice {
        stream_id: 0x1234,
        seq,
        end: false,
        ambe: NULL_AMBE,
        slow_data: [0; 3],
    };
    vec![
        DsvtPacket::Header {
            stream_id: 0x1234,
            header,
        }
        .encode(),
        voice(0).encode(),
        voice(1).encode(),
    ]
}

fn dstar_run(reflector_callsign: Option<&str>) -> Run {
    let reflector = astar_dstar::Reflector::bind(loopback()).expect("bind reflector");
    let tee = Tee::start(reflector.local_addr());
    let (audio, _mic, _speaker) = call_audio();
    let cfg = DstarConfig {
        host: tee.addr.ip().to_string(),
        port: tee.addr.port(),
        module: b'A',
        callsign: CALLSIGN.into(),
        reflector_callsign: reflector_callsign.map(str::to_string),
    };
    let session = DstarSession::connect_with_stream(
        cfg,
        audio,
        Box::new(SilentVocoder),
        AmbeBackend::Hardware,
    )
    .expect("connect");
    let events = tapped(&session.packet_slot());
    let handle = reflector.run();
    wait_until("the D-Star link", || {
        session.state().link == astar_dstar::LinkState::Linked
    });

    tee.inject(b"XRF757  \0");
    let media = dstar_media();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = session.packet_call();
    session.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

fn m17_media() -> Vec<Vec<u8>> {
    let lsf = Lsf {
        dst: BROADCAST,
        src: encode_callsign("W1AW").expect("a legal callsign"),
        type_field: Lsf::TYPE_VOICE_3200_STREAM,
        meta: [0; 14],
    };
    (0..3)
        .map(|n: u8| {
            StreamPacket {
                stream_id: 0x1234,
                lsf,
                frame_number: u16::from(n),
                payload: [n; 16],
            }
            .to_bytes()
            .to_vec()
        })
        .collect()
}

fn m17_ping() -> Vec<u8> {
    ControlPacket::Ping {
        callsign: encode_callsign("M17-KCW").expect("a legal callsign"),
    }
    .to_bytes()
}

fn m17_session_run() -> Run {
    let reflector = astar_m17::Reflector::bind(loopback()).expect("bind reflector");
    let tee = Tee::start(reflector.local_addr());
    let (audio, _mic, _speaker) = call_audio();
    let cfg = M17Config {
        host: tee.addr.ip().to_string(),
        port: tee.addr.port(),
        module: b'A',
        callsign: CALLSIGN.into(),
        codec_dirs: Vec::new(),
        keepalive_timeout: Duration::from_secs(30),
    };
    let session = M17Session::connect(cfg, audio).expect("connect");
    let events = tapped(&session.packet_slot());
    let handle = reflector.run();
    wait_until("the M17 link", || {
        session.state().link == astar_m17::LinkState::Linked
    });

    tee.inject(&m17_ping());
    let media = m17_media();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = session.packet_call();
    session.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

fn m17_monitor_run() -> Run {
    let reflector = astar_m17::Reflector::bind(loopback()).expect("bind reflector");
    let tee = Tee::start(reflector.local_addr());
    let (to_conference, _conference) = channel();
    let cfg = M17MonitorConfig {
        host: tee.addr.ip().to_string(),
        port: tee.addr.port(),
        module: b'a',
        callsign: CALLSIGN.into(),
        codec_dirs: Vec::new(),
        keepalive_timeout: Duration::from_secs(30),
    };
    let monitor = M17Monitor::spawn(
        cfg,
        SourceSink {
            to_conference,
            bus_rate: 8_000,
        },
    )
    .expect("spawn");
    let events = tapped(&monitor.packet_slot());
    let handle = reflector.run();
    wait_until("the M17 monitor", || monitor.status().link == "linked");

    tee.inject(&m17_ping());
    let media = m17_media();
    tee.inject_all(&media);

    let mut seen = gather(&events, media.len());
    let packet_call = monitor.packet_call();
    monitor.disconnect();
    seen.extend(events.try_iter());
    handle.shutdown();
    Run {
        call: packet_call,
        events: seen,
        media,
        sent: tee.sent(),
        tee_addr: tee.addr,
    }
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

#[test]
fn dmr_logs_data_and_never_the_login() {
    let run = dmr_run();
    run.assert_logged_only_the_media(PacketNetwork::Dmr);

    // RPTL, RPTK and RPTC each drew an RPTACK, and the first one held the salt.
    let acks: Vec<_> = run
        .sent
        .iter()
        .filter(|d| d.starts_with(b"RPTACK"))
        .collect();
    assert!(acks.len() >= 3, "the whole login chain ran: {acks:?}");
    let salt = &acks[0][6..10];
    assert!(run.was_sent(b"MSTPONG"));

    assert!(run.packets().iter().all(|p| p.starts_with(b"DMRD")));
    run.assert_no_packet_holds(b"RPTACK");
    run.assert_no_packet_holds(b"MSTPONG");
    run.assert_no_packet_holds(salt);
    run.assert_no_packet_holds(PASSWORD.as_bytes());
}

#[test]
fn a_wrong_password_logs_nothing() {
    let master = Master::bind(loopback(), PASSWORD).expect("bind master");
    let tee = Tee::start(master.local_addr());
    let link = DmrLink::connect(dmr_cfg(tee.addr, "not-the-password")).expect("connect");
    let events = tapped(&link.packet_slot());
    let handle = master.run();
    wait_until("the refusal", || link.snapshot().link_state == "failed");
    link.disconnect();
    handle.shutdown();

    let sent = tee.sent();
    assert!(sent.iter().any(|d| d.starts_with(b"RPTACK")));
    assert!(sent.iter().any(|d| d.starts_with(b"MSTNAK")));
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn ysf_logs_data_and_never_the_poll() {
    let run = ysf_run();
    run.assert_logged_only_the_media(PacketNetwork::Ysf);
    assert!(run.was_sent(b"YSFP"));
    run.assert_no_packet_holds(b"YSFP");
}

#[test]
fn nxdn_logs_data_and_never_the_poll() {
    let run = nxdn_run();
    run.assert_logged_only_the_media(PacketNetwork::Nxdn);
    assert!(run.was_sent(b"NXDNP"));
    run.assert_no_packet_holds(b"NXDNP");
}

#[test]
fn dstar_logs_dsvt_and_never_the_link_traffic() {
    let run = dstar_run(Some("XRF757"));
    run.assert_logged_only_the_media(PacketNetwork::DStar);
    assert!(run.sent.iter().any(|d| !d.starts_with(b"DSVT")));
    assert!(run.packets().iter().all(|p| p.starts_with(b"DSVT")));
}

#[test]
fn m17_session_logs_stream_packets_and_never_control() {
    let run = m17_session_run();
    run.assert_logged_only_the_media(PacketNetwork::M17);
    assert!(run.was_sent(b"ACKN"));
    assert!(run.was_sent(b"PING"));
    assert!(run.packets().iter().all(|p| p.starts_with(b"M17 ")));
}

#[test]
fn m17_monitor_logs_stream_packets_and_never_control() {
    let run = m17_monitor_run();
    run.assert_logged_only_the_media(PacketNetwork::M17);
    assert!(run.was_sent(b"ACKN"));
    assert!(run.was_sent(b"PING"));
    assert!(run.packets().iter().all(|p| p.starts_with(b"M17 ")));
}

#[test]
fn a_stranger_is_not_logged() {
    let fake = UdpSocket::bind(loopback()).expect("bind fake reflector");
    fake.set_read_timeout(Some(WAIT)).expect("timeout");
    let link = YsfLink::connect(
        &fake.local_addr().expect("addr").to_string(),
        CALLSIGN,
        None,
    )
    .expect("connect");
    let events = tapped(&link.packet_slot());
    let mut buf = [0u8; 256];
    let (_, link_addr) = fake.recv_from(&mut buf).expect("the link's poll");

    let stranger = UdpSocket::bind(loopback()).expect("bind stranger");
    stranger
        .send_to(&ysf_media(1), link_addr)
        .expect("stranger sends");
    let real = ysf_media(2);
    fake.send_to(&real, link_addr).expect("reflector sends");

    let seen = gather(&events, 1);
    assert_eq!(packets_in(&seen), vec![real]);
}

#[test]
fn labels_name_the_network_and_target() {
    let label = |run: Run| run.meta().label.clone();
    assert_eq!(label(dstar_run(Some("XRF757"))), "D-Star XRF757 A");
    assert_eq!(label(dstar_run(None)), "D-Star 127.0.0.1 A");
    assert_eq!(label(m17_session_run()), "M17 127.0.0.1 A");
    assert_eq!(label(m17_monitor_run()), "M17 127.0.0.1 A");
    assert_eq!(label(ysf_run()), "Fusion 127.0.0.1");
    assert_eq!(label(nxdn_run()), "NXDN 127.0.0.1 31337");
    assert_eq!(label(dmr_run()), "DMR freedmr-network 91");
}
