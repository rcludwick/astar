// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The settings page's pure core: what the page may edit, how each value is
//! checked, which edits take effect live, and how `node.toml` is rewritten in
//! place.
//!
//! Nothing here reads or writes a file, opens a socket or reaches the
//! controller; the one filesystem touch is the `stat` of a state-file parent
//! in `state_file_parent_problem`, so a path that cannot exist is refused
//! before the store writes anything. The store owns the filesystem and the
//! route owns the wire; this module is the part that can be tested to the
//! byte.
//!
//! Three ideas hold it together:
//!
//! * **A closed vocabulary.** The page speaks dotted wire paths
//!   (`discord.channel_id`, `links."2000".shape`) and only paths the table in
//!   `rule` lists are accepted. That is what makes `[discord]`'s
//!   `deny_unknown_fields` safe rather than a boot-breaker: the page cannot
//!   invent a key.
//! * **The daemon's own loader as the oracle.** After a patch is rendered,
//!   [`oracle`] runs `NodeFileConfig::from_toml_str_redacted`, then
//!   `to_inbound`, `to_bridge_config`, the `[control].bind` parse and the
//!   state-file parent check — the builders `main.rs` runs at boot that only
//!   parse — on the exact bytes about to be written. If any of them refuses,
//!   nothing is written. `to_register` and `to_link_transport` are
//!   deliberately not among them: both resolve DNS, and a save must never
//!   wait on the resolver.
//! * **Honest tiers.** [`tier`] says which paths apply live; everything else
//!   waits for a restart, and [`RedactedConfig::diff`] reports the difference
//!   between disk and the running mirror split the same way, so the page never
//!   claims a restart is pending when it is not, nor that a change is live
//!   when it is not.
//!
//! Secrets never pass through here: the vocabulary has no `secrets.secret`
//! path, [`RedactedConfig::from_config`] never reads it, and the only parse
//! error forwarded is toml's message, never its rendered snippet.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::Path;

use astar_station::CodecPolicy;
use serde_json::Value;
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike};

use crate::config::{NodeFileConfig, state_file_parent_problem};

/// Wire-path prefixes applied live. Everything else in the vocabulary is
/// restart-tier: its consumer is bound once before the controller is wrapped
/// in the `Arc`, and a half-live seam for `node_id` or `announce.*` is where
/// the on-air ID says one number and the registrar another.
///
/// `m17.enabled` and `[m17.reflectors]` are live because the controller
/// reads them at dial time and nothing else holds them. `m17.directory` (a
/// file read) and `m17.codec_dirs` (which decides the library the node loads
/// and runs) are not, and must never be.
///
/// `[packet_log]` is live because nothing starts from it at boot: a capture
/// starts at a press, and a save stops any capture that's running.
pub const LIVE_PREFIXES: &[&str] = &[
    "bridge.",
    "parrot.",
    "audio.input",
    "audio.output",
    "m17.enabled",
    "m17.reflectors",
    "packet_log.",
];

/// The event names `[announce.events]` understands.
const EVENT_NAMES: &[&str] = &[
    "incoming_call",
    "hangup",
    "registered",
    "register_failed",
    "answered",
];

/// When an edit to a path takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Applied by `POST /settings` after the file is written.
    Live,
    /// Written to disk; the node reads it at its next start.
    Restart,
}

/// The type-and-value rule for one vocabulary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// A whole removable table: accepts only `null`.
    Table,
    /// Top-level `codec_policy`; `CodecPolicy::from_str` owns the sentence.
    Codec,
    /// A `SocketAddr` literal.
    Bind,
    /// `[control] bind`: a `SocketAddr` that may not be a wildcard.
    ControlBind,
    /// `register.peer`: `host:port` syntax only, never resolved here.
    HostPort,
    /// A node number: ASCII digits only.
    NodeNumber,
    /// A list of node numbers.
    NodeNumbers,
    /// A Discord snowflake: a non-zero `u64` written as a string.
    Snowflake,
    /// A list of Discord user ids.
    Snowflakes,
    /// A closed picker. `exact_case` is set where the value is a canonical
    /// spelling the page writes verbatim: `audio.backend` is matched as
    /// written by `parse_audio_backend`, and an event destination — which
    /// the dispatcher lowercases before it looks at it — is pinned to the
    /// two spellings `node.toml.example` documents so the file stays
    /// uniform. The other pickers mirror the loader, which lowercases
    /// `answer`, `auth`, `bridge.mode` and `source`.
    Pick {
        options: &'static [&'static str],
        nullable: bool,
        exact_case: bool,
    },
    Str {
        nullable: bool,
    },
    NonEmptyStr {
        nullable: bool,
    },
    Bool {
        nullable: bool,
    },
    /// A non-negative integer. TOML integers are `i64`, so `i64::MAX` is the
    /// ceiling even though the wire type is "non-negative".
    Uint {
        nullable: bool,
    },
    /// Any JSON number, or null.
    Num,
    /// `discord.state_file`: its parent directory must exist now.
    StateFile,
    /// `credentials.file`: a non-empty path that is not the node's own
    /// config and does not put `secret`, `password` or `token` on the page.
    CredentialsFile,
    /// `links."<n>".shape`.
    LinkShape,
    /// `[m17] directory`: `"bundled"` or an absolute path, the loader's rule.
    M17Directory,
    /// `[m17] codec_dirs`: absolute paths only. The list decides which
    /// `libcodec2` the node loads, so the loader's rule is the page's rule.
    CodecDirs,
    /// `[announce.tts] binary`: the program the node runs to speak. The page
    /// may only name an existing regular file by absolute path; a bare name
    /// such as `"piper"` resolves through the daemon's `PATH`, which the file
    /// may say but the page may not.
    TtsBinary,
    /// One `[m17.reflectors]` entry: `host[:port]`, parsed as the loader
    /// parses it (no IPv6 literal, port 17000 when omitted).
    M17Reflector,
    /// `[vocoder] trusted_serials`: USB serial numbers by
    /// `config::check_vocoder_serial`, the loader's own rule.
    VocoderSerials,
    /// `[web] public_hostnames`: a list of names. The loader is the oracle
    /// for what a name may be (lower-cased, no IP literal, no wildcard).
    Hostnames,
}

impl Rule {
    fn accepts_null(self) -> bool {
        match self {
            Self::Table
            | Self::Codec
            | Self::Num
            | Self::StateFile
            | Self::CredentialsFile
            | Self::LinkShape
            | Self::NodeNumbers
            | Self::Snowflakes
            | Self::M17Directory
            | Self::CodecDirs
            | Self::TtsBinary
            | Self::M17Reflector
            | Self::Hostnames
            | Self::VocoderSerials => true,
            Self::Pick { nullable, .. }
            | Self::Str { nullable }
            | Self::NonEmptyStr { nullable }
            | Self::Bool { nullable }
            | Self::Uint { nullable } => nullable,
            Self::Bind
            | Self::ControlBind
            | Self::HostPort
            | Self::NodeNumber
            | Self::Snowflake => false,
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            Self::Bool { .. } => "a boolean",
            Self::Uint { .. } => "a non-negative integer",
            Self::Num => "a number",
            Self::NodeNumbers
            | Self::Snowflakes
            | Self::CodecDirs
            | Self::Hostnames
            | Self::VocoderSerials => "a list of strings",
            Self::Table => "null",
            Self::Codec
            | Self::Bind
            | Self::ControlBind
            | Self::HostPort
            | Self::NodeNumber
            | Self::Snowflake
            | Self::Pick { .. }
            | Self::Str { .. }
            | Self::NonEmptyStr { .. }
            | Self::StateFile
            | Self::CredentialsFile
            | Self::LinkShape
            | Self::M17Directory
            | Self::TtsBinary
            | Self::M17Reflector => "a string",
        }
    }
}

/// The closed vocabulary: wire-path segments → rule. `None` means the page
/// may not touch that path — `wireguard.*` is read-only, `[secrets]` is
/// reachable only through the two renamed `credentials.*` keys, and
/// `[discord]`'s reserved `model`/`latency` are not offered at all.
///
/// Grouped by section so it reads like `node.toml`, which is why identical
/// arms are not merged.
#[allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    reason = "one table, in node.toml order, deliberately in one place"
)]
fn rule(segments: &[&str]) -> Option<Rule> {
    use Rule::{
        Bind, Bool, Codec, CodecDirs, ControlBind, CredentialsFile, HostPort, Hostnames, LinkShape,
        M17Directory, M17Reflector, NodeNumber, NodeNumbers, NonEmptyStr, Num, Pick, Snowflake,
        Snowflakes, StateFile, Str, Table, TtsBinary, Uint, VocoderSerials,
    };
    Some(match segments {
        ["codec_policy"] => Codec,

        ["listener", "bind"] => Bind,
        ["listener", "answer"] => Pick {
            options: &["auto", "manual"],
            nullable: false,
            exact_case: false,
        },
        ["listener", "max_calls"] => Uint { nullable: false },
        ["listener", "auth"] => Pick {
            options: &["required", "optional", "off"],
            nullable: false,
            exact_case: false,
        },
        ["listener", "allowed_nodes"] => NodeNumbers,

        ["register"] => Table,
        ["register", "peer"] => HostPort,
        ["register", "node_id"] => NodeNumber,

        ["audio"] => Table,
        ["audio", "input" | "output"] => Str { nullable: true },
        ["audio", "backend"] => Pick {
            options: &["none", "cpal"],
            nullable: true,
            exact_case: true,
        },

        ["bridge"] => Table,
        ["bridge", "mode"] => Pick {
            options: &["handset", "bridge", "conference", "parrot"],
            nullable: false,
            exact_case: false,
        },
        ["bridge", "mix_minus" | "include_local_radio"] => Bool { nullable: false },

        ["parrot"] => Table,
        ["parrot", "playback_delay_ms" | "silence_gap_ms"] => Uint { nullable: true },
        ["parrot", "vox_threshold_db"] => Num,

        ["announce"] => Table,
        ["announce", "enabled"] => Bool { nullable: false },
        [
            "announce",
            "sounds_dir" | "join_template" | "link_connect_template" | "link_disconnect_template",
        ] => Str { nullable: true },
        ["announce", "mixunder_default_gain_db" | "cw_tone_hz"] => Num,
        ["announce", "id_interval_secs" | "cw_wpm"] => Uint { nullable: true },
        ["announce", "id_mode"] => Pick {
            options: &["off", "cw", "tts"],
            nullable: true,
            exact_case: false,
        },
        ["announce", "cw_keys_when_idle"] => Bool { nullable: true },
        ["announce", "tts"] => Table,
        ["announce", "tts", "enabled"] => Bool { nullable: false },
        ["announce", "tts", "binary"] => TtsBinary,
        ["announce", "tts", "voice"] => Str { nullable: true },
        ["announce", "tts", "timeout_ms"] => Uint { nullable: true },
        ["announce", "tts", "gain_db"] => Num,
        ["announce", "events"] => Table,
        ["announce", "events", _] => Table,
        ["announce", "events", _, "enabled"] => Bool { nullable: false },
        ["announce", "events", _, "destination"] => Pick {
            options: &["to_air", "to_monitor"],
            nullable: true,
            exact_case: true,
        },

        ["discord"] => Table,
        ["discord", "guild_id" | "channel_id"] => Snowflake,
        ["discord", "join_at_start" | "announce_on_join"] => Bool { nullable: false },
        ["discord", "approvers"] => Snowflakes,
        ["discord", "state_file"] => StateFile,
        ["discord", "mic_timeout_secs" | "mic_silence_secs"] => Uint { nullable: true },

        ["portal"] => Table,
        ["portal", "user" | "node"] => NonEmptyStr { nullable: false },
        ["portal", "credential_env"] => Str { nullable: true },

        ["links", _] => Table,
        ["links", _, "shape"] => LinkShape,

        ["dtmf"] => Table,
        ["dtmf", "enabled"] => Bool { nullable: false },
        ["dtmf", "inter_digit_timeout_ms"] => Uint { nullable: true },

        ["m17"] => Table,
        ["m17", "enabled"] => Bool { nullable: false },
        ["m17", "directory"] => M17Directory,
        ["m17", "codec_dirs"] => CodecDirs,
        ["m17", "reflectors"] => Table,
        ["m17", "reflectors", _] => M17Reflector,

        ["vocoder"] => Table,
        ["vocoder", "trusted_serials"] => VocoderSerials,
        // D-Star only (astar-dstar-perens): no ysf/nxdn/dmr path exists, so
        // the page cannot offer a software vocoder for an AMBE+2 mode.
        // Whether "software" is in this build is the loader's oracle check.
        ["vocoder", "dstar"] => Pick {
            options: &["dongle", "software"],
            nullable: true,
            exact_case: true,
        },

        // `[web]` and `[stream]`: restart-tier, because the public listener,
        // its certificate and the stream tap are all built once at start-up.
        // The loader checks every value (`validate_web`, `validate_stream`).
        ["web"] => Table,
        ["web", "public_bind" | "acme_http_bind" | "acme_email"] => Str { nullable: true },
        ["web", "public_hostnames"] => Hostnames,
        ["web", "acme_staging"] => Bool { nullable: true },
        // Sign-in (feature `auth`), restart-tier like the rest of `[web]`:
        // the sign-in service is built once at start-up.
        ["web", "anonymous_status" | "admin_on_public"] => Bool { nullable: true },
        ["web", "rp_id"] => Str { nullable: true },
        [
            "web",
            "session_idle_days" | "session_max_days" | "step_up_minutes",
        ] => Uint { nullable: true },
        ["web", "acme_challenge"] => Pick {
            options: &["http-01", "dns-01"],
            nullable: true,
            exact_case: true,
        },
        ["web", "dns"] => Table,
        ["web", "dns", "provider"] => Pick {
            options: &["porkbun"],
            nullable: false,
            exact_case: true,
        },
        ["web", "dns", "domain"] => NonEmptyStr { nullable: false },
        ["web", "dns", "host"] => Str { nullable: true },
        ["web", "dns", "ddns"] => Bool { nullable: true },
        ["web", "dns", "ddns_interval_secs" | "ttl"] => Uint { nullable: true },

        ["stream"] => Table,
        ["stream", "enabled"] => Bool { nullable: false },
        ["stream", "max_listeners" | "mp3_kbps" | "opus_kbps"] => Uint { nullable: true },

        // `[recording]`: restart-tier, because the node logger is built once
        // at start-up. `max_bytes` is a string ("2 GB"): the mirror writes
        // what the operator wrote, and `validate_recording` checks it.
        ["recording"] => Table,
        ["recording", "enabled"] => Bool { nullable: false },
        ["recording", "frames"] => Bool { nullable: true },
        ["recording", "dir"] => NonEmptyStr { nullable: true },
        ["recording", "max_bytes"] => Str { nullable: true },
        ["recording", "max_age_days"] => Uint { nullable: true },
        // `[packet_log]`: live, unlike `[recording]`; `validate_packet_log`
        // checks the folder and the 100 MB floor.
        ["packet_log"] => Table,
        ["packet_log", "debug_mode"] => Bool { nullable: false },
        ["packet_log", "dir"] => NonEmptyStr { nullable: true },
        ["packet_log", "max_bytes"] => Str { nullable: true },

        ["control", "bind"] => ControlBind,
        ["control", "loopback_login"] => Bool { nullable: true },

        ["credentials", "source"] => Pick {
            options: &["env", "control", "config", "file"],
            nullable: false,
            exact_case: false,
        },
        ["credentials", "file"] => CredentialsFile,

        _ => return None,
    })
}

/// Split a wire path on `.`, honouring one level of double quotes so that
/// `links."2000".shape` is three segments. `None` for anything malformed: an
/// empty segment, an unclosed quote, or text touching a closing quote.
fn split_path(path: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut rest = path;
    loop {
        let (segment, tail) = if let Some(after_quote) = rest.strip_prefix('"') {
            after_quote.split_once('"')?
        } else {
            match rest.find('.') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            }
        };
        if segment.is_empty() {
            return None;
        }
        segments.push(segment.to_string());
        if tail.is_empty() {
            return Some(segments);
        }
        rest = tail.strip_prefix('.')?;
        if rest.is_empty() {
            return None;
        }
    }
}

/// The canonical spelling of a segment list: only the `links` node segment
/// and a `[m17.reflectors]` name are quoted. Any other spelling of the same
/// keys is not in the vocabulary,
/// so every path has exactly one name on the wire and in `RedactedConfig`.
fn canonical(segments: &[&str]) -> String {
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let quoted = (i == 1 && segments[0] == "links")
                || (i == 2 && segments[0] == "m17" && segments[1] == "reflectors");
            if quoted {
                format!("\"{s}\"")
            } else {
                (*s).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// The one decision about whether a path is in the vocabulary. `Err` is the
/// sentence [`validate_field`] returns for it: an unknown path (including a
/// non-canonical spelling of a known one), an event name outside
/// `EVENT_NAMES`, a `links` node that is not digits, or a `[m17.reflectors]`
/// name the loader would refuse. [`tier`],
/// [`is_removable`] and [`toml_keys`] answer "outside" for exactly the same
/// paths, so no entry point can accept what another refuses.
fn vocab(path: &str) -> Result<(Vec<String>, Rule), String> {
    let outside = || format!("unknown setting \"{path}\"");
    let segments = split_path(path).ok_or_else(outside)?;
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    let rule = rule(&segs).ok_or_else(outside)?;
    if canonical(&segs) != path {
        return Err(outside());
    }
    if let ["announce", "events", name, ..] = segs.as_slice()
        && !EVENT_NAMES.contains(name)
    {
        return Err(format!("announce.events: unknown event {name:?}"));
    }
    if let ["links", node, ..] = segs.as_slice()
        && !node.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "links: {node:?} is not a node number (digits only)"
        ));
    }
    if let ["m17", "reflectors", name] = segs.as_slice()
        && let Err(e) = reflector_key(name)
    {
        return Err(format!("m17.reflectors.\"{name}\": {e}"));
    }
    Ok((segments, rule))
}

