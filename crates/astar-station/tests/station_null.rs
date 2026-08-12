// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline `Station` tests over the hardware-free `NullBackend`. No real
//! network, no fixed ports: only idle/guard paths are exercised (live dial is
//! Phase 2's parrot acceptance).

use astar_station::{
    AnswerPolicy, CallStatus, NodeConfig, OperatingMode, Station, StationConfig, StationError,
    StationEvent,
};

fn test_station(cfg: StationConfig) -> Station {
    // Inject a NullBackend factory so tests touch no hardware.
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

// --- Task 1.2: new + list_devices ---

#[test]
fn new_station_lists_devices() {
    let s = test_station(StationConfig::default());
    let (inputs, outputs) = s.list_devices().expect("devices");
    assert!(!inputs.is_empty());
    assert!(!outputs.is_empty());
}

// --- Task 1.3: connect / disconnect / snapshot (offline shape) ---

#[test]
fn idle_snapshot_then_disconnect_is_noop() {
    let s = test_station(StationConfig::default());
    let snap = s.snapshot();
    assert!(matches!(snap.status, CallStatus::Idle));
    s.disconnect(); // idempotent, no-op when idle
    assert!(matches!(s.snapshot().status, CallStatus::Idle));
}

#[test]
fn selected_devices_default_from_config_then_set_devices_overrides() {
    let cfg = StationConfig {
        input: Some("mic-default".to_string()),
        output: Some("spk-default".to_string()),
        ..StationConfig::default()
    };
    let s = test_station(cfg);
    // Seeded from config.
    assert_eq!(
        s.selected_devices(),
        (
            Some("mic-default".to_string()),
            Some("spk-default".to_string())
        )
    );
    // A later pick (the per-connect UI selection) overrides for the next dial.
    s.set_devices(Some("usb-mic".to_string()), None);
    assert_eq!(s.selected_devices(), (Some("usb-mic".to_string()), None));
}

// --- Task 1.4: connect_wt without portal config errors cleanly ---

#[test]
fn connect_wt_without_portal_is_portal_error() {
    let s = test_station(StationConfig::default()); // portal = None
    let err = s.connect_wt("55553").unwrap_err();
    assert!(matches!(err, StationError::Portal(_)));
}

// --- Task 1.5: set_ptt + gains ---

#[test]
fn set_ptt_idle_is_not_connected() {
    let s = test_station(StationConfig::default());
    assert!(matches!(s.set_ptt(true), Err(StationError::NotConnected)));
}

#[test]
fn gains_do_not_panic() {
    let s = test_station(StationConfig::default());
    s.set_input_gain(0.5);
    s.set_output_gain(1.5);
}

#[test]
fn rx_compression_setters_do_not_panic() {
    // iax-a4e7 PHASE 1: RX/output compression on/off + strength, mirroring
    // the mic-side compression setters' own smoke-test depth at this layer.
    let s = test_station(StationConfig::default());
    s.set_rx_compression(true);
    s.set_rx_compression_level(0.5);
    s.set_rx_compression(false);
}

// --- Task 1.6: next_event via snapshot diffing ---

#[test]
fn idle_yields_no_event() {
    let s = test_station(StationConfig::default());
    assert_eq!(s.next_event(), None);
}

// --- Phase 4: operating-mode switch ---

#[test]
fn default_mode_is_wt_and_snapshot_reports_it() {
    let s = test_station(StationConfig::default());
    assert_eq!(s.mode(), OperatingMode::Wt);
    assert_eq!(s.snapshot().mode, OperatingMode::Wt);
}

#[test]
fn set_mode_to_current_is_noop_with_no_event() {
    let s = test_station(StationConfig::default());
    assert!(s.set_mode(OperatingMode::Wt).is_ok());
    assert_eq!(s.mode(), OperatingMode::Wt);
    assert_eq!(s.next_event(), None); // no spurious ModeChanged
}

#[test]
fn set_mode_node_starts_engine_and_switch_back_to_wt_stops_it() {
    // Bind an ephemeral loopback port so the test never collides with the
    // default 4569 (a running node / anything else already on that port).
    let cfg = StationConfig {
        node: Some(astar_station::NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..astar_station::NodeConfig::default()
        }),
        ..StationConfig::default()
    };
    let s = test_station(cfg);
    // Switching to Node over the NullBackend binds the ephemeral listener.
    assert!(s.set_mode(OperatingMode::Node).is_ok());
    assert_eq!(s.mode(), OperatingMode::Node);
    assert!(
        s.node_bind_addr().is_some(),
        "Node mode binds a listener with an address"
    );
    // The switch surfaces as a ModeChanged(Node) edge.
    assert_eq!(
        s.next_event(),
        Some(StationEvent::ModeChanged(OperatingMode::Node))
    );

    // Switching back to Wt drops the engine (no bound listener).
    assert!(s.set_mode(OperatingMode::Wt).is_ok());
    assert_eq!(s.mode(), OperatingMode::Wt);
    assert!(s.node_bind_addr().is_none());
    assert_eq!(
        s.next_event(),
        Some(StationEvent::ModeChanged(OperatingMode::Wt))
    );
}

