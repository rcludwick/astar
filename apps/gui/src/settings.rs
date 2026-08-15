// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Persisted app settings (astar-de0a): the TOML `SettingsStore` substrate.
//!
//! One `settings.toml` under the platform config dir holds everything the
//! feature nuggets persist — audio preferences, Setups, and (as they land)
//! favorites, credentials pointers, and serial config. Models mirror
//! AstarCore's (`AudioSettings`, `Setup`, `SerialLineSpec`) field-for-field —
//! snake_case here, but the same names, semantics, and defaults — so features
//! stay conceptually identical across the Mac and Iced clients.
//!
//! * [`TomlStore`] — the real store: `settings.toml` under
//!   `ProjectDirs("com", "aj7hr", "astar")`, written atomically.
//! * [`MemStore`] — the in-memory fake for demos and tests; round-trips
//!   through the same TOML text as the real store.
//!
//! Versioning: `schema_version` is written into the file. Files from a NEWER
//! schema are refused (never clobber a newer app's config); older/missing
//! versions are migrated forward (currently: fill defaults).

use crate::network::Network;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The schema this build reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Everything persisted, as one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub audio: AudioSettings,
    /// Every Setup, in user order (mirrors AstarCore `SetupStore.all()`).
    pub setups: Vec<Setup>,
    /// The selected setup id, or `None` if none chosen.
    pub selected_setup: Option<String>,
    /// The launch-default setup id, applied on startup; `None` = none.
    pub default_setup: Option<String>,
    /// The node directory (favorites + auto-tracked recents), in storage
    /// order — mirrors AstarCore `NodeDirectoryStore.all()`.
    pub directory: Vec<NodeEntry>,
    /// The network picker's persisted selection (astar-9b3e) — "where the
    /// next dial goes". Resolved through [`Network::resolve`] against the
    /// engine's current capabilities before use, so a stale pick from before
    /// a capability existed falls back to AllStar rather than wedging the
    /// picker on an unavailable network.
    pub network: Network,
    /// The user's M17 callsign (M17 Task 10/iax-f2b8 Task 8): transmitted
    /// verbatim in every M17 frame, so it's required before an M17 dial (the
    /// app layer refuses `Connect` without it). Uppercase-normalized on set
    /// in the app layer (mirrors the Mac's `CallSession.m17Callsign`), not
    /// here — serde just persists whatever string lands in the field.
    pub m17_callsign: String,
    /// The persisted M17 TX-processing override (astar-5d8e) — mirror of the
    /// Mac's `M17AudioOverrides`. See its doc comment for Rob's field-tested
    /// M17 recipe (astar-m17defaults) that this now defaults to.
    pub m17_audio: M17AudioOverrides,
}

/// Per-network TX audio override for M17 (astar-5d8e) — mirror of the Mac's
/// `M17AudioOverrides`: M17 feeds Codec 2 (a vocoder), not a repeater's RF
/// chain, so it gets its own tuned mic feed rather than reusing AllStar's.
/// Defaults are Rob's field-tested M17 recipe (astar-m17defaults, 2026-08-04
/// on-air A/B testing): 25% mic level, compression ON at 80% strength, 80%
/// TX trim — compression beats a raw feed into Codec 2, reversing the
/// earlier "clean chain" default (Rob's AllStar-tuned NR+compression+trim
/// had produced a parrot echo that sounded "like transmitting from inside a
/// box" over M17; further testing found compression alone, at these levels,
/// doesn't). Noise reduction stays off. Devices and VOX are deliberately NOT
/// part of this set — those stay whatever the shared `AudioSettings` says,
/// for both networks. Output (speaker) gain also stays shared; only the mic
/// (input) gain joined this override (astar-m17defaults).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct M17AudioOverrides {
    pub noise_reduction: bool,
    pub compression: bool,
    pub compression_level: f32,
    pub tx_trim: f32,
    pub input_gain: f32,
}

impl Default for M17AudioOverrides {
    fn default() -> Self {
        M17AudioOverrides {
            noise_reduction: false,
            compression: true,
            compression_level: 0.80,
            tx_trim: 0.80,
            input_gain: 0.25,
        }
    }
}

/// Non-secret audio preferences — mirror of AstarCore `AudioSettings`
/// (credentials will live in the platform keyring, not here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Capture device name; `None` = system default.
    pub input: Option<String>,
    /// Playback device name; `None` = system default.
    pub output: Option<String>,
    pub input_gain: f32,
    /// Output (RX/speaker) gain multiplier. Engine range `0.0..=4.0`
    /// (iax-a4e7), but the quick-config slider only offers `1.0..=4.0` —
    /// 100%-400% headroom, floored at unity (no UI attenuation). The
    /// persisted default stays unity.
    pub output_gain: f32,
    /// Mic voice compression (dynamics) toggle.
    pub compression: bool,
    /// Compression strength (0…1), used when `compression` is on.
    pub compression_level: f32,
    /// TX trim (0…2, linear, 1.0 = unity): the always-on final TX gain stage
    /// after compression. Attenuates a hot mic that compression makeup gain
    /// would otherwise keep loud; above 1.0 boosts (engine clamps).
    pub tx_trim: f32,
    /// Mic noise reduction (denoise) toggle.
    pub noise_reduction: bool,
    /// RX/output compression toggle (iax-a4e7): automatic leveling of the
    /// received audio, reusing the mic-path compressor on the output bus.
    /// Shared across networks — output is listener-side, not per-network
    /// like the TX chain (no M17 override).
    pub rx_compression: bool,
    /// RX/output compression strength (0…1), used when `rx_compression` is on.
    pub rx_compression_level: f32,
    /// Voice-activated PTT toggle.
    pub vox_enabled: bool,
    /// Listen-only (monitor) mode: hard-mutes all transmit.
    pub tx_disabled: bool,
    /// Full-duplex audio; false (half-duplex) keeps VOX from keying while
    /// receiving so speaker bleed can't feed back.
    pub full_duplex: bool,
    /// VOX trigger level (dBFS); lower (toward −60) = more sensitive.
    pub vox_threshold_dbfs: f32,
    /// How long PTT stays keyed after the mic drops below threshold.
    pub vox_hangtime_ms: u32,
    /// Selected mic-profile id; `None` = the built-in Default profile.
    pub mic_profile_id: Option<String>,
    // Deliberately NO wideband field (astar-e542): wideband is always on —
    // the codec policy is unconditionally prefer_slin16 and the node decides
    // the narrowband fallback in IAX2 negotiation. A stale `wideband` key in
    // an old file is ignored (serde skips unknown fields; no
    // deny_unknown_fields here).
}

