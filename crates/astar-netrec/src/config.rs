// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `netrec.toml`: what to record, where to keep it, and when.
//!
//! Loading is two steps. [`from_toml_str`] reads the file into typed values
//! and refuses anything it cannot represent (an unknown key, a network this
//! build does not record, a bad size or duration). [`validate`] then checks
//! the rules that span several values (overlapping windows, a private net
//! asking for `YouTube`) and returns warnings for things that work but look
//! wrong.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use astar_netarchive::net_dir_name;
use astar_station::{DstarVocoder, VocoderTrust};
use serde::Deserialize;

use crate::listen::{Network, login_callsign};

/// Why a config could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file concerned.
        path: PathBuf,
        /// The operating-system error.
        source: std::io::Error,
    },
    /// The text is not valid `netrec.toml`.
    #[error("netrec.toml: {0}")]
    Parse(String),
    /// The text parses but a value is not acceptable.
    #[error("{0}")]
    Invalid(String),
}

/// Who may see a net's recordings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Signed-in viewers only. The default.
    #[default]
    Private,
    /// Anyone with the link.
    Public,
}

/// `[recorder]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderCfg {
    /// Where sessions are kept.
    pub root: PathBuf,
    /// The callsign the recorder logs in with. It never transmits.
    pub callsign: String,
    /// The letter appended to the callsign on a reflector login.
    pub callsign_suffix: char,
    /// Callsigns that count as "my side".
    pub my_callsigns: Vec<String>,
    /// The most the archive may hold, in bytes.
    pub max_bytes: u64,
    /// The oldest a session may get, 1 to 3650 days.
    pub max_age_days: u32,
    /// How long before a net's start the recorder joins.
    pub pre_roll: Duration,
    /// How long after a net's end the recorder stays.
    pub post_roll: Duration,
}

/// `[vocoder]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VocoderCfg {
    /// The D-Star vocoder: dongle (default) or software.
    pub dstar: DstarVocoder,
    /// USB serial numbers of the sticks the dongle may be, trimmed and
    /// de-duplicated. Same meaning as `node.toml`'s: nothing but a serial
    /// written here is ever opened, and none means no port is touched.
    pub trusted_serials: Vec<String>,
}

/// `[web]`. Stage 2 serves the pages; stage 1 only reads the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebCfg {
    /// Serve the pages on the loopback address.
    pub local: bool,
    /// Where to serve them.
    pub bind: SocketAddr,
}

/// What a net listens to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An M17 reflector.
    M17 {
        /// Reflector hostname or address.
        host: String,
        /// UDP port.
        port: u16,
        /// Module letter, `A` to `Z`.
        module: char,
    },
    /// A D-Star reflector.
    Dstar {
        /// Reflector hostname or address.
        host: String,
        /// UDP port.
        port: u16,
        /// Module letter, `A` to `Z`.
        module: char,
        /// The reflector's name, e.g. `XLX458`, if known.
        reflector: Option<String>,
    },
}

impl Target {
    /// The network this target is on.
    #[must_use]
    pub fn network(&self) -> Network {
        match self {
            Self::M17 { .. } => Network::M17,
            Self::Dstar { .. } => Network::Dstar,
        }
    }
}

/// One `[[net]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent switches the file spells out
pub struct NetCfg {
    /// The net's name; it appears in errors and on the page.
    pub name: String,
    /// What to listen to.
    pub target: Target,
    /// The schedule text, e.g. `"Tue 19:00"`. Parsed by the scheduler.
    pub when: String,
    /// How long the net runs.
    pub duration: Duration,
    /// The zone `when` is read in.
    pub tz: jiff::tz::TimeZone,
    /// Who may see it.
    pub visibility: Visibility,
    /// Default the page to "my side".
    pub my_side: bool,
    /// Render video, on unless switched off (read now, used in a later stage).
    pub video: bool,
    /// Stream live to `YouTube` (later stage).
    pub youtube_live: bool,
    /// Upload to `YouTube` afterwards (later stage).
    pub youtube_upload: bool,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `[recorder]`.
    pub recorder: RecorderCfg,
    /// `[vocoder]`.
    pub vocoder: VocoderCfg,
    /// `[web]`.
    pub web: WebCfg,
    /// Every `[[net]]`.
    pub nets: Vec<NetCfg>,
}

