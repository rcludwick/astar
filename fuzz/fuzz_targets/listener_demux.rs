// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![no_main]
//! Fuzz target: drive the Listener's pure demux surface — `peek_dest_call`,
//! `is_new`, and `IncomingOffer::from_new_ies` on a parsed full frame — over
//! arbitrary 0..65535-byte UDP payloads, asserting no panic regardless of
//! input. Any `None`/`Err` is fine; only an unwinding panic or abort is a bug.
//!
//! This mirrors the bytes an attacker can put on the wire at the shared
//! listener socket: the demux must never crash the process (iax-8baf Phase G).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Pure dest_call extraction must never panic.
    let _ = astar_iax::listener::peek_dest_call(data);
    let _ = astar_iax::listener::is_new(data);

    // The offer parser runs on attacker-controlled NEW IEs after a lenient
    // parse; it must reject malformed input via `Err`, never panic.
    if let Ok(astar_iax_core::frame::Frame::Full(f)) = astar_iax_core::frame::parse_lenient(data) {
        let _ = astar_iax_core::session::fsm::IncomingOffer::from_new_ies(&f.ies);
    }
});
