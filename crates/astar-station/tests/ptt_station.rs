// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station` PTT arbitration on a live loopback session: the OR of sources,
//! the Bluetooth lease, the transmit time-out, the `astar-ptt-watch` watchdog
//! that enforces both, the universal stop, release-to-re-arm, and the
//! disconnect, snapshot and Drop paths.
//!
//! Every keyed test links to a fake M17 reflector bound to 127.0.0.1 (the
//! `tests/m17_station.rs` pattern) over `NullBackend`. The fake ACKs the link
//! and ignores everything else: a "keyed" station here sends stream packets
//! into a loopback socket nobody reads. No radio, serial port or Bluetooth
//! adapter is ever opened.
//!
//! Timing bounds are read on the ENGINE side (`ptt_snapshot`), which is what
//! the watchdog promises. The console side (`snapshot().ptt`) is the M17 run
//! loop's applied state and lags an unkey by up to one run-loop poll, so it is
//! only ever waited for, never timed.
//!
//! Gated on `m17` (a default feature) because every keyed test needs the M17
//! session; `just test` builds with default features.
#![cfg(feature = "m17")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use astar_station::{
    CallStatus, ConsoleSession, DEFAULT_TX_TIMEOUT, ForcedReason, ForcedRecord, PttSource, Station,
    StationConfig, StationError,
};

/// A short lease so the expiry test runs in well under a second.
const LEASE_TTL: Duration = Duration::from_millis(300);
/// How late the watchdog may act: one 50 ms watchdog period plus
/// scheduling slack.
const WATCHDOG_SLACK: Duration = Duration::from_millis(100);
/// The largest TTL the engine accepts (5 s, the top of `ptt_lease_press`'s
/// 1..=5000 ms range), so a lease cannot expire inside any test that does
/// not renew it, even on a loaded runner.
const LONG_LEASE: Duration = Duration::from_millis(5_000);

/// Runs each deadline test alone in this binary. The harness runs tests in
/// parallel, and every other test here starts an M17 session (Codec 2
/// encode, a reflector thread, polling loops): that load could eat the
/// 100 ms of `WATCHDOG_SLACK` and fail a correct watchdog. Deadline tests
/// take the write side; every other test takes the read side at its first
/// line, so they still run in parallel with each other.
static TIMING: RwLock<()> = RwLock::new(());

/// Hold [`TIMING`] exclusively for the rest of a deadline test, surviving a
/// poisoned lock left by a test that failed.
fn timing_guard() -> RwLockWriteGuard<'static, ()> {
    TIMING.write().unwrap_or_else(PoisonError::into_inner)
}

/// Hold [`TIMING`] shared for the rest of a test that checks no deadline.
fn shared_timing() -> RwLockReadGuard<'static, ()> {
    TIMING.read().unwrap_or_else(PoisonError::into_inner)
}

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

/// A station sharing its `ConsoleSession` with the test, so the test can tear
/// a call down underneath the station or count references to the session.
fn shared_station() -> (Station, Arc<Mutex<ConsoleSession>>) {
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        StationConfig::default(),
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    (station, session)
}

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

/// `wait_until` with a 1 ms poll, for the waits whose finishing time is then
/// checked against an upper bound: a 10 ms poll would spend a tenth of the
/// 100 ms watchdog slack on the test's own polling.
fn wait_until_tight(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    pred()
}

/// Sleep until `deadline`, or not at all if it has passed. Mid-hold checks
/// sleep to a deadline measured from the press, not for a fixed time after
/// some earlier wait, so a slow run loop cannot push them past the limit.
fn sleep_until(deadline: Instant) {
    thread::sleep(deadline.saturating_duration_since(Instant::now()));
}

/// CONN(11 B) → ACKN, then silence. Lives 8 s, far longer than any test here.
fn spawn_reflector() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let mut acked = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if !acked && n == 11 && &buf[0..4] == b"CONN" {
                acked = true;
                let _ = sock.send_to(b"ACKN", src);
            }
        }
    });
    addr
}