/// The built-in "None" setup's id — mirrors `SetupController.noneID`. Always
/// offered first in the switcher; can't be edited or deleted.
pub const NONE_SETUP_ID: &str = "__none__";

/// A named rig you switch to in one action — mirror of AstarCore `Setup`.
/// `None` option fields mean "don't override on apply".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Setup {
    /// Stable id (a UUID string); used for upsert/select.
    pub id: String,
    /// User-facing label, e.g. "UCI150 desk", "Jabra mobile".
    pub name: String,
    /// Hardware profile id: `uci150` / `headset` / `custom`.
    pub hardware_profile_id: String,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: Option<f32>,
    pub output_gain: Option<f32>,
    pub compression: Option<bool>,
    pub compression_level: Option<f32>,
    /// TX trim (post-compression output gain, UI "TX Volume") for this rig.
    pub tx_trim: Option<f32>,
    pub noise_reduction: Option<bool>,
    pub vox_enabled: Option<bool>,
    pub vox_threshold: Option<f32>,
    pub full_duplex: Option<bool>,
    /// Per-setup serial line settings; `None` = the hardware profile's preset.
    pub serial: Option<SerialLineSpec>,
    pub mic_profile_id: Option<String>,
    // Deliberately NO per-Setup codec-policy override: wideband is always on
    // (astar-e542) — there is no knob anywhere, per-Setup or app-global. The
    // codec policy is unconditionally prefer_slin16; nodes without
    // allow=slin16 answer µ-law in IAX2 negotiation, so the node decides the
    // narrowband fallback (same reasoning as the Mac's Setup.swift).
}

/// Platform-neutral serial line settings — mirror of AstarCore
/// `SerialLineSpec` (raw `u32`s for the line/mode enums, same as the Mac).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SerialLineSpec {
    /// The manually-chosen port (used when not autodetecting).
    pub port_path: Option<String>,
    /// `None` = autodetect (the legacy default).
    pub autodetect: Option<bool>,
    pub key_line_raw: u32,
    pub key_active_high: bool,
    pub radio_line_raw: u32,
    pub radio_active_high: bool,
    pub debounce_ms: u32,
    pub rx_mode_raw: u32,
    pub rx_floor_db: f32,
    pub rx_hang_ms: u32,
    /// `None`/0 = tty; 1 = raw USB. Optional for back-compat, like the Mac.
    pub transport_raw: Option<u32>,
}

/// A saved node in the user's directory — mirror of AstarCore `NodeEntry`:
/// a callsign or human label mapped to an AllStar node number, e.g.
/// "AJ7HR" → "77777". `favorite` entries are curated by the user; entries
/// with a `last_used` are auto-tracked recents (a node the user has
/// successfully connected to). The two overlap: favoriting a recent keeps
/// its `last_used`, and re-dialing a favorite updates its `last_used`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NodeEntry {
    /// Stable id, used for upsert. The Mac generates UUIDs; gui-rs dedupes
    /// by node, so the node number doubles as the id.
    pub id: String,
    /// User-facing label — a callsign or name, e.g. "AJ7HR".
    pub label: String,
    /// AllStar node number as a string, e.g. "77777".
    pub node: String,
    /// User-curated favorite (shown first, sorted by label).
    pub favorite: bool,
    /// Last successful connect to this node (Unix seconds); `None` for a
    /// never-dialed favorite. Optional so entries stored before recents
    /// existed still decode, like the Mac.
    pub last_used: Option<i64>,
    /// Optional free-form note. Optional for back-compat, like the Mac.
    pub note: Option<String>,
    /// Which network this entry dials on (astar-9b3e). Defaults to
    /// `Allstar` for back-compat — every entry saved before the network
    /// switcher existed is an AllStar node.
    pub network: Network,
}

impl NodeEntry {
    /// A fresh entry for `node` (id = node; see `id`), labeled by `label`
    /// or by the node number when `label` is blank.
    fn new(node: &str, label: &str) -> Self {
        NodeEntry {
            id: node.to_string(),
            label: if label.is_empty() {
                node.to_string()
            } else {
                label.to_string()
            },
            node: node.to_string(),
            favorite: false,
            last_used: None,
            note: None,
            network: Network::default(),
        }
    }
}

impl Setup {
    /// The built-in "None" setup: system-default input/output, no overrides —
    /// mirrors `SetupController.noneSetup`. Applying it resets the devices to
    /// the system default and leaves every other knob as it stands.
    #[must_use]
    pub fn none() -> Self {
        Setup {
            id: NONE_SETUP_ID.to_string(),
            name: "None (system default)".to_string(),
            hardware_profile_id: "headset".to_string(),
            ..Setup::default()
        }
    }

    /// The named devices this Setup needs that aren't currently enumerated —
    /// so the switcher can refuse to apply when the USB gadget is unplugged.
    /// `None` devices (system default) are always present. A device named on
    /// both directions (combined gadget) is reported once. Mirrors
    /// `Setup.missingDevices`.
    #[must_use]
    pub fn missing_devices(&self, inputs: &[String], outputs: &[String]) -> Vec<String> {
        let mut missing = Vec::new();
        if let Some(d) = &self.input_device {
            if !inputs.contains(d) {
                missing.push(d.clone());
            }
        }
        if let Some(d) = &self.output_device {
            if !outputs.contains(d) && !missing.contains(d) {
                missing.push(d.clone());
            }
        }
        missing
    }

    /// Apply this Setup onto the live audio settings — the audio half of
    /// `SetupController.apply`. The devices and mic profile are applied
    /// unconditionally (`None` = system default / the built-in Default
    /// profile); every other field only overrides when `Some`, so a `None`
    /// keeps the standing global value. (`Setup` has no VOX-hangtime or
    /// listen-only field, so those are never touched — same as the Mac.)
    pub fn apply_to(&self, audio: &mut AudioSettings) {
        audio.input = self.input_device.clone();
        audio.output = self.output_device.clone();
        audio.mic_profile_id = self.mic_profile_id.clone();
        if let Some(g) = self.input_gain {
            audio.input_gain = g;
        }
        if let Some(g) = self.output_gain {
            audio.output_gain = g;
        }
        if let Some(c) = self.compression {
            audio.compression = c;
        }
        if let Some(l) = self.compression_level {
            audio.compression_level = l;
        }
        if let Some(t) = self.tx_trim {
            audio.tx_trim = t;
        }
        if let Some(n) = self.noise_reduction {
            audio.noise_reduction = n;
        }
        if let Some(v) = self.vox_enabled {
            audio.vox_enabled = v;
        }
        if let Some(t) = self.vox_threshold {
            audio.vox_threshold_dbfs = t;
        }
        if let Some(fd) = self.full_duplex {
            audio.full_duplex = fd;
        }
    }

