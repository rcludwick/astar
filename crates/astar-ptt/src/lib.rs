// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pluggable hardware PTT (iax-53da): a minimal [`PttBackend`] trait for raw
//! key-line I/O, the shared pure [`PttBridge`] decision logic (debounce,
//! polarity, radio-key modes), and a runner thread that composes them with a
//! consumer session via [`PttIo`] callbacks.
//!
//! Safety contract: the radio-key line is re-asserted every tick (a dropped
//! write self-heals) and released on EVERY exit path — normal stop, a
//! persistently failing backend (3 consecutive errors), or a panicking
//! callback — via [`PttBackend::fail_safe`].

mod agent;
mod backend;
mod bridge;
mod cm108;
mod lines;
mod poller;
mod runner;
#[cfg(feature = "serial-tty")]
mod uci150;
#[cfg(feature = "serial-usb")]
mod uci150_usb;

pub use agent::PttAgent;
pub use backend::{PttBackend, PttError};
pub use bridge::{BridgeAction, BridgeConfig, PttBridge, RxKeyMode};
pub use cm108::Cm108Hid;
pub use lines::{KeyLine, ModemPort, RadioLine};
pub use poller::{Poller, TickOutcome};
pub use runner::{PttIo, RadioKeyInput, spawn};
#[cfg(feature = "serial-tty")]
pub use uci150::Uci150Serial;
#[cfg(feature = "serial-usb")]
pub use uci150_usb::Uci150Usb;
