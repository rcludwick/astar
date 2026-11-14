// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Bluetooth LE push-to-talk buttons, as pure logic.
//!
//! This crate decides when a wireless button may key the station. It holds the
//! table that recognises a supported button ([`DEVICES`]), how that button's
//! bytes are read ([`decode`]), and the [`ButtonMachine`], which turns link
//! events into press and release intents.
//!
//! It has no dependencies, no `unsafe`, and it never reads a clock: time is a
//! `Duration` from an arbitrary origin, passed in by the caller. That keeps the
//! whole safety argument for a wireless keying source inside code these tests
//! cover, and lets any client drive it from its own transport. The macOS app
//! drives it through `astar-sys`. `ci/guard-ble-ptt.sh` keeps the crate
//! dependency-free and out of the node daemon.
#![forbid(unsafe_code)]

pub mod devices;
pub mod machine;

pub use devices::{
    BleButtonDevice, DEVICES, Decode, Decoded, DeviceKind, GattCharacteristic, Liveness, NOTIFY,
    Uuid128, accepts_gatt, decode, match_advertised_name,
};
pub use machine::{
    AdapterState, ButtonMachine, Cmds, Epoch, Intent, LinkEvent, LinkStatus, MachineConfig,
    NotifyBytes, Output, ReleaseReason, SignalBand,
};
