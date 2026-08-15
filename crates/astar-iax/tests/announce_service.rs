// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Manager::announce` wiring (iax-6c5d): a routed call can play a CW/PCM
//! announcement to-air; a monitor-only call can play to the local bus.

use std::sync::Arc;

use astar_iax::{
    AnnouncePolicyReq, AnnounceRequest, Destination, Phrase, ResolverConfig, ServiceConfig,
    TtsConfig,
};

mod common;

#[test]
fn announce_pcm_to_air_returns_a_handle() {
    let (mut mgr, call) = common::manager_with_routed_call();
    let pcm: Arc<[i16]> = vec![12_000_i16; 320].into();
    let req = AnnounceRequest {
        phrase: Phrase::Pcm(pcm),
        destination: Destination::ToAir,
        policy: AnnouncePolicyReq::Seize,
        priority: 5,
    };
    let handle = mgr.announce(call, req).expect("announce ok");
    assert!(!handle.is_done(), "announcement is in flight");
}

#[test]
fn announce_to_air_without_mic_errors() {
    let (mut mgr, call) = common::manager_with_monitor_only_call();
    let pcm: Arc<[i16]> = vec![12_000_i16; 320].into();
    let req = AnnounceRequest {
        phrase: Phrase::Pcm(pcm),
        destination: Destination::ToAir,
        policy: AnnouncePolicyReq::Seize,
        priority: 5,
    };
    assert!(
        mgr.announce(call, req).is_err(),
        "no mic → AnnounceUnavailable"
    );
}

#[test]
fn finished_to_air_announcement_auto_unkeys_when_not_operator_keyed() {
    let (mut mgr, call, controls) = common::manager_with_controlled_call();
    let pcm: Arc<[i16]> = vec![5_000_i16; 160].into(); // 1 frame
    let h = mgr.announce(call, common::pcm_to_air(pcm)).unwrap();
    // Drive 3 silent capture callbacks to ensure the PCM drains.
    common::drive_capture(&controls, 3);
    mgr.poll_announcements();
    assert!(h.is_done());
    assert!(
        !common::is_keyed(&mgr, call),
        "auto-unkeyed after announcement"
    );
}

#[test]
fn higher_priority_announcement_preempts_current() {
    let (mut mgr, call, _controls) = common::manager_with_controlled_call();
    let long: Arc<[i16]> = vec![5_000_i16; 8_000].into(); // ~1 s at 8 kHz
    let h_low = mgr
        .announce(call, common::pcm_to_air(long).with_priority(2))
        .unwrap();
    let _h_high = mgr
        .announce(
            call,
            common::pcm_to_air(vec![9_000_i16; 160].into()).with_priority(9),
        )
        .unwrap();
    assert!(
        h_low.is_done(),
        "low-priority current is cancelled on preempt"
    );
}

/// Regression test for the preemption `was_operator_keyed` inheritance bug
/// (iax-6c5d): when a non-operator-keyed announcement is preempted by a
/// higher-priority one, the preemptor must inherit `was_operator_keyed = false`
/// so it auto-unkeys after finishing — rather than leaving the call permanently
/// keyed because it erroneously read `already_keyed = true` from the
/// transiently-auto-keyed call.
#[test]
fn preempted_autokey_still_unkeys_after_preemptor_finishes() {
    let (mut mgr, call, controls) = common::manager_with_controlled_call();

    // Operator is NOT keyed initially.
    assert!(!common::is_keyed(&mgr, call), "call must start unkeyed");

    // Announce a LOW-priority long announcement (auto-keys; was_operator_keyed=false).
    let long: Arc<[i16]> = vec![5_000_i16; 8_000].into(); // ~1 s at 8 kHz
    let h_low = mgr
        .announce(call, common::pcm_to_air(long).with_priority(2))
        .unwrap();
    // The auto-key must have happened.
    assert!(
        common::is_keyed(&mgr, call),
        "low-priority announcement auto-keyed"
    );

    // Preempt with a HIGH-priority SHORT announcement.
    let short: Arc<[i16]> = vec![9_000_i16; 160].into(); // 1 frame
    let h_high = mgr
        .announce(call, common::pcm_to_air(short).with_priority(9))
        .unwrap();
    // The low-priority handle must be cancelled by preemption.
    assert!(
        h_low.is_done(),
        "low-priority current is cancelled on preempt"
    );

    // Drive capture so the high-priority short announcement drains.
    common::drive_capture(&controls, 5);

    // Signal completion.
    mgr.poll_announcements();

    // The high-priority announcement should have finished.
    assert!(
        h_high.is_done(),
        "high-priority announcement must have finished"
    );

    // CRITICAL: the call must be auto-unkeyed because the ORIGINAL operator
    // state was "not keyed". Pre-fix this would fail (call stays keyed forever).
    assert!(
        !common::is_keyed(&mgr, call),
        "call must auto-unkey after preemptor finishes (preempted was_operator_keyed=false inherited)"
    );
}

