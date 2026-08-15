// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! TOML configuration model for the node daemon.
//!
//! `NodeFileConfig` is what lives on disk. By default it is secret-free — no
//! passwords, no tokens — with credentials injected at runtime through the
//! [`crate::secrets::SecretProvider`] (env vars or control-channel
//! `ProvideSecret`, selected by `[secrets] source = "env" | "control"`).
//!
//! `source = "config"` (iax-4703) is the one deliberate exception: the
//! operator's own mounted config (e.g. `/etc/iaxnode/node.toml` on the VPS)
//! may carry the credential inline via `[secrets] secret = "..."`, trading the
//! secret-free-config invariant for a self-contained file on that one
//! operator-owned host. REPO-tracked config templates must never do this —
//! they ship `secret = ""` (or omit it) as a placeholder. Whatever the source,
//! [`NodeFileConfig`]'s `Debug` impl never emits the secret value.
//!
//! # Mapping
//! - `[listener].answer` → [`astar_station::AnswerPolicy`] (`"auto"` / `"manual"`)
//! - `[listener].auth`   → [`astar_station::IncomingAuthPolicy`]
//!   (`"required"` / `"optional"` / `"off"`, case-insensitive)
//! - `[secrets].source`  → `"env"`, `"control"`, or `"config"` (case-insensitive)
//! - top-level `codec_policy` → [`astar_station::CodecPolicy`] (iax-31f7)
//!   (`"ulaw_only"` (default) / `"allow_slin"` / `"prefer_slin"` /
//!   `"prefer_slin16"`), applied to both `to_inbound()`'s policy and (by the
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
use std::time::Duration;

use astar_iax::{BridgeConfig, BridgeMode};
use astar_station::{
    AnswerPolicy, CodecPolicy, InboundConfig, IncomingAuthPolicy, IncomingCallPolicy, KnownNodes,
    RegisterConfig,
};

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
    /// ID mode: `"cw"` or `"tts"`.
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
/// the config file: `credential_env` names the environment variable that
/// holds it (resolved once at startup in `main`, consumed into
/// `StationConfig.portal`, never logged).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PortalCfg {
    /// Portal account callsign (the allstarlink.org login).
    pub user: String,
    /// A node the account OWNS — token minting requires it.
    pub node: String,
    /// Environment variable NAME holding the portal account password
    /// (a reference, never material — the `secret_ref` idiom).
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
}

/// `[secrets]` section — secret source declaration.
///
/// **Not `Debug`-derived directly** — [`NodeFileConfig`]'s manual `Debug` impl
/// omits `secret` entirely so a real credential never reaches log output.
#[derive(Clone, serde::Deserialize)]
pub struct SecretsCfg {
    /// Source for runtime secrets: `"env"`, `"control"`, or `"config"`
    /// (case-insensitive).
    pub source: String,
    /// Inline secret (iax-4703), used when `source = "config"`. The operator's
    /// mounted `/etc/iaxnode/node.toml` is the only file allowed to carry a
    /// real value here; repo-tracked templates ship this empty. Empty/absent
    /// is valid at load time (boots; registration just can't authenticate) —
    /// [`NodeFileConfig::from_toml_str`] warns rather than errors in that case.
    #[serde(default)]
    pub secret: Option<String>,
}

// `secret` is deliberately excluded from `Debug` below — this section must
// never let a real credential reach `{:?}` output (iax-35b1, extended by
// iax-4703).
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for SecretsCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsCfg")
            .field("source", &self.source)
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
    /// Raw top-level `codec_policy` string (iax-31f7); absent ⇒ `None` ⇒
    /// `UlawOnly`. Parsed eagerly into [`Self::codec_policy`] by
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
            .field("control", &self.control)
            .field("codec_policy_raw", &self.codec_policy_raw)
            .field("codec_policy", &self.codec_policy)
            // Use a neutral key so "secret" never appears in the Debug output.
            .field("credential_source", &self.secrets.source)
            .field("wireguard", &self.wireguard)
            .field("portal", &self.portal)
            .field("links", &self.links)
            .field("dtmf", &self.dtmf)
            .finish()
    }
}

