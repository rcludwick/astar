// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 through [`ListenOnlyStation`], with every socket on 127.0.0.1.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_m17::{
    BROADCAST, ControlPacket, Lsf, Reflector, ReflectorHandle, StreamPacket, encode_callsign,
};
use astar_netarchive::{Ended, LogKind};
use astar_netrec::config::{NetCfg, Target};
use astar_netrec::feed::{AudioFeed, Frame};
use astar_netrec::listen::{LinkStatus, ListenOnlyStation, Network, login_callsign};
use astar_station::DstarVocoder;

mod common;
use common::{SILENCE_RMS, Scratch, assert_two_talkers, net, record, rms, tone_frame};

/// ACKNs every CONN, PINGs the client, and records every datagram. Being a
/// raw socket it sees exactly what the daemon put on the wire.
struct RecordingReflector {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RecordingReflector {
    fn start() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind recording reflector");
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("read timeout");
        let addr = sock.local_addr().expect("recorder addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t_seen, t_stop) = (Arc::clone(&seen), Arc::clone(&stop));
        let thread = thread::spawn(move || {
            let ping = ControlPacket::Ping {
                callsign: encode_callsign("M17-REC").expect("reflector callsign encodes"),
            }
            .to_bytes();
            let mut buf = [0u8; 2_048];
            let mut client: Option<SocketAddr> = None;
            let mut last_ping = Instant::now();
            while !t_stop.load(Ordering::Acquire) {
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    let datagram = buf[..n].to_vec();
                    if matches!(
                        ControlPacket::parse(&datagram),
                        Some(ControlPacket::Conn { .. })
                    ) {
                        client = Some(from);
                        let _ = sock.send_to(&ControlPacket::Ackn.to_bytes(), from);
                    }
                    t_seen.lock().expect("recorder lock").push(datagram);
                }
                if let Some(to) = client
                    && last_ping.elapsed() >= Duration::from_millis(100)
                {
                    let _ = sock.send_to(&ping, to);
                    last_ping = Instant::now();
                }
            }
        });
        Self {
            addr,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.seen.lock().expect("recorder lock").clone()
    }
}

impl Drop for RecordingReflector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wait_for(pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    common::wait_for(pred, Duration::from_millis(timeout_ms))
}

#[test]
fn m17_monitor_sends_no_voice() {
    let refl = RecordingReflector::start();
    let mut listener = ListenOnlyStation::new(DstarVocoder::Software, &[]).expect("station");
    let call = login_callsign(Network::M17, "N0CALL", 'R').expect("login");
    let mut feed = listener
        .join_m17("127.0.0.1", refl.addr.port(), 'A', &call)
        .expect("join");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert_eq!(feed.take(), None, "nobody is talking");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(listener.status(), LinkStatus::Linked);
    listener.leave();

    let seen = refl.datagrams();
    assert!(
        seen.iter()
            .any(|d| matches!(ControlPacket::parse(d), Some(ControlPacket::Conn { .. }))),
        "the monitor logged in"
    );
    for datagram in &seen {
        assert!(
            StreamPacket::parse(datagram).is_none(),
            "a stream packet left the daemon: {datagram:?}"
        );
    }
}

fn far_client(reflector: SocketAddr, callsign: &str) -> UdpSocket {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind far client");
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .expect("read timeout");
    let conn = ControlPacket::Conn {
        callsign: encode_callsign(callsign).expect("far callsign encodes"),
        module: b'A',
    }
    .to_bytes();
    sock.send_to(&conn, reflector).expect("send CONN");
    let mut buf = [0u8; 64];
    let acked = wait_for(
        || {
            matches!(
                sock.recv_from(&mut buf)
                    .ok()
                    .map(|(n, _)| ControlPacket::parse(&buf[..n])),
                Some(Some(ControlPacket::Ackn))
            )
        },
        2_000,
    );
    assert!(acked, "far client linked to the reflector");
    sock
}

/// Real-time pacing, as the monitor's flood gate expects. Drains the feed as
/// it goes, one tick per 20 ms frame in each packet.
fn stream_tone(
    sock: &UdpSocket,
    to: SocketAddr,
    callsign: &str,
    feed: &mut dyn AudioFeed,
    got: &mut Vec<Frame>,
) {
    let (mut codec, _) = astar_codec::codec2::open_codec2(&[]).expect("Codec 2");
    let src = encode_callsign(callsign).expect("far callsign encodes");
    let packets = 50u16;
    let mut n = 0u32;
    for k in 0..packets {
        let first = codec.encode(&tone_frame(&mut n));
        let second = codec.encode(&tone_frame(&mut n));
        let mut payload = [0u8; 16];
        payload[..8].copy_from_slice(&first);
        payload[8..].copy_from_slice(&second);
        let frame_number = if k + 1 == packets {
            k | StreamPacket::EOS_BIT
        } else {
            k
        };
        let packet = StreamPacket {
            stream_id: 0x4D17,
            lsf: Lsf {
                dst: BROADCAST,
                src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload,
        };
        sock.send_to(&packet.to_bytes(), to)
            .expect("send stream packet");
        for _ in 0..2 {
            thread::sleep(Duration::from_millis(20));
            got.extend(feed.take());
        }
    }
}

/// The real reflector and the real conference: frames appear at the engine
/// rate while someone talks, and stop when they stop.
#[test]
fn m17_feed_gives_20_ms_frames_only_while_someone_talks() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let mut listener = ListenOnlyStation::new(DstarVocoder::Software, &[]).expect("station");
    let call = login_callsign(Network::M17, "N0CALL", 'R').expect("login");
    let mut feed = listener
        .join_m17("127.0.0.1", addr.port(), 'A', &call)
        .expect("join");
    assert!(wait_for(|| listener.status() == LinkStatus::Linked, 3_000));

    for _ in 0..25 {
        assert_eq!(feed.take(), None, "no frame before anyone talks");
        thread::sleep(Duration::from_millis(20));
    }

    let far = far_client(addr, "N0FAR");
    let mut got = Vec::new();
    stream_tone(&far, addr, "N0FAR", feed.as_mut(), &mut got);

    assert!(got.len() > 40, "only {} frames arrived", got.len());
    assert!(got.iter().all(|f| f.rate == listener.rate()));
    let want = (listener.rate() / 50) as usize;
    assert!(got.iter().all(|f| f.samples.len() == want));
    assert!(
        got.iter().any(|f| f.samples.iter().any(|&s| s.abs() > 500)),
        "the frames carry the tone, not silence"
    );
    assert_eq!(listener.talker().as_deref(), Some("N0FAR"));

    // Past the monitor's 400 ms receive window the feed goes quiet again.
    thread::sleep(Duration::from_millis(600));
    while feed.take().is_some() {}
    thread::sleep(Duration::from_millis(100));
    assert_eq!(feed.take(), None);

    listener.leave();
    handle.shutdown();
}

// ---- whole sessions ----

fn m17_net(addr: SocketAddr) -> NetCfg {
    net(Target::M17 {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        module: 'A',
    })
}

/// Talk as `callsign` for `secs` seconds of a 1 kHz tone, paced to real time
/// against the clock so a slow sleep does not stretch the over.
fn talk(sock: &UdpSocket, to: SocketAddr, callsign: &str, stream_id: u16, secs: u16) {
    let (mut codec, _) = astar_codec::codec2::open_codec2(&[]).expect("Codec 2");
    let src = encode_callsign(callsign).expect("far callsign encodes");
    let packets = secs * 25;
    let began = Instant::now();
    let mut n = 0u32;
    for k in 0..packets {
        let mut payload = [0u8; 16];
        payload[..8].copy_from_slice(&codec.encode(&tone_frame(&mut n)));
        payload[8..].copy_from_slice(&codec.encode(&tone_frame(&mut n)));
        let frame_number = if k + 1 == packets {
            k | StreamPacket::EOS_BIT
        } else {
            k
        };
        let packet = StreamPacket {
            stream_id,
            lsf: Lsf {
                dst: BROADCAST,
                src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload,
        };
        sock.send_to(&packet.to_bytes(), to)
            .expect("send stream packet");
        let due = began + Duration::from_millis(40 * (u64::from(k) + 1));
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}

#[test]
fn m17_two_talkers_end_to_end() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback")).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();
    let scratch = Scratch::new("m17-two");

    let run = record(scratch.path(), &m17_net(addr), 20, |session| {
        assert!(
            wait_for(|| handle.client_count() == 1, 1_500),
            "the recorder linked"
        );
        let control = far_client(addr, "NC0NTL");
        let guest = far_client(addr, "KX0AAA");
        session.at(2.0);
        talk(&control, addr, "NC0NTL", 0x0001, 3);
        session.at(7.0);
        talk(&guest, addr, "KX0AAA", 0x0002, 2);
    });
    handle.shutdown();

    assert_eq!(
        run.count(|k| matches!(k, LogKind::Joined)),
        1,
        "one join: {:?}",
        run.rows
    );
    assert_eq!(run.count(|k| matches!(k, LogKind::Dropped { .. })), 0);
    assert_two_talkers(&run, "NC0NTL", "KX0AAA");
}

/// Bind the reflector's old port again. The old socket may take a moment to
/// let go.
fn rebind(addr: SocketAddr) -> Reflector {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match Reflector::bind(addr) {
            Ok(reflector) => return reflector,
            Err(e) if Instant::now() >= deadline => panic!("rebind {addr}: {e}"),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A running reflector that is shut down when this drops, so a panicking test
/// cannot leak its thread.
struct Running(Option<ReflectorHandle>);

impl Running {
    fn client_count(&self) -> usize {
        self.0.as_ref().map_or(0, ReflectorHandle::client_count)
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.shutdown();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The monitor sees a dead reflector by its silence, which takes the
/// production 30 s keepalive window, so this test runs about 40 s.
#[test]
fn m17_drop_and_rejoin() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().expect("loopback")).expect("bind");
    let addr = reflector.local_addr();
    let mut handle = Running(Some(reflector.run()));
    let scratch = Scratch::new("m17-drop");

    // The window is only a backstop: the script stops the session itself once
    // the second over is in. It runs 35 s past the longest rejoin wait (45 s
    // from 8 s), so a slow runner cannot push the over past the end.
    let run = record(scratch.path(), &m17_net(addr), 90, |session| {
        assert!(
            wait_for(|| handle.client_count() == 1, 1_500),
            "the recorder linked"
        );
        let before = far_client(addr, "NC0NTL");
        session.at(2.0);
        talk(&before, addr, "NC0NTL", 0x0001, 1);

        session.at(5.0);
        handle.shutdown();
        session.at(8.0);
        let mut back = Running(Some(rebind(addr).run()));

        assert!(
            wait_for(|| back.client_count() == 1, 45_000),
            "the recorder rejoined the new reflector"
        );
        let after = far_client(addr, "KX0AAA");
        talk(&after, addr, "KX0AAA", 0x0002, 2);
        // The splitter's tail, then a little for the queue to drain.
        thread::sleep(Duration::from_millis(1_800));
        session.stop();
        back.shutdown();
    });

    assert!(
        matches!(run.meta.ended, Some(Ended::Stopped)),
        "ended {:?}",
        run.meta.ended
    );
    assert_eq!(
        run.count(|k| matches!(k, LogKind::Dropped { .. })),
        1,
        "one drop: {:?}",
        run.rows
    );
    assert_eq!(
        run.count(|k| matches!(k, LogKind::Rejoined)),
        1,
        "one rejoin: {:?}",
        run.rows
    );
    let order: Vec<&str> = run
        .rows
        .iter()
        .map(|row| match row.kind {
            LogKind::Joined => "joined",
            LogKind::Over { .. } => "over",
            LogKind::Dropped { .. } => "dropped",
            LogKind::Rejoined => "rejoined",
            LogKind::Left => "left",
            LogKind::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        order,
        ["joined", "over", "dropped", "rejoined", "over", "left"],
        "rows: {:?}",
        run.rows
    );
    assert_eq!(run.meta.rejoins, 1);
    assert_eq!(run.talkers(), ["NC0NTL", "KX0AAA"], "rows: {:?}", run.rows);
    assert_eq!(run.checkins(), "NC0NTL\nKX0AAA\n");
    for over in run.overs() {
        let level = rms(&run.over_samples(&over));
        assert!(level > SILENCE_RMS, "{over:?} is silent: rms {level}");
    }

    // The file kept wall-clock time through the outage: the talker after the
    // rejoin sits at the right place in it, and it runs to the session's end.
    let began: jiff::Timestamp = run.meta.started_at.parse().expect("started_at");
    let ended: jiff::Timestamp = run
        .meta
        .ended_at
        .as_deref()
        .expect("ended_at")
        .parse()
        .expect("ended_at");
    let ran_ms = ended.duration_since(began).as_millis();
    let whole = run.session_samples();
    let file_ms = i128::from(whole.len() as u64 * 1000 / u64::from(run.rate));
    assert!(
        (file_ms - ran_ms).abs() <= 100,
        "session.flac runs {file_ms} ms of a {ran_ms} ms session"
    );
    let after = &run.overs()[1];
    let voice = run.slice(&whole, after.offset_ms, after.offset_ms + 1_500);
    assert!(
        rms(voice) > SILENCE_RMS,
        "session.flac is silent under {after:?}"
    );
}
