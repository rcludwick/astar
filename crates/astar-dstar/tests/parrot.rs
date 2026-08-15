// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration tests for [`Reflector`] (iax-a9d4 Task 5): a real,
//! `DExtra`-compatible loopback reflector, exercised over real `127.0.0.1`
//! UDP sockets standing in for raw `DExtra` clients — no `DextraFsm` or
//! codec involved, this crate has none of that. Byte layouts mirror
//! `fsm.rs`'s tests exactly (see its module doc's "Wire layout" section):
//! 11-byte connect/unlink, 14-byte ACK/NAK, 9-byte keepalive, 12-byte
//! `"DISCONNECTED"`.

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use astar_dstar::{DsvtPacket, Reflector, RfHeader, SYNC_INTERVAL};

// ---- helpers ------------------------------------------------------------

/// A raw UDP socket standing in for one `DExtra` client — bound but not
/// `connect()`-ed, so it can be pointed at the reflector with explicit
/// `send_to`/`recv_from` calls.
fn raw_client() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw client");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    (sock, addr)
}

fn callsign8(cs: &str) -> [u8; 8] {
    let mut buf = [b' '; 8];
    buf[..cs.len()].copy_from_slice(cs.as_bytes());
    buf
}

/// 11-byte connect request: 8-byte callsign, own module (always `' '` for a
/// plain client, per `fsm.rs`'s `OWN_MODULE`), dest module, revision byte.
fn connect_bytes(callsign: &str, dest_module: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(11);
    buf.extend_from_slice(&callsign8(callsign));
    buf.push(b' ');
    buf.push(dest_module);
    buf.push(0x00);
    buf
}

/// 11-byte unlink request: same shape as connect, dest module blanked.
fn unlink_bytes(callsign: &str) -> Vec<u8> {
    connect_bytes(callsign, b' ')
}

/// 9-byte keepalive: 8-byte callsign + NUL.
fn keepalive_bytes(callsign: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9);
    buf.extend_from_slice(&callsign8(callsign));
    buf.push(0x00);
    buf
}

fn sample_header() -> RfHeader {
    RfHeader {
        flags: [0x00, 0x00, 0x00],
        rpt2: *b"XRF757 G",
        rpt1: *b"XRF757 A",
        ur: *b"CQCQCQ  ",
        my: *b"AJ7HR   ",
        suffix: *b"    ",
    }
}

/// One 20ms voice frame of a synthetic transmission: distinct AMBE bytes per
/// `seq` so replayed-vs-original comparison is meaningful, not just
/// coincidentally equal.
fn voice_frame(stream_id: u16, seq: u8, end: bool) -> DsvtPacket {
    DsvtPacket::Voice {
        stream_id,
        seq,
        end,
        ambe: [seq; 9],
        slow_data: [0x00, 0x00, 0x00],
    }
}

fn recv_packet(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let (n, _) = sock.recv_from(&mut buf).expect("expected a packet");
    buf[..n].to_vec()
}

/// Deadline-polling helper (house style: no fixed sleeps) — mirrors
/// `astar-m17`'s `tests/reflector.rs::wait_until`.
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

// ---- tests ---------------------------------------------------------------

#[test]
fn connect_is_acked_and_invalid_module_is_nacked() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'B'), addr).unwrap();
    let reply = recv_packet(&c1);
    assert_eq!(reply.len(), 14, "ACK/NAK is 14 bytes");
    assert!(reply.ends_with(b"ACK\0"), "valid module must be ACKed");
    // Poll rather than assert outright: receiving the ACK proves the reflector
    // *replied*, not that its run-loop finished inserting the client into the
    // table. The two are separate steps on that thread, and on a loaded box the
    // read here can win the race — which is exactly how this test failed CI
    // with left: 0, right: 1.
    assert!(
        wait_until(|| handle.client_count() == 1, 1_000),
        "the ACKed client must be linked"
    );

    let (c2, _) = raw_client();
    c2.send_to(&connect_bytes("N0CALL2", b'5'), addr).unwrap();
    let reply2 = recv_packet(&c2);
    assert_eq!(reply2.len(), 14);
    assert!(
        reply2.ends_with(b"NAK\0"),
        "a non A-Z module must be NACKed"
    );
    assert!(
        wait_until(|| handle.client_count() == 1, 1_000),
        "the NACKed client must not be linked"
    );

    handle.shutdown();
}