// ---- the raw shape of the file ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    recorder: RawRecorder,
    #[serde(default)]
    web: RawWeb,
    #[serde(default)]
    vocoder: RawVocoder,
    #[serde(default, rename = "net")]
    nets: Vec<RawNet>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecorder {
    root: PathBuf,
    callsign: String,
    #[serde(default = "default_suffix")]
    callsign_suffix: char,
    #[serde(default)]
    my_callsigns: Vec<String>,
    #[serde(default = "default_max_bytes")]
    max_bytes: String,
    #[serde(default = "default_max_age")]
    max_age_days: u32,
    #[serde(default = "default_pre")]
    pre_roll: String,
    #[serde(default = "default_post")]
    post_roll: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWeb {
    #[serde(default)]
    local: bool,
    #[serde(default = "default_bind")]
    bind: SocketAddr,
}

impl Default for RawWeb {
    fn default() -> Self {
        Self {
            local: false,
            bind: default_bind(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawVocoder {
    dstar: Option<String>,
    #[serde(default)]
    trusted_serials: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct RawNet {
    name: String,
    network: String,
    host: Option<String>,
    port: Option<u16>,
    module: String,
    reflector: Option<String>,
    when: String,
    duration: String,
    tz: String,
    #[serde(default)]
    visibility: Visibility,
    #[serde(default)]
    my_side: bool,
    #[serde(default = "default_video")]
    video: bool,
    #[serde(default)]
    youtube_live: bool,
    #[serde(default)]
    youtube_upload: bool,
}

fn default_suffix() -> char {
    'R'
}
fn default_max_bytes() -> String {
    "50 GB".to_string()
}
fn default_max_age() -> u32 {
    365
}
fn default_pre() -> String {
    "5m".to_string()
}
fn default_post() -> String {
    "10m".to_string()
}
fn default_video() -> bool {
    true
}
fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8050))
}

/// Parse a size like `"50 GB"`: same grammar as astar-server's. `B`, `KB`,
/// `MB` and `GB` are decimal; `KiB`, `MiB` and `GiB` are binary; units ignore
/// case; a bare number is bytes.
fn parse_size(s: &str) -> Result<u64, String> {
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

/// Parse a duration made of number-and-unit pieces: `"90m"`, `"1h"`,
/// `"1h30m"`, `"45s"`.
///
/// # Errors
/// A sentence naming the text, for an empty string, a missing number or
/// unit, or an unknown unit.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let refuse = || format!("{s:?} is not a duration such as \"90m\", \"1h30m\" or \"45s\"");
    let mut rest = s.trim();
    if rest.is_empty() {
        return Err(refuse());
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 || digits == rest.len() {
            return Err(refuse());
        }
        let n: u64 = rest[..digits].parse().map_err(|_| refuse())?;
        let scale = match rest.as_bytes()[digits] {
            b'h' => 3600,
            b'm' => 60,
            b's' => 1,
            _ => return Err(refuse()),
        };
        total = n
            .checked_mul(scale)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(refuse)?;
        rest = &rest[digits + 1..];
    }
    Ok(Duration::from_secs(total))
}

/// The longest a roll or a net may be.
const DAY: Duration = Duration::from_secs(86_400);

fn invalid(s: String) -> ConfigError {
    ConfigError::Invalid(s)
}

fn one_char(net: &str, what: &str, s: &str) -> Result<char, ConfigError> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(invalid(format!(
            "net {net:?}: {what} must be a single letter, not {s:?}"
        ))),
    }
}

fn convert_net(r: RawNet) -> Result<NetCfg, ConfigError> {
    let name = r.name.clone();
    let module = one_char(&name, "module", &r.module)?;
    let host = |r: &RawNet| {
        r.host
            .clone()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| invalid(format!("net {:?}: host is required", r.name)))
    };
    let target = match r.network.as_str() {
        "m17" => {
            if r.reflector.is_some() {
                return Err(invalid(format!(
                    "net {name:?}: looking up an M17 reflector by name is not yet supported; give host, port and module"
                )));
            }
            Target::M17 {
                host: host(&r)?,
                port: r.port.unwrap_or(17000),
                module,
            }
        }
        "dstar" => Target::Dstar {
            host: host(&r)?,
            port: r.port.unwrap_or(30001),
            module,
            reflector: r.reflector.clone(),
        },
        "allstar" | "echolink" => {
            return Err(invalid(format!(
                "net {name:?}: network {:?} is not yet supported by astar-netrec (see the netrec docs); use \"m17\" or \"dstar\"",
                r.network
            )));
        }
        other => {
            return Err(invalid(format!(
                "net {name:?}: unknown network {other:?}; use \"m17\" or \"dstar\""
            )));
        }
    };
    let duration =
        parse_duration(&r.duration).map_err(|e| invalid(format!("net {name:?}: duration: {e}")))?;
    let tz = jiff::tz::TimeZone::get(&r.tz)
        .map_err(|e| invalid(format!("net {name:?}: tz {:?}: {e}", r.tz)))?;
    Ok(NetCfg {
        name,
        target,
        when: r.when,
        duration,
        tz,
        visibility: r.visibility,
        my_side: r.my_side,
        video: r.video,
        youtube_live: r.youtube_live,
        youtube_upload: r.youtube_upload,
    })
}

