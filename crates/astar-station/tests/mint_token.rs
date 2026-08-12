// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline tests for `Station::test_mint_token` (iax-6b58): the standalone
//! "validate WT credentials WITHOUT placing a call" path used by astar's
//! credentials Test button (astar-2fde).
//!
//! These drive the mint against a local `tiny_http` stub portal (via the
//! `portal_base` config override), so no live network or real creds are needed.
//! They cover the success path, the failure-category mapping (bad password →
//! `Login`, unknown node → `TokenNotFound`), the no-portal-config guard, and the
//! key invariant: the mint opens NO IAX call / call socket (the session stays
//! idle and `set_ptt` still reports `NotConnected` afterward).

use astar_asl3::{Asl3Error, PortalCredentials};
use astar_station::{CallStatus, Station, StationConfig, StationError};

fn test_station(cfg: StationConfig) -> Station {
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

/// Spawn a stub portal that plays the `AllStar` login.php / webtransceiver.php
/// flow. `good_node` is the node number the stub will issue a token for; any
/// other node (or a missing cookie) yields "Node not found". Returns the base
/// URL. The stub serves exactly `requests` requests then exits.
fn spawn_stub_portal(
    good_node: &'static str,
    requests: usize,
) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", server.server_addr());
    let handle = std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok(rq) = server.recv() else { return };
            let url = rq.url().to_string();
            if url.starts_with("/login.php") {
                let cookie =
                    |v: &[u8]| tiny_http::Header::from_bytes(&b"Set-Cookie"[..], v).unwrap();
                let resp = tiny_http::Response::from_string("ok")
                    .with_header(cookie(b"PHPSESSID=stub123; path=/"))
                    .with_header(cookie(b"allstar_token=jwt-abc; path=/"));
                let _ = rq.respond(resp);
            } else {
                let cookie_hdr = rq
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Cookie"))
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_default();
                let authed = cookie_hdr.contains("PHPSESSID=stub123");
                let node_q = format!("node={good_node}");
                let body = if authed && url.contains(&node_q) {
                    r#"<param name="callingName" value="tok-abcdef"/>"#
                } else {
                    "Node not found"
                };
                let _ = rq.respond(tiny_http::Response::from_string(body));
            }
        }
    });
    (base, handle)
}

fn portal_cfg(base: String, node: &str) -> StationConfig {
    StationConfig {
        portal: Some(PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: node.into(),
        }),
        portal_base: Some(base),
        ..StationConfig::default()
    }
}

#[test]
fn test_mint_token_succeeds_against_stub_portal() {
    let (base, handle) = spawn_stub_portal("77777", 2);
    let s = test_station(portal_cfg(base, "77777"));
    s.test_mint_token().expect("mint succeeds");
    handle.join().unwrap();
}

/// No portal config at all → a clean Portal error (no panic, no call).
#[test]
fn test_mint_token_without_portal_is_portal_error() {
    let s = test_station(StationConfig::default()); // portal = None
    let err = s.test_mint_token().unwrap_err();
    assert!(matches!(err, StationError::Portal(_)));
}

/// Bad password (no session cookie) maps to the `Login` category.
#[test]
fn test_mint_token_bad_password_maps_to_login() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", server.server_addr());
    let handle = std::thread::spawn(move || {
        if let Ok(rq) = server.recv() {
            // No Set-Cookie ⇒ login failure ⇒ Asl3Error::Login.
            let _ = rq.respond(tiny_http::Response::from_string("nope"));
        }
    });
    let s = test_station(portal_cfg(base, "77777"));
    let err = s.test_mint_token().unwrap_err();
    assert!(
        matches!(err, StationError::Portal(Asl3Error::Login)),
        "bad password should map to Login, got {err:?}"
    );
    handle.join().unwrap();
}