    /// Snapshot the live audio settings into this Setup, so switching back
    /// later restores the whole rig — mirrors
    /// `SetupController.saveCurrentToSelected`'s field list. The name,
    /// hardware profile, and serial spec are left alone.
    pub fn capture_from(&mut self, audio: &AudioSettings) {
        self.input_device = audio.input.clone();
        self.output_device = audio.output.clone();
        self.input_gain = Some(audio.input_gain);
        self.output_gain = Some(audio.output_gain);
        self.compression = Some(audio.compression);
        self.compression_level = Some(audio.compression_level);
        self.tx_trim = Some(audio.tx_trim);
        self.noise_reduction = Some(audio.noise_reduction);
        self.vox_enabled = Some(audio.vox_enabled);
        self.vox_threshold = Some(audio.vox_threshold_dbfs);
        self.full_duplex = Some(audio.full_duplex);
        self.mic_profile_id = audio.mic_profile_id.clone();
    }
}

impl SerialLineSpec {
    /// Resolved autodetect decision: an absent flag means autodetect.
    #[must_use]
    pub fn is_autodetect(&self) -> bool {
        self.autodetect.unwrap_or(true)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            schema_version: SCHEMA_VERSION,
            audio: AudioSettings::default(),
            setups: Vec::new(),
            selected_setup: None,
            default_setup: None,
            directory: Vec::new(),
            network: Network::default(),
            m17_callsign: String::new(),
            m17_audio: M17AudioOverrides::default(),
        }
    }
}

/// Recents cap — mirrors AstarCore `NodeDirectoryStore.recents(limit:)`.
const RECENTS_LIMIT: usize = 10;

/// Defaults mirror AstarCore's: mic gain backed off to 0.90 for compression
/// headroom, unity output, VOX −40 dBFS / 500 ms hang.
impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            input: None,
            output: None,
            input_gain: 0.90,
            output_gain: 1.0,
            compression: false,
            compression_level: 0.90,
            tx_trim: 1.0,
            noise_reduction: false,
            rx_compression: false,
            rx_compression_level: 0.90,
            vox_enabled: false,
            tx_disabled: false,
            full_duplex: false,
            vox_threshold_dbfs: -40.0,
            vox_hangtime_ms: 500,
            mic_profile_id: None,
        }
    }
}

impl Settings {
    /// Upsert by `id`, preserving list position (new ids append) — mirrors
    /// AstarCore `SetupStore.save`.
    pub fn upsert_setup(&mut self, setup: Setup) {
        match self.setups.iter_mut().find(|s| s.id == setup.id) {
            Some(slot) => *slot = setup,
            None => self.setups.push(setup),
        }
    }

    /// Remove the setup with `id` (no-op if absent).
    pub fn remove_setup(&mut self, id: &str) {
        self.setups.retain(|s| s.id != id);
    }

    /// Reorder: move the setup at `from` to index `to`. Out-of-range indices
    /// are a no-op, like the Mac store.
    pub fn move_setup(&mut self, from: usize, to: usize) {
        if from >= self.setups.len() || to > self.setups.len() {
            return;
        }
        let setup = self.setups.remove(from);
        let to = to.min(self.setups.len());
        self.setups.insert(to, setup);
    }

    // -- node directory (favorites + recents, astar-ac65) --------------------
    // Mirrors AstarCore `NodeDirectoryStore` + `CallSession`'s favorite
    // helpers, so the two clients keep identical directory semantics.

    /// Favorited entries, sorted by label (case-insensitive).
    #[must_use]
    pub fn favorites(&self) -> Vec<&NodeEntry> {
        let mut list: Vec<&NodeEntry> = self.directory.iter().filter(|e| e.favorite).collect();
        list.sort_by_key(|e| e.label.to_lowercase());
        list
    }

    /// Recently-used entries (those with a `last_used`), newest first, capped.
    #[must_use]
    pub fn recents(&self) -> Vec<&NodeEntry> {
        let mut list: Vec<&NodeEntry> = self
            .directory
            .iter()
            .filter(|e| e.last_used.is_some())
            .collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        list.truncate(RECENTS_LIMIT);
        list
    }

    /// Whether `node` is currently a favorite.
    #[must_use]
    pub fn is_favorite(&self, node: &str) -> bool {
        self.directory.iter().any(|e| e.node == node && e.favorite)
    }

    /// Auto-track a successful connect at `now` (Unix seconds): upsert by
    /// **node**, so re-dialing the same node updates its `last_used` rather
    /// than duplicating. An existing favorite flag and curated label are
    /// preserved (mirrors `recordRecent`).
    pub fn record_recent(&mut self, node: &str, now: i64) {
        match self.directory.iter_mut().find(|e| e.node == node) {
            Some(e) => e.last_used = Some(now),
            None => {
                let mut e = NodeEntry::new(node, "");
                e.last_used = Some(now);
                self.directory.push(e);
            }
        }
    }

    /// Favorite a node with a label (callsign/name) — mirrors CallSession
    /// `addFavorite`: upserts by node so it merges with an existing
    /// recent/entry rather than duplicating, preserving `last_used`. A blank
    /// label keeps the existing label (or defaults to the node number).
    pub fn add_favorite(&mut self, node: &str, label: &str) {
        let node = node.trim();
        if node.is_empty() {
            return;
        }
        let label = label.trim();
        match self.directory.iter_mut().find(|e| e.node == node) {
            Some(e) => {
                e.favorite = true;
                if !label.is_empty() {
                    e.label = label.to_string();
                }
            }
            None => {
                let mut e = NodeEntry::new(node, label);
                e.favorite = true;
                self.directory.push(e);
            }
        }
    }

    /// Un-favorite a node — mirrors CallSession `removeFavorite`: kept as a
    /// recent if it has a `last_used`, otherwise removed entirely so the
    /// directory doesn't accumulate dead entries.
    pub fn remove_favorite(&mut self, node: &str) {
        self.directory.retain_mut(|e| {
            if e.node != node {
                return true;
            }
            if e.last_used.is_some() {
                e.favorite = false;
                true
            } else {
                false
            }
        });
    }
}