/// Link `station` to a fresh loopback reflector and wait for Answered.
fn link(station: &Station) {
    let addr = spawn_reflector();
    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect to the loopback reflector");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
        "must link before keying, got {:?}",
        station.snapshot().status
    );
}

fn linked_station() -> Station {
    let station = test_station();
    link(&station);
    station
}

fn both() -> u32 {
    PttSource::Bluetooth.bit() | PttSource::Manual.bit()
}

// ---- the lease and the watchdog ------------------------------------------

#[test]
fn lease_press_keys_and_the_watchdog_unkeys_within_ttl_plus_100ms() {
    let _timing = timing_guard();
    let station = linked_station();
    let pressed_at = Instant::now();
    // `Ok` means the arbiter saw the rising edge and the console accepted the
    // key; the console's applied state is not waited for here, because a slow
    // run loop could push that wait past the TTL.
    station
        .ptt_lease_press(PttSource::Bluetooth, LEASE_TTL)
        .expect("a lease press keys an answered call");
    sleep_until(pressed_at + LEASE_TTL / 2);
    let mid_sources = station.ptt_snapshot().sources;
    let mid_at = pressed_at.elapsed();
    assert!(
        mid_at < LEASE_TTL,
        "the test thread overslept the half-way check: {mid_at:?}"
    );
    assert_eq!(
        mid_sources,
        PttSource::Bluetooth.bit(),
        "the lease must still hold halfway through its TTL"
    );
    assert!(
        wait_until_tight(|| station.ptt_snapshot().sources == 0, 2_000),
        "with no renew the watchdog must release the lease"
    );
    let released_after = pressed_at.elapsed();
    assert!(
        released_after >= LEASE_TTL,
        "released before the TTL: {released_after:?}"
    );
    assert!(
        released_after <= LEASE_TTL + WATCHDOG_SLACK,
        "released later than TTL + 100 ms: {released_after:?}"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(
        ptt.latched,
        PttSource::Bluetooth.bit(),
        "expiry latches the source"
    );
    assert_eq!(
        ptt.forced,
        ForcedRecord {
            seq: 1,
            sources: PttSource::Bluetooth.bit(),
            reason: Some(ForcedReason::LeaseExpired),
        }
    );
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "the console must follow the watchdog's unkey"
    );
    station.m17_disconnect();
}

#[test]
fn renewing_keeps_the_lease_alive_past_its_ttl() {
    let _timing = timing_guard();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LEASE_TTL)
        .expect("key");
    let until = Instant::now() + LEASE_TTL * 3;
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(100));
        station
            .ptt_lease_renew(PttSource::Bluetooth)
            .expect("renewing a held lease succeeds");
    }
    let ptt = station.ptt_snapshot();
    assert_eq!(
        ptt.sources,
        PttSource::Bluetooth.bit(),
        "a renewed lease must outlive three TTLs"
    );
    assert_eq!(ptt.forced.seq, 0, "no forced stop happened");
    station
        .ptt_source(PttSource::Bluetooth, false)
        .expect("release");
    assert_eq!(station.ptt_snapshot().sources, 0);
    assert!(wait_until(|| !station.snapshot().ptt, 1_000));
    station.m17_disconnect();
}

#[test]
fn manual_release_while_bluetooth_holds_stays_keyed() {
    let _timing = shared_timing();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("bluetooth keys");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    station
        .ptt_source(PttSource::Manual, true)
        .expect("manual joins the over");
    assert_eq!(station.ptt_snapshot().sources, both());
    station
        .ptt_source(PttSource::Manual, false)
        .expect("release is never refused");
    assert_eq!(
        station.ptt_snapshot().sources,
        PttSource::Bluetooth.bit(),
        "releasing Manual must not drop Bluetooth's hold"
    );
    // Longer than one M17 run-loop poll, so a stray unkey would have landed.
    thread::sleep(Duration::from_millis(150));
    assert!(
        station.snapshot().ptt,
        "still on air through Bluetooth: no unkey reached the console"
    );
    station
        .ptt_source(PttSource::Bluetooth, false)
        .expect("release");
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "the last release unkeys"
    );
    station.m17_disconnect();
}

