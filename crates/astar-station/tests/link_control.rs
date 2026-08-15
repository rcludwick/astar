// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline `Station` node-to-node link control tests (iax-213f, ported to the
//! current by-node-label surface in iax-d829.1) over the hardware-free
//! `NullBackend`. `link_connect_at` dials an explicit address, so these never
//! touch `AllStar` DNS or the network.

use astar_iax::LinkMode;
use astar_station::{Station, StationConfig};

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

#[test]
fn link_connect_at_adds_to_roster_then_disconnect_by_node_clears_it() {
    let s = test_station();
    s.link_connect_at(
        "55553",
        "127.0.0.1:4569",
        LinkMode::Monitor,
        "1999",
        "",
        astar_iax::CallMode::Standard,
        false,
    )
    .expect("link_connect_at");

    let roster = s.link_roster();
    assert_eq!(roster.links.len(), 1, "one link registered");
    assert_eq!(roster.links[0].node, "55553");
    assert_eq!(roster.links[0].mode, LinkMode::Monitor);

    s.link_disconnect("55553").expect("disconnect by node");
    assert!(
        s.link_roster().links.is_empty(),
        "roster empty after disconnect"
    );
}

#[test]
fn transceive_link_is_recorded_with_its_mode() {
    let s = test_station();
    s.link_connect_at(
        "42",
        "127.0.0.1:4569",
        LinkMode::Transceive,
        "1999",
        "",
        astar_iax::CallMode::Standard,
        false,
    )
    .expect("connect transceive");
    let roster = s.link_roster();
    assert_eq!(roster.links.len(), 1);
    assert_eq!(roster.links[0].mode, LinkMode::Transceive);
}

#[test]
fn link_roster_empty_when_idle_and_disconnect_unknown_node_errors() {
    let s = test_station();
    assert!(s.link_roster().links.is_empty(), "idle roster is empty");
    assert!(
        s.link_disconnect("55553").is_err(),
        "disconnecting an unknown node is an error, not a panic"
    );
}

/// A bad explicit address fails fast as a link error (no dial).
#[test]
fn link_connect_at_bad_address_is_error() {
    let s = test_station();
    let err = s.link_connect_at(
        "55553",
        "not a valid addr",
        LinkMode::Monitor,
        "1999",
        "",
        astar_iax::CallMode::Standard,
        false,
    );
    assert!(err.is_err(), "unparseable address must error");
}

/// `link_set_mode` upgrades an existing link in place — still one roster entry,
/// new mode. This is the primitive the node's `*2` → `*3` upgrade path uses.
#[test]
fn link_set_mode_switches_an_existing_link_without_a_second_entry() {
    let s = test_station();
    s.link_connect_at(
        "55553",
        "127.0.0.1:4569",
        LinkMode::Monitor,
        "1999",
        "",
        astar_iax::CallMode::Standard,
        false,
    )
    .expect("connect monitor");
    s.link_set_mode("55553", LinkMode::Transceive)
        .expect("upgrade to transceive");
    let roster = s.link_roster();
    assert_eq!(roster.links.len(), 1, "still one link");
    assert_eq!(roster.links[0].mode, LinkMode::Transceive);
}

/// Idle smoke test (iax-d254): no calls, no digits, no panic.
#[test]
fn drain_dtmf_digits_empty_when_idle() {
    let s = test_station();
    assert!(s.drain_dtmf_digits().is_empty());
}

/// The link roster never carries the dial-time secret.
#[test]
fn link_roster_is_secret_free() {
    let s = test_station();
    s.link_connect_at(
        "55553",
        "127.0.0.1:4569",
        LinkMode::Monitor,
        "1999",
        "hunter2-link-secret",
        astar_iax::CallMode::Standard,
        false,
    )
    .expect("connect");
    let dbg = format!("{:?}", s.link_roster());
    assert!(
        !dbg.contains("hunter2-link-secret"),
        "secret leaked into link roster: {dbg}"
    );
}

#[test]
fn link_connect_at_wt_shape_registers_roster_entry() {
    // iax-5029: a WT-shaped link dial threads through the console seam.
    // Offline (no peer): we assert the roster registration, not the wire.
    let s = test_station();
    s.link_connect_at(
        "55553",
        "127.0.0.1:4569",
        astar_iax::LinkMode::Transceive,
        "allstar-public",
        "allstar",
        astar_iax::CallMode::WebTransceiver {
            node: "55553".into(),
            name: "77777".into(),
        },
        false,
    )
    .expect("wt-shaped link dial should register");
    let roster = s.link_roster();
    assert_eq!(roster.links.len(), 1);
    assert_eq!(roster.links[0].node, "55553");
    // Secret-free: neither the guest secret nor the shape leaks into the roster.
    let dbg = format!("{roster:?}");
    assert!(!dbg.contains("allstar"), "roster must not leak credentials");
}
