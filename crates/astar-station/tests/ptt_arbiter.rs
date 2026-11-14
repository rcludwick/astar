// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure `PttArbiter` tests. The clock is injected: every test takes one
//! `Instant` as its origin and passes offsets from it, so the lease and the
//! transmit time-out are checked to the millisecond on both sides of each
//! boundary without sleeping. Nothing here reaches a console, a socket or a
//! radio: the arbiter only returns what its caller must do.

use std::time::{Duration, Instant};

use astar_station::PressOutcome::{Held, Ignored, KeyNow};
use astar_station::PttSource::{Bluetooth, Legacy, Manual, Serial, Vox};
use astar_station::{
    DEFAULT_TX_TIMEOUT, ForcedReason, ForcedRecord, MAX_TX_TIMEOUT, MIN_TX_TIMEOUT, NotLeased,
    PttArbiter, PttEdge, PttSnapshot, PttSource, TxTimeoutOutOfRange,
};

/// The Bluetooth lease the Mac uses: 1.5 s, renewed every 250 ms.
const TTL: Duration = Duration::from_millis(1_500);
const RENEW_MS: u64 = 250;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// One millisecond before `t`: the last instant a boundary must not fire at.
fn just_before(t: Instant) -> Instant {
    t.checked_sub(ms(1))
        .expect("every test instant is past its origin")
}

fn forced(seq: u32, sources: u32, reason: ForcedReason) -> ForcedRecord {
    ForcedRecord {
        seq,
        sources,
        reason: Some(reason),
    }
}

/// Keeps `src`'s lease alive the way the Mac's 250 ms tick does: renew, then
/// let the watchdog tick, at every 250 ms step strictly after `from_ms` and
/// strictly before `to_ms`. Asserts that nothing fires on the way.
fn renew_until(a: &mut PttArbiter, src: PttSource, t0: Instant, from_ms: u64, to_ms: u64) {
    let mut at = from_ms + RENEW_MS;
    while at < to_ms {
        let now = t0 + ms(at);
        assert_eq!(a.lease_renew(src, now), Ok(()), "renew at {at} ms");
        assert_eq!(a.tick(now), PttEdge::None, "tick at {at} ms");
        at += RENEW_MS;
    }
}

/// Keys `src` alone at `t0`: through a lease for Bluetooth, a plain press otherwise.
fn key_alone(a: &mut PttArbiter, src: PttSource, t0: Instant) {
    let outcome = if src == Bluetooth {
        a.lease_press(src, TTL, t0)
    } else {
        a.press(src, t0)
    };
    assert_eq!(outcome, KeyNow, "{src:?}");
}

// --- OR of sources, latch, rollback (rules 1-3 in ptt_arbiter.rs) ---

#[test]
fn or_release_one_keeps_other() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.press(Serial, t0 + ms(10)), Held);
    assert_eq!(a.release(Manual), PttEdge::None);
    assert!(a.is_on_air());
    assert_eq!(a.snapshot().sources, Serial.bit());
}

#[test]
fn last_release_unkeys() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Vox, t0), KeyNow);
    assert_eq!(a.press(Serial, t0), Held);
    assert_eq!(a.release(Vox), PttEdge::None);
    assert_eq!(a.release(Serial), PttEdge::Unkey);
    assert!(!a.is_on_air());
    assert_eq!(a.snapshot().sources, 0);
    assert_eq!(a.release(Serial), PttEdge::None, "release is idempotent");
}

#[test]
fn refused_press_rolled_back_and_latched() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    // The console refused the key (not connected, NXDN, DMR...): roll it back.
    a.rollback_press(Manual);
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
    assert_eq!(a.press(Manual, t0 + ms(100)), Ignored);
    // The clock the refused press started went with it: nothing times out.
    assert_eq!(a.tick(t0 + secs(121)), PttEdge::None);
    assert_eq!(a.snapshot().forced, ForcedRecord::default());
    assert_eq!(a.release(Manual), PttEdge::None);
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(a.press(Manual, t0 + secs(122)), KeyNow);
}

