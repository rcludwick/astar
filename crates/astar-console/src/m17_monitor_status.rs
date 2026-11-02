// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The M17 monitor's status and lifecycle edges, defined outside any `cfg`.
//!
//! `astar-station` and `astar-server` name these in every build, including
//! one without the `m17` feature where the monitor itself does not exist, so
//! they live here rather than in `crate::m17` and name nothing from
//! `astar-codec` or `astar-m17`.

/// A poll-cheap, secret-free view of one M17 monitor leg.
///
/// There is deliberately no keying field: a monitor has no transmit path, so
/// there is nothing to report, and a field that always reads `false` would
/// invite someone to wire it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M17MonitorStatus {
    /// `LinkState::as_str`: "linking" | "linked" | "failed". A monitor never
    /// reports "idle" — it is born linking.
    pub link: &'static str,
    /// A stream packet was admitted within the last 400 ms.
    pub receiving: bool,
    /// Last heard source callsign. Attacker-supplied: render as text only.
    pub talker: Option<String>,
    /// Why the leg failed (one of the shared end reasons), `None` while
    /// healthy. Never carries a secret: M17 has none.
    pub fail_reason: Option<String>,
    /// Stream packets the flood guard dropped (a second concurrent stream,
    /// or audio running more than 200 ms ahead of real time).
    pub dropped_packets: u64,
    /// Buffers the control-only socket refused to send. Always 0 in a
    /// correct build; tests assert it so an attempt to smuggle a stream
    /// packet out is visible rather than silent.
    pub refused_sends: u64,
}

/// A lifecycle edge the console session reports once per transition, so the
/// node can announce it without diffing statuses itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M17MonitorEdge {
    /// linking → linked (the reflector's ACKN arrived).
    Linked,
    /// The leg was reaped; the `String` is one of the shared end reasons.
    Ended(String),
}