#[test]
fn unleased_bluetooth_press_is_refused_and_holds_nothing() {
    let _timing = shared_timing();
    let station = linked_station();
    let err = station
        .ptt_source(PttSource::Bluetooth, true)
        .expect_err("a Bluetooth press without a lease must be refused");
    assert_eq!(
        err.to_string(),
        "invalid argument: bluetooth presses need a lease"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "the refused press holds nothing");
    assert_eq!(ptt.latched, 0, "an argument refusal latches nothing");
    // Longer than one M17 run-loop poll, so a stray key would have landed.
    thread::sleep(Duration::from_millis(150));
    assert!(
        !station.snapshot().ptt,
        "nothing reached the console: the link stays unkeyed"
    );
    station
        .ptt_source(PttSource::Bluetooth, false)
        .expect("a Bluetooth release is never refused");
    station.m17_disconnect();
}

#[test]
fn a_dead_watchdog_is_respawned_by_the_next_press() {
    let _timing = timing_guard();
    let station = linked_station();
    // Spawn astar-ptt-watch, then end it while leaving its slot occupied:
    // the state a panicked or otherwise exited watchdog leaves behind.
    station
        .ptt_source(PttSource::Manual, true)
        .expect("manual keys");
    station
        .ptt_source(PttSource::Manual, false)
        .expect("release");
    assert!(
        station.kill_ptt_watch_for_test(),
        "a press spawned the watchdog, so there was one to end"
    );
    let pressed_at = Instant::now();
    station
        .ptt_lease_press(PttSource::Bluetooth, LEASE_TTL)
        .expect("a lease press keys an answered call");
    assert!(
        wait_until_tight(|| station.ptt_snapshot().sources == 0, 2_000),
        "the press must have respawned the watchdog, which releases the lease"
    );
    let released_after = pressed_at.elapsed();
    assert!(
        released_after <= LEASE_TTL + WATCHDOG_SLACK,
        "released later than TTL + 100 ms: {released_after:?}"
    );
    assert_eq!(
        station.ptt_snapshot().forced,
        ForcedRecord {
            seq: 1,
            sources: PttSource::Bluetooth.bit(),
            reason: Some(ForcedReason::LeaseExpired),
        }
    );
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "the console must follow the respawned watchdog's unkey"
    );
    station.m17_disconnect();
}

#[test]
fn a_panicking_watchdog_tick_does_not_end_enforcement() {
    let _timing = timing_guard();
    let station = linked_station();
    let pressed_at = Instant::now();
    station
        .ptt_lease_press(PttSource::Bluetooth, LEASE_TTL)
        .expect("a lease press keys an answered call");
    // The next tick panics while holding the arbiter guard, poisoning it.
    station.panic_next_ptt_watch_tick_for_test();
    assert!(
        wait_until_tight(|| station.ptt_snapshot().sources == 0, 2_000),
        "the watchdog must outlive a panicking tick and still release the lease"
    );
    let released_after = pressed_at.elapsed();
    assert!(
        released_after <= LEASE_TTL + WATCHDOG_SLACK,
        "released later than TTL + 100 ms after a panicking tick: {released_after:?}"
    );
    assert_eq!(
        station.ptt_snapshot().forced,
        ForcedRecord {
            seq: 1,
            sources: PttSource::Bluetooth.bit(),
            reason: Some(ForcedReason::LeaseExpired),
        }
    );
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "the console must follow the unkey"
    );
    station.m17_disconnect();
}

// ---- the transmit time-out ------------------------------------------------

