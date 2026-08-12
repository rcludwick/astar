// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![no_main]
//! Fuzz target: feed arbitrary bytes to `astar_iax_core::parse` and assert
//! the parser never panics. Any `Err` is fine; only an unwinding panic or
//! abort would be a real bug.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = astar_iax_core::parse(data);
});
