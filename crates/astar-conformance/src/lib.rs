// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Conformance test harness for `astar-iax-core`.
//!
//! The primary entry point is the [`replay`] module, which iterates a
//! libpcap or pcapng capture file, extracts every UDP datagram on port
//! 4569 (IAX2), and round-trips each payload through
//! [`astar_iax_core::parse`] and [`astar_iax_core::encode`].
//!
//! The replay harness is the "asserting half" of `au` ticket `iax-7022`.
//! Real captures (a registration, a `NEW` with `CALLTOKEN`, an in-call mini
//! stream, ...) are dropped into `crates/astar-conformance/fixtures/` and the
//! integration test at `tests/replay.rs` picks them up automatically.

pub mod driver;
pub mod replay;
pub mod scenarios;

pub use replay::{RecordedFrame, ReplayAssertion, ReplayError, ReplayFixture, ReplayStats, play};
