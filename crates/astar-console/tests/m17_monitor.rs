// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end tests for the receive-only [`M17Monitor`]. It hears a far
//! talker through a real in-process reflector. It never puts anything on the
//! wire except `CONN`, `PONG` and `DISC`, and it ends with a plain reason
//! when the reflector or the conference goes away.
//!
//! Every socket is on `127.0.0.1:0`. Nothing resolves a name and nothing
//! binds a fixed port. Waits are `wait_until` deadline loops. Each scenario
//! runs at both conference rates a node uses: 8 kHz, and 16 kHz under
//! slin16, where the monitor's rate bridge is in the path.
//!
//! The Codec 2 codec comes from this crate's `codec2-static`
//! dev-dependency, so these tests never need a system libcodec2. The shipped
//! build still links no LGPL code.
#![cfg(feature = "m17")]

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::RxFrame;
use astar_codec::codec2::{Codec2Voice, open_codec2};
use astar_console::{M17Monitor, M17MonitorConfig, SourceSink};
use astar_m17::{
    BROADCAST, ControlPacket, Lsf, Reflector, StreamPacket, decode_callsign, encode_callsign,
};

/// The callsign the monitor presents in `CONN`, `PONG` and `DISC`.
const MON_CALL: &str = "AJ7HR";
/// The far station keying up on the reflector.
const FAR_CALL: &str = "KC1ABC";
/// A second far client on the same module that only listens.
const LISTENER_CALL: &str = "KC1LSN";
/// One stream packet carries 40 ms of audio, so a far talker sends at that pace.
const PACKET_PACE: Duration = Duration::from_millis(40);
/// The monitor clears `receiving` this long after the last voice packet.
const RX_SILENCE: Duration = Duration::from_millis(400);

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

// ---- deadline-polling helper (house style: no single fixed-timeout wait) ----

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    pred()
}

// ---- monitor helpers --------------------------------------------------------

fn monitor_cfg(reflector: SocketAddr) -> M17MonitorConfig {
    M17MonitorConfig {
        host: reflector.ip().to_string(),
        port: reflector.port(),
        module: b'A',
        callsign: MON_CALL.to_string(),
        codec_dirs: Vec::new(),
        keepalive_timeout: Duration::from_secs(30),
    }
}

/// Spawns a monitor whose conference end is a plain channel the test reads.
fn spawn_monitor(cfg: M17MonitorConfig, bus_rate: u32) -> (M17Monitor, Receiver<RxFrame>) {
    let (to_conference, from_monitor) = mpsc::channel();
    let mon = M17Monitor::spawn(
        cfg,
        SourceSink {
            to_conference,
            bus_rate,
        },
    )
    .expect("a valid config spawns the monitor");
    (mon, from_monitor)
}

fn drain(from_monitor: &Receiver<RxFrame>, into: &mut Vec<RxFrame>) {
    while let Ok(frame) = from_monitor.try_recv() {
        into.push(frame);
    }
}

/// 20 ms of audio at the conference rate: 160 samples at 8 kHz, 320 at 16 kHz.
fn frame_len(bus_rate: u32) -> usize {
    usize::try_from(bus_rate / 50).expect("a conference frame length fits usize")
}

#[allow(clippy::cast_precision_loss)] // sample count -> f64 for an RMS; exact at these lengths
fn rms(frames: &[RxFrame]) -> f64 {
    let samples: Vec<f64> = frames
        .iter()
        .flat_map(|f| f.pcm.iter().map(|&s| f64::from(s)))
        .collect();
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
}

// ---- far-talker helpers -----------------------------------------------------

/// `count` Codec 2 payloads (two 20 ms chunks each) of a continuous 1 kHz
/// tone at 8 kHz, amplitude 8000 (RMS about 5657).
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)] // test tone synthesis
fn tone_payloads(count: usize) -> Vec<[u8; 16]> {
    let (mut codec, _backend): (Box<dyn Codec2Voice>, _) =
        open_codec2(&[]).expect("the codec2-static dev-dependency provides a codec");
    let mut n = 0usize;
    let mut payloads = Vec::with_capacity(count);
    for _ in 0..count {
        let mut payload = [0u8; 16];
        for half in 0..2 {
            let mut pcm = [0i16; 160];
            for s in &mut pcm {
                let phase = std::f32::consts::TAU * 1_000.0 * n as f32 / 8_000.0;
                *s = (8_000.0 * phase.sin()) as i16;
                n += 1;
            }
            payload[half * 8..half * 8 + 8].copy_from_slice(&codec.encode(&pcm));
        }
        payloads.push(payload);
    }
    payloads
}

