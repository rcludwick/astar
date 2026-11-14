// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! TOML configuration model for the node daemon.
//!
//! `NodeFileConfig` is what lives on disk. By default it is secret-free — no
//! passwords, no tokens — with credentials injected at runtime through the
//! [`crate::secrets::SecretProvider`]: env vars, control-channel
//! `ProvideSecret`, or the credentials file beside this one, selected by
//! `[secrets] source = "env" | "control" | "file"`.
//!
//! `source = "config"` is the one deliberate exception, for an operator who
//! mounts a private `node.toml` with restricted permissions and carries the
//! registration password inline (iax-4703). The generated template does not
//! ship that shape: it ships `source = "file"` and no `secret` key at all.
//! Whatever the source, the manual `Debug` impl below never emits the secret.
//!
//! # Mapping
//! - `[listener].answer` → [`astar_station::AnswerPolicy`] (`"auto"` / `"manual"`)
//! - `[listener].auth`   → [`astar_station::IncomingAuthPolicy`]
//!   (`"required"` / `"optional"` / `"off"`, case-insensitive)
//! - `[secrets].source`  → `"env"`, `"control"`, `"config"`, or `"file"` (case-insensitive)
//! - top-level `codec_policy` → [`astar_station::CodecPolicy`] (iax-31f7)
//!   (`"ulaw_only"` / `"allow_slin"` / `"prefer_slin"` / `"prefer_slin16"`,
//!   which is the default when the key is absent — see `parse_codec_policy`),
//!   applied to both `to_inbound()`'s policy and (by the
//!   caller, in `main.rs`) the Station's outbound `StationConfig.codec_policy`.
//!   `"prefer_slin16"` also pins the Station's own audio pipeline to 16 kHz
//!   (iax-4348).
//! - `[bridge].mode = "parrot"` (iax-feab) → [`astar_iax::BridgeMode::Parrot`];
//!   the optional `[parrot]` section (`playback_delay_ms` / `silence_gap_ms` /
//!   `vox_threshold_db`) tunes [`astar_audio::ParrotTuning`] — see
//!   [`NodeFileConfig::to_bridge_config`]. `[parrot]` is ignored when
//!   `bridge.mode` is anything else. Its `max_record_ticks` (10 s hard cap) is
//!   NOT TOML-exposed.
//! - Unknown strings for any of these fields are rejected by [`NodeFileConfig::from_toml_str`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use astar_iax::{BridgeConfig, BridgeMode};
use astar_station::{
    AnswerPolicy, CodecPolicy, InboundConfig, IncomingAuthPolicy, IncomingCallPolicy, KnownNodes,
    RegisterConfig,
};

/// The one load-time warning. Printed by [`NodeFileConfig::from_toml_str`] —
/// the daemon's startup loader — and never by
/// [`NodeFileConfig::from_toml_str_redacted`], which the settings page runs
/// on every poll and every save.
const CONFIG_SOURCE_WITHOUT_SECRET_WARNING: &str = "astar-server: warning: [secrets] source = \"config\" but no secret is set; registration will not authenticate until one is provided";

// ---------------------------------------------------------------------------
// Sub-struct definitions
// ---------------------------------------------------------------------------

/// `[listener]` section — bind address, answer/auth policy, call cap.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ListenerCfg {
    /// Bind address, e.g. `"0.0.0.0:4569"`.
    pub bind: String,
    /// Answer policy string: `"auto"` or `"manual"` (case-insensitive).
    pub answer: String,
    /// Maximum simultaneous inbound calls.
    pub max_calls: usize,
    /// Auth policy string: `"required"`, `"optional"`, or `"off"` (case-insensitive).
    pub auth: String,
    /// Optional inbound node allowlist (iax-91c9): the set of caller node ids
    /// permitted to call this node. When omitted OR empty, every caller is
    /// admitted (subject to `auth`/`max_calls`) — backward compatible. When
    /// non-empty, a caller whose node id is not on the list is rejected with
    /// "not authorized" at call-setup time, before answer/adopt.
    #[serde(default)]
    pub allowed_nodes: Vec<String>,
}

/// `[register]` section (optional) — upstream registrar details.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegisterCfg {
    /// Upstream registrar address, e.g. `"104.232.32.242:4569"`.
    pub peer: String,
    /// This node's numeric ID / username to register as, e.g. `"77777"`.
    pub node_id: String,
}

/// `[audio]` section (optional) — device selection.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AudioCfg {
    /// Capture device substring; `None` = system default.
    pub input: Option<String>,
    /// Playback device substring; `None` = system default.
    pub output: Option<String>,
    /// Audio backend: `"cpal"` (default; real devices) or `"none"`
    /// (hardware-free `NullBackend` for headless hosts/containers).
    pub backend: Option<String>,
}

/// `[announce.tts]` sub-section — TTS subprocess config.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TtsCfg {
    /// Whether TTS synthesis is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Path or name of the TTS binary (default: `"piper"`).
    pub binary: Option<String>,
    /// Path to the voice model file.
    pub voice: Option<String>,
    /// Synthesis timeout in milliseconds (default: 4000).
    pub timeout_ms: Option<u64>,
    /// Output gain in decibels applied to synthesized PCM (default: 0.0 = unity).
    /// Use a negative value to tame a voice that renders too hot (e.g. -6.0).
    pub gain_db: Option<f32>,
}

/// `[announce.events.<name>]` sub-section — per-event announcement config.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EventCfg {
    /// Whether this event announcement is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Destination for this event: `"to_air"` or `"to_monitor"`.
    pub destination: Option<String>,
}

/// `[announce]` section (optional) — voice-announcement configuration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AnnounceCfg {
    /// Whether announcements are globally enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the sounds directory for WAV samples.
    pub sounds_dir: Option<String>,
    /// Default mix-under gain in dB (default: -12.0).
    pub mixunder_default_gain_db: Option<f32>,
    /// Interval in seconds between automatic ID announcements.
    pub id_interval_secs: Option<u64>,
    /// ID mode: `"off"`, `"cw"` (Morse) or `"tts"` (spoken). Absent reads as
    /// `"off"`; the controller keys on `"off"` and `"cw"` and speaks anything
    /// else, and the settings page offers exactly these three.
    pub id_mode: Option<String>,
    /// CW speed in words per minute (default: 20).
    pub cw_wpm: Option<u32>,
    /// CW tone frequency in Hz (default: 800.0).
    pub cw_tone_hz: Option<f32>,
    /// Whether to key the transmitter for CW even when idle (default: true).
    pub cw_keys_when_idle: Option<bool>,
    /// On-join greeting template (iax-c4ea): the spoken phrase each joining user
    /// hears on their own leg, announcing the node they reached. The literal
    /// token `{server-node-number}` is substituted at announce time with this
    /// node's id expanded into space-separated digits (so TTS reads it
    /// digit-by-digit, e.g. `77777` → `7 7 7 7 7`). When unset, the default is
    /// `"Connected to node {server-node-number}"`. The greeting fires only when
    /// the `answered` event is enabled under `[announce.events]` and the call is
    /// a conference member.
    pub join_template: Option<String>,
    /// Link-connect announcement template (iax-9e02): spoken BEFORE a
    /// `*3`/`*2`/`POST /link` dial starts, so an operator hears which node is
    /// being reached even when the dial then fails. The literal token
    /// `{node}` is substituted with the TARGET node number expanded into
    /// space-separated digits (TTS reads it digit-by-digit). Unset ⇒
    /// `"Connecting to node {node}"`.
    pub link_connect_template: Option<String>,
    /// Link-disconnect announcement template (iax-9e02): spoken AFTER the link
    /// is torn down, so it is never transmitted over the link being dropped.
    /// Same `{node}` substitution. Unset ⇒ `"Disconnected from node {node}"`.
    pub link_disconnect_template: Option<String>,
    /// M17-monitor-start announcement template: spoken to this node's own
    /// members (never toward the reflector, which has no receive path from
    /// the conference) once an M17 monitor has been set up. It gives inbound
    /// and handset users the same disclosure an `AllStar` link dial does. The
    /// literal token `{node}` is replaced with the reflector name spelled out
    /// character by character, then `" module "` and the module letter, so
    /// TTS reads `M17-USA` C as "M 1 7 - U S A module C". Unset ⇒
    /// `"Monitoring M17 reflector {node}"`.
    pub m17_connect_template: Option<String>,
    /// M17-monitor-end announcement template: spoken after the monitor is
    /// gone, whether the operator hung up or the reflector refused or
    /// dropped the link. Same `{node}` substitution. Unset ⇒
    /// `"Stopped monitoring M17 reflector {node}"`.
    pub m17_disconnect_template: Option<String>,
    /// TTS subsystem configuration.
    pub tts: Option<TtsCfg>,
    /// Per-event announcement configurations.
    pub events: Option<std::collections::HashMap<String, EventCfg>>,
}

impl AnnounceCfg {
    /// Build an [`astar_iax::ServiceConfig`] from this announce config section.
    pub fn to_service_config(&self) -> astar_iax::ServiceConfig {
        astar_iax::ServiceConfig {
            resolver: astar_iax::ResolverConfig {
                sounds_dir: self.sounds_dir.clone().map(std::path::PathBuf::from),
                cw_wpm: self.cw_wpm.unwrap_or(20),
                cw_tone_hz: self.cw_tone_hz.unwrap_or(800.0),
            },
            mixunder_default_gain_db: self.mixunder_default_gain_db.unwrap_or(-12.0),
            cw_keys_when_idle: self.cw_keys_when_idle.unwrap_or(true),
            tts: self.to_tts_config(),
        }
    }

    /// Build an [`astar_iax::TtsConfig`] from this announce config section.
    ///
    /// Returns [`astar_iax::TtsConfig::default()`] (disabled) if no `[announce.tts]`
    /// sub-section is present.
    pub fn to_tts_config(&self) -> astar_iax::TtsConfig {
        match &self.tts {
            None => astar_iax::TtsConfig::default(),
            Some(tts) => astar_iax::TtsConfig {
                enabled: tts.enabled,
                binary: tts.binary.clone().unwrap_or_else(|| "piper".into()),
                voice: tts.voice.clone().map(std::path::PathBuf::from),
                timeout: std::time::Duration::from_millis(tts.timeout_ms.unwrap_or(4000)),
                gain_db: tts.gain_db.unwrap_or(0.0),
            },
        }
    }
}

/// `[bridge]` section (optional) — conference-bridge topology (iax-647d).
///
/// **Daemon default:** an ABSENT `[bridge]` section means `mode = "bridge"` (a
/// pure mix-minus bridge among remote callers, local radio off) — see
/// [`NodeFileConfig::to_bridge_config`]. This differs from the `Station` library
/// default (handset), which keeps existing embedders byte-identical.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BridgeCfg {
    /// Topology mode: `"handset"` (1:1), `"bridge"` (mix-minus, the default), or
    /// `"conference"` (alias for the same mix-minus engine). Case-insensitive.
    /// Defaults to `"bridge"` when the field is omitted within a present section.
    #[serde(default = "default_bridge_mode")]
    pub mode: String,
    /// Mix-minus: each member hears everyone but itself (default `true`).
    /// `false` = full mix (members hear themselves), for parrot/loopback.
    #[serde(default = "default_true")]
    pub mix_minus: bool,
    /// Add the local mic as a conference source and feed the local speaker the
    /// sum of all members (default `false` = pure bridge).
    #[serde(default)]
    pub include_local_radio: bool,
}

fn default_bridge_mode() -> String {
    "bridge".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for BridgeCfg {
    /// The daemon default: pure mix-minus bridge, local radio off.
    fn default() -> Self {
        Self {
            mode: default_bridge_mode(),
            mix_minus: true,
            include_local_radio: false,
        }
    }
}

/// `[discord]` section (optional) — a Discord voice channel as a
/// receive-only conference member. Absent ⇒ the feature is inert.
///
/// There is deliberately no token key: the bot token is
/// the `discord` key of credentials.toml under `[secrets] source = "file"`,
/// `DISCORD_BOT_TOKEN` under `source = "env"`, or `POST /secrets` with
/// username `discord` under any source — never a key in this file.
///
/// There is equally deliberately no `arm`, `key`, `transmit` or
/// `contributes` key. `deny_unknown_fields` means an operator who invents
/// one gets a startup error rather than a silently ignored line that reads
/// as if it did something.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordCfg {
    /// Discord guild (server) snowflake, as a string.
    pub guild_id: String,
    /// Voice channel snowflake, as a string.
    pub channel_id: String,
    /// Join the channel when the node starts, as though `POST /discord`
    /// with `action = "join"` had been issued once the node is up. Default
    /// `false`. A failure is logged and the node carries on: Discord being
    /// unreachable must never stop a node coming up on the air.
    #[serde(default)]
    pub join_at_start: bool,
    /// Speak a relay notice into the channel on join. Default `true`.
    #[serde(default = "default_true")]
    pub announce_on_join: bool,
    /// **(reserved — not yet implemented.)** Path to an unamblify ONNX
    /// post-filter model.
    ///
    /// Parsed and carried, but nothing consumes it: the post-filter it would
    /// configure does not exist. `astar-discord`'s only `PostFilter` is
    /// `Bypass`, and `complete_discord_join` constructs it unconditionally —
    /// deliberately, since there is no inference crate in the workspace.
    /// Setting this changes nothing audible.
    ///
    /// The key stays accepted rather than being removed, because
    /// `deny_unknown_fields` would turn a config written against a later
    /// release into a startup failure.
    pub model: Option<String>,
    /// **(reserved — not yet implemented.)** Post-filter lookahead variant:
    /// `"ll5"` or `"ll20"`. Inert for the same reason as
    /// [`DiscordCfg::model`].
    pub latency: Option<String>,
    /// Discord user ids (snowflakes, as strings) who may make and remove
    /// admins. **This list is the master switch for the whole slash-command
    /// half of the feature**: empty or absent means no commands are
    /// registered, no gateway is kept online between voice sessions, and no
    /// channel muting happens — the bridge behaves exactly as it did before
    /// slash commands existed.
    ///
    /// Approvers are config, not state: they are never banned, revoked or
    /// demoted from inside Discord, and the admin page lists them read-only.
    /// That is why they live here rather than in the state file.
    ///
    /// Strings, not integers, because a snowflake does not survive a round
    /// trip through most JSON/JavaScript number types — the same reason
    /// `guild_id` and `channel_id` are strings.
    #[serde(default)]
    pub approvers: Vec<String>,
    /// Where the approval state (admins, approved, banned, pending, and the
    /// ids this bridge server-muted) is written. Absent ⇒ in memory only,
    /// and the node says so once at start-up: approvals then do not survive
    /// a restart.
    pub state_file: Option<String>,
    /// Repeater-style hard time-out timer on the open mic, in seconds.
    /// Absent ⇒ 180. `0` turns it off — which is a real choice on a
    /// listen-only bridge, but a bad one anywhere the mic can key a
    /// transmitter.
    pub mic_timeout_secs: Option<u64>,
    /// How much silence from the mic holder releases the mic, in seconds.
    /// Absent ⇒ 5. `0` turns it off, leaving only the hard time-out.
    pub mic_silence_secs: Option<u64>,
}

/// Mirrors the serde defaults exactly, so a struct literal built with
/// `..DiscordCfg::default()` and the same section parsed from TOML agree.
/// The one place they cannot agree is `guild_id`/`channel_id`, which have no
/// default on disk: a `Default` value carries empty strings, which is not a
/// configuration any real node has.
impl Default for DiscordCfg {
    fn default() -> Self {
        Self {
            guild_id: String::new(),
            channel_id: String::new(),
            join_at_start: false,
            announce_on_join: default_true(),
            model: None,
            latency: None,
            approvers: Vec::new(),
            state_file: None,
            mic_timeout_secs: None,
            mic_silence_secs: None,
        }
    }
}

impl DiscordCfg {
    /// The approver ids, parsed and de-duplicated, in the order they appear.
    ///
    /// Duplicates are tolerated rather than refused: a list written by hand
    /// and pasted twice is not a misconfiguration worth refusing to boot
    /// over, and a duplicate approver is indistinguishable from a single one
    /// in every rule that reads the list.
    ///
    /// # Errors
    /// A message NAMING the offending entry when one is not a snowflake —
    /// anything that is not a positive integer. `0` is refused too: it is a
    /// valid `u64` but never a Discord user id, and a silently-accepted `0`
    /// would sit in the approver list matching nobody while looking like it
    /// matched someone.
    pub fn approver_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids = Vec::with_capacity(self.approvers.len());
        for raw in &self.approvers {
            let id: u64 = raw.parse().map_err(|_| {
                format!("discord.approvers: {raw:?} is not a Discord user id (a snowflake)")
            })?;
            if id == 0 {
                return Err(format!(
                    "discord.approvers: {raw:?} is not a Discord user id (a snowflake)"
                ));
            }
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The hard time-out timer in seconds (absent ⇒ 180).
    ///
    /// Named for the `Commander::set_mic_timeout` it feeds rather than for
    /// the key it reads: a method spelled exactly like the `Option` field
    /// beside it is one missing pair of parentheses away from a silent bug.
    #[must_use]
    pub fn mic_timeout(&self) -> u64 {
        self.mic_timeout_secs.unwrap_or(180)
    }

    /// The silence unkey window in seconds (absent ⇒ 5). Named for its
    /// consumer for the same reason as [`DiscordCfg::mic_timeout`].
    #[must_use]
    pub fn mic_silence(&self) -> u64 {
        self.mic_silence_secs.unwrap_or(5)
    }
}

/// `[parrot]` section (optional) — tuning knobs for `bridge.mode = "parrot"`
/// (iax-feab). Only consulted when `[bridge] mode = "parrot"`; ignored (and
/// harmless if present) otherwise. Values are milliseconds on disk and are
/// converted to 20 ms ticks by [`NodeFileConfig::to_bridge_config`]. An absent
/// section, or an absent individual field within a present section, falls back
/// to the matching [`astar_audio::ParrotTuning::default`] value (150 ticks
/// / 40 ticks / -40.0 dB). `max_record_ticks` (the 10 s hard cap) is NOT
/// TOML-exposed — it always stays the library default (500 ticks).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ParrotFileCfg {
    /// Delay after deciding to replay, before playback starts, in ms.
    /// Default (when absent): 3000 ms (150 ticks).
    pub playback_delay_ms: Option<u64>,
    /// VOX-mode silence gap that ends a take, in ms. Default (when absent):
    /// 800 ms (40 ticks).
    pub silence_gap_ms: Option<u64>,
    /// VOX level threshold in dBFS. Default (when absent): -40.0.
    pub vox_threshold_db: Option<f32>,
}

/// `[portal]` section (iax-b7f2) — `AllStarLink` portal account used to mint
/// Web-Transceiver tokens for `wt-guest` link dials. The ASL3
/// `[allstar-public]` dialplan validates `CALLING_NAME` server-side
/// (`authwebphone.pl`, see `docs/wt-web-transceiver.md`) and silently clears
/// calls without a fresh valid token ~1 s after answer, so `wt-guest` targets
/// are unreachable without this section. The password itself NEVER lives in
/// this file: under `[secrets] source = "file"` it is the `portal` key of
/// credentials.toml; under the other sources `credential_env` names the
/// environment variable that holds it. Either way it is resolved once at
/// startup in `main`, consumed into `StationConfig.portal`, never logged.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PortalCfg {
    /// Portal account callsign (the allstarlink.org login).
    pub user: String,
    /// A node the account OWNS — token minting requires it.
    pub node: String,
    /// Environment variable NAME holding the portal account password
    /// (a reference, never material — the `secret_ref` idiom). Not read
    /// under `[secrets] source = "file"`.
    #[serde(default = "default_portal_credential_env")]
    pub credential_env: String,
}

fn default_portal_credential_env() -> String {
    "ALLSTAR_PORTAL_PASS".to_string()
}

/// `[links."<node>"]` sub-table — per-target link dial profile (iax-5029).
/// Secret-free by construction: shape only, never credential material.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LinkCfg {
    /// Dial shape: `"standard"` (default) or `"wt-guest"` (`AllStar` app
    /// nodes whose guest context only has the WT extension `"s"`).
    pub shape: Option<String>,
}

/// Parsed per-target link dial shape (iax-5029).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkShape {
    /// Plain node-to-node IAX2 — today's dial, byte-identical.
    #[default]
    Standard,
    /// `AllStar` web-transceiver guest shape (`CALLED_NUMBER "s"`, guest
    /// credentials) — see the design doc.
    WtGuest,
}

/// `[dtmf]` section — DTMF `*` command execution (iax-d254). Off by default:
/// enabling lets ANY connected member command links.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DtmfCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Inter-digit gap that finalizes a pending command (default 3000).
    pub inter_digit_timeout_ms: Option<u64>,
}

/// `[control]` section — HTTP control channel bind address.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ControlCfg {
    /// Bind address for the HTTP+SSE control adapter, e.g. `"127.0.0.1:8730"`.
    pub bind: String,
    /// `true`: peers at a loopback address must sign in too (shared hosts).
    /// Default `false`: a loopback peer is the built-in `console` admin, which
    /// is the recovery path and what `curl` and the CLI use. Only with the
    /// `auth` feature; without it the control port has no sign-in at all.
    #[serde(default)]
    pub loopback_login: bool,
    /// `true` lets `bind` be a wildcard (`0.0.0.0` / `::`). Refused at load
    /// otherwise: a wildcard puts the keying port on every interface. For a
    /// sealed container whose published port arrives on the pod interface
    /// (nodebench, the smoke image) and nothing else. Config file only; the
    /// settings page never offers it.
    #[serde(default)]
    pub allow_wildcard_bind: bool,
}

/// Every `[secrets]` key that may hold a credential inline. The redacted
/// loader type-checks each before serde can quote a mistyped value, and a
/// migration to `credentials.toml` blanks each.
pub const INLINE_CREDENTIAL_KEYS: &[&str] =
    &["secret", "portal", "discord", "porkbun_pk", "porkbun_sk"];

/// The port-80 bind `[web] acme_http_bind` falls back to under HTTP-01.
pub const DEFAULT_ACME_HTTP_BIND: &str = "0.0.0.0:80";

/// `[web]` — the public, read-only listener page and its HTTPS. Absent, or
/// present without `public_bind`, means no public page at all: the admin
/// control bind (`[control]`) is the only listener.
///
/// No credential lives here. The DNS provider's API keys come from the
/// `[secrets]` source like every other credential, so this derives `Debug`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebCfg {
    /// The public page's bind, e.g. `"0.0.0.0:443"`. Absent ⇒ no public page.
    pub public_bind: Option<String>,
    /// The names the public page answers to, and the names its certificate
    /// covers, **as written**. An entry may carry the `{node}` token, which
    /// is replaced with this node's number (`[register] node_id`). Absent
    /// with a `[web.dns]` section means `["<dns.host>.<dns.domain>"]`, which
    /// is `{node}.<domain>` by default. Read the expanded, normalised list
    /// through [`WebCfg::hostnames`]; a request whose `Host` is not in it is
    /// refused (DNS rebinding).
    #[serde(default)]
    pub public_hostnames: Option<Vec<String>>,
    /// The expanded hostnames: every `{node}` replaced, lower-cased, a
    /// trailing dot stripped, duplicates dropped. Filled by `validated()`.
    #[serde(skip)]
    hostnames: Vec<String>,
    /// Contact address given to the ACME CA for expiry notices. Optional.
    #[serde(default)]
    pub acme_email: Option<String>,
    /// Use Let's Encrypt's staging CA (untrusted certificates, generous rate
    /// limits) while testing.
    #[serde(default)]
    pub acme_staging: bool,
    /// The port-80 listener: HTTP-01 answers and the redirect to HTTPS.
    /// Read it through [`WebCfg::http_bind`]: under HTTP-01 it defaults to
    /// [`DEFAULT_ACME_HTTP_BIND`]; under DNS-01 port 80 is not needed and an
    /// absent key means no port-80 listener at all.
    #[serde(default)]
    pub acme_http_bind: Option<String>,
    /// `"http-01"` or `"dns-01"`. Read it through [`WebCfg::challenge`]:
    /// absent ⇒ DNS-01 when `[web.dns]` names a provider, else HTTP-01.
    #[serde(default)]
    pub acme_challenge: Option<AcmeChallenge>,
    /// `[web.dns]` — a DNS provider for dynamic DNS and ACME DNS-01.
    #[serde(default)]
    pub dns: Option<DnsCfg>,
    /// `false`: the listener page and its streams need a viewer (or admin)
    /// session. Read through [`WebCfg::anonymous_status`]; default `true`.
    #[serde(default)]
    pub anonymous_status: Option<bool>,
    /// Serve `/admin` and `/auth/*` (sign-in) on the public bind. Read
    /// through [`WebCfg::admin_on_public`]; default `true` when there is a
    /// public bind. Needs the `auth` feature.
    #[serde(default)]
    pub admin_on_public: Option<bool>,
    /// The WebAuthn relying-party ID: absent or `""` means each public
    /// hostname is its own; otherwise a parent domain you control that
    /// every public hostname sits under, so credentials survive a move
    /// between names under it.
    #[serde(default)]
    pub rp_id: Option<String>,
    /// A session idle this long ends, 1–30 days. Default 7.
    #[serde(default = "default_session_idle_days")]
    pub session_idle_days: u32,
    /// A session ends this long after sign-in whatever its use, from
    /// `session_idle_days` to 90 days. Default 30.
    #[serde(default = "default_session_max_days")]
    pub session_max_days: u32,
    /// How recently a credential must have been used for secrets, settings
    /// saves, restart and user management, 2–60 minutes. Default 10.
    #[serde(default = "default_step_up_minutes")]
    pub step_up_minutes: u32,
}

