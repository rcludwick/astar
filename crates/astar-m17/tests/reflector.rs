// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration tests for [`Reflector`] (iax-f2b8 Task 6): a real,
//! `mrefd`-compatible loopback reflector, exercised over real
//! `127.0.0.1` UDP sockets standing in for raw M17 clients (no `M17Session`
//! or codec involved — this crate has none of that).

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use astar_m17::{BROADCAST, ControlPacket, Lsf, Reflector, StreamPacket, encode_callsign};

// ---- helpers ----------------------------------------------------------

/// A raw UDP socket standing in for one M17 client — bound but otherwise
/// unconfigured (not `connect()`-ed), so it can be pointed at the reflector
/// with explicit `send_to`/`recv_from` calls.
fn raw_client() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw client");
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    (sock, addr)
}

fn conn_bytes(callsign: &str, module: u8) -> Vec<u8> {
    ControlPacket::Conn {
        callsign: encode_callsign(callsign).unwrap(),
        module,
    }
    .to_bytes()
}

fn stream_packet(callsign: &str, frame_number: u16) -> [u8; 54] {
    StreamPacket {
        stream_id: 0xBEEF,
        lsf: Lsf {
            dst: BROADCAST,
            src: encode_callsign(callsign).unwrap(),
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number,
        payload: [0xAB; 16],
    }
    .to_bytes()
}

/// Like [`stream_packet`] but with an explicit `stream_id` and EOS bit — the
/// parrot tests need to send a whole multi-packet transmission on one
/// `StreamID` and control exactly which packet carries EOS.
fn stream_packet_full(callsign: &str, stream_id: u16, frame_number: u16, eos: bool) -> [u8; 54] {
    let frame_number = if eos {
        frame_number | StreamPacket::EOS_BIT
    } else {
        frame_number
    };
    StreamPacket {
        stream_id,
        lsf: Lsf {
            dst: BROADCAST,
            src: encode_callsign(callsign).unwrap(),
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number,
        payload: [0xAB; 16],
    }
    .to_bytes()
}

/// Deadline-polling helper (house style: no fixed sleeps).
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

// ---- tests --------------------------------------------------------------

#[test]
fn two_clients_same_module_relay_to_each_other_not_to_sender() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _c1_addr) = raw_client();
    let (c2, _c2_addr) = raw_client();

    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = c1.recv_from(&mut buf).expect("c1 ACKN");
    assert_eq!(&buf[..n], b"ACKN");

    c2.send_to(&conn_bytes("N0CALL2", b'A'), addr).unwrap();
    let (n, _) = c2.recv_from(&mut buf).expect("c2 ACKN");
    assert_eq!(&buf[..n], b"ACKN");

    let pkt = stream_packet("N0CALL", 0);
    c1.send_to(&pkt, addr).unwrap();

    let mut relay_buf = [0u8; 128];
    let (n, _) = c2
        .recv_from(&mut relay_buf)
        .expect("c2 must receive c1's relayed stream packet");
    assert_eq!(
        &relay_buf[..n],
        &pkt[..],
        "relayed bytes must be identical to what was sent"
    );

    // c1 must NOT receive its own packet back.
    c1.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut self_buf = [0u8; 128];
    let result = c1.recv_from(&mut self_buf);
    assert!(
        result.is_err(),
        "the sender must never receive its own relayed packet back, got {result:?}"
    );

    handle.shutdown();
}

#[test]
fn client_in_a_different_module_hears_nothing() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let (c2, _) = raw_client();
    let (c3, _) = raw_client();

    let mut buf = [0u8; 64];
    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let (n, _) = c1.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    c2.send_to(&conn_bytes("N0CALL2", b'A'), addr).unwrap();
    let (n, _) = c2.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    // c3 joins module B.
    c3.send_to(&conn_bytes("N0CALL3", b'B'), addr).unwrap();
    let (n, _) = c3.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    // Module A traffic: c1 -> c2 (already covered above); c3 must receive
    // NOTHING from it.
    let pkt = stream_packet("N0CALL", 0);
    c1.send_to(&pkt, addr).unwrap();

    // Give c2 (same module) a chance to actually receive it, confirming the
    // relay fired at all, before asserting c3 got nothing.
    let mut c2_buf = [0u8; 128];
    c2.recv_from(&mut c2_buf)
        .expect("c2 (module A) must hear it");

    c3.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut c3_buf = [0u8; 128];
    let result = c3.recv_from(&mut c3_buf);
    assert!(
        result.is_err(),
        "a client on a different module must never receive module A's traffic, got {result:?}"
    );

    handle.shutdown();
}

#[test]
fn conn_with_non_alpha_module_is_nacked() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&conn_bytes("N0CALL", b'5'), addr).unwrap();

    let mut buf = [0u8; 64];
    let (n, _) = c1.recv_from(&mut buf).expect("reflector must reply");
    assert_eq!(&buf[..n], b"NACK", "a non A-Z module must be NACKed");

    handle.shutdown();
}