#[test]
fn held_through_answer_does_not_key() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    // Space held while the call is still dialing: the console refuses it.
    assert_eq!(a.press(Manual, t0), KeyNow);
    a.rollback_press(Manual);
    // The call answers with Space still down. Nothing presses by itself, and
    // key repeat is ignored.
    assert_eq!(a.observe_call(true, 1), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + secs(3)), Ignored);
    assert_eq!(a.tick(t0 + secs(3)), PttEdge::None);
    assert!(!a.is_on_air());
    // Let go and press again: now it keys.
    assert_eq!(a.release(Manual), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + secs(4)), KeyNow);
}

#[test]
fn latched_source_does_not_block_others() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    a.rollback_press(Manual);
    assert_eq!(a.press(Serial, t0 + ms(5)), KeyNow);
    assert!(a.is_on_air());
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (Serial.bit(), Manual.bit()));
}

// --- stop_all, call_ended, observe_call (rules 4-5) ---

#[test]
fn stop_all_releases_and_latches_all() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.press(Vox, t0), Held);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), Held);
    assert_eq!(a.stop_all(), PttEdge::Unkey);
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!(snap.sources, 0);
    assert_eq!(snap.latched, Manual.bit() | Vox.bit() | Bluetooth.bit());
    assert_eq!(
        snap.forced,
        ForcedRecord::default(),
        "stop_all is not a forced stop"
    );
    assert_eq!(a.press(Manual, t0 + ms(10)), Ignored);
    assert_eq!(a.press(Vox, t0 + ms(10)), Ignored);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0 + ms(10)), Ignored);
    // A source that was not holding was not latched.
    assert_eq!(a.press(Serial, t0 + ms(10)), KeyNow);
    assert_eq!(a.stop_all(), PttEdge::Unkey);
    assert_eq!(
        a.snapshot().latched,
        Manual.bit() | Vox.bit() | Serial.bit() | Bluetooth.bit()
    );

    let mut idle = PttArbiter::new();
    assert_eq!(idle.stop_all(), PttEdge::None);
    assert_eq!(idle.snapshot().latched, 0);
}

#[test]
fn call_ended_latches_held() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Serial, t0), KeyNow);
    assert_eq!(a.call_ended(), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Serial.bit()));
    assert_eq!(snap.forced, ForcedRecord::default());
    assert_eq!(a.press(Serial, t0 + secs(5)), Ignored);
    assert_eq!(a.call_ended(), PttEdge::None, "nothing held now");
    assert_eq!(a.snapshot().latched, Serial.bit());
    assert_eq!(a.release(Serial), PttEdge::None);
    assert_eq!(a.press(Serial, t0 + secs(6)), KeyNow);
}

#[test]
fn observe_call_seq_change_latches() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.observe_call(true, 5), PttEdge::None);
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.observe_call(true, 5), PttEdge::None, "same call");
    assert!(a.is_on_air());
    assert_eq!(
        a.observe_call(true, 6),
        PttEdge::Unkey,
        "a different call answered"
    );
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
    assert_eq!(snap.forced, ForcedRecord::default());
    assert_eq!(a.press(Manual, t0 + secs(1)), Ignored);
}

#[test]
fn observe_call_first_observation_adopts() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(
        a.observe_call(true, 42),
        PttEdge::None,
        "the first observation only records the seq"
    );
    assert!(a.is_on_air());
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(a.observe_call(true, 42), PttEdge::None);
    assert!(a.is_on_air());
}

#[test]
fn observe_call_not_answered_latches_held() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.observe_call(true, 3), PttEdge::None);
    assert_eq!(a.press(Serial, t0), KeyNow);
    assert_eq!(a.observe_call(false, 3), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Serial.bit()));

    // Idle and not answered: there is nothing to latch.
    let mut b = PttArbiter::new();
    assert_eq!(b.observe_call(false, 0), PttEdge::None);
    assert_eq!(b.snapshot().latched, 0);
    assert_eq!(b.press(Manual, t0), KeyNow);
}

