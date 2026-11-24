// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The loopback reflector and the P25 link FSM, over real UDP sockets on
//! `127.0.0.1` and nothing else. CLAUDE.md forbids a test that touches any
//! other address, which is exactly why `reflector.rs` exists.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_p25::reflector::{PARROT_RECORD_CAP, Reflector};
use astar_p25::{
    Callsign, FsmAction, LinkState, P25Fsm, Superframe, end_record, poll, superframe, unlink,
};

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().expect("v4")
}

fn client(timeout: Duration) -> UdpSocket {
    let s = UdpSocket::bind(loopback()).expect("bind client");
    s.set_read_timeout(Some(timeout)).expect("timeout");
    s
}

/// Polls the reflector and waits for the echo.
fn link(s: &UdpSocket, reflector: SocketAddr, call: &str) {
    let p = poll(&Callsign::new(call).expect("callsign"));
    s.send_to(&p, reflector).expect("poll");
    let mut buf = [0u8; 64];
    let (n, from) = s.recv_from(&mut buf).expect("echo");
    assert_eq!(from, reflector);
    assert_eq!(&buf[..n], &p);
}

fn records() -> Vec<Vec<u8>> {
    let frames: [[u8; 11]; 18] = std::array::from_fn(|i| [u8::try_from(i).unwrap(); 11]);
    superframe(
        &Superframe {
            source_id: 1_234_567,
            talkgroup: 10_200,
            algid: 0x80,
            kid: 0,
        },
        &frames,
    )
}

fn recv(s: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    s.recv_from(&mut buf).ok().map(|(n, _)| buf[..n].to_vec())
}

/// True when nothing arrives within the socket's read timeout.
fn hears_nothing(s: &UdpSocket) -> bool {
    recv(s).is_none()
}

fn wait_for(mut f: impl FnMut() -> bool, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

#[test]
fn a_poll_is_echoed_verbatim_and_registers_the_peer() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();
    let a = client(Duration::from_millis(500));
    link(&a, addr, "KC0ABC");
    assert_eq!(handle.client_count(), 1);
    handle.shutdown();
}

#[test]
fn an_unlink_removes_the_peer() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();
    let a = client(Duration::from_millis(500));
    link(&a, addr, "KC0ABC");
    a.send_to(&unlink(&Callsign::new("KC0ABC").unwrap()), addr)
        .expect("unlink");
    assert!(wait_for(
        || handle.client_count() == 0,
        Duration::from_secs(1)
    ));
    // The reflector never answers an unlink.
    a.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    assert!(hears_nothing(&a));
}

#[test]
fn a_silent_peer_is_reaped_after_the_client_timeout() {
    let r = Reflector::bind_with_timeout(loopback(), Duration::from_millis(200)).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();
    let a = client(Duration::from_millis(500));
    link(&a, addr, "KC0ABC");
    assert_eq!(handle.client_count(), 1);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(handle.client_count(), 1, "reaped too early");
    assert!(wait_for(
        || handle.client_count() == 0,
        Duration::from_secs(1)
    ));
}

#[test]
fn voice_is_relayed_to_others_and_not_back_to_the_talker() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let _handle = r.run();
    let a = client(Duration::from_millis(150));
    let b = client(Duration::from_millis(500));
    let c = client(Duration::from_millis(500));
    link(&a, addr, "A");
    link(&b, addr, "B");
    link(&c, addr, "C");

    let recs = records();
    for rec in &recs {
        a.send_to(rec, addr).expect("voice");
    }
    a.send_to(&end_record(), addr).expect("end");
    for listener in [&b, &c] {
        for rec in &recs {
            assert_eq!(recv(listener).as_ref(), Some(rec), "verbatim, in order");
        }
        assert_eq!(recv(listener), Some(end_record().to_vec()));
    }
    assert!(hears_nothing(&a), "the talker never hears itself");
}

