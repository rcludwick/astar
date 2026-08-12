// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station::connect_wt_at` — explicit-address `WebTransceiver` dial (iax-5991).
//!
//! The address is parsed BEFORE the token mint, so these run fully offline:
//! a bad address fails fast at parse (no portal/network needed), and a good
//! address with no portal config proves the parse path reached the mint step
//! (→ `Portal`) WITHOUT consulting the `AllStar` directory (`resolve_node`).

use astar_station::{Station, StationConfig, StationError};

fn test_station(cfg: StationConfig) -> Station {
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

#[test]
fn connect_wt_at_bad_address_fails_at_parse_before_mint() {
    // No portal configured. A garbage/empty address must yield Resolve
    // (parse-first), NOT Portal — proving the address is validated before the
    // token mint and without any network round-trip.
    let s = test_station(StationConfig::default());
    for bad in ["", "   ", "127.0.0.1:99999"] {
        let err = s.connect_wt_at("77777", bad).unwrap_err();
        assert!(
            matches!(err, StationError::Resolve(_)),
            "addr {bad:?} should be Resolve, got {err:?}"
        );
    }
}

#[test]
fn connect_wt_at_good_address_reaches_mint() {
    // Default config has no portal, so a VALID address parses and the mint step
    // then fails with Portal. This proves connect_wt_at got PAST address parsing
    // (and never called resolve_node, which would need the AllStar directory).
    let s = test_station(StationConfig::default());
    let err = s.connect_wt_at("77777", "127.0.0.1:4569").unwrap_err();
    assert!(
        matches!(err, StationError::Portal(_)),
        "valid address with no portal should reach the mint (Portal), got {err:?}"
    );
}
