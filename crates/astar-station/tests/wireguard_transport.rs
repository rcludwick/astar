// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Station-level `WireGuard` link-transport surface (iax-5bbd): set/clear
//! before connect, refusal while a session is up, and the secret rule (a bad
//! key surfaces the key *reference*, never material). Offline: the tunnel
//! endpoint is a bound-but-silent loopback socket, audio is the hardware-free
//! `NullBackend`, and the engine is observed through the shared session
//! (`Station::with_shared_session`) — per the design, `Station` itself exposes
//! no WG snapshot/event surface in v1.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use astar_console::{ConsoleConfig, ConsoleSession};
use astar_iax::{CodecPolicy, WgLinkConfig};
use astar_station::{InboundConfig, Station, StationConfig, StationError};

/// 32 zero bytes base64 — length-valid x25519 key material (mirrors the
/// engine's own transport tests). Never a real secret.
const KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// A valid `WgLinkConfig` whose endpoint is a caller-bound loopback socket
/// (so any handshake initiations the stack emits stay on 127.0.0.1).
fn wg_cfg(endpoint: SocketAddr) -> WgLinkConfig {
    WgLinkConfig::new(
        "WG_STATION_KEY",
        "10.77.0.3/32",
        KEY32,
        &endpoint.to_string(),
        &[],
        0,
    )
    .expect("valid config")
}

/// A bound-but-silent loopback socket: dial/handshake traffic lands here and
/// is never answered, keeping the test offline and deterministic.
fn sink_socket() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind sink");
    let a = s.local_addr().expect("sink addr");
    (s, a)
}

/// A station sharing its `ConsoleSession` with the test, so the engine's
/// tunnel state can be observed without any station-level WG surface.
fn shared_station() -> (Station, Arc<Mutex<ConsoleSession>>) {
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        StationConfig::default(),
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    (station, session)
}

fn inbound_cfg() -> InboundConfig {
    InboundConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        ..InboundConfig::default()
    }
}

#[test]
fn set_then_clear_before_connect_round_trips_to_plain_udp() {
    let (_sink, ep) = sink_socket();
    let (station, session) = shared_station();
    station
        .set_wireguard(wg_cfg(ep), KEY32)
        .expect("set before connect");
    station.clear_wireguard().expect("clear before connect");
    station
        .enable_inbound(inbound_cfg())
        .expect("engine builds on plain UDP");
    assert!(
        session.lock().unwrap().wg_status().is_none(),
        "cleared back to plain UDP before the engine was built"
    );
}

#[test]
fn set_wireguard_threads_through_to_the_engine() {
    let (_sink, ep) = sink_socket();
    let (station, session) = shared_station();
    station
        .set_wireguard(wg_cfg(ep), KEY32)
        .expect("set before connect");
    station
        .enable_inbound(inbound_cfg())
        .expect("engine build applies the tunnel");
    assert!(
        session.lock().unwrap().wg_status().is_some(),
        "the engine reports a live tunnel after set_wireguard + engine build"
    );
}

#[test]
fn set_wireguard_while_a_session_is_up_is_already_connected() {
    let (_peer, peer_addr) = sink_socket();
    let (_sink, ep) = sink_socket();
    let (station, session) = shared_station();
    // Pool a call directly on the shared session (a dialing call counts: the
    // transport is immutable while ANY call is pooled).
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            ConsoleConfig {
                node: "9999".into(),
                calling_node: "9999".into(),
                secret: "s".into(),
                name: "t".into(),
                input_device: None,
                output_device: None,
                codec_policy: CodecPolicy::default(),
            },
        )
        .expect("dial pools a call");
    let err = station
        .set_wireguard(wg_cfg(ep), KEY32)
        .expect_err("refused while a session is up");
    assert!(
        matches!(err, StationError::AlreadyConnected),
        "engine CallInProgress maps to AlreadyConnected, got: {err:?}"
    );
    // The UDP reset is refused under the same rule.
    let err = station
        .clear_wireguard()
        .expect_err("clear refused while a session is up");
    assert!(
        matches!(err, StationError::AlreadyConnected),
        "clear maps CallInProgress the same way, got: {err:?}"
    );
    station.disconnect();
}

#[test]
fn bad_key_material_surfaces_the_ref_never_the_material() {
    let (_sink, ep) = sink_socket();
    let (station, session) = shared_station();
    // "AAAA" decodes to 3 bytes — length-invalid key material. The set itself
    // is deferred (no engine yet), so it succeeds without touching the key.
    station
        .set_wireguard(wg_cfg(ep), "AAAA")
        .expect("deferred set stores the selection");
    let err = station
        .enable_inbound(inbound_cfg())
        .expect_err("bad key must fail the engine build");
    let msg = err.to_string();
    assert!(msg.contains("WG_STATION_KEY"), "names the reference: {msg}");
    assert!(!msg.contains("AAAA"), "never the material: {msg}");
    // Retained: a retry must NOT silently proceed over plain UDP.
    assert!(
        station.enable_inbound(inbound_cfg()).is_err(),
        "still refused until the config is fixed or cleared"
    );
    // An explicit clear back to UDP recovers.
    station.clear_wireguard().expect("explicit clear");
    station
        .enable_inbound(inbound_cfg())
        .expect("plain UDP after the explicit clear");
    assert!(
        session.lock().unwrap().wg_status().is_none(),
        "no tunnel after the clear"
    );
}