/// Packet `index` of a `total`-packet over from `FAR_CALL`. The packet at
/// index `total - 1` carries EOS.
fn voice_packet(stream_id: u16, index: usize, total: usize, payload: [u8; 16]) -> Vec<u8> {
    let mut frame_number = u16::try_from(index).expect("test overs are short");
    if index + 1 == total {
        frame_number |= StreamPacket::EOS_BIT;
    }
    StreamPacket {
        stream_id,
        lsf: Lsf {
            dst: BROADCAST,
            src: encode_callsign(FAR_CALL).expect("valid test callsign"),
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number,
        payload,
    }
    .to_bytes()
    .to_vec()
}

/// Sends `packets` of a `total`-packet tone over, one every 40 ms, through
/// `send`. Returns the instant taken just before the last send, which is no
/// later than the moment the last packet can have reached the monitor.
fn send_over(send: &dyn Fn(&[u8]), stream_id: u16, packets: Range<usize>, total: usize) -> Instant {
    let payloads = tone_payloads(total);
    let first = packets.start;
    let mut last_send = Instant::now();
    for i in packets {
        if i != first {
            thread::sleep(PACKET_PACE);
        }
        last_send = Instant::now();
        send(&voice_packet(stream_id, i, total, payloads[i]));
    }
    last_send
}

/// A raw M17 client linked to module A of `reflector` as `callsign`.
fn link_raw(reflector: SocketAddr, callsign: &str) -> UdpSocket {
    let sock = UdpSocket::bind(loopback()).expect("bind raw client");
    sock.set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set read timeout");
    let conn = ControlPacket::Conn {
        callsign: encode_callsign(callsign).expect("valid test callsign"),
        module: b'A',
    }
    .to_bytes();
    let mut buf = [0u8; 64];
    let acked = wait_until(
        || {
            sock.send_to(&conn, reflector).expect("send CONN");
            matches!(
                sock.recv_from(&mut buf),
                Ok((n, _)) if ControlPacket::parse(&buf[..n]) == Some(ControlPacket::Ackn)
            )
        },
        2_000,
    );
    assert!(acked, "{callsign} must be ACKN'd by the reflector");
    sock
}

// ---- (1) hears a far talker through a real reflector ------------------------

fn hears_a_far_talker(bus_rate: u32) {
    const N: usize = 10;
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let refl = reflector.local_addr();
    let handle = reflector.run();

    let (mon, from_monitor) = spawn_monitor(monitor_cfg(refl), bus_rate);
    assert!(
        wait_until(|| mon.status().link == "linked", 2_000),
        "the monitor must link via CONN/ACKN, got {:?}",
        mon.status()
    );

    let far = link_raw(refl, FAR_CALL);
    let before_last = send_over(
        &|b: &[u8]| {
            far.send_to(b, refl).expect("far talker send");
        },
        0x4D17,
        0..N,
        N,
    );

    assert!(
        wait_until(|| mon.status().receiving, 300),
        "receiving must be true right after the over, got {:?}",
        mon.status()
    );
    assert!(
        wait_until(|| !mon.status().receiving, 2_000),
        "receiving must clear after the over, got {:?}",
        mon.status()
    );
    assert!(
        before_last.elapsed() >= RX_SILENCE,
        "receiving cleared {:?} after the last packet was sent; it must hold for 400 ms",
        before_last.elapsed()
    );

    let mut frames = Vec::new();
    drain(&from_monitor, &mut frames);
    let lens: Vec<usize> = frames.iter().map(|f| f.pcm.len()).collect();
    assert_eq!(
        lens,
        vec![frame_len(bus_rate); 2 * N],
        "every packet must become exactly two conference frames at {bus_rate} Hz"
    );
    let level = rms(&frames);
    assert!(
        level > 100.0,
        "the decoded 1 kHz tone must not be silence (RMS {level}; input RMS is about 5657)"
    );

    let st = mon.status();
    assert_eq!(st.talker.as_deref(), Some(FAR_CALL));
    assert_eq!(
        st.dropped_packets, 0,
        "a paced single stream must not be gated"
    );
    assert_eq!(st.refused_sends, 0);
    let heard: Vec<(String, &str)> = mon
        .heard()
        .into_iter()
        .map(|h| (h.callsign, h.network))
        .collect();
    assert_eq!(heard, vec![(FAR_CALL.to_string(), "m17")]);

    mon.disconnect();
    handle.shutdown();
}

#[test]
fn hears_a_far_talker_at_8k() {
    hears_a_far_talker(8_000);
}

#[test]
fn hears_a_far_talker_at_16k() {
    hears_a_far_talker(16_000);
}

// ---- recording fake reflector -------------------------------------------------

/// How the fake answers the monitor's `CONN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeMode {
    /// ACKN, then PING the monitor every 100 ms.
    AcknAndPing,
    /// ACKN once, then say nothing (the keepalive must end the link).
    AcknThenSilent,
    /// NACK.
    Nack,
}

