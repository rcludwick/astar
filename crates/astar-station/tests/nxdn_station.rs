// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The NXDN facade with the feature OFF and ON, and never on the air: every
//! address here is `127.0.0.1` or unroutable on purpose.
//!
//! NXDN is hardware-only for the same reason D-Star and YSF are — AMBE+2, one
//! `ThumbDV` — so nothing here opens a link that makes sound. What is
//! asserted is the part that is decidable without a dongle: the argument
//! refusals (which run BEFORE the `#[cfg]` block and so hold identically
//! either way), the availability probe, and the three-step facade's failure
//! path, which must hand the station's one audio lane back rather than wedge
//! it against every later connect.
//!
//! `IAX_THUMBDV_PORT` is pointed at a path no VID/PID scan can ever return,
//! so the candidate list comes back empty and nothing is opened — not the
//! `ThumbDV`, and certainly not a radio interface's serial port.

#[cfg(feature = "nxdn")]
use std::sync::{Mutex, OnceLock};

use astar_station::{Station, StationConfig, StationError};

/// Serializes the tests that set the process-global `IAX_THUMBDV_PORT`
/// against each other, so `cargo test`'s default parallel execution never
/// lets one test see another's pinned path. `unwrap_or_else(into_inner)`
/// recovers from a prior panic — mutual exclusion is all that is needed here,
/// not the (nonexistent) guarded data.
#[cfg(feature = "nxdn")]
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

#[test]
fn an_empty_callsign_is_refused_before_anything_opens() {
    let st = test_station();
    let e = st.nxdn_connect("127.0.0.1:41400", "", 4242, 31_313);
    assert!(matches!(e, Err(StationError::Nxdn(_))), "got {e:?}");
}

#[test]
fn a_zero_radio_id_is_refused_before_anything_opens() {
    let st = test_station();
    let e = st.nxdn_connect("127.0.0.1:41400", "N0CALL", 0, 31_313);
    assert!(matches!(e, Err(StationError::Nxdn(_))), "got {e:?}");
}

#[test]
fn a_zero_talkgroup_is_refused_before_anything_opens() {
    // `NXDNReflector.cpp` registers a client only when the poll's TG matches
    // its own, and `Reflectors.h: CNXDNReflector::isEmpty` is `m_id == 0`.
    let st = test_station();
    let e = st.nxdn_connect("127.0.0.1:41400", "N0CALL", 4242, 0);
    assert!(matches!(e, Err(StationError::Nxdn(_))), "got {e:?}");
}

/// A refused argument must not have opened the station's one audio lane —
/// the whole point of validating before the `#[cfg]` block and before the
/// route is taken.
#[test]
fn a_refused_argument_leaves_no_route_reserved() {
    let st = test_station();
    assert!(
        st.nxdn_connect("127.0.0.1:41400", "", 4242, 31_313)
            .is_err()
    );
    // The tell: a later connect fails at ITS OWN validation rather than with
    // `AlreadyConnected` from a lane the first attempt never gave back.
    match st.nxdn_connect("127.0.0.1:41400", "N0CALL", 0, 31_313) {
        Err(StationError::Nxdn(_)) => {}
        other => panic!("the lane was not left alone — the retry gave {other:?}"),
    }
}

/// The methods exist and answer honestly with the feature off, so a
/// downstream caller never needs its own `#[cfg]`.
#[cfg(not(feature = "nxdn"))]
#[test]
fn the_methods_exist_and_answer_honestly_with_the_feature_off() {
    let st = test_station();
    assert!(!st.nxdn_available());
    st.nxdn_disconnect(); // no-op, must not panic
    let e = st.nxdn_connect("127.0.0.1:41400", "N0CALL", 4242, 31_313);
    assert!(
        matches!(e, Err(StationError::Nxdn(ref m)) if m.contains("not compiled")),
        "got {e:?}"
    );
}

/// A failed connect must not leave the station's ONE audio lane reserved.
///
/// The facade opens the lane BEFORE building the link, and that reservation
/// is what every other connect path refuses against — so a connect that dies
/// at the dongle probe (the overwhelmingly common failure: no dongle
/// attached) would wedge the station against every later connect of any
/// network, until process restart.
#[cfg(feature = "nxdn")]
#[test]
fn a_failed_nxdn_connect_leaves_no_route_reserved() {
    let _env = env_lock();
    // SAFETY: serialized by `env_lock`; no other test in this binary reads or
    // writes `IAX_THUMBDV_PORT` while the guard is held.
    unsafe {
        std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-NOSUCHDEVICE");
    }
    let station = test_station();
    let first = station.nxdn_connect("127.0.0.1:41400", "N0CALL", 4242, 31_313);
    let second = station.nxdn_connect("127.0.0.1:41400", "N0CALL", 4242, 31_313);
    // SAFETY: same serialization as the `set_var` above.
    unsafe {
        std::env::remove_var("IAX_THUMBDV_PORT");
    }

    assert!(
        matches!(first.expect_err("no ThumbDV"), StationError::Nxdn(_)),
        "a connect with no ThumbDV available must fail as an NXDN error"
    );
    // The tell: a station still holding the lane refuses the retry with
    // `AlreadyConnected` instead of failing at the dongle again.
    match second.expect_err("still no ThumbDV") {
        StationError::Nxdn(_) => {}
        other => panic!("the lane was not released — the retry failed with {other:?}"),
    }
    assert!(
        station.nxdn_state().is_none(),
        "no link may be installed by a failed connect"
    );
}

/// The availability probe is the ONE cached `ThumbDV` enumeration every
/// dongle network reads: one dongle, one answer, never two caches that
/// disagree about whether it is plugged in.
#[cfg(feature = "nxdn")]
#[test]
fn nxdn_available_matches_the_shared_thumbdv_probe() {
    let station = test_station();
    assert_eq!(
        station.nxdn_available(),
        astar_console::nxdn_available(),
        "the facade must not grow a probe of its own"
    );
}

/// With no link live there is nothing to report, and `nxdn_disconnect` on an
/// idle station is a no-op rather than a panic or a released lane.
#[cfg(feature = "nxdn")]
#[test]
fn an_idle_station_has_no_nxdn_state_and_disconnect_is_a_no_op() {
    let station = test_station();
    assert!(station.nxdn_state().is_none());
    station.nxdn_disconnect();
    station.nxdn_disconnect();
    assert!(station.nxdn_state().is_none());
    assert!(!station.snapshot().nxdn_active);
}
