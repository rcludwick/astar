// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![no_main]
//! Fuzz target: parse -> encode -> parse round-trip. When the first parse
//! succeeds, re-encoding and re-parsing must also succeed and produce an
//! equal frame. Catches encode/parse asymmetry.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(frame1) = astar_iax_core::parse(data) else {
        return;
    };
    let mut buf = Vec::with_capacity(data.len());
    astar_iax_core::encode(&frame1, &mut buf);
    let frame2 = match astar_iax_core::parse(&buf) {
        Ok(f) => f,
        Err(e) => panic!(
            "re-parse of encoded frame failed: {e:?}\n original input: {data:02x?}\n re-encoded: {buf:02x?}\n frame: {frame1:?}"
        ),
    };
    assert_eq!(
        frame1, frame2,
        "round-tripped frame differs from original\n input: {data:02x?}\n re-encoded: {buf:02x?}"
    );
});
