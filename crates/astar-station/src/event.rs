// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use astar_console::OperatingMode;

/// A drained lifecycle event. The consumer polls [`crate::Station::next_event`],
/// which derives these by diffing successive console snapshots (the console has
/// no separate event queue). Secret-free: carries only a public hangup reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StationEvent {
    /// The peer answered; media is flowing.
    Answered,
    /// The remote end keyed (`true`) or unkeyed (`false`).
    RemotePtt(bool),
    /// The call ended (normal hangup or failed dial); `reason` is the
    /// human-readable cause.
    Hangup { reason: String },
    /// The operating mode changed (Wt <-> Node).
    ModeChanged(OperatingMode),
    /// An inbound call is ringing in Node/Manual mode, awaiting `answer()`/`reject()`.
    /// `from` is the caller's node id / caller-ID (public; never a secret).
    IncomingCall { from: String },
    /// Outbound node registration succeeded (the upstream registrar acked the
    /// REGREQ). Secret-free: carries no credential.
    Registered,
    /// Outbound node registration failed. `reason` is a secret-free, debug-only
    /// description of the failure cause (the registrar password never appears).
    RegisterFailed { reason: String },
}