/// Unknown node (login OK, but no token in the WT page) maps to `TokenNotFound`.
#[test]
fn test_mint_token_unknown_node_maps_to_token_not_found() {
    // Stub only issues a token for 77777; ask for a node it doesn't own.
    let (base, handle) = spawn_stub_portal("77777", 2);
    let s = test_station(portal_cfg(base, "00000"));
    let err = s.test_mint_token().unwrap_err();
    assert!(
        matches!(err, StationError::Portal(Asl3Error::TokenNotFound)),
        "unknown node should map to TokenNotFound, got {err:?}"
    );
    handle.join().unwrap();
}

/// Network error (nothing listening) maps to the `Http` category.
#[test]
fn test_mint_token_network_error_maps_to_http() {
    // Reserve a port, then drop the listener so the connect is refused.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let base = format!("http://{addr}");
    let s = test_station(portal_cfg(base, "77777"));
    let err = s.test_mint_token().unwrap_err();
    assert!(
        matches!(err, StationError::Portal(Asl3Error::Http(_))),
        "network failure should map to Http, got {err:?}"
    );
}

/// The mint opens NO IAX call / call socket: the session stays Idle and
/// transmit still reports `NotConnected` after a successful mint.
#[test]
fn test_mint_token_opens_no_call() {
    let (base, handle) = spawn_stub_portal("77777", 2);
    let s = test_station(portal_cfg(base, "77777"));
    s.test_mint_token().expect("mint succeeds");
    // No call was placed: status is Idle, and PTT is rejected (no active call).
    assert!(matches!(s.snapshot().status, CallStatus::Idle));
    assert!(matches!(s.set_ptt(true), Err(StationError::NotConnected)));
    s.disconnect(); // idempotent no-op
    assert!(matches!(s.snapshot().status, CallStatus::Idle));
    handle.join().unwrap();
}

/// Secret-free invariant: neither the portal password nor the minted token
/// appears in any surfaced error (success path returns no token at all).
#[test]
fn test_mint_token_error_carries_no_secret() {
    let password = "super-secret-portal-pw";
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
    let base = format!("http://{}", server.server_addr());
    let handle = std::thread::spawn(move || {
        if let Ok(rq) = server.recv() {
            let _ = rq.respond(tiny_http::Response::from_string("nope"));
        }
    });
    let cfg = StationConfig {
        portal: Some(PortalCredentials {
            user: "AJ7HR".into(),
            password: password.into(),
            node: "77777".into(),
        }),
        portal_base: Some(base),
        ..StationConfig::default()
    };
    let s = test_station(cfg);
    let err = s.test_mint_token().unwrap_err();
    assert!(!err.to_string().contains(password));
    assert!(!format!("{err:?}").contains(password));
    handle.join().unwrap();
}

/// LIVE end-to-end mint against the real `AllStarLink` portal — opt-in only,
/// mirroring the `IAX_PARROT_LIVE` gating used elsewhere for network/hardware
/// paths. Set `IAX_PORTAL_LIVE=1` plus `ASL_USER` / `ASL_PASS` / `ASL_NODE`
/// (same env names as the `live_mint` example) to exercise it; otherwise it is
/// a no-op. The library itself never reads env — the test (not the lib) does.
#[test]
fn test_mint_token_live_when_opted_in() {
    if std::env::var("IAX_PORTAL_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping live portal mint (set IAX_PORTAL_LIVE=1 + ASL_USER/ASL_PASS/ASL_NODE)");
        return;
    }
    let var = |k: &str| std::env::var(k).expect("set the live portal env var");
    let cfg = StationConfig {
        portal: Some(PortalCredentials {
            user: var("ASL_USER"),
            password: var("ASL_PASS"),
            node: var("ASL_NODE"),
        }),
        // portal_base = None → the real portal.
        ..StationConfig::default()
    };
    let s = test_station(cfg);
    // Validates credentials with no call placed; the token is discarded.
    s.test_mint_token().expect("live portal mint succeeds");
    assert!(matches!(s.snapshot().status, CallStatus::Idle));
}