impl NodeFileConfig {
    /// Parse a TOML string into a `NodeFileConfig`, validating that the
    /// `answer`, `auth`, and `codec_policy` strings are recognised.
    ///
    /// Returns `Err(String)` on both TOML parse errors and unknown policy strings.
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let mut cfg: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        // Eagerly validate string fields so callers see clear errors at load time.
        cfg.parse_answer_policy()?;
        cfg.parse_auth_policy()?;
        cfg.to_bridge_config()?;
        cfg.parse_audio_backend()?;
        cfg.parse_secrets_source()?;
        cfg.codec_policy = parse_codec_policy(cfg.codec_policy_raw.as_deref())?;
        for (node, link) in &cfg.links {
            if let Some(shape) = link.shape.as_deref()
                && !shape.eq_ignore_ascii_case("standard")
                && !shape.eq_ignore_ascii_case("wt-guest")
            {
                return Err(format!(
                    "links.\"{node}\": unknown shape \"{shape}\" (expected \"standard\" or \"wt-guest\")"
                ));
            }
        }
        Ok(cfg)
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
                "astar-server: generated template config at {} — edit it (node_id, secret, [register]) and restart",
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
        let peer: SocketAddr = reg
            .peer
            .parse()
            .map_err(|e| format!("register.peer: {e}"))?;
        Ok(Some(RegisterConfig {
            peer,
            username: reg.node_id.clone(),
            refresh: Duration::from_secs(60),
        }))
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
    /// `"env"` and `"control"` are accepted as before. `"config"` (iax-4703) is
    /// also accepted; an empty/absent `secret` alongside it is valid — it just
    /// means registration won't be able to authenticate until an operator
    /// fills it in — so we warn rather than error, since Task 9's generated
    /// template ships `source = "config"` with `secret = ""`.
    fn parse_secrets_source(&self) -> Result<(), String> {
        match self.secrets.source.to_ascii_lowercase().as_str() {
            "env" | "control" => Ok(()),
            "config" => {
                let has_secret = self
                    .secrets
                    .secret
                    .as_deref()
                    .is_some_and(|s| !s.is_empty());
                if !has_secret {
                    eprintln!(
                        "astar-server: warning: [secrets] source = \"config\" but no secret is set; registration will not authenticate until one is provided"
                    );
                }
                Ok(())
            }
            other => Err(format!(
                "secrets.source: unknown value {other:?} (expected \"env\", \"control\", or \"config\")"
            )),
        }
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
/// absent) defaults to [`CodecPolicy::UlawOnly`]. Delegates to
/// [`CodecPolicy::from_str`], whose error message already names the field.
fn parse_codec_policy(s: Option<&str>) -> Result<CodecPolicy, String> {
    match s {
        None => Ok(CodecPolicy::default()),
        Some(raw) => raw.parse(),
    }
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
    fn config_has_no_secret_fields() {
        let toml = "[listener]\nbind=\"0.0.0.0:4569\"\nanswer=\"auto\"\nmax_calls=20\nauth=\"off\"\n[control]\nbind=\"127.0.0.1:8730\"\n[secrets]\nsource=\"env\"\n";
        let c = NodeFileConfig::from_toml_str(toml).unwrap();
        let d = format!("{c:?}");
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
        // Task 9's generated template ships `source = "config"` with
        // `secret = ""` — this must boot (warn, not error).
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
    fn unknown_secrets_source_is_error() {
        let toml = minimal_toml().replace("source = \"env\"", "source = \"carrier-pigeon\"");
        let err = NodeFileConfig::from_toml_str(&toml).unwrap_err();
        assert!(
            err.contains("secrets.source"),
            "error should mention field: {err}"
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
    fn codec_policy_defaults_to_ulaw_only() {
        let cfg = NodeFileConfig::from_toml_str(minimal_toml()).unwrap();
        assert_eq!(cfg.codec_policy, CodecPolicy::UlawOnly);
    }

    #[test]
    fn unknown_codec_policy_is_an_error() {
        assert!(
            NodeFileConfig::from_toml_str(&format!("codec_policy = \"opus\"\n{}", minimal_toml()))
                .is_err()
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
}
