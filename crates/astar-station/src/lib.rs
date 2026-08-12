// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-station` — one-dependency facade bundling the IAX2 station
//! machinery (console session, audio, the ASL3 WT-connect recipe) behind a
//! thread-safe [`Station`]. The consumer writes only its own event loop and UI.
//! Vendor-neutral: a generic IAX2 station that also offers `AllStar`
//! WT-connect as one convenience method.
//!
//! PTT-source-agnostic: consumers call [`Station::set_ptt`]`(bool)` to key the
//! mic; the PTT hardware driver lives in `astar-ptt` / `astar-serial-sys`
//! and is wired up by the application layer, not this crate.
//!
//! # Poll + snapshot, no callbacks
//!
//! The whole surface is poll-based: a consumer calls [`Station::snapshot`] for
//! live meters/status and [`Station::next_event`] for discrete lifecycle
//! edges. There are no callbacks into a managed runtime, which keeps the later
//! C/Python/Swift bindings simple and sound.
//!
//! # Operating mode: a derived label
//!
//! [`OperatingMode`] (`Wt` / `Node`) is a **derived compatibility label**,
//! not a first-class engine selector. The station always runs a single
//! [`ConsoleSession`] / `Manager` engine. `Node` is reported while an inbound
//! listener is running (via [`Station::enable_inbound`] or the back-compat
//! [`Station::set_mode`] shim); `Wt` otherwise. There is no separate
//! `NodeEngine`. Inbound listening and outbound registration are independent
//! opt-in capabilities (see [`Station::enable_inbound`] / [`Station::register`])
//! that can coexist with an active outbound dial.
//!
//! # Secret-free
//!
//! Call secrets (the guest secret and, on the WT path, a minted token) are
//! call-time arguments consumed into the session. They never appear in a
//! snapshot, an event, a device list, `Debug`, or any tracing line.

mod config;
mod error;
mod event;
mod station;

pub use config::{AnswerPolicy, InboundConfig, NodeConfig, RegisterConfig, StationConfig};
pub use error::StationError;
pub use event::StationEvent;
pub use station::{DEFAULT_DTMF_GAP_MS, DEFAULT_DTMF_MS, DtmfMode, SecretResolver, Station};

// Re-export the consumer surface behind one dependency.
pub use astar_asl3::{Asl3Error, PortalCredentials};
// Policy types referenced by `NodeConfig::policy`, so a consumer can tune
// inbound auth without depending on `astar-iax` directly.
pub use astar_audio::{
    CharacterizeOpts, CpalBackend, DeviceInfo, MicProfile, NotchSpec, SPECTRUM_BINS,
};
pub use astar_console::{
    CallStatus, ConsoleConfig, ConsoleSession, ConsoleState, OperatingMode, VoiceFormat,
};
pub use astar_iax::{
    CallMode, CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy, IncomingCallTokenPolicy,
    KnownNodes, LinkEvent, LinkMode, LinkRoster, LinkSnapshot, LinkState, WgConfigError,
    WgLinkConfig,
};
// The D-Star state surface returned by [`Station::dstar_state`], re-exported
// so a consumer (the C-ABI in `astar-sys`, a GUI) can name and
// destructure it without taking `astar-console`, `astar-dstar` or
// `astar-codec` as direct dependencies (iax-4c8e).
#[cfg(feature = "dstar")]
pub use astar_console::{AmbeBackend, DstarLinkState, DstarSnapshotState};

/// Common imports for a station consumer.
pub mod prelude {
    pub use crate::{
        AnswerPolicy, NodeConfig, RegisterConfig, Station, StationConfig, StationError,
        StationEvent,
    };
    pub use astar_console::{CallStatus, ConsoleState, OperatingMode};
}
