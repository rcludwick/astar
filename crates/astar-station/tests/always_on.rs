// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Task 1.2: `dial`/`connect_wt` are always legal — no `NoPendingCall` or
//! `AtCapacity` errors from dialing.

use astar_station::{OperatingMode, Station, StationConfig, StationError};

fn test_station(cfg: StationConfig) -> Station {
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

/// `connect_wt` must never return a mode-gate error, even in Node mode.
#[test]
fn dial_is_legal_without_mode_setup() {
    let st = test_station(StationConfig::default());
    // connect_wt may fail on portal/resolve (no network), but it must NOT
    // fail with a capacity or pending-call error.
    let err = st.connect_wt("99999").err();
    assert!(
        !matches!(
            err,
            Some(StationError::NoPendingCall | StationError::AtCapacity)
        ),
        "connect_wt must never fail with a mode-gate error; got {err:?}"
    );
}

/// Same guard in Node mode: switching to Node then dialing must not be a
/// mode-gate error. (Portal error is expected — no credentials configured.)
#[test]
fn dial_is_legal_in_node_mode() {
    let cfg = StationConfig {
        node: Some(astar_station::NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..astar_station::NodeConfig::default()
        }),
        ..StationConfig::default()
    };
    let st = test_station(cfg);
    st.set_mode(OperatingMode::Node)
        .expect("Node mode must start cleanly with ephemeral port");

    let err = st.connect_wt("99999").err();
    assert!(
        !matches!(
            err,
            Some(StationError::NoPendingCall | StationError::AtCapacity)
        ),
        "connect_wt in Node mode must not fail with a mode-gate error; got {err:?}"
    );
}

/// `Station::call_count` delegates to the session and starts at 0.
#[test]
fn call_count_starts_at_zero() {
    let st = test_station(StationConfig::default());
    assert_eq!(st.call_count(), 0, "idle station has no calls");
}

/// Task 4.1: `set_mode(Node)` is a shim over `enable_inbound`; `mode()` is
/// derived from `is_listening()` so it stays consistent whether the listener
/// was toggled via `set_mode` or `enable_inbound`/`disable_inbound` directly.
#[test]
fn set_mode_node_is_shim_for_enable_inbound() {
    let st = test_station(StationConfig::default());
    st.set_node_config(astar_station::NodeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..astar_station::NodeConfig::default()
    });

    // set_mode(Node) enables the inbound listener — mode() derives Node.
    st.set_mode(OperatingMode::Node)
        .expect("Node mode must bind on loopback:0");
    assert!(st.is_listening(), "is_listening() after set_mode(Node)");
    assert_eq!(
        st.mode(),
        OperatingMode::Node,
        "mode() derives Node from listener running"
    );

    // set_mode(Wt) stops the listener — mode() derives Wt.
    st.set_mode(OperatingMode::Wt)
        .expect("switch back to Wt must succeed");
    assert!(!st.is_listening(), "is_listening() after set_mode(Wt)");
    assert_eq!(
        st.mode(),
        OperatingMode::Wt,
        "mode() derives Wt when listener is stopped"
    );

    // Derived-mode consistency: calling enable_inbound directly (not set_mode)
    // must also make mode() return Node — the derived label tracks capability.
    let cfg = astar_station::InboundConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ..astar_station::InboundConfig::default()
    };
    let st2 = test_station(StationConfig::default());
    st2.enable_inbound(cfg)
        .expect("enable_inbound on loopback:0 must succeed");
    assert_eq!(
        st2.mode(),
        OperatingMode::Node,
        "mode() must derive Node when enable_inbound is called directly"
    );
    st2.disable_inbound();
    assert_eq!(
        st2.mode(),
        OperatingMode::Wt,
        "mode() must derive Wt after disable_inbound"
    );
}

/// Task 2.2: `enable_inbound`/`disable_inbound` toggle `is_listening`, and
/// `connect_wt` stays legal throughout (no `ModeMismatch`).
#[test]
fn enable_then_disable_inbound_toggles_listening() {
    let st = test_station(StationConfig::default());
    let cfg = astar_station::InboundConfig {
        bind: "127.0.0.1:0".parse().unwrap(), // ephemeral loopback port
        ..astar_station::InboundConfig::default()
    };

    st.enable_inbound(cfg)
        .expect("bind on loopback:0 must succeed");
    assert!(
        st.is_listening(),
        "is_listening() must be true after enable_inbound"
    );

    st.disable_inbound();
    assert!(
        !st.is_listening(),
        "is_listening() must be false after disable_inbound"
    );

    // Dialing stays legal throughout — must NOT return a mode-gate error.
    let err = st.connect_wt("99999").err();
    assert!(
        !matches!(
            err,
            Some(StationError::NoPendingCall | StationError::AtCapacity)
        ),
        "connect_wt must never fail with a mode-gate error; got {err:?}"
    );
}