/// A raw-UDP reflector that records every datagram it receives, in order, so
/// a test can inspect everything the monitor ever sent.
struct FakeReflector {
    addr: SocketAddr,
    sock: UdpSocket,
    log: Arc<Mutex<Vec<Vec<u8>>>>,
    peer: Arc<Mutex<Option<SocketAddr>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeReflector {
    fn start(mode: FakeMode) -> Self {
        let sock = UdpSocket::bind(loopback()).expect("bind fake reflector");
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("set read timeout");
        let addr = sock.local_addr().expect("local addr");
        let log = Arc::new(Mutex::new(Vec::new()));
        let peer = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let sock = sock.try_clone().expect("clone fake socket");
            let (log, peer, stop) = (Arc::clone(&log), Arc::clone(&peer), Arc::clone(&stop));
            thread::spawn(move || fake_loop(&sock, mode, &log, &peer, &stop))
        };
        Self {
            addr,
            sock,
            log,
            peer,
            stop,
            thread: Some(thread),
        }
    }

    /// Everything received so far, in arrival order.
    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.log.lock().expect("log mutex").clone()
    }

    /// Sends `bytes` to the monitor (the address its `CONN` came from).
    fn send_to_peer(&self, bytes: &[u8]) {
        let peer = self
            .peer
            .lock()
            .expect("peer mutex")
            .expect("the monitor has sent CONN");
        self.sock.send_to(bytes, peer).expect("fake reflector send");
    }
}

impl Drop for FakeReflector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn fake_loop(
    sock: &UdpSocket,
    mode: FakeMode,
    log: &Mutex<Vec<Vec<u8>>>,
    peer: &Mutex<Option<SocketAddr>>,
    stop: &AtomicBool,
) {
    let ping = ControlPacket::Ping {
        callsign: encode_callsign("M17-TST").expect("valid reflector callsign"),
    }
    .to_bytes();
    let mut buf = [0u8; 2_048];
    let mut last_ping = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Ok((n, src)) = sock.recv_from(&mut buf) {
            log.lock().expect("log mutex").push(buf[..n].to_vec());
            if matches!(
                ControlPacket::parse(&buf[..n]),
                Some(ControlPacket::Conn { .. })
            ) {
                let mut p = peer.lock().expect("peer mutex");
                if p.is_none() {
                    *p = Some(src);
                    let reply = if mode == FakeMode::Nack {
                        ControlPacket::Nack
                    } else {
                        ControlPacket::Ackn
                    };
                    let _ = sock.send_to(&reply.to_bytes(), src);
                }
            }
        }
        let linked = *peer.lock().expect("peer mutex");
        if mode == FakeMode::AcknAndPing
            && let Some(p) = linked
            && last_ping.elapsed() >= Duration::from_millis(100)
        {
            let _ = sock.send_to(&ping, p);
            last_ping = Instant::now();
        }
    }
}

fn is_pong(d: &[u8]) -> bool {
    matches!(ControlPacket::parse(d), Some(ControlPacket::Pong { .. }))
}

