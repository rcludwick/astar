// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar-server: a launch-and-leave `AllStarLink` node daemon over the
//! iax-a1fb always-on `Station`. Ports-and-adapters: a `NodeController` core
//! with HTTP+SSE and stdin/TUI adapters.

pub mod command;
pub mod config;
pub mod controller;
pub mod dtmf_commands;
pub mod http;
pub mod run;
pub mod secrets;
pub mod server;
pub mod sse;
pub mod template;
pub mod tui;
