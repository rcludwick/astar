// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A whole D-Star session against the in-process reflector, decoded by the
//! software vocoder. Runs under `--features release`.
#![cfg(feature = "dstar-soft-ambe")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_codec::ambe::{AmbeBackend, open_ambe};
use astar_codec::vocoder_trust::VocoderTrust;
use astar_dstar::{DsvtPacket, Reflector, RfHeader};
use astar_netarchive::LogKind;
use astar_netrec::listen::{LinkStatus, ListenOnlyStation, Network, login_callsign};
use astar_station::DstarVocoder;

mod common;
mod dstar_common;
use common::{SILENCE_RMS, Scratch, assert_two_talkers, record, rms, tone_frame, wait_for};
use dstar_common::{Talker, dstar_net, loopback, recorder_linked};

#[test]
fn dstar_two_talkers_end_to_end() {
    let reflector = Reflector::bind(loopback()).expect("bind");
    let addr = reflector.local_addr();
    let handle = reflector.run();
    let scratch = Scratch::new("dstar-two");

    let run = record(scratch.path(), &dstar_net(addr), 20, |session| {
        recorder_linked(&handle);
        let control = Talker::link(addr, "NC0NTL");
        let guest = Talker::link(addr, "KX0AAA");
        session.at(2.0);
        control.talk(0x0001, "NC0NTL", 3);
        session.at(7.0);
        guest.talk(0x0002, "KX0AAA", 2);
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

/// The reflector goes away mid-net and comes back on the same port. It comes
/// back knowing nobody, so the client hears nothing until its 30 s link timeout
/// runs out, which is what sets the length of this test.
#[test]
fn dstar_drop_and_rejoin_keeps_recording() {
    const WINDOW: i64 = 45;
    let first = Reflector::bind(loopback()).expect("bind");
    let addr = first.local_addr();
    let first = first.run();
    let scratch = Scratch::new("dstar-rejoin");

    let run = record(scratch.path(), &dstar_net(addr), WINDOW, |session| {
        recorder_linked(&first);
        let control = Talker::link(addr, "NC0NTL");
        session.at(1.0);
        control.talk(0x0001, "NC0NTL", 2);
        session.at(4.0);
        first.shutdown();
        session.at(6.0);
        let second = Reflector::bind(addr).expect("bind the same port").run();
        assert!(
            wait_for(|| second.client_count() == 1, Duration::from_secs(34)),
            "the recorder linked again"
        );
        let guest = Talker::link(addr, "KX0AAA");
        guest.talk(0x0002, "KX0AAA", 2);
        // Held open so the session's tail still has a reflector to leave.
        session.at(44.0);
        second.shutdown();
    });

    assert_eq!(
        run.count(|k| matches!(k, LogKind::Joined)),
        1,
        "{:?}",
        run.rows
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
    assert_eq!(run.meta.rejoins, 1);
    assert_eq!(run.errors(), Vec::<String>::new(), "no error rows");
    assert_eq!(run.talkers(), ["NC0NTL", "KX0AAA"], "rows: {:?}", run.rows);

    let rejoined = run
        .rows
        .iter()
        .position(|r| matches!(r.kind, LogKind::Rejoined))
        .expect("a rejoined row");
    let last_over = run
        .rows
        .iter()
        .rposition(|r| matches!(r.kind, LogKind::Over { .. }))
        .expect("an over row");
    assert!(
        rejoined < last_over,
        "voice after the rejoin: {:?}",
        run.rows
    );

    // The voice after the rejoin is on the timeline, and the outage is silence.
    let whole = run.session_samples();
    let want = usize::try_from(WINDOW).expect("a test-sized window") * run.rate as usize;
    let slack = run.rate as usize / 10;
    assert!(
        whole.len().abs_diff(want) <= slack,
        "session.flac is {} samples, wanted {want}",
        whole.len()
    );
    let overs = run.overs();
    let voice = run.slice(&whole, overs[1].offset_ms, overs[1].offset_ms + 1_500);
    assert!(rms(voice) > SILENCE_RMS, "silent after the rejoin");
    assert!(
        rms(run.slice(&whole, 5_000, 30_000)) < 1.0,
        "the outage holds sound"
    );
    let level = rms(&run.over_samples(&overs[1]));
    assert!(
        level > SILENCE_RMS,
        "the second over is silent: rms {level}"
    );
}

/// Answers every `DExtra` link request with `ACK`, pings the client the way a
/// reflector does, and records every datagram. Being a raw socket it sees
/// exactly what the daemon put on the wire.
struct RecordingReflector {
    sock: UdpSocket,
    addr: SocketAddr,
    client: Arc<Mutex<Option<SocketAddr>>>,
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
        let client = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t_sock, t_client, t_seen, t_stop) = (
            sock.try_clone().expect("clone socket"),
            Arc::clone(&client),
            Arc::clone(&seen),
            Arc::clone(&stop),
        );
        let thread = thread::spawn(move || {
            let mut ping = [0u8; 9];
            ping[..8].copy_from_slice(b"DSTR-REF");
            let mut buf = [0u8; 2_048];
            let mut last_ping = Instant::now();
            while !t_stop.load(Ordering::Acquire) {
                if let Ok((n, from)) = t_sock.recv_from(&mut buf) {
                    let datagram = buf[..n].to_vec();
                    // An 11-byte request with a module to link to is a connect.
                    if n == 11 && buf[9] != b' ' {
                        let mut ack = [0u8; 14];
                        ack[..8].copy_from_slice(b"DSTR-REF");
                        ack[8] = buf[8];
                        ack[9] = buf[9];
                        ack[10..].copy_from_slice(b"ACK\0");
                        let _ = t_sock.send_to(&ack, from);
                        *t_client.lock().expect("client lock") = Some(from);
                    }
                    t_seen.lock().expect("recorder lock").push(datagram);
                }
                let to = *t_client.lock().expect("client lock");
                if let Some(to) = to
                    && last_ping.elapsed() >= Duration::from_millis(500)
                {
                    let _ = t_sock.send_to(&ping, to);
                    last_ping = Instant::now();
                }
            }
        });
        Self {
            sock,
            addr,
            client,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.seen.lock().expect("recorder lock").clone()
    }

    /// One second of a far talker, sent to the linked client.
    fn far_talker_over(&self) {
        let to = self
            .client
            .lock()
            .expect("client lock")
            .expect("the client linked");
        let (mut vocoder, _) = open_ambe(Some(AmbeBackend::Software), &VocoderTrust::none())
            .expect("the software vocoder");
        let send = |packet: &DsvtPacket| {
            self.sock.send_to(&packet.encode(), to).expect("send");
        };
        send(&DsvtPacket::Header {
            stream_id: 0x0001,
            header: RfHeader {
                flags: [0x00, 0x00, 0x00],
                rpt2: *b"XRF757 G",
                rpt1: *b"XRF757 A",
                ur: *b"CQCQCQ  ",
                my: *b"N0FAR   ",
                suffix: *b"    ",
            },
        });
        let mut n = 0u32;
        for k in 0..50u8 {
            send(&DsvtPacket::Voice {
                stream_id: 0x0001,
                seq: k % 21,
                end: k == 49,
                ambe: vocoder.encode(&tone_frame(&mut n)),
                slow_data: [0x00, 0x00, 0x00],
            });
            thread::sleep(Duration::from_millis(20));
        }
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

/// The daemon is receive-only: against a raw-socket reflector, with a far
/// talker heard, everything it sends is link or keepalive traffic.
#[test]
fn dstar_client_sends_no_voice() {
    let refl = RecordingReflector::start();
    let mut listener = ListenOnlyStation::new(DstarVocoder::Software, &[]).expect("station");
    let call = login_callsign(Network::Dstar, "N0CALL", 'R').expect("login");
    let mut feed = listener
        .join_dstar("127.0.0.1", refl.addr.port(), 'A', &call, Some("XRF757"))
        .expect("join");
    assert!(
        wait_for(
            || listener.status() == LinkStatus::Linked,
            Duration::from_millis(3_000)
        ),
        "the client linked"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert_eq!(feed.take(), None, "nobody is talking");
        thread::sleep(Duration::from_millis(20));
    }
    refl.far_talker_over();
    let mut heard = false;
    let tail = Instant::now() + Duration::from_millis(800);
    while Instant::now() < tail {
        heard |= feed.take().is_some();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(heard, "the far talker reached the client's feed");
    listener.leave();

    let seen = refl.datagrams();
    assert!(
        seen.iter().any(|d| d.len() == 11 && d[9] != b' '),
        "the client sent a link request: {seen:?}"
    );
    for datagram in &seen {
        assert!(
            DsvtPacket::parse(datagram).is_err(),
            "a header or voice packet left the daemon: {datagram:?}"
        );
        assert!(
            matches!(datagram.len(), 9 | 11),
            "unexpected traffic: {datagram:?}"
        );
    }
}
