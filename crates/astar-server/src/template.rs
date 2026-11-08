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
//! Kept registration-free and secret-free by default: `[register]` and the
//! non-default parts of `[audio]`/`[announce]` are commented out so the
//! daemon never tries to dial upstream (or assume real audio hardware) using
//! placeholder values. `[secrets]` uses `source = "file"`: every credential
//! lives in `credentials.toml` beside the config, written by the settings
//! page, so a fresh node never learns the inline `secret = "..."` shape and
//! nothing secret is ever typed into this file.

/// The commented template written by [`crate::config::NodeFileConfig::load_or_bootstrap`].
///
/// Every section other than `[listener]`, `[bridge]`, `[control]`, and
/// `[secrets]` is commented out — see `tests/config_bootstrap.rs` for the
/// round-trip test asserting the commented sections parse as absent.
pub const NODE_TOML_TEMPLATE: &str = r#"# astar-server config — generated template (iax-4703 Task 9).
#
# This file was auto-generated because the --config path did not exist. The
# daemon has already loaded it and is running with the safe defaults below
# (listener up, no upstream registration, no credentials yet). Edit this file
# in place, then restart astar-server to pick up your changes — no rebuild
# needed:
#   - fill in [register] node_id below (and uncomment the section) to
#     register with an upstream registrar
#   - set the node's registration password on the settings page at
#     http://127.0.0.1:8730/settings — it is kept in credentials.toml beside
#     this file, never in this one
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
# source = "file" keeps every credential — the registration password, the
# portal password, the Discord bot token, per-node link secrets — in
# credentials.toml beside this file, mode 0600, written by the settings
# page at http://127.0.0.1:8730/settings. Nothing secret goes in THIS file.
source = "file"

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

# [discord]
# Mirror this node's conference into a Discord voice channel. Listen-only:
# the channel hears the net and can never reach it. The bot token is NOT
# configured here — set it on the settings page (it goes to credentials.toml),
# or POST /secrets with username "discord".
# Invite the bot with no privileged gateway intents and permissions=7343104
# (View Channel, Send Messages, Connect, Speak, Mute Members). Send Messages
# posts the bridge's state and its PTT panel into the voice channel's own
# text chat; Mute Members is what enforces the one-open-mic rule. A bot
# invited with an older integer must be re-authorized to gain the rest —
# re-authorizing updates its role in place.
# guild_id = "000000000000000000"
# channel_id = "000000000000000000"
# join_at_start = false
# announce_on_join = true
# Slash commands are off until approvers are listed. See deploy/node.toml.example
# for approvers / state_file / mic_timeout_secs / mic_silence_secs and for the
# limits of a guild-wide server mute.
# approvers = []
# state_file = "/var/lib/astar/discord-approvals.json"
# Reserved — parsed but not yet implemented; the post-filter they would
# configure does not exist, so setting them changes nothing.
# model = "/path/to/unamblify.onnx"   # (reserved - not yet implemented)
# latency = "ll5"                     # (reserved - not yet implemented)

# [echolink]
# Log this node in to EchoLink so an admin can dial EchoLink stations into
# the conference from /admin. Calling out only: no incoming call is ever
# accepted. The callsign must carry -L (a link) or -R (a repeater), and must
# already be validated with EchoLink. The password is NOT configured here —
# set ASTAR_ECHOLINK_PASSWORD, add "echolink:<CALLSIGN>" to credentials.toml,
# or POST /secrets with username "echolink:<CALLSIGN>".
# EchoLink's access policy forbids linking it to VoIP systems people reach
# directly from a computer, so this section refuses to load beside
# [discord], beside [web] public_bind, and beside [stream] enabled = true.
# callsign = "N0CALL-L"
# servers  = ["servers.echolink.org", "naeast.echolink.org", "nasouth.echolink.org", "backup.echolink.org"]
# location = "Anytown, ST"

# [m17]
# Let /dial m17 (Discord) and POST /m17 link the node to one module of an M17
# reflector as a monitor. Off until this section exists.
# WARNING: the node never sends voice to the reflector, but it relays what it
# hears there to every AllStar node and EchoLink station connected to this
# node, and a connected node with a radio puts that on the air. Link only
# reflectors whose traffic you would rebroadcast, and mind your licence's
# rules on retransmission.
# enabled = true
# directory = "bundled"
# [m17.reflectors]
# "M17-XYZ" = "m17.example.net:17000"

# ─── [packet_log] — OPTIONAL: debug log of received media packets ────────────
# One pcap per call for Wireshark, plus jitter-buffer history. Holds other
# people's voices. With debug_mode on, a capture is started from the
# Recordings page and downloads there (/admin/recordings). See
# docs/site/server/packet-log.md.
# [packet_log]
# debug_mode = true                    # default false; shows a capture button
# dir = "/var/lib/astar/packet-logs"   # absolute path
# max_bytes = "1 GB"                   # at least 100 MB; oldest logs go first
"#;