#[test]
fn injected_tx_timeout_unkeys_through_the_watchdog_and_latches() {
    let _timing = timing_guard();
    let station = linked_station();
    let timeout = Duration::from_millis(200);
    station
        .set_tx_timeout_for_test(timeout)
        .expect("200 ms is inside the test hook's range");
    let keyed_at = Instant::now();
    // `Ok` is the arbiter's rising edge accepted by the console; the applied
    // console state is not waited for, so a slow run loop cannot eat into
    // the 200 ms window the checks below measure.
    station
        .ptt_source(PttSource::Manual, true)
        .expect("manual keys");
    sleep_until(keyed_at + timeout / 2);
    // Read first, then time: the arbiter's clock started after `keyed_at`, so
    // a read taken before `timeout` has elapsed can never see it fired.
    let mid_sources = station.ptt_snapshot().sources;
    let mid_at = keyed_at.elapsed();
    assert!(
        mid_at < timeout,
        "the test thread overslept the half-way check: {mid_at:?}"
    );
    assert_eq!(
        mid_sources,
        PttSource::Manual.bit(),
        "the time-out must not fire early"
    );
    assert!(
        wait_until_tight(|| station.ptt_snapshot().sources == 0, 2_000),
        "the watchdog must apply the transmit time-out"
    );
    let fired_after = keyed_at.elapsed();
    assert!(fired_after >= timeout, "fired early: {fired_after:?}");
    assert!(
        fired_after <= timeout + WATCHDOG_SLACK,
        "fired late: {fired_after:?}"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.latched, PttSource::Manual.bit(), "the time-out latches");
    assert_eq!(
        ptt.forced,
        ForcedRecord {
            seq: 1,
            sources: PttSource::Manual.bit(),
            reason: Some(ForcedReason::TxTimeout),
        }
    );
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "the console must be unkeyed"
    );
    assert!(
        matches!(
            station.ptt_source(PttSource::Manual, true),
            Err(StationError::PttLatched)
        ),
        "a press after the time-out needs a release first"
    );
    station
        .ptt_source(PttSource::Manual, false)
        .expect("release re-arms");
    // Back to a real time-out before the fresh press, so the re-armed over
    // cannot be cut off again by the 200 ms one while this test checks it.
    station
        .set_tx_timeout(Duration::from_secs(60))
        .expect("60 s is in range");
    station
        .ptt_source(PttSource::Manual, true)
        .expect("a fresh press after release keys");
    assert_eq!(
        station.ptt_snapshot().sources,
        PttSource::Manual.bit(),
        "the re-armed press is held"
    );
    assert!(
        wait_until(|| station.snapshot().ptt, 1_000),
        "the re-armed press reaches the console"
    );
    station
        .ptt_source(PttSource::Manual, false)
        .expect("release");
    station.m17_disconnect();
}

#[test]
fn set_tx_timeout_refuses_out_of_range_and_keeps_the_old_value() {
    let _timing = shared_timing();
    let station = test_station();
    assert_eq!(DEFAULT_TX_TIMEOUT, Duration::from_secs(120));
    assert_eq!(
        station.tx_timeout(),
        Duration::from_secs(120),
        "new station"
    );
    for bad in [
        Duration::ZERO,
        Duration::from_millis(59_999),
        Duration::from_millis(600_001),
    ] {
        let err = station
            .set_tx_timeout(bad)
            .expect_err("out of range must be refused");
        assert_eq!(
            err.to_string(),
            "invalid argument: transmit time-out must be 60..=600 s",
            "for {bad:?}"
        );
        assert_eq!(
            station.tx_timeout(),
            Duration::from_secs(120),
            "a refused {bad:?} must keep 120 s"
        );
    }
    station
        .set_tx_timeout(Duration::from_secs(60))
        .expect("60 s is the floor");
    assert_eq!(station.tx_timeout(), Duration::from_secs(60));
    station
        .set_tx_timeout(Duration::from_secs(600))
        .expect("600 s is the ceiling");
    assert_eq!(station.tx_timeout(), Duration::from_secs(600));
    assert_eq!(station.ptt_snapshot().tx_timeout, Duration::from_secs(600));
    assert!(matches!(
        station.set_tx_timeout(Duration::ZERO),
        Err(StationError::InvalidArgument(_))
    ));
    assert_eq!(
        station.tx_timeout(),
        Duration::from_secs(600),
        "refusing 0 keeps a non-default value too"
    );
}