#[test]
fn observe_call_answer_after_unanswered_only_records() {
    // A call (seq 7) answers and hangs up with nothing held, then the next
    // call answers and Manual keys on it: the first answered observation
    // after the hang-up must only record the new seq, not treat the press as
    // belonging to "a different call" because 7 was the last seq seen.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.observe_call(true, 7), PttEdge::None);
    assert_eq!(
        a.observe_call(false, 7),
        PttEdge::None,
        "nothing held to release"
    );
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(
        a.observe_call(true, 8),
        PttEdge::None,
        "first answered observation after a hang-up only records"
    );
    assert!(a.is_on_air());
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(
        a.observe_call(true, 9),
        PttEdge::Unkey,
        "a later seq change still latches"
    );
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
}

// --- Legacy latches only on the time-out and stop_all (rules 2, 4, 5, 8) ---

#[test]
fn refused_legacy_press_is_not_latched() {
    // astar-server's /key with no call up: the console refuses it. The next
    // /key, once a call answers, must key with no /unkey in between.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Legacy, t0), KeyNow);
    a.rollback_press(Legacy);
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, 0));
    // The clock the refused press started went with it.
    assert_eq!(a.tick(t0 + secs(121)), PttEdge::None);
    assert_eq!(a.snapshot().forced, ForcedRecord::default());
    assert_eq!(a.observe_call(true, 1), PttEdge::None);
    assert_eq!(a.press(Legacy, t0 + secs(122)), KeyNow);
}

#[test]
fn call_end_releases_legacy_without_latch() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Legacy, t0), KeyNow);
    assert_eq!(a.press(Serial, t0 + ms(10)), Held);
    assert_eq!(a.call_ended(), PttEdge::Unkey);
    let snap = a.snapshot();
    // Serial is latched as before; Legacy is released and not latched.
    assert_eq!((snap.sources, snap.latched), (0, Serial.bit()));
    assert_eq!(snap.forced, ForcedRecord::default());
    // The next call's /key keys with no /unkey between.
    assert_eq!(a.press(Legacy, t0 + secs(5)), KeyNow);
}

#[test]
fn observe_call_releases_legacy_without_latch() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    // A different call answers under a held /key.
    assert_eq!(a.observe_call(true, 1), PttEdge::None);
    assert_eq!(a.press(Legacy, t0), KeyNow);
    assert_eq!(a.press(Manual, t0), Held);
    assert_eq!(a.observe_call(true, 2), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
    assert_eq!(a.press(Legacy, t0 + secs(1)), KeyNow);
    // The call goes (no longer answered) under a held /key.
    assert_eq!(a.observe_call(false, 2), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
    assert_eq!(a.press(Legacy, t0 + secs(2)), KeyNow);
}

#[test]
fn stop_all_latches_legacy_until_legacy_off() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Legacy, t0), KeyNow);
    assert_eq!(a.stop_all(), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Legacy.bit()));
    assert_eq!(a.press(Legacy, t0 + ms(10)), Ignored);
    // set_ptt(false) is Legacy's release.
    assert_eq!(a.legacy_off(), PttEdge::None);
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(a.press(Legacy, t0 + ms(20)), KeyNow);
}

// --- Bluetooth lease (rule 6) ---

#[test]
fn lease_expires_at_ttl() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.tick(t0 + ms(1_499)), PttEdge::None);
    assert!(a.is_on_air());
    assert_eq!(a.tick(t0 + ms(1_500)), PttEdge::Unkey);
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Bluetooth.bit()));
    assert_eq!(
        snap.forced,
        forced(1, Bluetooth.bit(), ForcedReason::LeaseExpired)
    );
    assert_eq!(
        a.lease_press(Bluetooth, TTL, t0 + ms(1_600)),
        Ignored,
        "re-arms only on release"
    );
    // The ignored lease press armed nothing: no hold, no renewable lease, and
    // no second expiry later.
    assert_eq!(
        a.snapshot().sources,
        0,
        "an ignored lease press holds nothing"
    );
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(1_700)), Err(NotLeased));
    assert_eq!(a.tick(t0 + ms(3_200)), PttEdge::None);
    assert_eq!(a.snapshot().forced.seq, 1);
}