fn default_session_idle_days() -> u32 {
    7
}

fn default_session_max_days() -> u32 {
    30
}

fn default_step_up_minutes() -> u32 {
    10
}

impl WebCfg {
    /// The public hostnames, expanded and normalised: what the `Host`
    /// allowlist, the certificate and the DNS records all use.
    #[must_use]
    pub fn hostnames(&self) -> &[String] {
        &self.hostnames
    }

    /// The ACME challenge in force: the written one, else DNS-01 when a DNS
    /// provider is configured, else HTTP-01.
    #[must_use]
    pub fn challenge(&self) -> AcmeChallenge {
        self.acme_challenge.unwrap_or(if self.dns.is_some() {
            AcmeChallenge::Dns01
        } else {
            AcmeChallenge::Http01
        })
    }

    /// Whether the listener page is open to anyone (`true`, the default)
    /// or needs a session.
    #[must_use]
    pub fn anonymous_status(&self) -> bool {
        self.anonymous_status.unwrap_or(true)
    }

    /// Whether sign-in and the admin pages are served on the public bind.
    #[must_use]
    pub fn admin_on_public(&self) -> bool {
        self.public_bind.is_some() && self.admin_on_public.unwrap_or(true)
    }

    /// The configured parent RP ID, or `None` for "each hostname".
    #[must_use]
    pub fn rp_id(&self) -> Option<&str> {
        self.rp_id.as_deref().filter(|r| !r.is_empty())
    }

    /// The port-80 bind, or `None` when there is no port-80 listener: under
    /// DNS-01 with no `acme_http_bind` written. HTTP-01 always has one.
    #[must_use]
    pub fn http_bind(&self) -> Option<&str> {
        match (self.acme_http_bind.as_deref(), self.challenge()) {
            (Some(b), _) => Some(b),
            (None, AcmeChallenge::Http01) => Some(DEFAULT_ACME_HTTP_BIND),
            (None, AcmeChallenge::Dns01) => None,
        }
    }
}

/// How the ACME CA checks that this node controls its hostnames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum AcmeChallenge {
    /// A token served on port 80 under `/.well-known/acme-challenge/`.
    #[serde(rename = "http-01")]
    Http01,
    /// A TXT record at `_acme-challenge.<host>`, written through the DNS
    /// provider's API. Needs no inbound port at all.
    #[serde(rename = "dns-01")]
    Dns01,
}

impl AcmeChallenge {
    /// The spelling `node.toml` uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http01 => "http-01",
            Self::Dns01 => "dns-01",
        }
    }
}

/// The DNS providers astar-server can drive: Porkbun, built in, and the
/// dns-update providers in [`DNS_UPDATE_PROVIDERS`] with `dns-providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DnsProviderKind {
    /// Porkbun's JSON API v3 (<https://porkbun.com/api/json/v3/documentation>).
    #[default]
    Porkbun,
    /// A dns-update provider, by its `node.toml` name.
    Other(&'static str),
}

impl DnsProviderKind {
    /// The spelling `node.toml` uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Porkbun => "porkbun",
            Self::Other(n) => n,
        }
    }
}

impl<'de> serde::Deserialize<'de> for DnsProviderKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "porkbun" {
            return Ok(Self::Porkbun);
        }
        DNS_UPDATE_PROVIDERS
            .iter()
            .find(|p| p.name == s)
            .map(|p| Self::Other(p.name))
            .ok_or_else(|| {
                let known: Vec<&str> = std::iter::once("porkbun")
                    .chain(DNS_UPDATE_PROVIDERS.iter().map(|p| p.name))
                    .collect();
                serde::de::Error::custom(format!(
                    "unknown DNS provider \"{s}\"; known: {}",
                    known.join(", ")
                ))
            })
    }
}

impl serde::Serialize for DnsProviderKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// One dns-update provider: its `node.toml` name, the credential key it
/// reads from the secret store, the env var that feeds it, and what the
/// operator creates at the provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub name: &'static str,
    pub credential: &'static str,
    pub env: &'static str,
    pub title: &'static str,
}

/// The dns-update providers wired today: each takes one API token. Adding
/// one is a row here and an arm in `public::dns_update::build_updater`.
pub const DNS_UPDATE_PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        name: "cloudflare",
        credential: "dns:cloudflare",
        env: "ASTAR_DNS_CLOUDFLARE_TOKEN",
        title: "Cloudflare API token (Zone → DNS → Edit)",
    },
    ProviderSpec {
        name: "digitalocean",
        credential: "dns:digitalocean",
        env: "ASTAR_DNS_DIGITALOCEAN_TOKEN",
        title: "DigitalOcean personal access token",
    },
    ProviderSpec {
        name: "desec",
        credential: "dns:desec",
        env: "ASTAR_DNS_DESEC_TOKEN",
        title: "deSEC token",
    },
    ProviderSpec {
        name: "hetzner",
        credential: "dns:hetzner",
        env: "ASTAR_DNS_HETZNER_TOKEN",
        title: "Hetzner DNS API token",
    },
    ProviderSpec {
        name: "linode",
        credential: "dns:linode",
        env: "ASTAR_DNS_LINODE_TOKEN",
        title: "Linode personal access token",
    },
    ProviderSpec {
        name: "gandi",
        credential: "dns:gandi",
        env: "ASTAR_DNS_GANDI_TOKEN",
        title: "Gandi personal access token",
    },
    ProviderSpec {
        name: "vultr",
        credential: "dns:vultr",
        env: "ASTAR_DNS_VULTR_TOKEN",
        title: "Vultr API key",
    },
    ProviderSpec {
        name: "ionos",
        credential: "dns:ionos",
        env: "ASTAR_DNS_IONOS_TOKEN",
        title: "IONOS API key (prefix.secret)",
    },
    ProviderSpec {
        name: "bunny",
        credential: "dns:bunny",
        env: "ASTAR_DNS_BUNNY_TOKEN",
        title: "bunny.net API key",
    },
    ProviderSpec {
        name: "gcore",
        credential: "dns:gcore",
        env: "ASTAR_DNS_GCORE_TOKEN",
        title: "Gcore API token",
    },
];

/// Where `[web.dns] ip_source` says the public address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpSourceCfg<'a> {
    Provider,
    Url(&'a str),
}

/// Porkbun refuses a TTL under 600 seconds.
pub const MIN_DNS_TTL: u32 = 600;
/// The shortest dynamic-DNS check interval accepted.
pub const MIN_DDNS_INTERVAL_SECS: u64 = 60;

fn default_ddns_interval_secs() -> u64 {
    300
}

fn default_dns_ttl() -> u32 {
    MIN_DNS_TTL
}

/// `[web.dns]` — the zone this node's hostnames live in, at a DNS provider
/// whose API it can drive. The API keys are credentials and come from
/// `[secrets]`, never from here.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsCfg {
    #[serde(default)]
    pub provider: DnsProviderKind,
    /// The registered zone at the provider, e.g. `"example.com"`. Every
    /// `public_hostnames` entry must be this name or a name under it.
    pub domain: String,
    /// The subdomain template for the default hostname when
    /// `public_hostnames` is absent: `{node}` (the default) is replaced with
    /// the node number, so node 69586 under `example.net` answers as
    /// `69586.example.net`. An empty string means the zone apex itself.
    #[serde(default)]
    pub host: Option<String>,
    /// Keep an A record for each `public_hostnames` entry pointed at this
    /// box's public IPv4 address.
    #[serde(default)]
    pub ddns: bool,
    /// How often the dynamic-DNS loop checks the address. Default 300.
    #[serde(default = "default_ddns_interval_secs")]
    pub ddns_interval_secs: u64,
    /// TTL of every record this node writes. Default and minimum 600.
    #[serde(default = "default_dns_ttl")]
    pub ttl: u32,
    /// `"provider"` (Porkbun's `/ping`; the default) or an `https://` URL
    /// that answers with this box's public IPv4 address as plain text.
    #[serde(default)]
    pub ip_source: Option<String>,
}

/// True when an `@` sits after the first `/`, `?` or `#` of `rest` (the part
/// of a URL after `scheme://`): a raw delimiter inside the userinfo, which
/// would otherwise be read as the end of the authority and leak the password.
pub(crate) fn raw_delimiter_in_userinfo(rest: &str) -> bool {
    rest.find(['/', '?', '#'])
        .is_some_and(|i| rest[i..].contains('@'))
}

impl DnsCfg {
    #[must_use]
    pub fn ip_source(&self) -> IpSourceCfg<'_> {
        match self.ip_source.as_deref() {
            None | Some("provider") => IpSourceCfg::Provider,
            Some(u) => IpSourceCfg::Url(u),
        }
    }

    /// The provider, `ip_source`, TTL and interval rules, as a `node.toml` error message.
    fn validate_provider(&self) -> Result<(), String> {
        let dns = self;
        // First, so a build without the feature says so before anything else.
        if !cfg!(feature = "dns-providers") && dns.provider != DnsProviderKind::Porkbun {
            return Err(format!(
                "web.dns.provider: \"{}\" needs a build with the dns-providers feature (the release build has it); this build drives Porkbun only",
                dns.provider.as_str()
            ));
        }
        match dns.ip_source.as_deref() {
            None | Some("provider") => {
                if dns.provider != DnsProviderKind::Porkbun && (dns.ip_source.is_some() || dns.ddns)
                {
                    return Err(format!(
                        "web.dns.ip_source: only Porkbun can report this box's public address; set ip_source to an https:// URL that returns it (provider \"{}\")",
                        dns.provider.as_str()
                    ));
                }
            }
            Some(u)
                if u.starts_with("https://")
                    && u.len() > "https://".len()
                    && raw_delimiter_in_userinfo(&u["https://".len()..]) =>
            {
                return Err("web.dns.ip_source: userinfo must be percent-encoded".into());
            }
            Some(u) if u.starts_with("https://") && u.len() > "https://".len() => {}
            Some(other) => {
                // Only scheme and host are echoed: the rest may carry a token.
                let shown = other
                    .split_once("://")
                    .map_or_else(String::new, |(scheme, rest)| {
                        if raw_delimiter_in_userinfo(rest) {
                            return format!(" \"{scheme}://(URL withheld)\"");
                        }
                        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
                        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
                        format!(" \"{scheme}://{host}\"")
                    });
                return Err(format!(
                    "web.dns.ip_source:{shown} is neither \"provider\" nor an https:// URL"
                ));
            }
        }
        if dns.ttl < MIN_DNS_TTL {
            return Err(format!(
                "web.dns.ttl: {} is under {MIN_DNS_TTL}, the provider's minimum",
                dns.ttl
            ));
        }
        if dns.ddns_interval_secs < MIN_DDNS_INTERVAL_SECS {
            return Err(format!(
                "web.dns.ddns_interval_secs: {} is under {MIN_DDNS_INTERVAL_SECS}",
                dns.ddns_interval_secs
            ));
        }
        Ok(())
    }
}

fn default_max_listeners() -> u32 {
    25
}

fn default_mp3_kbps() -> u32 {
    32
}

fn default_opus_kbps() -> u32 {
    24
}

/// MPEG-2 / 2.5 Layer III bitrates the LAME wrapper offers at 16 kHz and
/// below (its `Bitrate` enum has no 56).
pub const MP3_KBPS: &[u32] = &[8, 16, 24, 32, 40, 48, 64];

/// `[stream]` — the live audio streams on the public page. Off by default,
/// and never in parrot mode whatever `enabled` says: a parrot caller is
/// making a private test.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Listeners across both streams together, 1–200. Default 25.
    #[serde(default = "default_max_listeners")]
    pub max_listeners: u32,
    /// `/stream.mp3` bitrate, one of [`MP3_KBPS`]. Default 32.
    #[serde(default = "default_mp3_kbps")]
    pub mp3_kbps: u32,
    /// The page player's Opus bitrate, 6–128. Default 24.
    #[serde(default = "default_opus_kbps")]
    pub opus_kbps: u32,
}

impl Default for StreamCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            max_listeners: default_max_listeners(),
            mp3_kbps: default_mp3_kbps(),
            opus_kbps: default_opus_kbps(),
        }
    }
}

/// A size as written in `node.toml` (`"2 GB"`, `"500MB"`, `"1 GiB"`): the
/// byte count, and the text exactly as the operator wrote it, so the
/// settings page shows and saves back `"2 GB"` rather than `2000000000`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteSize {
    pub bytes: u64,
    pub text: String,
}

impl<'de> serde::Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let bytes = parse_size(&text).map_err(serde::de::Error::custom)?;
        Ok(Self { bytes, text })
    }
}

/// Parse a size: a whole number, an optional space, and an optional unit.
/// `B`, `KB`, `MB` and `GB` are decimal (`"2 GB"` is 2 000 000 000 bytes);
/// `KiB`, `MiB` and `GiB` are binary. Units ignore case; a bare number is
/// bytes.
///
/// # Errors
/// A sentence naming the text, for anything else: an empty string, a
/// fraction or a sign, an unknown unit, or a size past `u64`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let refuse = || format!("{s:?} is not a size such as \"2 GB\" or \"500 MB\"");
    if number.is_empty() {
        return Err(refuse());
    }
    let n: u64 = number.parse().map_err(|_| refuse())?;
    let scale: u64 = match unit.trim_start().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        _ => return Err(refuse()),
    };
    n.checked_mul(scale).ok_or_else(refuse)
}

/// The smallest `[recording] max_bytes` the loader accepts: 100 MB.
pub const MIN_RECORDING_BYTES: u64 = 100_000_000;

fn default_recording_dir() -> PathBuf {
    PathBuf::from("/var/lib/astar/recordings")
}

fn default_recording_max_bytes() -> ByteSize {
    ByteSize {
        bytes: 2_000_000_000,
        text: "2 GB".to_string(),
    }
}

fn default_recording_max_age_days() -> u32 {
    30
}

/// `[recording]` — the node's log of every transmission the conference
/// carries. Off by default. Oldest recordings go first once the folder
/// passes `max_bytes` or a recording is older than `max_age_days`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Where recordings go. Default `/var/lib/astar/recordings`.
    #[serde(default = "default_recording_dir")]
    pub dir: PathBuf,
    /// The most the folder may hold, at least 100 MB. Default `"2 GB"`.
    #[serde(default = "default_recording_max_bytes")]
    pub max_bytes: ByteSize,
    /// The oldest a recording may get, 1–3650 days. Default 30.
    #[serde(default = "default_recording_max_age_days")]
    pub max_age_days: u32,
    /// Keep the codec frames beside each recording, for unamblify. Default on.
    #[serde(default = "default_true")]
    pub frames: bool,
}

/// `[netrec]` — the net recordings pages under `/nets/`, read from the
/// archive astar-netrec writes. Absent ⇒ off, and `/nets/` is a 404. The
/// server only reads the archive, and deletes a session when an admin asks;
/// it never creates the folder, so a missing one is an empty list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetrecCfg {
    /// The archive folder (astar-netrec's `archive`).
    pub archive: PathBuf,
}

impl Default for RecordingCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_recording_dir(),
            max_bytes: default_recording_max_bytes(),
            max_age_days: default_recording_max_age_days(),
            frames: true,
        }
    }
}

fn default_packet_log_dir() -> PathBuf {
    PathBuf::from("/var/lib/astar/packet-logs")
}

fn default_packet_log_max_bytes() -> ByteSize {
    ByteSize {
        bytes: astar_station::DEFAULT_PACKET_LOG_BYTES,
        text: "1 GB".to_string(),
    }
}

/// `[packet_log]` — a debug log of received media packets: one pcap per call
/// plus jitter-buffer history. Nothing is captured until someone presses the
/// capture button, which `debug_mode` shows, so nothing here waits for a
/// restart. Oldest logs go first once the folder passes `max_bytes`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketLogCfg {
    /// Whether a capture may be started. It starts nothing itself.
    #[serde(default)]
    pub debug_mode: bool,
    /// Where logs go. Default `/var/lib/astar/packet-logs`.
    #[serde(default = "default_packet_log_dir")]
    pub dir: PathBuf,
    /// The most the folder may hold, at least 100 MB. Default `"1 GB"`.
    #[serde(default = "default_packet_log_max_bytes")]
    pub max_bytes: ByteSize,
}

impl Default for PacketLogCfg {
    fn default() -> Self {
        Self {
            debug_mode: false,
            dir: default_packet_log_dir(),
            max_bytes: default_packet_log_max_bytes(),
        }
    }
}

/// The token a hostname template carries for this node's number.
pub const NODE_TOKEN: &str = "{node}";

/// The `[web.dns] host` default: the node number as the subdomain.
pub const DEFAULT_DNS_HOST: &str = NODE_TOKEN;

/// Replace [`NODE_TOKEN`] in `template` with `node`, or say why not. `key`
/// names the setting the template came from, for the message.
fn expand_node_token(template: &str, node: Option<&str>, key: &str) -> Result<String, String> {
    if !template.contains(NODE_TOKEN) {
        return Ok(template.to_string());
    }
    match node {
        Some(n) if !n.is_empty() => Ok(template.replace(NODE_TOKEN, n)),
        _ => Err(format!(
            "{key}: {template:?} uses {NODE_TOKEN}, but no node number is configured; set [register] node_id, or write the hostname out"
        )),
    }
}

/// The state directory: `$STATE_DIRECTORY` under systemd (its first entry),
/// else `state/` beside the config. Runtime state lives here (ACME account
/// and certificates); nothing is deployed into it.
#[must_use]
pub fn state_dir(config_path: &Path) -> std::path::PathBuf {
    if let Ok(dirs) = std::env::var("STATE_DIRECTORY")
        && let Some(first) = dirs.split(':').find(|d| !d.is_empty())
    {
        return std::path::PathBuf::from(first);
    }
    config_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("state")
}

/// `[web]`'s sign-in keys: ranges, `admin_on_public` against the public
/// bind, and `rp_id` against the hostnames (already expanded).
fn validate_web_signin(web: &mut WebCfg) -> Result<(), String> {
    if web.admin_on_public == Some(true) && web.public_bind.is_none() {
        return Err(
            "web.admin_on_public: true needs web.public_bind, the page it is served on".to_string(),
        );
    }
    if !(1..=30).contains(&web.session_idle_days) {
        return Err(format!(
            "web.session_idle_days: {} is outside 1–30",
            web.session_idle_days
        ));
    }
    if web.session_max_days < web.session_idle_days || web.session_max_days > 90 {
        return Err(format!(
            "web.session_max_days: {} must be at least session_idle_days ({}) and at most 90",
            web.session_max_days, web.session_idle_days
        ));
    }
    if !(2..=60).contains(&web.step_up_minutes) {
        return Err(format!(
            "web.step_up_minutes: {} is outside 2–60",
            web.step_up_minutes
        ));
    }
    if let Some(rp) = web.rp_id.as_deref().filter(|r| !r.is_empty()) {
        let rp = normalize_hostname(rp);
        if let Some(problem) = rp_id_problem(&rp, &web.hostnames) {
            return Err(format!("web.rp_id: {problem}"));
        }
        web.rp_id = Some(rp);
    }
    Ok(())
}

/// Shared wildcard-DNS and dynamic-DNS parents: anyone can get a name under
/// these, so a credential scoped to one would be usable by every other
/// user of the service. The public suffix list covers the rest of the
/// risk well enough for a node's purposes: a two-label minimum refuses a
/// bare TLD, and these are the parents an astar node is likely to sit
/// under.
const SHARED_PARENTS: &[&str] = &[
    "sslip.io",
    "nip.io",
    "xip.io",
    "traefik.me",
    "localtest.me",
    "duckdns.org",
    "dyndns.org",
    "no-ip.org",
    "ddns.net",
    "github.io",
    "co.uk",
    "org.uk",
    "com.au",
    "net.au",
    "co.nz",
    "co.jp",
];

/// Why `rp` (normalised) cannot be the `[web] rp_id` for `hostnames`, or
/// `None`. It must be a domain you control that every public hostname is
/// equal to or under.
#[must_use]
pub fn rp_id_problem(rp: &str, hostnames: &[String]) -> Option<String> {
    if let Some(problem) = hostname_problem(rp) {
        return Some(problem);
    }
    if !rp.contains('.') {
        return Some(format!(
            "\"{rp}\" is a top-level domain; name a domain you control"
        ));
    }
    if SHARED_PARENTS.contains(&rp) {
        return Some(format!(
            "\"{rp}\" is shared by everyone who uses it; credentials scoped to it would work for every other site under it. Leave rp_id empty, or use a domain you own"
        ));
    }
    if hostnames.is_empty() {
        return Some("needs web.public_hostnames to sit under it".to_string());
    }
    let suffix = format!(".{rp}");
    hostnames
        .iter()
        .find(|h| *h != rp && !h.ends_with(&suffix))
        .map(|h| format!("\"{h}\" is not under \"{rp}\"; every public hostname must be"))
}

/// Lower-case, trim, and strip one trailing dot: the form a hostname is
/// compared in everywhere (`Host` headers, certificates, DNS records).
#[must_use]
pub fn normalize_hostname(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    lower.strip_suffix('.').unwrap_or(&lower).to_string()
}

/// Why `name` (already normalised) cannot be a public hostname, or `None`.
/// ACME issues for DNS names only, so an IP literal is refused, and so is a
/// wildcard: neither challenge this node runs can prove one.
fn hostname_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("an empty name".to_string());
    }
    if name.parse::<std::net::IpAddr>().is_ok() {
        return Some(format!(
            "\"{name}\" is an IP address; certificates are issued for names (try <ip-with-dashes>.sslip.io)"
        ));
    }
    if name.len() > 253 {
        return Some(format!("\"{name}\" is longer than 253 characters"));
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !name.contains('.') || !name.split('.').all(label_ok) {
        return Some(format!("\"{name}\" is not a DNS name"));
    }
    None
}

/// `[secrets]` section — where the node's credentials come from.
///
/// **Not `Debug`-derived directly** — [`NodeFileConfig`]'s manual `Debug` impl
/// omits `secret` entirely so a real credential never reaches log output.
#[derive(Clone, serde::Deserialize)]
pub struct SecretsCfg {
    /// Source for runtime secrets: `"env"`, `"control"`, `"config"`, or
    /// `"file"` (case-insensitive). `"file"` — what the generated template
    /// ships — reads `credentials.toml` beside this config, or
    /// [`SecretsCfg::file`], and is the only source that holds every
    /// credential at once: registration password, portal password, bot
    /// token, link secrets, inbound peer secrets.
    pub source: String,
    /// Inline secret (iax-4703), used when `source = "config"`. The operator's
    /// mounted `/etc/iaxnode/node.toml` is the only file allowed to carry a
    /// real value here; repo-tracked templates ship this empty. Empty/absent
    /// is valid at load time (boots; registration just can't authenticate) —
    /// [`NodeFileConfig::from_toml_str`] warns rather than errors in that case.
    #[serde(default)]
    pub secret: Option<String>,
    /// The other credentials inline, under `source = "config"` only, so a
    /// node can be one executable plus one `node.toml` (kept 0600): the
    /// allstarlink.org portal password, the Discord bot token, and
    /// Porkbun's two API keys (`pk1_…`, `sk1_…`). Each lands in the provider
    /// under the same key `credentials.toml` uses. Never in `Debug`.
    #[serde(default)]
    pub portal: Option<String>,
    #[serde(default)]
    pub discord: Option<String>,
    #[serde(default)]
    pub porkbun_pk: Option<String>,
    #[serde(default)]
    pub porkbun_sk: Option<String>,
    /// Path of the credentials file under `source = "file"`. A relative path
    /// is resolved against the directory holding `node.toml`, never the
    /// working directory: podman, systemd and `just node` start the daemon
    /// from three different places and `--config` is the one path they
    /// agree on. Absent ⇒ `credentials.toml` beside the config — see
    /// [`NodeFileConfig::credentials_path`].
    #[serde(default)]
    pub file: Option<String>,
}