/// Why a load or save failed. Callers surface these on the status card —
/// never panic, never silently drop the user's file.
#[derive(Debug)]
pub enum SettingsError {
    /// Filesystem trouble (permissions, disk, …).
    Io(std::io::Error),
    /// The file exists but isn't valid TOML for this schema.
    Parse(String),
    /// The file was written by a newer app; refuse to touch it.
    NewerSchema { found: u32 },
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::Io(e) => write!(f, "settings file error: {e}"),
            SettingsError::Parse(e) => write!(f, "settings file unreadable: {e}"),
            SettingsError::NewerSchema { found } => write!(
                f,
                "settings were saved by a newer astar (schema {found} > {SCHEMA_VERSION})"
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

/// The seam feature code saves/loads through — real file or in-memory fake.
pub trait SettingsStore {
    /// Current settings; a missing file yields defaults, a broken or
    /// newer-schema file yields an error (so the UI can say so).
    fn load(&self) -> Result<Settings, SettingsError>;
    fn save(&mut self, settings: &Settings) -> Result<(), SettingsError>;
}

/// Parse + migrate one TOML document. A document from a newer schema is
/// refused; older documents migrate forward (currently just: absent fields
/// take defaults) and are stamped with the current version.
fn parse(text: &str) -> Result<Settings, SettingsError> {
    let mut settings: Settings =
        toml::from_str(text).map_err(|e| SettingsError::Parse(e.to_string()))?;
    if settings.schema_version > SCHEMA_VERSION {
        return Err(SettingsError::NewerSchema {
            found: settings.schema_version,
        });
    }
    // Version-specific migrations slot in here as the schema grows.
    settings.schema_version = SCHEMA_VERSION;
    Ok(settings)
}

/// Serialize for writing.
fn to_toml(settings: &Settings) -> String {
    toml::to_string_pretty(settings).expect("settings serialize to TOML")
}

/// In-memory fake for demos and tests. Round-trips through the same TOML text
/// as [`TomlStore`] so fidelity bugs show up in fast tests, not on disk.
#[derive(Default)]
pub struct MemStore {
    doc: Option<String>,
}

impl SettingsStore for MemStore {
    fn load(&self) -> Result<Settings, SettingsError> {
        match &self.doc {
            None => Ok(Settings::default()),
            Some(text) => parse(text),
        }
    }

    fn save(&mut self, settings: &Settings) -> Result<(), SettingsError> {
        self.doc = Some(to_toml(settings));
        Ok(())
    }
}

/// The real store: one `settings.toml`, written atomically (temp file +
/// rename) so a crash mid-write can't truncate the user's config.
pub struct TomlStore {
    path: PathBuf,
}

impl TomlStore {
    /// A store at an explicit path (tests, portable installs).
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        TomlStore { path: path.into() }
    }

    /// The per-user store under the platform config dir — Linux
    /// `~/.config/astar`, Windows `AppData\Roaming`, macOS
    /// `~/Library/Application Support`. `None` if the OS reports no home.
    #[must_use]
    pub fn open_default() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("com", "aj7hr", "astar")?;
        Some(Self::at(dirs.config_dir().join("settings.toml")))
    }