#[test]
fn lease_expiry_leaves_other_sources_held() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), Held);
    assert_eq!(
        a.tick(t0 + ms(1_500)),
        PttEdge::None,
        "Manual still holds the key"
    );
    assert!(a.is_on_air());
    let snap = a.snapshot();
    assert_eq!(
        (snap.sources, snap.latched),
        (Manual.bit(), Bluetooth.bit())
    );
    assert_eq!(
        snap.forced,
        forced(1, Bluetooth.bit(), ForcedReason::LeaseExpired)
    );
}

#[test]
fn lease_renew_extends_deadline() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(1_000)), Ok(()));
    assert_eq!(a.tick(t0 + ms(2_499)), PttEdge::None);
    assert_eq!(a.tick(t0 + ms(2_500)), PttEdge::Unkey);
}

#[test]
fn lease_press_while_held_rearms_deadline() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0 + ms(1_000)), Held);
    assert_eq!(a.tick(t0 + ms(2_499)), PttEdge::None);
    assert_eq!(a.tick(t0 + ms(2_500)), PttEdge::Unkey);
}

#[test]
fn renew_never_presses() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_renew(Bluetooth, t0), Err(NotLeased));
    assert!(!a.is_on_air());
    assert_eq!(a.snapshot().sources, 0);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.release(Bluetooth), PttEdge::Unkey);
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(250)), Err(NotLeased));
    assert!(!a.is_on_air());
    assert_eq!(a.snapshot().sources, 0);
}

#[test]
fn renew_after_stop_all_is_not_leased() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.stop_all(), PttEdge::Unkey);
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(250)), Err(NotLeased));
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Bluetooth.bit()));
}

#[test]
fn renew_unleased_is_not_leased() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    // Bluetooth keys only through a lease: a plain press is refused and
    // leaves no trace, so no Bluetooth hold can exist without a deadline.
    assert_eq!(a.press(Bluetooth, t0), Ignored);
    assert_eq!(a.snapshot(), PttArbiter::new().snapshot());
    assert!(!a.is_on_air());
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(250)), Err(NotLeased));
    // A plain press of another source carries no deadline: renew refuses it,
    // and nothing expires.
    assert_eq!(a.press(Manual, t0 + ms(300)), KeyNow);
    assert_eq!(a.lease_renew(Manual, t0 + ms(300)), Err(NotLeased));
    assert_eq!(a.tick(t0 + secs(5)), PttEdge::None);
    assert_eq!(a.snapshot().sources, Manual.bit());
}

#[test]
fn renew_at_deadline_is_not_leased() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(1_499)), Ok(()));
    // Deadline is now 2 999 ms. A renew that arrives at it is too late: the
    // lease is already spent, and renewing it would hide the stall.
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(2_999)), Err(NotLeased));
    assert_eq!(a.tick(t0 + ms(2_999)), PttEdge::Unkey);
}

#[test]
fn dstar_forced_unkey_renews_but_never_rekeys() {
    // D-Star's PttGate can unkey the console on its own (its 300 s TOT). The
    // arbiter never sees that and still counts Bluetooth as holding. The
    // lease can only unkey: renew keeps succeeding, and another source joining
    // is Held, which sends nothing to the console, so nothing re-keys the gate
    // until every source has let go.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    // The external unkey happens here.
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(1_000)), Ok(()));
    assert_eq!(a.tick(t0 + ms(1_000)), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + ms(1_100)), Held);
    assert_eq!(a.lease_renew(Bluetooth, t0 + ms(1_250)), Ok(()));
    assert_eq!(a.release(Bluetooth), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + ms(1_300)), Held);
    assert_eq!(a.release(Manual), PttEdge::Unkey);
    assert_eq!(a.press(Manual, t0 + ms(1_400)), KeyNow);
}