#[test]
fn client_keepalive_is_not_echoed_but_still_refreshes_liveness() {
    // Regression test (whole-branch review finding 1, CRITICAL) for the
    // ping-pong storm: this reflector used to echo every inbound client
    // keepalive with one of its own. Harmless with a raw test client that
    // never answers, but linked against a real client (or
    // `DextraFsm::on_packet`, which correctly answers every keepalive it
    // receives per the DExtra client-side contract) that echo ignites an
    // unbounded ping-pong loop — measured at ~156k pkt/s against the local
    // parrot smoke path. Real xlxd treats an inbound client keepalive as
    // liveness-refresh only; the reflector→client direction is covered
    // entirely by its own periodic tick ping. This test asserts BOTH
    // halves: the keepalive is never replied to, AND it still refreshes
    // `last_seen` (the client is not reaped even though it sends nothing
    // else for a while).
    let client_timeout = Duration::from_millis(300);
    let reflector = Reflector::bind_with_timeouts(
        "127.0.0.1:0".parse().unwrap(),
        // A keepalive_interval far longer than this test's own timing so
        // the reflector's own periodic tick ping can never be mistaken for
        // a reply to the client's keepalive below.
        Duration::from_secs(10),
        client_timeout,
    )
    .expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();
    let t_connect = Instant::now();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    assert!(recv_packet(&c1).ends_with(b"ACK\0"));

    // Send one keepalive partway through the client_timeout window.
    let t_keepalive = t_connect + client_timeout / 2;
    if Instant::now() < t_keepalive {
        thread::sleep(t_keepalive - Instant::now());
    }
    c1.send_to(&keepalive_bytes("N0CALL"), addr).unwrap();

    // It must never be echoed.
    c1.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut buf = [0u8; 128];
    assert!(
        c1.recv_from(&mut buf).is_err(),
        "an inbound client keepalive must never be echoed back"
    );

    // But it must still have refreshed `last_seen`: wait until AFTER the
    // ORIGINAL (pre-refresh) reap deadline (t_connect + client_timeout)
    // would have fired, yet comfortably before the REFRESHED deadline
    // (t_keepalive + client_timeout) — and confirm the client is still
    // linked, proving the keepalive refreshed liveness.
    let t_check = t_connect + client_timeout + Duration::from_millis(50);
    if Instant::now() < t_check {
        thread::sleep(t_check - Instant::now());
    }
    assert_eq!(
        handle.client_count(),
        1,
        "the keepalive must have refreshed liveness even though it got no reply"
    );

    handle.shutdown();
}

