// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Front-end-agnostic operator-console core over `astar-iax` (drives a single
//! IAX2 call in WT or Node mode) (iax-dd42). Taps live audio levels and
//! exposes a pollable [`ConsoleState`]. Sync, no async runtime — consumed by
//! the web harness, a later TUI, and the astar Tauri app.

#[cfg(feature = "dstar")]
pub mod dstar;
pub mod dtmf;
#[cfg(feature = "m17")]
pub mod m17;
pub mod metering;
pub mod parrot;
pub mod session;
pub mod state;
pub mod tracer;

pub use astar_audio::{MicProfile, NotchSpec};
pub use astar_iax::{Direction, TracedFrame};
#[cfg(feature = "dstar")]
pub use dstar::{DstarConfig, DstarSession, DstarSnapshotState};
pub use dtmf::{DetectedDigit, DtmfShared, DtmfSource, DtmfTester};
// `AmbeBackend` and `DstarLinkState` are the two enums reachable through
// `DstarSnapshotState`'s own fields, re-exported so a consumer can match on
// them without depending on `astar-dstar` or `astar-codec` directly
// (iax-4c8e). `LinkState` is RENAMED on the way out: this crate already
// re-exports M17's identically-named enum, and an unqualified `LinkState` in
// a consumer that uses both would be ambiguous.
#[cfg(feature = "dstar")]
pub use astar_codec::ambe::AmbeBackend;
#[cfg(feature = "dstar")]
pub use astar_dstar::LinkState as DstarLinkState;
#[cfg(feature = "m17")]
pub use m17::{M17Config, M17Prefs, M17Session, M17SnapshotState};
pub use metering::{Gain, Level, MeteringBackend, peak_to_dbfs};
pub use parrot::{LocalParrot, ParrotPhase, ParrotShared, calibrate_mic};
#[cfg(feature = "dstar")]
pub use session::dstar_available;
pub use session::{
    AnswerPolicy, ConsoleConfig, ConsoleError, ConsoleSession, LinkConnectSpec, LinkKeyResolver,
    RegisterOutcome, list_devices, m17_available, resolve_device,
};
pub use state::{CallSnapshot, CallStatus, ConsoleState, OperatingMode, VoiceFormat};
pub use tracer::{TimelineEvent, Tracer};