    /// Where this store reads/writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SettingsStore for TomlStore {
    fn load(&self) -> Result<Settings, SettingsError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(SettingsError::Io(e)),
        }
    }

    fn save(&mut self, settings: &Settings) -> Result<(), SettingsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(SettingsError::Io)?;
        }
        // Write-then-rename so a crash mid-write can't truncate the config.
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, to_toml(settings)).map_err(SettingsError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(SettingsError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Settings with every optional field populated, to prove nothing is
    /// dropped on a round trip.
    fn full_settings() -> Settings {
        Settings {
            schema_version: SCHEMA_VERSION,
            audio: AudioSettings {
                input: Some("UCI150 Audio".into()),
                output: Some("MacBook Pro Speakers".into()),
                input_gain: 0.75,
                output_gain: 1.25,
                compression: true,
                compression_level: 0.5,
                tx_trim: 0.4,
                noise_reduction: true,
                rx_compression: true,
                rx_compression_level: 0.65,
                vox_enabled: true,
                tx_disabled: true,
                full_duplex: true,
                vox_threshold_dbfs: -33.5,
                vox_hangtime_ms: 750,
                mic_profile_id: Some("mp-1".into()),
            },
            setups: vec![Setup {
                id: "s-1".into(),
                name: "UCI150 desk".into(),
                hardware_profile_id: "uci150".into(),
                input_device: Some("UCI150 Audio".into()),
                output_device: Some("UCI150 Audio".into()),
                input_gain: Some(0.9),
                output_gain: Some(1.0),
                compression: Some(true),
                compression_level: Some(0.9),
                tx_trim: Some(0.4),
                noise_reduction: Some(false),
                vox_enabled: Some(true),
                vox_threshold: Some(-40.0),
                full_duplex: Some(false),
                serial: Some(SerialLineSpec {
                    port_path: Some("/dev/ttyUSB0".into()),
                    autodetect: Some(false),
                    key_line_raw: 1,
                    key_active_high: true,
                    radio_line_raw: 2,
                    radio_active_high: false,
                    debounce_ms: 20,
                    rx_mode_raw: 1,
                    rx_floor_db: -55.0,
                    rx_hang_ms: 200,
                    transport_raw: Some(1),
                }),
                mic_profile_id: Some("mp-1".into()),
            }],
            selected_setup: Some("s-1".into()),
            default_setup: Some("s-1".into()),
            directory: vec![NodeEntry {
                id: "77777".into(),
                label: "AJ7HR".into(),
                node: "77777".into(),
                favorite: true,
                last_used: Some(1_750_000_000),
                note: Some("home node".into()),
                network: Network::Allstar,
            }],
            network: Network::Allstar,
            m17_callsign: "AJ7HR".into(),
            m17_audio: M17AudioOverrides {
                noise_reduction: true,
                compression: true,
                compression_level: 0.55,
                tx_trim: 0.4,
                input_gain: 0.6,
            },
        }
    }

    fn named_setup(id: &str) -> Setup {
        Setup {
            id: id.into(),
            name: format!("setup {id}"),
            hardware_profile_id: "custom".into(),
            ..Setup::default()
        }
    }

    // -- schema & defaults ---------------------------------------------------

    #[test]
    fn defaults_mirror_astarcore() {
        let s = Settings::default();
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        assert_eq!(s.audio.input_gain, 0.90, "mic backed off for headroom");
        assert_eq!(s.audio.output_gain, 1.0, "unity output");
        assert_eq!(s.audio.compression_level, 0.90);
        assert_eq!(s.audio.vox_threshold_dbfs, -40.0);
        assert_eq!(s.audio.vox_hangtime_ms, 500);
        assert!(!s.audio.compression && !s.audio.vox_enabled && !s.audio.full_duplex);
        assert!(s.setups.is_empty());
        assert_eq!(s.selected_setup, None);
    }

    #[test]
    fn tx_trim_defaults_to_unity_when_missing() {
        assert_eq!(Settings::default().audio.tx_trim, 1.0);
        // A pre-trim file (other audio keys, no tx_trim) must parse to unity.
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("pre-trim doc");
        assert_eq!(parsed.audio.tx_trim, 1.0);
    }

    #[test]
    fn tx_trim_round_trips() {
        let mut s = Settings::default();
        s.audio.tx_trim = 0.6;
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.audio.tx_trim, 0.6);
    }

    #[test]
    fn setup_tx_trim_missing_parses_as_none_and_some_round_trips() {
        // A setup saved before per-setup trim existed has no tx_trim key —
        // it must parse as None (= don't override on apply).
        let doc = "[[setups]]\nid = \"s-1\"\nname = \"Old\"\nhardware_profile_id = \"headset\"\n";
        let parsed = parse(doc).expect("pre-trim setup parses");
        assert_eq!(parsed.setups[0].tx_trim, None);

        // And a stored override survives a round trip.
        let mut s = Settings::default();
        let mut setup = named_setup("s-1");
        setup.tx_trim = Some(0.4);
        s.setups.push(setup);
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.setups[0].tx_trim, Some(0.4));
    }

    #[test]
    fn stale_wideband_key_still_parses() {
        // Wideband is always on (astar-e542): the field is gone, but a file
        // saved while the toggle existed (astar-eb6c) may still carry a
        // `wideband` key. serde ignores unknown fields (no
        // deny_unknown_fields), so the old file must load cleanly.
        let parsed = parse("[audio]\ninput_gain = 0.5\nwideband = false\n")
            .expect("doc with stale wideband key parses");
        assert_eq!(parsed.audio.input_gain, 0.5);
    }

    #[test]
    fn quick_config_fields_default_when_missing() {
        // A file from before the quick config panel (astar-f4d9) has none of
        // the DSP/VOX/duplex keys — each must take its engine default.
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("pre-quick-config doc");
        assert!(!parsed.audio.compression);
        assert_eq!(parsed.audio.compression_level, 0.90);
        assert!(!parsed.audio.noise_reduction);
        assert!(!parsed.audio.full_duplex);
        assert_eq!(parsed.audio.vox_threshold_dbfs, -40.0);
        assert_eq!(parsed.audio.vox_hangtime_ms, 500);
    }

    #[test]
    fn quick_config_fields_round_trip() {
        let mut s = Settings::default();
        s.audio.compression = true;
        s.audio.compression_level = 0.35;
        s.audio.noise_reduction = true;
        s.audio.full_duplex = true;
        s.audio.vox_threshold_dbfs = -22.5;
        s.audio.vox_hangtime_ms = 1450;
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.audio, s.audio);
    }

    #[test]
    fn devices_default_to_system_default_when_missing() {
        // `None` = system default; a file without device keys (or from before
        // pickers existed) must select the system default, not fail.
        assert_eq!(Settings::default().audio.input, None);
        assert_eq!(Settings::default().audio.output, None);
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("no-device doc");
        assert_eq!(parsed.audio.input, None);
        assert_eq!(parsed.audio.output, None);
    }

    #[test]
    fn devices_round_trip() {
        let mut s = Settings::default();
        s.audio.input = Some("USB Audio CODEC".into());
        s.audio.output = Some("Speakers (Realtek)".into());
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.audio.input.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(parsed.audio.output.as_deref(), Some("Speakers (Realtek)"));
    }

    #[test]
    fn toml_round_trip_preserves_everything() {
        let original = full_settings();
        let parsed = parse(&to_toml(&original)).expect("round trip parses");
        assert_eq!(parsed, original);
    }

    #[test]
    fn empty_and_minimal_documents_parse_as_defaults() {
        assert_eq!(parse("").expect("empty doc"), Settings::default());
        assert_eq!(
            parse("schema_version = 1\n").expect("minimal doc"),
            Settings::default()
        );
    }

    #[test]
    fn partial_document_fills_missing_fields_with_defaults() {
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("partial doc");
        assert_eq!(parsed.audio.input_gain, 0.5);
        assert_eq!(parsed.audio.output_gain, 1.0, "absent fields get defaults");
        assert_eq!(parsed.audio.vox_hangtime_ms, 500);
    }

    #[test]
    fn newer_schema_is_refused() {
        let err = parse("schema_version = 99\n").expect_err("newer schema must not load");
        match err {
            SettingsError::NewerSchema { found } => assert_eq!(found, 99),
            other => panic!("expected NewerSchema, got {other:?}"),
        }
    }

    #[test]
    fn autodetect_defaults_to_true_like_the_mac() {
        assert!(SerialLineSpec::default().is_autodetect());
        let parsed = parse("[[setups]]\nid = \"s\"\nname = \"n\"\nhardware_profile_id = \"uci150\"\n[setups.serial]\nkey_line_raw = 1\n")
            .expect("setup with bare serial");
        assert!(parsed.setups[0].serial.as_ref().unwrap().is_autodetect());
    }

    // -- setup list operations (mirror AstarCore SetupStore) -----------------

    #[test]
    fn upsert_replaces_in_place_and_appends_new() {
        let mut s = Settings::default();
        s.upsert_setup(named_setup("a"));
        s.upsert_setup(named_setup("b"));

        let mut a2 = named_setup("a");
        a2.name = "renamed".into();
        s.upsert_setup(a2);

        assert_eq!(s.setups.len(), 2);
        assert_eq!(s.setups[0].id, "a", "upsert keeps list position");
        assert_eq!(s.setups[0].name, "renamed");

        s.upsert_setup(named_setup("c"));
        assert_eq!(s.setups[2].id, "c", "new ids append");
    }

    #[test]
    fn remove_setup_deletes_by_id_and_ignores_absent() {
        let mut s = Settings::default();
        s.upsert_setup(named_setup("a"));
        s.upsert_setup(named_setup("b"));
        s.remove_setup("a");
        assert_eq!(s.setups.len(), 1);
        assert_eq!(s.setups[0].id, "b");
        s.remove_setup("nope");
        assert_eq!(s.setups.len(), 1);
    }

    #[test]
    fn move_setup_reorders_and_ignores_bad_indices() {
        let mut s = Settings::default();
        for id in ["a", "b", "c"] {
            s.upsert_setup(named_setup(id));
        }
        s.move_setup(0, 2);
        let order: Vec<&str> = s.setups.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);

        s.move_setup(9, 0); // out-of-range: no-op
        let order: Vec<&str> = s.setups.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
    }

    // -- setup apply/capture semantics (astar-80ae) ---------------------------
    // Mirror SetupController.apply's audio half + saveCurrentToSelected.

    #[test]
    fn the_builtin_none_setup_matches_the_mac() {
        let none = Setup::none();
        assert_eq!(none.id, NONE_SETUP_ID);
        assert_eq!(none.name, "None (system default)");
        assert_eq!(none.input_device, None, "system default input");
        assert_eq!(none.output_device, None, "system default output");
        // No audio overrides: applying None leaves every knob alone.
        assert_eq!(none.input_gain, None);
        assert_eq!(none.compression, None);
        assert_eq!(none.full_duplex, None);
    }

    #[test]
    fn apply_to_overrides_only_the_some_fields() {
        let mut audio = AudioSettings {
            input: Some("Old Mic".into()),
            output: Some("Old Speakers".into()),
            input_gain: 1.3,
            compression: true,
            vox_threshold_dbfs: -20.0,
            mic_profile_id: Some("mp-old".into()),
            ..AudioSettings::default()
        };
        let setup = Setup {
            input_device: Some("USB Audio CODEC".into()),
            output_device: Some("USB Audio CODEC".into()),
            input_gain: Some(0.75),
            noise_reduction: Some(true),
            ..named_setup("s-1")
        };
        setup.apply_to(&mut audio);

        // Some overrides land…
        assert_eq!(audio.input.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(audio.input_gain, 0.75);
        assert!(audio.noise_reduction);
        // …None fields keep the standing global values…
        assert!(audio.compression, "None = keep global, not reset");
        assert_eq!(audio.vox_threshold_dbfs, -20.0);
        assert_eq!(audio.output_gain, 1.0);
        // …but the mic profile follows the setup unconditionally (None = the
        // built-in Default profile), like the Mac's setMicProfileSelection.
        assert_eq!(audio.mic_profile_id, None);
    }

    #[test]
    fn applying_none_resets_devices_and_keeps_every_knob() {
        let mut audio = AudioSettings {
            input: Some("USB Audio CODEC".into()),
            output: Some("USB Audio CODEC".into()),
            input_gain: 0.75,
            compression: true,
            full_duplex: true,
            ..AudioSettings::default()
        };
        Setup::none().apply_to(&mut audio);
        assert_eq!(audio.input, None, "None reverts to the system default");
        assert_eq!(audio.output, None);
        assert_eq!(audio.input_gain, 0.75, "audio knobs stay as they stand");
        assert!(audio.compression && audio.full_duplex);
    }

    #[test]
    fn capture_from_snapshots_every_audio_field() {
        let audio = AudioSettings {
            input: Some("USB Audio CODEC".into()),
            output: None,
            input_gain: 0.8,
            output_gain: 1.2,
            compression: true,
            compression_level: 0.4,
            tx_trim: 0.9,
            noise_reduction: true,
            vox_enabled: true,
            vox_threshold_dbfs: -35.0,
            full_duplex: true,
            mic_profile_id: Some("mp-1".into()),
            ..AudioSettings::default()
        };
        let mut setup = Setup {
            serial: Some(SerialLineSpec::default()),
            ..named_setup("s-1")
        };
        setup.capture_from(&audio);

        assert_eq!(setup.input_device.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(setup.output_device, None);
        assert_eq!(setup.input_gain, Some(0.8));
        assert_eq!(setup.output_gain, Some(1.2));
        assert_eq!(setup.compression, Some(true));
        assert_eq!(setup.compression_level, Some(0.4));
        assert_eq!(setup.tx_trim, Some(0.9));
        assert_eq!(setup.noise_reduction, Some(true));
        assert_eq!(setup.vox_enabled, Some(true));
        assert_eq!(setup.vox_threshold, Some(-35.0));
        assert_eq!(setup.full_duplex, Some(true));
        assert_eq!(setup.mic_profile_id.as_deref(), Some("mp-1"));
        // Identity + hardware are not audio state: left alone.
        assert_eq!(setup.name, "setup s-1");
        assert_eq!(setup.hardware_profile_id, "custom");
        assert!(setup.serial.is_some());
    }

    #[test]
    fn capture_then_apply_round_trips_the_audio_settings() {
        // saveCurrentToSelected followed by a later apply must restore the
        // exact rig (the whole point of a Setup).
        let audio = AudioSettings {
            input: Some("USB Audio CODEC".into()),
            compression: true,
            tx_trim: 0.7,
            ..AudioSettings::default()
        };
        let mut setup = named_setup("s-1");
        setup.capture_from(&audio);
        let mut restored = AudioSettings {
            input: Some("Other Mic".into()),
            compression: false,
            tx_trim: 1.4,
            ..AudioSettings::default()
        };
        setup.apply_to(&mut restored);
        assert_eq!(restored, audio);
    }

    #[test]
    fn missing_devices_mirrors_the_mac() {
        let inputs = vec!["Demo Microphone".to_string(), "USB Audio CODEC".to_string()];
        let outputs = vec!["Demo Speakers".to_string(), "USB Audio CODEC".to_string()];

        // System-default devices are always present.
        assert!(Setup::none().missing_devices(&inputs, &outputs).is_empty());

        // A present combined gadget: nothing missing.
        let present = Setup {
            input_device: Some("USB Audio CODEC".into()),
            output_device: Some("USB Audio CODEC".into()),
            ..named_setup("s-1")
        };
        assert!(present.missing_devices(&inputs, &outputs).is_empty());

        // An unplugged combined gadget is reported once, not twice.
        let unplugged = Setup {
            input_device: Some("UCI150 Audio".into()),
            output_device: Some("UCI150 Audio".into()),
            ..named_setup("s-2")
        };
        assert_eq!(
            unplugged.missing_devices(&inputs, &outputs),
            ["UCI150 Audio"]
        );

        // Distinct missing devices both show, input first.
        let both = Setup {
            input_device: Some("Gone Mic".into()),
            output_device: Some("Gone Speakers".into()),
            ..named_setup("s-3")
        };
        assert_eq!(
            both.missing_devices(&inputs, &outputs),
            ["Gone Mic", "Gone Speakers"]
        );
    }

    // -- node directory (favorites + recents, astar-ac65) --------------------

    fn dir_entry(label: &str, node: &str, favorite: bool, last_used: Option<i64>) -> NodeEntry {
        NodeEntry {
            id: node.into(),
            label: label.into(),
            node: node.into(),
            favorite,
            last_used,
            note: None,
            network: Network::Allstar,
        }
    }

    #[test]
    fn directory_defaults_empty_and_is_absent_from_old_files() {
        assert!(Settings::default().directory.is_empty());
        // A file from before favorites existed has no [[directory]] table —
        // it must parse to an empty directory, not fail.
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("pre-favorites doc");
        assert!(parsed.directory.is_empty());
    }

    #[test]
    fn node_entry_without_network_decodes_as_allstar() {
        // A file from before the network switcher (astar-9b3e) has no
        // `network` key on its directory entries — must decode as AllStar.
        let s = parse(
            "[[directory]]\nid = \"77777\"\nlabel = \"AJ7HR\"\nnode = \"77777\"\nfavorite = true\n",
        )
        .expect("pre-9b3e entry");
        assert_eq!(s.directory[0].network, Network::Allstar);
    }

    #[test]
    fn settings_without_network_key_decodes_as_allstar() {
        // A root settings doc from before the switcher has no top-level
        // `network` key either — must decode as AllStar, not fail.
        let s = parse("[audio]\ninput_gain = 0.5\n").expect("pre-9b3e doc");
        assert_eq!(s.network, Network::Allstar);
    }

    #[test]
    fn unknown_network_string_falls_back_to_allstar_instead_of_failing_the_parse() {
        // A future build wrote "dmr" (the next family after M17); this build
        // doesn't know that case yet (an app downgrade). An unknown value on
        // the root `network` key must not fail the whole document — that
        // would refuse the file, boot on defaults, and clobber it on the next
        // save.
        let s = parse("network = \"dmr\"\n").expect("unknown root network parses");
        assert_eq!(s.network, Network::Allstar);

        // Same for a `[[directory]]` entry's `network` key — other fields on
        // the entry must survive intact.
        let s = parse(
            "[[directory]]\nid = \"77777\"\nlabel = \"AJ7HR\"\nnode = \"77777\"\nfavorite = true\nnetwork = \"dmr\"\n",
        )
        .expect("unknown directory-entry network parses");
        assert_eq!(s.directory.len(), 1);
        assert_eq!(s.directory[0].network, Network::Allstar);
        assert_eq!(s.directory[0].id, "77777");
        assert_eq!(s.directory[0].label, "AJ7HR");
        assert_eq!(s.directory[0].node, "77777");
        assert!(s.directory[0].favorite);
    }

    #[test]
    fn m17_network_string_decodes_correctly_now_that_the_variant_exists() {
        // "m17" used to be the unknown-fallback example above; now that
        // `Network::M17` exists it must decode to the real variant, not
        // fall back — the tolerant `Deserialize`'s known set must include it.
        let s = parse("network = \"m17\"\n").expect("m17 root network parses");
        assert_eq!(s.network, Network::M17);
        let s = parse(
            "[[directory]]\nid = \"m17.example.net/A\"\nlabel = \"M17 reflector\"\nnode = \"m17.example.net/A\"\nfavorite = true\nnetwork = \"m17\"\n",
        )
        .expect("m17 directory-entry network parses");
        assert_eq!(s.directory[0].network, Network::M17);
    }

    #[test]
    fn m17_callsign_defaults_empty_and_round_trips() {
        assert_eq!(Settings::default().m17_callsign, "");
        // A file from before M17 existed has no `m17_callsign` key — must
        // decode to empty, not fail.
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("pre-M17 doc");
        assert_eq!(parsed.m17_callsign, "");

        let s = Settings {
            m17_callsign: "AJ7HR".into(),
            ..Settings::default()
        };
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.m17_callsign, "AJ7HR");
    }

    // -- M17 TX-processing override (astar-5d8e) ------------------------------

    #[test]
    fn m17_audio_defaults_to_robs_field_tested_recipe() {
        // astar-m17defaults, 2026-08-04 on-air A/B testing: 25% mic level,
        // compression ON at 80% strength, 80% TX trim. Noise reduction stays
        // off. Replaces the earlier "clean chain" default (compression OFF).
        let m17 = M17AudioOverrides::default();
        assert!(!m17.noise_reduction);
        assert!(m17.compression);
        assert_eq!(m17.compression_level, 0.80);
        assert_eq!(m17.tx_trim, 0.80);
        assert_eq!(m17.input_gain, 0.25);
        assert_eq!(Settings::default().m17_audio, M17AudioOverrides::default());
    }

    #[test]
    fn m17_audio_defaults_when_missing_and_round_trips() {
        // A file from before this override existed has no `[m17_audio]`
        // table — must decode to the field-tested-recipe default, not fail.
        let parsed = parse("[audio]\ninput_gain = 0.5\n").expect("pre-override doc");
        assert_eq!(parsed.m17_audio, M17AudioOverrides::default());

        let s = Settings {
            m17_audio: M17AudioOverrides {
                noise_reduction: true,
                compression: true,
                compression_level: 0.35,
                tx_trim: 0.2,
                input_gain: 0.15,
            },
            ..Settings::default()
        };
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.m17_audio, s.m17_audio);
    }

    #[test]
    fn m17_audio_input_gain_absent_key_defaults_and_explicit_false_compression_is_respected() {
        // Back-compat (astar-m17defaults): a file saved before `input_gain`
        // joined this override — and before `compression` flipped its
        // default to `true` — has an `[m17_audio]` table with the other
        // fields but neither of these two. `input_gain` must fill in the new
        // 0.25 default; an explicitly-persisted `compression = false` (e.g.
        // Rob's own setting from before the flip, if he never turned it on)
        // must NOT be silently migrated to `true` — defaults only fill an
        // ABSENT key.
        let doc = "[m17_audio]\nnoise_reduction = false\ncompression = false\n\
                    compression_level = 0.80\ntx_trim = 0.80\n";
        let parsed = parse(doc).expect("partial m17_audio table parses");
        assert_eq!(
            parsed.m17_audio.input_gain, 0.25,
            "absent key loads the new default"
        );
        assert!(
            !parsed.m17_audio.compression,
            "an explicit persisted false is respected, not auto-flipped to the new default"
        );
    }

    #[test]
    fn directory_round_trips_with_and_without_optionals() {
        let mut s = Settings::default();
        // A never-dialed favorite (no last_used) and a favorited recent with
        // a note — both shapes must survive the TOML round trip verbatim.
        s.directory.push(dir_entry("AJ7HR", "77777", true, None));
        s.directory.push(NodeEntry {
            note: Some("home node".into()),
            ..dir_entry("W6ABC Repeater", "546054", true, Some(1_750_000_000))
        });
        let parsed = parse(&to_toml(&s)).expect("round trip parses");
        assert_eq!(parsed.directory, s.directory);
    }

    #[test]
    fn favorites_are_sorted_by_label_case_insensitively() {
        let mut s = Settings::default();
        s.directory.push(dir_entry("w6abc", "546054", true, None));
        s.directory.push(dir_entry("AJ7HR", "77777", true, None));
        s.directory.push(dir_entry("Bravo", "2000", true, None));
        s.directory
            .push(dir_entry("Aardvark", "3000", false, Some(5))); // not a favorite
        let labels: Vec<&str> = s.favorites().iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["AJ7HR", "Bravo", "w6abc"]);
    }

    #[test]
    fn recents_are_newest_first_and_capped_at_ten() {
        let mut s = Settings::default();
        for i in 1..=12 {
            s.directory
                .push(dir_entry(&format!("n{i}"), &format!("{i}"), false, Some(i)));
        }
        // A never-dialed favorite is not a recent.
        s.directory.push(dir_entry("AJ7HR", "77777", true, None));
        let recents = s.recents();
        assert_eq!(recents.len(), 10, "capped at 10, like the Mac");
        assert_eq!(recents[0].node, "12", "newest first");
        assert_eq!(recents[9].node, "3", "the two oldest fall off");
    }

    #[test]
    fn record_recent_appends_new_with_node_as_label() {
        let mut s = Settings::default();
        s.record_recent("546054", 100);
        assert_eq!(s.directory.len(), 1);
        let e = &s.directory[0];
        assert_eq!(e.label, "546054", "unnamed recents are labeled by node");
        assert_eq!(e.node, "546054");
        assert!(!e.favorite);
        assert_eq!(e.last_used, Some(100));
    }

    #[test]
    fn record_recent_bumps_existing_preserving_label_and_favorite() {
        let mut s = Settings::default();
        s.add_favorite("77777", "AJ7HR");
        s.record_recent("77777", 200);
        assert_eq!(s.directory.len(), 1, "upsert by node — no duplicate");
        let e = &s.directory[0];
        assert_eq!(e.label, "AJ7HR", "curated label preserved");
        assert!(e.favorite, "favorite flag preserved");
        assert_eq!(e.last_used, Some(200));
        // Re-dialing later bumps last_used again.
        s.record_recent("77777", 300);
        assert_eq!(s.directory[0].last_used, Some(300));
    }

    #[test]
    fn add_favorite_merges_with_an_existing_recent() {
        let mut s = Settings::default();
        s.record_recent("546054", 100);
        s.add_favorite("546054", "W6ABC Repeater");
        assert_eq!(
            s.directory.len(),
            1,
            "favoriting a recent merges, not duplicates"
        );
        let e = &s.directory[0];
        assert!(e.favorite);
        assert_eq!(e.label, "W6ABC Repeater");
        assert_eq!(e.last_used, Some(100), "recency preserved");
    }

    #[test]
    fn add_favorite_defaults_blank_label_to_node_and_ignores_empty_node() {
        let mut s = Settings::default();
        s.add_favorite("77777", "  ");
        assert_eq!(
            s.directory[0].label, "77777",
            "blank label defaults to the node"
        );
        // A blank relabel keeps the curated label.
        s.add_favorite("77777", "AJ7HR");
        s.add_favorite("77777", "");
        assert_eq!(s.directory[0].label, "AJ7HR");
        // An empty node is a no-op.
        s.add_favorite("   ", "X");
        assert_eq!(s.directory.len(), 1);
    }

    #[test]
    fn remove_favorite_keeps_recents_and_drops_pure_favorites() {
        let mut s = Settings::default();
        // A favorited recent: un-favoriting keeps it as a recent.
        s.record_recent("546054", 100);
        s.add_favorite("546054", "W6ABC Repeater");
        // A never-dialed favorite: un-favoriting removes it entirely.
        s.add_favorite("77777", "AJ7HR");

        s.remove_favorite("546054");
        s.remove_favorite("77777");

        assert_eq!(s.directory.len(), 1, "dead entries don't accumulate");
        let e = &s.directory[0];
        assert_eq!(e.node, "546054");
        assert!(!e.favorite);
        assert_eq!(e.last_used, Some(100));
    }

    #[test]
    fn is_favorite_only_for_favorited_nodes() {
        let mut s = Settings::default();
        s.record_recent("2000", 100);
        s.add_favorite("77777", "AJ7HR");
        assert!(s.is_favorite("77777"));
        assert!(!s.is_favorite("2000"), "a plain recent is not a favorite");
        assert!(!s.is_favorite("12345"));
    }

    // -- stores ---------------------------------------------------------------

    #[test]
    fn mem_store_loads_defaults_then_round_trips() {
        let mut store = MemStore::default();
        assert_eq!(store.load().expect("fresh fake"), Settings::default());
        let full = full_settings();
        store.save(&full).expect("save");
        assert_eq!(store.load().expect("reload"), full);
    }

    #[test]
    fn toml_store_missing_file_loads_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TomlStore::at(dir.path().join("settings.toml"));
        assert_eq!(store.load().expect("missing file"), Settings::default());
    }

    #[test]
    fn toml_store_saves_then_loads_creating_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Parent dir does not exist yet — save must create it (first launch).
        let mut store = TomlStore::at(dir.path().join("nested/config/settings.toml"));
        let full = full_settings();
        store.save(&full).expect("save creates dirs");
        assert_eq!(store.load().expect("reload"), full);
    }

    #[test]
    fn toml_store_save_leaves_only_the_settings_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TomlStore::at(dir.path().join("settings.toml"));
        store.save(&full_settings()).expect("save");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.toml"], "no temp-file droppings");
    }

    #[test]
    fn toml_store_broken_file_is_a_parse_error_not_a_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "this is { not toml").unwrap();
        let store = TomlStore::at(&path);
        match store.load() {
            Err(SettingsError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