// --- Transmit time-out (rules 7-8) ---

#[test]
fn tx_timeout_fires_at_setting() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.tick(just_before(t0 + secs(120))), PttEdge::None);
    assert!(a.is_on_air());
    assert_eq!(a.snapshot().forced, ForcedRecord::default());
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
    assert_eq!(
        snap.forced,
        forced(1, Manual.bit(), ForcedReason::TxTimeout)
    );

    let mut b = PttArbiter::new();
    assert_eq!(b.set_tx_timeout(secs(300)), Ok(()));
    assert_eq!(b.press(Serial, t0), KeyNow);
    assert_eq!(b.tick(just_before(t0 + secs(300))), PttEdge::None);
    assert_eq!(b.tick(t0 + secs(300)), PttEdge::Unkey);
}

#[test]
fn tx_timeout_stops_all_and_latches_every_held_source() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.lease_press(Bluetooth, TTL, t0), KeyNow);
    renew_until(&mut a, Bluetooth, t0, 0, 10_000);
    assert_eq!(a.press(Vox, t0 + secs(10)), Held);
    renew_until(&mut a, Bluetooth, t0, 9_750, 120_000);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    assert!(!a.is_on_air());
    let snap = a.snapshot();
    assert_eq!(snap.sources, 0);
    assert_eq!(snap.latched, Bluetooth.bit() | Vox.bit());
    assert_eq!(
        snap.forced,
        forced(1, Bluetooth.bit() | Vox.bit(), ForcedReason::TxTimeout)
    );
    assert_eq!(a.lease_renew(Bluetooth, t0 + secs(120)), Err(NotLeased));
}

#[test]
fn tx_timeout_survives_handover() {
    // Space at 0 s, the B02PTT joins at 30 s, Space lets go at 60 s: one over,
    // one clock, started by Space.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0 + secs(30)), Held);
    renew_until(&mut a, Bluetooth, t0, 30_000, 60_000);
    assert_eq!(a.release(Manual), PttEdge::None);
    renew_until(&mut a, Bluetooth, t0, 59_750, 120_000);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    assert_eq!(
        a.snapshot().forced,
        forced(1, Bluetooth.bit(), ForcedReason::TxTimeout)
    );
}

#[test]
fn tx_timeout_resets_only_on_unkey() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    // Let go: the radio unkeys and the clock stops, whenever that happens.
    assert_eq!(a.release(Manual), PttEdge::Unkey);
    assert_eq!(a.tick(t0 + secs(130)), PttEdge::None);
    assert_eq!(a.snapshot().forced, ForcedRecord::default());
    // A fresh over gets a fresh clock.
    assert_eq!(a.press(Manual, t0 + secs(131)), KeyNow);
    assert_eq!(a.tick(just_before(t0 + secs(251))), PttEdge::None);
    assert_eq!(a.tick(t0 + secs(251)), PttEdge::Unkey);
}

#[test]
fn tx_timeout_covers_every_source() {
    for src in PttSource::ALL {
        let t0 = Instant::now();
        let mut a = PttArbiter::new();
        key_alone(&mut a, src, t0);
        if src == Bluetooth {
            renew_until(&mut a, src, t0, 0, 120_000);
        }
        assert_eq!(
            a.tick(just_before(t0 + secs(120))),
            PttEdge::None,
            "{src:?}"
        );
        assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey, "{src:?}");
        let snap = a.snapshot();
        assert_eq!(snap.latched, src.bit(), "{src:?}");
        assert_eq!(
            snap.forced,
            forced(1, src.bit(), ForcedReason::TxTimeout),
            "{src:?}"
        );
    }
}