#[test]
fn linked_client_answering_reflector_pings_stays_alive_with_bounded_traffic() {
    // Integration-level regression for the storm (closes the ledger gap
    // "proactive-keepalive + eviction untested at integration level"):
    // with the client correctly answering every reflector-initiated ping
    // (exactly what `DextraFsm::on_packet`'s keepalive branch does) but
    // this reflector correctly NOT echoing that answer, the link survives
    // across many keepalive intervals with strictly bounded traffic —
    // never the ~156k pkt/s storm the un-fixed reflector produced.
    let keepalive_interval = Duration::from_millis(40);
    let client_timeout = Duration::from_millis(150);
    let reflector = Reflector::bind_with_timeouts(
        "127.0.0.1:0".parse().unwrap(),
        keepalive_interval,
        client_timeout,
    )
    .expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    assert!(recv_packet(&c1).ends_with(b"ACK\0"));

    // Hold the link for several keepalive intervals — well past
    // client_timeout too, so staying linked genuinely depends on the
    // pings being answered — answering every 9-byte keepalive-shaped ping
    // from the reflector with our own keepalive, mirroring
    // `DextraFsm::on_packet`'s keepalive branch exactly.
    c1.set_read_timeout(Some(Duration::from_millis(30)))
        .unwrap();
    let hold_for = keepalive_interval * 8;
    let deadline = Instant::now() + hold_for;
    let mut packets_received = 0usize;
    let mut buf = [0u8; 128];
    while Instant::now() < deadline {
        if let Ok((n, _)) = c1.recv_from(&mut buf) {
            packets_received += 1;
            if n == 9 && buf[8] == 0x00 {
                c1.send_to(&keepalive_bytes("N0CALL"), addr).unwrap();
            }
        }
    }

    assert_eq!(
        handle.client_count(),
        1,
        "the link must survive many keepalive intervals when the client answers pings"
    );
    assert!(
        packets_received > 0,
        "the reflector must have sent at least one periodic ping"
    );
    assert!(
        packets_received < 50,
        "traffic must stay bounded (a handful of pings), not a storm: got {packets_received}"
    );

    handle.shutdown();
}

#[test]
fn unlink_replies_disconnected_and_reaps_the_client() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    let ack = recv_packet(&c1);
    assert!(ack.ends_with(b"ACK\0"));
    assert!(
        wait_until(|| handle.client_count() == 1, 1_000),
        "the ACKed client must be linked before the unlink below"
    );

    c1.send_to(&unlink_bytes("N0CALL"), addr).unwrap();
    let reply = recv_packet(&c1);
    assert_eq!(reply, b"DISCONNECTED");

    assert!(
        wait_until(|| handle.client_count() == 0, 1_000),
        "the client must be reaped immediately on unlink"
    );

    handle.shutdown();
}