#[test]
fn the_test_hook_cannot_disable_the_time_out_either() {
    let _timing = shared_timing();
    let station = test_station();
    for bad in [
        Duration::ZERO,
        Duration::from_micros(999),
        Duration::from_millis(600_001),
    ] {
        let err = station
            .set_tx_timeout_for_test(bad)
            .expect_err("out of the hook's range must be refused");
        assert_eq!(
            err.to_string(),
            "invalid argument: test transmit time-out must be 1 ms..=600 s",
            "for {bad:?}"
        );
        assert_eq!(station.tx_timeout(), Duration::from_secs(120));
    }
    station
        .set_tx_timeout_for_test(Duration::from_millis(1))
        .expect("1 ms is the hook's floor");
    assert_eq!(station.tx_timeout(), Duration::from_millis(1));
}

// ---- stop, latch, re-arm --------------------------------------------------

#[test]
fn stop_all_unkeys_latches_and_renew_is_not_leased() {
    let _timing = shared_timing();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("key");
    station
        .ptt_source(PttSource::Manual, true)
        .expect("manual joins");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    station.ptt_stop_all();
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "stop_all releases everything");
    assert_eq!(ptt.latched, both(), "stop_all latches every held source");
    assert_eq!(ptt.forced.seq, 0, "stop_all is not a forced-stop record");
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "stop_all must unkey the console"
    );
    assert!(matches!(
        station.ptt_lease_renew(PttSource::Bluetooth),
        Err(StationError::NotLeased)
    ));
    assert_eq!(station.ptt_snapshot().sources, 0, "renew never presses");
    station.m17_disconnect();
}

#[test]
fn latched_press_returns_ptt_latched_until_released() {
    let _timing = shared_timing();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("key");
    station.ptt_stop_all();
    assert!(matches!(
        station.ptt_lease_press(PttSource::Bluetooth, LONG_LEASE),
        Err(StationError::PttLatched)
    ));
    assert_eq!(
        station.ptt_snapshot().sources,
        0,
        "a latched press records nothing"
    );
    station
        .ptt_source(PttSource::Bluetooth, false)
        .expect("release re-arms");
    assert_eq!(station.ptt_snapshot().latched, 0);
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("a fresh press after release keys");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    station
        .ptt_source(PttSource::Bluetooth, false)
        .expect("release");
    station.m17_disconnect();
}

#[test]
fn lease_press_refuses_a_non_bluetooth_source_and_a_bad_ttl() {
    let _timing = shared_timing();
    let station = test_station();
    for src in [
        PttSource::Manual,
        PttSource::Vox,
        PttSource::Serial,
        PttSource::Legacy,
    ] {
        let err = station
            .ptt_lease_press(src, LONG_LEASE)
            .expect_err("only Bluetooth takes a lease");
        assert_eq!(
            err.to_string(),
            "invalid argument: lease source must be bluetooth",
            "for {src:?}"
        );
    }
    for ttl in [
        Duration::ZERO,
        Duration::from_micros(999),
        Duration::from_millis(5_001),
    ] {
        let err = station
            .ptt_lease_press(PttSource::Bluetooth, ttl)
            .expect_err("ttl outside 1..=5000 ms");
        assert_eq!(
            err.to_string(),
            "invalid argument: lease ttl must be 1..=5000 ms",
            "for {ttl:?}"
        );
    }
    assert_eq!(
        station.ptt_snapshot().latched,
        0,
        "a refused argument latches nothing"
    );
    // Valid arguments on an idle station reach the console, which refuses;
    // the press is rolled back and latched (held through dialing never keys).
    assert!(matches!(
        station.ptt_lease_press(PttSource::Bluetooth, Duration::from_millis(5_000)),
        Err(StationError::NotConnected)
    ));
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0);
    assert_eq!(ptt.latched, PttSource::Bluetooth.bit());
    assert!(matches!(
        station.ptt_lease_renew(PttSource::Bluetooth),
        Err(StationError::NotLeased)
    ));
}