fn is_disc(d: &[u8]) -> bool {
    matches!(ControlPacket::parse(d), Some(ControlPacket::Disc { .. }))
}

/// The only three things a monitor may ever send.
fn is_monitor_control(d: &[u8]) -> bool {
    StreamPacket::parse(d).is_none()
        && matches!(
            ControlPacket::parse(d),
            Some(
                ControlPacket::Conn { .. }
                    | ControlPacket::Pong { .. }
                    | ControlPacket::Disc { .. }
            )
        )
}

fn count(log: &[Vec<u8>], pred: fn(&[u8]) -> bool) -> usize {
    log.iter().filter(|d| pred(d.as_slice())).count()
}

// ---- (2) never transmits (load-bearing) ---------------------------------------

fn never_transmits_anything_but_control(bus_rate: u32) {
    const K: usize = 5;
    let fake = FakeReflector::start(FakeMode::AcknAndPing);
    let (mon, from_monitor) = spawn_monitor(monitor_cfg(fake.addr), bus_rate);
    assert!(
        wait_until(|| mon.status().link == "linked", 2_000),
        "the monitor must link to the fake, got {:?}",
        mon.status()
    );
    assert!(
        wait_until(|| count(&fake.datagrams(), is_pong) >= 2, 2_000),
        "the monitor must answer the fake's 100 ms PINGs"
    );

    // Audio flows while the fake records: the receive path is live, not idle.
    send_over(&|b: &[u8]| fake.send_to_peer(b), 0x5AFE, 0..K, K);
    let mut frames = Vec::new();
    assert!(
        wait_until(
            || {
                drain(&from_monitor, &mut frames);
                frames.len() == 2 * K
            },
            2_000
        ),
        "the scripted over must reach the sink, got {} frames",
        frames.len()
    );
    let st = mon.status();
    assert_eq!(
        st.refused_sends, 0,
        "the monitor must never even try to send a non-control buffer"
    );
    assert_eq!(st.dropped_packets, 0);

    mon.disconnect();
    assert!(
        wait_until(
            || fake
                .datagrams()
                .last()
                .is_some_and(|d| is_disc(d.as_slice())),
            2_000
        ),
        "disconnect() must end with a DISC, got {:?}",
        fake.datagrams()
    );

    let log = fake.datagrams();
    let mon_call = encode_callsign(MON_CALL).expect("valid test callsign");
    let streams = log
        .iter()
        .filter(|d| StreamPacket::parse(d.as_slice()).is_some())
        .count();
    assert_eq!(streams, 0, "the monitor sent a stream packet: {log:?}");
    let stray: Vec<&Vec<u8>> = log
        .iter()
        .filter(|d| !is_monitor_control(d.as_slice()))
        .collect();
    assert_eq!(
        stray,
        Vec::<&Vec<u8>>::new(),
        "every datagram must be CONN, PONG or DISC"
    );
    let odd_sizes: Vec<usize> = log
        .iter()
        .map(Vec::len)
        .filter(|n| *n != 10 && *n != 11)
        .collect();
    assert_eq!(
        odd_sizes,
        Vec::<usize>::new(),
        "only 11-byte CONN and 10-byte PONG/DISC may leave"
    );
    assert_eq!(
        ControlPacket::parse(&log[0]),
        Some(ControlPacket::Conn {
            callsign: mon_call,
            module: b'A'
        })
    );
    assert_eq!(
        log.last().and_then(|d| ControlPacket::parse(d)),
        Some(ControlPacket::Disc {
            callsign: Some(mon_call)
        })
    );
    assert_eq!(count(&log, is_disc), 1, "exactly one DISC: {log:?}");
    let foreign_pongs = log
        .iter()
        .filter(|d| {
            matches!(
                ControlPacket::parse(d.as_slice()),
                Some(ControlPacket::Pong { callsign }) if callsign != mon_call
            )
        })
        .count();
    assert_eq!(foreign_pongs, 0, "every PONG must carry {MON_CALL}");
}

#[test]
fn never_transmits_anything_but_control_at_8k() {
    never_transmits_anything_but_control(8_000);
}