#[test]
fn tx_timeout_change_applies_to_running_clock() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.tick(t0 + secs(70)), PttEdge::None);
    assert_eq!(a.set_tx_timeout(secs(60)), Ok(()));
    assert_eq!(
        a.tick(t0 + secs(70)),
        PttEdge::Unkey,
        "70 s on air >= the new 60 s"
    );

    let mut b = PttArbiter::new();
    assert_eq!(b.press(Manual, t0), KeyNow);
    assert_eq!(b.set_tx_timeout(secs(300)), Ok(()));
    assert_eq!(b.tick(t0 + secs(120)), PttEdge::None);
    assert_eq!(b.tick(t0 + secs(300)), PttEdge::Unkey);
}

#[test]
fn tx_timeout_change_mid_over_applies_to_running_clock() {
    // Lowering the time-out past the elapsed time stops at the next tick and
    // records a TxTimeout; raising it never restarts the clock.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.set_tx_timeout(secs(600)), Ok(()));
    assert_eq!(a.tick(t0 + secs(179)), PttEdge::None);
    assert_eq!(a.set_tx_timeout(secs(60)), Ok(()));
    assert_eq!(a.tick(t0 + secs(180)), PttEdge::Unkey);
    assert_eq!(a.snapshot().forced.reason, Some(ForcedReason::TxTimeout));
    assert_eq!(a.snapshot().forced.sources, Manual.bit());

    let mut b = PttArbiter::new();
    assert_eq!(b.set_tx_timeout(secs(60)), Ok(()));
    assert_eq!(b.press(Manual, t0), KeyNow);
    assert_eq!(b.tick(t0 + secs(30)), PttEdge::None);
    assert_eq!(b.set_tx_timeout(secs(600)), Ok(()));
    assert_eq!(
        b.tick(t0 + secs(599)),
        PttEdge::None,
        "raising it never restarts the clock"
    );
    assert_eq!(
        b.tick(t0 + secs(600)),
        PttEdge::Unkey,
        "600 s from the original key"
    );
}

#[test]
fn vox_rearms_after_gate_closes() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Vox, t0), KeyNow);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    // Still talking: the open gate presses again and is ignored.
    assert_eq!(a.press(Vox, t0 + secs(121)), Ignored);
    assert!(!a.is_on_air());
    // Silence for the hang time closes the gate: that is VOX's release.
    assert_eq!(a.release(Vox), PttEdge::None);
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(a.press(Vox, t0 + secs(125)), KeyNow);
}

#[test]
fn fresh_press_after_rearm_keys() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.press(Serial, t0), Held);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    assert_eq!(a.snapshot().latched, Manual.bit() | Serial.bit());
    assert_eq!(a.release(Manual), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + secs(121)), KeyNow);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (Manual.bit(), Serial.bit()));
    assert_eq!(a.tick(just_before(t0 + secs(241))), PttEdge::None);
    assert_eq!(a.tick(t0 + secs(241)), PttEdge::Unkey);
    assert_eq!(
        a.snapshot().forced,
        forced(2, Manual.bit(), ForcedReason::TxTimeout)
    );
}

#[test]
fn legacy_rearms_on_set_ptt_false() {
    // The transmit time-out is the event that latches Legacy in normal use
    // (astar-server never calls stop_all): the next /key is refused until
    // /unkey.
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Legacy, t0), KeyNow);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    assert_eq!(a.snapshot().latched, Legacy.bit());
    assert_eq!(a.press(Legacy, t0 + secs(121)), Ignored);
    // set_ptt(false) is the only release a Legacy caller has.
    assert_eq!(a.legacy_off(), PttEdge::None);
    assert_eq!(a.snapshot().latched, 0);
    assert_eq!(a.press(Legacy, t0 + secs(130)), KeyNow);
    // set_ptt(false) while another source holds stops everything and clears
    // only Legacy's own latch.
    assert_eq!(a.press(Manual, t0 + secs(131)), Held);
    assert_eq!(a.legacy_off(), PttEdge::Unkey);
    let snap = a.snapshot();
    assert_eq!((snap.sources, snap.latched), (0, Manual.bit()));
}