#[test]
fn client_that_never_pongs_is_reaped_while_a_ponging_client_stays() {
    let reflector = Reflector::bind_with_timeouts(
        "127.0.0.1:0".parse().unwrap(),
        Duration::from_millis(100),
        Duration::from_millis(400),
    )
    .expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (silent, _) = raw_client();
    let (responder, _) = raw_client();

    let mut buf = [0u8; 64];
    silent.send_to(&conn_bytes("SILENT", b'A'), addr).unwrap();
    let (n, _) = silent.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    responder
        .send_to(&conn_bytes("PONGER", b'A'), addr)
        .unwrap();
    let (n, _) = responder.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    assert_eq!(handle.client_count(), 2, "both clients start out linked");

    // Keep `responder` alive by answering every PING with a PONG, for well
    // past the 400ms client_timeout; `silent` never answers.
    let responder_cs = encode_callsign("PONGER").unwrap();
    let keep_alive_deadline = Instant::now() + Duration::from_millis(900);
    while Instant::now() < keep_alive_deadline {
        let mut ping_buf = [0u8; 64];
        if let Ok((n, src)) = responder.recv_from(&mut ping_buf)
            && &ping_buf[..n.min(4)] == b"PING"
        {
            let pong = ControlPacket::Pong {
                callsign: responder_cs,
            }
            .to_bytes();
            responder.send_to(&pong, src).unwrap();
        }
    }

    assert_eq!(
        handle.client_count(),
        1,
        "the silent client must have been reaped while the ponging one stays"
    );

    // Confirm behaviorally too: a stream packet from `responder` no longer
    // reaches `silent` (it isn't in the table anymore to relay to) — but
    // there's no third listener to positively assert that against here, so
    // the client_count check above is the primary assertion per the task
    // brief's "or expose client_count" allowance.

    handle.shutdown();
}

#[test]
fn disc_gets_a_bare_ack_and_the_client_is_reaped_immediately() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let cs = encode_callsign("N0CALL").unwrap();

    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = c1.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");
    assert_eq!(handle.client_count(), 1);

    let disc = ControlPacket::Disc { callsign: Some(cs) }.to_bytes();
    c1.send_to(&disc, addr).unwrap();

    let (n, _) = c1.recv_from(&mut buf).expect("reflector must ack the DISC");
    assert_eq!(
        &buf[..n],
        b"DISC",
        "the DISC ack must be the bare 4-byte form"
    );

    assert!(
        wait_until(|| handle.client_count() == 0, 1_000),
        "the client must be reaped immediately on DISC"
    );

    handle.shutdown();
}

// ---- parrot mode (iax-91f4) -------------------------------------------------

#[test]
fn parrot_echoes_the_eos_terminated_stream_back_to_sender_paced_and_still_relays_to_others() {
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let (c2, _) = raw_client();

    let mut buf = [0u8; 64];
    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let (n, _) = c1.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    c2.send_to(&conn_bytes("N0CALL2", b'A'), addr).unwrap();
    let (n, _) = c2.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    let stream_id = 0x4242;
    let p0 = stream_packet_full("N0CALL", stream_id, 0, false);
    let p1 = stream_packet_full("N0CALL", stream_id, 1, false);
    let p2 = stream_packet_full("N0CALL", stream_id, 2, true); // EOS
    c1.send_to(&p0, addr).unwrap();
    c1.send_to(&p1, addr).unwrap();
    c1.send_to(&p2, addr).unwrap();

    // c2 (same module) must still receive the ORIGINAL relay, byte-for-byte,
    // exactly as the non-parrot relay test asserts — parrot is additive.
    let mut relay_buf = [0u8; 128];
    for expected in [&p0[..], &p1[..], &p2[..]] {
        let (n, _) = c2
            .recv_from(&mut relay_buf)
            .expect("c2 must receive the original relayed packet");
        assert_eq!(
            &relay_buf[..n],
            expected,
            "relay to other same-module clients must be byte-identical, unaffected by parrot"
        );
    }

    // c1 (the sender) must receive its own echoed transmission back: 3
    // packets, fresh StreamID, DST = its own callsign, paced (not blasted).
    c1.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let n0call = encode_callsign("N0CALL").unwrap();
    let mut received = Vec::new();
    let mut arrival_times = Vec::new();
    let mut echo_buf = [0u8; 128];
    for _ in 0..3 {
        let (n, _) = c1
            .recv_from(&mut echo_buf)
            .expect("must receive echoed playback packet");
        arrival_times.push(Instant::now());
        received.push(StreamPacket::parse(&echo_buf[..n]).expect("valid stream packet"));
    }

    assert_ne!(
        received[0].stream_id, stream_id,
        "playback must use a fresh StreamID, not the original transmission's"
    );
    for pkt in &received {
        assert_eq!(
            pkt.stream_id, received[0].stream_id,
            "every playback packet shares the one fresh StreamID"
        );
        assert_eq!(
            pkt.lsf.dst, n0call,
            "playback DST must be the sender's own callsign"
        );
    }
    assert_eq!(received[0].frame_number, 0);
    assert_eq!(received[1].frame_number, 1);
    assert!(!received[0].is_last());
    assert!(!received[1].is_last());
    assert!(
        received[2].is_last(),
        "EOS must be set on the last playback packet"
    );
    assert_eq!(received[2].frame_number & !StreamPacket::EOS_BIT, 2);

    // Paced, not blasted: consecutive arrivals must be measurably apart. The
    // real target is ~40ms; this asserts a generous 20ms floor so the check
    // isn't flaky under CI scheduling jitter while still failing hard against
    // an accidental "send everything in one go" regression.
    for w in arrival_times.windows(2) {
        assert!(
            w[1].duration_since(w[0]) >= Duration::from_millis(20),
            "playback packets must be paced apart, not sent back-to-back, got {:?}",
            w[1].duration_since(w[0])
        );
    }

    handle.shutdown();
}

