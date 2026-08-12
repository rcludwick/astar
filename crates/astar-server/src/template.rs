// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Generated template for a fresh `astar-server` config (iax-4703 Task 9).
//!
//! Written verbatim by [`crate::config::NodeFileConfig::load_or_bootstrap`]
//! when the `--config` path does not exist, so a fresh container mounting an
//! empty `/etc/iaxnode` directory boots into a safe default node (listener
//! up, no upstream registration, no real audio hardware assumed) and the
//! operator edits the generated file in place.
//!
//! Kept registration-free and secret-free-in-spirit by default: `[register]`
//! and the non-default parts of `[audio]`/`[announce]` are commented out so
//! the daemon never tries to dial upstream (or assume real audio hardware)
//! using placeholder values. `[secrets]` uses `source = "config"` with
//! `secret = ""` — exactly the Task 8 shape — which is valid-with-warning at
//! load time (see [`crate::config::NodeFileConfig::from_toml_str`]).

/// The commented template written by [`crate::config::NodeFileConfig::load_or_bootstrap`].
///
/// Every section other than `[listener]`, `[bridge]`, `[control]`, and
/// `[secrets]` is commented out — see `tests/config_bootstrap.rs` for the
/// round-trip test asserting the commented sections parse as absent.
pub const NODE_TOML_TEMPLATE: &str = r#"# astar-server config — generated template (iax-4703 Task 9).
#
# This file was auto-generated because the --config path did not exist. The
# daemon has already loaded it and is running with the safe defaults below
# (listener up, no upstream registration, no secret set). Edit this file in
# place, then restart astar-server to pick up your changes — no rebuild
# needed:
#   - fill in [register] node_id below (and uncomment the section) to
#     register with an upstream registrar
#   - fill in [secrets] secret with your node's registration password
#   - uncomment [audio] backend = "none" on a headless VPS/container host
#     with no sound hardware

[listener]
bind      = "0.0.0.0:4569"
answer    = "auto"
max_calls = 8
auth      = "off"

# [audio]
# Uncomment on a headless VPS/container host with no sound hardware — this
# selects the hardware-free NullBackend instead of the real cpal backend.
# backend = "none"

[bridge]
mode                = "bridge"
mix_minus           = true
include_local_radio = false

# [register]
# Uncomment and fill in node_id to register with an upstream registrar. The
# peer below is preset to AllStarLink's register.allstarlink.org.
# peer    = "52.20.63.146:4569"
# node_id = "CHANGE_ME"

[control]
bind = "127.0.0.1:8730"

[secrets]
# source = "config" carries the registration secret inline, right here in
# this file — see [register] above. Fill in `secret` with your node's
# registration password before uncommenting [register]. An empty secret is
# valid at boot: the node still runs, it just can't authenticate
# registration until you fill this in.
source = "config"
secret = ""

# [announce]
# Uncomment to enable voice announcements (TTS via piper) on call/inbound
# events.
# enabled       = true
# id_mode       = "off"
# join_template = "Connected to node {server-node-number}"
#
# [announce.tts]
# enabled = true
# binary  = "/usr/local/bin/piper"
# voice   = "/usr/share/piper/voices/en_GB-cori-medium.onnx"
#
# [announce.events]
# answered = { enabled = true, destination = "to_air" }
"#;