// `secret` is deliberately excluded from `Debug` below — this section must
// never let a real credential reach `{:?}` output (iax-35b1, extended by
// iax-4703).
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for SecretsCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsCfg")
            .field("source", &self.source)
            .field("file", &self.file)
            .finish()
    }
}

/// `[wireguard]` section (optional) — userspace `WireGuard` link transport
/// (iax-580b, superseding the iax-99ae TUN tunnel) for a guaranteed-reachable
/// inbound UDP port behind CGNAT. Presence of the section selects
/// [`astar_iax::LinkTransport::Wireguard`] for the WHOLE engine — outgoing
/// calls, the registrar, and the inbound listener all ride ONE shared
/// userspace stack (no TUN device, no root). **Secret-free:** the private key
/// is NOT here — the built [`astar_wireguard::WgLinkConfig`] carries only
/// the reference named by `secret_ref` (default `WIREGUARD_PRIVATE_KEY`),
/// resolved through an injected `SecretResolver` (env-backed in `main`) when
/// the stack is built (see [`NodeFileConfig::to_link_transport`]).
///
/// `Debug` is implemented manually: `secret_ref` (a reference NAME, never key
/// material) renders under the neutral key `key_ref` so the word "secret"
/// never reaches log output (house guard rule).
#[derive(Clone, serde::Deserialize)]
pub struct WireguardCfg {
    /// Whether the WG transport is enabled. Defaults to `true` when the
    /// section is present (presence selects WG); set `false` to keep plain
    /// UDP without deleting the section.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Node tunnel address in CIDR form, e.g. `"10.99.0.2/32"` (IPv4).
    pub address: String,
    /// VPS peer public key (base64 x25519).
    pub peer_public_key: String,
    /// VPS `WireGuard` endpoint, `host:port` (public underlay address).
    pub endpoint: String,
    /// Networks reachable through the tunnel, e.g. `["10.99.0.0/24"]`.
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    /// Persistent keepalive in seconds (default 25 when building the stack).
    pub keepalive_secs: Option<u16>,
    /// Private-key *reference* — the env-var name the node's resolver reads,
    /// NOT key material. Default `"WIREGUARD_PRIVATE_KEY"`, so existing
    /// deployments keep working without config changes.
    pub secret_ref: Option<String>,
    /// Optional plain (non-tunnel) UDP listener address bound ALONGSIDE the
    /// tunnel listener for direct/LAN peers, e.g. `"0.0.0.0:4569"`.
    pub also_bind_udp: Option<String>,
}

// `secret_ref` holds a reference NAME (never key material), but the guard rule
// is that the word "secret" never appears in Debug output — render it under a
// neutral key instead.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for WireguardCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireguardCfg")
            .field("enabled", &self.enabled)
            .field("address", &self.address)
            .field("peer_public_key", &self.peer_public_key)
            .field("endpoint", &self.endpoint)
            .field("allowed_ips", &self.allowed_ips)
            .field("keepalive_secs", &self.keepalive_secs)
            .field("key_ref", &self.secret_ref)
            .field("also_bind_udp", &self.also_bind_udp)
            .finish()
    }
}

/// `[m17]` — link to M17 reflectors as a receive-only monitor. Absent ⇒ off.
///
/// Opt-in on purpose: a dial presents a person's callsign to a third-party
/// reflector, so an upgrade alone must never make that possible. There is
/// deliberately no `callsign`, `transmit`, `ptt`, `contributes`, `arm`,
/// `secret`, `token` or `password` key: the monitor has no transmit path to
/// arm, the callsign comes from whoever dials, and M17 has no credential.
/// `deny_unknown_fields` turns any of them into a startup error rather than
/// a silently ignored line that reads as if it did something.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct M17Cfg {
    /// Default `true` once the section exists; `false` keeps the section
    /// written down while every dial is refused.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `"bundled"` (the compiled-in hamcall-db snapshot), an absolute path to
    /// a JSON file of the same shape, or absent (the table only). Its shape is
    /// checked at load; a file that will not load is logged once and treated
    /// as absent, so a bad file never crash-loops the node.
    pub directory: Option<String>,
    /// Extra directories to look for `libcodec2` in. This decides which code
    /// is loaded, so it is read once at start-up and never live-edited, and
    /// every entry must be absolute: a relative one would load a library from
    /// whatever directory the daemon happened to be started in.
    #[serde(default)]
    pub codec_dirs: Vec<String>,
    /// `"NAME" = "host[:port]"`, port default 17000. Consulted before the
    /// directory, so an operator can override a row or add a private reflector.
    #[serde(default)]
    pub reflectors: std::collections::BTreeMap<String, String>,
}

/// `[vocoder]` — which AMBE vocoder sticks this node may open, by USB serial
/// number. Absent or empty ⇒ none: the engine opens no vocoder port at all.
///
/// USB identity cannot tell a `DVstick` from a radio's FT-X CAT/PTT cable (both
/// enumerate as FTDI `0403:6015`), and opening a tty raises DTR/RTS, which on
/// a cable keys the transmitter. So the engine never hunts: it opens only the
/// serials written here. `astar-server vocoder add <serial>` probes a stick
/// and writes it here; listing a serial by hand trusts it without a probe.
///
/// There is deliberately no `port`, `scan` or `any` key — nothing may widen
/// the list beyond the serials written down — and `deny_unknown_fields`
/// turns one into a startup error.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VocoderCfg {
    /// USB serial numbers of the trusted sticks.
    #[serde(default)]
    pub trusted_serials: Vec<String>,
    /// D-Star's vocoder (astar-dstar-perens): `"dongle"` (the default when
    /// absent) or `"software"`. D-Star ONLY: there is deliberately no `ysf`,
    /// `nxdn` or `dmr` key — their AMBE+2 is hardware-only (US 8,359,197 until
    /// 2028-05-20) — and `deny_unknown_fields` turns one into a startup error.
    /// `"software"` needs a build with the `dstar-soft-ambe` feature; without
    /// it, the node refuses to start rather than quietly use the dongle.
    #[serde(default)]
    pub dstar: Option<String>,
}

/// `[vocoder] dstar`, checked: the parsed choice, or the start-up error.
///
/// Takes the "is the software vocoder compiled" answer as an argument so both
/// builds' behaviour is testable from either.
pub fn check_dstar_vocoder(
    value: Option<&str>,
    software_compiled: bool,
) -> Result<astar_station::DstarVocoder, String> {
    let Some(value) = value else {
        return Ok(astar_station::DstarVocoder::Dongle);
    };
    match astar_station::DstarVocoder::parse(value) {
        None => Err(format!(
            "vocoder.dstar: unknown value {value:?} (expected \"dongle\" or \"software\")"
        )),
        Some(astar_station::DstarVocoder::Software) if !software_compiled => Err(
            "vocoder.dstar: \"software\" needs an astar-server built with the dstar-soft-ambe feature, and this one was not; use \"dongle\", or rebuild with --features dstar-soft-ambe".to_string(),
        ),
        Some(v) => Ok(v),
    }
}

/// The longest serial `[vocoder] trusted_serials` accepts. FTDI's own are 8
/// characters; the cap only keeps a pasted paragraph out of the file.
pub const VOCODER_SERIAL_MAX: usize = 64;

/// One `trusted_serials` entry, by the loader's rule: 1–64 printable ASCII
/// characters, no whitespace and no comma (the separators every other trust
/// list spelling uses), written exactly — edge whitespace is refused rather
/// than trimmed, so the file says what is matched.
///
/// # Errors
/// The sentence, prefixed with `vocoder.trusted_serials: `.
pub fn check_vocoder_serial(serial: &str) -> Result<(), String> {
    let problem = if serial.is_empty() {
        Some("a serial must not be empty".to_string())
    } else if serial.len() > VOCODER_SERIAL_MAX {
        Some(format!(
            "{serial:?} is longer than {VOCODER_SERIAL_MAX} characters"
        ))
    } else if !serial.bytes().all(|b| b.is_ascii_graphic() && b != b',') {
        Some(format!(
            "{serial:?} is not a USB serial number (printable ASCII, no spaces or commas)"
        ))
    } else {
        None
    };
    problem.map_or(Ok(()), |p| Err(format!("vocoder.trusted_serials: {p}")))
}

/// The directory servers an `[echolink]` section with no `servers` key logs
/// in to, in the order they are tried. They live here, in astar-server's
/// config default, and nowhere in the engine crates.
pub const ECHOLINK_DEFAULT_SERVERS: &[&str] = &[
    "servers.echolink.org",
    "naeast.echolink.org",
    "nasouth.echolink.org",
    "backup.echolink.org",
];

fn default_echolink_servers() -> Vec<String> {
    ECHOLINK_DEFAULT_SERVERS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// `[echolink]` — log this node in to `EchoLink` as a link (`-L`) or repeater
/// (`-R`) station, so an admin can dial `EchoLink` stations into the
/// conference. Absent ⇒ nothing `EchoLink` happens at all.
///
/// There is no password key: the password is the secret `echolink:<CALL>`
/// (env `ASTAR_ECHOLINK_PASSWORD`, the credentials file, or `POST /secrets`),
/// and `deny_unknown_fields` turns a `password = ...` line into a load
/// error rather than a password sitting in `node.toml`. That is also why
/// `Debug` can be derived: nothing here is secret.
///
/// `EchoLink`'s access policy forbids linking it to `VoIP` systems people reach
/// straight from a computer, so the section refuses to load beside
/// `[discord]` and beside the public listener (see
/// [`NodeFileConfig::public_listener_configured`]).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EchoLinkCfg {
    /// The node's `EchoLink` callsign, `-L` or `-R` suffixed. Uppercased at
    /// load.
    pub callsign: String,
    /// Directory servers, tried in order. Default
    /// [`ECHOLINK_DEFAULT_SERVERS`].
    #[serde(default = "default_echolink_servers")]
    pub servers: Vec<String>,
    /// The location text other stations see in the station list.
    #[serde(default)]
    pub location: String,
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// The node daemon's on-disk TOML configuration. **Secret-free** — no passwords,
/// no tokens, no API keys. Credentials are injected at runtime via the
/// [`crate::secrets::SecretProvider`].
///
/// `Debug` is implemented manually to avoid emitting field/struct names that
/// contain the word "secret" (which would trip the `config_has_no_secret_fields`
/// guard test), while still being useful for diagnostics.
#[derive(Clone, serde::Deserialize)]
pub struct NodeFileConfig {
    pub listener: ListenerCfg,
    pub register: Option<RegisterCfg>,
    pub audio: Option<AudioCfg>,
    pub announce: Option<AnnounceCfg>,
    /// Conference-bridge topology (iax-647d). Absent ⇒ daemon default
    /// `mode = "bridge"` (see [`NodeFileConfig::to_bridge_config`]).
    pub bridge: Option<BridgeCfg>,
    /// Parrot-mode tuning (iax-feab). Only consulted when `bridge.mode =
    /// "parrot"`; absent ⇒ [`astar_audio::ParrotTuning::default`].
    pub parrot: Option<ParrotFileCfg>,
    /// Discord voice bridge (`[discord]`). Absent ⇒ inert.
    pub discord: Option<DiscordCfg>,
    /// `[m17]` receive-only reflector monitor. Absent ⇒ M17 off.
    pub m17: Option<M17Cfg>,
    /// `[vocoder]` trusted AMBE sticks. Absent ⇒ no vocoder is ever opened.
    pub vocoder: Option<VocoderCfg>,
    /// `[echolink]` node login. Absent ⇒ `EchoLink` off.
    pub echolink: Option<EchoLinkCfg>,
    pub control: ControlCfg,
    pub secrets: SecretsCfg,
    /// `WireGuard` link transport (iax-580b). Absent ⇒ plain UDP.
    pub wireguard: Option<WireguardCfg>,
    /// `AllStarLink` portal account for WT-token minting (iax-b7f2). Absent ⇒
    /// no minting; `wt-guest` link dials fall back to the node-id
    /// `CALLING_NAME` (which WT contexts reject).
    pub portal: Option<PortalCfg>,
    /// Per-target link dial profiles (iax-5029). Absent ⇒ every target
    /// dials `LinkShape::Standard`.
    #[serde(default)]
    pub links: std::collections::HashMap<String, LinkCfg>,
    /// DTMF `*` command execution (iax-d254). Absent ⇒ disabled.
    pub dtmf: Option<DtmfCfg>,
    /// `[web]` — the public listener page and its HTTPS. Absent ⇒ none.
    pub web: Option<WebCfg>,
    /// `[stream]` — the live audio streams. Absent ⇒ off.
    pub stream: Option<StreamCfg>,
    /// `[recording]` — the node's transmission log. Absent ⇒ off.
    pub recording: Option<RecordingCfg>,
    /// `[packet_log]` — the received-packet debug log. Absent ⇒ no capture
    /// button and no logs to reach.
    pub packet_log: Option<PacketLogCfg>,
    /// `[netrec]` — the net recordings pages. Absent ⇒ no `/nets/`.
    pub netrec: Option<NetrecCfg>,
    /// Raw top-level `codec_policy` string (iax-31f7); absent ⇒ `None` ⇒
    /// `PreferSlin16`. Parsed eagerly into [`Self::codec_policy`] by
    /// [`NodeFileConfig::from_toml_str`]; see [`parse_codec_policy`].
    #[serde(default, rename = "codec_policy")]
    codec_policy_raw: Option<String>,
    /// Parsed codec-negotiation policy (iax-31f7). NOT deserialized directly
    /// (`#[serde(skip)]`) — populated by [`NodeFileConfig::from_toml_str`] from
    /// [`Self::codec_policy_raw`]. Feeds both `to_inbound()`'s
    /// `IncomingCallPolicy.codec_policy` and (via `main.rs`) the Station's
    /// outbound `StationConfig.codec_policy`.
    #[serde(skip)]
    pub codec_policy: CodecPolicy,
}

impl std::fmt::Debug for NodeFileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeFileConfig")
            .field("listener", &self.listener)
            .field("register", &self.register)
            .field("audio", &self.audio)
            .field("announce", &self.announce)
            .field("bridge", &self.bridge)
            .field("parrot", &self.parrot)
            .field("discord", &self.discord)
            .field("m17", &self.m17)
            .field("vocoder", &self.vocoder)
            .field("echolink", &self.echolink)
            .field("control", &self.control)
            .field("codec_policy_raw", &self.codec_policy_raw)
            .field("codec_policy", &self.codec_policy)
            // Use a neutral key so "secret" never appears in the Debug output.
            .field("credential_source", &self.secrets.source)
            .field("credential_file", &self.secrets.file)
            .field("wireguard", &self.wireguard)
            .field("portal", &self.portal)
            .field("links", &self.links)
            .field("dtmf", &self.dtmf)
            .field("web", &self.web)
            .field("stream", &self.stream)
            .field("recording", &self.recording)
            .field("packet_log", &self.packet_log)
            .field("netrec", &self.netrec)
            .finish()
    }
}