/// Regression test for the QUEUE-DRAIN `was_operator_keyed` inheritance bug
/// (iax-9aca, Fix C1): when two equal-priority announcements are queued (FIFO,
/// no preemption), the second must inherit `was_operator_keyed = false` from
/// the first so the call is auto-unkeyed after the second finishes.
///
/// Pre-fix repro: operator NOT keyed → `announce(A)` auto-keys
/// (`was_operator_keyed=false`) → `announce(B)` same priority → queued → A
/// finishes → poll starts B via `begin_announcement` which reads
/// `already_keyed = true` (call still keyed from A) and sets
/// `B.was_operator_keyed=true` → B finishes → unkey is skipped → call stays
/// keyed forever (PTT pinned on).
#[test]
fn queue_drain_preserves_was_operator_keyed_across_fifo_chain() {
    let (mut mgr, call, controls) = common::manager_with_controlled_call();

    // Operator is NOT keyed initially.
    assert!(!common::is_keyed(&mgr, call), "call must start unkeyed");

    // Announce A (short, auto-keys; was_operator_keyed=false).
    let pcm_a: Arc<[i16]> = vec![5_000_i16; 160].into(); // 1 frame
    let _h_a = mgr
        .announce(call, common::pcm_to_air(pcm_a).with_priority(5))
        .unwrap();
    assert!(common::is_keyed(&mgr, call), "A auto-keyed the call");

    // Announce B at equal priority — goes to pending queue (FIFO, no preempt).
    let pcm_b: Arc<[i16]> = vec![6_000_i16; 160].into(); // 1 frame
    let _h_b = mgr
        .announce(call, common::pcm_to_air(pcm_b).with_priority(5))
        .unwrap();

    // Drive capture so A drains, then poll — this finishes A and starts B.
    common::drive_capture(&controls, 5);
    mgr.poll_announcements();

    // Call is still keyed (B is now in-flight, inheriting was_operator_keyed=false).
    assert!(
        common::is_keyed(&mgr, call),
        "call must still be keyed while B plays"
    );

    // Drive capture so B drains, then poll — this finishes B and should unkey.
    common::drive_capture(&controls, 5);
    mgr.poll_announcements();

    // CRITICAL (C1): the call must be auto-unkeyed because the original operator
    // state was "not keyed". Pre-fix this would fail (call stays keyed forever).
    assert!(
        !common::is_keyed(&mgr, call),
        "call must auto-unkey after B finishes (was_operator_keyed=false inherited from A)"
    );
}

/// `cw_keys_when_idle=false`: a `MixUnder` CW announcement on an idle (unkeyed)
/// call must NOT key the call (Fix I2).
#[test]
fn cw_keys_when_idle_false_does_not_key_idle_call() {
    let (mut mgr, call, _controls) = common::manager_with_controlled_call();

    // Configure cw_keys_when_idle = false.
    mgr.set_announce_config(ServiceConfig {
        resolver: ResolverConfig::default(),
        mixunder_default_gain_db: -12.0,
        cw_keys_when_idle: false,
        tts: TtsConfig::default(),
    });

    // Operator is NOT keyed.
    assert!(!common::is_keyed(&mgr, call), "call must start unkeyed");

    // Announce a MixUnder (CW-style) announcement to-air.
    let pcm: Arc<[i16]> = vec![5_000_i16; 160].into();
    let req = AnnounceRequest {
        phrase: Phrase::Pcm(pcm),
        destination: Destination::ToAir,
        policy: AnnouncePolicyReq::MixUnder { gain_db: None },
        priority: 5,
    };
    // With cw_keys_when_idle=false and no pre-existing key, the to-air leg is
    // skipped. The announce call may return Ok (monitor-only fallback) or Err
    // (no monitor leg either). Either way, the call must NOT be keyed.
    let _ = mgr.announce(call, req);

    assert!(
        !common::is_keyed(&mgr, call),
        "cw_keys_when_idle=false: idle call must NOT be keyed by MixUnder announcement"
    );
}

/// `cw_keys_when_idle=true` (default): a `MixUnder` announcement on an idle call
/// DOES key (Fix I2 — regression guard, default behaviour preserved).
#[test]
fn cw_keys_when_idle_true_keys_idle_call() {
    let (mut mgr, call, _controls) = common::manager_with_controlled_call();

    // Default config has cw_keys_when_idle = true — no change needed.
    assert!(!common::is_keyed(&mgr, call), "call must start unkeyed");

    let pcm: Arc<[i16]> = vec![5_000_i16; 160].into();
    let req = AnnounceRequest {
        phrase: Phrase::Pcm(pcm),
        destination: Destination::ToAir,
        policy: AnnouncePolicyReq::MixUnder { gain_db: None },
        priority: 5,
    };
    mgr.announce(call, req).expect("announce ok");

    assert!(
        common::is_keyed(&mgr, call),
        "cw_keys_when_idle=true (default): idle call IS keyed by MixUnder announcement"
    );
}