#[test]
fn relay_forwards_verbatim_to_other_same_module_clients_not_the_sender() {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    let (c2, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    assert!(recv_packet(&c1).ends_with(b"ACK\0"));
    c2.send_to(&connect_bytes("N0CALL2", b'A'), addr).unwrap();
    assert!(recv_packet(&c2).ends_with(b"ACK\0"));

    let header_pkt = DsvtPacket::Header {
        stream_id: 0xBEEF,
        header: sample_header(),
    }
    .encode();
    c1.send_to(&header_pkt, addr).unwrap();

    let relayed = recv_packet(&c2);
    assert_eq!(
        relayed, header_pkt,
        "relay to another client on the same module must be byte-identical"
    );

    // The sender must never receive its own relayed packet back.
    c1.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut buf = [0u8; 128];
    assert!(
        c1.recv_from(&mut buf).is_err(),
        "the sender must not receive its own packet back"
    );

    handle.shutdown();
}

#[test]
fn parrot_records_a_full_stream_and_replays_it_intact_after_a_short_delay() {
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    assert!(recv_packet(&c1).ends_with(b"ACK\0"));

    let stream_id = 0x4242;
    let header = sample_header();
    let header_pkt = DsvtPacket::Header { stream_id, header }.encode();
    c1.send_to(&header_pkt, addr).unwrap();

    // 21 voice frames (one full superframe, SYNC_INTERVAL) — the last one
    // carries the EOT bit.
    let n_frames = SYNC_INTERVAL;
    let mut sent_voice = Vec::new();
    for seq in 0..n_frames {
        let end = seq == n_frames - 1;
        let pkt = voice_frame(stream_id, seq, end);
        let bytes = pkt.encode();
        c1.send_to(&bytes, addr).unwrap();
        sent_voice.push(pkt);
    }

    // Nothing must come back before the ~150ms replay delay has elapsed —
    // guards against an "instant blast" implementation that skips the gap
    // entirely. Uses a short, bounded wait (well under the 500ms budget),
    // not a hard timing assertion.
    c1.set_read_timeout(Some(Duration::from_millis(60)))
        .unwrap();
    let mut early_buf = [0u8; 128];
    assert!(
        c1.recv_from(&mut early_buf).is_err(),
        "playback must not start before the ~150ms replay delay"
    );

    // The full stream (header + 21 voice frames) must now arrive, verbatim.
    c1.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let replayed_header_bytes = recv_packet(&c1);
    assert_eq!(
        replayed_header_bytes, header_pkt,
        "replayed header must be byte-identical, including the stream id verbatim"
    );
    let replayed_header = DsvtPacket::parse(&replayed_header_bytes).expect("valid header packet");
    match replayed_header {
        DsvtPacket::Header {
            stream_id: replayed_id,
            header: replayed_hdr,
        } => {
            assert_eq!(replayed_id, stream_id, "stream id replayed verbatim");
            assert_eq!(replayed_hdr, header);
        }
        DsvtPacket::Voice { .. } => panic!("expected a header packet first"),
    }

    for (i, original) in sent_voice.iter().enumerate() {
        let bytes = recv_packet(&c1);
        let parsed = DsvtPacket::parse(&bytes).expect("valid voice packet");
        assert_eq!(
            parsed, *original,
            "replayed voice frame #{i} must match the original exactly (AMBE bytes, seq, EOT)"
        );
    }

    if let DsvtPacket::Voice { end, .. } = sent_voice.last().unwrap() {
        assert!(*end, "test sanity: last sent frame carried the EOT bit");
    }

    handle.shutdown();
}

#[test]
fn a_second_stream_sent_while_the_first_is_queued_for_replay_is_ignored() {
    // A generous replay delay so there's a real window, after the first
    // stream's EOT, to inject a second ("overlapping") stream before
    // playback fires — still well under the 500ms test budget.
    let replay_delay = Duration::from_millis(250);
    let reflector = Reflector::bind_parrot_with_timeouts(
        "127.0.0.1:0".parse().unwrap(),
        Duration::from_secs(3),
        Duration::from_secs(30),
        replay_delay,
    )
    .expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let (c1, _) = raw_client();
    c1.send_to(&connect_bytes("N0CALL", b'A'), addr).unwrap();
    assert!(recv_packet(&c1).ends_with(b"ACK\0"));

    let first_stream_id = 0x1111;
    let first_header = DsvtPacket::Header {
        stream_id: first_stream_id,
        header: sample_header(),
    }
    .encode();
    c1.send_to(&first_header, addr).unwrap();
    let first_voice = voice_frame(first_stream_id, 0, true).encode(); // single-frame stream
    c1.send_to(&first_voice, addr).unwrap();

    // Still inside the replay-delay window: send a second, distinct stream
    // from the same sender. It must be completely ignored for capture
    // purposes (a playback is already queued for this sender).
    let second_stream_id = 0x2222;
    let second_header = DsvtPacket::Header {
        stream_id: second_stream_id,
        header: sample_header(),
    }
    .encode();
    c1.send_to(&second_header, addr).unwrap();
    let second_voice = voice_frame(second_stream_id, 0, true).encode();
    c1.send_to(&second_voice, addr).unwrap();

    // Only the FIRST stream's header+frame must ever come back.
    c1.set_read_timeout(Some(Duration::from_millis(600)))
        .unwrap();
    let replayed_header = recv_packet(&c1);
    assert_eq!(
        replayed_header, first_header,
        "the replayed header must be the FIRST stream's, not the overlapping second one"
    );
    let replayed_voice = recv_packet(&c1);
    assert_eq!(
        replayed_voice, first_voice,
        "the replayed voice frame must be the FIRST stream's"
    );

    // Nothing further (i.e. no second playback for the ignored overlapping
    // stream) must ever arrive.
    c1.set_read_timeout(Some(Duration::from_millis(150)))
        .unwrap();
    let mut buf = [0u8; 128];
    assert!(
        c1.recv_from(&mut buf).is_err(),
        "the overlapping second stream must never be captured or replayed"
    );

    handle.shutdown();
}
