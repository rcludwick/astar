// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station`'s DMR facade, with the feature OFF and ON, and never on the air:
//! every address here is `127.0.0.1`.
//!
//! DMR is hardware-only for the same reason D-Star, YSF and NXDN are — AMBE+2,
//! one `ThumbDV` — so nothing here opens a link that makes sound. What is
//! asserted is the part that is decidable without a dongle: the argument
//! refusals and the BrandMeister consent gate (all of which run BEFORE the
//! `#[cfg]` block, so they hold identically either way), the availability
//! probe, and the three-step facade's failure path, which must hand the
//! station's one audio lane back rather than wedge it against every later
//! connect.
//!
//! The password appears in exactly one place in this file — the `PASSWORD`
//! constant handed to `dmr_connect` — and every refusal is asserted not to
//! contain it.
//!
//! `IAX_THUMBDV_PORT` is pointed at a path no VID/PID scan can ever return,
//! so the candidate list comes back empty and nothing is opened — not the
//! `ThumbDV`, and certainly not a radio interface's serial port.

#[cfg(feature = "dmr")]
use std::sync::{Mutex, OnceLock};

use astar_station::{Station, StationConfig, StationError};

const PASSWORD: &str = "passw0rd";

/// Serializes the tests that set the process-global `IAX_THUMBDV_PORT`
/// against each other, so `cargo test`'s default parallel execution never
/// lets one test see another's pinned path.
#[cfg(feature = "dmr")]
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A station on a hardware-free backend. `Station::new` would build a real
/// `CpalBackend`; nothing in this file wants a sound card.
fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

#[test]
fn a_station_refuses_an_unset_radio_id_before_it_touches_the_network() {
    let station = test_station();
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        0,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    assert!(matches!(e, Err(StationError::Dmr(_))), "got {e:?}");
}

/// And a radio id wider than the 24 bits a `DMRD` header carries. Both
/// halves of `RadioId::new` are asked before the feature gate, so the answer
/// is the same in a build with no DMR compiled in at all.
#[test]
fn a_station_refuses_a_radio_id_past_twenty_four_bits() {
    let station = test_station();
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        0x0100_0000,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    assert!(matches!(e, Err(StationError::Dmr(_))), "got {e:?}");
}

#[test]
fn a_station_refuses_an_unknown_system_slug() {
    // astar_dmr::DmrNetwork::from_slug answers None rather than guessing, and
    // a target that names a network this build does not know must be refused
    // rather than pointed somewhere else under the operator's own ID.
    let station = test_station();
    let e = station.dmr_connect(
        "not-a-network",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    assert!(matches!(e, Err(StationError::Dmr(_))), "got {e:?}");
}

#[test]
fn a_station_refuses_brandmeister_without_consent() {
    // `astar_dmr::dialable(consented)` is the gate written once so no call
    // site can forget it. The facade is a call site.
    let station = test_station();
    let e = station.dmr_connect(
        "brandmeister",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        91,
        2,
        PASSWORD.into(),
    );
    let Err(StationError::Dmr(message)) = e else {
        panic!("a refusal")
    };
    assert!(
        message.contains("BrandMeister"),
        "the refusal must name the network: {message:?}"
    );
    assert!(!message.contains(PASSWORD));
}

/// And an independent network is NOT gated: the consent check must refuse
/// BrandMeister and nothing else, or the gate is just "DMR is off".
#[test]
fn an_independent_network_is_not_gated() {
    let station = test_station();
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    // With the feature off this is "not compiled"; with it on, no dongle. It
    // is never the consent refusal.
    let Err(StationError::Dmr(message)) = e else {
        panic!("no dongle is attached, so this cannot succeed")
    };
    assert!(
        !message.contains("BrandMeister"),
        "TGIF must not be gated behind BrandMeister's consent: {message:?}"
    );
}

#[test]
fn a_station_refuses_a_timeslot_that_is_not_one_or_two() {
    let station = test_station();
    for slot in [0u8, 3, 255] {
        let e = station.dmr_connect(
            "tgif",
            "127.0.0.1",
            62031,
            3_153_591,
            "KC0ABC",
            31_313,
            slot,
            PASSWORD.into(),
        );
        assert!(matches!(e, Err(StationError::Dmr(_))), "slot {slot}");
    }
}

#[test]
fn a_station_refuses_an_empty_callsign_and_an_empty_password() {
    let station = test_station();
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "",
        31_313,
        2,
        PASSWORD.into(),
    );
    assert!(matches!(e, Err(StationError::Dmr(_))), "got {e:?}");
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        String::new(),
    );
    assert!(matches!(e, Err(StationError::Dmr(_))), "got {e:?}");
}