/// A `[m17.reflectors]` name, by the loader's rule (`validate_reflector_name`:
/// 3–8 ASCII letters, digits or `-`, any case), except that edge whitespace
/// is refused rather than trimmed: the page writes the key exactly as it
/// arrives, so `" M17-X"` would reach node.toml with its space.
fn reflector_key(name: &str) -> Result<(), String> {
    if name.trim() != name {
        return Err("A reflector name is letters, digits and '-' only.".to_string());
    }
    crate::m17_directory::validate_reflector_name(name).map(|_| ())
}

/// The segments and rule for a path inside the vocabulary.
fn segments_of(path: &str) -> Option<(Vec<String>, Rule)> {
    vocab(path).ok()
}

/// The tier of a vocabulary path; `None` when the page may not touch it.
#[must_use]
pub fn tier(path: &str) -> Option<Tier> {
    let (segments, _) = segments_of(path)?;
    let whole_live_table = matches!(
        segments.as_slice(),
        [only] if matches!(only.as_str(), "audio" | "bridge" | "packet_log" | "parrot")
    );
    // The joined segments, not the raw path: the two agree for every path
    // `vocab` accepts, and liveness should never hang on a spelling.
    let joined = segments.join(".");
    let live = whole_live_table || LIVE_PREFIXES.iter().any(|p| joined.starts_with(p));
    Some(if live { Tier::Live } else { Tier::Restart })
}

/// Paths that name a whole optional table and so accept `null`, which
/// removes the table from `node.toml`.
#[must_use]
pub fn is_removable(path: &str) -> bool {
    segments_of(path).is_some_and(|(_, rule)| rule == Rule::Table)
}

/// Wire path → TOML key segments, or `None` outside the vocabulary.
///
/// The wire renames `[secrets]` to `credentials` so that no key on the
/// settings routes carries the word "secret"; this is where the rename is
/// undone on the way back to the file.
#[must_use]
pub fn toml_keys(path: &str) -> Option<Vec<String>> {
    let (mut segments, _) = segments_of(path)?;
    if segments.first().is_some_and(|s| s == "credentials") {
        segments[0] = "secrets".to_string();
    }
    Some(segments)
}

/// One refused field, in the shape the route serialises as
/// `{"error": message, "field": field[, "acknowledge": key]}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FieldError {
    /// The wire path the sentence is about; the page pins the message under it.
    pub field: String,
    /// The sentence, already prefixed with the path where the loader's own
    /// sentences are, so the page shows exactly what a boot log would.
    pub message: String,
    /// Set when re-sending the same patch with this key in `acknowledge`
    /// would succeed: the page turns it into an inline confirm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acknowledge: Option<String>,
}

impl FieldError {
    fn new(field: &str, message: String) -> Self {
        Self {
            field: field.to_string(),
            message,
            acknowledge: None,
        }
    }

    fn needs(field: &str, message: &str, key: &str) -> Self {
        Self {
            field: field.to_string(),
            message: message.to_string(),
            acknowledge: Some(key.to_string()),
        }
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A non-zero `u64` written in digits. `str::parse::<u64>` alone would accept
/// `"+5"`, so the digits check comes first.
fn is_snowflake(s: &str) -> bool {
    is_digits(s) && s.parse::<u64>().is_ok_and(|n| n != 0)
}

/// `host:port` by shape only: a `SocketAddr` literal, or a non-empty host
/// (no scheme, no path, no bare IPv6 colons) and a port in 1–65535. The
/// registrar is resolved by `to_register` at boot, never here.
fn is_host_port(s: &str) -> bool {
    if s.parse::<SocketAddr>().is_ok() {
        return true;
    }
    if s.contains("://") || s.contains('/') {
        return false;
    }
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok_and(|p| p != 0)
}

/// `"a" or "b"` / `"a", "b", or "c"` — the loader's own list shape.
fn expected_list(options: &[&str]) -> String {
    match options {
        [] => String::new(),
        [only] => format!("{only:?}"),
        [a, b] => format!("{a:?} or {b:?}"),
        [init @ .., last] => {
            let head: Vec<String> = init.iter().map(|o| format!("{o:?}")).collect();
            format!("{}, or {last:?}", head.join(", "))
        }
    }
}

/// Every element as a `&str`, or `None` when the value is not an array of
/// strings.
fn strings_of(value: &Value) -> Option<Vec<&str>> {
    value.as_array()?.iter().map(Value::as_str).collect()
}

/// Type and value rule for one path. Null is allowed only for optional keys
/// and removable tables; a required key answers `"<path> is required"`.
///
/// Sentences copy the loader's where the loader has one, so an operator sees
/// the same words on the page as in a boot log.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per rule; splitting hides the table"
)]
pub fn validate_field(path: &str, value: &Value) -> Result<(), FieldError> {
    let (segments, rule) = vocab(path).map_err(|m| FieldError::new(path, m))?;
    let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
    if value.is_null() {
        return if rule.accepts_null() {
            Ok(())
        } else {
            Err(FieldError::new(path, format!("{path} is required")))
        };
    }
    let type_error = || FieldError::new(path, format!("{path}: expected {}", rule.type_name()));
    match rule {
        Rule::Table => Err(FieldError::new(
            path,
            format!("{path}: only null is accepted (it removes the table)"),
        )),
        Rule::Codec => {
            let s = value.as_str().ok_or_else(type_error)?;
            s.parse::<CodecPolicy>()
                .map(|_| ())
                .map_err(|m| FieldError::new(path, m))
        }
        Rule::Bind => {
            let s = value.as_str().ok_or_else(type_error)?;
            s.parse::<SocketAddr>()
                .map(|_| ())
                .map_err(|e| FieldError::new(path, format!("{path}: {e}")))
        }
        Rule::ControlBind => {
            let s = value.as_str().ok_or_else(type_error)?;
            let addr: SocketAddr = s
                .parse()
                .map_err(|e| FieldError::new(path, format!("{path}: {e}")))?;
            if addr.ip().is_unspecified() {
                return Err(FieldError::new(
                    path,
                    format!(
                        "{path}: a wildcard bind (0.0.0.0 or ::) is refused — it exposes an unauthenticated keying port on every interface"
                    ),
                ));
            }
            Ok(())
        }
        Rule::HostPort => {
            let s = value.as_str().ok_or_else(type_error)?;
            if is_host_port(s) {
                Ok(())
            } else {
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} is not host:port"),
                ))
            }
        }
        Rule::NodeNumber => {
            let s = value.as_str().ok_or_else(type_error)?;
            if is_digits(s) {
                Ok(())
            } else {
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} is not a node number (digits only)"),
                ))
            }
        }
        Rule::NodeNumbers => {
            for item in strings_of(value).ok_or_else(type_error)? {
                if !is_digits(item) {
                    return Err(FieldError::new(
                        path,
                        format!("{path}: {item:?} is not a node number (digits only)"),
                    ));
                }
            }
            Ok(())
        }
        Rule::Snowflake => {
            let s = value.as_str().ok_or_else(type_error)?;
            if is_snowflake(s) {
                Ok(())
            } else {
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} is not a Discord id (a snowflake)"),
                ))
            }
        }
        Rule::Snowflakes => {
            for item in strings_of(value).ok_or_else(type_error)? {
                if !is_snowflake(item) {
                    return Err(FieldError::new(
                        path,
                        format!("{path}: {item:?} is not a Discord user id (a snowflake)"),
                    ));
                }
            }
            Ok(())
        }
        Rule::Pick {
            options,
            exact_case,
            ..
        } => {
            let s = value.as_str().ok_or_else(type_error)?;
            let hit = options.iter().any(|o| {
                if exact_case {
                    *o == s
                } else {
                    o.eq_ignore_ascii_case(s)
                }
            });
            if hit {
                Ok(())
            } else {
                Err(FieldError::new(
                    path,
                    format!(
                        "{path}: unknown value {s:?} (expected {})",
                        expected_list(options)
                    ),
                ))
            }
        }
        Rule::Str { .. } => value.as_str().map(|_| ()).ok_or_else(type_error),
        Rule::NonEmptyStr { .. } => {
            let s = value.as_str().ok_or_else(type_error)?;
            if s.is_empty() {
                Err(FieldError::new(path, format!("{path}: must not be empty")))
            } else {
                Ok(())
            }
        }
        Rule::Bool { .. } => value.as_bool().map(|_| ()).ok_or_else(type_error),
        Rule::Uint { .. } => {
            if !value.is_number() {
                return Err(type_error());
            }
            if value.as_i64().is_some_and(|n| n >= 0) {
                Ok(())
            } else {
                Err(FieldError::new(
                    path,
                    format!("{path}: must be a non-negative integer"),
                ))
            }
        }
        Rule::Num => value.as_f64().map(|_| ()).ok_or_else(type_error),
        Rule::StateFile => {
            let s = value.as_str().ok_or_else(type_error)?;
            match state_file_parent_problem(Path::new(s)) {
                None => Ok(()),
                Some(problem) => Err(FieldError::new(path, format!("{path}: {problem}"))),
            }
        }
        Rule::CredentialsFile => {
            let s = value.as_str().ok_or_else(type_error)?;
            let lower = s.to_ascii_lowercase();
            if s.is_empty() {
                Err(FieldError::new(path, format!("{path}: must not be empty")))
            } else if Path::new(s)
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                // Where the file may live is the store's check, which knows
                // the config's directory; `..` is refused before that so no
                // spelling can climb out of it.
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} must not contain a .. component"),
                ))
            } else if Path::new(s).file_name() == Some(OsStr::new("node.toml")) {
                // `save_file` would read-modify-write the config itself and
                // leave the password in the file that must stay secret-free.
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} is the node's own config; credentials never go in it"),
                ))
            } else if ["secret", "password", "token"]
                .iter()
                .any(|w| lower.contains(w))
            {
                // The path is shown on GET /settings/state, which must never
                // carry these words.
                Err(FieldError::new(
                    path,
                    format!(
                        "{path}: {s:?} would put the word on the settings page; name it like credentials.toml"
                    ),
                ))
            } else {
                Ok(())
            }
        }
        Rule::LinkShape => {
            let s = value.as_str().ok_or_else(type_error)?;
            if s.eq_ignore_ascii_case("standard") || s.eq_ignore_ascii_case("wt-guest") {
                Ok(())
            } else {
                let node = segs.get(1).copied().unwrap_or_default();
                Err(FieldError::new(
                    path,
                    format!(
                        "links.\"{node}\": unknown shape \"{s}\" (expected \"standard\" or \"wt-guest\")"
                    ),
                ))
            }
        }
        Rule::M17Directory => {
            let s = value.as_str().ok_or_else(type_error)?;
            crate::m17_directory::check_directory(Some(s))
                .map(|_| ())
                .map_err(|m| FieldError::new(path, m))
        }
        Rule::CodecDirs => {
            let dirs: Vec<String> = strings_of(value)
                .ok_or_else(type_error)?
                .into_iter()
                .map(str::to_string)
                .collect();
            crate::m17_directory::check_codec_dirs(&dirs).map_err(|m| FieldError::new(path, m))
        }
        Rule::TtsBinary => {
            let s = value.as_str().ok_or_else(type_error)?;
            if !Path::new(s).is_absolute() {
                Err(FieldError::new(
                    path,
                    format!(
                        "{path}: {s:?} is not an absolute path; name the program by its full path"
                    ),
                ))
            } else if !std::fs::metadata(s).is_ok_and(|m| m.is_file()) {
                Err(FieldError::new(
                    path,
                    format!("{path}: {s:?} is not an existing file"),
                ))
            } else {
                Ok(())
            }
        }
        Rule::M17Reflector => {
            let s = value.as_str().ok_or_else(type_error)?;
            crate::m17_directory::parse_host_port(s)
                .map(|_| ())
                .map_err(|e| FieldError::new(path, format!("{path}: {e}")))
        }
        Rule::VocoderSerials => {
            for item in strings_of(value).ok_or_else(type_error)? {
                crate::config::check_vocoder_serial(item).map_err(|m| FieldError::new(path, m))?;
            }
            Ok(())
        }
        Rule::Hostnames => strings_of(value).map(|_| ()).ok_or_else(type_error),
    }
}

/// Paths whose disk value differs from the running mirror, split by tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Pending {
    /// Differences the node will read at its next start. Sorted.
    pub restart: Vec<String>,
    /// Differences `POST /settings` can apply now. Sorted.
    pub apply: Vec<String>,
}

/// Flat map of wire path → JSON value. Every key present on disk, plus every
/// key serde fills from a default (`allowed_nodes`, `approvers`, the `enabled`
/// booleans, `portal.credential_env`, …) — the parsed struct cannot tell those
/// apart, and the page shows the value the node will use. Two further
/// exceptions: a `[links."<n>"]` table with no `shape` appears as `null` so
/// the link's presence is visible, and `wireguard.key_ref` shows the default
/// the stack will use when `secret_ref` is not written, because the page's
/// summary is about what will happen, not what was typed.
///
/// `[secrets].secret` is never read; `[secrets]` surfaces as `credentials.*`
/// and `[wireguard].secret_ref` as `wireguard.key_ref`, so no key on the wire
/// carries the word.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedactedConfig(pub BTreeMap<String, Value>);

fn put<V: Into<Value>>(m: &mut BTreeMap<String, Value>, path: &str, v: V) {
    m.insert(path.to_string(), v.into());
}

fn put_opt<V: Into<Value>>(m: &mut BTreeMap<String, Value>, path: &str, v: Option<V>) {
    if let Some(v) = v {
        put(m, path, v);
    }
}

/// An `f32` as the JSON number an operator wrote. `f64::from` is exact, and
/// that is the problem: `gain_db = -10.3` is stored as the nearest `f32`, and
/// widening it gives `-10.300000190734863`, which the page would render and
/// then fail to match against the `-10.3` it sends back. `f32`'s `Display` is
/// the shortest decimal that round-trips, so going through it recovers what
/// was typed. A non-finite value becomes `null` (TOML's `nan`/`inf` have no
/// JSON spelling).
fn f32_json(f: f32) -> Value {
    f.to_string()
        .parse::<f64>()
        .map_or(Value::Null, Value::from)
}