#[test]
fn legacy_refused_press_is_not_latched() {
    let _timing = shared_timing();
    let station = test_station();
    assert!(matches!(
        station.set_ptt(true),
        Err(StationError::NotConnected)
    ));
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "the refused Legacy press is rolled back");
    assert_eq!(ptt.latched, 0, "a refused Legacy press is not latched");
    assert!(
        matches!(station.set_ptt(true), Err(StationError::NotConnected)),
        "a second refused /key gets the console's answer again, never PttLatched"
    );
    assert_eq!(station.ptt_snapshot().latched, 0);
    link(&station);
    station
        .set_ptt(true)
        .expect("once a call answers, /key keys with no set_ptt(false) between");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Legacy.bit());
    assert!(
        wait_until(|| station.snapshot().ptt, 1_000),
        "the console keyed the new call"
    );
    station.set_ptt(false).expect("unkey");
    station.m17_disconnect();
}

#[test]
fn a_legacy_hold_through_a_hangup_is_released_not_latched() {
    let _timing = shared_timing();
    let station = linked_station();
    station.set_ptt(true).expect("key, as POST /key does");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    station.m17_disconnect();
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "the hang-up releases the held /key");
    assert_eq!(ptt.latched, 0, "the hang-up does not latch Legacy");
    link(&station);
    thread::sleep(Duration::from_millis(150));
    assert!(
        !station.snapshot().ptt,
        "the next call never comes up keyed"
    );
    station
        .set_ptt(true)
        .expect("the next call's /key keys with no set_ptt(false) between");
    assert!(
        wait_until(|| station.snapshot().ptt, 1_000),
        "the console keyed the next call"
    );
    // stop_all (never sent by astar-server) is the other event that latches
    // a held Legacy; set_ptt(false) is how a Legacy caller re-arms it.
    station.ptt_stop_all();
    assert_eq!(
        station.ptt_snapshot().latched,
        PttSource::Legacy.bit(),
        "stop_all latches a held Legacy"
    );
    assert!(
        matches!(station.set_ptt(true), Err(StationError::PttLatched)),
        "after stop_all a /key needs an /unkey first"
    );
    assert_eq!(
        station.ptt_snapshot().sources,
        0,
        "a latched /key records nothing"
    );
    station
        .set_ptt(false)
        .expect("set_ptt(false) re-arms Legacy");
    assert_eq!(station.ptt_snapshot().latched, 0);
    station.m17_disconnect();
}

#[test]
fn a_legacy_hold_on_a_call_torn_down_under_the_station_is_released_not_latched() {
    let _timing = shared_timing();
    let (station, session) = shared_station();
    link(&station);
    station.set_ptt(true).expect("key, as POST /key does");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    // Bypass every Station hook: only observe_call(!answered) can notice this.
    session.lock().unwrap().m17_disconnect();
    assert_eq!(
        station.ptt_snapshot().sources,
        PttSource::Legacy.bit(),
        "nothing has observed the loss yet"
    );
    let snap = station.snapshot();
    assert!(
        !snap.ptt,
        "the snapshot that observed the loss reports unkeyed"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(
        ptt.sources, 0,
        "the observed hang-up releases the held /key"
    );
    assert_eq!(ptt.latched, 0, "the observed hang-up does not latch Legacy");
    link(&station);
    station
        .set_ptt(true)
        .expect("the next call's /key keys with no set_ptt(false) between");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Legacy.bit());
    station.set_ptt(false).expect("unkey");
    station.m17_disconnect();
}