#[test]
fn never_transmits_anything_but_control_at_16k() {
    never_transmits_anything_but_control(16_000);
}

// ---- (3) a second far client on the same module hears only the talker ------

fn listener_never_hears_the_monitor(bus_rate: u32) {
    const N: usize = 5;
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let refl = reflector.local_addr();
    let handle = reflector.run();
    let talker = link_raw(refl, FAR_CALL);
    let listener = link_raw(refl, LISTENER_CALL);
    assert!(
        wait_until(|| handle.client_count() == 2, 2_000),
        "talker and listener must both be linked, got {}",
        handle.client_count()
    );
    let before = handle.client_count();

    let (mon, from_monitor) = spawn_monitor(monitor_cfg(refl), bus_rate);
    assert!(
        wait_until(|| mon.status().link == "linked", 2_000),
        "the monitor must link, got {:?}",
        mon.status()
    );
    assert!(
        wait_until(|| handle.client_count() == before + 1, 2_000),
        "the reflector must count the monitor as a client, got {}",
        handle.client_count()
    );

    send_over(
        &|b: &[u8]| {
            talker.send_to(b, refl).expect("talker send");
        },
        0x7A1C,
        0..N,
        N,
    );
    let mut frames = Vec::new();
    assert!(
        wait_until(
            || {
                drain(&from_monitor, &mut frames);
                frames.len() == 2 * N
            },
            2_000
        ),
        "the monitor must hear the talker, got {} frames",
        frames.len()
    );

    mon.disconnect();
    assert!(
        wait_until(|| handle.client_count() == before, 2_000),
        "the monitor's DISC must return client_count to {before}, got {}",
        handle.client_count()
    );

    // Everything the reflector relayed to the listener over the whole run is
    // still queued on its socket. Any stream packet the monitor had sent would
    // be here too, with the monitor's callsign as its source.
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let mut sources: Vec<String> = Vec::new();
    let mut buf = [0u8; 2_048];
    assert!(
        wait_until(
            || {
                while let Ok((n, _)) = listener.recv_from(&mut buf) {
                    if let Some(pkt) = StreamPacket::parse(&buf[..n]) {
                        sources.push(decode_callsign(&pkt.lsf.src));
                    }
                }
                sources.len() >= N
            },
            2_000
        ),
        "the listener must receive the talker's over, got {sources:?}"
    );
    assert_eq!(
        sources,
        vec![FAR_CALL.to_string(); N],
        "the listener must hear the talker and nothing from {MON_CALL}"
    );
    handle.shutdown();
}

#[test]
fn listener_never_hears_the_monitor_at_8k() {
    listener_never_hears_the_monitor(8_000);
}

#[test]
fn listener_never_hears_the_monitor_at_16k() {
    listener_never_hears_the_monitor(16_000);
}

// ---- (4) end reasons: each leg ends by itself, with a plain reason ----------

fn nack_fails_with_the_refusal_reason(bus_rate: u32) {
    let fake = FakeReflector::start(FakeMode::Nack);
    let (mon, _from_monitor) = spawn_monitor(monitor_cfg(fake.addr), bus_rate);
    assert!(
        wait_until(|| mon.finished(), 2_000),
        "a NACK must end the monitor's thread on its own, got {:?}",
        mon.status()
    );
    let st = mon.status();
    assert_eq!(st.link, "failed");
    assert_eq!(
        st.fail_reason.as_deref(),
        Some("reflector refused the link")
    );
    assert_eq!(st.refused_sends, 0);
    assert!(!mon.sink_lost(), "a NACK is not a lost conference");
    mon.disconnect();
}

fn silence_fails_with_the_no_reply_reason(bus_rate: u32) {
    let fake = FakeReflector::start(FakeMode::AcknThenSilent);
    let mut cfg = monitor_cfg(fake.addr);
    cfg.keepalive_timeout = Duration::from_millis(300);
    let (mon, _from_monitor) = spawn_monitor(cfg, bus_rate);
    assert!(
        wait_until(|| mon.status().link == "linked", 2_000),
        "the monitor must link before the silence starts, got {:?}",
        mon.status()
    );
    assert!(
        wait_until(|| mon.finished(), 3_000),
        "300 ms of silence must end the monitor's thread on its own, got {:?}",
        mon.status()
    );
    let st = mon.status();
    assert_eq!(st.link, "failed");
    assert_eq!(st.fail_reason.as_deref(), Some("no reply for 300 ms"));
    assert!(!mon.sink_lost(), "silence is not a lost conference");
    mon.disconnect();
}