/// Read a config from text.
///
/// # Errors
/// [`ConfigError::Parse`] for malformed TOML or an unknown key;
/// [`ConfigError::Invalid`] for a bad size, duration, zone or network
/// (`AllStar` and `EchoLink` say "not yet supported").
pub fn from_toml_str(s: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let r = raw.recorder;
    let recorder = RecorderCfg {
        root: r.root,
        callsign: r.callsign,
        callsign_suffix: r.callsign_suffix,
        my_callsigns: r.my_callsigns,
        max_bytes: parse_size(&r.max_bytes).map_err(|e| invalid(format!("max_bytes: {e}")))?,
        max_age_days: r.max_age_days,
        pre_roll: parse_duration(&r.pre_roll).map_err(|e| invalid(format!("pre_roll: {e}")))?,
        post_roll: parse_duration(&r.post_roll).map_err(|e| invalid(format!("post_roll: {e}")))?,
    };
    let dstar = match raw.vocoder.dstar {
        None => DstarVocoder::default(),
        Some(v) => DstarVocoder::parse(&v).ok_or_else(|| {
            invalid(format!(
                "[vocoder] dstar: {v:?} is not \"dongle\" or \"software\""
            ))
        })?,
    };
    let nets = raw
        .nets
        .into_iter()
        .map(convert_net)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Config {
        recorder,
        vocoder: VocoderCfg {
            dstar,
            trusted_serials: VocoderTrust::new(raw.vocoder.trusted_serials).serials,
        },
        web: WebCfg {
            local: raw.web.local,
            bind: raw.web.bind,
        },
        nets,
    })
}

const TEMPLATE: &str = r#"# netrec.toml: configuration for astar-netrec, the net recorder.
# astar-netrec only listens. It logs in to reflectors but never transmits.

[recorder]
# Where sessions are kept.
root = "/var/lib/astar/netrec"
# The callsign the recorder logs in with.
callsign = "N0CALL"
callsign_suffix = "R"
# Callsigns that count as "my side" on the page.
my_callsigns = ["N0CALL"]
# Delete the oldest sessions past this size, or older than max_age_days (1-3650).
max_bytes = "50 GB"
max_age_days = 365
# Join this long before a net starts, and stay this long after it ends.
pre_roll = "5m"
post_roll = "10m"

[web]
# Serving the pages arrives in a later stage; these keys are read now.
local = false
bind = "127.0.0.1:8050"

[vocoder]
# "dongle" (ThumbDV/DVstick) or "software" (D-Star only).
dstar = "dongle"
# The USB serial of each dongle that may be opened. Nothing else ever is, so a
# D-Star net on "dongle" needs one here.
trusted_serials = []

# One [[net]] table per scheduled net. Uncomment and edit.
#
# [[net]]
# name = "Tuesday M17 net"
# network = "m17"              # "m17" or "dstar"
# host = "m17.example.org"     # the reflector's address
# port = 17000                 # optional: 17000 for M17, 30001 for D-Star
# module = "C"
# when = "Tue 19:00"           # "Mon Wed Fri 07:30", "daily 21:00", or "2026-11-14 18:00"
# duration = "90m"
# tz = "America/Denver"
# visibility = "private"       # or "public"
# my_side = false
"#;