impl RedactedConfig {
    /// The wire view of a parsed config.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "one flat listing of every section, in node.toml order"
    )]
    pub fn from_config(cfg: &NodeFileConfig) -> Self {
        let mut m = BTreeMap::new();
        put_opt(&mut m, "codec_policy", cfg.codec_policy_raw());

        let l = &cfg.listener;
        put(&mut m, "listener.bind", l.bind.as_str());
        put(&mut m, "listener.answer", l.answer.as_str());
        put(&mut m, "listener.max_calls", l.max_calls);
        put(&mut m, "listener.auth", l.auth.as_str());
        put(&mut m, "listener.allowed_nodes", l.allowed_nodes.clone());

        if let Some(r) = &cfg.register {
            put(&mut m, "register.peer", r.peer.as_str());
            put(&mut m, "register.node_id", r.node_id.as_str());
        }
        if let Some(a) = &cfg.audio {
            put_opt(&mut m, "audio.input", a.input.as_deref());
            put_opt(&mut m, "audio.output", a.output.as_deref());
            put_opt(&mut m, "audio.backend", a.backend.as_deref());
        }
        if let Some(b) = &cfg.bridge {
            put(&mut m, "bridge.mode", b.mode.as_str());
            put(&mut m, "bridge.mix_minus", b.mix_minus);
            put(&mut m, "bridge.include_local_radio", b.include_local_radio);
        }
        if let Some(p) = &cfg.parrot {
            put_opt(&mut m, "parrot.playback_delay_ms", p.playback_delay_ms);
            put_opt(&mut m, "parrot.silence_gap_ms", p.silence_gap_ms);
            put_opt(
                &mut m,
                "parrot.vox_threshold_db",
                p.vox_threshold_db.map(f32_json),
            );
        }
        if let Some(a) = &cfg.announce {
            put(&mut m, "announce.enabled", a.enabled);
            put_opt(&mut m, "announce.sounds_dir", a.sounds_dir.as_deref());
            put_opt(
                &mut m,
                "announce.mixunder_default_gain_db",
                a.mixunder_default_gain_db.map(f32_json),
            );
            put_opt(&mut m, "announce.id_interval_secs", a.id_interval_secs);
            put_opt(&mut m, "announce.id_mode", a.id_mode.as_deref());
            put_opt(&mut m, "announce.cw_wpm", a.cw_wpm);
            put_opt(&mut m, "announce.cw_tone_hz", a.cw_tone_hz.map(f32_json));
            put_opt(&mut m, "announce.cw_keys_when_idle", a.cw_keys_when_idle);
            put_opt(&mut m, "announce.join_template", a.join_template.as_deref());
            put_opt(
                &mut m,
                "announce.link_connect_template",
                a.link_connect_template.as_deref(),
            );
            put_opt(
                &mut m,
                "announce.link_disconnect_template",
                a.link_disconnect_template.as_deref(),
            );
            if let Some(t) = &a.tts {
                put(&mut m, "announce.tts.enabled", t.enabled);
                put_opt(&mut m, "announce.tts.binary", t.binary.as_deref());
                put_opt(&mut m, "announce.tts.voice", t.voice.as_deref());
                put_opt(&mut m, "announce.tts.timeout_ms", t.timeout_ms);
                put_opt(&mut m, "announce.tts.gain_db", t.gain_db.map(f32_json));
            }
            if let Some(events) = &a.events {
                for (name, e) in events {
                    put(
                        &mut m,
                        &format!("announce.events.{name}.enabled"),
                        e.enabled,
                    );
                    put_opt(
                        &mut m,
                        &format!("announce.events.{name}.destination"),
                        e.destination.as_deref(),
                    );
                }
            }
        }
        if let Some(d) = &cfg.discord {
            put(&mut m, "discord.guild_id", d.guild_id.as_str());
            put(&mut m, "discord.channel_id", d.channel_id.as_str());
            put(&mut m, "discord.join_at_start", d.join_at_start);
            put(&mut m, "discord.announce_on_join", d.announce_on_join);
            put(&mut m, "discord.approvers", d.approvers.clone());
            put_opt(&mut m, "discord.state_file", d.state_file.as_deref());
            put_opt(&mut m, "discord.mic_timeout_secs", d.mic_timeout_secs);
            put_opt(&mut m, "discord.mic_silence_secs", d.mic_silence_secs);
        }
        if let Some(w) = &cfg.web {
            put_opt(&mut m, "web.public_bind", w.public_bind.as_deref());
            put_opt(&mut m, "web.public_hostnames", w.public_hostnames.clone());
            put_opt(&mut m, "web.acme_email", w.acme_email.as_deref());
            put(&mut m, "web.acme_staging", w.acme_staging);
            put_opt(&mut m, "web.anonymous_status", w.anonymous_status);
            put_opt(&mut m, "web.admin_on_public", w.admin_on_public);
            put_opt(&mut m, "web.rp_id", w.rp_id.as_deref());
            put(&mut m, "web.session_idle_days", w.session_idle_days);
            put(&mut m, "web.session_max_days", w.session_max_days);
            put(&mut m, "web.step_up_minutes", w.step_up_minutes);
            put_opt(&mut m, "web.acme_http_bind", w.acme_http_bind.as_deref());
            put_opt(
                &mut m,
                "web.acme_challenge",
                w.acme_challenge.map(crate::config::AcmeChallenge::as_str),
            );
            if let Some(d) = &w.dns {
                put(&mut m, "web.dns.provider", d.provider.as_str());
                put(&mut m, "web.dns.domain", d.domain.as_str());
                put_opt(&mut m, "web.dns.host", d.host.as_deref());
                put(&mut m, "web.dns.ddns", d.ddns);
                put(&mut m, "web.dns.ddns_interval_secs", d.ddns_interval_secs);
                put(&mut m, "web.dns.ttl", d.ttl);
            }
        }
        if let Some(st) = &cfg.stream {
            put(&mut m, "stream.enabled", st.enabled);
            put(&mut m, "stream.max_listeners", st.max_listeners);
            put(&mut m, "stream.mp3_kbps", st.mp3_kbps);
            put(&mut m, "stream.opus_kbps", st.opus_kbps);
        }
        if let Some(r) = &cfg.recording {
            put(&mut m, "recording.enabled", r.enabled);
            put(
                &mut m,
                "recording.dir",
                r.dir.to_string_lossy().into_owned(),
            );
            put(&mut m, "recording.max_bytes", r.max_bytes.text.as_str());
            put(&mut m, "recording.max_age_days", r.max_age_days);
            put(&mut m, "recording.frames", r.frames);
        }
        if let Some(p) = &cfg.packet_log {
            put(&mut m, "packet_log.debug_mode", p.debug_mode);
            put(
                &mut m,
                "packet_log.dir",
                p.dir.to_string_lossy().into_owned(),
            );
            put(&mut m, "packet_log.max_bytes", p.max_bytes.text.as_str());
        }
        put(&mut m, "control.bind", cfg.control.bind.as_str());
        put(&mut m, "control.loopback_login", cfg.control.loopback_login);
        put(&mut m, "credentials.source", cfg.secrets.source.as_str());
        put_opt(&mut m, "credentials.file", cfg.secrets.file.as_deref());
        if let Some(w) = &cfg.wireguard {
            put(&mut m, "wireguard.enabled", w.enabled);
            put(&mut m, "wireguard.address", w.address.as_str());
            put(
                &mut m,
                "wireguard.peer_public_key",
                w.peer_public_key.as_str(),
            );
            put(&mut m, "wireguard.endpoint", w.endpoint.as_str());
            put(&mut m, "wireguard.allowed_ips", w.allowed_ips.clone());
            put_opt(&mut m, "wireguard.keepalive_secs", w.keepalive_secs);
            put(
                &mut m,
                "wireguard.key_ref",
                w.secret_ref.as_deref().unwrap_or("WIREGUARD_PRIVATE_KEY"),
            );
            put_opt(
                &mut m,
                "wireguard.also_bind_udp",
                w.also_bind_udp.as_deref(),
            );
        }
        if let Some(p) = &cfg.portal {
            put(&mut m, "portal.user", p.user.as_str());
            put(&mut m, "portal.node", p.node.as_str());
            put(&mut m, "portal.credential_env", p.credential_env.as_str());
        }
        for (node, link) in &cfg.links {
            m.insert(
                format!("links.\"{node}\".shape"),
                link.shape.as_deref().map_or(Value::Null, Value::from),
            );
        }
        if let Some(d) = &cfg.dtmf {
            put(&mut m, "dtmf.enabled", d.enabled);
            put_opt(
                &mut m,
                "dtmf.inter_digit_timeout_ms",
                d.inter_digit_timeout_ms,
            );
        }
        if let Some(s) = &cfg.m17 {
            put(&mut m, "m17.enabled", s.enabled);
            put_opt(&mut m, "m17.directory", s.directory.as_deref());
            // An empty list and an absent key are the same config: no extra
            // directory. Carrying `[]` would put a restart tag on turning M17
            // on from the page, for a change that changes nothing.
            if !s.codec_dirs.is_empty() {
                put(&mut m, "m17.codec_dirs", s.codec_dirs.clone());
            }
            for (name, addr) in &s.reflectors {
                put(&mut m, &format!("m17.reflectors.\"{name}\""), addr.as_str());
            }
        }
        // Present whenever the section is, empty list included: `[vocoder]`
        // holds nothing else, so this one key is how the page sees it exists.
        if let Some(v) = &cfg.vocoder {
            put(&mut m, "vocoder.trusted_serials", v.trusted_serials.clone());
            put_opt(&mut m, "vocoder.dstar", v.dstar.as_deref());
        }
        Self(m)
    }

    /// The value at a wire path, if present.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.0.get(path)
    }

    /// Every path present in either side with a different value, classified
    /// by [`tier`]. A path outside the vocabulary (a hand-edited
    /// `wireguard.*`, an unknown event name) counts as restart: the page
    /// cannot apply what it cannot name. Both lists come out sorted because
    /// the union is a `BTreeSet`.
    #[must_use]
    pub fn diff(disk: &Self, running: &Self) -> Pending {
        let mut pending = Pending::default();
        let paths: BTreeSet<&String> = disk.0.keys().chain(running.0.keys()).collect();
        for path in paths {
            if disk.0.get(path) == running.0.get(path) {
                continue;
            }
            match tier(path) {
                Some(Tier::Live) => pending.apply.push(path.clone()),
                Some(Tier::Restart) | None => pending.restart.push(path.clone()),
            }
        }
        pending
    }
}

/// 16 lowercase hex digits of `std::hash::DefaultHasher` over the bytes.
///
/// `DefaultHasher::new()` uses fixed keys, so the value is stable across
/// requests and across restarts of the daemon — which is all an etag needs.
/// It is not a content-addressable digest and is never compared to anything
/// but another `etag_of`.
#[must_use]
pub fn etag_of(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The body of `POST /settings`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SettingsPatch {
    /// [`etag_of`] the bytes the page last saw; a mismatch is a 409, which is
    /// how a `sudo -e` edit becomes a conflict rather than a clobber.
    pub etag: String,
    /// Wire path → new value; `null` removes an optional key or a whole
    /// removable table.
    #[serde(default)]
    pub patch: BTreeMap<String, Value>,
    /// Acknowledgement keys the operator has confirmed, e.g.
    /// [`ACK_CONTROL_BIND_PUBLIC`].
    #[serde(default)]
    pub acknowledge: Vec<String>,
}

/// What the route knows about the request that the rules need.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteContext {
    /// The request's socket peer was a loopback address (and no proxy
    /// header said otherwise). Only such a request may change
    /// `[control].bind`: a caller on the LAN must not be able to widen the
    /// very bind it arrived through, whatever `Host` it sends.
    pub loopback_peer: bool,
}

/// `[control].bind` moves off loopback.
pub const ACK_CONTROL_BIND_PUBLIC: &str = "control.bind.public";
/// `listener.auth = "off"` with no `allowed_nodes`.
pub const ACK_LISTENER_AUTH_OFF: &str = "listener.auth.off";
/// `dtmf.enabled` turns on.
pub const ACK_DTMF_ENABLED: &str = "dtmf.enabled";
/// `bridge.include_local_radio` turns on.
pub const ACK_LOCAL_RADIO: &str = "bridge.include_local_radio";
/// `announce.cw_keys_when_idle` goes from an explicit `false` to `true`.
pub const ACK_CW_IDLE: &str = "announce.cw_keys_when_idle";
/// `discord.mic_timeout_secs` becomes `0`.
pub const ACK_MIC_TIMEOUT_OFF: &str = "discord.mic_timeout.off";
/// An id leaves `discord.approvers`.
pub const ACK_APPROVER_REMOVED: &str = "discord.approvers.remove";
/// An entry is added to `m17.codec_dirs`: new code the node may load.
pub const ACK_M17_CODEC_DIRS: &str = "m17.codec_dirs";
/// A serial is added to `vocoder.trusted_serials`: a USB port the node may
/// now open, which on a radio's CAT/PTT cable keys the transmitter.
pub const ACK_VOCODER_SERIALS: &str = "vocoder.trusted_serials";
/// `announce.tts.binary` is set to a new program.
pub const ACK_TTS_BINARY: &str = "announce.tts.binary";

fn flag(c: &RedactedConfig, path: &str) -> bool {
    c.get(path).and_then(Value::as_bool).unwrap_or(false)
}

/// `auth = "off"` with an absent or empty allowlist: every caller admitted.
fn admits_everyone(c: &RedactedConfig) -> bool {
    let off = c
        .get("listener.auth")
        .and_then(Value::as_str)
        .is_some_and(|a| a.eq_ignore_ascii_case("off"));
    let no_list = c
        .get("listener.allowed_nodes")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    off && no_list
}