#[test]
fn a_second_talker_is_dropped_while_the_first_holds_the_floor() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let _handle = r.run();
    let a = client(Duration::from_millis(500));
    let b = client(Duration::from_millis(500));
    let c = client(Duration::from_millis(500));
    link(&a, addr, "A");
    link(&b, addr, "B");
    link(&c, addr, "C");

    let recs = records();
    a.send_to(&recs[0], addr).expect("a voice");
    assert_eq!(recv(&c).as_ref(), Some(&recs[0]));
    assert_eq!(recv(&b).as_ref(), Some(&recs[0]));

    b.send_to(&recs[1], addr).expect("b voice");
    b.send_to(&end_record(), addr).expect("b end");
    a.send_to(&recs[2], addr).expect("a voice");
    // C hears A's next record and nothing of B's.
    assert_eq!(recv(&c).as_ref(), Some(&recs[2]));
    c.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    assert!(hears_nothing(&c));
    // A still holds the floor: B's end record released nothing.
    a.send_to(&recs[3], addr).expect("a voice");
    assert_eq!(recv(&c).as_ref(), Some(&recs[3]));
}

#[test]
fn the_talker_is_released_by_the_end_record() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let _handle = r.run();
    let a = client(Duration::from_millis(500));
    let b = client(Duration::from_millis(500));
    link(&a, addr, "A");
    link(&b, addr, "B");

    let recs = records();
    a.send_to(&recs[0], addr).expect("a voice");
    a.send_to(&end_record(), addr).expect("a end");
    assert_eq!(recv(&b).as_ref(), Some(&recs[0]));
    assert_eq!(recv(&b), Some(end_record().to_vec()));

    b.send_to(&recs[5], addr).expect("b voice");
    assert_eq!(recv(&a).as_ref(), Some(&recs[5]), "B now has the floor");
}

/// Collects everything the socket hears until it has `count` datagrams,
/// with the time each arrived.
fn collect(s: &UdpSocket, count: usize) -> Vec<(Instant, Vec<u8>)> {
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        match recv(s) {
            Some(d) => out.push((Instant::now(), d)),
            None => break,
        }
    }
    out
}

#[test]
fn the_parrot_replays_an_over_to_its_sender_after_the_delay() {
    let r = Reflector::bind_parrot_with_timeouts(
        loopback(),
        Duration::from_secs(5),
        Duration::from_millis(100),
    )
    .expect("bind");
    let addr = r.local_addr();
    let _handle = r.run();
    let a = client(Duration::from_secs(2));
    link(&a, addr, "A");

    let recs = records();
    for rec in &recs {
        a.send_to(rec, addr).expect("voice");
    }
    a.send_to(&end_record(), addr).expect("end");
    let ended = Instant::now();

    let got = collect(&a, 19);
    assert_eq!(got.len(), 19);
    for (rec, (_, d)) in recs.iter().zip(&got) {
        assert_eq!(d, rec);
    }
    assert_eq!(got[18].1, end_record().to_vec());
    assert!(got[0].0.duration_since(ended) >= Duration::from_millis(100));
    // Nominally 360 ms; a late first record shortens what the clock sees,
    // because the parrot catches up rather than stretching the over.
    assert!(got[18].0.duration_since(got[0].0) >= Duration::from_millis(200));
    a.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(hears_nothing(&a), "played once");
}

#[test]
fn the_parrot_appends_an_end_record_when_the_over_stopped_dead() {
    let r = Reflector::bind_parrot_with_timeouts(
        loopback(),
        Duration::from_secs(5),
        Duration::from_millis(100),
    )
    .expect("bind");
    let addr = r.local_addr();
    let _handle = r.run();
    let a = client(Duration::from_secs(3));
    link(&a, addr, "A");

    let recs = records();
    for rec in &recs[..9] {
        a.send_to(rec, addr).expect("voice");
    }
    let stopped = Instant::now();
    let got = collect(&a, 10);
    assert_eq!(got.len(), 10);
    for (rec, (_, d)) in recs[..9].iter().zip(&got) {
        assert_eq!(d, rec);
    }
    assert_eq!(got[9].1, end_record().to_vec());
    // Not before the 1.5 s watchdog plus the replay delay.
    assert!(got[0].0.duration_since(stopped) >= Duration::from_millis(1_500));
}