#[test]
fn release_and_stop_all_are_never_refused_while_idle() {
    let _timing = shared_timing();
    let station = test_station();
    for src in [
        PttSource::Manual,
        PttSource::Vox,
        PttSource::Serial,
        PttSource::Bluetooth,
        PttSource::Legacy,
    ] {
        station
            .ptt_source(src, false)
            .expect("release is never refused");
    }
    station.ptt_stop_all();
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0);
    assert_eq!(ptt.latched, 0, "stop_all latches only HELD sources");
}

// ---- disconnect, snapshot and Drop ---------------------------------------

#[test]
fn disconnect_latches_every_held_source_and_a_new_call_never_comes_up_keyed() {
    let _timing = shared_timing();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("key");
    station
        .ptt_source(PttSource::Manual, true)
        .expect("manual joins");
    station.disconnect();
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "disconnect releases every source");
    assert_eq!(ptt.latched, both(), "disconnect latches every held source");
    link(&station);
    thread::sleep(Duration::from_millis(150));
    assert!(!station.snapshot().ptt, "a new call never comes up keyed");
    assert!(matches!(
        station.ptt_source(PttSource::Manual, true),
        Err(StationError::PttLatched)
    ));
    station.m17_disconnect();
}

#[test]
fn m17_disconnect_latches_a_held_lease() {
    let _timing = shared_timing();
    let station = linked_station();
    station
        .ptt_lease_press(PttSource::Bluetooth, LONG_LEASE)
        .expect("key");
    station.m17_disconnect();
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0);
    assert_eq!(ptt.latched, PttSource::Bluetooth.bit());
    assert!(matches!(
        station.ptt_lease_renew(PttSource::Bluetooth),
        Err(StationError::NotLeased)
    ));
}

#[test]
fn a_call_torn_down_under_the_station_latches_on_the_next_snapshot() {
    let _timing = shared_timing();
    let (station, session) = shared_station();
    link(&station);
    station.ptt_source(PttSource::Manual, true).expect("key");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    // Bypass every Station hook: only observe_call can notice this.
    session.lock().unwrap().m17_disconnect();
    assert_eq!(
        station.ptt_snapshot().sources,
        PttSource::Manual.bit(),
        "nothing has observed the loss yet"
    );
    let snap = station.snapshot();
    assert!(
        !snap.ptt,
        "the snapshot that observed the loss reports unkeyed"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0);
    assert_eq!(ptt.latched, PttSource::Manual.bit());
}

#[test]
fn a_call_torn_down_under_the_station_latches_on_the_next_try_snapshot() {
    let _timing = shared_timing();
    let (station, session) = shared_station();
    link(&station);
    station.ptt_source(PttSource::Manual, true).expect("key");
    assert!(wait_until(|| station.snapshot().ptt, 1_000));
    session.lock().unwrap().m17_disconnect();
    // `try_snapshot` skips the observation when the arbiter is busy, and
    // astar-ptt-watch (spawned by the press above) takes it every 50 ms, so
    // one call can miss. Poll, bounded: the observation must land within 1 s.
    assert!(
        wait_until(
            || station.try_snapshot().is_some_and(|s| !s.ptt)
                && station.ptt_snapshot().sources == 0,
            1_000
        ),
        "try_snapshot must observe the lost call and release the hold"
    );
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0);
    assert_eq!(ptt.latched, PttSource::Manual.bit());
}

#[test]
fn dropping_the_station_ends_the_watchdog() {
    let _timing = shared_timing();
    let (station, session) = shared_station();
    // Any press spawns astar-ptt-watch, even one the idle console refuses.
    assert!(matches!(
        station.ptt_lease_press(PttSource::Bluetooth, LONG_LEASE),
        Err(StationError::NotConnected)
    ));
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        drop(station);
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_millis(500)).is_ok(),
        "Drop must stop and join astar-ptt-watch within 500 ms"
    );
    assert_eq!(
        Arc::strong_count(&session),
        1,
        "nothing the station left behind may keep the session alive"
    );
}