// --- Phase 6: answer()/reject() guarded in WT mode ---

#[test]
fn answer_reject_in_wt_mode_is_no_pending_call() {
    let s = test_station(StationConfig::default()); // Wt
    assert!(matches!(s.answer(), Err(StationError::NoPendingCall)));
    assert!(matches!(s.reject(), Err(StationError::NoPendingCall)));
}

// --- Phase 5.1: NodeConfig / AnswerPolicy secret-free Debug ---

#[test]
fn node_config_debug_does_not_leak_inbound_secret() {
    use astar_iax::IncomingCallPolicy;
    use astar_iax_core::session::auth::Secret;

    let fake_secret = "inbound-node-secret-xyz";
    let mut policy = IncomingCallPolicy::default();
    policy.credentials.insert(
        "node1".to_string(),
        std::sync::Arc::new(Secret::new(fake_secret.to_string())),
    );

    let cfg = NodeConfig {
        policy,
        answer: AnswerPolicy::Auto,
        ..NodeConfig::default()
    };
    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains(fake_secret),
        "inbound credential leaked into NodeConfig Debug: {dbg}"
    );
}

// --- Secret-free invariant guard ---

#[test]
fn secret_never_appears_in_snapshot_event_or_error() {
    let secret = "topsecret-guest-pw";
    let s = test_station(StationConfig {
        secret: secret.to_string(),
        ..StationConfig::default()
    });

    // Snapshot must not embed the secret anywhere (including the new `calls`
    // field, which is a Vec<CallSnapshot> — each entry is secret-free by
    // construction; assert the whole snapshot Debug is clean, iax-a1fb P5).
    let snap = s.snapshot();
    let dbg = format!("{snap:?}");
    assert!(!dbg.contains(secret), "secret leaked into snapshot Debug");
    // `calls` is empty while idle (no active call), but the type is present.
    assert!(snap.calls.is_empty(), "calls empty while idle");

    // next_event (idle -> None) and its Debug must not embed the secret.
    let ev: Option<StationEvent> = s.next_event();
    assert!(!format!("{ev:?}").contains(secret));

    // A surfaced error must not embed the secret.
    let err = s.set_ptt(true).unwrap_err();
    assert!(!err.to_string().contains(secret));
    assert!(!format!("{err:?}").contains(secret));
}

// --- iax-2377: monitor mode (mic capture without a call) ---

#[test]
fn monitor_start_opens_then_stop_releases_the_device() {
    let s = test_station(StationConfig::default());
    assert!(!s.is_monitoring(), "idle: not monitoring");
    s.monitor_start(None)
        .expect("monitor opens the default input");
    assert!(s.is_monitoring(), "monitor running after start");
    s.monitor_stop();
    assert!(!s.is_monitoring(), "monitor released after stop");
}

#[test]
fn monitor_start_is_idempotent() {
    let s = test_station(StationConfig::default());
    s.monitor_start(None).expect("first start");
    // A second start while already monitoring is a no-op (still Ok, still one).
    s.monitor_start(None).expect("second start is a no-op");
    assert!(s.is_monitoring());
    s.monitor_stop();
    s.monitor_stop(); // stop is idempotent too
    assert!(!s.is_monitoring());
}

#[test]
fn monitor_start_resolves_a_named_input_device() {
    let s = test_station(StationConfig::default());
    // The NullBackend exposes "in:null"; a matching substring resolves.
    s.monitor_start(Some("in:null"))
        .expect("named input resolves and opens");
    assert!(s.is_monitoring());
    s.monitor_stop();
}

// --- iax-e73e: live voice-band mic spectrum tap ---

#[test]
fn mic_spectrum_returns_zero_until_monitoring_then_bins() {
    let s = test_station(StationConfig::default());
    let mut bins = [0.0_f32; astar_audio::SPECTRUM_BINS];
    // Not monitoring → no spectrum.
    assert_eq!(
        s.mic_spectrum(&mut bins),
        0,
        "no spectrum without a monitor"
    );
    // While monitoring → a full set of log bins (floored, since the NullBackend
    // tap is never driven in this offline test).
    s.monitor_start(None).expect("monitor");
    let n = s.mic_spectrum(&mut bins);
    assert_eq!(
        n,
        astar_audio::SPECTRUM_BINS,
        "monitoring yields the full log-bin set"
    );
    s.monitor_stop();
}

