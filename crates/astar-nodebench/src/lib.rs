// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! nodebench: run astar-server against a real `AllStarLink` (ASL3) node in a
//! podman pod, and report what held and what broke.
//!
//! # Safety
//!
//! There is no radio anywhere in the rig. Every container shares one pod
//! network namespace and talks on `127.0.0.1`; ports are published to the
//! host on `127.0.0.1` only. `AllStar` DNS is never queried and nothing
//! registers with the real `AllStar` portal: node 1999 is the ASL3 node and
//! 1998 is astar-server, and neither has a portal registration.
//!
//! # Layout
//!
//! * [`rig`] brings a topology up and down through `podman` and collects its
//!   logs and captures.
//! * [`node`] drives the ASL3 node: AMI plus `asterisk -rx`, with typed
//!   parsers for the CLI tables.
//! * [`server`] drives astar-server's HTTP control channel and SSE stream.
//! * [`scenario`] is the scenario trait, the registry and the timing helpers.
//! * [`report`] prints the table and writes `report.json`.
//! * [`soak`] holds a link for hours and logs every drop (and plays the
//!   nat topology).
//! * [`serve`] is the results page on 127.0.0.1:8740.
//! * [`speech`] is the committed test phrase and the comparison that says
//!   whether it came back through a link whole.
//!
//! Nothing in `cargo test` needs a container; everything that touches the rig
//! runs from the `nodebench` binary.

pub mod audio;
pub mod node;
pub mod pcap;
pub mod probe;
pub mod report;
pub mod rig;
pub mod scenario;
pub mod serve;
pub mod server;
pub mod soak;
pub mod speech;
pub mod time;