#[test]
fn set_tx_timeout_rejects_out_of_range_keeps_old() {
    let mut a = PttArbiter::new();
    assert_eq!(a.set_tx_timeout(secs(300)), Ok(()));
    for bad in [
        Duration::ZERO,
        secs(59),
        MIN_TX_TIMEOUT
            .checked_sub(Duration::from_nanos(1))
            .expect("MIN_TX_TIMEOUT is above zero"),
        secs(601),
        MAX_TX_TIMEOUT + Duration::from_nanos(1),
    ] {
        assert_eq!(a.set_tx_timeout(bad), Err(TxTimeoutOutOfRange), "{bad:?}");
        assert_eq!(
            a.tx_timeout(),
            secs(300),
            "{bad:?} must leave the old value"
        );
    }
    assert_eq!(a.set_tx_timeout(secs(60)), Ok(()));
    assert_eq!(a.tx_timeout(), secs(60));
    assert_eq!(a.set_tx_timeout(secs(600)), Ok(()));
    assert_eq!(a.tx_timeout(), secs(600));
    assert_eq!(a.snapshot().tx_timeout, secs(600));
}

#[test]
fn default_tx_timeout_is_120s() {
    assert_eq!(DEFAULT_TX_TIMEOUT, secs(120));
    assert_eq!(MIN_TX_TIMEOUT, secs(60));
    assert_eq!(MAX_TX_TIMEOUT, secs(600));
    let a = PttArbiter::new();
    assert_eq!(a.tx_timeout(), secs(120));
    assert!(!a.is_on_air());
    assert_eq!(
        a.snapshot(),
        PttSnapshot {
            sources: 0,
            latched: 0,
            forced: ForcedRecord {
                seq: 0,
                sources: 0,
                reason: None,
            },
            tx_timeout: secs(120),
        }
    );
    assert_eq!(PttArbiter::default().snapshot(), a.snapshot());
}

#[test]
fn forced_seq_increments() {
    let t0 = Instant::now();
    let mut a = PttArbiter::new();
    assert_eq!(a.press(Manual, t0), KeyNow);
    assert_eq!(a.tick(t0 + secs(120)), PttEdge::Unkey);
    assert_eq!(a.snapshot().forced.seq, 1);
    assert_eq!(a.release(Manual), PttEdge::None);
    assert_eq!(a.lease_press(Bluetooth, TTL, t0 + secs(130)), KeyNow);
    assert_eq!(a.tick(t0 + secs(130) + TTL), PttEdge::Unkey);
    let second = forced(2, Bluetooth.bit(), ForcedReason::LeaseExpired);
    assert_eq!(a.snapshot().forced, second);
    // stop_all, call_ended and legacy_off are not forced stops.
    assert_eq!(a.release(Bluetooth), PttEdge::None);
    assert_eq!(a.press(Manual, t0 + secs(140)), KeyNow);
    assert_eq!(a.stop_all(), PttEdge::Unkey);
    assert_eq!(a.call_ended(), PttEdge::None);
    assert_eq!(a.legacy_off(), PttEdge::None);
    assert_eq!(a.snapshot().forced, second);
}

// --- ABI values ---

#[test]
fn source_bits_and_raw_values_are_the_abi() {
    assert_eq!(PttSource::ALL, [Manual, Vox, Serial, Bluetooth, Legacy]);
    assert_eq!(PttSource::ALL.map(PttSource::bit), [1, 2, 4, 8, 16]);
    for (raw, src) in (0_u32..).zip(PttSource::ALL) {
        assert_eq!(PttSource::from_raw(raw), Some(src));
        assert_eq!(src as u32, raw);
    }
    assert_eq!(PttSource::from_raw(5), None);
    assert_eq!(PttSource::from_raw(u32::MAX), None);
    assert_eq!(ForcedReason::LeaseExpired as u32, 1);
    assert_eq!(ForcedReason::TxTimeout as u32, 2);
}