impl NodeFileConfig {
    /// Parse a TOML string into a `NodeFileConfig`, validating that the
    /// `answer`, `auth`, and `codec_policy` strings are recognised.
    ///
    /// Returns `Err(String)` on both TOML parse errors and unknown policy strings.
    /// A parse error is toml's full rendering, source line included — right
    /// for the daemon's own stderr at boot, wrong for anything that travels
    /// over HTTP; see [`Self::from_toml_str_redacted`].
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let cfg = toml::from_str::<Self>(s)
            .map_err(|e| e.to_string())?
            .validated()?;
        if let Some(warning) = cfg.startup_warning() {
            eprintln!("{warning}");
        }
        Ok(cfg)
    }

    /// The settings page's loader: the same validators as
    /// [`Self::from_toml_str`], but a parse error carries only
    /// `toml::de::Error::message()`, and a non-string `[secrets] secret` is
    /// refused by name before serde can describe it.
    ///
    /// toml's `Display` quotes the offending source line, and on a node with
    /// `[secrets] source = "config"` that line can be the one holding the
    /// registration password. The message alone ("invalid basic string",
    /// "missing field `channel_id`") is what an operator at the page needs,
    /// and it is the only part that is safe to send back — except for a type
    /// error, whose message quotes the value: `secret = 12345678` (an
    /// all-digit password with its quotes dropped) would come back as
    /// "invalid type: integer `12345678`, expected a string". So the document
    /// is read as a plain table first and that one key is checked by type.
    /// `secrets` itself is checked the same way: written as a bare value
    /// (`secrets = 12345678`, `secrets = ["…"]`) instead of a table, serde
    /// would quote it too, or read an array's first element as `source` and
    /// echo it back through that validator.
    pub fn from_toml_str_redacted(s: &str) -> Result<Self, String> {
        let table: toml::Table = toml::from_str(s).map_err(|e| e.message().to_string())?;
        if table.get("secrets").is_some_and(|v| !v.is_table()) {
            return Err("secrets: expected a table".to_string());
        }
        for key in INLINE_CREDENTIAL_KEYS {
            if table
                .get("secrets")
                .and_then(|t| t.get(*key))
                .is_some_and(|v| !v.is_str())
            {
                return Err(format!("secrets.{key}: expected a string"));
            }
        }
        let cfg: Self = toml::from_str(s).map_err(|e| e.message().to_string())?;
        cfg.validated()
    }

    /// The load-time warning for this config, as a value, or `None`. Only
    /// [`Self::from_toml_str`] prints it; a caller that re-reads the file on
    /// a timer — the settings page — must not repeat it into the log.
    /// Private: its only callers are `from_toml_str` and this file's tests.
    #[must_use]
    fn startup_warning(&self) -> Option<&'static str> {
        let config_source = self.secrets.source.eq_ignore_ascii_case("config");
        let has_secret = self
            .secrets
            .secret
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        (config_source && !has_secret).then_some(CONFIG_SOURCE_WITHOUT_SECRET_WARNING)
    }

    /// The raw top-level `codec_policy` string as written, if any. The
    /// settings page shows what is on disk, not the default the parsed
    /// [`Self::codec_policy`] falls back to when the key is absent.
    #[must_use]
    pub fn codec_policy_raw(&self) -> Option<&str> {
        self.codec_policy_raw.as_deref()
    }

    /// Eagerly validate the string fields so callers see clear errors at load
    /// time. One body for both loaders, so they can never drift apart.
    fn validated(mut self) -> Result<Self, String> {
        self.parse_answer_policy()?;
        self.parse_auth_policy()?;
        self.to_bridge_config()?;
        self.parse_audio_backend()?;
        self.parse_secrets_source()?;
        self.validate_discord()?;
        self.validate_m17()?;
        self.validate_vocoder()?;
        self.validate_echolink()?;
        self.validate_control()?;
        self.validate_web()?;
        self.validate_stream()?;
        self.validate_recording()?;
        self.validate_packet_log()?;
        self.codec_policy = parse_codec_policy(self.codec_policy_raw.as_deref())?;
        for (node, link) in &self.links {
            if let Some(shape) = link.shape.as_deref()
                && !shape.eq_ignore_ascii_case("standard")
                && !shape.eq_ignore_ascii_case("wt-guest")
            {
                return Err(format!(
                    "links.\"{node}\": unknown shape \"{shape}\" (expected \"standard\" or \"wt-guest\")"
                ));
            }
        }
        Ok(self)
    }

    /// Load the config at `path`, bootstrapping a commented template if the
    /// file does not exist yet (iax-4703 Task 9).
    ///
    /// - **Missing path:** create parent directories as needed, write
    ///   [`crate::template::NODE_TOML_TEMPLATE`], log prominently, then load
    ///   the just-written file and return it. This is the shared load path
    ///   used by both `serve` and `tui`, so a fresh container mounting an
    ///   empty config directory boots into a safe default node (listener up,
    ///   no registration) rather than crashing — generate-and-exit would
    ///   crashloop under `--restart=always`.
    /// - **Existing path:** read + parse, unmodified.
    /// - **Unwritable parent:** returns `Err` naming `path`.
    pub fn load_or_bootstrap(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("cannot create config directory {}: {e}", parent.display())
                })?;
            }
            std::fs::write(path, crate::template::NODE_TOML_TEMPLATE)
                .map_err(|e| format!("cannot write template config to {}: {e}", path.display()))?;
            eprintln!(
                "astar-server: generated template config at {} — edit it (node_id, [register]), set the node password on the settings page, and restart",
                path.display()
            );
        }
        let toml_str = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        Self::from_toml_str(&toml_str)
    }

    /// Build an [`InboundConfig`] from this file config.
    ///
    /// Returns `Err(String)` if the `bind` address is not a valid [`SocketAddr`].
    pub fn to_inbound(&self) -> Result<InboundConfig, String> {
        let bind: SocketAddr = self
            .listener
            .bind
            .parse()
            .map_err(|e| format!("listener.bind: {e}"))?;
        let answer = self.parse_answer_policy()?;
        let auth = self.parse_auth_policy()?;
        // An empty `allowed_nodes` (omitted or `[]`) means "admit all": keep the
        // allowlist `None` so the admission path is a no-op. A non-empty list
        // becomes a `KnownNodes` allowlist enforced at call-setup time.
        let allowlist = if self.listener.allowed_nodes.is_empty() {
            None
        } else {
            Some(KnownNodes::from_iter_labels(
                self.listener.allowed_nodes.iter().cloned(),
            ))
        };
        Ok(InboundConfig {
            bind,
            policy: IncomingCallPolicy {
                auth,
                codec_policy: self.codec_policy,
                ..IncomingCallPolicy::default()
            },
            answer,
            max_calls: self.listener.max_calls,
            allowlist,
        })
    }

    /// Build a [`RegisterConfig`] from the optional `[register]` section.
    ///
    /// Returns `None` if the section is absent.
    /// Returns `Err(String)` if the `peer` address is not a valid [`SocketAddr`].
    pub fn to_register(&self) -> Result<Option<RegisterConfig>, String> {
        let Some(reg) = &self.register else {
            return Ok(None);
        };
        // Resolve rather than parse. A literal `IP:port` still works and
        // yields one candidate; a hostname yields every address it has, which
        // is what makes the registrar failover in `RegisterConfig` possible
        // at all. Requiring a literal is why deployments pin one address —
        // and pinning is what fails silently when that host stops answering.
        let cfg = RegisterConfig::resolve(&reg.peer, reg.node_id.clone(), Duration::from_secs(60))
            .map_err(|e| format!("register.peer {}: {e}", reg.peer))?;
        Ok(Some(cfg))
    }

    /// Build the engine [`astar_iax::LinkTransport`] from the optional
    /// `[wireguard]` section (iax-580b).
    ///
    /// - Absent section (or an explicit `enabled = false`) →
    ///   [`astar_iax::LinkTransport::Udp`] — plain OS UDP, byte-identical to
    ///   today's defaults.
    /// - Present section → [`astar_iax::LinkTransport::Wireguard`] carrying a
    ///   validated [`astar_wireguard::WgLinkConfig`]; the whole engine
    ///   (outgoing + registrar + inbound) rides the shared userspace stack.
    ///
    /// Secret-free (iax-8516): the config carries only the private-key
    /// *reference* named by `secret_ref` (default `WIREGUARD_PRIVATE_KEY`);
    /// the base64 x25519 secret is resolved through a `SecretResolver` when
    /// the stack is built — never read from the TOML file, never stored in
    /// the config. Returns `Err(String)` if any field is invalid.
    pub fn to_link_transport(&self) -> Result<astar_iax::LinkTransport, String> {
        let Some(wg) = &self.wireguard else {
            return Ok(astar_iax::LinkTransport::Udp);
        };
        if !wg.enabled {
            return Ok(astar_iax::LinkTransport::Udp);
        }
        let also_bind_udp = wg
            .also_bind_udp
            .as_deref()
            .map(|s| {
                s.parse::<SocketAddr>()
                    .map_err(|e| format!("wireguard.also_bind_udp: {e}"))
            })
            .transpose()?;
        let cfg = astar_wireguard::WgLinkConfig::new(
            wg.secret_ref.as_deref().unwrap_or("WIREGUARD_PRIVATE_KEY"),
            &wg.address,
            &wg.peer_public_key,
            &wg.endpoint,
            &wg.allowed_ips,
            wg.keepalive_secs.unwrap_or(25),
        )
        .map_err(|e| format!("wireguard: {e}"))?
        .with_also_bind_udp(also_bind_udp);
        Ok(astar_iax::LinkTransport::Wireguard(cfg))
    }

    /// Build an [`astar_iax::BridgeConfig`] from the optional `[bridge]` section
    /// (iax-647d). **The daemon default is `mode = "bridge"`**: an ABSENT
    /// `[bridge]` section yields a pure mix-minus bridge (local radio off), NOT
    /// the library handset default. Within a present section, omitted fields fall
    /// back to `mode = "bridge"`, `mix_minus = true`, `include_local_radio =
    /// false`.
    ///
    /// When `mode = "parrot"`, also builds [`astar_audio::ParrotTuning`]
    /// from the optional `[parrot]` section (iax-feab) — see
    /// [`Self::parrot_tuning`]. For every other mode `parrot` is `None`.
    ///
    /// Returns `Err(String)` if `mode` is not one of `"handset"`, `"bridge"`,
    /// `"conference"`, or `"parrot"` (case-insensitive).
    pub fn to_bridge_config(&self) -> Result<BridgeConfig, String> {
        let cfg = self.bridge.clone().unwrap_or_default();
        let mode = match cfg.mode.to_ascii_lowercase().as_str() {
            "handset" => BridgeMode::Handset,
            "bridge" => BridgeMode::Bridge,
            "conference" => BridgeMode::Conference,
            "parrot" => BridgeMode::Parrot,
            other => {
                return Err(format!(
                    "bridge.mode: unknown value {other:?} (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
                ));
            }
        };
        let parrot = matches!(mode, BridgeMode::Parrot).then(|| self.parrot_tuning());
        Ok(BridgeConfig {
            mode,
            mix_minus: cfg.mix_minus,
            include_local_radio: cfg.include_local_radio,
            parrot,
        })
    }

    /// Build [`astar_audio::ParrotTuning`] from the optional `[parrot]`
    /// section (iax-feab). Milliseconds on disk are converted to 20 ms ticks
    /// (`ms / 20`); an absent section, or an absent individual field, falls
    /// back to the matching [`astar_audio::ParrotTuning::default`] value.
    /// `max_record_ticks` always stays the library default — it is not
    /// TOML-exposed.
    #[allow(clippy::cast_possible_truncation)]
    fn parrot_tuning(&self) -> astar_audio::ParrotTuning {
        let default = astar_audio::ParrotTuning::default();
        let p = self.parrot.as_ref();
        astar_audio::ParrotTuning {
            playback_delay_ticks: p
                .and_then(|p| p.playback_delay_ms)
                .map_or(default.playback_delay_ticks, |ms| (ms / 20) as u32),
            silence_gap_ticks: p
                .and_then(|p| p.silence_gap_ms)
                .map_or(default.silence_gap_ticks, |ms| (ms / 20) as u32),
            vox_threshold_db: p
                .and_then(|p| p.vox_threshold_db)
                .unwrap_or(default.vox_threshold_db),
            max_record_ticks: default.max_record_ticks,
        }
    }

    /// Resolved dial shape for `node` (iax-5029). Absent table/entry ⇒
    /// `LinkShape::Standard` — byte-identical defaults.
    #[must_use]
    pub fn link_shape(&self, node: &str) -> LinkShape {
        match self.links.get(node).and_then(|l| l.shape.as_deref()) {
            Some(s) if s.eq_ignore_ascii_case("wt-guest") => LinkShape::WtGuest,
            _ => LinkShape::Standard,
        }
    }

    /// Whether the node should join the Discord channel as soon as it is up
    /// (`[discord] join_at_start`). False when there is no `[discord]`
    /// section at all — the gate `main.rs` reads before issuing the join.
    #[must_use]
    pub fn discord_join_at_start(&self) -> bool {
        self.discord.as_ref().is_some_and(|d| d.join_at_start)
    }

    /// Whether DTMF `*` command execution is enabled (iax-d254; default off).
    #[must_use]
    pub fn dtmf_enabled(&self) -> bool {
        self.dtmf.as_ref().is_some_and(|d| d.enabled)
    }

    /// Inter-digit timeout finalizing a pending DTMF command (default 3000 ms).
    #[must_use]
    pub fn dtmf_inter_digit_timeout_ms(&self) -> u64 {
        self.dtmf
            .as_ref()
            .and_then(|d| d.inter_digit_timeout_ms)
            .unwrap_or(3000)
    }

    /// Where the credentials file lives under `[secrets] source = "file"`:
    /// `[secrets] file` resolved against the directory of `config_path` (an
    /// absolute `file` is used as it is), else
    /// [`crate::secrets::CREDENTIALS_FILE_NAME`] beside the config.
    ///
    /// Resolved against the config and never the working directory, because
    /// the three ways this daemon is started — podman's PID 1, a systemd
    /// unit, `cargo run` under `just node` — have three different cwds, and
    /// a file that moves with the cwd is a file that is sometimes empty.
    #[must_use]
    pub fn credentials_path(&self, config_path: &std::path::Path) -> std::path::PathBuf {
        let dir = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(""));
        match self.secrets.file.as_deref() {
            Some(file) => dir.join(file),
            None => dir.join(crate::secrets::CREDENTIALS_FILE_NAME),
        }
    }

    /// Load the node's credentials into `secrets` per `[secrets] source` —
    /// the whole startup path. The four sources are exclusive: one active
    /// source, no precedence stack.
    ///
    /// `env` reads the well-known variables; `config` (iax-4703) stores the
    /// inline `secret` under `register.node_id`, the same identity the env
    /// path registers as; `file` loads [`Self::credentials_path`] with
    /// `node` mapped onto `register.node_id`; `control` loads nothing, its
    /// credentials arrive over `ProvideSecret`. Under every source but
    /// `file` the portal password is then seeded from the variable
    /// `[portal] credential_env` names, so the provider is the one place
    /// [`Self::portal_credentials`] reads it from. Under `file` that
    /// variable is never read.
    ///
    /// `Some` only under `file`, the one source with a line to report: the
    /// path and the number of entries loaded, or the refusal sentence from
    /// [`crate::secrets::SecretProvider::load_file`]. The node boots either
    /// way — a refusal must not crash-loop it under `--restart=always`.
    #[must_use]
    pub fn load_credentials(
        &self,
        config_path: &std::path::Path,
        secrets: &crate::secrets::SecretProvider,
    ) -> Option<Result<(std::path::PathBuf, usize), String>> {
        let source = self.secrets.source.to_ascii_lowercase();
        if source == "file" {
            let path = self.credentials_path(config_path);
            let node_id = self.register.as_ref().map(|r| r.node_id.as_str());
            return Some(secrets.load_file(&path, node_id).map(|n| (path, n)));
        }
        match source.as_str() {
            "env" => secrets.load_env(),
            "config" => {
                if let Some(reg) = &self.register {
                    secrets.load_config_secret(
                        &reg.node_id,
                        self.secrets.secret.as_deref().unwrap_or(""),
                    );
                }
            }
            // "control"; parse_secrets_source has refused anything else.
            _ => {}
        }
        if let Some(p) = &self.portal {
            secrets.load_portal_env(&p.credential_env);
        }
        if source == "config" {
            // After the env seeding, so a value written in the file wins.
            for (key, value) in [
                (crate::secrets::PORTAL_CREDENTIAL_KEY, &self.secrets.portal),
                (
                    crate::secrets::DISCORD_CREDENTIAL_KEY,
                    &self.secrets.discord,
                ),
                (crate::secrets::PORKBUN_PK_KEY, &self.secrets.porkbun_pk),
                (crate::secrets::PORKBUN_SK_KEY, &self.secrets.porkbun_sk),
            ] {
                if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                    secrets.put(key, v);
                }
            }
        }
        None
    }

    /// A warning when credentials sit inline in `node.toml` (`[secrets]
    /// source = "config"`) and group or others can read the file. Names
    /// only; the node still starts (a chmod must not crash-loop it).
    #[must_use]
    pub fn inline_credentials_mode_warning(&self, config_path: &std::path::Path) -> Option<String> {
        if !self.secrets.source.eq_ignore_ascii_case("config") {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(config_path).ok()?.permissions().mode() & 0o777;
            (mode & 0o077 != 0).then(|| {
                format!(
                    "WARNING: {} holds credentials ([secrets] source = \"config\") and is readable by group or others (mode {mode:#o}) — run: chmod 600 {}",
                    config_path.display(),
                    config_path.display()
                )
            })
        }
        #[cfg(not(unix))]
        {
            let _ = config_path;
            None
        }
    }

    /// The `[portal]` account (iax-b7f2) for `StationConfig.portal`, its
    /// password read from `secrets` under the `portal` key under every
    /// source — [`Self::load_credentials`] put it there. `Ok(None)` without
    /// a `[portal]` section. `Err` when the section is there and the
    /// password is not: `[portal] configured but <holder> is unset —
    /// wt-guest links will dial without a WT token`, the holder being
    /// `credentials.toml key "portal"` under `file` and the variable's name
    /// otherwise. Names only; the value never reaches the error.
    pub fn portal_credentials(
        &self,
        secrets: &crate::secrets::SecretProvider,
    ) -> Result<Option<astar_station::PortalCredentials>, String> {
        let Some(p) = &self.portal else {
            return Ok(None);
        };
        let password = secrets.resolve(crate::secrets::PORTAL_CREDENTIAL_KEY);
        if password.is_empty() {
            let holder = if self.secrets.source.eq_ignore_ascii_case("file") {
                format!(
                    "{} key \"{}\"",
                    crate::secrets::CREDENTIALS_FILE_NAME,
                    crate::secrets::PORTAL_CREDENTIAL_KEY
                )
            } else {
                p.credential_env.clone()
            };
            return Err(format!(
                "[portal] configured but {holder} is unset — wt-guest links will dial without a WT token"
            ));
        }
        Ok(Some(astar_station::PortalCredentials {
            user: p.user.clone(),
            password,
            node: p.node.clone(),
        }))
    }

    // ------------------------------------------------------------------
    // Private helpers
    // ------------------------------------------------------------------

    fn parse_answer_policy(&self) -> Result<AnswerPolicy, String> {
        match self.listener.answer.to_ascii_lowercase().as_str() {
            "auto" => Ok(AnswerPolicy::Auto),
            "manual" => Ok(AnswerPolicy::Manual),
            other => Err(format!(
                "listener.answer: unknown value {other:?} (expected \"auto\" or \"manual\")"
            )),
        }
    }

    fn parse_auth_policy(&self) -> Result<IncomingAuthPolicy, String> {
        match self.listener.auth.to_ascii_lowercase().as_str() {
            "required" => Ok(IncomingAuthPolicy::Required),
            "optional" => Ok(IncomingAuthPolicy::Optional),
            "off" => Ok(IncomingAuthPolicy::Off),
            other => Err(format!(
                "listener.auth: unknown value {other:?} (expected \"required\", \"optional\", or \"off\")"
            )),
        }
    }

    /// Validate the `[secrets] source` string (case-insensitive).
    ///
    /// `"env"` and `"control"` are accepted as before; `"config"` (iax-4703)
    /// too, with or without an inline `secret` — the empty shape is the one
    /// the generated template used to ship, and only the startup loader
    /// warns about it (see [`Self::startup_warning`]). `"file"` selects the
    /// credentials file beside the config and needs nothing else in this
    /// section: a missing file is a node with no credentials yet, which
    /// `main.rs` reports when it loads, not a config error.
    ///
    /// Silent on purpose: [`Self::validated`] runs it for both loaders, and
    /// the settings page's loader runs every five seconds.
    fn parse_secrets_source(&self) -> Result<(), String> {
        match self.secrets.source.to_ascii_lowercase().as_str() {
            "env" | "control" | "config" | "file" => Ok(()),
            other => Err(format!(
                "secrets.source: unknown value {other:?} (expected \"env\", \"control\", \"config\", or \"file\")"
            )),
        }
    }

    /// Validate the optional `[discord] approvers` list, if present.
    ///
    /// Eager, at load time, and fatal: an approver id that is not a
    /// snowflake is a typo in the ONE list that decides who may make admins,
    /// and the failure mode of accepting it is silent — the entry simply
    /// never matches anyone, and the operator discovers it the first time
    /// they try to use a command they believed they owned. `from_toml_str`
    /// is the only place that can turn that into a message naming the
    /// offending entry before the node is running.
    fn validate_discord(&self) -> Result<(), String> {
        match &self.discord {
            None => Ok(()),
            Some(d) => d.approver_ids().map(|_| ()),
        }
    }

    /// Fatal at load: a reflector key failing `validate_reflector_name`, a value
    /// failing `parse_host_port`, two keys equal after uppercasing, a
    /// `directory` that is neither "bundled" nor an absolute path, or a
    /// relative `codec_dirs` entry.
    ///
    /// These are typos in a table the operator wrote. Refusing to start names
    /// the bad line; accepting it would leave a reflector that silently never
    /// resolves. Whether a directory FILE loads is not checked here — see
    /// [`crate::m17_directory::M17Settings::from_cfg`].
    fn validate_m17(&self) -> Result<(), String> {
        match &self.m17 {
            None => Ok(()),
            Some(m) => crate::m17_directory::check_cfg(m),
        }
    }

    /// Every `[vocoder] trusted_serials` entry by [`check_vocoder_serial`]. A
    /// typo here would leave a stick silently untrusted, so it stops the node.
    fn validate_vocoder(&self) -> Result<(), String> {
        self.vocoder
            .iter()
            .flat_map(|v| &v.trusted_serials)
            .try_for_each(|s| check_vocoder_serial(s))?;
        self.dstar_vocoder().map(|_| ())
    }

    /// `[vocoder] dstar`: which vocoder D-Star uses. The dongle when absent.
    ///
    /// # Errors
    /// An unknown value, or `"software"` in a build without
    /// `dstar-soft-ambe` — see [`check_dstar_vocoder`].
    pub fn dstar_vocoder(&self) -> Result<astar_station::DstarVocoder, String> {
        check_dstar_vocoder(
            self.vocoder.as_ref().and_then(|v| v.dstar.as_deref()),
            astar_station::software_dstar_vocoder_compiled(),
        )
    }

    /// The trust list the station opens vocoders by: `[vocoder]
    /// trusted_serials`, de-duplicated. Empty when the section is absent.
    #[must_use]
    pub fn vocoder_trust(&self) -> astar_station::VocoderTrust {
        astar_station::VocoderTrust::new(
            self.vocoder
                .iter()
                .flat_map(|v| v.trusted_serials.iter().cloned()),
        )
    }

    /// Whether this node serves the public listener: a `[web] public_bind`
    /// page, or `[stream] enabled = true`. Named apart from
    /// [`Self::listener`], which is the IAX inbound socket.
    #[must_use]
    pub fn public_listener_configured(&self) -> bool {
        self.web.as_ref().is_some_and(|w| w.public_bind.is_some())
            || self.stream.as_ref().is_some_and(|s| s.enabled)
    }

    /// Uppercase `[echolink] callsign` and refuse what `EchoLink` would: a
    /// callsign with no `-L` or `-R` suffix, or an empty server list. Then
    /// refuse the combinations `EchoLink`'s access policy forbids — the
    /// Discord bridge and the public listener both let people reach the
    /// conference straight from a computer. Because `/settings` saves go
    /// through [`Self::validated`] too, a save that would make either
    /// combination is refused with the same sentence.
    fn validate_echolink(&mut self) -> Result<(), String> {
        const POLICY: &str = "EchoLink's access policy forbids linking it to VoIP systems people reach directly from a computer";
        let public_listener = self.public_listener_configured();
        let discord = self.discord.is_some();
        let Some(e) = self.echolink.as_mut() else {
            return Ok(());
        };
        let call = e.callsign.trim().to_ascii_uppercase();
        let base = call
            .strip_suffix("-L")
            .or_else(|| call.strip_suffix("-R"))
            .filter(|b| !b.is_empty());
        if base.is_none() {
            return Err(format!(
                "echolink.callsign: {:?} has no -L or -R suffix; EchoLink requires one for a node",
                e.callsign
            ));
        }
        e.callsign = call;
        e.servers = e
            .servers
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if e.servers.is_empty() {
            return Err("echolink.servers: at least one server is needed".to_string());
        }
        if discord {
            return Err(format!(
                "echolink: can't run with [discord] — {POLICY}; remove one of the two sections"
            ));
        }
        if public_listener {
            return Err(format!(
                "echolink: can't run with the public listener ([web] public_bind or [stream] enabled) — {POLICY}; remove one of them"
            ));
        }
        Ok(())
    }

    /// Normalise and check `[web]`: hostnames lower-cased with a trailing
    /// dot stripped and refused when they cannot carry a certificate; a
    /// `public_bind` needs at least one; binds parse; DNS-01 needs a
    /// provider; every hostname sits under `[web.dns] domain`. Each message
    /// starts with the key it is about.
    /// `[control] bind` may not be a wildcard unless the file opts in. Only a
    /// literal is judged: a name is never resolved here.
    fn validate_control(&self) -> Result<(), String> {
        let bind = &self.control.bind;
        let wildcard = bind
            .parse::<SocketAddr>()
            .is_ok_and(|a| a.ip().is_unspecified());
        if wildcard && !self.control.allow_wildcard_bind {
            return Err(format!(
                "control.bind: {bind:?} is a wildcard bind, which exposes the keying port on every interface; bind it to 127.0.0.1 (or set [control] allow_wildcard_bind = true inside a sealed container)"
            ));
        }
        Ok(())
    }

    fn validate_web(&mut self) -> Result<(), String> {
        let node = self.register.as_ref().map(|r| r.node_id.clone());
        let Some(web) = self.web.as_mut() else {
            return Ok(());
        };
        if let Some(dns) = web.dns.as_mut() {
            let domain = normalize_hostname(&dns.domain);
            if let Some(problem) = hostname_problem(&domain) {
                return Err(format!("web.dns.domain: {problem}"));
            }
            dns.domain = domain;
        }
        // The templates as written, each with the key its error should name.
        let templates: Vec<(String, &str)> = match (&web.public_hostnames, &web.dns) {
            (Some(list), _) => list
                .iter()
                .map(|h| (h.clone(), "web.public_hostnames"))
                .collect(),
            (None, Some(dns)) => {
                let host = dns.host.as_deref().unwrap_or(DEFAULT_DNS_HOST).trim();
                let name = if host.is_empty() {
                    dns.domain.clone()
                } else {
                    format!("{host}.{}", dns.domain)
                };
                vec![(name, "web.dns.host")]
            }
            (None, None) => Vec::new(),
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut names = Vec::with_capacity(templates.len());
        for (template, key) in &templates {
            let expanded = expand_node_token(template, node.as_deref(), key)?;
            let name = normalize_hostname(&expanded);
            if let Some(problem) = hostname_problem(&name) {
                return Err(format!("{key}: {problem}"));
            }
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        web.hostnames = names;
        if let Some(bind) = web.public_bind.as_deref() {
            bind.parse::<std::net::SocketAddr>()
                .map_err(|e| format!("web.public_bind: {bind:?} is not an address:port ({e})"))?;
            if web.hostnames.is_empty() {
                return Err(
                    "web.public_hostnames: public_bind is set, so at least one hostname is needed"
                        .to_string(),
                );
            }
        }
        if let Some(bind) = web.acme_http_bind.as_deref() {
            bind.parse::<std::net::SocketAddr>().map_err(|e| {
                format!("web.acme_http_bind: {bind:?} is not an address:port ({e})")
            })?;
        }
        if let Some(email) = web.acme_email.as_deref()
            && !email.is_empty()
            && !email.contains('@')
        {
            return Err(format!("web.acme_email: {email:?} is not an email address"));
        }
        validate_web_signin(web)?;
        if web.acme_challenge == Some(AcmeChallenge::Dns01) && web.dns.is_none() {
            return Err(
                "web.acme_challenge: \"dns-01\" needs a [web.dns] provider to write the TXT records"
                    .to_string(),
            );
        }
        if let Some(dns) = web.dns.as_ref() {
            dns.validate_provider()?;
            if dns.ddns && web.hostnames.is_empty() {
                return Err(
                    "web.public_hostnames: [web.dns] ddns = true keeps these names pointed at this box, so at least one is needed"
                        .to_string(),
                );
            }
            let suffix = format!(".{}", dns.domain);
            if let Some(outside) = web
                .hostnames
                .iter()
                .find(|h| **h != dns.domain && !h.ends_with(&suffix))
            {
                return Err(format!(
                    "web.public_hostnames: \"{outside}\" is not under web.dns.domain \"{}\"",
                    dns.domain
                ));
            }
        }
        Ok(())
    }

    /// Check `[stream]`'s limits against what the encoders accept.
    fn validate_stream(&self) -> Result<(), String> {
        let Some(s) = &self.stream else {
            return Ok(());
        };
        if !(1..=200).contains(&s.max_listeners) {
            return Err(format!(
                "stream.max_listeners: {} is outside 1–200",
                s.max_listeners
            ));
        }
        if !MP3_KBPS.contains(&s.mp3_kbps) {
            return Err(format!(
                "stream.mp3_kbps: {} is not one of {MP3_KBPS:?}",
                s.mp3_kbps
            ));
        }
        if !(6..=128).contains(&s.opus_kbps) {
            return Err(format!(
                "stream.opus_kbps: {} is outside 6–128",
                s.opus_kbps
            ));
        }
        Ok(())
    }

    /// Check `[recording]`'s folder and limits.
    fn validate_recording(&self) -> Result<(), String> {
        let Some(r) = &self.recording else {
            return Ok(());
        };
        if r.dir.as_os_str().is_empty() {
            return Err("recording.dir: must not be empty".to_string());
        }
        // Retention deletes files under this folder, so a relative path,
        // which would move with the working directory, is refused.
        if !r.dir.is_absolute() {
            return Err(format!(
                "recording.dir: {} must be an absolute path",
                r.dir.display()
            ));
        }
        if r.max_bytes.bytes < MIN_RECORDING_BYTES {
            return Err(format!(
                "recording.max_bytes: {} is below the 100 MB minimum",
                r.max_bytes.text
            ));
        }
        if !(1..=3650).contains(&r.max_age_days) {
            return Err(format!(
                "recording.max_age_days: {} is outside 1–3650",
                r.max_age_days
            ));
        }
        Ok(())
    }

    /// Check `[packet_log]`'s folder and limit.
    fn validate_packet_log(&self) -> Result<(), String> {
        let Some(p) = &self.packet_log else {
            return Ok(());
        };
        if p.dir.as_os_str().is_empty() {
            return Err("packet_log.dir: must not be empty".to_string());
        }
        // Retention deletes files under this folder, so a relative path,
        // which would move with the working directory, is refused.
        if !p.dir.is_absolute() {
            return Err(format!(
                "packet_log.dir: {} must be an absolute path",
                p.dir.display()
            ));
        }
        if p.max_bytes.bytes < astar_station::MIN_PACKET_LOG_BYTES {
            return Err(format!(
                "packet_log.max_bytes: {} is below the 100 MB minimum",
                p.max_bytes.text
            ));
        }
        Ok(())
    }

    /// Validate the optional `[audio] backend` string, if present.
    ///
    /// Absent `[audio]` section or absent `backend` key both mean "cpal"
    /// (today's behavior) and are not errors here.
    fn parse_audio_backend(&self) -> Result<(), String> {
        let Some(backend) = self.audio.as_ref().and_then(|a| a.backend.as_deref()) else {
            return Ok(());
        };
        match backend {
            "none" | "cpal" => Ok(()),
            other => Err(format!(
                "audio.backend: unknown value {other:?} (expected \"none\" or \"cpal\")"
            )),
        }
    }
}

/// Parse the top-level `codec_policy` string (iax-31f7). `None` (the key
/// absent) defaults to [`CodecPolicy::PreferSlin16`] — deliberately NOT
/// `CodecPolicy::default()`, which is the engine-wide `UlawOnly`.
///
/// A node is the one place where the bandwidth is cheap and the audio is worth
/// it: the astar clients always dial asking for slin16, and a server left on
/// `ulaw_only` answered them in 8-bit companded µ-law, so the default silently
/// threw the wideband path away. `PreferSlin16` advertises
/// slin16 → slin → ulaw → alaw (mask `0x804c`), and a µ-law-only peer still
/// lands on µ-law because a peer's stated FORMAT wins whenever we can carry it
/// (see `choose_codec` in `astar-iax-core`). Operators who need the old wire
/// behaviour set `codec_policy = "ulaw_only"` explicitly.
///
/// Delegates to [`CodecPolicy::from_str`], whose error message already names
/// the field.
fn parse_codec_policy(s: Option<&str>) -> Result<CodecPolicy, String> {
    match s {
        None => Ok(CodecPolicy::PreferSlin16),
        Some(raw) => raw.parse(),
    }
}

// ---------------------------------------------------------------------------
// Filesystem facts the validators need
// ---------------------------------------------------------------------------

/// Why `path` cannot become a state file, if it cannot.
///
/// Only the directory is judged, and only when the path names one: a file
/// that does not exist yet is the normal first run. `Some` means the caller
/// must refuse — at start-up, the node does not boot; on the settings page,
/// the `[discord] state_file` field is rejected — because the alternative is
/// booting clean and failing at the first approval, hours later, with
/// people already muted.
///
/// It lives here rather than in the binary so that a validator in this
/// library can ask the same question the daemon asks, with or without the
/// `discord` feature: the settings oracle checks a proposed `state_file`
/// before a byte of `node.toml` is written. The two sentences reach the
/// operator through that page, so they are part of its contract.
#[must_use]
pub fn state_file_parent_problem(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    // A bare filename parses as an empty parent, which means the working
    // directory and is always fine.
    if parent.as_os_str().is_empty() || parent.is_dir() {
        return None;
    }
    Some(if parent.exists() {
        format!("{} is not a directory", parent.display())
    } else {
        format!("the directory {} does not exist", parent.display())
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
        [listener]
        bind = "0.0.0.0:4569"
        answer = "auto"
        max_calls = 20
        auth = "optional"
        [control]
        bind = "127.0.0.1:8730"
        [secrets]
        source = "env"
        "#
    }

    #[test]
    fn parses_minimal_config() {
        let toml = r#"
            [listener]
            bind = "0.0.0.0:4569"
            answer = "auto"
            max_calls = 20
            auth = "optional"
            [control]
            bind = "127.0.0.1:8730"
            [secrets]
            source = "env"
        "#;
        let c = NodeFileConfig::from_toml_str(toml).unwrap();
        assert_eq!(c.control.bind, "127.0.0.1:8730");
        assert_eq!(c.to_inbound().unwrap().max_calls, 20);
    }

    #[test]
    fn a_wildcard_control_bind_is_refused_at_load_unless_opted_in() {
        for wildcard in ["0.0.0.0:8730", "[::]:8730"] {
            let toml = format!(
                "[listener]\nbind=\"0.0.0.0:4569\"\nanswer=\"auto\"\nmax_calls=2\nauth=\"off\"\n[control]\nbind=\"{wildcard}\"\n[secrets]\nsource=\"env\"\n"
            );
            let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "control.bind: {wildcard:?} is a wildcard bind, which exposes the keying port on every interface; bind it to 127.0.0.1 (or set [control] allow_wildcard_bind = true inside a sealed container)"
                )
            );
            let opted = toml.replace("[secrets]", "allow_wildcard_bind = true\n[secrets]");
            let c = NodeFileConfig::from_toml_str(&opted).expect("the opt-in loads");
            assert!(c.control.allow_wildcard_bind);
        }
        let lan = "[listener]\nbind=\"0.0.0.0:4569\"\nanswer=\"auto\"\nmax_calls=2\nauth=\"off\"\n[control]\nbind=\"192.168.1.9:8730\"\n[secrets]\nsource=\"env\"\n";
        assert!(
            NodeFileConfig::from_toml_str(lan).is_ok(),
            "a LAN literal is the operator's choice"
        );
    }

    #[test]
    fn config_has_no_secret_fields() {
        let toml = "[listener]\nbind=\"0.0.0.0:4569\"\nanswer=\"auto\"\nmax_calls=20\nauth=\"off\"\n[control]\nbind=\"127.0.0.1:8730\"\n[secrets]\nsource=\"env\"\n[echolink]\ncallsign=\"KX7ABC-L\"\n";
        let c = NodeFileConfig::from_toml_str(toml).unwrap();
        let d = format!("{c:?}");
        assert!(d.contains("echolink: Some(EchoLinkCfg {"), "{d}");
        for bad in ["secret", "password"] {
            assert!(!d.contains(bad));
        }
    }

    #[test]
    fn to_inbound_answer_auto() {
        let c = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        let inbound = c.to_inbound().unwrap();
        assert_eq!(inbound.answer, AnswerPolicy::Auto);
    }

    #[test]
    fn to_inbound_answer_manual() {
        let toml = minimal_toml().replace("answer = \"auto\"", "answer = \"Manual\"");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let inbound = c.to_inbound().unwrap();
        assert_eq!(inbound.answer, AnswerPolicy::Manual);
    }

    #[test]
    fn to_inbound_auth_required() {
        let toml = minimal_toml().replace("auth = \"optional\"", "auth = \"Required\"");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let inbound = c.to_inbound().unwrap();
        assert_eq!(inbound.policy.auth, IncomingAuthPolicy::Required);
    }

    #[test]
    fn to_inbound_auth_off() {
        let toml = minimal_toml().replace("auth = \"optional\"", "auth = \"off\"");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let inbound = c.to_inbound().unwrap();
        assert_eq!(inbound.policy.auth, IncomingAuthPolicy::Off);
    }

    #[test]
    fn to_inbound_bind_parsed() {
        let c = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        let inbound = c.to_inbound().unwrap();
        let expected: SocketAddr = "0.0.0.0:4569".parse().unwrap();
        assert_eq!(inbound.bind, expected);
    }

    #[test]
    fn to_inbound_max_calls() {
        let toml = minimal_toml().replace("max_calls = 20", "max_calls = 5");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.to_inbound().unwrap().max_calls, 5);
    }

    #[test]
    fn to_inbound_allowlist_absent_is_none() {
        // The minimal config omits `allowed_nodes` → admit all (allowlist None).
        let c = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(c.to_inbound().unwrap().allowlist.is_none());
    }

    #[test]
    fn to_inbound_allowlist_empty_is_none() {
        // An explicit empty list is the same as absent → admit all (None).
        let toml = minimal_toml().replace("max_calls = 20", "max_calls = 20\nallowed_nodes = []");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert!(c.to_inbound().unwrap().allowlist.is_none());
    }

    #[test]
    fn to_inbound_allowlist_present_contains_entries() {
        let toml = minimal_toml().replace(
            "max_calls = 20",
            "max_calls = 20\nallowed_nodes = [\"55553\", \"77777\"]",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let allow = c
            .to_inbound()
            .unwrap()
            .allowlist
            .expect("allowlist present");
        assert!(allow.contains("55553"));
        assert!(allow.contains("77777"));
        assert!(!allow.contains("40000"), "uncited node not on the list");
    }

    #[test]
    fn unknown_answer_policy_is_error() {
        let toml = minimal_toml().replace("answer = \"auto\"", "answer = \"yolo\"");
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            err.contains("listener.answer"),
            "error should mention field: {err}"
        );
    }

    #[test]
    fn unknown_auth_policy_is_error() {
        let toml = minimal_toml().replace("auth = \"optional\"", "auth = \"maybe\"");
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            err.contains("listener.auth"),
            "error should mention field: {err}"
        );
    }

    #[test]
    fn to_register_absent_when_section_missing() {
        let c = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(c.to_register().unwrap().is_none());
    }

    #[test]
    fn to_register_present_when_section_given() {
        let toml = format!(
            "{}\n[register]\npeer = \"104.232.32.242:4569\"\nnode_id = \"77777\"\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let reg = c.to_register().unwrap().expect("register section present");
        assert_eq!(reg.username, "77777");
        let expected_peer: SocketAddr = "104.232.32.242:4569".parse().unwrap();
        assert_eq!(reg.peer, expected_peer);
        assert_eq!(reg.refresh, Duration::from_secs(60));
    }

    #[test]
    fn to_register_bad_peer_addr_is_error() {
        let toml = format!(
            "{}\n[register]\npeer = \"not-an-addr\"\nnode_id = \"77777\"\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let err = c.to_register().unwrap_err();
        assert!(
            err.contains("register.peer"),
            "error should mention field: {err}"
        );
    }

    #[test]
    fn audio_section_is_optional() {
        let toml = format!(
            "{}\n[audio]\ninput = \"USB\"\noutput = \"Built-in\"\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.audio.as_ref().unwrap().input.as_deref(), Some("USB"));
    }

    #[test]
    fn audio_backend_none_parses() {
        let toml = format!("{}\n[audio]\nbackend = \"none\"\n", minimal_toml());
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.audio.as_ref().unwrap().backend.as_deref(), Some("none"));
    }

    #[test]
    fn audio_backend_rejects_unknown_value() {
        let toml = format!("{}\n[audio]\nbackend = \"pulse\"\n", minimal_toml());
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            err.contains("audio.backend"),
            "error should name the bad key: {err}"
        );
    }

    #[test]
    fn bridge_absent_section_defaults_to_bridge_mode() {
        // The DAEMON default is mix-minus bridge even with no [bridge] section
        // (iax-647d) — distinct from the library handset default.
        let c = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert_eq!(b.mode, BridgeMode::Bridge);
        assert!(b.mix_minus, "mix_minus defaults true");
        assert!(!b.include_local_radio, "local radio off by default");
    }

    #[test]
    fn bridge_section_round_trips_all_fields() {
        let toml = format!(
            "{}\n[bridge]\nmode = \"conference\"\nmix_minus = false\ninclude_local_radio = true\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert_eq!(b.mode, BridgeMode::Conference);
        assert!(!b.mix_minus);
        assert!(b.include_local_radio);
    }

    #[test]
    fn bridge_mode_handset_parses() {
        let toml = format!("{}\n[bridge]\nmode = \"Handset\"\n", minimal_toml());
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.to_bridge_config().unwrap().mode, BridgeMode::Handset);
    }

    #[test]
    fn bridge_present_section_omitted_fields_default() {
        // A present [bridge] section with only `mode` keeps mix_minus=true and
        // include_local_radio=false.
        let toml = format!("{}\n[bridge]\nmode = \"bridge\"\n", minimal_toml());
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert!(b.mix_minus);
        assert!(!b.include_local_radio);
    }

    #[test]
    fn unknown_bridge_mode_is_error() {
        let toml = format!("{}\n[bridge]\nmode = \"mesh\"\n", minimal_toml());
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            err.contains("bridge.mode"),
            "error should mention field: {err}"
        );
    }

    // -- iax-feab: [bridge] mode = "parrot" + [parrot] tuning ---------------

    #[test]
    fn bridge_mode_parrot_parses_with_tuning() {
        let toml = format!(
            "{}\n[bridge]\nmode = \"parrot\"\n[parrot]\nplayback_delay_ms = 1000\nsilence_gap_ms = 400\nvox_threshold_db = -35.0\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert_eq!(b.mode, BridgeMode::Parrot);
        let tuning = b.parrot.expect("parrot mode must carry ParrotTuning");
        assert_eq!(
            tuning.playback_delay_ticks, 50,
            "1000 ms / 20 ms per tick = 50 ticks"
        );
        assert_eq!(
            tuning.silence_gap_ticks, 20,
            "400 ms / 20 ms per tick = 20 ticks"
        );
        assert!((tuning.vox_threshold_db - -35.0).abs() < 1e-6);
    }

    #[test]
    fn parrot_defaults_apply_when_section_absent() {
        // mode = "parrot" with no [parrot] section at all -> ParrotTuning::default().
        let toml = format!("{}\n[bridge]\nmode = \"parrot\"\n", minimal_toml());
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert_eq!(b.mode, BridgeMode::Parrot);
        let tuning = b.parrot.expect("parrot mode must carry ParrotTuning");
        assert_eq!(tuning.playback_delay_ticks, 150);
        assert_eq!(tuning.silence_gap_ticks, 40);
        assert!((tuning.vox_threshold_db - -40.0).abs() < 1e-6);
        assert_eq!(
            tuning.max_record_ticks, 500,
            "the 10 s cap is not TOML-exposed; stays the library default"
        );
    }

    #[test]
    fn parrot_tuning_absent_for_non_parrot_modes() {
        // A present [parrot] section is simply ignored when bridge.mode isn't
        // "parrot" — no error, and BridgeConfig.parrot stays None.
        let toml = format!(
            "{}\n[bridge]\nmode = \"bridge\"\n[parrot]\nplayback_delay_ms = 1000\n",
            minimal_toml()
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let b = c.to_bridge_config().unwrap();
        assert_eq!(b.mode, BridgeMode::Bridge);
        assert!(b.parrot.is_none());
    }

    #[test]
    fn parses_announce_section() {
        let toml = r#"
[listener]
bind = "127.0.0.1:4569"
answer = "auto"
max_calls = 5
auth = "off"

[control]
bind = "127.0.0.1:8730"

[secrets]
source = "env"

[announce]
enabled = true
sounds_dir = "/etc/iaxnode/sounds"
mixunder_default_gain_db = -10.0
id_interval_secs = 600
id_mode = "cw"
cw_wpm = 18

[announce.tts]
enabled = true
voice = "/etc/iaxnode/voices/en_US.onnx"

[announce.events]
peer_connected = { enabled = true, destination = "to_air" }
"#;
        let cfg = NodeFileConfig::from_toml_str(toml).expect("parse");
        let a = cfg.announce.expect("announce section");
        assert!(a.enabled);
        assert_eq!(a.id_mode.as_deref(), Some("cw"));
        let svc = a.to_service_config();
        assert!((svc.mixunder_default_gain_db - -10.0).abs() < 1e-6);
    }

    #[test]
    fn announce_join_template_absent_is_none() {
        // iax-c4ea: an [announce] section without join_template leaves it None
        // (the controller falls back to the default template).
        let toml = format!("{}\n[announce]\nenabled = true\n", minimal_toml());
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("parse");
        let a = cfg.announce.expect("announce section");
        assert!(a.join_template.is_none());
    }

    #[test]
    fn announce_join_template_round_trips() {
        // iax-c4ea: a custom join_template parses and round-trips verbatim.
        let toml = format!(
            "{}\n[announce]\nenabled = true\njoin_template = \"Connected to node {{server-node-number}}\"\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("parse");
        let a = cfg.announce.expect("announce section");
        assert_eq!(
            a.join_template.as_deref(),
            Some("Connected to node {server-node-number}")
        );
    }

    #[test]
    fn announce_m17_templates_absent_are_none() {
        // An [announce] section written before M17 existed leaves both M17
        // templates unset, so the controller falls back to its defaults.
        let toml = format!("{}\n[announce]\nenabled = true\n", minimal_toml());
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("parse");
        let a = cfg.announce.expect("announce section");
        assert_eq!(a.m17_connect_template, None);
        assert_eq!(a.m17_disconnect_template, None);
    }

    #[test]
    fn announce_m17_templates_round_trip() {
        // Both keys parse and keep the `{node}` token verbatim: substitution
        // happens at announce time, never at load.
        let toml = format!(
            "{}\n[announce]\nenabled = true\nm17_connect_template = \"Now hearing {{node}}\"\nm17_disconnect_template = \"Done with {{node}}\"\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("parse");
        let a = cfg.announce.expect("announce section");
        assert_eq!(
            a.m17_connect_template.as_deref(),
            Some("Now hearing {node}")
        );
        assert_eq!(
            a.m17_disconnect_template.as_deref(),
            Some("Done with {node}")
        );
    }

    #[test]
    fn parses_wireguard_section() {
        let toml = r#"
            [listener]
            bind = "0.0.0.0:4569"
            answer = "auto"
            max_calls = 2
            auth = "off"

            [control]
            bind = "127.0.0.1:8730"

            [secrets]
            source = "env"

            [wireguard]
            enabled = true
            address = "10.99.0.2/32"
            peer_public_key = "qZD0J8m7l3w0pYJ9k2bQ3cV5xWv8oQz1aB2cD3eF4g="
            endpoint = "vps.example.org:51820"
            allowed_ips = ["10.99.0.0/24"]
            keepalive_secs = 25
            secret_ref = "MY_WG_KEY"
            also_bind_udp = "0.0.0.0:4569"
        "#;
        let cfg = NodeFileConfig::from_toml_str(toml).expect("parse");
        let wg = cfg.wireguard.expect("wireguard section present");
        assert!(wg.enabled);
        assert_eq!(wg.address, "10.99.0.2/32");
        assert_eq!(wg.endpoint, "vps.example.org:51820");
        assert_eq!(wg.allowed_ips, vec!["10.99.0.0/24".to_string()]);
        assert_eq!(wg.keepalive_secs, Some(25));
        assert_eq!(wg.secret_ref.as_deref(), Some("MY_WG_KEY"));
        assert_eq!(wg.also_bind_udp.as_deref(), Some("0.0.0.0:4569"));
    }

    #[test]
    fn wireguard_section_is_optional() {
        let toml = r#"
            [listener]
            bind = "0.0.0.0:4569"
            answer = "auto"
            max_calls = 2
            auth = "off"
            [control]
            bind = "127.0.0.1:8730"
            [secrets]
            source = "env"
        "#;
        let cfg = NodeFileConfig::from_toml_str(toml).expect("parse");
        assert!(cfg.wireguard.is_none());
    }

    const WG_KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    /// A config whose `[wireguard]` section ends with `extra` lines (empty for
    /// the plain section). Endpoint is an IP literal so tests never hit DNS.
    fn wg_toml_with(extra: &str) -> String {
        format!(
            r#"
        [listener]
        bind = "0.0.0.0:4569"
        answer = "auto"
        max_calls = 2
        auth = "off"
        [control]
        bind = "127.0.0.1:8730"
        [secrets]
        source = "env"
        [wireguard]
        address = "10.99.0.2/32"
        peer_public_key = "{WG_KEY32}"
        endpoint = "127.0.0.1:51820"
        allowed_ips = ["10.99.0.0/24"]
        {extra}
    "#
        )
    }

    /// Unwrap the `Wireguard` arm of a built transport.
    fn expect_wg(t: astar_iax::LinkTransport) -> astar_wireguard::WgLinkConfig {
        match t {
            astar_iax::LinkTransport::Wireguard(cfg) => cfg,
            astar_iax::LinkTransport::Udp => panic!("expected LinkTransport::Wireguard, got Udp"),
        }
    }

    #[test]
    fn to_link_transport_absent_section_is_udp() {
        // No [wireguard] section at all -> plain UDP, byte-identical to today.
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(matches!(
            cfg.to_link_transport().unwrap(),
            astar_iax::LinkTransport::Udp
        ));
    }

    #[test]
    fn to_link_transport_disabled_section_is_udp() {
        // An explicit `enabled = false` opts back out to plain UDP.
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("enabled = false")).unwrap();
        assert!(matches!(
            cfg.to_link_transport().unwrap(),
            astar_iax::LinkTransport::Udp
        ));
    }

    #[test]
    fn to_link_transport_presence_selects_wireguard() {
        // Presence of the section (no `enabled` key) selects the WG transport
        // (iax-580b, per the link-transport design: presence = on).
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("")).unwrap();
        let wg = expect_wg(cfg.to_link_transport().unwrap());
        assert_eq!(wg.endpoint(), "127.0.0.1:51820".parse().unwrap());
    }

    #[test]
    fn to_link_transport_maps_fields() {
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("enabled = true")).unwrap();
        let wg = expect_wg(cfg.to_link_transport().unwrap());
        assert_eq!(wg.endpoint(), "127.0.0.1:51820".parse().unwrap());
        assert_eq!(
            wg.tunnel_ip(),
            "10.99.0.2".parse::<std::net::Ipv4Addr>().unwrap()
        );
        assert_eq!(wg.tunnel_prefix(), 32);
        assert_eq!(
            wg.allowed_ips(),
            &[("10.99.0.0".parse::<std::net::IpAddr>().unwrap(), 24)]
        );
        assert_eq!(wg.keepalive(), 25, "keepalive_secs default applied");
        // Secret-free (iax-8516): the config carries the env-var *reference*,
        // never key material — and the default ref keeps existing deployments
        // (WIREGUARD_PRIVATE_KEY) working unchanged.
        assert_eq!(wg.private_key_ref(), "WIREGUARD_PRIVATE_KEY");
        assert_eq!(wg.also_bind_udp(), None, "default is no extra UDP bind");
        assert!(!format!("{wg:?}").contains(WG_KEY32));
    }

    #[test]
    fn to_link_transport_secret_ref_override() {
        let cfg =
            NodeFileConfig::from_toml_str(&wg_toml_with("secret_ref = \"MY_WG_KEY\"")).unwrap();
        let wg = expect_wg(cfg.to_link_transport().unwrap());
        assert_eq!(wg.private_key_ref(), "MY_WG_KEY");
    }

    #[test]
    fn to_link_transport_also_bind_udp_parses() {
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("also_bind_udp = \"0.0.0.0:4569\""))
            .unwrap();
        let wg = expect_wg(cfg.to_link_transport().unwrap());
        assert_eq!(wg.also_bind_udp(), Some("0.0.0.0:4569".parse().unwrap()));
    }

    #[test]
    fn to_link_transport_bad_also_bind_udp_is_error() {
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("also_bind_udp = \"not-an-addr\""))
            .unwrap();
        let err = cfg.to_link_transport().unwrap_err();
        assert!(
            err.contains("wireguard.also_bind_udp"),
            "error should name the field: {err}"
        );
    }

    #[test]
    fn to_link_transport_bad_section_is_error() {
        let cfg =
            NodeFileConfig::from_toml_str(&wg_toml_with("").replace("10.99.0.2/32", "not-a-cidr"))
                .unwrap();
        let err = cfg.to_link_transport().unwrap_err();
        assert!(
            err.contains("wireguard"),
            "error should name section: {err}"
        );
    }

    #[test]
    fn wireguard_stack_build_errors_when_key_unresolved() {
        // The missing-key failure surfaces when the stack is built (the
        // resolver returns empty for an unset env var) and names the ref.
        let cfg = NodeFileConfig::from_toml_str(&wg_toml_with("enabled = true")).unwrap();
        let wg = expect_wg(cfg.to_link_transport().unwrap());
        let err = astar_wireguard::WgStack::new(
            &wg,
            &|_| String::new(),
            Box::new(astar_wireguard::FakeTransport::new()),
        )
        .expect_err("unresolved key must fail");
        assert!(
            err.to_string().contains("WIREGUARD_PRIVATE_KEY"),
            "got: {err}"
        );
    }

    #[test]
    fn wireguard_debug_never_prints_the_word_secret() {
        // `secret_ref` is a reference (an env-var NAME), but the guard rule is
        // that the word "secret" never reaches Debug output — the manual Debug
        // impl renders it under a neutral key.
        let cfg =
            NodeFileConfig::from_toml_str(&wg_toml_with("secret_ref = \"MY_WG_KEY\"")).unwrap();
        let d = format!("{cfg:?}");
        assert!(!d.contains("secret"), "Debug leaked the word: {d}");
        assert!(
            d.contains("MY_WG_KEY"),
            "the ref NAME is fine to print: {d}"
        );
    }

    // -- iax-4703: [secrets] source = "config" (inline secret) -------------

    #[test]
    fn secrets_source_config_with_secret_parses() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"hunter2xyz\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.secrets.source, "config");
        assert_eq!(c.secrets.secret.as_deref(), Some("hunter2xyz"));
    }

    #[test]
    fn secrets_source_config_empty_secret_is_valid_at_load() {
        // Older generated templates shipped `source = "config"` with
        // `secret = ""` and live nodes still carry it; this must boot (warn,
        // not error).
        let toml = minimal_toml().replace("source = \"env\"", "source = \"config\"\nsecret = \"\"");
        assert!(NodeFileConfig::from_toml_str(&toml).is_ok());
    }

    #[test]
    fn secrets_source_config_absent_secret_is_valid_at_load() {
        let toml = minimal_toml().replace("source = \"env\"", "source = \"config\"");
        assert!(NodeFileConfig::from_toml_str(&toml).is_ok());
    }

    #[test]
    fn secrets_source_config_case_insensitive() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"Config\"\nsecret = \"hunter2xyz\"",
        );
        assert!(NodeFileConfig::from_toml_str(&toml).is_ok());
    }

    #[test]
    fn an_empty_inline_secret_is_a_startup_warning_not_a_load_side_effect() {
        for toml in [
            minimal_toml().replace("source = \"env\"", "source = \"config\"\nsecret = \"\""),
            minimal_toml().replace("source = \"env\"", "source = \"config\""),
            minimal_toml().replace("source = \"env\"", "source = \"Config\""),
        ] {
            let cfg = NodeFileConfig::from_toml_str_redacted(&toml).expect("an empty secret loads");
            assert_eq!(
                cfg.startup_warning(),
                Some(CONFIG_SOURCE_WITHOUT_SECRET_WARNING),
                "{toml}"
            );
        }
        for toml in [
            minimal_toml().replace(
                "source = \"env\"",
                "source = \"config\"\nsecret = \"hunter2xyz\"",
            ),
            minimal_toml().replace("source = \"env\"", "source = \"file\""),
            minimal_toml().to_owned(),
        ] {
            let cfg = NodeFileConfig::from_toml_str_redacted(&toml).expect("loads");
            assert_eq!(cfg.startup_warning(), None, "{toml}");
        }
        assert_eq!(
            CONFIG_SOURCE_WITHOUT_SECRET_WARNING,
            "astar-server: warning: [secrets] source = \"config\" but no secret is set; registration will not authenticate until one is provided",
            "the startup sentence is unchanged"
        );
    }

    #[test]
    fn unknown_secrets_source_is_error() {
        let toml = minimal_toml().replace("source = \"env\"", "source = \"carrier-pigeon\"");
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert_eq!(
            err,
            "secrets.source: unknown value \"carrier-pigeon\" (expected \"env\", \"control\", \"config\", or \"file\")"
        );
    }

    #[test]
    fn secrets_debug_redacts_config_secret_value() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"hunter2xyz\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let d = format!("{c:?}");
        assert!(
            !d.contains("hunter2xyz"),
            "Debug output leaked the secret: {d}"
        );
    }

    // -- [secrets] source = "file": the credentials file --------------------

    #[test]
    fn secrets_source_file_parses_with_no_secret_slot() {
        let toml = minimal_toml().replace("source = \"env\"", "source = \"file\"");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.secrets.source, "file");
        assert_eq!(c.secrets.secret, None);
        assert_eq!(c.secrets.file, None);
    }

    #[test]
    fn secrets_file_path_is_kept_as_written_and_the_source_is_case_insensitive() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"File\"\nfile = \"creds/keys.toml\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(c.secrets.file.as_deref(), Some("creds/keys.toml"));
    }

    #[test]
    fn credentials_path_defaults_to_credentials_toml_beside_the_config() {
        let toml = minimal_toml().replace("source = \"env\"", "source = \"file\"");
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(
            c.credentials_path(std::path::Path::new("/etc/iaxnode/node.toml")),
            std::path::PathBuf::from("/etc/iaxnode/credentials.toml")
        );
        // A bare file name has an empty parent: the sibling is a bare name
        // too, not something rooted in the working directory by us.
        assert_eq!(
            c.credentials_path(std::path::Path::new("node.toml")),
            std::path::PathBuf::from("credentials.toml")
        );
    }

    #[test]
    fn credentials_path_resolves_a_relative_file_against_the_config_dir() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"file\"\nfile = \"creds/keys.toml\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(
            c.credentials_path(std::path::Path::new("/etc/iaxnode/node.toml")),
            std::path::PathBuf::from("/etc/iaxnode/creds/keys.toml")
        );
    }

    #[test]
    fn credentials_path_keeps_an_absolute_file() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"file\"\nfile = \"/run/astar/credentials.toml\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        assert_eq!(
            c.credentials_path(std::path::Path::new("/etc/iaxnode/node.toml")),
            std::path::PathBuf::from("/run/astar/credentials.toml")
        );
    }

    #[test]
    fn debug_names_the_credential_file_but_never_a_secret() {
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"file\"\nfile = \"creds/keys.toml\"",
        );
        let c = NodeFileConfig::from_toml_str(&toml).unwrap();
        let d = format!("{c:?}");
        assert!(
            d.contains("credential_file: Some(\"creds/keys.toml\")"),
            "{d}"
        );
        assert!(!d.contains("secret"), "{d}");
        assert!(!d.contains("password"), "{d}");
    }

    // -- load_credentials / portal_credentials: the startup path ------------

    /// `minimal_toml()` on `source`, plus a `[portal]` whose password lives
    /// in the variable `var`.
    fn with_portal(source: &str, var: &str) -> NodeFileConfig {
        let toml = format!(
            "{}\n[portal]\nuser = \"AJ7HR\"\nnode = \"77777\"\ncredential_env = \"{var}\"\n",
            minimal_toml().replace("source = \"env\"", &format!("source = \"{source}\""))
        );
        NodeFileConfig::from_toml_str(&toml).expect("valid config")
    }

    #[test]
    fn under_the_file_source_the_portal_password_never_comes_from_the_env_var() {
        let dir = tempfile::tempdir().expect("tempdir");
        // An empty credentials file only its owner can read, so the load
        // succeeds and simply holds no `portal` key.
        let creds = dir.path().join(crate::secrets::CREDENTIALS_FILE_NAME);
        std::fs::write(&creds, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let cfg = with_portal("file", "ASTAR_TEST_CFG_PORTAL_FILE");
        let secrets = crate::secrets::SecretProvider::new();

        // SAFETY: the secrets tests already set and remove process env vars
        // the same way; this name is unique to this test.
        unsafe { std::env::set_var("ASTAR_TEST_CFG_PORTAL_FILE", "SENTINEL-env-portal") };
        let loaded = cfg.load_credentials(&dir.path().join("node.toml"), &secrets);
        let portal = cfg.portal_credentials(&secrets);
        unsafe { std::env::remove_var("ASTAR_TEST_CFG_PORTAL_FILE") };

        assert_eq!(loaded, Some(Ok((creds, 0))));
        assert_eq!(
            portal
                .err()
                .expect("the file holds no portal key, so there is no password"),
            "[portal] configured but credentials.toml key \"portal\" is unset — wt-guest links will dial without a WT token"
        );
        assert_eq!(secrets.resolve(crate::secrets::PORTAL_CREDENTIAL_KEY), "");
    }

    #[test]
    fn under_the_env_source_the_portal_password_comes_from_the_named_var() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = with_portal("env", "ASTAR_TEST_CFG_PORTAL_ENV");
        let secrets = crate::secrets::SecretProvider::new();

        // SAFETY: as above; this name is unique to this test.
        unsafe { std::env::set_var("ASTAR_TEST_CFG_PORTAL_ENV", "portal-pw-from-env") };
        let loaded = cfg.load_credentials(&dir.path().join("node.toml"), &secrets);
        let portal = cfg.portal_credentials(&secrets);
        unsafe { std::env::remove_var("ASTAR_TEST_CFG_PORTAL_ENV") };

        assert_eq!(loaded, None, "only the file source has a line to report");
        let p = portal
            .ok()
            .flatten()
            .expect("the named variable holds the password");
        assert_eq!(p.password, "portal-pw-from-env");
        assert_eq!(p.user, "AJ7HR");
        assert_eq!(p.node, "77777");
        // Seeded into the store, where export_file and the page's chip look.
        assert_eq!(
            secrets.resolve(crate::secrets::PORTAL_CREDENTIAL_KEY),
            "portal-pw-from-env"
        );
    }

    #[test]
    fn under_the_file_source_node_is_loaded_under_the_register_node_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join(crate::secrets::CREDENTIALS_FILE_NAME);
        crate::secrets::SecretProvider::save_file(
            &creds,
            crate::secrets::NODE_CREDENTIAL_KEY,
            "reg-pw-54321",
        )
        .unwrap();
        let toml = format!(
            "{}\n[register]\npeer = \"104.232.32.242:4569\"\nnode_id = \"54321\"\n",
            minimal_toml().replace("source = \"env\"", "source = \"file\"")
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("valid config");
        let secrets = crate::secrets::SecretProvider::new();

        assert_eq!(
            cfg.load_credentials(&dir.path().join("node.toml"), &secrets),
            Some(Ok((creds, 1)))
        );
        assert_eq!(secrets.resolve("54321"), "reg-pw-54321");
        assert_eq!(secrets.resolve("node"), "");
    }

    // -- iax-31f7: codec_policy --------------------------------------------

    #[test]
    fn codec_policy_parses_and_reaches_inbound() {
        let cfg = NodeFileConfig::from_toml_str(&format!(
            "codec_policy = \"prefer_slin\"\n{}",
            minimal_toml()
        ))
        .unwrap();
        assert_eq!(cfg.codec_policy, CodecPolicy::PreferSlin);
        assert_eq!(
            cfg.to_inbound().unwrap().policy.codec_policy,
            CodecPolicy::PreferSlin
        );
    }

    #[test]
    fn codec_policy_parses_prefer_slin16_and_reaches_inbound() {
        // iax-4348: prefer_slin16 must parse and flow to the inbound policy the
        // same way prefer_slin does; the Station's audio-pipeline rate switch
        // itself is exercised at the Manager level, not here.
        let cfg = NodeFileConfig::from_toml_str(&format!(
            "codec_policy = \"prefer_slin16\"\n{}",
            minimal_toml()
        ))
        .unwrap();
        assert_eq!(cfg.codec_policy, CodecPolicy::PreferSlin16);
        assert_eq!(
            cfg.to_inbound().unwrap().policy.codec_policy,
            CodecPolicy::PreferSlin16
        );
    }

    #[test]
    fn codec_policy_defaults_to_prefer_slin16() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert_eq!(
            cfg.codec_policy,
            CodecPolicy::PreferSlin16,
            "an absent codec_policy key must default the SERVER to wideband, \
             not to the engine-wide UlawOnly default"
        );
        assert_eq!(
            cfg.to_inbound().unwrap().policy.codec_policy,
            CodecPolicy::PreferSlin16,
            "and the default must reach the inbound listener, not just the struct"
        );
    }

    #[test]
    fn codec_policy_ulaw_only_is_still_pinnable() {
        let cfg = NodeFileConfig::from_toml_str(&format!(
            "codec_policy = \"ulaw_only\"\n{}",
            minimal_toml()
        ))
        .unwrap();
        assert_eq!(cfg.codec_policy, CodecPolicy::UlawOnly);
        assert_eq!(
            cfg.to_inbound().unwrap().policy.codec_policy,
            CodecPolicy::UlawOnly
        );
    }

    #[test]
    fn unknown_codec_policy_is_an_error() {
        assert!(
            NodeFileConfig::from_toml_str(&format!("codec_policy = \"opus\"\n{}", minimal_toml()))
                .is_err()
        );
    }

    // -- settings oracle: the redacted loader ---------------------------------

    #[test]
    fn a_syntax_error_on_the_secret_line_is_quoted_by_the_plain_loader_and_not_by_the_redacted_one()
    {
        // The VPS carries its registration password inline. A typo on that
        // line makes toml's Display print the line — password included —
        // which is exactly what a settings page must never forward.
        let toml = minimal_toml().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"SENTINEL-9f3a",
        );
        let plain = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            plain.contains("SENTINEL-9f3a"),
            "the hazard this test guards against must be real: {plain}"
        );
        let redacted = NodeFileConfig::from_toml_str_redacted(&toml).unwrap_err();
        assert!(
            !redacted.contains("SENTINEL-9f3a"),
            "the redacted loader forwarded the secret line: {redacted}"
        );
        // The whole message is knowable: toml_parser's `decode_basic_string`
        // reports "invalid basic string" expecting a closing `"`, and toml
        // renders that expectation with `render_literal`.
        assert_eq!(
            redacted, "invalid basic string, expected `\"`",
            "the redacted error still says what went wrong, and only that"
        );
    }

    #[test]
    fn a_non_string_secret_is_named_but_never_quoted() {
        // AllStar node passwords are often all digits, and a hand edit that
        // drops the quotes makes serde's type error quote the value itself:
        // "invalid type: integer `12345678`, expected a string".
        let toml =
            minimal_toml().replace("source = \"env\"", "source = \"config\"\nsecret = 12345678");
        let plain = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            plain.contains("12345678"),
            "the hazard this test guards against must be real: {plain}"
        );
        let redacted = NodeFileConfig::from_toml_str_redacted(&toml).unwrap_err();
        assert_eq!(redacted, "secrets.secret: expected a string");
    }

    #[test]
    fn a_non_table_secrets_is_named_but_never_quoted() {
        // `secrets` written as a bare value instead of a table: serde's type
        // error would quote a string or an integer, and an array is read as
        // a sequence into SecretsCfg, so its first element becomes `source`
        // and comes back through that validator's message.
        let without_table = minimal_toml().replace("[secrets]\n        source = \"env\"", "");
        assert!(
            !without_table.contains("[secrets]"),
            "fixture: {without_table}"
        );
        for value in [
            "\"SENTINEL-7c1e\"",
            "7654321",
            "[\"SENTINEL-7c1e\"]",
            "[\"SENTINEL-7c1e\", \"SENTINEL-7c1e\"]",
        ] {
            let toml = format!("secrets = {value}\n{without_table}");
            let plain = NodeFileConfig::from_toml_str(&toml).unwrap_err();
            let sentinel = if value.contains("SENTINEL") {
                "sentinel-7c1e"
            } else {
                "7654321"
            };
            assert!(
                plain.to_lowercase().contains(sentinel),
                "the hazard this test guards against must be real for {value}: {plain}"
            );
            let redacted = NodeFileConfig::from_toml_str_redacted(&toml).unwrap_err();
            assert_eq!(redacted, "secrets: expected a table", "for {value}");
            assert!(
                !redacted.to_lowercase().contains(sentinel),
                "{value} leaked: {redacted}"
            );
        }
        // An inline table is still a table: it reaches the `secret` check.
        let inline =
            format!("secrets = {{ source = \"config\", secret = 7654321 }}\n{without_table}");
        assert_eq!(
            NodeFileConfig::from_toml_str_redacted(&inline).unwrap_err(),
            "secrets.secret: expected a string"
        );
    }

    #[test]
    fn the_redacted_loader_runs_the_same_validators() {
        let toml = format!("{}\n[bridge]\nmode = \"mesh\"\n", minimal_toml());
        assert_eq!(
            NodeFileConfig::from_toml_str_redacted(&toml).unwrap_err(),
            "bridge.mode: unknown value \"mesh\" (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
        );
        assert_eq!(
            NodeFileConfig::from_toml_str(&toml).unwrap_err(),
            NodeFileConfig::from_toml_str_redacted(&toml).unwrap_err(),
            "both loaders share one validator body"
        );
    }

    #[test]
    fn codec_policy_raw_is_the_string_as_written() {
        let cfg = NodeFileConfig::from_toml_str(&format!(
            "codec_policy = \"prefer_slin\"\n{}",
            minimal_toml()
        ))
        .unwrap();
        assert_eq!(cfg.codec_policy_raw(), Some("prefer_slin"));
        let absent = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert_eq!(
            absent.codec_policy_raw(),
            None,
            "an absent key is None, not the name of the default"
        );
    }

    // -- iax-b7f2: [portal] WT-token mint account ---------------------------

    #[test]
    fn portal_section_parses_with_default_credential_env() {
        let toml = format!(
            "{}\n[portal]\nuser = \"AJ7HR\"\nnode = \"77777\"\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("valid config");
        let portal = cfg.portal.expect("portal section present");
        assert_eq!(portal.user, "AJ7HR");
        assert_eq!(portal.node, "77777");
        assert_eq!(portal.credential_env, "ALLSTAR_PORTAL_PASS");
    }

    #[test]
    fn portal_absent_is_none() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).expect("valid config");
        assert!(cfg.portal.is_none());
    }

    // -- iax-5029: [links."<node>"] shape table -----------------------------

    #[test]
    fn links_table_parses_shapes() {
        let toml = format!(
            "{}\n[links.\"55553\"]\nshape = \"wt-guest\"\n[links.\"1999\"]\nshape = \"standard\"\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("valid config");
        assert_eq!(cfg.link_shape("55553"), LinkShape::WtGuest);
        assert_eq!(cfg.link_shape("1999"), LinkShape::Standard);
        assert_eq!(
            cfg.link_shape("77777"),
            LinkShape::Standard,
            "absent = standard"
        );
    }

    #[test]
    fn links_table_unknown_shape_errors_at_load() {
        let toml = format!(
            "{}\n[links.\"55553\"]\nshape = \"webtransceiver\"\n",
            minimal_toml()
        );
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(err.contains("shape"), "error names the bad field: {err}");
    }

    // -- iax-d254: [dtmf] section -------------------------------------------

    #[test]
    fn dtmf_defaults_off_with_3s_timeout() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).expect("valid config");
        assert!(!cfg.dtmf_enabled(), "DTMF commands are opt-in");
        assert_eq!(cfg.dtmf_inter_digit_timeout_ms(), 3000);
    }

    #[test]
    fn dtmf_section_parses() {
        let toml = format!(
            "{}\n[dtmf]\nenabled = true\ninter_digit_timeout_ms = 5000\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("valid config");
        assert!(cfg.dtmf_enabled());
        assert_eq!(cfg.dtmf_inter_digit_timeout_ms(), 5000);
    }

    // -- [discord] section ---------------------------------------------------

    // The brief's `[listener]`/`[control]`/`[secrets]` fixture only sets
    // `bind`, but `ListenerCfg::answer/max_calls/auth` and the top-level
    // `control`/`secrets` fields are mandatory (no `#[serde(default)]`) —
    // unrelated to the `[discord]` feature under test. `minimal_toml()`
    // already supplies the minimum required elsewhere (its `bind` and
    // `max_calls` values are irrelevant to pure string parsing), so reuse it
    // instead of keeping a second near-identical fixture in sync.

    #[test]
    fn discord_section_parses_with_defaults() {
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).unwrap();
        let d = cfg.discord.expect("section present");
        assert_eq!(d.guild_id, "111");
        assert_eq!(d.channel_id, "222");
        assert!(!d.join_at_start, "the node comes up quiet by default");
        assert!(d.announce_on_join, "the join notice is on by default");
        assert!(d.model.is_none());
    }

    #[test]
    fn absent_discord_section_is_inert() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(cfg.discord.is_none());
        assert!(
            !cfg.discord_join_at_start(),
            "no section means no start-up join"
        );
    }

    #[test]
    fn join_at_start_reaches_the_gate_main_reads() {
        // `join_at_start` was parsed and then consumed by nothing at all, so
        // the documented `true` did nothing. This pins the accessor
        // `main.rs` branches on, in both directions.
        let on = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\njoin_at_start = true\n",
            minimal_toml()
        );
        assert!(
            NodeFileConfig::from_toml_str(&on)
                .unwrap()
                .discord_join_at_start()
        );

        let off = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n",
            minimal_toml()
        );
        assert!(
            !NodeFileConfig::from_toml_str(&off)
                .unwrap()
                .discord_join_at_start(),
            "an omitted key must stay false, not inherit the last config read"
        );
    }

    #[test]
    fn discord_slash_command_keys_parse() {
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n\
             approvers = [\"333\", \"444\"]\n\
             state_file = \"/var/lib/astar/discord-approvals.json\"\n\
             mic_timeout_secs = 90\nmic_silence_secs = 3\n",
            minimal_toml()
        );
        let cfg = NodeFileConfig::from_toml_str(&toml).expect("all four keys parse");
        let d = cfg.discord.expect("section present");
        assert_eq!(d.approvers, vec!["333".to_string(), "444".to_string()]);
        assert_eq!(d.approver_ids().unwrap(), vec![333, 444]);
        assert_eq!(
            d.state_file.as_deref(),
            Some("/var/lib/astar/discord-approvals.json")
        );
        assert_eq!(d.mic_timeout(), 90);
        assert_eq!(d.mic_silence(), 3);
    }

    #[test]
    fn a_discord_section_without_the_new_keys_still_parses_with_defaults() {
        // The additive promise: a config written before slash commands
        // existed keeps parsing under `deny_unknown_fields`, and lands on
        // the documented defaults rather than on zero.
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n",
            minimal_toml()
        );
        let d = NodeFileConfig::from_toml_str(&toml)
            .expect("an old [discord] section still parses")
            .discord
            .expect("section present");
        assert!(
            d.approvers.is_empty(),
            "no approvers means no slash commands"
        );
        assert_eq!(d.approver_ids().unwrap(), Vec::<u64>::new());
        assert!(d.state_file.is_none());
        assert_eq!(d.mic_timeout(), 180, "absent mic_timeout_secs means 180");
        assert_eq!(d.mic_silence(), 5, "absent mic_silence_secs means 5");
    }

    #[test]
    fn a_non_snowflake_approver_is_a_startup_error_naming_the_entry() {
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\napprovers = [\"x\"]\n",
            minimal_toml()
        );
        let err = NodeFileConfig::from_toml_str(&toml)
            .expect_err("an approver that is not a snowflake must refuse the config");
        assert!(
            err.contains("discord.approvers"),
            "the error names the key: {err}"
        );
        assert!(
            err.contains('x'),
            "the error names the OFFENDING entry, not just the key: {err}"
        );
    }

    #[test]
    fn a_zero_approver_is_refused() {
        // `0` parses as a u64 and is never a Discord user id. Accepted, it
        // would sit in the list matching nobody while looking like it
        // matched someone.
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\napprovers = [\"0\"]\n",
            minimal_toml()
        );
        let err = NodeFileConfig::from_toml_str(&toml).expect_err("0 is not a user id");
        assert!(err.contains("discord.approvers"), "{err}");
        assert!(err.contains('0'), "the error names the entry: {err}");
    }

    #[test]
    fn duplicate_approvers_are_tolerated_and_deduped() {
        let toml = format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n\
             approvers = [\"333\", \"444\", \"333\"]\n",
            minimal_toml()
        );
        let d = NodeFileConfig::from_toml_str(&toml)
            .expect("a duplicate is not worth refusing to boot over")
            .discord
            .expect("section present");
        assert_eq!(
            d.approver_ids().unwrap(),
            vec![333, 444],
            "deduped, and in the order written"
        );
    }

    #[test]
    fn discord_default_matches_the_serde_defaults() {
        // `..DiscordCfg::default()` is used in controller tests; it must not
        // quietly disagree with what the same section parsed from TOML says.
        // `announce_on_join` is the one that matters: a derived Default
        // would make it `false` and silently turn the join notice off.
        let parsed = NodeFileConfig::from_toml_str(&format!(
            "{}\n[discord]\nguild_id = \"111\"\nchannel_id = \"222\"\n",
            minimal_toml()
        ))
        .unwrap()
        .discord
        .unwrap();
        let d = DiscordCfg::default();
        assert_eq!(d.announce_on_join, parsed.announce_on_join);
        assert_eq!(d.join_at_start, parsed.join_at_start);
        assert_eq!(d.approvers, parsed.approvers);
        assert_eq!(d.mic_timeout(), parsed.mic_timeout());
        assert_eq!(d.mic_silence(), parsed.mic_silence());
    }

    #[test]
    fn discord_section_has_no_transmit_key() {
        // The safety property at the config layer: anything that looks like an
        // arming knob must be rejected as an unknown field, not silently
        // ignored. Assert on the message, not just `is_err()` — otherwise
        // this test can't tell "rejected by deny_unknown_fields" apart from
        // "rejected for some unrelated parse reason", and would keep passing
        // even if the fixture broke for a different reason.
        for key in [
            "arm = true",
            "transmit = true",
            "contributes = true",
            "key = true",
        ] {
            let toml = format!(
                "{}\n[discord]\nguild_id = \"1\"\nchannel_id = \"2\"\n{key}\n",
                minimal_toml()
            );
            let err = NodeFileConfig::from_toml_str(&toml)
                .expect_err(&format!("{key} must be refused, not ignored"));
            assert!(
                err.contains("unknown field"),
                "{key} must be refused BY deny_unknown_fields, not by an \
                 unrelated parse error — got: {err}"
            );
        }
    }

    // ---- state_file_parent_problem ------------------------------------------
    //
    // Moved here from the binary so the settings oracle can call it with or
    // without the discord feature. The behaviour is unchanged; the two
    // sentences are pinned exactly because the settings page shows them.

    #[test]
    fn an_existing_directory_is_no_problem() {
        let dir = std::env::temp_dir();
        assert!(dir.is_dir(), "the temp dir exists on any test host");
        let path = dir.join("astar-discord-approvals.json");
        assert_eq!(
            state_file_parent_problem(&path),
            None,
            "a file that does not exist yet in a directory that does is a first run"
        );
    }

    #[test]
    fn a_missing_directory_names_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("no-such-dir");
        assert!(!dir.exists(), "the test needs a path that is really absent");
        let problem = state_file_parent_problem(&dir.join("approvals.json"))
            .expect("a state_file under a missing directory must be refused");
        assert_eq!(
            problem,
            format!("the directory {} does not exist", dir.display()),
            "the refusal names the directory, in this exact sentence"
        );
    }

    #[test]
    fn a_file_in_the_way_of_the_directory_is_a_problem() {
        // The parent exists but is not a directory — a different mistake
        // with a different sentence, and one `is_dir()` alone would call
        // "missing". The blocking file lives in a tempdir of its own, so
        // its name is unique and it goes away with the guard.
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("parent-is-a-file");
        std::fs::write(&file, b"not a directory").expect("the tempdir is writable");
        let problem = state_file_parent_problem(&file.join("approvals.json"))
            .expect("a state_file under a regular file must be refused");
        assert_eq!(
            problem,
            format!("{} is not a directory", file.display()),
            "the refusal names the file that is in the way, in this exact sentence"
        );
    }

    #[test]
    fn a_bare_filename_is_no_problem() {
        assert_eq!(
            state_file_parent_problem(std::path::Path::new("approvals.json")),
            None,
            "an empty parent is the working directory"
        );
    }

    // ---- [m17] ------------------------------------------------------------

    fn m17_toml(section: &str) -> String {
        format!("{}\n{section}", minimal_toml())
    }

    #[test]
    fn m17_section_parses_in_full() {
        let cfg = NodeFileConfig::from_toml_str(&m17_toml(
            "[m17]\nenabled = true\ndirectory = \"bundled\"\ncodec_dirs = [\"/opt/astar/lib\"]\n\
             [m17.reflectors]\n\"M17-XYZ\" = \"m17.example.net:17001\"\n\"M17-PRV\" = \"10.0.0.5\"\n",
        ))
        .expect("the documented section parses");
        let m = cfg.m17.expect("section present");
        assert!(m.enabled);
        assert_eq!(m.directory.as_deref(), Some("bundled"));
        assert_eq!(m.codec_dirs, vec!["/opt/astar/lib".to_string()]);
        assert_eq!(
            m.reflectors,
            std::collections::BTreeMap::from([
                ("M17-PRV".to_string(), "10.0.0.5".to_string()),
                ("M17-XYZ".to_string(), "m17.example.net:17001".to_string()),
            ])
        );
    }

    #[test]
    fn m17_is_off_when_the_section_is_absent() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(cfg.m17.is_none(), "no [m17] section means M17 is off");
    }

    #[test]
    fn m17_bare_section_defaults_to_enabled_and_empty() {
        let m = NodeFileConfig::from_toml_str(&m17_toml("[m17]\n"))
            .unwrap()
            .m17
            .expect("section present");
        assert!(m.enabled, "a present section defaults to enabled");
        assert_eq!(m.directory, None);
        assert_eq!(m.codec_dirs, [] as [std::string::String; 0]);
        assert!(m.reflectors.is_empty());
        let off = NodeFileConfig::from_toml_str(&m17_toml("[m17]\nenabled = false\n"))
            .unwrap()
            .m17
            .expect("section present");
        assert!(!off.enabled);
    }

    #[test]
    fn m17_section_refuses_forbidden_and_unknown_keys() {
        // Receive-only at the config layer: nothing that reads like an
        // identity, a transmit knob or a credential may be silently ignored.
        // The whole "expected one of" list is pinned, so it also proves the
        // vocabulary is exactly these four keys.
        for key in [
            "callsign",
            "transmit",
            "ptt",
            "contributes",
            "arm",
            "secret",
            "token",
            "password",
            "reflector_list",
        ] {
            let err = NodeFileConfig::from_toml_str(&m17_toml(&format!("[m17]\n{key} = \"x\"\n")))
                .expect_err(&format!("{key} must be refused, not ignored"));
            assert!(
                err.contains(&format!(
                    "unknown field `{key}`, expected one of `enabled`, `directory`, `codec_dirs`, `reflectors`"
                )),
                "{key} must be refused BY deny_unknown_fields — got: {err}"
            );
        }
    }

    #[test]
    fn m17_bad_values_fail_load_with_exact_messages() {
        for (section, want) in [
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"m17.example.net:0\"\n",
                "m17.reflectors.\"M17-XYZ\": bad port \"0\"",
            ),
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"m17.example.net:x\"\n",
                "m17.reflectors.\"M17-XYZ\": bad port \"x\"",
            ),
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"2001:db8::1\"\n",
                "m17.reflectors.\"M17-XYZ\": IPv6 literals are not supported",
            ),
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"m17 example.net\"\n",
                "m17.reflectors.\"M17-XYZ\": host contains whitespace",
            ),
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"\"\n",
                "m17.reflectors.\"M17-XYZ\": host is empty",
            ),
            (
                "[m17.reflectors]\n\"M17 XYZ\" = \"m17.example.net\"\n",
                "m17.reflectors.\"M17 XYZ\": A reflector name is letters, digits and '-' only.",
            ),
            (
                "[m17.reflectors]\n\"M1\" = \"m17.example.net\"\n",
                "m17.reflectors.\"M1\": A reflector name is 3 to 8 characters.",
            ),
            (
                "[m17.reflectors]\n\"M17-XYZ\" = \"a.example.net\"\n\"m17-xyz\" = \"b.example.net\"\n",
                "m17.reflectors: \"M17-XYZ\" and \"m17-xyz\" name the same reflector",
            ),
            (
                "[m17]\ndirectory = \"reflectors.json\"\n",
                "m17.directory: \"reflectors.json\" is neither \"bundled\" nor an absolute path",
            ),
            (
                "[m17]\ndirectory = \"Bundled\"\n",
                "m17.directory: \"Bundled\" is neither \"bundled\" nor an absolute path",
            ),
            (
                "[m17]\ncodec_dirs = [\"lib\"]\n",
                "m17.codec_dirs: \"lib\" is not an absolute path",
            ),
        ] {
            assert_eq!(
                NodeFileConfig::from_toml_str(&m17_toml(section)).map(|_| ()),
                Err(want.to_string()),
                "{section}"
            );
        }
    }

    #[test]
    fn m17_debug_shows_the_section_and_no_secret_words() {
        let cfg =
            NodeFileConfig::from_toml_str(&m17_toml("[m17]\ndirectory = \"bundled\"\n")).unwrap();
        assert_eq!(
            format!("{:?}", cfg.m17),
            "Some(M17Cfg { enabled: true, directory: Some(\"bundled\"), codec_dirs: [], reflectors: {} })"
        );
        let d = format!("{cfg:?}");
        // Only proves the hand-written NodeFileConfig Debug wires the field in;
        // the section's own text is pinned exactly above.
        assert!(d.contains("m17: Some(M17Cfg {"), "{d}");
        for bad in ["secret", "password", "token"] {
            assert!(!d.contains(bad), "{bad} in {d}");
        }
    }

    #[test]
    fn m17_example_block_parses() {
        // deploy/node.toml.example documents [m17] as a commented block. Read
        // the real file, uncomment only its key lines, and load them through
        // the real loader, so the example cannot drift into something
        // from_toml_str refuses.
        const EXAMPLE: &str = include_str!("../../../deploy/node.toml.example");
        let lines: Vec<&str> = EXAMPLE.lines().collect();
        let start = lines
            .iter()
            .position(|l| *l == "# [m17]")
            .expect("the example has a commented `# [m17]` line");
        let end = start
            + lines[start..]
                .iter()
                .position(|l| l.starts_with("# \"M17-XYZ\" = "))
                .expect("the [m17] block ends with the M17-XYZ row");
        let block = lines[start..=end]
            .iter()
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| {
                ["[m17", "enabled", "directory", "codec_dirs", "\"M17-XYZ\""]
                    .iter()
                    .any(|p| l.starts_with(*p))
            })
            .map(|l| l.split_once(" #").map_or(l, |(v, _)| v).trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            block,
            "[m17]\nenabled = true\ndirectory = \"bundled\"\ncodec_dirs = [\"/opt/astar/lib\"]\n\
             [m17.reflectors]\n\"M17-XYZ\" = \"m17.example.net:17000\"",
            "the extraction found exactly the documented key lines"
        );
        let m = NodeFileConfig::from_toml_str(&m17_toml(&block))
            .unwrap_or_else(|e| panic!("the example [m17] block must load: {e}\n{block}"))
            .m17
            .expect("section present");
        assert!(m.enabled, "the example leaves enabled = true");
        assert_eq!(m.directory.as_deref(), Some("bundled"));
        assert_eq!(m.codec_dirs, vec!["/opt/astar/lib".to_string()]);
        assert_eq!(
            m.reflectors,
            std::collections::BTreeMap::from([(
                "M17-XYZ".to_string(),
                "m17.example.net:17000".to_string()
            )])
        );
    }

    // ---- [vocoder] -----------------------------------------------------------

    fn vocoder_toml(block: &str) -> String {
        format!("{}\n{block}\n", minimal_toml())
    }

    #[test]
    fn dstar_vocoder_defaults_to_the_dongle() {
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml("")).unwrap();
        assert_eq!(cfg.dstar_vocoder(), Ok(astar_station::DstarVocoder::Dongle));
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml("[vocoder]\ntrusted_serials = []\n"))
            .unwrap();
        assert_eq!(cfg.dstar_vocoder(), Ok(astar_station::DstarVocoder::Dongle));
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml("[vocoder]\ndstar = \"dongle\"\n"))
            .unwrap();
        assert_eq!(cfg.dstar_vocoder(), Ok(astar_station::DstarVocoder::Dongle));
    }

    #[test]
    fn dstar_vocoder_software_depends_on_the_build() {
        use astar_station::DstarVocoder;
        assert_eq!(
            check_dstar_vocoder(Some("software"), true),
            Ok(DstarVocoder::Software)
        );
        let err = check_dstar_vocoder(Some("software"), false).unwrap_err();
        assert!(err.contains("dstar-soft-ambe"), "{err}");
        for bad in ["Software", "hardware", "auto", ""] {
            assert!(check_dstar_vocoder(Some(bad), true).is_err(), "{bad:?}");
        }
        // And the loader agrees with this build.
        let loaded =
            NodeFileConfig::from_toml_str(&vocoder_toml("[vocoder]\ndstar = \"software\"\n"));
        assert_eq!(
            loaded.is_ok(),
            astar_station::software_dstar_vocoder_compiled()
        );
    }

    /// The software vocoder is D-Star only: no other mode has a key, so
    /// naming one is a start-up error rather than an ignored line.
    #[test]
    fn no_vocoder_choice_exists_for_ambe_plus_2_modes() {
        for key in ["ysf", "nxdn", "dmr", "fusion", "software"] {
            let toml = vocoder_toml(&format!("[vocoder]\n{key} = \"software\"\n"));
            assert!(
                NodeFileConfig::from_toml_str(&toml).is_err(),
                "[vocoder] {key} must not be accepted"
            );
        }
    }

    #[test]
    fn absent_vocoder_section_trusts_nothing() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(cfg.vocoder.is_none());
        assert!(cfg.vocoder_trust().is_empty());
    }

    #[test]
    fn vocoder_section_parses_into_the_trust_list() {
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml(
            "[vocoder]\ntrusted_serials = [\"TESTSER1\", \"TESTSER2\", \"TESTSER1\"]",
        ))
        .unwrap();
        assert_eq!(
            cfg.vocoder.as_ref().unwrap().trusted_serials,
            vec!["TESTSER1", "TESTSER2", "TESTSER1"]
        );
        let trust = cfg.vocoder_trust();
        assert_eq!(trust.serials, vec!["TESTSER1", "TESTSER2"], "de-duplicated");
        assert!(trust.contains("TESTSER2"));
        assert!(!trust.contains("CATCABLE"));
    }

    #[test]
    fn an_empty_vocoder_section_trusts_nothing() {
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml("[vocoder]")).unwrap();
        assert_eq!(
            cfg.vocoder.as_ref().unwrap().trusted_serials,
            [] as [std::string::String; 0]
        );
        assert!(cfg.vocoder_trust().is_empty());
    }

    #[test]
    fn a_bad_vocoder_serial_is_a_startup_error_naming_it() {
        for bad in [
            "",
            " TESTSER1",
            "TESTSER1 ",
            "TEST SER",
            "A,B",
            "TEST\tSER",
            "TÉSTSER",
        ] {
            let toml = vocoder_toml(&format!("[vocoder]\ntrusted_serials = [{bad:?}]"));
            let err = NodeFileConfig::from_toml_str(&toml)
                .expect_err(&format!("{bad:?} must be refused"));
            assert!(err.starts_with("vocoder.trusted_serials: "), "{err}");
        }
        let long = "A".repeat(65);
        let err = NodeFileConfig::from_toml_str(&vocoder_toml(&format!(
            "[vocoder]\ntrusted_serials = [\"{long}\"]"
        )))
        .expect_err("an over-long serial is refused");
        assert!(err.contains("64"), "{err}");
    }

    #[test]
    fn the_vocoder_section_has_no_widening_key() {
        // No `port`, `scan`, `any` or `all`: nothing may widen the trust list
        // beyond the serials written down, and `deny_unknown_fields` makes a
        // stray key a startup error rather than a line that reads as if it did
        // something.
        for key in [
            "port = \"/dev/ttyUSB0\"",
            "scan = true",
            "any = true",
            "all = true",
        ] {
            let toml = vocoder_toml(&format!("[vocoder]\n{key}"));
            assert!(
                NodeFileConfig::from_toml_str(&toml).is_err(),
                "{key} must be refused"
            );
        }
    }

    #[test]
    fn the_redacted_loader_checks_vocoder_serials_too() {
        let toml = vocoder_toml("[vocoder]\ntrusted_serials = [\"TEST SER\"]");
        assert!(NodeFileConfig::from_toml_str_redacted(&toml).is_err());
    }

    #[test]
    fn vocoder_example_block_parses() {
        const EXAMPLE: &str = include_str!("../../../deploy/node.toml.example");
        let lines: Vec<&str> = EXAMPLE.lines().collect();
        let start = lines
            .iter()
            .position(|l| *l == "# [vocoder]")
            .expect("the example has a commented `# [vocoder]` line");
        let block = lines[start..]
            .iter()
            .take_while(|l| l.starts_with('#'))
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| l.starts_with("[vocoder]") || l.starts_with("trusted_serials"))
            .map(|l| l.split_once(" #").map_or(l, |(v, _)| v).trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(block, "[vocoder]\ntrusted_serials = [\"TESTSER1\"]");
        let cfg = NodeFileConfig::from_toml_str(&vocoder_toml(&block)).unwrap();
        assert_eq!(cfg.vocoder_trust().serials, vec!["TESTSER1"]);
    }

    // ---- [web] and [stream] -------------------------------------------------

    fn with_sections(extra: &str) -> String {
        format!("{}\n{extra}\n", minimal_toml())
    }

    fn parse_ok(extra: &str) -> NodeFileConfig {
        NodeFileConfig::from_toml_str(&with_sections(extra))
            .unwrap_or_else(|e| panic!("must load: {e}\n{extra}"))
    }

    fn parse_err(extra: &str) -> String {
        match NodeFileConfig::from_toml_str(&with_sections(extra)) {
            Ok(_) => panic!("must be refused:\n{extra}"),
            Err(e) => e,
        }
    }

    #[test]
    fn web_and_stream_defaults_parse() {
        let cfg = parse_ok(
            r#"
            [web]
            public_bind = "0.0.0.0:443"
            public_hostnames = ["108-175-15-153.SSLIP.io."]
            [stream]
            enabled = true
        "#,
        );
        let web = cfg.web.unwrap();
        assert_eq!(web.hostnames(), ["108-175-15-153.sslip.io"]);
        assert_eq!(web.http_bind(), Some("0.0.0.0:80"));
        assert_eq!(web.challenge(), AcmeChallenge::Http01);
        assert!(!web.acme_staging);
        let s = cfg.stream.unwrap();
        assert!(s.enabled);
        assert_eq!((s.max_listeners, s.mp3_kbps, s.opus_kbps), (25, 32, 24));
    }

    #[test]
    fn absent_sections_are_none_and_stream_defaults_off() {
        let cfg = parse_ok("");
        assert!(cfg.web.is_none() && cfg.stream.is_none());
        assert!(!StreamCfg::default().enabled);
    }

    // ---- [recording] ----------------------------------------------------------

    #[test]
    fn recording_defaults_match_the_spec() {
        let r = parse_ok("[recording]\n")
            .recording
            .expect("[recording] is present");
        assert!(!r.enabled);
        assert_eq!(r.dir, std::path::PathBuf::from("/var/lib/astar/recordings"));
        assert_eq!(r.max_bytes.bytes, 2_000_000_000);
        assert_eq!(r.max_bytes.text, "2 GB");
        assert_eq!(r.max_age_days, 30);
        assert!(r.frames);
        assert!(parse_ok("").recording.is_none(), "absent stays absent");
    }

    // ---- [netrec] -------------------------------------------------------------

    #[test]
    fn netrec_table_parses_and_refuses_unknown_keys() {
        let n = parse_ok("[netrec]\narchive = \"/srv/nets\"\n")
            .netrec
            .expect("[netrec] is present");
        assert_eq!(n.archive, std::path::PathBuf::from("/srv/nets"));
        assert!(parse_ok("").netrec.is_none(), "absent stays absent");
        let e = parse_err("[netrec]\narchive = \"/srv/nets\"\nffmpeg = true\n");
        assert!(e.contains("ffmpeg"), "{e}");
        let e = parse_err("[netrec]\n");
        assert!(e.contains("archive"), "{e}");
    }

    // ---- [packet_log] ---------------------------------------------------------

    #[test]
    fn packet_log_absent_means_off() {
        assert!(parse_ok("").packet_log.is_none());
    }

    #[test]
    fn packet_log_defaults() {
        let p = parse_ok("[packet_log]\n")
            .packet_log
            .expect("[packet_log] is present");
        assert!(!p.debug_mode);
        assert_eq!(
            p.dir,
            std::path::PathBuf::from("/var/lib/astar/packet-logs")
        );
        assert_eq!(p.max_bytes.bytes, 1_000_000_000);
        assert_eq!(p.max_bytes.text, "1 GB");
    }

    #[test]
    fn packet_log_refuses_unknown_keys() {
        let e = parse_err("[packet_log]\nkeep = 3\n");
        assert!(e.contains("keep"), "{e}");
    }

    #[test]
    fn packet_log_refuses_the_old_enabled_key() {
        let e = parse_err("[packet_log]\nenabled = true\n");
        assert!(e.contains("enabled"), "{e}");
    }

    #[test]
    fn packet_log_dir_must_be_absolute() {
        assert_eq!(
            parse_err("[packet_log]\ndir = \"logs\"\n"),
            "packet_log.dir: logs must be an absolute path"
        );
        assert_eq!(
            parse_err("[packet_log]\ndir = \"\"\n"),
            "packet_log.dir: must not be empty"
        );
    }

    #[test]
    fn packet_log_refuses_below_100_mb() {
        assert_eq!(
            parse_err("[packet_log]\nmax_bytes = \"10 MB\"\n"),
            "packet_log.max_bytes: 10 MB is below the 100 MB minimum"
        );
        let p = parse_ok("[packet_log]\nmax_bytes = \"100 MB\"\n")
            .packet_log
            .unwrap();
        assert_eq!(p.max_bytes.bytes, 100_000_000);
    }

    #[test]
    fn packet_log_is_not_refused_in_parrot_mode() {
        let p = parse_ok("[bridge]\nmode = \"parrot\"\n[packet_log]\ndebug_mode = true\n")
            .packet_log
            .unwrap();
        assert!(p.debug_mode);
    }

    #[test]
    fn node_toml_without_packet_log_loads_and_stays_off() {
        // As Ansible first wrote it, and as the Settings page keeps it.
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).expect("loads");
        assert!(cfg.packet_log.is_none());
        let saved = format!("{}\n[packet_log]\ndebug_mode = true\n", minimal_toml());
        let cfg = NodeFileConfig::from_toml_str(&saved).expect("loads");
        assert!(cfg.packet_log.unwrap().debug_mode);
    }

    #[test]
    fn parse_size_units() {
        for (text, bytes) in [
            ("2 GB", 2_000_000_000),
            ("500MB", 500_000_000),
            ("1 GiB", 1_073_741_824),
            ("123", 123),
            ("7 kb", 7_000),
            ("3 MiB", 3 * 1_048_576),
            ("4 KiB", 4_096),
            ("9 B", 9),
            ("  2  gb ", 2_000_000_000),
        ] {
            assert_eq!(parse_size(text), Ok(bytes), "{text:?}");
        }
        for text in ["2 XB", "", "-1 GB", "GB", "1.5 GB", "99999999999999 GB"] {
            assert!(parse_size(text).is_err(), "{text:?} must be refused");
        }
    }

    #[test]
    fn recording_keeps_max_bytes_as_written() {
        let r = parse_ok("[recording]\nmax_bytes = \"500MB\"\n")
            .recording
            .unwrap();
        assert_eq!(
            (r.max_bytes.bytes, r.max_bytes.text.as_str()),
            (500_000_000, "500MB")
        );
        let e = parse_err("[recording]\nmax_bytes = \"2 XB\"\n");
        assert!(e.contains("2 XB"), "{e}");
        let e = parse_err("[recording]\nmax_bytes = 2000000000\n");
        assert!(e.contains("max_bytes"), "{e}");
        let e = parse_err("[recording]\nkeep = 3\n");
        assert!(e.contains("keep"), "{e}");
    }

    #[test]
    fn recording_rejects_bad_values() {
        assert_eq!(
            parse_err("[recording]\nmax_bytes = \"10 MB\"\n"),
            "recording.max_bytes: 10 MB is below the 100 MB minimum"
        );
        assert_eq!(
            parse_err("[recording]\nmax_age_days = 0\n"),
            "recording.max_age_days: 0 is outside 1–3650"
        );
        assert_eq!(
            parse_err("[recording]\nmax_age_days = 3651\n"),
            "recording.max_age_days: 3651 is outside 1–3650"
        );
        assert_eq!(
            parse_err("[recording]\ndir = \"\"\n"),
            "recording.dir: must not be empty"
        );
        assert_eq!(
            parse_err("[recording]\ndir = \"recordings\"\n"),
            "recording.dir: recordings must be an absolute path"
        );
        assert_eq!(
            parse_err("[recording]\ndir = \"./var/rec\"\n"),
            "recording.dir: ./var/rec must be an absolute path"
        );
        let r = parse_ok("[recording]\nmax_bytes = \"100 MB\"\nmax_age_days = 3650\n")
            .recording
            .unwrap();
        assert_eq!((r.max_bytes.bytes, r.max_age_days), (100_000_000, 3650));
    }

    // ---- sign-in keys ([control] loopback_login, [web] auth keys) ----------

    #[test]
    fn sign_in_keys_default() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert!(!cfg.control.loopback_login);
        let cfg = parse_ok(
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.org\"]\n",
        );
        let web = cfg.web.unwrap();
        assert!(web.anonymous_status());
        assert!(web.admin_on_public());
        assert_eq!(web.rp_id(), None);
        assert_eq!(
            (
                web.session_idle_days,
                web.session_max_days,
                web.step_up_minutes
            ),
            (7, 30, 10)
        );
    }

    #[test]
    fn loopback_login_parses() {
        let toml = minimal_toml().replace(
            "bind = \"127.0.0.1:8730\"",
            "bind = \"127.0.0.1:8730\"\n        loopback_login = true",
        );
        assert!(
            NodeFileConfig::from_toml_str(&toml)
                .unwrap()
                .control
                .loopback_login
        );
    }

    #[test]
    fn sign_in_ranges_are_checked() {
        let base =
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.org\"]\n";
        for (extra, key) in [
            ("session_idle_days = 0", "web.session_idle_days"),
            ("session_idle_days = 31", "web.session_idle_days"),
            (
                "session_idle_days = 10\nsession_max_days = 9",
                "web.session_max_days",
            ),
            ("session_max_days = 91", "web.session_max_days"),
            ("step_up_minutes = 1", "web.step_up_minutes"),
            ("step_up_minutes = 61", "web.step_up_minutes"),
        ] {
            let e = parse_err(&format!("{base}{extra}\n"));
            assert!(e.starts_with(key), "{extra}: {e}");
        }
        parse_ok(&format!(
            "{base}session_idle_days = 1\nsession_max_days = 90\nstep_up_minutes = 60\n"
        ));
    }

    #[test]
    fn admin_on_public_needs_a_public_bind() {
        let e = parse_err("[web]\nadmin_on_public = true\n");
        assert!(e.starts_with("web.admin_on_public"), "{e}");
        let cfg = parse_ok("[web]\nadmin_on_public = false\n");
        assert!(!cfg.web.unwrap().admin_on_public());
    }

    #[test]
    fn rp_id_must_be_a_parent_you_control() {
        let ok = parse_ok(
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.org\", \"n2.example.org\"]\nrp_id = \"Example.org.\"\n",
        );
        assert_eq!(ok.web.unwrap().rp_id(), Some("example.org"));
        let empty = parse_ok(
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.org\"]\nrp_id = \"\"\n",
        );
        assert_eq!(empty.web.unwrap().rp_id(), None, "empty means per-hostname");
        for (hosts, rp, why) in [
            ("\"node.example.org\"", "other.org", "not under"),
            (
                "\"node.example.org\", \"x.other.net\"",
                "example.org",
                "not under",
            ),
            ("\"1-2-3-4.sslip.io\"", "sslip.io", "shared"),
            ("\"node.example.org\"", "org", "not a DNS name"),
        ] {
            let e = parse_err(&format!(
                "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [{hosts}]\nrp_id = \"{rp}\"\n"
            ));
            assert!(e.starts_with("web.rp_id") && e.contains(why), "{rp}: {e}");
        }
    }

    #[test]
    fn public_bind_without_hostnames_is_rejected() {
        let e = parse_err("[web]\npublic_bind = \"0.0.0.0:443\"\n");
        assert!(e.contains("public_hostnames"), "{e}");
    }

    #[test]
    fn ip_literal_and_wildcard_hostnames_are_rejected() {
        for bad in [
            "108.175.15.153",
            "::1",
            "*.example.com",
            "localhost",
            "a..b",
        ] {
            let e = parse_err(&format!(
                "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"{bad}\"]\n"
            ));
            assert!(e.starts_with("web.public_hostnames"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_bad_bind_is_rejected_naming_the_key() {
        let e = parse_err("[web]\npublic_bind = \"443\"\npublic_hostnames = [\"a.example.com\"]\n");
        assert!(e.starts_with("web.public_bind"), "{e}");
        let e = parse_err("[web]\nacme_http_bind = \"nope\"\n");
        assert!(e.starts_with("web.acme_http_bind"), "{e}");
    }

    #[test]
    fn an_unknown_web_key_is_refused() {
        let e = parse_err("[web]\npublic_hostname = \"a.example.com\"\n");
        assert!(e.contains("public_hostname"), "{e}");
    }

    #[test]
    fn stream_limits_are_validated() {
        for (k, v) in [
            ("max_listeners", "0"),
            ("max_listeners", "201"),
            ("mp3_kbps", "33"),
            ("opus_kbps", "5"),
            ("opus_kbps", "129"),
        ] {
            let e = parse_err(&format!("[stream]\n{k} = {v}\n"));
            assert!(e.contains(k), "{k}={v}: {e}");
        }
        for (k, v) in [
            ("max_listeners", "1"),
            ("max_listeners", "200"),
            ("mp3_kbps", "8"),
            ("mp3_kbps", "64"),
            ("opus_kbps", "6"),
            ("opus_kbps", "128"),
        ] {
            parse_ok(&format!("[stream]\n{k} = {v}\n"));
        }
    }

    #[test]
    fn a_dns_provider_defaults_the_challenge_to_dns01_and_drops_port_80() {
        let cfg = parse_ok(
            r#"
            [web]
            public_bind = "0.0.0.0:443"
            public_hostnames = ["node.Example.com", "example.com"]
            [web.dns]
            provider = "porkbun"
            domain = "Example.com."
            ddns = true
        "#,
        );
        let web = cfg.web.unwrap();
        assert_eq!(web.challenge(), AcmeChallenge::Dns01);
        assert_eq!(web.http_bind(), None, "DNS-01 needs no port 80");
        let dns = web.dns.unwrap();
        assert_eq!(dns.provider, DnsProviderKind::Porkbun);
        assert_eq!(dns.domain, "example.com");
        assert!(dns.ddns);
        assert_eq!((dns.ddns_interval_secs, dns.ttl), (300, 600));
    }

    #[test]
    fn dns01_with_an_explicit_http_bind_keeps_the_redirect_listener() {
        let cfg = parse_ok(
            r#"
            [web]
            public_bind = "0.0.0.0:443"
            public_hostnames = ["node.example.com"]
            acme_http_bind = "0.0.0.0:80"
            [web.dns]
            provider = "porkbun"
            domain = "example.com"
        "#,
        );
        assert_eq!(cfg.web.unwrap().http_bind(), Some("0.0.0.0:80"));
    }

    #[test]
    fn http01_can_be_forced_with_a_provider_present() {
        let cfg = parse_ok(
            r#"
            [web]
            public_hostnames = ["node.example.com"]
            acme_challenge = "http-01"
            [web.dns]
            provider = "porkbun"
            domain = "example.com"
            ddns = true
        "#,
        );
        let web = cfg.web.unwrap();
        assert_eq!(web.challenge(), AcmeChallenge::Http01);
        assert_eq!(web.http_bind(), Some(DEFAULT_ACME_HTTP_BIND));
    }

    #[test]
    fn the_dns_provider_defaults_to_porkbun_and_names_are_checked() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n";
        let web = parse_ok(&format!("{base}[web.dns]\ndomain = \"example.com\"\n"))
            .web
            .unwrap();
        assert_eq!(web.dns.unwrap().provider, DnsProviderKind::Porkbun);
        let e = parse_err(&format!(
            "{base}[web.dns]\nprovider = \"nosuchdns\"\ndomain = \"example.com\"\n"
        ));
        assert!(e.contains("nosuchdns") && e.contains("cloudflare"), "{e}");
    }

    #[test]
    fn only_porkbun_can_be_its_own_ip_source() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\ndomain = \"example.com\"\n";
        for extra in [
            "provider = \"cloudflare\"\nip_source = \"provider\"\n",
            "provider = \"cloudflare\"\nddns = true\n",
            "ip_source = \"http://example.org/ip\"\n",
            "ip_source = \"ftp://x\"\n",
        ] {
            let e = parse_err(&format!("{base}{extra}"));
            assert!(
                e.contains("ip_source") || e.contains("dns-providers"),
                "{extra}: {e}"
            );
        }
        let web = parse_ok(&format!(
            "{base}ip_source = \"https://ip.example.org/\"\nddns = true\n"
        ))
        .web
        .unwrap();
        assert!(matches!(
            web.dns.unwrap().ip_source(),
            IpSourceCfg::Url("https://ip.example.org/")
        ));
    }

    #[test]
    fn a_bad_ip_source_error_never_echoes_its_query_or_userinfo() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\ndomain = \"example.com\"\n";
        for bad in [
            "http://user:SENTINEL@ip.example.org/ip?token=SENTINEL",
            "ftp://ip.example.org/ip?token=SENTINEL",
            "SENTINEL",
            "SENTINEL?token=SENTINEL",
        ] {
            let e = parse_err(&format!("{base}ip_source = \"{bad}\"\n"));
            assert!(e.contains("ip_source"), "{bad}: {e}");
            assert!(!e.contains("SENTINEL"), "{bad}: {e}");
        }
        let e = parse_err(&format!(
            "{base}ip_source = \"http://u:SENTINEL@ip.example.org/ip?t=SENTINEL\"\n"
        ));
        assert!(e.contains("http://ip.example.org"), "{e}");
        for bad in [
            "http://user:SENTINEL/TAIL@ip.example.org/ip",
            "http://user:SENTINEL?TAIL@ip.example.org/ip",
            "http://user:SENTINEL#TAIL@ip.example.org/ip",
        ] {
            let e = parse_err(&format!("{base}ip_source = \"{bad}\"\n"));
            assert!(!e.contains("SENTINEL") && !e.contains("TAIL"), "{bad}: {e}");
            assert!(!e.contains("user"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_raw_delimiter_in_ip_source_userinfo_is_refused_at_load() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\ndomain = \"example.com\"\n";
        for bad in [
            "https://user:SENTINEL/TAIL@ip.example.org/ip",
            "https://user:SENTINEL?TAIL@ip.example.org/ip",
            "https://user:SENTINEL#TAIL@ip.example.org/ip",
        ] {
            let e = parse_err(&format!("{base}ip_source = \"{bad}\"\n"));
            assert!(e.contains("userinfo must be percent-encoded"), "{bad}: {e}");
            assert!(!e.contains("SENTINEL") && !e.contains("TAIL"), "{bad}: {e}");
        }
        parse_ok(&format!(
            "{base}ip_source = \"https://user:p%2Fw@ip.example.org/ip\"\n"
        ));
    }

    #[cfg(not(feature = "dns-providers"))]
    #[test]
    fn a_dns_update_provider_needs_the_feature() {
        let e = parse_err(
            "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\nprovider = \"cloudflare\"\ndomain = \"example.com\"\nip_source = \"https://ip.example.org/\"\n",
        );
        assert!(e.contains("dns-providers") && e.contains("release"), "{e}");
    }

    #[cfg(not(feature = "dns-providers"))]
    #[test]
    fn the_missing_feature_is_reported_before_the_ip_source() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\ndomain = \"example.com\"\nprovider = \"cloudflare\"\n";
        for extra in ["ddns = true\n", "ip_source = \"provider\"\n"] {
            let e = parse_err(&format!("{base}{extra}"));
            assert!(e.contains("dns-providers"), "{extra}: {e}");
        }
    }

    #[cfg(feature = "dns-providers")]
    #[test]
    fn a_dns_update_provider_loads_with_the_feature() {
        let web = parse_ok(
            "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\nprovider = \"cloudflare\"\ndomain = \"example.com\"\nip_source = \"https://ip.example.org/\"\nddns = true\n",
        )
        .web
        .unwrap();
        assert_eq!(
            web.dns.unwrap().provider,
            DnsProviderKind::Other("cloudflare")
        );
    }

    #[test]
    fn dns_settings_are_validated() {
        let base = "[web]\npublic_hostnames = [\"node.example.com\"]\n";
        for (extra, key) in [
            ("acme_challenge = \"dns-01\"\n", "web.acme_challenge"),
            ("acme_challenge = \"tls-alpn-01\"\n", "acme_challenge"),
            (
                "[web.dns]\nprovider = \"nosuchdns\"\ndomain = \"example.com\"\n",
                "provider",
            ),
            (
                "[web.dns]\nprovider = \"porkbun\"\ndomain = \"other.org\"\n",
                "web.public_hostnames",
            ),
            (
                "[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.com\"\nttl = 300\n",
                "web.dns.ttl",
            ),
            (
                "[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.com\"\nddns_interval_secs = 5\n",
                "web.dns.ddns_interval_secs",
            ),
            (
                "[web.dns]\nprovider = \"porkbun\"\ndomain = \"1.2.3.4\"\n",
                "web.dns.domain",
            ),
        ] {
            let e = parse_err(&format!("{base}{extra}"));
            assert!(e.contains(key), "{extra}: {e}");
        }
        let e = parse_err(
            "[web]\npublic_hostnames = []\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.com\"\nddns = true\n",
        );
        assert!(e.starts_with("web.public_hostnames"), "{e}");
    }

    #[test]
    fn web_and_stream_reach_the_debug_output_without_a_secret_word() {
        let cfg = parse_ok(
            "[web]\npublic_hostnames = [\"node.example.com\"]\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.com\"\n[stream]\nenabled = true\n[recording]\n",
        );
        let d = format!("{cfg:?}");
        assert!(
            d.contains("web: Some(WebCfg {")
                && d.contains("stream: Some(StreamCfg {")
                && d.contains("recording: Some(RecordingCfg {"),
            "{d}"
        );
        for bad in ["secret", "password", "token"] {
            assert!(!d.contains(bad), "{bad} in {d}");
        }
    }

    // ---- hostnames derived from the node number -----------------------------

    const REGISTER_69586: &str = "[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"69586\"\n";

    #[test]
    fn the_node_token_expands_to_the_register_node_id() {
        let cfg = parse_ok(&format!(
            "{REGISTER_69586}[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"{{node}}.Example.NET.\", \"n{{node}}.sslip.io\"]\n"
        ));
        let web = cfg.web.unwrap();
        assert_eq!(web.hostnames(), ["69586.example.net", "n69586.sslip.io"]);
        assert_eq!(
            web.public_hostnames,
            Some(vec![
                "{node}.Example.NET.".to_string(),
                "n{node}.sslip.io".to_string()
            ]),
            "the list as written is kept for the settings page"
        );
    }

    #[test]
    fn a_dns_section_with_no_hostnames_defaults_to_node_dot_domain() {
        let cfg = parse_ok(&format!(
            "{REGISTER_69586}[web]\npublic_bind = \"0.0.0.0:443\"\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"Example.net.\"\nddns = true\n"
        ));
        let web = cfg.web.unwrap();
        assert_eq!(web.hostnames(), ["69586.example.net"]);
        assert!(web.public_hostnames.is_none());
    }

    #[test]
    fn the_dns_host_template_shapes_the_default() {
        for (host, want) in [
            ("node{node}", Some("node69586.example.net")),
            ("{NODE}x", None),
            ("Radio.", None),
            ("radio", Some("radio.example.net")),
            ("", Some("example.net")),
        ] {
            let text = format!(
                "{REGISTER_69586}[web]\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.net\"\nhost = \"{host}\"\nddns = true\n"
            );
            if let Some(name) = want {
                assert_eq!(parse_ok(&text).web.unwrap().hostnames(), [name], "{host}");
            } else {
                // A token in the wrong case is not a token, and braces or an
                // empty label are not a DNS name: refused under the host key.
                let e = parse_err(&text);
                assert!(e.starts_with("web.dns.host"), "{host}: {e}");
            }
        }
    }

    #[test]
    fn the_node_token_without_a_node_number_names_register() {
        let e = parse_err(
            "[web]\npublic_bind = \"0.0.0.0:443\"\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.net\"\n",
        );
        assert!(
            e.starts_with("web.dns.host") && e.contains("[register]"),
            "{e}"
        );
        let e = parse_err(
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"{node}.example.net\"]\n",
        );
        assert!(
            e.starts_with("web.public_hostnames") && e.contains("[register]"),
            "{e}"
        );
    }

    #[test]
    fn an_expanded_name_outside_the_zone_is_refused() {
        let e = parse_err(&format!(
            "{REGISTER_69586}[web]\npublic_hostnames = [\"{{node}}.example.org\"]\n[web.dns]\nprovider = \"porkbun\"\ndomain = \"example.net\"\n"
        ));
        assert!(e.contains("69586.example.org"), "{e}");
    }

    #[test]
    fn web_and_stream_example_blocks_parse() {
        // deploy/node.toml.example documents [web], [web.dns] and [stream]
        // as commented blocks. Uncomment their key lines and load them.
        const EXAMPLE: &str = include_str!("../../../deploy/node.toml.example");
        let lines: Vec<&str> = EXAMPLE.lines().collect();
        let start = lines
            .iter()
            .position(|l| *l == "# [web]")
            .expect("the example has a commented `# [web]` line");
        let block = lines[start..]
            .iter()
            .take_while(|l| l.starts_with('#'))
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| {
                l.starts_with('[') || l.split_once(" = ").is_some_and(|(k, _)| !k.contains(' '))
            })
            .map(|l| l.split_once(" #").map_or(l, |(v, _)| v).trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            block.contains("[web.dns]") && block.contains("[stream]"),
            "{block}"
        );
        let text = format!("[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"69586\"\n{block}");
        let cfg = parse_ok(&text);
        let web = cfg.web.expect("[web]");
        assert_eq!(web.hostnames(), ["69586.example.net"]);
        assert_eq!(web.challenge(), AcmeChallenge::Dns01);
        assert!(cfg.stream.expect("[stream]").enabled);
    }

    // ---- inline credentials under source = "config" ---------------------------

    const INLINE_ALL: &str = "[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"69586\"\n\
        [portal]\nuser = \"op\"\nnode = \"69586\"\n";

    fn config_source(extra: &str) -> NodeFileConfig {
        let text = format!(
            "[listener]\nbind = \"0.0.0.0:4569\"\nanswer = \"auto\"\nmax_calls = 2\nauth = \"off\"\n\
             [control]\nbind = \"127.0.0.1:8730\"\n{INLINE_ALL}[secrets]\nsource = \"config\"\n{extra}"
        );
        NodeFileConfig::from_toml_str(&text).unwrap()
    }

    #[test]
    fn source_config_loads_every_inline_credential() {
        let cfg = config_source(
            "secret = \"SENTINEL-node\"\nportal = \"SENTINEL-portal\"\ndiscord = \"SENTINEL-bot\"\n\
             porkbun_pk = \"pk1_SENTINEL\"\nporkbun_sk = \"sk1_SENTINEL\"\n",
        );
        let p = crate::secrets::SecretProvider::new();
        assert!(
            cfg.load_credentials(std::path::Path::new("/x/node.toml"), &p)
                .is_none()
        );
        assert_eq!(p.resolve("69586"), "SENTINEL-node");
        assert_eq!(p.resolve("portal"), "SENTINEL-portal");
        assert_eq!(p.resolve("discord"), "SENTINEL-bot");
        assert_eq!(p.resolve("porkbun_pk"), "pk1_SENTINEL");
        assert_eq!(p.resolve("porkbun_sk"), "sk1_SENTINEL");
        assert!(
            cfg.portal_credentials(&p).unwrap().is_some(),
            "no portal.env needed"
        );
        let d = format!("{cfg:?}");
        assert!(!d.contains("SENTINEL"), "Debug leaked an inline value: {d}");
    }

    #[test]
    fn inline_credentials_are_ignored_under_other_sources() {
        let text = format!(
            "{}{INLINE_ALL}",
            minimal_toml().replace(
                "source = \"env\"",
                "source = \"env\"\nporkbun_pk = \"pk1_SENTINEL\""
            )
        );
        let cfg = NodeFileConfig::from_toml_str(&text).unwrap();
        let p = crate::secrets::SecretProvider::new();
        let _ = cfg.load_credentials(std::path::Path::new("/x/node.toml"), &p);
        assert_eq!(
            p.resolve("porkbun_pk"),
            "",
            "env source reads the environment only"
        );
    }

    #[test]
    fn a_non_string_inline_credential_is_named_but_never_quoted() {
        for key in INLINE_CREDENTIAL_KEYS {
            let text = format!(
                "{}\n",
                minimal_toml().replace(
                    "source = \"env\"",
                    &format!("source = \"config\"\n{key} = 31337424242")
                )
            );
            let e = NodeFileConfig::from_toml_str_redacted(&text).unwrap_err();
            assert_eq!(e, format!("secrets.{key}: expected a string"));
            assert!(!e.contains("31337424242"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn inline_credentials_in_a_readable_file_are_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        let text = minimal_toml().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"SENTINEL\"",
        );
        std::fs::write(&path, &text).unwrap();
        let cfg = NodeFileConfig::from_toml_str(&text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let w = cfg
            .inline_credentials_mode_warning(&path)
            .expect("0644 is warned about");
        assert!(w.contains("chmod 600") && !w.contains("SENTINEL"), "{w}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(cfg.inline_credentials_mode_warning(&path).is_none());
        let env = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            env.inline_credentials_mode_warning(&path).is_none(),
            "no inline credentials"
        );
    }

    #[test]
    fn the_state_dir_is_beside_the_config_or_systemds() {
        // STATE_DIRECTORY is process-wide; only this test touches it.
        unsafe { std::env::remove_var("STATE_DIRECTORY") };
        assert_eq!(
            state_dir(Path::new("/etc/astar/node.toml")),
            Path::new("/etc/astar/state")
        );
        unsafe { std::env::set_var("STATE_DIRECTORY", "/var/lib/astar:/var/lib/other") };
        assert_eq!(
            state_dir(Path::new("/etc/astar/node.toml")),
            Path::new("/var/lib/astar")
        );
        unsafe { std::env::remove_var("STATE_DIRECTORY") };
    }

    // ---- [echolink] -----------------------------------------------------------

    const ECHOLINK: &str = "[echolink]\ncallsign = \"KX7ABC-L\"\n";
    const DISCORD: &str =
        "[discord]\nguild_id = \"123456789012345678\"\nchannel_id = \"223456789012345678\"\n";
    const POLICY: &str = "EchoLink's access policy forbids linking it to VoIP systems people reach directly from a computer";

    #[test]
    fn no_echolink_section_means_none() {
        const EXAMPLE: &str = include_str!("../../../deploy/node.toml.example");
        for text in [crate::template::NODE_TOML_TEMPLATE, EXAMPLE, minimal_toml()] {
            let cfg = NodeFileConfig::from_toml_str(text).expect("the fixture loads");
            assert!(cfg.echolink.is_none());
        }
    }

    #[test]
    fn echolink_needs_an_l_or_r_suffix() {
        assert_eq!(
            parse_err("[echolink]\ncallsign = \"KX7ABC\"\n"),
            "echolink.callsign: \"KX7ABC\" has no -L or -R suffix; EchoLink requires one for a node"
        );
        for (written, kept) in [("kx7abc-l", "KX7ABC-L"), ("KX7ABC-R", "KX7ABC-R")] {
            let cfg = parse_ok(&format!("[echolink]\ncallsign = \"{written}\"\n"));
            assert_eq!(cfg.echolink.unwrap().callsign, kept);
        }
    }

    #[test]
    fn echolink_refuses_discord() {
        assert_eq!(
            parse_err(&format!("{ECHOLINK}{DISCORD}")),
            format!(
                "echolink: can't run with [discord] — {POLICY}; remove one of the two sections"
            )
        );
    }

    #[test]
    fn echolink_refuses_the_public_listener() {
        let web =
            "[web]\npublic_bind = \"0.0.0.0:443\"\npublic_hostnames = [\"node.example.org\"]\n";
        // Without EchoLink the same page loads, so the refusal is EchoLink's.
        assert!(parse_ok(web).public_listener_configured());
        assert_eq!(
            parse_err(&format!("{ECHOLINK}{web}")),
            format!(
                "echolink: can't run with the public listener ([web] public_bind or [stream] enabled) — {POLICY}; remove one of them"
            )
        );
    }

    #[test]
    fn echolink_refuses_the_stream() {
        let stream = "[stream]\nenabled = true\n";
        assert!(parse_ok(stream).public_listener_configured());
        assert!(parse_err(&format!("{ECHOLINK}{stream}")).starts_with(
            "echolink: can't run with the public listener ([web] public_bind or [stream] enabled)"
        ));
        // A [stream] section left switched off, or a [web] section with no
        // public bind, is not a listener.
        let quiet = parse_ok(&format!(
            "{ECHOLINK}[stream]\nenabled = false\n[web]\nacme_email = \"op@example.org\"\n"
        ));
        assert!(!quiet.public_listener_configured());
        assert!(quiet.echolink.is_some());
    }

    #[test]
    fn echolink_defaults_to_the_four_servers() {
        let e = parse_ok(ECHOLINK).echolink.unwrap();
        assert_eq!(e.servers, ECHOLINK_DEFAULT_SERVERS);
        assert_eq!(
            ECHOLINK_DEFAULT_SERVERS,
            [
                "servers.echolink.org",
                "naeast.echolink.org",
                "nasouth.echolink.org",
                "backup.echolink.org"
            ]
        );
        assert_eq!(e.location, "");
        assert_eq!(
            parse_err(&format!("{ECHOLINK}servers = []\n")),
            "echolink.servers: at least one server is needed"
        );
        assert_eq!(
            parse_err(&format!("{ECHOLINK}servers = [\" \"]\n")),
            "echolink.servers: at least one server is needed"
        );
        let e = parse_ok(&format!(
            "{ECHOLINK}servers = [\"127.0.0.1:5200\"]\nlocation = \"Testville\"\n"
        ))
        .echolink
        .unwrap();
        assert_eq!(e.servers, ["127.0.0.1:5200"]);
        assert_eq!(e.location, "Testville");
        assert!(
            parse_err(&format!("{ECHOLINK}password = \"x\"\n")).contains("unknown field"),
            "the password never has a key in node.toml"
        );
    }
}