#[test]
fn the_parrot_keeps_only_the_cap() {
    let r = Reflector::bind_parrot_with_timeouts(
        loopback(),
        Duration::from_secs(60),
        Duration::from_millis(50),
    )
    .expect("bind")
    // 9 000 records at the real 20 ms would take three minutes to play.
    .with_replay_interval(Duration::from_micros(200));
    let addr = r.local_addr();
    let handle = r.run();
    let a = client(Duration::from_secs(2));
    link(&a, addr, "A");

    // Every record is unique, so the replay can be checked in order even if
    // a socket buffer drops some. A default Linux receive buffer holds only
    // a few hundred small datagrams, and SO_RCVBUF is capped by rmem_max, so
    // the cap itself is read from what the parrot stored, and the send
    // overshoots it by enough that a few lost records cannot keep it short.
    let sent = unique_records(PARROT_RECORD_CAP + 360);
    for (i, rec) in sent.iter().enumerate() {
        a.send_to(rec, addr).expect("voice");
        if i % 32 == 31 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(wait_for(
        || handle.parrot_recorded() == PARROT_RECORD_CAP,
        Duration::from_secs(2)
    ));
    a.send_to(&end_record(), addr).expect("end");

    let got = collect(&a, PARROT_RECORD_CAP + 10);
    assert_eq!(handle.parrot_recorded(), PARROT_RECORD_CAP, "the cap held");
    assert!(got.len() <= PARROT_RECORD_CAP + 1, "{} replayed", got.len());
    assert!(got.len() > PARROT_RECORD_CAP / 2, "{} replayed", got.len());
    // What came back is the over, in order, with nothing invented: each
    // datagram is the next one sent or a later one, and only the end record
    // is not a voice record.
    let mut next = 0;
    for (i, (_, d)) in got.iter().enumerate() {
        if d == &end_record().to_vec() {
            assert_eq!(i, got.len() - 1, "the end record comes last");
            continue;
        }
        let at = sent[next..]
            .iter()
            .position(|s| s == d)
            .unwrap_or_else(|| panic!("record {i} is not the next part of the over"));
        next += at + 1;
    }
    if got.len() == PARROT_RECORD_CAP + 1 {
        assert_eq!(got.last().unwrap().1, end_record().to_vec());
    }
}

/// `count` voice records, each distinct: superframe `k` carries `k` in the
/// first two bytes of every IMBE frame and the record's index in the third.
fn unique_records(count: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(count);
    let mut k: u16 = 0;
    while out.len() < count {
        let frames: [[u8; 11]; 18] = std::array::from_fn(|i| {
            let [hi, lo] = k.to_be_bytes();
            let mut f = [0u8; 11];
            f[0] = hi;
            f[1] = lo;
            f[2] = u8::try_from(i).unwrap();
            f
        });
        out.extend(superframe(
            &Superframe {
                source_id: 1_234_567,
                talkgroup: 10_200,
                algid: 0x80,
                kid: 0,
            },
            &frames,
        ));
        k += 1;
    }
    out.truncate(count);
    out
}

#[test]
fn the_fsm_links_to_the_reflector() {
    let r = Reflector::bind(loopback()).expect("bind");
    let addr = r.local_addr();
    let handle = r.run();
    let s = client(Duration::from_millis(500));
    let mut fsm = P25Fsm::new("KC0ABC").expect("callsign");
    for d in fsm.connect(Instant::now()) {
        s.send_to(&d, addr).expect("poll");
    }
    let d = recv(&s).expect("echo");
    assert_eq!(fsm.on_packet(&d, Instant::now()), FsmAction::Linked);
    assert_eq!(fsm.state(), LinkState::Linked);
    assert_eq!(handle.client_count(), 1);
    for d in fsm.unlink(Instant::now()) {
        s.send_to(&d, addr).expect("unlink");
    }
    assert!(wait_for(
        || handle.client_count() == 0,
        Duration::from_secs(1)
    ));
    handle.shutdown();
}