// --- iax-5fb6: characterize from monitor capture ---

#[test]
fn characterize_requires_monitoring_then_returns_a_profile() {
    let s = test_station(StationConfig::default());
    // Not monitoring → None.
    assert!(
        s.characterize(false).is_none(),
        "no profile without a monitor"
    );
    // While monitoring → a (secret-free) MicProfile shape, even with no audio
    // driven through the offline NullBackend tap.
    s.monitor_start(None).expect("monitor");
    let profile = s
        .characterize(false)
        .expect("characterize while monitoring");
    assert!(
        profile.gate_threshold_db >= profile.noise_floor_dbfs,
        "gate threshold sits at/above the floor: {profile:?}"
    );
    // The harmonic-comb toggle is a valid argument either way (default off at
    // the FFI, but the engine accepts true too).
    assert!(s.characterize(true).is_some());
    s.monitor_stop();
}

// --- iax-2095: set/clear mic profile ---

#[test]
fn set_mic_profile_apply_and_clear_do_not_panic_when_idle() {
    let s = test_station(StationConfig::default());
    let profile = astar_audio::MicProfile {
        highpass_hz: 90.0,
        notches: vec![astar_audio::NotchSpec {
            freq_hz: 588.0,
            q: 20.0,
        }],
        noise_floor_dbfs: -52.0,
        gate_threshold_db: -46.0,
    };
    // Applying a recalled profile while idle is a standing preference (no active
    // call yet) — it must not panic and seeds the next call.
    s.set_mic_profile(Some(profile));
    // Clearing is also a no-op-safe path.
    s.set_mic_profile(None);
}

// --- Task 3.1: register / deregister ---

// --- iax-be48: inbound honors named device selection ---

/// A nonexistent input device query on `enable_inbound` must fail with
/// `StationError::Audio` (device-not-found), proving the inbound path
/// consults the named device rather than silently falling through to the
/// system default. This is the TDD gate: it will fail until `start_inbound`
/// accepts and resolves the device params.
#[test]
fn enable_inbound_with_bad_named_input_fails_device_not_found() {
    let s = test_station(StationConfig::default());
    s.set_devices(Some("nonexistent-xyzzy-input".to_string()), None);
    let cfg = astar_station::InboundConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..astar_station::InboundConfig::default()
    };
    let err = s.enable_inbound(cfg).unwrap_err();
    assert!(
        matches!(err, StationError::Audio(_)),
        "bad named input on inbound must fail with Audio/device error; got {err:?}"
    );
}

/// A nonexistent output device query on `enable_inbound` must also fail,
/// proving the output side is consulted too.
#[test]
fn enable_inbound_with_bad_named_output_fails_device_not_found() {
    let s = test_station(StationConfig::default());
    s.set_devices(None, Some("nonexistent-xyzzy-output".to_string()));
    let cfg = astar_station::InboundConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..astar_station::InboundConfig::default()
    };
    let err = s.enable_inbound(cfg).unwrap_err();
    assert!(
        matches!(err, StationError::Audio(_)),
        "bad named output on inbound must fail with Audio/device error; got {err:?}"
    );
}

/// A valid named device query on `enable_inbound` must succeed, confirming
/// the named-device resolution works when the device exists.
#[test]
fn enable_inbound_with_valid_named_devices_succeeds() {
    let s = test_station(StationConfig::default());
    // NullBackend advertises "in:null" (Input) and "out:null" (Output).
    s.set_devices(Some("in:null".to_string()), Some("out:null".to_string()));
    let cfg = astar_station::InboundConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..astar_station::InboundConfig::default()
    };
    s.enable_inbound(cfg)
        .expect("named devices that exist must succeed on inbound");
    assert!(s.is_listening(), "listener is up after enable_inbound");
    s.disable_inbound();
}

/// No secret resolver configured → `register()` must NOT panic and must
/// surface `StationEvent::RegisterFailed` through the normal poll path.
#[test]
fn register_without_resolver_surfaces_register_failed() {
    let st = test_station(StationConfig::default());
    let cfg = astar_station::RegisterConfig {
        peer: "127.0.0.1:4569".parse().unwrap(),
        username: "99999".into(),
        refresh: std::time::Duration::from_secs(60),
    };
    // No secret_resolver set → register() should not panic and should queue a
    // RegisterFailed station event.
    let _ = st.register(cfg);
    let mut saw_failed = false;
    for _ in 0..8 {
        if let Some(StationEvent::RegisterFailed { .. }) = st.next_event() {
            saw_failed = true;
            break;
        }
    }
    assert!(saw_failed, "no resolver must surface RegisterFailed");
}