/// Load the config at `path`; if there is no file, write a commented
/// template there (mode 0644) and load that.
///
/// # Errors
/// [`ConfigError::Io`] if the file cannot be read or written, or whatever
/// [`from_toml_str`] says about its contents.
pub fn load_or_bootstrap(path: &Path) -> Result<Config, ConfigError> {
    let io = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    match std::fs::read_to_string(path) {
        Ok(text) => from_toml_str(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            std::fs::write(path, TEMPLATE).map_err(io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
                    .map_err(io)?;
            }
            from_toml_str(TEMPLATE)
        }
        Err(e) => Err(io(e)),
    }
}

/// Check the rules that span values. Returns warnings (things that work
/// but look wrong).
///
/// # Errors
/// [`ConfigError::Invalid`] naming the net (where one is at fault) for a
/// module outside `A` to `Z`, an empty callsign, a suffix that is not one
/// uppercase letter or digit, a callsign that cannot log in on a net's network,
/// a net name unfit for a folder, two nets sharing a name or a folder,
/// overlapping windows, a private net with a `YouTube` switch on, `my_side`
/// with no `my_callsigns`, a software D-Star net in a build without the
/// vocoder, or `max_age_days` outside 1 to 3650.
pub fn validate(cfg: &Config, now: jiff::Timestamp) -> Result<Vec<String>, ConfigError> {
    let warnings = validate_recorder(&cfg.recorder)?;
    validate_folders(&cfg.nets)?;
    for n in &cfg.nets {
        validate_net(cfg, n)?;
    }
    if cfg.nets.iter().any(|n| n.my_side) && cfg.recorder.my_callsigns.is_empty() {
        return Err(invalid(
            "a net sets my_side = true but [recorder] my_callsigns is empty".into(),
        ));
    }
    let horizon = crate::schedule::overlap_horizon_days(cfg, now);
    if let Some(&(a, b, at)) = crate::schedule::overlaps_from(cfg, now, horizon).first() {
        return Err(invalid(format!(
            "nets {:?} and {:?} overlap (including pre- and post-roll) from {at}; move one or shorten the rolls",
            cfg.nets[a].name, cfg.nets[b].name
        )));
    }
    Ok(warnings)
}

/// The rules of `[recorder]` alone.
fn validate_recorder(r: &RecorderCfg) -> Result<Vec<String>, ConfigError> {
    let mut warnings = Vec::new();
    if r.callsign.trim().is_empty() {
        return Err(invalid("[recorder] callsign must not be empty".into()));
    }
    // A space here would log in as the operator's own call.
    if !(r.callsign_suffix.is_ascii_uppercase() || r.callsign_suffix.is_ascii_digit()) {
        return Err(invalid(format!(
            "[recorder] callsign_suffix is {:?}; it must be one uppercase letter or digit",
            r.callsign_suffix
        )));
    }
    if r.callsign == "N0CALL" {
        warnings.push("[recorder] callsign is still the placeholder N0CALL".to_string());
    }
    if !(1..=3650).contains(&r.max_age_days) {
        return Err(invalid(format!(
            "[recorder] max_age_days is {}; it must be 1 to 3650",
            r.max_age_days
        )));
    }
    if r.pre_roll > DAY || r.post_roll > DAY {
        return Err(invalid(
            "[recorder] pre_roll and post_roll must each be 24 hours or less".into(),
        ));
    }
    if r.max_bytes < 500_000_000 {
        warnings.push(
            "[recorder] max_bytes is under 500 MB, so sessions may be deleted soon after they are recorded"
                .to_string(),
        );
    }
    Ok(warnings)
}