fn dropped_sink_ends_with_conference_gone(bus_rate: u32) {
    const TOTAL: usize = 10;
    let fake = FakeReflector::start(FakeMode::AcknAndPing);
    let (mon, from_monitor) = spawn_monitor(monitor_cfg(fake.addr), bus_rate);
    assert!(
        wait_until(|| mon.status().link == "linked", 2_000),
        "the monitor must link, got {:?}",
        mon.status()
    );

    send_over(&|b: &[u8]| fake.send_to_peer(b), 0x0D0D, 0..3, TOTAL);
    let mut frames = Vec::new();
    assert!(
        wait_until(
            || {
                drain(&from_monitor, &mut frames);
                frames.len() == 6
            },
            2_000
        ),
        "the first three packets must reach the sink, got {} frames",
        frames.len()
    );
    assert!(!mon.sink_lost(), "the sink is still there");

    // The conference goes away in the middle of the over.
    drop(from_monitor);
    send_over(&|b: &[u8]| fake.send_to_peer(b), 0x0D0D, 3..TOTAL, TOTAL);
    assert!(
        wait_until(|| mon.finished(), 2_000),
        "a lost sink must end the monitor's thread on its own, got {:?}",
        mon.status()
    );
    assert!(mon.sink_lost());
    assert_eq!(mon.status().fail_reason.as_deref(), Some("conference gone"));
    mon.disconnect();
}

fn resolve_failure_fails_with_the_resolve_reason(bus_rate: u32) {
    // An interior NUL is refused by std while it builds the C string for the
    // lookup, before getaddrinfo runs: the production resolver runs, and no
    // DNS query ever leaves this machine.
    let host = "m17\0invalid";
    let e = (host, 17_000)
        .to_socket_addrs()
        .expect_err("a host with an interior NUL never resolves");
    assert_eq!(
        e.kind(),
        std::io::ErrorKind::InvalidInput,
        "std must refuse the name locally, not look it up"
    );
    let mut cfg = monitor_cfg(loopback());
    cfg.host = host.to_string();
    cfg.port = 17_000;
    let (mon, _from_monitor) = spawn_monitor(cfg, bus_rate);
    assert!(
        wait_until(|| mon.finished(), 2_000),
        "a resolve failure must end the monitor's thread on its own, got {:?}",
        mon.status()
    );
    let st = mon.status();
    assert_eq!(st.link, "failed");
    assert_eq!(
        st.fail_reason,
        Some(format!("could not resolve {host}: {e}"))
    );
    assert_eq!(st.refused_sends, 0);
    assert!(
        !mon.sink_lost(),
        "a resolve failure is not a lost conference"
    );
    mon.disconnect();
}

#[test]
fn nack_fails_with_the_refusal_reason_at_8k() {
    nack_fails_with_the_refusal_reason(8_000);
}

#[test]
fn nack_fails_with_the_refusal_reason_at_16k() {
    nack_fails_with_the_refusal_reason(16_000);
}

#[test]
fn silence_fails_with_the_no_reply_reason_at_8k() {
    silence_fails_with_the_no_reply_reason(8_000);
}

#[test]
fn silence_fails_with_the_no_reply_reason_at_16k() {
    silence_fails_with_the_no_reply_reason(16_000);
}

#[test]
fn dropped_sink_ends_with_conference_gone_at_8k() {
    dropped_sink_ends_with_conference_gone(8_000);
}

#[test]
fn dropped_sink_ends_with_conference_gone_at_16k() {
    dropped_sink_ends_with_conference_gone(16_000);
}

#[test]
fn resolve_failure_fails_with_the_resolve_reason_at_8k() {
    resolve_failure_fails_with_the_resolve_reason(8_000);
}

#[test]
fn resolve_failure_fails_with_the_resolve_reason_at_16k() {
    resolve_failure_fails_with_the_resolve_reason(16_000);
}