#[test]
fn parrot_flushes_and_echoes_after_a_silence_gap_with_no_eos_ever_sent() {
    let reflector = Reflector::bind_parrot_with_timeouts(
        "127.0.0.1:0".parse().unwrap(),
        Duration::from_secs(3),
        Duration::from_secs(30),
        Duration::from_millis(200), // shortened silence-flush window for the test
    )
    .expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let mut buf = [0u8; 64];
    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let (n, _) = c1.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    // A single stream packet, no EOS ever sent — the reflector must flush and
    // echo it back anyway once the (shortened) silence window elapses.
    let p0 = stream_packet_full("N0CALL", 0x99, 0, false);
    c1.send_to(&p0, addr).unwrap();

    c1.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut echo_buf = [0u8; 128];
    let (n, _) = c1
        .recv_from(&mut echo_buf)
        .expect("must receive the silence-flushed echo");
    let pkt = StreamPacket::parse(&echo_buf[..n]).expect("valid stream packet");
    assert!(
        pkt.is_last(),
        "the single buffered frame must be echoed back with EOS set"
    );
    assert_eq!(pkt.lsf.dst, encode_callsign("N0CALL").unwrap());

    handle.shutdown();
}

/// iax-91f4 fix-forward: a permanent measurement harness for playback pacing.
///
/// Before the fix, the run-loop's socket read timeout was a fixed 50ms
/// regardless of parrot mode — but while a playback drains, the sender is
/// unkeyed, so that read timeout is the loop's ONLY wake source, and a 40ms
/// pacing deadline can never fire on time against a 50ms poll. Measured
/// average inter-packet arrival was ~50.9ms, not ~40ms. The fix shortens the
/// read timeout on the fly to the earliest pending playback deadline
/// (`next_read_timeout`) whenever it's sooner than the normal poll.
///
/// This test sends a 25-packet stream (24 inter-packet gaps, comfortably
/// over "20 packets") and asserts EVERY gap lands in a 34-48ms band —
/// widened from 36-46 after 2/85 local runs flaked at the tighter bound
/// (scheduling jitter is real; the band still catches the pre-fix ~51ms)
/// around the 40ms target — tight enough to fail against the old ~50ms
/// behavior, loose enough not to flake under CI scheduling jitter.
#[test]
fn parrot_playback_packets_are_paced_34_to_48ms_apart_over_many_packets() {
    const N: u16 = 25;

    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let mut buf = [0u8; 64];
    c1.send_to(&conn_bytes("N0CALL", b'A'), addr).unwrap();
    let (n, _) = c1.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ACKN");

    let stream_id = 0x7777;
    for i in 0..N {
        let pkt = stream_packet_full("N0CALL", stream_id, i, i == N - 1);
        c1.send_to(&pkt, addr).unwrap();
    }

    c1.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut arrival_times = Vec::with_capacity(N as usize);
    let mut echo_buf = [0u8; 128];
    for _ in 0..N {
        let (_n, _) = c1
            .recv_from(&mut echo_buf)
            .expect("must receive every paced playback packet");
        arrival_times.push(Instant::now());
    }

    for (i, w) in arrival_times.windows(2).enumerate() {
        let delta = w[1].duration_since(w[0]);
        assert!(
            delta >= Duration::from_millis(34) && delta <= Duration::from_millis(48),
            "gap #{i} between playback packets must land in the 34-48ms band \
             (target ~40ms), got {delta:?}"
        );
    }

    handle.shutdown();
}