/// The library default for `cw_keys_when_idle` is `true`, so an absent key
/// means keying.
fn cw_keys_when_idle(c: &RedactedConfig) -> bool {
    c.get("announce.cw_keys_when_idle")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn approver_ids(c: &RedactedConfig) -> Vec<&str> {
    c.get("discord.approvers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn codec_dirs(c: &RedactedConfig) -> Vec<&str> {
    c.get("m17.codec_dirs")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn vocoder_serials(c: &RedactedConfig) -> Vec<&str> {
    c.get("vocoder.trusted_serials")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn tts_binary(c: &RedactedConfig) -> Option<&str> {
    c.get("announce.tts.binary").and_then(Value::as_str)
}

/// Cross-field rules evaluated on the patched config. First failure wins, in
/// the order of the table in the design: the control bind, then the four
/// on-air switches, then the two Discord ones, then the two `[m17]` ones (the
/// section's opt-in, and the one code-loading key), then `[vocoder]`.
///
/// Every rule asks on the way *up* only — `before` is what is on disk now,
/// `after` is what the patch would write — so a node already in the risky
/// state is not re-asked every time an unrelated card is saved.
pub fn check_acknowledgements(
    before: &RedactedConfig,
    after: &RedactedConfig,
    patch: &SettingsPatch,
    ctx: &WriteContext,
) -> Result<(), FieldError> {
    let acked = |key: &str| patch.acknowledge.iter().any(|k| k == key);

    let bind_before = before.get("control.bind");
    let bind_after = after.get("control.bind");
    if bind_after != bind_before {
        if !ctx.loopback_peer {
            return Err(FieldError::new(
                "control.bind",
                "control.bind: may only be changed by a request from this host (a loopback peer)"
                    .to_string(),
            ));
        }
        let public = bind_after
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<SocketAddr>().ok())
            .is_some_and(|a| !a.ip().is_loopback() && !a.ip().is_unspecified());
        if public && !acked(ACK_CONTROL_BIND_PUBLIC) {
            return Err(FieldError::needs(
                "control.bind",
                "control.bind: a non-loopback bind needs the control.bind.public acknowledgement",
                ACK_CONTROL_BIND_PUBLIC,
            ));
        }
    }

    if admits_everyone(after) && !admits_everyone(before) && !acked(ACK_LISTENER_AUTH_OFF) {
        return Err(FieldError::needs(
            "listener.auth",
            "listener.auth = \"off\" with no allowed_nodes admits every caller; it needs the listener.auth.off acknowledgement",
            ACK_LISTENER_AUTH_OFF,
        ));
    }

    if flag(after, "dtmf.enabled") && !flag(before, "dtmf.enabled") && !acked(ACK_DTMF_ENABLED) {
        return Err(FieldError::needs(
            "dtmf.enabled",
            "dtmf.enabled lets any connected member command links; it needs the dtmf.enabled acknowledgement",
            ACK_DTMF_ENABLED,
        ));
    }

    if flag(after, "bridge.include_local_radio")
        && !flag(before, "bridge.include_local_radio")
        && !acked(ACK_LOCAL_RADIO)
    {
        return Err(FieldError::needs(
            "bridge.include_local_radio",
            "bridge.include_local_radio puts the local microphone on the air; it needs the bridge.include_local_radio acknowledgement",
            ACK_LOCAL_RADIO,
        ));
    }

    // Only while `[announce]` survives the patch: `announce.enabled` is
    // always present when the section is (serde default), and removing the
    // section turns every announcement off, so the confirm would be false.
    if after.get("announce.enabled").is_some()
        && cw_keys_when_idle(after)
        && !cw_keys_when_idle(before)
        && !acked(ACK_CW_IDLE)
    {
        return Err(FieldError::needs(
            "announce.cw_keys_when_idle",
            "announce.cw_keys_when_idle keys the transmitter for an idle ID; it needs the announce.cw_keys_when_idle acknowledgement",
            ACK_CW_IDLE,
        ));
    }

    let mic_off =
        |c: &RedactedConfig| c.get("discord.mic_timeout_secs").and_then(Value::as_u64) == Some(0);
    if mic_off(after) && !mic_off(before) && !acked(ACK_MIC_TIMEOUT_OFF) {
        return Err(FieldError::needs(
            "discord.mic_timeout_secs",
            "discord.mic_timeout_secs = 0 removes the hard time-out on the open mic; it needs the discord.mic_timeout.off acknowledgement",
            ACK_MIC_TIMEOUT_OFF,
        ));
    }

    let kept = approver_ids(after);
    if approver_ids(before).iter().any(|id| !kept.contains(id)) && !acked(ACK_APPROVER_REMOVED) {
        return Err(FieldError::needs(
            "discord.approvers",
            "removing an approver leaves their slash commands registered in Discord; it needs the discord.approvers.remove acknowledgement",
            ACK_APPROVER_REMOVED,
        ));
    }

    // `[m17]` is the opt-in, and `enabled` defaults to true once the section
    // exists, so a save that creates the section must say which it wants:
    // adding one reflector must never quietly let every approved Discord
    // member present a callsign to it. Not an acknowledgement — no
    // confirmation stands in for saying it.
    if before.get("m17.enabled").is_none()
        && after.get("m17.enabled").is_some()
        && !patch.patch.contains_key("m17.enabled")
    {
        return Err(FieldError::new(
            "m17.enabled",
            "m17.enabled: a save that creates [m17] must set m17.enabled too — the section turns M17 on by default"
                .to_string(),
        ));
    }

    check_code_keys(before, after, &acked)
}

/// The two keys that decide what code the node runs, last in table order.
fn check_code_keys(
    before: &RedactedConfig,
    after: &RedactedConfig,
    acked: &impl Fn(&str) -> bool,
) -> Result<(), FieldError> {
    // Asks on the way up only, like the others: a directory new to the list
    // is new code the node may load and run at its next start. Narrowing or
    // reordering the list loads nothing that was not already trusted.
    let trusted = codec_dirs(before);
    if codec_dirs(after).iter().any(|d| !trusted.contains(d)) && !acked(ACK_M17_CODEC_DIRS) {
        return Err(FieldError::needs(
            "m17.codec_dirs",
            "m17.codec_dirs loads a libcodec2 from these directories into the node at its next start; it needs the m17.codec_dirs acknowledgement",
            ACK_M17_CODEC_DIRS,
        ));
    }

    // The other code-running key: a new program here is run for every
    // spoken announcement. Removing it falls back to the file's default.
    let new_binary = tts_binary(after);
    if new_binary.is_some() && new_binary != tts_binary(before) && !acked(ACK_TTS_BINARY) {
        return Err(FieldError::needs(
            "announce.tts.binary",
            "announce.tts.binary is a program the node runs for every spoken announcement; it needs the announce.tts.binary acknowledgement",
            ACK_TTS_BINARY,
        ));
    }

    check_vocoder_ack(before, after, &acked)
}

/// The `[vocoder]` rule of [`check_acknowledgements`], on its own so that
/// function stays one screen. On the way up only: a serial new to the list
/// is a port the node may open, and the settings page cannot probe it the
/// way `astar-server vocoder add` does. Removing a serial opens nothing.
fn check_vocoder_ack(
    before: &RedactedConfig,
    after: &RedactedConfig,
    acked: &dyn Fn(&str) -> bool,
) -> Result<(), FieldError> {
    let known = vocoder_serials(before);
    if vocoder_serials(after).iter().any(|s| !known.contains(s)) && !acked(ACK_VOCODER_SERIALS) {
        return Err(FieldError::needs(
            "vocoder.trusted_serials",
            "vocoder.trusted_serials: a serial new to the list lets the node open that USB port as a vocoder, and opening a radio's CAT/PTT cable keys it; it needs the vocoder.trusted_serials acknowledgement",
            ACK_VOCODER_SERIALS,
        ));
    }
    Ok(())
}

/// The daemon's own loader run on the exact bytes about to be written.
///
/// Parses with [`NodeFileConfig::from_toml_str_redacted`] — so a parse error
/// carries toml's message and never its rendered snippet — then runs the
/// builders `main.rs` runs at boot that only parse: `to_inbound`,
/// `to_bridge_config`, the `[control].bind` parse and the state-file parent
/// check.
///
/// Two boot-time builders are deliberately left out, because a save must
/// never wait on DNS:
///
/// * **Not `to_register`**: it is `RegisterConfig::resolve` with a
///   sixty-second budget.
/// * **Not `to_link_transport`**: it builds an `astar_wireguard::WgLinkConfig`,
///   whose `new` resolves `endpoint` with `to_socket_addrs` — on a node whose
///   endpoint is a hostname that is a resolver round-trip under the store's
///   write lock, and an outright failure offline. `wireguard.*` is outside
///   the vocabulary, so no patch can change that section and its validity is
///   exactly what it was when the node booted.
///
/// # Errors
/// The first refusal, in the loader's own words. Nothing is written when
/// this returns `Err`.
pub fn oracle(text: &str) -> Result<NodeFileConfig, String> {
    let cfg = NodeFileConfig::from_toml_str_redacted(text)?;
    cfg.to_inbound()?;
    cfg.to_bridge_config()?;
    cfg.control
        .bind
        .parse::<SocketAddr>()
        .map_err(|e| format!("control.bind: {e}"))?;
    if let Some(state_file) = cfg.discord.as_ref().and_then(|d| d.state_file.as_deref())
        && let Some(problem) = state_file_parent_problem(Path::new(state_file))
    {
        return Err(format!("discord.state_file: {problem}"));
    }
    Ok(cfg)
}

/// A JSON value as the TOML value that will be written. `None` for a shape
/// [`validate_field`] never lets through (an object, or a list with a
/// non-string in it).
fn to_toml(value: &Value) -> Option<toml_edit::Value> {
    Some(match value {
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml_edit::Value::from(i),
            None => toml_edit::Value::from(n.as_f64()?),
        },
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Array(items) => {
            let strings: Vec<&str> = items.iter().map(Value::as_str).collect::<Option<_>>()?;
            toml_edit::Value::Array(strings.into_iter().collect())
        }
        Value::Null | Value::Object(_) => return None,
    })
}

/// Set `keys` to `value`, creating the tables on the way down.
///
/// A missing intermediate becomes an implicit table — `toml_edit` writes its
/// header only if it ends up holding values of its own, which is how
/// `links."2000".shape` renders as `[links.2000]` and not an empty `[links]`
/// above it — or an inline table when its parent is one, so a patch inside
/// `answered = { … }` stays on that line. An existing scalar is replaced
/// in place with its decor copied back, which is what keeps the alignment
/// spaces and the trailing `# comment` on the line the operator wrote.
fn set_key(
    root: &mut Table,
    keys: &[String],
    value: toml_edit::Value,
    path: &str,
) -> Result<(), FieldError> {
    let (leaf, parents) = keys
        .split_last()
        .expect("toml_keys never returns an empty path");
    let mut cur: &mut dyn TableLike = root;
    let mut inline = false;
    for key in parents {
        if !cur.contains_key(key) {
            let fresh = if inline {
                Item::Value(toml_edit::Value::InlineTable(InlineTable::new()))
            } else {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            };
            cur.insert(key, fresh);
        }
        let item = cur
            .get_mut(key)
            .expect("present: it was either found or just inserted");
        let next_inline = item.is_inline_table();
        cur = item.as_table_like_mut().ok_or_else(|| {
            FieldError::new(path, format!("{path}: {key} is not a table in node.toml"))
        })?;
        inline = next_inline;
    }
    match cur.get_mut(leaf).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        None => {
            cur.insert(leaf, Item::Value(value));
        }
    }
    Ok(())
}

/// Remove `keys` if present. A missing parent means there is nothing to
/// remove, which is a success: the patch asked for absence and has it.
fn remove_key(root: &mut Table, keys: &[String]) {
    let (leaf, parents) = keys
        .split_last()
        .expect("toml_keys never returns an empty path");
    let mut cur: &mut dyn TableLike = root;
    for key in parents {
        match cur.get_mut(key).and_then(Item::as_table_like_mut) {
            Some(table) => cur = table,
            None => return,
        }
    }
    cur.remove(leaf);
}

fn unknown(path: &str) -> FieldError {
    FieldError::new(path, format!("unknown setting \"{path}\""))
}

/// Apply every entry to `text` with `toml_edit`, preserving comments, key
/// order, formatting and every key the vocabulary does not know about.
///
/// Every entry is validated before the document is touched, so a patch with
/// one bad field changes nothing. `null` removes an optional key or a whole
/// removable table; a missing table is created; nothing is ever regenerated
/// from a model, because the operator's file is the operator's.
///
/// # Errors
/// The first [`validate_field`] refusal; or, for a `text` that `toml_edit`
/// cannot parse (which the loader would have refused at boot), a
/// `FieldError` on the field `node.toml` carrying only `toml_edit`'s message.
pub fn apply_patch(text: &str, patch: &BTreeMap<String, Value>) -> Result<String, FieldError> {
    for (path, value) in patch {
        validate_field(path, value)?;
    }
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        FieldError::new("node.toml", format!("node.toml: {}", e.message()))
    })?;
    for (path, value) in patch {
        let keys = toml_keys(path).ok_or_else(|| unknown(path))?;
        if value.is_null() {
            remove_key(doc.as_table_mut(), &keys);
        } else {
            let toml_value = to_toml(value)
                .ok_or_else(|| FieldError::new(path, format!("{path}: unsupported value")))?;
            set_key(doc.as_table_mut(), &keys, toml_value, path)?;
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::template::NODE_TOML_TEMPLATE;

    const EXAMPLE: &str = include_str!("../../../deploy/node.toml.example");

    #[test]
    fn the_tier_table_is_pinned() {
        const LIVE: &[&str] = &[
            "bridge",
            "bridge.mode",
            "bridge.mix_minus",
            "bridge.include_local_radio",
            "parrot",
            "parrot.playback_delay_ms",
            "parrot.silence_gap_ms",
            "parrot.vox_threshold_db",
            "audio",
            "audio.input",
            "audio.output",
        ];
        const RESTART: &[&str] = &[
            "codec_policy",
            "listener.bind",
            "listener.answer",
            "listener.max_calls",
            "listener.auth",
            "listener.allowed_nodes",
            "register",
            "register.peer",
            "register.node_id",
            "audio.backend",
            "announce",
            "announce.enabled",
            "announce.id_mode",
            "announce.cw_keys_when_idle",
            "announce.tts",
            "announce.tts.enabled",
            "announce.events",
            "announce.events.answered",
            "announce.events.answered.enabled",
            "announce.events.answered.destination",
            "discord",
            "discord.guild_id",
            "discord.channel_id",
            "discord.approvers",
            "discord.state_file",
            "discord.mic_timeout_secs",
            "portal",
            "portal.user",
            "portal.node",
            "portal.credential_env",
            "links.\"2000\"",
            "links.\"2000\".shape",
            "dtmf",
            "dtmf.enabled",
            "dtmf.inter_digit_timeout_ms",
            "control.bind",
            "credentials.source",
            "credentials.file",
        ];
        const OUTSIDE: &[&str] = &[
            "wireguard.key_ref",
            "wireguard.secret_ref",
            "secrets.source",
            "secrets.secret",
            "credentials.secret",
            "discord.model",
            "discord.latency",
            "discord.token",
            "listener",
            "control",
            "",
            "listener.",
            ".bind",
            "links.\"2000\".secret",
            "links.\"20\"00.shape",
            // Refused by validate_field as paths, so outside here too.
            "links.\"20a0\".shape",
            "links.\"20a0\"",
            "announce.events.peer_connected.enabled",
            "announce.events.peer_connected",
            // Only the canonical spelling is in the vocabulary.
            "\"bridge\".mode",
            "links.2000.shape",
        ];
        for p in LIVE {
            assert_eq!(tier(p), Some(Tier::Live), "{p}");
        }
        for p in RESTART {
            assert_eq!(tier(p), Some(Tier::Restart), "{p}");
        }
        for p in OUTSIDE {
            assert_eq!(tier(p), None, "{p}");
        }
    }

    #[test]
    fn removable_tables_and_only_those() {
        for p in [
            "register",
            "audio",
            "bridge",
            "parrot",
            "announce",
            "announce.tts",
            "announce.events",
            "announce.events.hangup",
            "discord",
            "portal",
            "dtmf",
            "links.\"2000\"",
        ] {
            assert!(is_removable(p), "{p}");
        }
        for p in [
            "listener",
            "control",
            "register.node_id",
            "bridge.mode",
            "links.\"2000\".shape",
            "wireguard",
            "links.\"20a0\"",
            "announce.events.peer_connected",
        ] {
            assert!(!is_removable(p), "{p}");
        }
    }

    #[test]
    fn wire_paths_map_to_toml_keys_including_the_renames() {
        let keys = |p: &str| toml_keys(p).map(|v| v.join("/"));
        assert_eq!(
            keys("credentials.source").as_deref(),
            Some("secrets/source")
        );
        assert_eq!(keys("credentials.file").as_deref(), Some("secrets/file"));
        assert_eq!(
            keys("links.\"2000\".shape").as_deref(),
            Some("links/2000/shape")
        );
        assert_eq!(
            keys("announce.events.answered.enabled").as_deref(),
            Some("announce/events/answered/enabled")
        );
        assert_eq!(keys("codec_policy").as_deref(), Some("codec_policy"));
        assert_eq!(keys("wireguard.key_ref"), None, "read-only on the page");
        assert_eq!(
            keys("secrets.source"),
            None,
            "the TOML name is not the wire name"
        );
        assert_eq!(keys("links.\"20a0\".shape"), None, "not a node number");
        assert_eq!(keys("announce.events.bogus.enabled"), None, "not an event");
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "a test table reads as ok(path, value)"
    )]
    fn ok(path: &str, v: Value) {
        assert_eq!(validate_field(path, &v), Ok(()), "{path} = {v}");
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "a test table reads as bad(path, value, message)"
    )]
    fn bad(path: &str, v: Value, message: &str) {
        assert_eq!(
            validate_field(path, &v),
            Err(FieldError {
                field: path.to_string(),
                message: message.to_string(),
                acknowledge: None,
            }),
            "{path} = {v}"
        );
    }

    #[test]
    fn node_numbers_are_digits_only() {
        ok("register.node_id", json!("12345"));
        bad(
            "register.node_id",
            json!("12a"),
            "register.node_id: \"12a\" is not a node number (digits only)",
        );
        bad(
            "register.node_id",
            json!(""),
            "register.node_id: \"\" is not a node number (digits only)",
        );
        ok("listener.allowed_nodes", json!(["2000", "3000"]));
        ok("listener.allowed_nodes", json!([]));
        bad(
            "listener.allowed_nodes",
            json!(["2000", "12a"]),
            "listener.allowed_nodes: \"12a\" is not a node number (digits only)",
        );
        bad(
            "listener.allowed_nodes",
            json!("2000"),
            "listener.allowed_nodes: expected a list of strings",
        );
        bad(
            "listener.allowed_nodes",
            json!([2000]),
            "listener.allowed_nodes: expected a list of strings",
        );
        ok("links.\"2000\".shape", json!("wt-guest"));
        bad(
            "links.\"20a0\".shape",
            json!("wt-guest"),
            "links: \"20a0\" is not a node number (digits only)",
        );
        bad(
            "links.\"20a0\"",
            Value::Null,
            "links: \"20a0\" is not a node number (digits only)",
        );
    }

    #[test]
    fn snowflakes_are_non_zero_u64() {
        ok("discord.channel_id", json!("123456789012345678"));
        bad(
            "discord.channel_id",
            json!("0"),
            "discord.channel_id: \"0\" is not a Discord id (a snowflake)",
        );
        bad(
            "discord.guild_id",
            json!("+5"),
            "discord.guild_id: \"+5\" is not a Discord id (a snowflake)",
        );
        bad(
            "discord.channel_id",
            json!("99999999999999999999"),
            "discord.channel_id: \"99999999999999999999\" is not a Discord id (a snowflake)",
        );
        bad(
            "discord.channel_id",
            json!(123),
            "discord.channel_id: expected a string",
        );
        ok("discord.approvers", json!(["333", "444"]));
        bad(
            "discord.approvers",
            json!(["333", "x"]),
            "discord.approvers: \"x\" is not a Discord user id (a snowflake)",
        );
        bad(
            "discord.approvers",
            json!(["0"]),
            "discord.approvers: \"0\" is not a Discord user id (a snowflake)",
        );
    }

    #[test]
    fn binds_parse_as_socket_addrs_and_the_control_bind_refuses_a_wildcard() {
        const WILDCARD: &str = "control.bind: a wildcard bind (0.0.0.0 or ::) is refused — it exposes an unauthenticated keying port on every interface";
        ok("listener.bind", json!("0.0.0.0:4569"));
        ok("listener.bind", json!("[::]:4569"));
        bad(
            "listener.bind",
            json!("4569"),
            "listener.bind: invalid socket address syntax",
        );
        bad(
            "listener.bind",
            json!("0.0.0.0"),
            "listener.bind: invalid socket address syntax",
        );
        ok("control.bind", json!("127.0.0.1:8730"));
        ok("control.bind", json!("[::1]:8730"));
        // A LAN literal passes the field rule; the acknowledgement rule is
        // what gates it, and it needs the request's Host to decide.
        ok("control.bind", json!("192.168.1.20:8730"));
        bad("control.bind", json!("0.0.0.0:8730"), WILDCARD);
        bad("control.bind", json!("[::]:8730"), WILDCARD);
        bad(
            "control.bind",
            json!("localhost:8730"),
            "control.bind: invalid socket address syntax",
        );
    }

    #[test]
    fn the_registrar_is_checked_for_shape_and_never_resolved() {
        ok("register.peer", json!("register.allstarlink.org:4569"));
        ok("register.peer", json!("52.20.63.146:4569"));
        ok("register.peer", json!("[2001:db8::1]:4569"));
        // A name that cannot resolve is accepted instantly: the shape is the
        // rule, and the resolver (a sixty-second budget) is never consulted.
        ok("register.peer", json!("no-such-host.invalid:4569"));
        bad(
            "register.peer",
            json!("register.allstarlink.org"),
            "register.peer: \"register.allstarlink.org\" is not host:port",
        );
        bad(
            "register.peer",
            json!(":4569"),
            "register.peer: \":4569\" is not host:port",
        );
        bad(
            "register.peer",
            json!("host:0"),
            "register.peer: \"host:0\" is not host:port",
        );
        bad(
            "register.peer",
            json!("host:65536"),
            "register.peer: \"host:65536\" is not host:port",
        );
        bad(
            "register.peer",
            json!("iax2://host:4569"),
            "register.peer: \"iax2://host:4569\" is not host:port",
        );
    }

    #[test]
    fn closed_pickers_use_the_loaders_sentences() {
        ok("listener.answer", json!("auto"));
        ok("listener.answer", json!("Manual"));
        bad(
            "listener.answer",
            json!("yes"),
            "listener.answer: unknown value \"yes\" (expected \"auto\" or \"manual\")",
        );
        bad(
            "listener.auth",
            json!("maybe"),
            "listener.auth: unknown value \"maybe\" (expected \"required\", \"optional\", or \"off\")",
        );
        bad(
            "bridge.mode",
            json!("mesh"),
            "bridge.mode: unknown value \"mesh\" (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")",
        );
        ok("audio.backend", json!("none"));
        bad(
            "audio.backend",
            json!("None"),
            "audio.backend: unknown value \"None\" (expected \"none\" or \"cpal\")",
        );
        bad(
            "announce.id_mode",
            json!("voice"),
            "announce.id_mode: unknown value \"voice\" (expected \"off\", \"cw\", or \"tts\")",
        );
        ok("announce.events.answered.destination", json!("to_monitor"));
        bad(
            "announce.events.answered.destination",
            json!("air"),
            "announce.events.answered.destination: unknown value \"air\" (expected \"to_air\" or \"to_monitor\")",
        );
        bad(
            "announce.events.peer_connected.enabled",
            json!(true),
            "announce.events: unknown event \"peer_connected\"",
        );
        bad(
            "credentials.source",
            json!("vault"),
            "credentials.source: unknown value \"vault\" (expected \"env\", \"control\", \"config\", or \"file\")",
        );
        ok("codec_policy", json!("prefer_slin16"));
        bad(
            "codec_policy",
            json!("opus"),
            "unknown codec_policy 'opus' (expected ulaw_only | allow_slin | prefer_slin | prefer_slin16)",
        );
        bad(
            "links.\"2000\".shape",
            json!("webtransceiver"),
            "links.\"2000\": unknown shape \"webtransceiver\" (expected \"standard\" or \"wt-guest\")",
        );
    }

    #[test]
    fn numbers_booleans_and_strings_are_type_checked() {
        // The page converts <input type="number"> itself, but a hand-written
        // client (or a form-to-JSON shim) sends strings: every wrong JSON
        // type is a field error naming the path, never a serde sentence and
        // never a `max_calls = "8"` on disk.
        ok("listener.max_calls", json!(8));
        ok("listener.max_calls", json!(0));
        bad(
            "listener.max_calls",
            json!(-1),
            "listener.max_calls: must be a non-negative integer",
        );
        bad(
            "listener.max_calls",
            json!(2.5),
            "listener.max_calls: must be a non-negative integer",
        );
        bad(
            "listener.max_calls",
            json!("8"),
            "listener.max_calls: expected a non-negative integer",
        );
        ok("parrot.vox_threshold_db", json!(-35.5));
        ok("parrot.vox_threshold_db", json!(-35));
        bad(
            "parrot.vox_threshold_db",
            json!("loud"),
            "parrot.vox_threshold_db: expected a number",
        );
        ok("bridge.mix_minus", json!(false));
        bad(
            "bridge.mix_minus",
            json!("true"),
            "bridge.mix_minus: expected a boolean",
        );
        bad(
            "register.node_id",
            json!(12345),
            "register.node_id: expected a string",
        );
        bad(
            "discord.approvers",
            json!("333"),
            "discord.approvers: expected a list of strings",
        );
        ok(
            "announce.join_template",
            json!("Connected to node {server-node-number}"),
        );
        bad(
            "announce.join_template",
            json!(1),
            "announce.join_template: expected a string",
        );
        ok("portal.user", json!("N0CALL"));
        bad("portal.user", json!(""), "portal.user: must not be empty");
        ok("portal.node", json!("12345"));
        ok("portal.credential_env", json!("ALLSTAR_PORTAL_PASS"));
        ok("credentials.file", json!("credentials.toml"));
        ok("credentials.file", json!("creds/keys.toml"));
        bad(
            "credentials.file",
            json!("../elsewhere/credentials.toml"),
            "credentials.file: \"../elsewhere/credentials.toml\" must not contain a .. component",
        );
        bad(
            "credentials.file",
            json!("/etc/iaxnode/state/../credentials.toml"),
            "credentials.file: \"/etc/iaxnode/state/../credentials.toml\" must not contain a .. component",
        );
        bad(
            "credentials.file",
            json!(""),
            "credentials.file: must not be empty",
        );
        bad(
            "credentials.file",
            json!("node.toml"),
            "credentials.file: \"node.toml\" is the node's own config; credentials never go in it",
        );
        bad(
            "credentials.file",
            json!("/etc/iaxnode/node.toml"),
            "credentials.file: \"/etc/iaxnode/node.toml\" is the node's own config; credentials never go in it",
        );
        bad(
            "credentials.file",
            json!("secrets.toml"),
            "credentials.file: \"secrets.toml\" would put the word on the settings page; name it like credentials.toml",
        );
        bad(
            "credentials.file",
            json!("/srv/Discord-Token.toml"),
            "credentials.file: \"/srv/Discord-Token.toml\" would put the word on the settings page; name it like credentials.toml",
        );
        ok("audio.input", json!("USB"));
    }

    #[test]
    fn null_removes_optional_keys_and_tables_but_never_a_required_key() {
        for p in [
            "register",
            "audio",
            "bridge",
            "parrot",
            "announce",
            "announce.tts",
            "announce.events",
            "announce.events.answered",
            "discord",
            "portal",
            "dtmf",
            "links.\"2000\"",
            "audio.input",
            "codec_policy",
            "announce.id_mode",
            "discord.state_file",
            "parrot.vox_threshold_db",
            "credentials.file",
            "listener.allowed_nodes",
            "discord.approvers",
        ] {
            ok(p, Value::Null);
        }
        for p in [
            "listener.bind",
            "listener.answer",
            "listener.max_calls",
            "listener.auth",
            "control.bind",
            "credentials.source",
            "register.peer",
            "register.node_id",
            "discord.guild_id",
            "discord.channel_id",
            "portal.user",
            "portal.node",
            "bridge.mode",
            "announce.enabled",
            "dtmf.enabled",
        ] {
            bad(p, Value::Null, &format!("{p} is required"));
        }
        bad(
            "register",
            json!({"node_id": "1"}),
            "register: only null is accepted (it removes the table)",
        );
    }

    #[test]
    fn a_path_outside_the_vocabulary_is_refused() {
        bad(
            "wireguard.key_ref",
            json!("X"),
            "unknown setting \"wireguard.key_ref\"",
        );
        bad(
            "secrets.secret",
            json!("hunter2"),
            "unknown setting \"secrets.secret\"",
        );
        bad(
            "discord.token",
            json!("x"),
            "unknown setting \"discord.token\"",
        );
        bad(
            "discord.arm",
            json!(true),
            "unknown setting \"discord.arm\"",
        );
        bad("", json!(1), "unknown setting \"\"");
    }

    /// One row per path in the contract's vocabulary table: its tier, a value
    /// it accepts, a value of the wrong JSON type with the type its sentence
    /// names (`None` for a removable table, whose sentence is its own), and
    /// whether `null` is accepted. A dropped or misspelt arm in `rule`, or a
    /// path given the wrong rule, fails its row here rather than surfacing as
    /// a 400 from the page.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one row per vocabulary path")]
    fn every_contract_path_has_its_tier_type_and_nullability() {
        use Tier::{Live as L, Restart as R};
        type Row = (&'static str, Tier, Value, Value, Option<&'static str>, bool);
        const TABLE: Option<&str> = None;
        const STR: Option<&str> = Some("a string");
        const BOOL: Option<&str> = Some("a boolean");
        const UINT: Option<&str> = Some("a non-negative integer");
        const NUM: Option<&str> = Some("a number");
        const LIST: Option<&str> = Some("a list of strings");
        let rows: Vec<Row> = vec![
            ("register", R, Value::Null, json!({}), TABLE, true),
            ("audio", L, Value::Null, json!({}), TABLE, true),
            ("bridge", L, Value::Null, json!({}), TABLE, true),
            ("parrot", L, Value::Null, json!({}), TABLE, true),
            ("announce", R, Value::Null, json!({}), TABLE, true),
            ("announce.tts", R, Value::Null, json!({}), TABLE, true),
            ("announce.events", R, Value::Null, json!({}), TABLE, true),
            (
                "announce.events.answered",
                R,
                Value::Null,
                json!({}),
                TABLE,
                true,
            ),
            ("discord", R, Value::Null, json!({}), TABLE, true),
            ("portal", R, Value::Null, json!({}), TABLE, true),
            ("dtmf", R, Value::Null, json!({}), TABLE, true),
            ("links.\"2000\"", R, Value::Null, json!({}), TABLE, true),
            ("codec_policy", R, json!("prefer_slin"), json!(1), STR, true),
            (
                "listener.bind",
                R,
                json!("0.0.0.0:4569"),
                json!(4569),
                STR,
                false,
            ),
            (
                "listener.answer",
                R,
                json!("manual"),
                json!(true),
                STR,
                false,
            ),
            ("listener.max_calls", R, json!(8), json!("8"), UINT, false),
            ("listener.auth", R, json!("required"), json!(1), STR, false),
            (
                "listener.allowed_nodes",
                R,
                json!(["2000"]),
                json!("2000"),
                LIST,
                true,
            ),
            (
                "register.peer",
                R,
                json!("host:4569"),
                json!(4569),
                STR,
                false,
            ),
            (
                "register.node_id",
                R,
                json!("12345"),
                json!(12345),
                STR,
                false,
            ),
            ("audio.input", L, json!("USB"), json!(1), STR, true),
            ("audio.output", L, json!("USB"), json!(false), STR, true),
            ("audio.backend", R, json!("cpal"), json!(1), STR, true),
            ("bridge.mode", L, json!("conference"), json!(1), STR, false),
            (
                "bridge.mix_minus",
                L,
                json!(true),
                json!("true"),
                BOOL,
                false,
            ),
            (
                "bridge.include_local_radio",
                L,
                json!(false),
                json!(0),
                BOOL,
                false,
            ),
            (
                "parrot.playback_delay_ms",
                L,
                json!(500),
                json!("500"),
                UINT,
                true,
            ),
            (
                "parrot.silence_gap_ms",
                L,
                json!(800),
                json!(true),
                UINT,
                true,
            ),
            (
                "parrot.vox_threshold_db",
                L,
                json!(-35.5),
                json!("x"),
                NUM,
                true,
            ),
            (
                "announce.enabled",
                R,
                json!(true),
                json!("yes"),
                BOOL,
                false,
            ),
            (
                "announce.sounds_dir",
                R,
                json!("/var/lib/sounds"),
                json!(1),
                STR,
                true,
            ),
            (
                "announce.join_template",
                R,
                json!("hi"),
                json!(1),
                STR,
                true,
            ),
            (
                "announce.link_connect_template",
                R,
                json!("hi"),
                json!(1),
                STR,
                true,
            ),
            (
                "announce.link_disconnect_template",
                R,
                json!("bye"),
                json!(1),
                STR,
                true,
            ),
            (
                "announce.mixunder_default_gain_db",
                R,
                json!(-12.0),
                json!("x"),
                NUM,
                true,
            ),
            (
                "announce.cw_tone_hz",
                R,
                json!(800),
                json!("800"),
                NUM,
                true,
            ),
            (
                "announce.id_interval_secs",
                R,
                json!(600),
                json!("600"),
                UINT,
                true,
            ),
            ("announce.cw_wpm", R, json!(20), json!("20"), UINT, true),
            ("announce.id_mode", R, json!("cw"), json!(1), STR, true),
            (
                "announce.cw_keys_when_idle",
                R,
                json!(false),
                json!("no"),
                BOOL,
                true,
            ),
            (
                "announce.tts.enabled",
                R,
                json!(true),
                json!(1),
                BOOL,
                false,
            ),
            (
                "announce.tts.binary",
                R,
                json!("/bin/sh"),
                json!(1),
                STR,
                true,
            ),
            ("announce.tts.voice", R, json!("en_US"), json!(1), STR, true),
            (
                "announce.tts.timeout_ms",
                R,
                json!(5000),
                json!("5000"),
                UINT,
                true,
            ),
            ("announce.tts.gain_db", R, json!(-6), json!("x"), NUM, true),
            (
                "announce.events.answered.enabled",
                R,
                json!(true),
                json!("true"),
                BOOL,
                false,
            ),
            (
                "announce.events.answered.destination",
                R,
                json!("to_air"),
                json!(1),
                STR,
                true,
            ),
            (
                "discord.guild_id",
                R,
                json!("123456789012345678"),
                json!(1),
                STR,
                false,
            ),
            (
                "discord.channel_id",
                R,
                json!("123456789012345678"),
                json!(1),
                STR,
                false,
            ),
            (
                "discord.join_at_start",
                R,
                json!(true),
                json!("x"),
                BOOL,
                false,
            ),
            (
                "discord.announce_on_join",
                R,
                json!(false),
                json!(1),
                BOOL,
                false,
            ),
            (
                "discord.approvers",
                R,
                json!(["333"]),
                json!("333"),
                LIST,
                true,
            ),
            (
                "discord.state_file",
                R,
                json!("approvals.json"),
                json!(1),
                STR,
                true,
            ),
            (
                "discord.mic_timeout_secs",
                R,
                json!(30),
                json!("30"),
                UINT,
                true,
            ),
            (
                "discord.mic_silence_secs",
                R,
                json!(5),
                json!("5"),
                UINT,
                true,
            ),
            ("portal.user", R, json!("N0CALL"), json!(1), STR, false),
            ("portal.node", R, json!("12345"), json!(12345), STR, false),
            (
                "portal.credential_env",
                R,
                json!("ALLSTAR_PORTAL_PASS"),
                json!(1),
                STR,
                true,
            ),
            (
                "links.\"2000\".shape",
                R,
                json!("standard"),
                json!(1),
                STR,
                true,
            ),
            ("dtmf.enabled", R, json!(true), json!("on"), BOOL, false),
            (
                "dtmf.inter_digit_timeout_ms",
                R,
                json!(3000),
                json!("3000"),
                UINT,
                true,
            ),
            (
                "control.bind",
                R,
                json!("127.0.0.1:8730"),
                json!(8730),
                STR,
                false,
            ),
            ("credentials.source", R, json!("file"), json!(1), STR, false),
            (
                "credentials.file",
                R,
                json!("credentials.toml"),
                json!(1),
                STR,
                true,
            ),
        ];
        assert_eq!(rows.len(), 64, "one row per contract path");
        for (p, t, good, wrong, ty, nullable) in &rows {
            let err = |message: String| {
                Err(FieldError {
                    field: (*p).to_string(),
                    message,
                    acknowledge: None,
                })
            };
            assert_eq!(tier(p), Some(*t), "{p}");
            assert_eq!(validate_field(p, good), Ok(()), "{p} = {good}");
            let refused = match ty {
                Some(ty) => format!("{p}: expected {ty}"),
                None => format!("{p}: only null is accepted (it removes the table)"),
            };
            assert_eq!(validate_field(p, wrong), err(refused), "{p} = {wrong}");
            let null = if *nullable {
                Ok(())
            } else {
                err(format!("{p} is required"))
            };
            assert_eq!(validate_field(p, &Value::Null), null, "{p} = null");
        }
    }

    #[test]
    fn the_state_file_parent_must_exist_now() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fine = dir.path().join("approvals.json");
        ok(
            "discord.state_file",
            json!(fine.to_str().expect("utf-8 temp path")),
        );
        let missing_dir = dir.path().join("missing");
        let missing = missing_dir.join("approvals.json");
        bad(
            "discord.state_file",
            json!(missing.to_str().expect("utf-8 temp path")),
            &format!(
                "discord.state_file: the directory {} does not exist",
                missing_dir.display()
            ),
        );
    }

    fn minimal() -> String {
        "[listener]\nbind = \"0.0.0.0:4569\"\nanswer = \"auto\"\nmax_calls = 20\nauth = \"optional\"\n\
         [control]\nbind = \"127.0.0.1:8730\"\n[secrets]\nsource = \"env\"\n"
            .to_string()
    }

    fn redacted(toml: &str) -> RedactedConfig {
        RedactedConfig::from_config(&NodeFileConfig::from_toml_str(toml).expect("fixture parses"))
    }

    fn cfg(entries: &[(&str, Value)]) -> RedactedConfig {
        RedactedConfig(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    const FIXTURE: &str = r#"
codec_policy = "prefer_slin"

[listener]
bind = "0.0.0.0:4569"
answer = "auto"
max_calls = 8
auth = "off"
allowed_nodes = ["2000"]

[register]
peer = "register.allstarlink.org:4569"
node_id = "12345"

[bridge]
mode = "bridge"

[announce]
enabled = true
id_mode = "off"

[announce.tts]
enabled = true
gain_db = -10.5

[announce.events]
answered = { enabled = true, destination = "to_air" }

[control]
bind = "127.0.0.1:8730"

[secrets]
source = "file"

[links."2000"]
shape = "wt-guest"

[links."3000"]

[dtmf]
enabled = false
"#;

    #[test]
    fn the_redacted_view_is_a_flat_map_of_what_is_on_disk() {
        let view = redacted(FIXTURE);
        assert_eq!(
            serde_json::to_value(&view).expect("serialises"),
            json!({
                "announce.enabled": true,
                "announce.events.answered.destination": "to_air",
                "announce.events.answered.enabled": true,
                "announce.id_mode": "off",
                "announce.tts.enabled": true,
                "announce.tts.gain_db": -10.5,
                "bridge.include_local_radio": false,
                "bridge.mix_minus": true,
                "bridge.mode": "bridge",
                "codec_policy": "prefer_slin",
                "control.bind": "127.0.0.1:8730",
                "control.loopback_login": false,
                "credentials.source": "file",
                "dtmf.enabled": false,
                "links.\"2000\".shape": "wt-guest",
                "links.\"3000\".shape": null,
                "listener.allowed_nodes": ["2000"],
                "listener.answer": "auto",
                "listener.auth": "off",
                "listener.bind": "0.0.0.0:4569",
                "listener.max_calls": 8,
                "register.node_id": "12345",
                "register.peer": "register.allstarlink.org:4569",
            })
        );
        assert_eq!(view.get("bridge.mode"), Some(&json!("bridge")));
        assert_eq!(view.get("register.secret"), None);
    }

    #[test]
    fn the_redacted_view_never_carries_the_inline_secret_or_the_word() {
        let toml = minimal().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"hunter2xyz\"",
        ) + "[wireguard]\naddress = \"10.99.0.2/32\"\npeer_public_key = \"k\"\n\
             endpoint = \"127.0.0.1:51820\"\nsecret_ref = \"MY_WG_KEY\"\n";
        let view = redacted(&toml);
        let json = serde_json::to_string(&view).expect("serialises");
        assert!(!json.contains("hunter2xyz"), "{json}");
        for word in ["secret", "password", "token"] {
            assert!(
                !json.to_ascii_lowercase().contains(word),
                "{word} reached the wire shape: {json}"
            );
        }
        assert_eq!(view.get("wireguard.key_ref"), Some(&json!("MY_WG_KEY")));
        assert_eq!(view.get("credentials.source"), Some(&json!("config")));
    }

    #[test]
    fn a_float_reaches_the_wire_as_the_decimal_that_was_written() {
        // -10.3 has no exact binary form; the FIXTURE's -10.5 does, which is
        // why it cannot catch a widening that leaks f32 rounding.
        let view = redacted(&(minimal() + "[announce]\n[announce.tts]\ngain_db = -10.3\n"));
        assert_eq!(view.get("announce.tts.gain_db"), Some(&json!(-10.3)));
    }

    #[test]
    fn an_absent_wireguard_ref_shows_the_default_the_stack_will_use() {
        let toml = minimal()
            + "[wireguard]\naddress = \"10.99.0.2/32\"\npeer_public_key = \"k\"\nendpoint = \"127.0.0.1:51820\"\n";
        assert_eq!(
            redacted(&toml).get("wireguard.key_ref"),
            Some(&json!("WIREGUARD_PRIVATE_KEY"))
        );
    }

    #[test]
    fn diff_splits_by_tier_and_counts_a_one_sided_path_as_a_difference() {
        let disk = cfg(&[
            ("bridge.mode", json!("handset")),
            ("listener.bind", json!("0.0.0.0:4569")),
            ("wireguard.key_ref", json!("WG")),
            ("control.bind", json!("127.0.0.1:8730")),
        ]);
        let running = cfg(&[
            ("bridge.mode", json!("bridge")),
            ("listener.bind", json!("0.0.0.0:4570")),
            ("audio.input", json!("USB")),
            ("control.bind", json!("127.0.0.1:8730")),
        ]);
        assert_eq!(
            RedactedConfig::diff(&disk, &running),
            Pending {
                restart: vec!["listener.bind".into(), "wireguard.key_ref".into()],
                apply: vec!["audio.input".into(), "bridge.mode".into()],
            }
        );
        assert_eq!(RedactedConfig::diff(&disk, &disk), Pending::default());
    }

    #[test]
    fn etag_is_sixteen_lowercase_hex_digits_and_tracks_every_byte() {
        let a = etag_of(NODE_TOML_TEMPLATE.as_bytes());
        assert_eq!(a.len(), 16);
        assert!(
            a.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{a}"
        );
        assert_eq!(
            a,
            etag_of(NODE_TOML_TEMPLATE.as_bytes()),
            "deterministic: the page compares strings across requests"
        );
        let mut one_byte_off = NODE_TOML_TEMPLATE.as_bytes().to_vec();
        one_byte_off[0] ^= 0x01;
        assert_ne!(a, etag_of(&one_byte_off));
        assert_ne!(etag_of(b""), etag_of(b" "));
    }

    const LOOPBACK: WriteContext = WriteContext {
        loopback_peer: true,
    };
    const LAN_HOST: WriteContext = WriteContext {
        loopback_peer: false,
    };

    fn req(acks: &[&str]) -> SettingsPatch {
        SettingsPatch {
            etag: String::new(),
            patch: BTreeMap::new(),
            acknowledge: acks.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    fn needs(field: &str, message: &str, key: &str) -> Result<(), FieldError> {
        Err(FieldError {
            field: field.to_string(),
            message: message.to_string(),
            acknowledge: Some(key.to_string()),
        })
    }

    #[test]
    fn a_non_loopback_control_bind_needs_the_acknowledgement_and_a_loopback_host() {
        let before = cfg(&[("control.bind", json!("127.0.0.1:8730"))]);
        let lan = cfg(&[("control.bind", json!("192.168.1.20:8730"))]);
        assert_eq!(
            check_acknowledgements(&before, &lan, &req(&[]), &LOOPBACK),
            needs(
                "control.bind",
                "control.bind: a non-loopback bind needs the control.bind.public acknowledgement",
                ACK_CONTROL_BIND_PUBLIC
            )
        );
        assert_eq!(
            check_acknowledgements(&before, &lan, &req(&["control.bind.public"]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&before, &lan, &req(&["control.bind.public"]), &LAN_HOST),
            Err(FieldError {
                field: "control.bind".into(),
                message: "control.bind: may only be changed by a request from this host (a loopback peer)"
                    .into(),
                acknowledge: None,
            }),
            "no acknowledgement helps a request that did not arrive over loopback"
        );
        let v6 = cfg(&[("control.bind", json!("[::1]:8730"))]);
        assert_eq!(
            check_acknowledgements(&before, &v6, &req(&[]), &LOOPBACK),
            Ok(()),
            "an IPv6 loopback literal is loopback"
        );
        assert_eq!(
            check_acknowledgements(&lan, &lan, &req(&[]), &LAN_HOST),
            Ok(()),
            "an unchanged bind is not an edit, whatever the Host"
        );
    }

    #[test]
    fn auth_off_with_no_allowlist_needs_the_acknowledgement_only_when_it_is_new() {
        let closed = cfg(&[
            ("listener.auth", json!("required")),
            ("listener.allowed_nodes", json!([])),
        ]);
        let open = cfg(&[
            ("listener.auth", json!("off")),
            ("listener.allowed_nodes", json!([])),
        ]);
        let open_absent_list = cfg(&[("listener.auth", json!("OFF"))]);
        let off_with_list = cfg(&[
            ("listener.auth", json!("off")),
            ("listener.allowed_nodes", json!(["2000"])),
        ]);
        let want = needs(
            "listener.auth",
            "listener.auth = \"off\" with no allowed_nodes admits every caller; it needs the listener.auth.off acknowledgement",
            ACK_LISTENER_AUTH_OFF,
        );
        assert_eq!(
            check_acknowledgements(&closed, &open, &req(&[]), &LOOPBACK),
            want
        );
        assert_eq!(
            check_acknowledgements(&closed, &open_absent_list, &req(&[]), &LOOPBACK),
            want
        );
        assert_eq!(
            check_acknowledgements(&closed, &open, &req(&["listener.auth.off"]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&open, &open, &req(&[]), &LOOPBACK),
            Ok(()),
            "already open: saving something else does not re-ask"
        );
        assert_eq!(
            check_acknowledgements(&closed, &off_with_list, &req(&[]), &LOOPBACK),
            Ok(()),
            "an allowlist keeps the door narrow"
        );
    }

    #[test]
    fn the_on_air_switches_ask_once_on_the_way_up() {
        for (path, key, message) in [
            (
                "dtmf.enabled",
                ACK_DTMF_ENABLED,
                "dtmf.enabled lets any connected member command links; it needs the dtmf.enabled acknowledgement",
            ),
            (
                "bridge.include_local_radio",
                ACK_LOCAL_RADIO,
                "bridge.include_local_radio puts the local microphone on the air; it needs the bridge.include_local_radio acknowledgement",
            ),
        ] {
            let off = cfg(&[(path, json!(false))]);
            let absent = cfg(&[]);
            let on = cfg(&[(path, json!(true))]);
            assert_eq!(
                check_acknowledgements(&off, &on, &req(&[]), &LOOPBACK),
                needs(path, message, key),
                "{path}"
            );
            assert_eq!(
                check_acknowledgements(&absent, &on, &req(&[]), &LOOPBACK),
                needs(path, message, key),
                "{path}: absent is off"
            );
            assert_eq!(
                check_acknowledgements(&off, &on, &req(&[key]), &LOOPBACK),
                Ok(()),
                "{path}"
            );
            assert_eq!(
                check_acknowledgements(&on, &on, &req(&[]), &LOOPBACK),
                Ok(()),
                "{path}: already on"
            );
            assert_eq!(
                check_acknowledgements(&on, &off, &req(&[]), &LOOPBACK),
                Ok(()),
                "{path}: turning it off never asks"
            );
        }
    }

    #[test]
    fn cw_keys_when_idle_defaults_to_true_so_only_false_to_true_asks() {
        // Every side carries `announce.enabled`, as a real [announce] section
        // always does: the rule only runs while the section exists.
        let explicit_false = cfg(&[
            ("announce.enabled", json!(true)),
            ("announce.cw_keys_when_idle", json!(false)),
        ]);
        let explicit_true = cfg(&[
            ("announce.enabled", json!(true)),
            ("announce.cw_keys_when_idle", json!(true)),
        ]);
        let absent = cfg(&[("announce.enabled", json!(true))]);
        let no_announce = cfg(&[]);
        let want = needs(
            "announce.cw_keys_when_idle",
            "announce.cw_keys_when_idle keys the transmitter for an idle ID; it needs the announce.cw_keys_when_idle acknowledgement",
            ACK_CW_IDLE,
        );
        assert_eq!(
            check_acknowledgements(&explicit_false, &explicit_true, &req(&[]), &LOOPBACK),
            want
        );
        assert_eq!(
            check_acknowledgements(&explicit_false, &absent, &req(&[]), &LOOPBACK),
            want,
            "removing the key restores the library default, which is true"
        );
        assert_eq!(
            check_acknowledgements(&absent, &explicit_true, &req(&[]), &LOOPBACK),
            Ok(()),
            "absent already meant true"
        );
        assert_eq!(
            check_acknowledgements(&absent, &explicit_false, &req(&[]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&explicit_false, &no_announce, &req(&[]), &LOOPBACK),
            Ok(()),
            "removing [announce] turns every announcement off; nothing will key"
        );
    }

    #[test]
    fn a_zero_mic_timeout_asks_once() {
        let default = cfg(&[]);
        let off = cfg(&[("discord.mic_timeout_secs", json!(0))]);
        assert_eq!(
            check_acknowledgements(&default, &off, &req(&[]), &LOOPBACK),
            needs(
                "discord.mic_timeout_secs",
                "discord.mic_timeout_secs = 0 removes the hard time-out on the open mic; it needs the discord.mic_timeout.off acknowledgement",
                ACK_MIC_TIMEOUT_OFF
            )
        );
        assert_eq!(
            check_acknowledgements(
                &default,
                &off,
                &req(&["discord.mic_timeout.off"]),
                &LOOPBACK
            ),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&off, &off, &req(&[]), &LOOPBACK),
            Ok(())
        );
    }

    #[test]
    fn removing_an_approver_asks_and_reordering_or_adding_does_not() {
        let two = cfg(&[("discord.approvers", json!(["333", "444"]))]);
        let one = cfg(&[("discord.approvers", json!(["444"]))]);
        let swapped = cfg(&[("discord.approvers", json!(["444", "333"]))]);
        let three = cfg(&[("discord.approvers", json!(["333", "444", "555"]))]);
        let no_discord = cfg(&[]);
        let want = needs(
            "discord.approvers",
            "removing an approver leaves their slash commands registered in Discord; it needs the discord.approvers.remove acknowledgement",
            ACK_APPROVER_REMOVED,
        );
        assert_eq!(
            check_acknowledgements(&two, &one, &req(&[]), &LOOPBACK),
            want
        );
        assert_eq!(
            check_acknowledgements(&two, &no_discord, &req(&[]), &LOOPBACK),
            want,
            "removing the whole section removes every approver"
        );
        assert_eq!(
            check_acknowledgements(&two, &one, &req(&["discord.approvers.remove"]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&two, &swapped, &req(&[]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&two, &three, &req(&[]), &LOOPBACK),
            Ok(())
        );
    }

    #[test]
    fn the_first_failing_rule_wins_in_table_order() {
        let before = cfg(&[
            ("control.bind", json!("127.0.0.1:8730")),
            ("listener.auth", json!("required")),
        ]);
        let after = cfg(&[
            ("control.bind", json!("192.168.1.20:8730")),
            ("listener.auth", json!("off")),
        ]);
        assert_eq!(
            check_acknowledgements(&before, &after, &req(&[]), &LOOPBACK),
            needs(
                "control.bind",
                "control.bind: a non-loopback bind needs the control.bind.public acknowledgement",
                ACK_CONTROL_BIND_PUBLIC
            )
        );
        assert_eq!(
            check_acknowledgements(&before, &after, &req(&["control.bind.public"]), &LOOPBACK),
            needs(
                "listener.auth",
                "listener.auth = \"off\" with no allowed_nodes admits every caller; it needs the listener.auth.off acknowledgement",
                ACK_LISTENER_AUTH_OFF
            )
        );
    }

    #[test]
    fn the_oracle_rejects_what_the_loader_rejects_with_the_loaders_words() {
        let toml = minimal() + "[bridge]\nmode = \"mesh\"\n";
        assert_eq!(
            oracle(&toml).unwrap_err(),
            "bridge.mode: unknown value \"mesh\" (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
        );
    }

    #[test]
    fn the_oracle_checks_the_control_bind_the_loader_does_not() {
        let toml = minimal().replace("bind = \"127.0.0.1:8730\"", "bind = \"nope\"");
        assert!(
            NodeFileConfig::from_toml_str(&toml).is_ok(),
            "the loader itself never parses control.bind; main.rs does"
        );
        assert_eq!(
            oracle(&toml).unwrap_err(),
            "control.bind: invalid socket address syntax"
        );
    }

    #[test]
    fn the_oracle_checks_the_state_file_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing_dir = dir.path().join("missing");
        let toml = format!(
            "{}[discord]\nguild_id = \"1\"\nchannel_id = \"2\"\nstate_file = {:?}\n",
            minimal(),
            missing_dir.join("approvals.json").to_str().expect("utf-8")
        );
        assert_eq!(
            oracle(&toml).unwrap_err(),
            format!(
                "discord.state_file: the directory {} does not exist",
                missing_dir.display()
            )
        );
    }

    #[test]
    fn the_oracle_never_resolves_the_registrar() {
        // A registrar that cannot resolve is fine at save time: `to_register`
        // is the boot path's job, with its sixty-second budget.
        let toml =
            minimal() + "[register]\npeer = \"no-such-host.invalid:4569\"\nnode_id = \"12345\"\n";
        let cfg = oracle(&toml).expect("accepted without a lookup");
        assert_eq!(
            cfg.register.expect("section present").peer,
            "no-such-host.invalid:4569"
        );
    }

    #[test]
    fn the_oracle_never_runs_the_wireguard_builder() {
        // `to_link_transport` builds a WgLinkConfig, and on a valid peer key
        // WgLinkConfig::new resolves `endpoint` with to_socket_addrs. This
        // section's key ("k") is not 32 bytes of base64, so that builder
        // refuses it before any lookup: an oracle that ran it fails here,
        // hermetically, with "wireguard: invalid key: …". The key stays
        // invalid on purpose — a valid one would make the mutation check a
        // real DNS query. wireguard.* is outside the vocabulary, so the
        // section is exactly as valid as it was at boot.
        let toml = minimal()
            + "[wireguard]\naddress = \"10.99.0.2/32\"\npeer_public_key = \"k\"\n\
               endpoint = \"no-such-host.invalid:51820\"\n";
        let cfg = oracle(&toml).expect("accepted without a lookup");
        assert_eq!(
            cfg.wireguard.expect("section present").endpoint,
            "no-such-host.invalid:51820"
        );
    }

    #[test]
    fn the_oracle_error_never_quotes_the_secret_line() {
        let toml = minimal().replace(
            "source = \"env\"",
            "source = \"config\"\nsecret = \"SENTINEL-9f3a",
        );
        let err = oracle(&toml).unwrap_err();
        assert!(!err.contains("SENTINEL-9f3a"), "{err}");
        assert_eq!(err, "invalid basic string, expected `\"`");
    }

    #[test]
    fn the_template_and_the_example_pass_the_oracle() {
        oracle(NODE_TOML_TEMPLATE).expect("the generated template must be saveable as-is");
        oracle(EXAMPLE).expect("deploy/node.toml.example must be saveable as-is");
    }

    fn patch(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn an_empty_patch_round_trips_the_template_and_the_example_byte_for_byte() {
        assert_eq!(
            apply_patch(NODE_TOML_TEMPLATE, &BTreeMap::new()).unwrap(),
            NODE_TOML_TEMPLATE
        );
        assert_eq!(apply_patch(EXAMPLE, &BTreeMap::new()).unwrap(), EXAMPLE);
    }

    #[test]
    fn a_replaced_value_keeps_its_alignment_and_trailing_comment() {
        let out = apply_patch(EXAMPLE, &patch(&[("listener.max_calls", json!(3))])).unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace("max_calls = 2 ", "max_calls = 3 "),
            "only the value changed; alignment, the trailing comment and every other byte are as written"
        );
        assert_eq!(
            oracle(&out).unwrap().listener.max_calls,
            3,
            "and the loader reads the new value"
        );
    }

    #[test]
    fn unknown_keys_survive_a_patch_to_their_section() {
        let toml = minimal()
            + "[discord]\nguild_id = \"1\"\nchannel_id = \"2\"\nmodel = \"/x.onnx\"\nlatency = \"ll5\"\n";
        let out = apply_patch(&toml, &patch(&[("discord.join_at_start", json!(true))])).unwrap();
        assert!(
            oracle(&out)
                .unwrap()
                .discord
                .expect("section")
                .join_at_start
        );
        // [discord] is the last table in this fixture, so the new key is
        // appended to it and the reserved model/latency lines are untouched.
        assert_eq!(out, toml + "join_at_start = true\n");
    }

    #[test]
    fn a_missing_register_table_is_created_and_the_commented_one_is_left_alone() {
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[
                ("register.node_id", json!("12345")),
                ("register.peer", json!("register.allstarlink.org:4569")),
            ]),
        )
        .unwrap();
        let reg = oracle(&out)
            .unwrap()
            .register
            .expect("register now present");
        assert_eq!(reg.node_id, "12345");
        assert_eq!(reg.peer, "register.allstarlink.org:4569");
        // toml_edit gives a new table the "\n" prefix of DEFAULT_TABLE_DECOR
        // and renders it after the last positioned table ([secrets]); the
        // document's trailing comments — the "# [register]" commentary and
        // the "# [announce]" / "# [discord]" blocks — follow unchanged.
        assert_eq!(
            out,
            NODE_TOML_TEMPLATE.replace(
                "source = \"file\"\n",
                "source = \"file\"\n\n[register]\nnode_id = \"12345\"\npeer = \"register.allstarlink.org:4569\"\n"
            )
        );
    }

    #[test]
    fn null_on_a_table_removes_it() {
        let toml = minimal() + "[register]\npeer = \"52.20.63.146:4569\"\nnode_id = \"12345\"\n";
        let out = apply_patch(&toml, &patch(&[("register", Value::Null)])).unwrap();
        assert!(oracle(&out).unwrap().register.is_none());
        // The fixture's [register] header carries no decor of its own, so the
        // whole table and its lines are gone and nothing else moved.
        assert_eq!(
            out,
            minimal(),
            "the whole table and its lines are gone, nothing else moved"
        );
    }

    #[test]
    fn a_link_table_is_inserted_by_quoted_node_and_removed_by_null() {
        let added = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("links.\"2000\".shape", json!("wt-guest"))]),
        )
        .unwrap();
        // The implicit `links` table gets no header of its own; the node's
        // table lands after [secrets] like any new table. If toml_edit
        // renders the numeric key quoted, adjust `[links.2000]` below to
        // `[links."2000"]` — both parse — but the position and the key
        // lines must not move.
        assert_eq!(
            added,
            NODE_TOML_TEMPLATE.replace(
                "source = \"file\"\n",
                "source = \"file\"\n\n[links.2000]\nshape = \"wt-guest\"\n"
            )
        );
        let cfg = oracle(&added).unwrap();
        assert_eq!(
            cfg.links.get("2000").and_then(|l| l.shape.as_deref()),
            Some("wt-guest")
        );
        let removed = apply_patch(&added, &patch(&[("links.\"2000\"", Value::Null)])).unwrap();
        assert!(oracle(&removed).unwrap().links.is_empty());
        assert_eq!(
            removed, NODE_TOML_TEMPLATE,
            "removing the only link leaves the file exactly as it was — no empty [links] header"
        );
    }

    #[test]
    fn a_key_inside_an_inline_table_stays_inline() {
        // The shape in deploy/node.toml.example and in the live VPS config:
        // a per-event inline table. A patch to one of its keys must edit
        // the inline table in place, keep its spacing and trailing comment,
        // and still parse.
        let out = apply_patch(
            EXAMPLE,
            &patch(&[("announce.events.answered.destination", json!("to_monitor"))]),
        )
        .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace(
                "answered = { enabled = true, destination = \"to_air\" }",
                "answered = { enabled = true, destination = \"to_monitor\" }"
            ),
            "the inline table stays on its line with its spacing and its trailing comment"
        );
        let cfg = oracle(&out).unwrap();
        let events = cfg.announce.expect("announce").events.expect("events");
        assert_eq!(
            events["answered"].destination.as_deref(),
            Some("to_monitor")
        );

        // The other key of the same inline table, with the exact spelling.
        let out = apply_patch(
            EXAMPLE,
            &patch(&[("announce.events.answered.enabled", json!(false))]),
        )
        .unwrap();
        assert_eq!(
            out,
            EXAMPLE.replace(
                "answered = { enabled = true, destination = \"to_air\" }",
                "answered = { enabled = false, destination = \"to_air\" }"
            )
        );
        assert!(
            !oracle(&out)
                .unwrap()
                .announce
                .expect("announce")
                .events
                .expect("events")["answered"]
                .enabled
        );

        // A fixture whose inner spacing is NOT toml_edit's default: the last
        // value of an inline table renders with " " / " " decor when it has
        // none of its own, so only a non-default fixture proves the decor
        // was copied rather than defaulted.
        let tight =
            minimal() + "[announce.events]\nanswered = {enabled = true,destination = \"to_air\"}\n";
        let out = apply_patch(
            &tight,
            &patch(&[("announce.events.answered.destination", json!("to_monitor"))]),
        )
        .unwrap();
        assert_eq!(
            out,
            tight.replace("destination = \"to_air\"}", "destination = \"to_monitor\"}"),
            "the inner value's own (absent) decor is kept, not replaced by the default"
        );
    }

    #[test]
    fn a_new_event_is_created_under_an_existing_events_table_and_under_none() {
        let out = apply_patch(
            EXAMPLE,
            &patch(&[("announce.events.hangup.enabled", json!(true))]),
        )
        .unwrap();
        let events = oracle(&out)
            .unwrap()
            .announce
            .expect("announce")
            .events
            .expect("events");
        assert!(events["hangup"].enabled);
        assert!(
            events["answered"].enabled,
            "the inline sibling is untouched"
        );
        // [announce.events] is the example's last table and the answered line
        // (example line 241) is its last line, so the new implicit-then-
        // headed table renders directly after it with DEFAULT_TABLE_DECOR's
        // leading "\n", and the inline sibling keeps its line and comment.
        assert_eq!(
            out,
            EXAMPLE.replace(
                "answered = { enabled = true, destination = \"to_air\" }   # the join greeting\n",
                "answered = { enabled = true, destination = \"to_air\" }   # the join greeting\n\n[announce.events.hangup]\nenabled = true\n"
            )
        );

        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("announce.events.answered.enabled", json!(true))]),
        )
        .unwrap();
        // [announce] and [announce.events] are created implicit and hold no
        // values of their own, so only the leaf table gets a header, after
        // [secrets] like every new table in the template.
        assert_eq!(
            out,
            NODE_TOML_TEMPLATE.replace(
                "source = \"file\"\n",
                "source = \"file\"\n\n[announce.events.answered]\nenabled = true\n"
            )
        );
        let cfg = oracle(&out).unwrap();
        let announce = cfg.announce.expect("[announce] was implied into existence");
        assert!(!announce.enabled, "and its own keys are the serde defaults");
        assert!(announce.events.expect("events")["answered"].enabled);
    }

    #[test]
    fn credentials_paths_land_in_the_secrets_table() {
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("credentials.source", json!("env"))]),
        )
        .unwrap();
        assert_eq!(oracle(&out).unwrap().secrets.source, "env");
        // The comment line above the key also contains `source = "file"`,
        // but followed by a space, not a newline, so it is not touched.
        assert_eq!(
            out,
            NODE_TOML_TEMPLATE.replace("source = \"file\"\n", "source = \"env\"\n")
        );
    }

    #[test]
    fn numbers_are_written_as_toml_integers_and_floats() {
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[
                ("parrot.vox_threshold_db", json!(-35.5)),
                ("parrot.playback_delay_ms", json!(1000)),
            ]),
        )
        .unwrap();
        // The patch is a BTreeMap, so playback_delay_ms is inserted before
        // vox_threshold_db; the new [parrot] table lands after [secrets]
        // like the [register] case above.
        assert_eq!(
            out,
            NODE_TOML_TEMPLATE.replace(
                "source = \"file\"\n",
                "source = \"file\"\n\n[parrot]\nplayback_delay_ms = 1000\nvox_threshold_db = -35.5\n"
            )
        );
        let parrot = oracle(&out).unwrap().parrot.expect("parrot");
        assert_eq!(parrot.playback_delay_ms, Some(1000));
        assert!((parrot.vox_threshold_db.expect("vox") - -35.5).abs() < 1e-6);
    }

    #[test]
    fn one_bad_entry_fails_the_whole_patch_before_anything_is_rendered() {
        let err = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[
                ("listener.answer", json!("manual")),
                ("listener.max_calls", json!(-1)),
            ]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FieldError {
                field: "listener.max_calls".into(),
                message: "listener.max_calls: must be a non-negative integer".into(),
                acknowledge: None,
            },
            "the error names the bad entry, not the first entry"
        );
        // And a wrong JSON type, the page's own likeliest slip.
        let err = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("listener.max_calls", json!("8"))]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            FieldError {
                field: "listener.max_calls".into(),
                message: "listener.max_calls: expected a non-negative integer".into(),
                acknowledge: None,
            }
        );
    }

    #[test]
    fn a_path_outside_the_vocabulary_never_reaches_the_document() {
        assert_eq!(
            apply_patch(
                NODE_TOML_TEMPLATE,
                &patch(&[("wireguard.key_ref", json!("X"))])
            )
            .unwrap_err(),
            FieldError {
                field: "wireguard.key_ref".into(),
                message: "unknown setting \"wireguard.key_ref\"".into(),
                acknowledge: None,
            }
        );
    }

    #[test]
    fn a_patch_the_field_rules_pass_can_still_be_refused_by_the_oracle() {
        // guild_id alone is a valid field; [discord] without channel_id is
        // not a valid section. The page sends both, but the oracle is what
        // makes that a rule rather than a convention.
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("discord.guild_id", json!("1"))]),
        )
        .unwrap();
        assert_eq!(oracle(&out).unwrap_err(), "missing field `channel_id`");
    }

    // ---- [m17] ---------------------------------------------------------------

    const M17_OPT_IN: &str = "m17.enabled: a save that creates [m17] must set m17.enabled too — the section turns M17 on by default";
    const M17_CODEC_ACK: &str = "m17.codec_dirs loads a libcodec2 from these directories into the node at its next start; it needs the m17.codec_dirs acknowledgement";

    #[test]
    fn m17_paths_have_their_tier_and_only_the_dial_time_keys_are_live() {
        for p in [
            "m17.enabled",
            "m17.reflectors",
            "m17.reflectors.\"M17-XYZ\"",
            "m17.reflectors.\"m17-xyz\"",
        ] {
            assert_eq!(tier(p), Some(Tier::Live), "{p}");
        }
        // codec_dirs decides which library is loaded: never live.
        for p in ["m17", "m17.directory", "m17.codec_dirs"] {
            assert_eq!(tier(p), Some(Tier::Restart), "{p}");
        }
        for p in [
            "m17.callsign",
            "m17.transmit",
            "m17.ptt",
            "m17.arm",
            "m17.reflectors.M17-XYZ",
            "m17.reflectors.\"M17-XYZ\".host",
            "m17.reflectors.\"M1\"",
            "m17.reflectors.\" M17-X\"",
            "\"m17\".enabled",
        ] {
            assert_eq!(tier(p), None, "{p}");
        }
        assert!(is_removable("m17"));
        assert!(is_removable("m17.reflectors"));
        assert!(!is_removable("m17.enabled"));
        assert!(!is_removable("m17.reflectors.\"M17-XYZ\""));
        assert_eq!(
            toml_keys("m17.reflectors.\"M17-XYZ\""),
            Some(vec![
                "m17".to_string(),
                "reflectors".to_string(),
                "M17-XYZ".to_string()
            ])
        );
        assert_eq!(
            toml_keys("m17.codec_dirs"),
            Some(vec!["m17".to_string(), "codec_dirs".to_string()])
        );
        assert_eq!(
            LIVE_PREFIXES,
            &[
                "bridge.",
                "parrot.",
                "audio.input",
                "audio.output",
                "m17.enabled",
                "m17.reflectors",
                "packet_log."
            ]
        );
    }

    #[test]
    fn m17_values_are_refused_in_the_loaders_words() {
        ok("m17.enabled", json!(true));
        bad(
            "m17.enabled",
            json!("yes"),
            "m17.enabled: expected a boolean",
        );
        bad("m17.enabled", Value::Null, "m17.enabled is required");

        ok("m17.directory", json!("bundled"));
        ok("m17.directory", json!("/etc/iaxnode/m17-reflectors.json"));
        ok("m17.directory", Value::Null);
        bad(
            "m17.directory",
            json!("reflectors.json"),
            "m17.directory: \"reflectors.json\" is neither \"bundled\" nor an absolute path",
        );
        bad(
            "m17.directory",
            json!("Bundled"),
            "m17.directory: \"Bundled\" is neither \"bundled\" nor an absolute path",
        );

        ok("m17.codec_dirs", json!(["/opt/astar/lib"]));
        ok("m17.codec_dirs", json!([]));
        ok("m17.codec_dirs", Value::Null);
        bad(
            "m17.codec_dirs",
            json!(["/opt/astar/lib", "lib"]),
            "m17.codec_dirs: \"lib\" is not an absolute path",
        );
        bad(
            "m17.codec_dirs",
            json!([""]),
            "m17.codec_dirs: \"\" is not an absolute path",
        );
        bad(
            "m17.codec_dirs",
            json!("/opt/astar/lib"),
            "m17.codec_dirs: expected a list of strings",
        );

        let entry = "m17.reflectors.\"M17-XYZ\"";
        ok(entry, json!("m17.example.net"));
        ok(entry, json!("m17.example.net:17001"));
        ok(entry, Value::Null);
        ok("m17.reflectors.\"m17-xyz\"", json!("m17.example.net"));
        bad(
            entry,
            json!(""),
            "m17.reflectors.\"M17-XYZ\": host is empty",
        );
        bad(
            entry,
            json!("2001:db8::1"),
            "m17.reflectors.\"M17-XYZ\": IPv6 literals are not supported",
        );
        bad(
            entry,
            json!("m17 example.net"),
            "m17.reflectors.\"M17-XYZ\": host contains whitespace",
        );
        bad(
            entry,
            json!("m17.example.net:0"),
            "m17.reflectors.\"M17-XYZ\": bad port \"0\"",
        );
        bad(
            entry,
            json!(17000),
            "m17.reflectors.\"M17-XYZ\": expected a string",
        );
        bad(
            "m17.reflectors.\"M1\"",
            json!("h"),
            "m17.reflectors.\"M1\": A reflector name is 3 to 8 characters.",
        );
        bad(
            "m17.reflectors.\"M17 X\"",
            json!("h"),
            "m17.reflectors.\"M17 X\": A reflector name is letters, digits and '-' only.",
        );
        bad(
            "m17.reflectors.\" M17-X\"",
            json!("h"),
            "m17.reflectors.\" M17-X\": A reflector name is letters, digits and '-' only.",
        );
        bad(
            "m17.reflectors",
            json!({}),
            "m17.reflectors: only null is accepted (it removes the table)",
        );
        bad(
            "m17.callsign",
            json!("AJ7HR"),
            "unknown setting \"m17.callsign\"",
        );
    }

    /// The page and the boot log say the same thing about the same value:
    /// each case is refused by `validate_field` and by the loader with one
    /// identical sentence.
    #[test]
    fn m17_field_sentences_are_the_loaders_sentences() {
        for (path, value, toml) in [
            (
                "m17.directory",
                json!("reflectors.json"),
                "[m17]\ndirectory = \"reflectors.json\"\n",
            ),
            (
                "m17.codec_dirs",
                json!(["lib"]),
                "[m17]\ncodec_dirs = [\"lib\"]\n",
            ),
            (
                "m17.reflectors.\"M17-XYZ\"",
                json!("2001:db8::1"),
                "[m17.reflectors]\n\"M17-XYZ\" = \"2001:db8::1\"\n",
            ),
            (
                "m17.reflectors.\"M17-XYZ\"",
                json!("h:0"),
                "[m17.reflectors]\n\"M17-XYZ\" = \"h:0\"\n",
            ),
            (
                "m17.reflectors.\"M1\"",
                json!("h"),
                "[m17.reflectors]\n\"M1\" = \"h\"\n",
            ),
        ] {
            let loader = NodeFileConfig::from_toml_str(&(minimal() + toml))
                .expect_err("the loader refuses it");
            let page = validate_field(path, &value)
                .expect_err("the page refuses it")
                .message;
            assert_eq!(page, loader, "{path}");
        }
    }

    #[test]
    fn the_redacted_view_carries_m17_and_leaves_an_empty_codec_list_out() {
        let m17_of = |toml: &str| -> BTreeMap<String, Value> {
            redacted(&(minimal() + toml))
                .0
                .into_iter()
                .filter(|(k, _)| k.starts_with("m17"))
                .collect()
        };
        assert_eq!(m17_of(""), BTreeMap::new(), "no section, no keys");
        assert_eq!(
            m17_of(
                "[m17]\ndirectory = \"bundled\"\ncodec_dirs = [\"/opt/astar/lib\"]\n\
                 [m17.reflectors]\n\"M17-XYZ\" = \"m17.example.net:17000\"\nm17-abc = \"10.0.0.2\"\n"
            ),
            BTreeMap::from([
                ("m17.codec_dirs".to_string(), json!(["/opt/astar/lib"])),
                ("m17.directory".to_string(), json!("bundled")),
                ("m17.enabled".to_string(), json!(true)),
                (
                    "m17.reflectors.\"M17-XYZ\"".to_string(),
                    json!("m17.example.net:17000")
                ),
                ("m17.reflectors.\"m17-abc\"".to_string(), json!("10.0.0.2")),
            ])
        );
        assert_eq!(
            m17_of("[m17]\nenabled = false\ncodec_dirs = []\n"),
            BTreeMap::from([("m17.enabled".to_string(), json!(false))]),
            "an empty codec_dirs is the same config as none"
        );
    }

    #[test]
    fn tts_binary_must_name_an_existing_file_by_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let piper = dir.path().join("piper");
        std::fs::write(&piper, b"#!/bin/sh\n").unwrap();
        let piper = piper.to_str().unwrap().to_string();
        let path = "announce.tts.binary";

        ok(path, json!(piper));
        ok(path, Value::Null);
        bad(
            path,
            json!("piper"),
            "announce.tts.binary: \"piper\" is not an absolute path; name the program by its full path",
        );
        let missing = dir.path().join("nope").to_str().unwrap().to_string();
        bad(
            path,
            json!(missing),
            &format!("announce.tts.binary: {missing:?} is not an existing file"),
        );
        let d = dir.path().to_str().unwrap().to_string();
        bad(
            path,
            json!(d),
            &format!("announce.tts.binary: {d:?} is not an existing file"),
        );
        bad(path, json!(1), "announce.tts.binary: expected a string");
    }

    const TTS_BINARY_ACK: &str = "announce.tts.binary is a program the node runs for every spoken announcement; it needs the announce.tts.binary acknowledgement";

    #[test]
    fn changing_the_tts_binary_needs_the_acknowledgement() {
        let with = |b: Option<&str>| {
            let mut e = vec![("announce.tts.enabled", json!(true))];
            if let Some(b) = b {
                e.push(("announce.tts.binary", json!(b)));
            }
            cfg(&e)
        };
        let none = with(None);
        let a = with(Some("/usr/local/bin/piper"));
        let b = with(Some("/opt/piper/piper"));
        let want = needs("announce.tts.binary", TTS_BINARY_ACK, ACK_TTS_BINARY);

        assert_eq!(
            check_acknowledgements(&none, &a, &req(&[]), &LOOPBACK),
            want
        );
        assert_eq!(check_acknowledgements(&a, &b, &req(&[]), &LOOPBACK), want);
        assert_eq!(
            check_acknowledgements(&a, &b, &req(&["announce.tts.binary"]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&a, &a, &req(&[]), &LOOPBACK),
            Ok(()),
            "an unrelated save does not re-ask"
        );
        assert_eq!(
            check_acknowledgements(&a, &none, &req(&[]), &LOOPBACK),
            Ok(()),
            "removing the key falls back to the default and runs nothing new"
        );
        assert_eq!(ACK_TTS_BINARY, "announce.tts.binary");
    }

    #[test]
    fn adding_a_codec_dir_needs_the_acknowledgement_and_narrowing_does_not() {
        let on = |dirs: Option<Value>| {
            let mut e = vec![("m17.enabled", json!(true))];
            if let Some(d) = dirs {
                e.push(("m17.codec_dirs", d));
            }
            cfg(&e)
        };
        let none = on(None);
        let one = on(Some(json!(["/opt/astar/lib"])));
        let two = on(Some(json!(["/opt/astar/lib", "/usr/local/opt/codec2/lib"])));
        let swapped = on(Some(json!(["/usr/local/opt/codec2/lib", "/opt/astar/lib"])));
        let want = needs("m17.codec_dirs", M17_CODEC_ACK, ACK_M17_CODEC_DIRS);

        assert_eq!(
            check_acknowledgements(&none, &one, &req(&[]), &LOOPBACK),
            want,
            "the first directory"
        );
        assert_eq!(
            check_acknowledgements(&one, &two, &req(&[]), &LOOPBACK),
            want,
            "a second directory"
        );
        assert_eq!(
            check_acknowledgements(&none, &one, &req(&["m17.codec_dirs"]), &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&two, &one, &req(&[]), &LOOPBACK),
            Ok(()),
            "narrowing loads nothing new"
        );
        assert_eq!(
            check_acknowledgements(&two, &swapped, &req(&[]), &LOOPBACK),
            Ok(()),
            "reordering loads nothing that was not already trusted"
        );
        assert_eq!(
            check_acknowledgements(&one, &one, &req(&[]), &LOOPBACK),
            Ok(()),
            "an unrelated save does not re-ask"
        );
        assert_eq!(
            check_acknowledgements(&two, &cfg(&[]), &req(&[]), &LOOPBACK),
            Ok(()),
            "removing [m17] loads nothing"
        );
        // Last in table order: an earlier rule that also fires wins.
        let dtmf_off = cfg(&[("m17.enabled", json!(true)), ("dtmf.enabled", json!(false))]);
        let dtmf_on_and_a_dir = cfg(&[
            ("m17.enabled", json!(true)),
            ("dtmf.enabled", json!(true)),
            ("m17.codec_dirs", json!(["/opt/astar/lib"])),
        ]);
        assert_eq!(
            check_acknowledgements(&dtmf_off, &dtmf_on_and_a_dir, &req(&[]), &LOOPBACK),
            needs(
                "dtmf.enabled",
                "dtmf.enabled lets any connected member command links; it needs the dtmf.enabled acknowledgement",
                ACK_DTMF_ENABLED
            )
        );
        assert_eq!(ACK_M17_CODEC_DIRS, "m17.codec_dirs");
    }

    #[test]
    fn a_save_that_creates_m17_must_say_whether_it_is_on() {
        let without = cfg(&[]);
        let with = cfg(&[
            ("m17.enabled", json!(true)),
            ("m17.reflectors.\"M17-XYZ\"", json!("m17.example.net")),
        ]);
        let only_entry = SettingsPatch {
            etag: String::new(),
            patch: BTreeMap::from([(
                "m17.reflectors.\"M17-XYZ\"".to_string(),
                json!("m17.example.net"),
            )]),
            acknowledge: vec![],
        };
        let refused = Err(FieldError {
            field: "m17.enabled".to_string(),
            message: M17_OPT_IN.to_string(),
            acknowledge: None,
        });
        assert_eq!(
            check_acknowledgements(&without, &with, &only_entry, &LOOPBACK),
            refused
        );
        let said = SettingsPatch {
            patch: BTreeMap::from([
                ("m17.enabled".to_string(), json!(true)),
                (
                    "m17.reflectors.\"M17-XYZ\"".to_string(),
                    json!("m17.example.net"),
                ),
            ]),
            ..only_entry.clone()
        };
        assert_eq!(
            check_acknowledgements(&without, &with, &said, &LOOPBACK),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(&with, &with, &only_entry, &LOOPBACK),
            Ok(()),
            "a section already on disk has said"
        );
        let acked = SettingsPatch {
            acknowledge: vec!["m17.codec_dirs".to_string(), "m17.enabled".to_string()],
            ..only_entry.clone()
        };
        assert_eq!(
            check_acknowledgements(&without, &with, &acked, &LOOPBACK),
            refused,
            "no acknowledgement stands in for saying it"
        );
    }

    #[test]
    fn a_new_m17_section_is_written_and_reads_back_through_the_loader() {
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[
                ("m17.enabled", json!(false)),
                ("m17.reflectors.\"M17-XYZ\"", json!("m17.example.net:17001")),
            ]),
        )
        .expect("valid entries");
        let m = oracle(&out)
            .expect("the loader takes it")
            .m17
            .expect("the section exists");
        assert!(!m.enabled);
        assert_eq!(m.directory, None);
        assert_eq!(m.codec_dirs, [] as [std::string::String; 0]);
        assert_eq!(
            m.reflectors,
            BTreeMap::from([("M17-XYZ".to_string(), "m17.example.net:17001".to_string())])
        );

        let dropped = apply_patch(&out, &patch(&[("m17.reflectors.\"M17-XYZ\"", Value::Null)]))
            .expect("null removes an entry");
        assert_eq!(
            oracle(&dropped)
                .expect("still loads")
                .m17
                .expect("the section stays")
                .reflectors,
            BTreeMap::new()
        );
        let gone =
            apply_patch(&out, &patch(&[("m17", Value::Null)])).expect("null removes the section");
        assert!(oracle(&gone).expect("still loads").m17.is_none());
    }

    #[test]
    fn the_oracle_refuses_two_reflector_names_that_differ_only_in_case() {
        // Each entry is a valid field on its own; only the loader's
        // whole-table check sees the clash, so the oracle has to run it.
        let text = minimal() + "[m17]\n[m17.reflectors]\n\"m17-xyz\" = \"10.0.0.2\"\n";
        let out = apply_patch(
            &text,
            &patch(&[("m17.reflectors.\"M17-XYZ\"", json!("10.0.0.3"))]),
        )
        .expect("the entry is valid on its own");
        assert_eq!(
            oracle(&out).unwrap_err(),
            "m17.reflectors: \"M17-XYZ\" and \"m17-xyz\" name the same reflector"
        );
    }

    // ---- [vocoder] -----------------------------------------------------------

    const VOCODER_ACK_TEXT: &str = "vocoder.trusted_serials: a serial new to the list lets the node open that USB port as a vocoder, and opening a radio's CAT/PTT cable keys it; it needs the vocoder.trusted_serials acknowledgement";

    #[test]
    fn vocoder_paths_are_restart_tier_and_nothing_widens_the_list() {
        for p in ["vocoder", "vocoder.trusted_serials"] {
            assert_eq!(tier(p), Some(Tier::Restart), "{p}");
        }
        for p in [
            "vocoder.port",
            "vocoder.scan",
            "vocoder.any",
            "vocoder.trusted_serials.0",
            "\"vocoder\".trusted_serials",
        ] {
            assert_eq!(tier(p), None, "{p}");
        }
        assert!(is_removable("vocoder"));
        assert!(!is_removable("vocoder.trusted_serials"));
    }

    /// astar-dstar-perens: `vocoder.dstar` is a closed, exact-case picker of
    /// "dongle" and "software", restart-tier like the rest of the section,
    /// and no other mode has a vocoder path at all.
    #[test]
    fn the_dstar_vocoder_is_a_closed_picker_and_dstar_only() {
        let path = "vocoder.dstar";
        assert_eq!(tier(path), Some(Tier::Restart));
        assert_eq!(validate_field(path, &json!("dongle")), Ok(()));
        assert_eq!(validate_field(path, &json!("software")), Ok(()));
        assert_eq!(validate_field(path, &Value::Null), Ok(()));
        for bad in ["Software", "hardware", "auto", ""] {
            assert!(validate_field(path, &json!(bad)).is_err(), "{bad:?}");
        }
        for other in [
            "vocoder.ysf",
            "vocoder.nxdn",
            "vocoder.dmr",
            "vocoder.software",
        ] {
            assert_eq!(tier(other), None, "{other}");
        }
        let v = redacted(&(minimal() + "[vocoder]\ndstar = \"dongle\"\n"));
        assert_eq!(v.get("vocoder.dstar"), Some(&json!("dongle")));
    }

    #[test]
    fn vocoder_serials_are_checked_by_the_loaders_rule() {
        let path = "vocoder.trusted_serials";
        assert_eq!(
            validate_field(path, &json!(["TESTSER1", "TESTSER2"])),
            Ok(())
        );
        assert_eq!(validate_field(path, &json!([])), Ok(()));
        assert_eq!(
            validate_field(path, &Value::Null),
            Ok(()),
            "null removes the key"
        );
        assert_eq!(
            validate_field(path, &json!("TESTSER1")),
            Err(FieldError::new(
                path,
                "vocoder.trusted_serials: expected a list of strings".to_string()
            ))
        );
        for bad in ["TEST SER", "", "A,B", " TESTSER1"] {
            let want = crate::config::check_vocoder_serial(bad).expect_err("the loader refuses it");
            assert_eq!(
                validate_field(path, &json!([bad])),
                Err(FieldError::new(path, want)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_redacted_view_carries_the_vocoder_list_when_the_section_exists() {
        let vocoder_of = |toml: &str| -> BTreeMap<String, Value> {
            redacted(&(minimal() + toml))
                .0
                .into_iter()
                .filter(|(k, _)| k.starts_with("vocoder"))
                .collect()
        };
        assert_eq!(vocoder_of(""), BTreeMap::new());
        assert_eq!(
            vocoder_of("[vocoder]\n"),
            BTreeMap::from([("vocoder.trusted_serials".to_string(), json!([]))])
        );
        assert_eq!(
            vocoder_of("[vocoder]\ntrusted_serials = [\"TESTSER1\"]\n"),
            BTreeMap::from([("vocoder.trusted_serials".to_string(), json!(["TESTSER1"]))])
        );
    }

    #[test]
    fn trusting_a_new_vocoder_serial_needs_the_acknowledgement_and_removing_does_not() {
        let list = |s: &[&str]| cfg(&[("vocoder.trusted_serials", json!(s))]);
        let want = needs(
            "vocoder.trusted_serials",
            VOCODER_ACK_TEXT,
            ACK_VOCODER_SERIALS,
        );
        assert_eq!(
            check_acknowledgements(&cfg(&[]), &list(&["TESTSER1"]), &req(&[]), &LOOPBACK),
            want,
            "the first serial"
        );
        assert_eq!(
            check_acknowledgements(
                &list(&["TESTSER1"]),
                &list(&["TESTSER1", "TESTSER2"]),
                &req(&[]),
                &LOOPBACK
            ),
            want,
            "a second serial"
        );
        assert_eq!(
            check_acknowledgements(
                &cfg(&[]),
                &list(&["TESTSER1"]),
                &req(&["vocoder.trusted_serials"]),
                &LOOPBACK
            ),
            Ok(())
        );
        assert_eq!(
            check_acknowledgements(
                &list(&["TESTSER1", "TESTSER2"]),
                &list(&["TESTSER2"]),
                &req(&[]),
                &LOOPBACK
            ),
            Ok(()),
            "removing a stick opens nothing new"
        );
        assert_eq!(
            check_acknowledgements(&list(&["TESTSER1"]), &cfg(&[]), &req(&[]), &LOOPBACK),
            Ok(()),
            "removing [vocoder] opens nothing"
        );
        assert_eq!(
            check_acknowledgements(
                &list(&["TESTSER1"]),
                &list(&["TESTSER1"]),
                &req(&[]),
                &LOOPBACK
            ),
            Ok(()),
            "an unrelated save does not re-ask"
        );
    }

    #[test]
    fn a_vocoder_serial_is_written_into_node_toml_and_the_loader_reads_it_back() {
        let out = apply_patch(
            NODE_TOML_TEMPLATE,
            &patch(&[("vocoder.trusted_serials", json!(["TESTSER1"]))]),
        )
        .expect("a valid serial patches");
        assert!(
            out.contains("[vocoder]\ntrusted_serials = [\"TESTSER1\"]"),
            "{out}"
        );
        let cfg = oracle(&out).expect("the patched file boots");
        assert_eq!(cfg.vocoder_trust().serials, vec!["TESTSER1"]);
        let removed = apply_patch(&out, &patch(&[("vocoder", Value::Null)])).unwrap();
        assert!(!removed.contains("[vocoder]"));
        assert!(oracle(&removed).unwrap().vocoder_trust().is_empty());
    }

    // ---- [web] and [stream] ----------------------------------------------------

    #[test]
    fn web_and_stream_paths_are_restart_tier_and_closed() {
        for p in [
            "web",
            "web.public_bind",
            "web.public_hostnames",
            "web.acme_email",
            "web.acme_staging",
            "web.acme_http_bind",
            "web.acme_challenge",
            "web.dns",
            "web.dns.provider",
            "web.dns.domain",
            "web.dns.host",
            "web.dns.ddns",
            "web.dns.ddns_interval_secs",
            "web.dns.ttl",
            "stream",
            "stream.enabled",
            "stream.max_listeners",
            "stream.mp3_kbps",
            "stream.opus_kbps",
        ] {
            assert_eq!(tier(p), Some(Tier::Restart), "{p}");
        }
        // No credential path: the provider's keys live in [secrets] only.
        for p in [
            "web.dns.apikey",
            "web.dns.secretapikey",
            "web.dns.key",
            "web.cert",
            "stream.password",
        ] {
            assert_eq!(tier(p), None, "{p}");
        }
        assert!(is_removable("web") && is_removable("web.dns") && is_removable("stream"));
        ok("web.public_hostnames", json!(["{node}.example.net"]));
        ok("web.acme_challenge", json!("dns-01"));
        ok("web.dns.provider", json!("porkbun"));
        ok("stream.enabled", json!(true));
        bad(
            "web.dns.provider",
            json!("cloudflare"),
            "web.dns.provider: unknown value \"cloudflare\" (expected \"porkbun\")",
        );
    }

    #[test]
    fn web_and_stream_reach_the_mirror_as_written() {
        let text = format!(
            "{}[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"69586\"\n\
             [web]\npublic_bind = \"0.0.0.0:443\"\n\
             [web.dns]\nprovider = \"porkbun\"\ndomain = \"example.net\"\nhost = \"{{node}}\"\n\
             [stream]\nenabled = true\n",
            minimal()
        );
        let m = redacted(&text);
        assert_eq!(m.get("stream.enabled"), Some(&json!(true)));
        assert_eq!(m.get("stream.max_listeners"), Some(&json!(25)));
        assert_eq!(m.get("web.public_bind"), Some(&json!("0.0.0.0:443")));
        assert_eq!(m.get("web.dns.host"), Some(&json!("{node}")));
        assert_eq!(m.get("web.dns.provider"), Some(&json!("porkbun")));
        assert_eq!(m.get("web.public_hostnames"), None, "absent stays absent");
        let m = redacted(&format!(
            "{}[web]\npublic_hostnames = [\"Node.Example.net\"]\n",
            minimal()
        ));
        assert_eq!(
            m.get("web.public_hostnames"),
            Some(&json!(["Node.Example.net"]))
        );
    }

    // ---- [packet_log] ------------------------------------------------------

    #[test]
    fn packet_log_paths_are_live_tier() {
        for p in [
            "packet_log",
            "packet_log.debug_mode",
            "packet_log.dir",
            "packet_log.max_bytes",
        ] {
            assert_eq!(tier(p), Some(Tier::Live), "{p}");
        }
        assert_eq!(tier("packet_log.enabled"), None);
        assert_eq!(tier("packet_log.keep"), None);
        assert!(is_removable("packet_log"));
        assert!(!is_removable("packet_log.dir"));
        ok("packet_log.debug_mode", json!(true));
        ok("packet_log.max_bytes", json!("2 GB"));
        assert!(validate_field("packet_log.debug_mode", &json!(null)).is_err());
        assert!(validate_field("packet_log.dir", &json!("")).is_err());
    }

    #[test]
    fn packet_log_reaches_the_mirror_as_written() {
        let text = format!("{}[packet_log]\nmax_bytes = \"500MB\"\n", minimal());
        let m = redacted(&text);
        assert_eq!(m.get("packet_log.max_bytes"), Some(&json!("500MB")));
        assert_eq!(m.get("packet_log.debug_mode"), Some(&json!(false)));
        assert_eq!(
            m.get("packet_log.dir"),
            Some(&json!("/var/lib/astar/packet-logs"))
        );
        assert_eq!(redacted(&minimal()).get("packet_log.max_bytes"), None);
        let patch: BTreeMap<String, Value> =
            m.0.iter()
                .filter(|(k, _)| k.starts_with("packet_log."))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        assert_eq!(patch.len(), 3, "{patch:?}");
        let saved = apply_patch(&minimal(), &patch).expect("the mirror saves back");
        let p = oracle(&saved).expect("and loads").packet_log.unwrap();
        assert_eq!(p.max_bytes.text, "500MB");
    }

    // ---- [recording] -------------------------------------------------------

    #[test]
    fn recording_paths_are_restart_tier() {
        for p in [
            "recording",
            "recording.enabled",
            "recording.dir",
            "recording.max_bytes",
            "recording.max_age_days",
            "recording.frames",
        ] {
            assert_eq!(tier(p), Some(Tier::Restart), "{p}");
        }
        assert_eq!(tier("recording.keep"), None);
        assert!(is_removable("recording"));
        assert!(!is_removable("recording.dir"));
        ok("recording.enabled", json!(true));
        ok("recording.frames", json!(null));
        ok("recording.max_bytes", json!("2 GB"));
        ok("recording.max_age_days", json!(30));
        assert!(validate_field("recording.enabled", &json!(null)).is_err());
        assert!(validate_field("recording.dir", &json!("")).is_err());
    }

    #[test]
    fn recording_reaches_the_mirror_as_written() {
        let text = format!("{}[recording]\nmax_bytes = \"500MB\"\n", minimal());
        let m = redacted(&text);
        assert_eq!(m.get("recording.max_bytes"), Some(&json!("500MB")));
        assert_eq!(m.get("recording.enabled"), Some(&json!(false)));
        assert_eq!(
            m.get("recording.dir"),
            Some(&json!("/var/lib/astar/recordings"))
        );
        assert_eq!(m.get("recording.max_age_days"), Some(&json!(30)));
        assert_eq!(m.get("recording.frames"), Some(&json!(true)));
        assert_eq!(redacted(&minimal()).get("recording.max_bytes"), None);

        // Saving the mirror back: every mirrored value is a valid field and,
        // written over the file, loads to the same sizes (F13).
        let patch: BTreeMap<String, Value> =
            m.0.iter()
                .filter(|(k, _)| k.starts_with("recording."))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        assert_eq!(patch.len(), 5, "{patch:?}");
        let saved = apply_patch(&minimal(), &patch).expect("the mirror saves back");
        let r = oracle(&saved).expect("and loads").recording.unwrap();
        assert_eq!(
            (r.max_bytes.bytes, r.max_bytes.text.as_str()),
            (500_000_000, "500MB")
        );
        assert_eq!(redacted(&saved).0, m.0);
    }
}