/// Every net gets a folder of its own, directly under `root`.
fn validate_folders(nets: &[NetCfg]) -> Result<(), ConfigError> {
    let mut folders: HashMap<String, &str> = HashMap::new();
    for n in nets {
        let folder = net_dir_name(&n.name);
        // ".." would put the sessions outside `root`, where retention never
        // sees them.
        if n.name.trim().is_empty()
            || n.name.contains(['/', '\\'])
            || matches!(folder.as_str(), "." | "..")
        {
            return Err(invalid(format!(
                "net {:?}: the name becomes a folder under root, so it must not be empty, \".\" or \"..\", or hold a slash or backslash",
                n.name
            )));
        }
        match folders.insert(folder.clone(), &n.name) {
            Some(other) if other == n.name => {
                return Err(invalid(format!("the net name {other:?} is used twice")));
            }
            Some(other) => {
                return Err(invalid(format!(
                    "nets {other:?} and {:?} would share the folder {folder:?}; rename one",
                    n.name
                )));
            }
            None => {}
        }
    }
    Ok(())
}

/// The rules of one `[[net]]`.
fn validate_net(cfg: &Config, n: &NetCfg) -> Result<(), ConfigError> {
    let r = &cfg.recorder;
    if let Err(e) = crate::schedule::parse_when(&n.when) {
        return Err(invalid(format!("net {:?}: when: {e}", n.name)));
    }
    if n.duration > DAY {
        return Err(invalid(format!(
            "net {:?}: duration must be 24 hours or less",
            n.name
        )));
    }
    if n.duration.is_zero() {
        return Err(invalid(format!(
            "net {:?}: duration must be longer than zero",
            n.name
        )));
    }
    if matches!(n.target, Target::Dstar { .. })
        && cfg.vocoder.dstar == DstarVocoder::Dongle
        && cfg.vocoder.trusted_serials.is_empty()
    {
        return Err(invalid(format!(
            "net {:?}: [vocoder] dstar = \"dongle\" has no stick to open; add the dongle's serial to trusted_serials, or choose dstar = \"software\"",
            n.name
        )));
    }
    if matches!(n.target, Target::Dstar { .. })
        && cfg.vocoder.dstar == DstarVocoder::Software
        && !cfg!(feature = "dstar-soft-ambe")
    {
        return Err(invalid(format!(
            "net {:?}: [vocoder] dstar = \"software\" needs a build with the dstar-soft-ambe feature, which --features release includes; this build has none, so rebuild or choose dstar = \"dongle\"",
            n.name
        )));
    }
    let (Target::M17 { module, .. } | Target::Dstar { module, .. }) = &n.target;
    if !module.is_ascii_uppercase() {
        return Err(invalid(format!(
            "net {:?}: module {module:?} must be a letter from A to Z",
            n.name
        )));
    }
    if let Err(e) = login_callsign(n.target.network(), &r.callsign, r.callsign_suffix) {
        return Err(invalid(format!("net {:?}: {e}", n.name)));
    }
    if n.visibility == Visibility::Private && (n.youtube_live || n.youtube_upload) {
        return Err(invalid(format!(
            "net {:?}: youtube_live and youtube_upload need visibility = \"public\"",
            n.name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
[recorder]
root = "/var/lib/astar/netrec"
callsign = "N0CALL"
callsign_suffix = "R"
my_callsigns = ["N0CALL"]
max_bytes = "50 GB"
max_age_days = 365
pre_roll = "5m"
post_roll = "10m"

[web]
local = true
bind = "127.0.0.1:8050"

[vocoder]
dstar = "dongle"

[[net]]
name = "Tuesday M17 net"
network = "m17"
host = "m17.example.org"
module = "C"
when = "Tue 19:00"
duration = "90m"
tz = "America/Denver"
visibility = "public"
my_side = false
video = true
youtube_live = false
youtube_upload = false
"#;

    #[test]
    fn spec_example_parses() {
        let c = from_toml_str(SPEC).unwrap();
        assert_eq!(c.recorder.max_bytes, 50_000_000_000);
        assert_eq!(c.recorder.pre_roll.as_secs(), 300);
        assert_eq!(c.nets.len(), 1);
        assert!(matches!(
            &c.nets[0].target,
            Target::M17 {
                port: 17000,
                module: 'C',
                ..
            }
        ));
        assert_eq!(c.nets[0].duration.as_secs(), 5400);
        assert_eq!(c.nets[0].visibility, Visibility::Public);
        validate(&c, jiff::Timestamp::now()).unwrap();
    }

    /// `SPEC` turned into a D-Star net, with `vocoder` as its `[vocoder]`.
    fn dstar_spec(vocoder: &str) -> Config {
        let s = SPEC
            .replace("\"m17\"", "\"dstar\"")
            .replace("dstar = \"dongle\"", vocoder);
        from_toml_str(&s).unwrap()
    }

    #[test]
    fn dongle_net_without_trusted_serial_is_refused() {
        for vocoder in ["", "dstar = \"dongle\"", "trusted_serials = [\"  \"]"] {
            let e = validate(&dstar_spec(vocoder), jiff::Timestamp::now())
                .unwrap_err()
                .to_string();
            assert!(e.contains("Tuesday M17 net"), "{e}");
            assert!(e.contains("trusted_serials"), "{e}");
            assert!(e.contains("\"software\""), "{e}");
        }
    }

    #[test]
    fn dongle_net_with_a_serial_and_software_net_are_accepted() {
        let now = jiff::Timestamp::now();
        let c = dstar_spec("trusted_serials = [\" DV1 \", \"DV1\", \"DV2\"]");
        assert_eq!(c.vocoder.dstar, DstarVocoder::Dongle);
        assert_eq!(c.vocoder.trusted_serials, ["DV1", "DV2"]);
        validate(&c, now).unwrap();
    }

    /// `check` refuses what this build left out, so this holds under both
    /// feature sets: plain (`just ci`) and `--features release`.
    #[test]
    fn software_net_needs_a_build_with_the_vocoder() {
        let r = validate(&dstar_spec("dstar = \"software\""), jiff::Timestamp::now());
        if cfg!(feature = "dstar-soft-ambe") {
            r.unwrap();
        } else {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("Tuesday M17 net"), "{e}");
            assert!(e.contains("dstar-soft-ambe"), "{e}");
        }
    }

    #[test]
    fn m17_net_with_the_software_setting_is_fine_in_any_build() {
        let c = from_toml_str(&SPEC.replace("dstar = \"dongle\"", "dstar = \"software\"")).unwrap();
        validate(&c, jiff::Timestamp::now()).unwrap();
    }

    #[test]
    fn m17_net_needs_no_trusted_serial() {
        let c = from_toml_str(SPEC).unwrap();
        assert_eq!(c.vocoder.dstar, DstarVocoder::Dongle);
        assert_eq!(c.vocoder.trusted_serials, Vec::<String>::new());
        validate(&c, jiff::Timestamp::now()).unwrap();
    }

    #[test]
    fn unknown_key_is_refused() {
        let s = SPEC.replace("[web]", "[web]\nbogus = 1");
        assert!(from_toml_str(&s).is_err());
    }

    #[test]
    fn allstar_net_is_refused_with_not_yet() {
        let s = SPEC.replace("\"m17\"", "\"allstar\"");
        let e = from_toml_str(&s).unwrap_err().to_string();
        assert!(e.contains("not yet supported"), "{e}");
    }

    #[test]
    fn private_net_with_youtube_live_is_refused() {
        let s = SPEC
            .replace("\"public\"", "\"private\"")
            .replace("youtube_live = false", "youtube_live = true");
        let c = from_toml_str(&s).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(e.contains("Tuesday M17 net"), "{e}");
    }

    #[test]
    fn bad_module_empty_callsign_and_my_side_are_refused() {
        let c = from_toml_str(&SPEC.replace("\"C\"", "\"c\"")).unwrap();
        assert!(validate(&c, jiff::Timestamp::now()).is_err());
        let c = from_toml_str(&SPEC.replace("callsign = \"N0CALL\"", "callsign = \"\"")).unwrap();
        assert!(validate(&c, jiff::Timestamp::now()).is_err());
        let s = SPEC
            .replace("my_side = false", "my_side = true")
            .replace("my_callsigns = [\"N0CALL\"]", "my_callsigns = []");
        let c = from_toml_str(&s).unwrap();
        assert!(validate(&c, jiff::Timestamp::now()).is_err());
    }

    #[test]
    fn overlong_callsign_is_refused_by_validate() {
        let long = "callsign = \"N0CALLXX\"";
        let c = from_toml_str(&SPEC.replace("callsign = \"N0CALL\"", long)).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Tuesday M17 net") && e.contains("N0CALLXX"),
            "{e}"
        );
        // Seven characters fit M17's field and D-Star's alike.
        let fits = "callsign = \"N0CALLX\"";
        let c = from_toml_str(&SPEC.replace("callsign = \"N0CALL\"", fits)).unwrap();
        validate(&c, jiff::Timestamp::now()).unwrap();
    }

    #[test]
    fn zero_duration_is_refused() {
        let c = from_toml_str(&SPEC.replace("\"90m\"", "\"0s\"")).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Tuesday M17 net") && e.contains("duration"),
            "{e}"
        );
    }

    #[test]
    fn duration_over_a_day_is_refused() {
        let c = from_toml_str(&SPEC.replace("\"90m\"", "\"25h\"")).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Tuesday M17 net") && e.contains("24 hours"),
            "{e}"
        );
        let c = from_toml_str(&SPEC.replace("\"10m\"", "\"30h\"")).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(e.contains("post_roll"), "{e}");
    }

    #[test]
    fn absurd_duration_is_refused_not_panicking() {
        for d in [
            "9999999999999999h",
            "5124095576030431h",
            "18446744073709551615s",
        ] {
            if let Ok(c) = from_toml_str(&SPEC.replace("\"90m\"", &format!("\"{d}\""))) {
                assert!(validate(&c, jiff::Timestamp::now()).is_err(), "{d}");
            }
        }
    }

    #[test]
    fn overlapping_nets_and_bad_when_are_refused() {
        let second = SPEC[SPEC.find("[[net]]").unwrap()..]
            .replace("Tuesday M17 net", "Second net")
            .replace("Tue 19:00", "Tue 20:00");
        let c = from_toml_str(&format!("{SPEC}\n{second}")).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Tuesday M17 net") && e.contains("Second net"),
            "{e}"
        );
        let c = from_toml_str(&SPEC.replace("Tue 19:00", "whenever")).unwrap();
        let e = validate(&c, jiff::Timestamp::now())
            .unwrap_err()
            .to_string();
        assert!(e.contains("Tuesday M17 net"), "{e}");
    }

    /// `SPEC` with a second net of `name`, on a day clear of the first.
    fn with_second_net(name: &str) -> Config {
        let second = SPEC[SPEC.find("[[net]]").unwrap()..]
            .replace("Tuesday M17 net", name)
            .replace("Tue 19:00", "Fri 19:00");
        from_toml_str(&format!("{SPEC}\n{second}")).unwrap()
    }

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    /// A one-off that clashes with a weekly net far ahead. Waved through
    /// today, it would stop the daemon on a restart nearer the date.
    #[test]
    fn a_far_off_one_off_that_overlaps_is_refused_now() {
        let today = at("2026-11-03T12:00:00Z");
        // 119 and 490 days ahead, both Tuesdays.
        for date in ["2027-03-02", "2028-03-07"] {
            let c = with_second_net("Special event");
            let mut c2 = c.clone();
            c2.nets[1].when = format!("{date} 19:00");
            let e = validate(&c2, today).unwrap_err().to_string();
            assert!(
                e.contains("Tuesday M17 net") && e.contains("Special event"),
                "{date}: {e}"
            );
        }
        // The same date a day later is clear.
        let mut c = with_second_net("Special event");
        c.nets[1].when = "2027-03-03 19:00".to_string();
        validate(&c, today).unwrap();
    }

    #[test]
    fn a_one_off_past_every_bound_or_long_gone_is_no_trouble() {
        let today = at("2026-11-03T12:00:00Z");
        for date in ["9999-12-31", "1999-01-01"] {
            let mut c = with_second_net("Special event");
            c.nets[1].when = format!("{date} 19:00");
            validate(&c, today).unwrap();
        }
    }

    /// Two weekly nets in zones that change their clocks on different dates
    /// meet only in those weeks, which may be most of a year away.
    #[test]
    fn a_clash_only_daylight_saving_brings_is_refused_months_ahead() {
        let today = at("2026-07-01T12:00:00Z");
        let mut c = with_second_net("London net");
        // In UTC the Denver net is 02:00 in winter and 01:00 in summer, and
        // the London net 01:15 and 00:15. They clear each other all year,
        // except in the weeks when Denver has changed its clocks and London
        // has not.
        c.recorder.pre_roll = Duration::ZERO;
        c.recorder.post_roll = Duration::ZERO;
        c.nets[0].duration = Duration::from_secs(1800);
        c.nets[1].duration = Duration::from_secs(1800);
        c.nets[1].when = "Wed 01:15".to_string();
        c.nets[1].tz = jiff::tz::TimeZone::get("Europe/London").unwrap();
        let e = validate(&c, today).unwrap_err().to_string();
        assert!(e.contains("London net"), "{e}");
    }

    #[test]
    fn a_net_name_that_would_leave_the_archive_is_refused() {
        let now = jiff::Timestamp::now();
        for bad in [
            "..", ".", "  ..  ", "", "   ", "a/b", "../x", "a\\\\b", "/etc",
        ] {
            let s = SPEC.replace("\"Tuesday M17 net\"", &format!("\"{bad}\""));
            let c = from_toml_str(&s).unwrap();
            let e = validate(&c, now).unwrap_err().to_string();
            assert!(e.contains("name"), "{bad:?}: {e}");
        }
        for fine in ["Tuesday...net", ".hidden", "..x", "N0CALL's net"] {
            let s = SPEC.replace("\"Tuesday M17 net\"", &format!("{fine:?}"));
            let c = from_toml_str(&s).unwrap();
            validate(&c, now).unwrap();
        }
    }

    #[test]
    fn two_nets_cannot_share_a_name_or_a_folder() {
        let now = jiff::Timestamp::now();
        let e = validate(&with_second_net("Tuesday M17 net"), now)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Tuesday M17 net") && e.contains("twice"), "{e}");
        // Different names, one folder: tuesday-m17-net.
        let e = validate(&with_second_net("tuesday-M17 NET"), now)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Tuesday M17 net")
                && e.contains("tuesday-M17 NET")
                && e.contains("tuesday-m17-net"),
            "{e}"
        );
    }

    #[test]
    fn callsign_suffix_must_be_an_uppercase_letter_or_digit() {
        let now = jiff::Timestamp::now();
        let spec = |suffix: &str| {
            let s = format!("callsign_suffix = \"{suffix}\"");
            from_toml_str(&SPEC.replace("callsign_suffix = \"R\"", &s)).unwrap()
        };
        for bad in [" ", "r", "-", "\\t", "\u{c4}"] {
            let e = validate(&spec(bad), now).unwrap_err().to_string();
            assert!(e.contains("callsign_suffix"), "{bad:?}: {e}");
        }
        for fine in ["R", "Z", "7"] {
            validate(&spec(fine), now).unwrap();
        }
    }

    #[test]
    fn video_is_on_unless_switched_off() {
        let s = SPEC.replace("video = true\n", "");
        assert!(from_toml_str(&s).unwrap().nets[0].video);
        let s = SPEC.replace("video = true", "video = false");
        assert!(!from_toml_str(&s).unwrap().nets[0].video);
    }

    #[test]
    fn a_small_archive_limit_is_warned_about_as_a_limit() {
        let s = SPEC.replace("\"50 GB\"", "\"100 MB\"");
        let warnings = validate(&from_toml_str(&s).unwrap(), jiff::Timestamp::now()).unwrap();
        let w = warnings.iter().find(|w| w.contains("max_bytes")).unwrap();
        assert!(!w.contains("free"), "{w}");
        assert!(w.contains("deleted"), "{w}");
    }

    #[test]
    fn bootstrap_template_parses() {
        let dir = std::env::temp_dir().join(format!("netrec-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("netrec.toml");
        let _ = std::fs::remove_file(&p);
        let c = load_or_bootstrap(&p).unwrap();
        assert_eq!(c.nets.len(), 0);
        assert!(p.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        load_or_bootstrap(&p).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("1h30m").unwrap().as_secs(), 5400);
        assert_eq!(parse_duration("90m").unwrap().as_secs(), 5400);
        assert_eq!(parse_duration("45s").unwrap().as_secs(), 45);
        assert_eq!(parse_duration("1h").unwrap().as_secs(), 3600);
        assert!(parse_duration("x").is_err());
        assert!(parse_duration("").is_err());
    }
}