#[test]
fn no_station_error_ever_carries_the_password() {
    let station = test_station();
    for (system, id) in [
        ("tgif", 0u32),
        ("nope", 3_153_591),
        ("brandmeister", 3_153_591),
    ] {
        let Err(e) = station.dmr_connect(
            system,
            "127.0.0.1",
            62031,
            id,
            "KC0ABC",
            1,
            2,
            PASSWORD.into(),
        ) else {
            panic!("a refusal")
        };
        assert!(!e.to_string().contains(PASSWORD), "{system}: {e}");
        assert!(!format!("{e:?}").contains(PASSWORD), "{system}: {e:?}");
    }
}

/// A refused argument must not have opened the station's one audio lane —
/// the whole point of validating before the `#[cfg]` block and before the
/// route is taken.
#[test]
fn a_refused_argument_leaves_no_route_reserved() {
    let station = test_station();
    assert!(
        station
            .dmr_connect(
                "tgif",
                "127.0.0.1",
                62031,
                0,
                "KC0ABC",
                31_313,
                2,
                PASSWORD.into()
            )
            .is_err()
    );
    // The tell: a later connect fails at ITS OWN validation rather than with
    // `AlreadyConnected` from a lane the first attempt never gave back.
    match station.dmr_connect(
        "not-a-network",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    ) {
        Err(StationError::Dmr(_)) => {}
        other => panic!("the lane was not left alone — the retry gave {other:?}"),
    }
}

/// The methods exist and answer honestly with the feature off, so a
/// downstream caller never needs its own `#[cfg]`.
#[cfg(not(feature = "dmr"))]
#[test]
fn the_methods_exist_and_answer_honestly_with_the_feature_off() {
    let station = test_station();
    assert!(!station.dmr_available());
    station.dmr_disconnect(); // no-op, must not panic
    let e = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    assert!(
        matches!(e, Err(StationError::Dmr(ref m)) if m.contains("not compiled")),
        "got {e:?}"
    );
    assert!(!station.snapshot().dmr_active);
}

/// A failed connect must not leave the station's ONE audio lane reserved.
///
/// The facade opens the lane BEFORE building the link, and that reservation
/// is what every other connect path refuses against — so a connect that dies
/// at the dongle probe (the overwhelmingly common failure: no dongle
/// attached) would wedge the station against every later connect of any
/// network, until process restart.
#[cfg(feature = "dmr")]
#[test]
fn a_failed_dmr_connect_leaves_no_route_reserved() {
    let _env = env_lock();
    // SAFETY: serialized by `env_lock`; no other test in this binary reads or
    // writes `IAX_THUMBDV_PORT` while the guard is held.
    unsafe {
        std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-NOSUCHDEVICE");
    }
    let station = test_station();
    let first = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    let second = station.dmr_connect(
        "tgif",
        "127.0.0.1",
        62031,
        3_153_591,
        "KC0ABC",
        31_313,
        2,
        PASSWORD.into(),
    );
    // SAFETY: same serialization as the `set_var` above.
    unsafe {
        std::env::remove_var("IAX_THUMBDV_PORT");
    }

    let first = first.expect_err("no ThumbDV");
    assert!(
        matches!(first, StationError::Dmr(_)),
        "a connect with no ThumbDV available must fail as a DMR error, got {first:?}"
    );
    assert!(!first.to_string().contains(PASSWORD));
    // The tell: a station still holding the lane refuses the retry with
    // `AlreadyConnected` instead of failing at the dongle again.
    match second.expect_err("still no ThumbDV") {
        StationError::Dmr(_) => {}
        other => panic!("the lane was not released — the retry failed with {other:?}"),
    }
    assert!(
        station.dmr_state().is_none(),
        "no link may be installed by a failed connect"
    );
}

/// The availability probe is the ONE cached `ThumbDV` enumeration every
/// dongle network reads: one dongle, one answer, never two caches that
/// disagree about whether it is plugged in.
#[cfg(feature = "dmr")]
#[test]
fn dmr_available_matches_the_shared_thumbdv_probe() {
    let station = test_station();
    assert_eq!(
        station.dmr_available(),
        astar_console::dmr_available(),
        "the facade must not grow a probe of its own"
    );
}

#[cfg(feature = "dmr")]
#[test]
fn dmr_available_is_answerable_on_a_station_with_no_dongle() {
    let station = test_station();
    let _ = station.dmr_available();
    assert!(station.dmr_state().is_none());
    station.dmr_disconnect();
    station.dmr_disconnect();
    assert!(station.dmr_state().is_none());
    assert!(!station.snapshot().dmr_active);
}
