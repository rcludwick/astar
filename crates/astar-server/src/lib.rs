// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar-server: a launch-and-leave `AllStarLink` node daemon over the
//! iax-a1fb always-on `Station`. Ports-and-adapters: a `NodeController` core
//! with HTTP+SSE and stdin/TUI adapters.

/// Who may reach which admin route: the route table and its classes.
pub mod access;
/// Admin sign-in: passkeys, security keys, TOTP, sessions, invites.
#[cfg(feature = "auth")]
pub mod auth;
/// Compiled-in features and the printable systemd unit.
pub mod build_info;
pub mod command;
/// The non-contributing conference member the stream and the logger enrol.
pub mod conference_tap;
pub mod config;
pub mod controller;
/// The node's side of the Discord slash commands — adapters and wiring
/// only; every rule lives in `astar_discord::Commander`.
mod discord_commands;
pub mod dtmf_commands;
/// The node's `EchoLink` leg: state, the dial pre-flight, refusals, the card.
mod echolink_node;
#[cfg(test)]
mod golden_tests;
pub mod http;
/// M17 reflector names → addresses, from operator-owned data only.
pub mod m17_directory;
/// What the M17 monitor's audio reaches, and the warning every surface shows.
pub mod m17_relay;
/// The public, read-only listener page: a separate router, never the admin one.
pub mod public;
/// The node logger (`[recording]`): one file per over, with size and age caps.
pub mod recording;
pub mod run;
pub mod secrets;
pub mod server;
pub mod settings;
pub mod settings_store;
pub mod sse;
/// The live audio streams (`[stream]`): tap, encoders, fan-out.
#[cfg(feature = "stream")]
pub mod stream;
pub mod template;
#[cfg(test)]
mod test_support;
pub mod tui;
/// `astar-server vocoder list|add`: trusted AMBE sticks in `[vocoder]`.
pub mod vocoder;
