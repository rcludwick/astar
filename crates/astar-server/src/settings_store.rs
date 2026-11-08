// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The settings page's only door to the filesystem.
//!
//! [`SettingsStore`] owns every write the page can cause: `node.toml`
//! through `toml_edit`, `credentials.toml` through the file API on
//! [`SecretProvider`]. The HTTP router never holds a path and never keeps
//! file text between requests — on the VPS the inline registration secret
//! is still in `node.toml` until the operator migrates, and a resident copy
//! would be a second place it could leak from.
//!
//! Reads go back to disk every time. The operator's `sudo -e` is a fact the
//! page must see, not a cache miss: the etag on every save is how that edit
//! becomes a 409 instead of a clobber.
//!
//! What the store deliberately does not know — whether the node is on a
//! net, its running node id, whether the Discord feature is compiled in —
//! arrives as [`StateFacts`] from the route. Reading those off the
//! controller from in here would make the store and the controller each
//! other's dependency.
//!
//! No credential value is ever held in a field of this type. A value passes
//! through [`SettingsStore::save_credential`] as a borrowed `&str` and is
//! gone when it returns; the provider is the route's to update, after the
//! file.

use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde_json::{Value, json};

use crate::{
    config::NodeFileConfig,
    secrets::{
        DISCORD_CREDENTIAL_KEY, LINK_PREFIX, NODE_CREDENTIAL_KEY, PORTAL_CREDENTIAL_KEY,
        SecretProvider,
    },
    settings::{
        FieldError, LIVE_PREFIXES, Pending, RedactedConfig, SettingsPatch, WriteContext,
        apply_patch, check_acknowledgements, etag_of, oracle,
    },
};

/// One of the five credential slots the page can set, parsed from the wire
/// name. Parsing is the whole guard: a name that is not one of these shapes
/// never becomes a file key or a provider key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialName {
    /// The registration password: `node` in the file, `register.node_id`
    /// in the provider.
    Node,
    /// The allstarlink.org account password (`[portal].user`).
    Portal,
    /// The Discord bot token.
    Discord,
    /// Porkbun's API key (`pk1_…`), for the public page's DNS.
    PorkbunPk,
    /// Porkbun's secret API key (`sk1_…`).
    PorkbunSk,
    /// The outbound dial secret for one node: `link:<n>` in both places.
    Link(String),
    /// An inbound peer's secret, under its bare username in both places.
    Peer(String),
}

impl CredentialName {
    /// `"node" | "portal" | "discord" | "link:<digits>" | "peer:<user>"`,
    /// where `<user>` is 1–64 of `[A-Za-z0-9_-]`. The three named slots are
    /// refused as peer names — `peer:discord` would otherwise land on the
    /// bot token's file key. Anything else is
    /// `Err("unknown credential \"<name>\"")`.
    pub fn parse(name: &str) -> Result<Self, String> {
        let unknown = || format!("unknown credential \"{name}\"");
        match name {
            "node" => return Ok(Self::Node),
            "portal" => return Ok(Self::Portal),
            "discord" => return Ok(Self::Discord),
            "porkbun_pk" => return Ok(Self::PorkbunPk),
            "porkbun_sk" => return Ok(Self::PorkbunSk),
            _ => {}
        }
        if let Some(n) = name.strip_prefix("link:") {
            return if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                Ok(Self::Link(n.to_owned()))
            } else {
                Err(unknown())
            };
        }
        if let Some(u) = name.strip_prefix("peer:") {
            let length_ok = (1..=64).contains(&u.len());
            let chars_ok = u
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            let reserved = matches!(
                u,
                "node" | "portal" | "discord" | "porkbun_pk" | "porkbun_sk"
            );
            return if length_ok && chars_ok && !reserved {
                Ok(Self::Peer(u.to_owned()))
            } else {
                Err(unknown())
            };
        }
        Err(unknown())
    }

    /// The key in `credentials.toml`. `node` is a name rather than the node
    /// number so renumbering the node in the Identity card cannot orphan
    /// the password.
    #[must_use]
    pub fn file_key(&self) -> String {
        match self {
            Self::Node => NODE_CREDENTIAL_KEY.to_owned(),
            Self::Portal => PORTAL_CREDENTIAL_KEY.to_owned(),
            Self::Discord => DISCORD_CREDENTIAL_KEY.to_owned(),
            Self::PorkbunPk => crate::secrets::PORKBUN_PK_KEY.to_owned(),
            Self::PorkbunSk => crate::secrets::PORKBUN_SK_KEY.to_owned(),
            Self::Link(n) => format!("{LINK_PREFIX}{n}"),
            Self::Peer(u) => u.clone(),
        }
    }
}

// The wire spelling, for error sentences: `credential "link:2000" could not
// be saved`. Never a value.
impl std::fmt::Display for CredentialName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Node => f.write_str("node"),
            Self::Portal => f.write_str("portal"),
            Self::Discord => f.write_str("discord"),
            Self::PorkbunPk => f.write_str("porkbun_pk"),
            Self::PorkbunSk => f.write_str("porkbun_sk"),
            Self::Link(n) => write!(f, "link:{n}"),
            Self::Peer(u) => write!(f, "peer:{u}"),
        }
    }
}

/// Facts the store cannot know and the route reads off the controller.
#[derive(Debug, Clone, Default)]
pub struct StateFacts {
    /// The node id the RUNNING process registered as, which can differ from
    /// the one on disk until a restart.
    pub node_id: Option<String>,
    /// Whether the running process has a `[register]` section loaded.
    pub registered: bool,
    /// `Some("2 calls, 1 link")` while on a net, else `None`: the sentence
    /// from `NodeController::busy_on_a_net`, shown as `restart.why`.
    pub busy: Option<String>,
    /// `cfg!(feature = "discord")` at the route, so the page can say why the
    /// Discord card does nothing on a build that cannot join.
    pub discord_feature: bool,
}

/// What a successful `save` leaves behind.
#[derive(Debug)]
pub struct SaveOutcome {
    /// Etag of the bytes now on disk; the page carries it into its next save.
    pub etag: String,
    /// Re-parsed from disk after the write. The live tier is applied from
    /// this, never from the patch, so what runs is what the file says.
    pub config: NodeFileConfig,
    /// `false` when the patch changed no byte; the file was not touched.
    pub written: bool,
    /// Wire paths whose disk value changed, both tiers merged, sorted — plus
    /// any patch path whose value already equals the disk value but which
    /// the running node has not taken yet, so the page's Apply on a
    /// hand-edited live field reaches the live tier.
    pub changed: Vec<String>,
}

/// Why a save did not happen. The route maps each variant to a status.
#[derive(Debug)]
pub enum SaveError {
    /// The file is not the one the page loaded (409).
    Conflict,
    /// One entry failed its rule or needs an acknowledgement (400 with `field`).
    Field(FieldError),
    /// The patched document would not load — the daemon's own verdict (400).
    Oracle(String),
    /// The file could not be read or replaced (500).
    Io(String),
}

/// Where a credential landed, for the route to finish the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialOutcome {
    /// What `ProvideSecret` / `SecretProvider::remove` is called with.
    pub provider_key: String,
    /// The key in `credentials.toml`.
    pub file_key: String,
    /// `true` when `credentials.toml` was written: the source is `file`, or
    /// this call migrated to it. `false` means the value lives only in the
    /// running provider until a restart.
    pub persisted: bool,
}

/// Why `credential_key` has no answer. The route maps `Rejected` to 400
/// and `Unreadable` to 500. Both carry a sentence and neither carries a
/// value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKeyError {
    /// The name parsed but this node refuses it — a peer whose username is
    /// the node's own number.
    Rejected(String),
    /// `node.toml` could not be read or parsed: `cannot read <path>: …` or
    /// the redacted parse message.
    Unreadable(String),
}

impl std::fmt::Display for CredentialKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(s) | Self::Unreadable(s) => f.write_str(s),
        }
    }
}

/// The page's view of, and only door to, `node.toml` and `credentials.toml`.
pub struct SettingsStore {
    config_path: PathBuf,
    credentials_path: PathBuf,
    /// The redacted shape the process started with, updated only by
    /// [`SettingsStore::mark_applied`]. `pending` is the diff between this
    /// and the disk, which is what keeps the restart tag honest both ways.
    running: Mutex<RedactedConfig>,
    /// Serialises every write the page can cause — `node.toml` saves,
    /// `credentials.toml` writes, the writability probe. Two pages saving
    /// at once take turns; the second sees a stale etag and gets a conflict
    /// rather than silently undoing the first, and two credential POSTs on
    /// two worker threads cannot lose each other's key.
    write_lock: Mutex<()>,
}

impl SettingsStore {
    /// `running` is `RedactedConfig::from_config` of the config the process
    /// booted from — captured by `main.rs` before the controller is shared.
    #[must_use]
    pub fn new(config_path: PathBuf, credentials_path: PathBuf, running: RedactedConfig) -> Self {
        Self {
            config_path,
            credentials_path,
            running: Mutex::new(running),
            write_lock: Mutex::new(()),
        }
    }

    /// Where `node.toml` is.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Where `credentials.toml` is, whether or not it exists yet.
    #[must_use]
    pub fn credentials_path(&self) -> &Path {
        &self.credentials_path
    }

    /// The file's text. Both failures are `cannot read <path>: …`; a file
    /// that is not UTF-8 is unreadable for our purposes, not a panic.
    fn read_text(&self) -> Result<String, String> {
        let bytes = std::fs::read(&self.config_path)
            .map_err(|e| format!("cannot read {}: {e}", self.config_path.display()))?;
        String::from_utf8(bytes).map_err(|e| {
            format!(
                "cannot read {}: {}",
                self.config_path.display(),
                e.utf8_error()
            )
        })
    }

    /// The text and its parse, through the redacted loader: a parse error
    /// here can be on the line that holds an inline secret.
    fn read_config(&self) -> Result<(String, NodeFileConfig), String> {
        let text = self.read_text()?;
        let cfg = NodeFileConfig::from_toml_str_redacted(&text)?;
        Ok((text, cfg))
    }

    /// The whole `GET /settings/state` body, re-read from disk on every
    /// call. Presence only for credentials, never values; `credential_source`
    /// is lowercased because the loader compares it case-insensitively and
    /// the page compares it with `===`. The writability probe runs under
    /// the write lock: two overlapping polls could otherwise race each
    /// other's probe file and report a writable directory as read-only.
    pub fn state(&self, secrets: &SecretProvider, facts: &StateFacts) -> Result<Value, String> {
        let (text, cfg) = self.read_config()?;
        let disk = RedactedConfig::from_config(&cfg);
        let running = self
            .running
            .lock()
            .expect("settings running mirror poisoned")
            .clone();
        let pending = RedactedConfig::diff(&disk, &running);
        let dir = self.config_path.parent().unwrap_or(Path::new("."));
        let writable = {
            let _serial = self
                .write_lock
                .lock()
                .expect("settings write lock poisoned");
            dir_writable(dir)
        };
        Ok(json!({
            "disk": disk,
            "running": running,
            "pending": pending,
            "live": LIVE_PREFIXES,
            "credentials": credential_presence(&cfg, secrets, facts),
            "credential_source": cfg.secrets.source.to_ascii_lowercase(),
            "credential_file": self.credentials_path.display().to_string(),
            "etag": etag_of(text.as_bytes()),
            "file": {
                "path": self.config_path.display().to_string(),
                "writable": writable,
            },
            "restart": { "busy": facts.busy.is_some(), "why": facts.busy },
            "node_id": facts.node_id,
            "registered": facts.registered,
            "feature": { "discord": facts.discord_feature },
        }))
    }

    /// One save, start to finish, under the write lock: read, compare the
    /// etag, patch, ask the daemon's own loader whether the result boots,
    /// check the acknowledgements, and only then replace the file. A patch
    /// that renders the same bytes returns `written: false` and touches
    /// nothing — no rename, no new mtime, no restart tag — but still names
    /// in `changed` any patched path the running node has not taken yet,
    /// which is how a hand-edited live field is applied from the page.
    pub fn save(&self, req: &SettingsPatch, ctx: &WriteContext) -> Result<SaveOutcome, SaveError> {
        let _serial = self
            .write_lock
            .lock()
            .expect("settings write lock poisoned");
        self.save_locked(req, *ctx)
    }

    /// `save`, then `apply` on what it wrote, then the running mirror
    /// updated with the paths `applied` picks out of `apply`'s result — all
    /// under the one write lock. That is the route's door. Released between
    /// the write and the apply, the lock would let a second save write,
    /// apply and mark in the gap, and the first save's apply would then put
    /// its older config back on the station while the mirror still claimed
    /// the second's: `pending` empty, and a value running that the file
    /// does not say. `apply` is not called when the save fails.
    pub fn save_and_apply<R>(
        &self,
        req: &SettingsPatch,
        ctx: &WriteContext,
        apply: impl FnOnce(&SaveOutcome) -> R,
        applied: impl FnOnce(&R) -> &[String],
    ) -> Result<(SaveOutcome, R), SaveError> {
        let _serial = self
            .write_lock
            .lock()
            .expect("settings write lock poisoned");
        let outcome = self.save_locked(req, *ctx)?;
        let result = apply(&outcome);
        self.mark_applied(
            applied(&result),
            &RedactedConfig::from_config(&outcome.config),
        );
        Ok((outcome, result))
    }

    /// The body of `save`. The caller holds `write_lock`.
    fn save_locked(
        &self,
        req: &SettingsPatch,
        ctx: WriteContext,
    ) -> Result<SaveOutcome, SaveError> {
        let text = self.read_text().map_err(SaveError::Io)?;
        if etag_of(text.as_bytes()) != req.etag {
            return Err(SaveError::Conflict);
        }
        // A disk file that no longer loads is the loader's verdict too; the
        // operator needs the message, not a 500.
        let before_cfg =
            NodeFileConfig::from_toml_str_redacted(&text).map_err(SaveError::Oracle)?;
        let before = RedactedConfig::from_config(&before_cfg);
        let new_text = apply_patch(&text, &req.patch).map_err(SaveError::Field)?;
        if let Some(Value::String(file)) = req.patch.get("credentials.file")
            && let Some(message) = credentials_file_problem(&self.config_path, file)
        {
            return Err(SaveError::Field(FieldError {
                field: "credentials.file".to_owned(),
                message,
                acknowledge: None,
            }));
        }
        let after_cfg = oracle(&new_text).map_err(SaveError::Oracle)?;
        let after = RedactedConfig::from_config(&after_cfg);
        check_acknowledgements(&before, &after, req, &ctx).map_err(SaveError::Field)?;

        // What differs on disk after this save, plus any patched path whose
        // value already matched the disk but which the running node has not
        // taken: the disk-vs-running diff says so, and the patch naming it
        // is the page asking for it to be applied.
        let still_pending = {
            let running = self
                .running
                .lock()
                .expect("settings running mirror poisoned");
            RedactedConfig::diff(&before, &running).apply
        };
        let mut changed = merged_paths(RedactedConfig::diff(&before, &after));
        for path in req.patch.keys() {
            if still_pending.contains(path) && !changed.contains(path) {
                changed.push(path.clone());
            }
        }
        changed.sort();

        if new_text == text {
            return Ok(SaveOutcome {
                etag: req.etag.clone(),
                config: after_cfg,
                written: false,
                changed,
            });
        }
        self.write_atomic(&new_text).map_err(SaveError::Io)?;
        let (written, config) = self.read_config().map_err(SaveError::Io)?;
        Ok(SaveOutcome {
            etag: etag_of(written.as_bytes()),
            config,
            written: true,
            changed,
        })
    }

    /// `<name>.tmp` beside the target, fsync, rename: a crash at any point
    /// leaves the old file or the new one, never a torn one. The temp is
    /// removed on failure so it is never mistaken for the config.
    fn write_atomic(&self, text: &str) -> Result<(), String> {
        let path = &self.config_path;
        let mut tmp_name = path
            .file_name()
            .map_or_else(std::ffi::OsString::new, ToOwned::to_owned);
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        if let Err(e) = write_then_rename(&tmp, path, text) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {e}", path.display()));
        }
        // Best effort: an fsync of the directory makes the rename itself
        // durable. Nothing to report when it is refused.
        if let Some(dir) = path.parent()
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// After the live tier applied `paths`, make the mirror say so. A path
    /// absent from `from` (a removed `[bridge]`) is removed here too, or it
    /// would sit in `pending.apply` forever. Private: only
    /// `save_and_apply` calls it, under the write lock, so the mirror can
    /// never be told about an apply that a later save has already
    /// superseded.
    fn mark_applied(&self, paths: &[String], from: &RedactedConfig) {
        let mut running = self
            .running
            .lock()
            .expect("settings running mirror poisoned");
        for path in paths {
            // `if let … else`, not a two-arm `match`: pedantic clippy's
            // `single_match_else` refuses that shape.
            if let Some(v) = from.get(path) {
                running.0.insert(path.clone(), v.clone());
            } else {
                running.0.remove(path);
            }
        }
    }

    /// Provider key for a slot, from the config on disk: `Node` is
    /// `register.node_id` or the literal `node`; the other four are their
    /// file keys. Read fresh, because the Identity card can have renumbered
    /// the node since the process started.
    pub fn credential_key(&self, name: &CredentialName) -> Result<String, CredentialKeyError> {
        let (_, cfg) = self.read_config().map_err(CredentialKeyError::Unreadable)?;
        provider_key_for(&cfg, name).map_err(CredentialKeyError::Rejected)
    }

    /// Write one credential to `credentials.toml` — when the disk source is
    /// `file`, or when `migrate` asks to make it so. Never calls the
    /// provider: the route does that after this returns `Ok`, so a crash in
    /// between leaves a value that will be loaded at the next start rather
    /// than one that is lost. Holds the write lock for the whole call: two
    /// credential POSTs run on two worker threads, and two read-modify-write
    /// cycles on the file would otherwise lose one key. `Err` names the
    /// slot and the path, never the value.
    pub fn save_credential(
        &self,
        secrets: &SecretProvider,
        name: &CredentialName,
        value: &str,
        migrate: bool,
    ) -> Result<CredentialOutcome, String> {
        let _serial = self
            .write_lock
            .lock()
            .expect("settings write lock poisoned");
        let fail = |e: String| format!("credential \"{name}\" could not be saved: {e}");
        let (_, cfg) = self.read_config().map_err(fail)?;
        // The route has already refused this through `credential_key`; the
        // bare sentence here keeps an embedder that skips that step honest.
        let key = provider_key_for(&cfg, name)?;
        let file_key = name.file_key();
        let on_file = cfg.secrets.source.eq_ignore_ascii_case("file");
        if !on_file && !migrate {
            return Ok(CredentialOutcome {
                provider_key: key,
                file_key,
                persisted: false,
            });
        }
        // `validate_field` refuses `file = "node.toml"`, but it cannot see a
        // renamed config (`--config site.toml` with `file = "site.toml"`).
        // Checked before the migration, so neither `export_file` nor
        // `save_file` ever opens the config as the credentials file.
        if same_file(&self.credentials_path, &self.config_path) {
            return Err(fail(format!(
                "{} is the node's own config; credentials never go in it",
                self.config_path.display()
            )));
        }
        if !on_file {
            self.migrate_to_file(secrets, &cfg).map_err(fail)?;
        }
        SecretProvider::save_file(&self.credentials_path, &file_key, value).map_err(fail)?;
        Ok(CredentialOutcome {
            provider_key: key,
            file_key,
            persisted: true,
        })
    }

    /// The one-way move off `env`, `control` or `config`, in an order a
    /// crash cannot make worse. (1) Every credential the running provider
    /// holds is exported to `credentials.toml` — under `env` that is the
    /// link secrets, the bot token and the portal password too, and under
    /// `config` the inline password already loaded under the node id, which
    /// lands as `node`. (1b) An inline `[secrets].secret` the provider never
    /// loaded is captured as `node` as well: `main.rs` loads it only inside
    /// `if let Some(reg)`, and the template flow fills `secret` in before
    /// `[register]` is uncommented, so on such a node the provider has
    /// nothing for it and step (2) would blank the only copy. When the
    /// provider does hold a value under the node key, its copy is the more
    /// current one and (1) already wrote it. (2) `node.toml` gets
    /// `source = "file"` and, when the key is present, `secret = ""`, in ONE
    /// atomic write, so the flip and the blank cannot be seen apart. Stop
    /// after (1) and the node still authenticates from its old source at
    /// the next start; stop after (2) and it authenticates from the file it
    /// was just given. The caller's `save_file` is (3).
    ///
    /// The two keys are replaced through `get_mut`, never `Table::insert`:
    /// in `toml_edit` 0.25 `insert` on an existing key reformats that key
    /// and drops the comment and whitespace above it (`Key::fmt` clears the
    /// leaf decor), which is exactly the comment an operator wrote over
    /// `source`. Assigning through `get_mut` leaves the key untouched.
    ///
    /// The caller holds `write_lock` (std's `Mutex` is not reentrant, so
    /// this function must not take it again). No etag here: this is not a
    /// patch, and a page that was open now holds a stale one, which is the
    /// right outcome — its next save is a conflict and it reloads a file
    /// that says `file`.
    fn migrate_to_file(
        &self,
        secrets: &SecretProvider,
        cfg: &NodeFileConfig,
    ) -> Result<(), String> {
        let node_id = cfg.register.as_ref().map(|r| r.node_id.as_str());
        secrets.export_file(&self.credentials_path, node_id)?;
        if cfg.secrets.source.eq_ignore_ascii_case("config")
            && secrets.resolve(node_provider_key(cfg)).is_empty()
            && let Some(inline) = cfg.secrets.secret.as_deref().filter(|s| !s.is_empty())
        {
            SecretProvider::save_file(&self.credentials_path, NODE_CREDENTIAL_KEY, inline)?;
        }

        let text = self.read_text()?;
        // `message()`, never `Display`: the rendered form quotes the source
        // line, which on a `config` node is the one holding the secret.
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("{}: {}", self.config_path.display(), e.message()))?;
        let item = doc
            .as_table_mut()
            .entry("secrets")
            .or_insert(toml_edit::table());
        let Some(table) = item.as_table_like_mut() else {
            return Err(format!(
                "{}: [secrets] is not a table",
                self.config_path.display()
            ));
        };
        // `if let … else`, not a two-arm `match`: pedantic clippy's
        // `single_match_else` refuses that shape.
        if let Some(source) = table.get_mut("source") {
            *source = toml_edit::value("file");
        } else {
            table.insert("source", toml_edit::value("file"));
        }
        // Every inline credential, not just the registration password:
        // export_file above already put the provider's copies in the file.
        for key in crate::config::INLINE_CREDENTIAL_KEYS {
            if let Some(item) = table.get_mut(key) {
                *item = toml_edit::value("");
            }
        }
        let new_text = doc.to_string();
        oracle(&new_text)?;
        self.write_atomic(&new_text)
    }

    /// Remove one credential from `credentials.toml` when the file exists,
    /// and say which provider key the route should `remove` (for `Node` the
    /// route also removes the literal `node`, where a password saved before
    /// the number lives). A page that can set a credential but not revoke
    /// it is the more dangerous one. Holds the write lock like
    /// `save_credential`. The `Err` prefix is `could not be removed`, so a
    /// failed forget never reads as a failed save.
    pub fn forget_credential(&self, name: &CredentialName) -> Result<CredentialOutcome, String> {
        let _serial = self
            .write_lock
            .lock()
            .expect("settings write lock poisoned");
        let fail = |e: String| format!("credential \"{name}\" could not be removed: {e}");
        let (_, cfg) = self.read_config().map_err(fail)?;
        let key = provider_key_for(&cfg, name)?;
        // The same alias `save_credential` refuses: on a config that is its
        // own credentials path, `remove_from_file` would delete a top-level
        // table of the config (`[portal]`, `[discord]`, or `[listener]`
        // through `peer:listener`) and rewrite it at 0600, past the etag
        // and the oracle.
        if same_file(&self.credentials_path, &self.config_path) {
            return Err(fail(format!(
                "{} is the node's own config; credentials never go in it",
                self.config_path.display()
            )));
        }
        let file_key = name.file_key();
        let persisted = self.credentials_path.exists();
        if persisted {
            SecretProvider::remove_from_file(&self.credentials_path, &file_key).map_err(fail)?;
        }
        Ok(CredentialOutcome {
            provider_key: key,
            file_key,
            persisted,
        })
    }
}

/// `path` as the file it names: canonical when it exists, else its parent
/// made canonical with the name put back. `None` when the parent does not
/// exist either. Symlinks and `.`/`..` are resolved, so two spellings of
/// one file compare equal.
fn resolve_file(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Some(p);
    }
    let name = path.file_name()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.canonicalize().ok().map(|p| p.join(name))
}

/// Whether `a` and `b` are the same file, by any spelling.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || resolve_file(a).is_some_and(|ra| resolve_file(b).is_some_and(|rb| ra == rb))
}

/// The settings page's half of the `credentials.file` rule that needs the
/// config's location. Every credential save read-modify-writes that file
/// and renames it into place at 0600, so a value from the page must name a
/// file directly in the config's own directory or in the state directory —
/// resolved the way the loader resolves it (relative to the config), with
/// symlinks followed — and never the config itself. `validate_field` has
/// already refused `..`. The loader does not apply this: a path the
/// operator writes into the file by hand is theirs to choose.
pub(crate) fn credentials_file_problem(config_path: &Path, file: &str) -> Option<String> {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(target) = resolve_file(&dir.join(file)) else {
        return Some(format!(
            "credentials.file: {file:?}: its directory does not exist"
        ));
    };
    if resolve_file(config_path).is_some_and(|c| c == target) {
        return Some(format!(
            "credentials.file: {file:?} is the node's own config; credentials never go in it"
        ));
    }
    let state = crate::config::state_dir(config_path);
    let roots = [
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
        state.canonicalize().unwrap_or(state),
    ];
    if roots.iter().any(|r| target.parent() == Some(r.as_path())) {
        None
    } else {
        Some(format!(
            "credentials.file: {file:?} must be a file directly in {} or {}",
            roots[0].display(),
            roots[1].display()
        ))
    }
}

/// The provider key the registration password lives under: the node number
/// when `[register]` exists — that is the username the resolver is asked
/// for (main.rs:121-127) — else the literal `node`, so a password set before
/// the number is not lost.
fn node_provider_key(cfg: &NodeFileConfig) -> &str {
    cfg.register
        .as_ref()
        .map_or(NODE_CREDENTIAL_KEY, |r| r.node_id.as_str())
}

/// The provider key for `name` under `cfg`. A peer whose username is this
/// node's own number is refused: it would be a second file key for the
/// provider key `node` already maps to, and the last one loaded would win
/// silently. The `Err` is the bare sentence; callers decide the status.
fn provider_key_for(cfg: &NodeFileConfig, name: &CredentialName) -> Result<String, String> {
    match name {
        CredentialName::Node => Ok(node_provider_key(cfg).to_owned()),
        CredentialName::Peer(u) if cfg.register.as_ref().is_some_and(|r| r.node_id == *u) => Err(
            format!("credential \"peer:{u}\" is this node's own number — set it as \"node\""),
        ),
        other => Ok(other.file_key()),
    }
}

/// Presence only, never values: the `has_token` idiom (controller.rs:1431).
/// `resolve` returns `""` for an unknown username, so non-empty is the whole
/// test, and it is asked fresh every time because `ProvideSecret` can land
/// between two polls. The node password counts as set under ANY of its
/// three keys: the number on disk; the literal `node`, where a password
/// saved before `[register]` existed lives until the next start; and the
/// id the process is RUNNING as (`facts.node_id`), which is where
/// `load_file` put a `file`-source node's password at startup — so a
/// renumbering saved to disk does not orphan it until the restart. `links`
/// is the union of the shapes on disk and the `link:` usernames in the
/// provider; `peers` is everything else the provider holds that is not one
/// of the named slots or the running id.
fn credential_presence(
    cfg: &NodeFileConfig,
    secrets: &SecretProvider,
    facts: &StateFacts,
) -> Value {
    let node_key = node_provider_key(cfg);
    let running_key = facts.node_id.as_deref();
    let presence = |key: &str| {
        if secrets.resolve(key).is_empty() {
            "unset"
        } else {
            "set"
        }
    };
    let node = if presence(node_key) == "set"
        || presence(NODE_CREDENTIAL_KEY) == "set"
        || running_key.is_some_and(|k| presence(k) == "set")
    {
        "set"
    } else {
        "unset"
    };
    let usernames = secrets.usernames();
    let mut links: BTreeMap<String, &str> = BTreeMap::new();
    for n in cfg.links.keys() {
        links.insert(n.clone(), presence(&format!("{LINK_PREFIX}{n}")));
    }
    for u in &usernames {
        if let Some(n) = u.strip_prefix(LINK_PREFIX) {
            links.insert(n.to_owned(), presence(u));
        }
    }
    let peers: BTreeMap<&str, &str> = usernames
        .iter()
        .map(String::as_str)
        .filter(|u| {
            *u != node_key && Some(*u) != running_key && crate::secrets::is_inbound_username(u)
        })
        .map(|u| (u, "set"))
        .collect();
    json!({
        "node": node,
        "portal": presence(PORTAL_CREDENTIAL_KEY),
        "discord": presence(DISCORD_CREDENTIAL_KEY),
        "porkbun_pk": presence(crate::secrets::PORKBUN_PK_KEY),
        "porkbun_sk": presence(crate::secrets::PORKBUN_SK_KEY),
        "links": links,
        "peers": peers,
    })
}

/// Whether this process can create a file in `dir`, which is what a rename
/// needs. A probe rather than a permission-bit read: ACLs, read-only mounts
/// and a root-owned `/etc/iaxnode` under uid 1000 all answer honestly this
/// way, and the page shows a red `file` badge instead of failing on save.
///
/// Any other error is `false`; the `AlreadyExists` arm exists because a
/// probe left behind by a process killed between `open` and `remove_file`
/// would otherwise report the directory unwritable forever. Callers hold
/// the store's write lock, so two probes never interleave.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".astar-settings-probe");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        // A probe left by an interrupted earlier call: deleting it needs
        // the same directory permission creating it did, so that is the
        // answer.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&probe).is_ok()
        }
        Err(_) => false,
    }
}

/// Both tiers of a diff as one sorted list — the base of
/// `SaveOutcome::changed`, which the route splits again by `tier()` after
/// it knows what applied.
fn merged_paths(pending: Pending) -> Vec<String> {
    let mut changed = pending.restart;
    changed.extend(pending.apply);
    changed.sort();
    changed.dedup();
    changed
}

/// Create `tmp` with `target`'s permission bits, write, fsync, rename over
/// `target`. The mode is set ON the open (`create_new` + `mode`), never by a
/// later chmod: until a node migrates, `node.toml` can hold the inline
/// registration password, and a temp that existed even briefly at
/// `0666 & !umask` could be opened by another local user, whose descriptor
/// would then read every byte written after it. A temp left by an
/// interrupted earlier write is removed first, so it cannot make every
/// later save fail. `set_permissions` still follows the open, because the
/// umask strips bits from the open's mode, and a `0604` config must come
/// back `0604`, not `0600`.
fn write_then_rename(tmp: &Path, target: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mode = std::fs::metadata(target)?.permissions().mode() & 0o777;
        options.mode(mode);
        mode
    };
    let _ = std::fs::remove_file(tmp);
    let mut file = options.open(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::template::NODE_TOML_TEMPLATE;

    /// A registered node on the `file` source with one link shape, a portal
    /// account and a Discord section: every credential slot has somewhere
    /// to show up. The leading comment is what the in-place edits must keep.
    const FIXED_TOML: &str = r#"# a fixed node — the fixture every state assertion is built on
[listener]
bind      = "0.0.0.0:4569"
answer    = "auto"
max_calls = 2
auth      = "required"
allowed_nodes = ["2000"]

[bridge]
mode                = "bridge"
mix_minus           = true
include_local_radio = false

[register]
peer    = "52.20.63.146:4569"
node_id = "12345"

[control]
bind = "127.0.0.1:8730"

[secrets]
source = "file"

[portal]
user = "K0ABC"
node = "12345"

[links."2000"]
shape = "wt-guest"

[discord]
guild_id   = "123456789012345678"
channel_id = "223456789012345678"
"#;

    /// `FIXED_TOML` plus a `[wireguard]` section, whose `secret_ref` key must
    /// reach the wire as `wireguard.key_ref` and nothing else.
    fn wide_toml() -> String {
        format!(
            "{FIXED_TOML}\n[wireguard]\nenabled = false\naddress = \"10.99.0.2/32\"\n\
             peer_public_key = \"AAAA\"\nendpoint = \"vps.example.org:51820\"\n\
             secret_ref = \"WIREGUARD_PRIVATE_KEY\"\n"
        )
    }

    /// `FIXED_TOML` on the `env` source.
    fn env_toml() -> String {
        FIXED_TOML.replace("source = \"file\"", "source = \"env\"")
    }

    /// `FIXED_TOML` the way the VPS is configured today: `source = "config"`
    /// with the registration password inline, and a comment directly above
    /// the `source` key — the one place an in-place edit is most likely to
    /// eat a comment, and the one the migration must leave where it is.
    fn config_source_toml() -> String {
        FIXED_TOML.replace(
            "[secrets]\nsource = \"file\"\n",
            "[secrets]\n# the source line: kept in place by the migration\nsource = \"config\"\nsecret = \"inline-pw-SENTINEL\"\n",
        )
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: SettingsStore,
    }

    /// `node.toml` and the store over it, the running mirror captured from
    /// the same text — exactly what `main.rs` does at startup.
    fn fixture(toml: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("node.toml");
        std::fs::write(&config, toml).expect("write the fixture");
        let cfg = NodeFileConfig::from_toml_str_redacted(toml).expect("the fixture parses");
        let store = SettingsStore::new(
            config,
            dir.path().join("credentials.toml"),
            RedactedConfig::from_config(&cfg),
        );
        Fixture { dir, store }
    }

    fn facts() -> StateFacts {
        StateFacts {
            node_id: Some("12345".to_owned()),
            registered: true,
            busy: None,
            discord_feature: false,
        }
    }

    fn etag(store: &SettingsStore) -> String {
        store
            .state(&SecretProvider::new(), &facts())
            .expect("state")["etag"]
            .as_str()
            .expect("etag is a string")
            .to_owned()
    }

    fn patch(store: &SettingsStore, entries: &[(&str, Value)]) -> SettingsPatch {
        SettingsPatch {
            etag: etag(store),
            patch: entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            acknowledge: Vec::new(),
        }
    }

    // -- state ------------------------------------------------------------------

    #[test]
    fn state_has_every_key_field_by_field() {
        let f = fixture(FIXED_TOML);
        let secrets = SecretProvider::new();
        secrets.put("12345", "node-pw");
        secrets.put("discord", "bot-value");
        secrets.put("link:2001", "dial-2001");
        secrets.put("allstar-public", "peer-pw");

        let state = f.store.state(&secrets, &facts()).expect("state");

        // `disk` is `RedactedConfig`'s flat map of the same fixture; pin two entries
        // so a wrong shape cannot hide behind an equal-but-empty object.
        let cfg = NodeFileConfig::from_toml_str_redacted(FIXED_TOML).unwrap();
        let disk = serde_json::to_value(RedactedConfig::from_config(&cfg)).unwrap();
        assert_eq!(disk["register.node_id"], json!("12345"));
        assert_eq!(disk["links.\"2000\".shape"], json!("wt-guest"));

        let expected = json!({
            "disk": disk,
            "running": disk,
            "pending": { "restart": [], "apply": [] },
            "live": ["bridge.", "parrot.", "audio.input", "audio.output", "m17.enabled", "m17.reflectors", "packet_log."],
            "credentials": {
                "node": "set",
                "portal": "unset",
                "discord": "set",
                "porkbun_pk": "unset",
                "porkbun_sk": "unset",
                "links": { "2000": "unset", "2001": "set" },
                "peers": { "allstar-public": "set" }
            },
            "credential_source": "file",
            "credential_file": f.dir.path().join("credentials.toml").display().to_string(),
            "etag": etag_of(FIXED_TOML.as_bytes()),
            "file": {
                "path": f.dir.path().join("node.toml").display().to_string(),
                "writable": true
            },
            "restart": { "busy": false, "why": null },
            "node_id": "12345",
            "registered": true,
            "feature": { "discord": false }
        });
        assert_eq!(state, expected);
    }

    #[test]
    fn restart_reports_the_busy_sentence_and_the_feature() {
        let f = fixture(FIXED_TOML);
        let facts = StateFacts {
            busy: Some("2 calls, 1 link".to_owned()),
            discord_feature: true,
            ..facts()
        };
        let state = f
            .store
            .state(&SecretProvider::new(), &facts)
            .expect("state");
        assert_eq!(
            state["restart"],
            json!({ "busy": true, "why": "2 calls, 1 link" })
        );
        assert_eq!(state["feature"], json!({ "discord": true }));
    }

    #[test]
    fn a_stored_dns_token_is_not_listed_as_an_inbound_peer() {
        let f = fixture(NODE_TOML_TEMPLATE);
        let secrets = SecretProvider::new();
        secrets.put("12345", "pw");
        secrets.put("dns:cloudflare", "tok");
        let state = f
            .store
            .state(&secrets, &StateFacts::default())
            .expect("state");
        assert_eq!(state["credentials"]["peers"], json!({ "12345": "set" }));
    }

    #[test]
    fn settings_state_never_lists_the_echolink_key() {
        let f = fixture(NODE_TOML_TEMPLATE);
        let secrets = SecretProvider::new();
        secrets.put("12345", "pw");
        secrets.put("echolink:KX7ABC-L", "el-pw-SENTINEL");
        let state = f
            .store
            .state(&secrets, &StateFacts::default())
            .expect("state");
        assert_eq!(state["credentials"]["peers"], json!({ "12345": "set" }));
        let text = state.to_string();
        assert!(
            !text.contains("echolink") && !text.contains("SENTINEL"),
            "{text}"
        );
    }

    #[test]
    fn settings_credential_refuses_an_echolink_name() {
        for bad in ["echolink:KX7ABC-L", "peer:echolink:KX7ABC-L", "echolink"] {
            assert_eq!(
                CredentialName::parse(bad).unwrap_err(),
                format!("unknown credential \"{bad}\""),
            );
        }
    }

    #[test]
    fn settings_save_with_discord_and_echolink_is_refused() {
        let without_discord = FIXED_TOML
            .split("[discord]")
            .next()
            .expect("FIXED_TOML has text before [discord]");
        let toml = format!("{without_discord}[echolink]\ncallsign = \"KX7ABC-L\"\n");
        let f = fixture(&toml);
        let req = patch(
            &f.store,
            &[
                ("discord.guild_id", json!("123456789012345678")),
                ("discord.channel_id", json!("223456789012345678")),
            ],
        );
        match f.store.save(&req, &WriteContext::default()) {
            Err(SaveError::Oracle(msg)) => assert!(
                msg.starts_with("echolink: can't run with [discord] — EchoLink's access policy"),
                "{msg}"
            ),
            other => panic!("expected the oracle's refusal, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(f.store.config_path()).unwrap(),
            toml,
            "a refused save writes nothing"
        );
    }

    /// The public listener beside `[echolink]`, turned on from the page:
    /// `[web] public_bind` or `[stream] enabled`. Each is refused with the
    /// loader's sentence, and nothing is written.
    #[test]
    fn settings_save_with_the_public_listener_and_echolink_is_refused() {
        let without_discord = FIXED_TOML
            .split("[discord]")
            .next()
            .expect("FIXED_TOML has text before [discord]");
        let toml = format!("{without_discord}[echolink]\ncallsign = \"KX7ABC-L\"\n");
        for (path, value) in [
            ("web.public_bind", json!("0.0.0.0:443")),
            ("stream.enabled", json!(true)),
        ] {
            let f = fixture(&toml);
            let req = patch(&f.store, &[(path, value)]);
            match f.store.save(&req, &WriteContext::default()) {
                Err(SaveError::Oracle(msg)) => assert!(
                    msg.starts_with(
                        "echolink: can't run with the public listener ([web] public_bind or [stream] enabled) — EchoLink's access policy"
                    ),
                    "{path}: {msg}"
                ),
                other => panic!("{path}: expected the oracle's refusal, got {other:?}"),
            }
            assert_eq!(
                std::fs::read_to_string(f.store.config_path()).unwrap(),
                toml,
                "{path}: a refused save writes nothing"
            );
        }
    }

    #[test]
    fn without_register_the_node_credential_is_the_literal_node_key() {
        // A fresh template node: no [register] yet. A password saved before
        // the number lives under "node"; the number itself is just a peer.
        let f = fixture(NODE_TOML_TEMPLATE);
        let secrets = SecretProvider::new();
        secrets.put("12345", "pw");
        let facts = StateFacts::default();

        let state = f.store.state(&secrets, &facts).expect("state");
        assert_eq!(state["credentials"]["node"], json!("unset"));
        assert_eq!(state["credentials"]["peers"], json!({ "12345": "set" }));
        assert_eq!(state["node_id"], Value::Null);
        assert_eq!(state["registered"], json!(false));

        secrets.put("node", "pw");
        let state = f.store.state(&secrets, &facts).expect("state");
        assert_eq!(state["credentials"]["node"], json!("set"));
        assert_eq!(state["credentials"]["peers"], json!({ "12345": "set" }));
    }

    #[test]
    fn a_password_saved_before_the_number_still_reads_set_after_the_number_lands() {
        // The bootstrap flow in the other order: password first (stored
        // under the literal "node" while [register] is absent), then the
        // node number saved to disk. Until the restart re-maps the file's
        // `node` key, the running provider still holds it under "node" —
        // and the chip must not flip back to "not set".
        let f = fixture(FIXED_TOML);
        let secrets = SecretProvider::new();
        secrets.put("node", "pw");
        let state = f.store.state(&secrets, &facts()).expect("state");
        assert_eq!(state["credentials"]["node"], json!("set"));
        assert_eq!(
            state["credentials"]["peers"],
            json!({}),
            "\"node\" is never a peer"
        );
    }

    #[test]
    fn renumbering_on_disk_does_not_orphan_the_running_password() {
        // A `file`-source node started as 12345: `load_file` put the file's
        // `node` entry under "12345", not under the literal "node". The
        // operator then saves register.node_id = "777" (restart tier, disk
        // only). Until the restart, the password still lives under the
        // RUNNING id — the chip must stay "set" and 12345 is not a peer.
        let f = fixture(&FIXED_TOML.replace("node_id = \"12345\"", "node_id = \"777\""));
        let secrets = SecretProvider::new();
        secrets.put("12345", "pw");
        let state = f.store.state(&secrets, &facts()).expect("state");
        assert_eq!(
            state["disk"]["register.node_id"],
            json!("777"),
            "the fixture edit must take"
        );
        assert_eq!(state["node_id"], json!("12345"));
        assert_eq!(state["credentials"]["node"], json!("set"));
        assert_eq!(
            state["credentials"]["peers"],
            json!({}),
            "the running id is never a peer"
        );
    }

    #[test]
    fn a_missing_node_toml_is_an_error_not_a_panic() {
        let f = fixture(FIXED_TOML);
        std::fs::remove_file(f.store.config_path()).unwrap();
        let err = f
            .store
            .state(&SecretProvider::new(), &facts())
            .expect_err("missing file");
        assert_eq!(
            err,
            format!(
                "cannot read {}: {}",
                f.store.config_path().display(),
                std::fs::read(f.store.config_path()).unwrap_err()
            )
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_are_an_error_not_a_panic() {
        let f = fixture(FIXED_TOML);
        let bytes: &[u8] = b"[listener]\nbind = \"\xff\xfe\"\n";
        std::fs::write(f.store.config_path(), bytes).unwrap();
        let err = f
            .store
            .state(&SecretProvider::new(), &facts())
            .expect_err("non-utf8");
        assert_eq!(
            err,
            format!(
                "cannot read {}: {}",
                f.store.config_path().display(),
                String::from_utf8(bytes.to_vec()).unwrap_err().utf8_error()
            )
        );
    }

    #[test]
    fn a_broken_node_toml_reports_the_message_without_the_line() {
        // The syntax error is ON the line holding the inline secret — the
        // exact VPS shape — so a loader that forwards toml's rendered
        // snippet would quote it. The store must use the redacted loader.
        let f = fixture(FIXED_TOML);
        let text = "[listener]\nbind = \"0.0.0.0:4569\"\nanswer = \"auto\"\nmax_calls = 2\nauth = \"off\"\n\
                    [control]\nbind = \"127.0.0.1:8730\"\n[secrets]\nsource = \"config\"\nsecret = \"SENTINEL-9f3a\n";
        std::fs::write(f.store.config_path(), text).unwrap();
        let err = f
            .store
            .state(&SecretProvider::new(), &facts())
            .expect_err("unterminated string");
        // The redacted loader's own message, verbatim: the store adds nothing
        // to it — in particular no `cannot read` prefix, which would make a
        // parse error look like a missing file.
        assert_eq!(
            err,
            NodeFileConfig::from_toml_str_redacted(text).unwrap_err()
        );
        assert!(
            !err.contains("SENTINEL"),
            "the parse error must not quote the line: {err}"
        );
    }

    // -- CredentialName --------------------------------------------------------

    #[test]
    fn credential_names_parse_to_their_slots_and_file_keys() {
        let cases = [
            ("node", CredentialName::Node, "node"),
            ("portal", CredentialName::Portal, "portal"),
            ("discord", CredentialName::Discord, "discord"),
            ("porkbun_pk", CredentialName::PorkbunPk, "porkbun_pk"),
            ("porkbun_sk", CredentialName::PorkbunSk, "porkbun_sk"),
            (
                "link:2000",
                CredentialName::Link("2000".to_owned()),
                "link:2000",
            ),
            (
                "peer:allstar-public",
                CredentialName::Peer("allstar-public".to_owned()),
                "allstar-public",
            ),
            (
                "peer:W1AW_2",
                CredentialName::Peer("W1AW_2".to_owned()),
                "W1AW_2",
            ),
        ];
        for (wire, parsed, file_key) in cases {
            let name = CredentialName::parse(wire).unwrap_or_else(|e| panic!("{wire}: {e}"));
            assert_eq!(name, parsed);
            assert_eq!(name.file_key(), file_key);
            assert_eq!(name.to_string(), wire, "Display is the wire spelling");
        }
    }

    #[test]
    fn a_name_outside_the_five_shapes_is_unknown() {
        let sixty_five = format!("peer:{}", "a".repeat(65));
        for bad in [
            "",
            "Node",
            "secret",
            "link:",
            "link:12a",
            "link:2000 ",
            "peer:",
            "peer:has space",
            "peer:a:b",
            "peer:../x",
            // The three named slots are not peer names — `peer:discord`
            // would otherwise write over the bot token's file key.
            "peer:node",
            "peer:portal",
            "peer:discord",
            "peer:porkbun_pk",
            "peer:porkbun_sk",
            sixty_five.as_str(),
        ] {
            assert_eq!(
                CredentialName::parse(bad).unwrap_err(),
                format!("unknown credential \"{bad}\""),
                "{bad:?}"
            );
        }
        let sixty_four = format!("peer:{}", "a".repeat(64));
        assert!(
            CredentialName::parse(&sixty_four).is_ok(),
            "64 is the limit, inclusive"
        );
    }

    #[test]
    fn state_never_carries_the_words_secret_password_or_token() {
        let f = fixture(&wide_toml());
        let secrets = SecretProvider::new();
        for (key, value) in [
            ("12345", "SENTINEL-node-9f3a"),
            ("portal", "SENTINEL-portal-1c0b"),
            ("discord", "SENTINEL-bot-7e2d"),
            ("link:2000", "SENTINEL-link-4455"),
            ("allstar-public", "SENTINEL-peer-a1b2"),
        ] {
            secrets.put(key, value);
        }
        let state = f.store.state(&secrets, &facts()).expect("state");
        let body = state.to_string();
        // Copied from `status_route_returns_200_secret_free` (http.rs:775-780).
        for bad in ["secret", "password", "token"] {
            assert!(
                !body.contains(bad),
                "state body must not contain '{bad}': {body}"
            );
        }
        assert!(
            !body.contains("SENTINEL-"),
            "no credential value may reach the state: {body}"
        );
        assert_eq!(
            state["credentials"],
            json!({
                "node": "set", "portal": "set", "discord": "set",
                "porkbun_pk": "unset", "porkbun_sk": "unset",
                "links": { "2000": "set" },
                "peers": { "allstar-public": "set" }
            })
        );
        // The secret_ref → key_ref rename is what lets a [wireguard] node pass the loop above.
        assert_eq!(
            state["disk"]["wireguard.key_ref"],
            json!("WIREGUARD_PRIVATE_KEY")
        );
    }

    // -- save ---------------------------------------------------------------------

    /// An operator's hand edit of `node.toml` (an editor on the host, or a
    /// deploy that rewrites it) while the page is open. The page's 5 s poll never refreshes the etag
    /// it saves with, so the next save must be a conflict that leaves the
    /// hand edit on disk byte for byte — and the save after a reload goes in.
    #[test]
    fn a_stale_etag_is_a_conflict_and_the_hand_edit_survives() {
        let f = fixture(FIXED_TOML);
        let stale = etag(&f.store);
        let edited = format!("{FIXED_TOML}\n# edited by hand\n");
        std::fs::write(f.store.config_path(), &edited).unwrap();

        let req = SettingsPatch {
            etag: stale,
            patch: [("listener.max_calls".to_owned(), json!(4))]
                .into_iter()
                .collect(),
            acknowledge: Vec::new(),
        };
        let err = f
            .store
            .save(&req, &WriteContext::default())
            .expect_err("a stale etag never writes");
        assert!(matches!(err, SaveError::Conflict), "{err:?}");
        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            edited.as_bytes()
        );

        // The page reloads, takes the fresh etag, and the same patch lands.
        let out = f
            .store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!(4))]),
                &WriteContext::default(),
            )
            .expect("the fresh etag saves");
        assert!(out.written);
        assert_eq!(out.config.listener.max_calls, 4);
        assert!(
            std::fs::read_to_string(f.store.config_path())
                .unwrap()
                .ends_with("\n# edited by hand\n"),
            "the hand edit rides along in the in-place rewrite"
        );
    }

    #[test]
    fn an_empty_patch_writes_nothing() {
        let f = fixture(FIXED_TOML);
        let before = etag(&f.store);
        // A directory squatting on the temp name would make any real write
        // fail — so success here proves no write was even attempted.
        std::fs::create_dir(f.dir.path().join("node.toml.tmp")).unwrap();

        let out = f
            .store
            .save(&patch(&f.store, &[]), &WriteContext::default())
            .expect("an empty patch is a no-op, not an error");
        assert!(!out.written);
        assert_eq!(out.etag, before);
        assert_eq!(out.changed, [] as [std::string::String; 0]);
        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            FIXED_TOML.as_bytes()
        );
        assert_eq!(etag(&f.store), before);
    }

    /// If anything fails, nothing is written. Both refusals — a missing
    /// acknowledgement and a failed field rule — come back as `Field` with
    /// `node.toml` byte-identical and the etag unchanged. The order inside
    /// `save` is what this pins: the acknowledgement check runs BEFORE the
    /// write, or an unacknowledged `dtmf.enabled = true` would be on disk
    /// with a 400 on the wire.
    #[test]
    fn a_field_error_writes_nothing() {
        let f = fixture(FIXED_TOML);

        let err = f
            .store
            .save(
                &patch(&f.store, &[("dtmf.enabled", json!(true))]),
                &WriteContext::default(),
            )
            .expect_err("dtmf.enabled needs its acknowledgement");
        let SaveError::Field(e) = err else {
            panic!("expected Field, got {err:?}");
        };
        assert_eq!(
            e,
            FieldError {
                field: "dtmf.enabled".to_owned(),
                message: "dtmf.enabled lets any connected member command links; it needs the dtmf.enabled acknowledgement".to_owned(),
                acknowledge: Some("dtmf.enabled".to_owned()),
            }
        );

        let err = f
            .store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!("8"))]),
                &WriteContext::default(),
            )
            .expect_err("a string is not a non-negative integer");
        let SaveError::Field(e) = err else {
            panic!("expected Field, got {err:?}");
        };
        assert_eq!(
            e,
            FieldError {
                field: "listener.max_calls".to_owned(),
                message: "listener.max_calls: expected a non-negative integer".to_owned(),
                acknowledge: None,
            }
        );

        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            FIXED_TOML.as_bytes()
        );
        assert_eq!(etag(&f.store), etag_of(FIXED_TOML.as_bytes()));
        assert!(
            !f.dir.path().join("node.toml.tmp").exists(),
            "no temp file left behind"
        );
    }

    #[test]
    fn a_restart_path_lands_in_pending_restart() {
        let f = fixture(FIXED_TOML);
        let out = f
            .store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!(4))]),
                &WriteContext::default(),
            )
            .expect("save");
        assert!(out.written);
        assert_eq!(out.changed, vec!["listener.max_calls".to_owned()]);
        assert_eq!(out.config.listener.max_calls, 4);
        assert_ne!(out.etag, etag_of(FIXED_TOML.as_bytes()));

        let state = f.store.state(&SecretProvider::new(), &facts()).unwrap();
        assert_eq!(
            state["pending"],
            json!({ "restart": ["listener.max_calls"], "apply": [] })
        );
        assert_eq!(state["etag"], json!(out.etag));
        assert!(
            std::fs::read_to_string(f.store.config_path())
                .unwrap()
                .starts_with("# a fixed node"),
            "the in-place edit keeps the comment"
        );
    }

    #[test]
    fn a_live_path_is_pending_apply_until_marked_applied() {
        let f = fixture(FIXED_TOML);
        let secrets = SecretProvider::new();
        let out = f
            .store
            .save(
                &patch(&f.store, &[("bridge.mode", json!("conference"))]),
                &WriteContext::default(),
            )
            .expect("save");
        assert_eq!(out.changed, vec!["bridge.mode".to_owned()]);

        let state = f.store.state(&secrets, &facts()).unwrap();
        assert_eq!(
            state["pending"],
            json!({ "restart": [], "apply": ["bridge.mode"] })
        );
        assert_eq!(state["running"]["bridge.mode"], json!("bridge"));

        f.store
            .mark_applied(&out.changed, &RedactedConfig::from_config(&out.config));

        let state = f.store.state(&secrets, &facts()).unwrap();
        assert_eq!(state["pending"], json!({ "restart": [], "apply": [] }));
        assert_eq!(state["running"]["bridge.mode"], json!("conference"));
    }

    #[test]
    fn mark_applied_forgets_a_path_the_disk_no_longer_has() {
        let f = fixture(FIXED_TOML);
        let out = f
            .store
            .save(
                &patch(&f.store, &[("bridge", Value::Null)]),
                &WriteContext::default(),
            )
            .expect("removing [bridge] is a live change");
        // Every key the removed table held, sorted: nothing more, nothing less.
        assert_eq!(
            out.changed,
            [
                "bridge.include_local_radio",
                "bridge.mix_minus",
                "bridge.mode"
            ]
        );

        f.store
            .mark_applied(&out.changed, &RedactedConfig::from_config(&out.config));

        let state = f.store.state(&SecretProvider::new(), &facts()).unwrap();
        assert_eq!(state["pending"], json!({ "restart": [], "apply": [] }));
        assert_eq!(state["running"].get("bridge.mode"), None);
    }

    /// The page's Apply button on a live-tier field that was hand-edited on
    /// disk. The patch carries the file's own value, so no byte changes —
    /// but the path is still pending, and the route keys the live tier off
    /// `changed`, so `changed` must name it.
    #[test]
    fn a_patch_equal_to_disk_but_pending_apply_counts_as_changed_without_a_write() {
        let f = fixture(FIXED_TOML);
        let hand_edited = FIXED_TOML.replace(
            "mode                = \"bridge\"",
            "mode                = \"parrot\"",
        );
        assert_ne!(hand_edited, FIXED_TOML, "the fixture edit must take");
        std::fs::write(f.store.config_path(), &hand_edited).unwrap();
        let before = etag(&f.store);
        assert_eq!(
            f.store.state(&SecretProvider::new(), &facts()).unwrap()["pending"],
            json!({ "restart": [], "apply": ["bridge.mode"] })
        );

        let out = f
            .store
            .save(
                &patch(&f.store, &[("bridge.mode", json!("parrot"))]),
                &WriteContext::default(),
            )
            .expect("save");
        assert_eq!(out.changed, vec!["bridge.mode".to_owned()]);
        assert!(!out.written, "no byte differed, so nothing was rewritten");
        assert_eq!(out.etag, before);
        assert_eq!(
            std::fs::read_to_string(f.store.config_path()).unwrap(),
            hand_edited
        );

        // A patch on a path that is NOT pending and equals the disk is still
        // nothing: the file's listener.max_calls is 2 already.
        let out = f
            .store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!(2))]),
                &WriteContext::default(),
            )
            .expect("save");
        assert_eq!(out.changed, [] as [std::string::String; 0]);
        assert!(!out.written);
    }

    // -- atomic write ---------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn an_unwritable_directory_leaves_the_old_file() {
        // Same idiom as tests/config_bootstrap.rs:135-159, and the same
        // assumption: not running as root, who ignores 0o500.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let config = locked.join("node.toml");
        std::fs::write(&config, FIXED_TOML).unwrap();
        let cfg = NodeFileConfig::from_toml_str_redacted(FIXED_TOML).unwrap();
        let store = SettingsStore::new(
            config.clone(),
            dir.path().join("credentials.toml"),
            RedactedConfig::from_config(&cfg),
        );
        let req = patch(&store, &[("listener.max_calls", json!(4))]);

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let writable =
            store.state(&SecretProvider::new(), &facts()).unwrap()["file"]["writable"].clone();
        let result = store.save(&req, &WriteContext::default());
        // Restore before asserting, so tempdir cleanup works even on failure.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            writable,
            json!(false),
            "the badge, not the save, is what reports this"
        );
        let err = result.expect_err("no write into a read-only directory");
        let SaveError::Io(msg) = err else {
            panic!("expected Io, got {err:?}");
        };
        assert!(
            msg.starts_with(&format!("cannot write {}: ", config.display())),
            "{msg}"
        );
        assert_eq!(std::fs::read(&config).unwrap(), FIXED_TOML.as_bytes());
        assert!(
            !locked.join("node.toml.tmp").exists(),
            "no temp file left behind"
        );
    }

    #[test]
    fn the_write_goes_through_a_sibling_tmp_file() {
        // A directory squatting on the temp name makes the temp open fail,
        // while a direct write to node.toml would still succeed — so this
        // passes only if the write really goes through `node.toml.tmp`.
        let f = fixture(FIXED_TOML);
        std::fs::create_dir(f.dir.path().join("node.toml.tmp")).unwrap();
        let err = f
            .store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!(4))]),
                &WriteContext::default(),
            )
            .expect_err("the temp cannot be opened");
        assert!(matches!(err, SaveError::Io(_)), "{err:?}");
        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            FIXED_TOML.as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_rewritten_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        // 0o604, not 0o640: a fresh file is 0o666 minus the umask, and no
        // common umask (022, 027, 077, 002) yields 0o604 — so a temp file
        // that skipped the mode copy can never pass this by accident.
        let f = fixture(FIXED_TOML);
        std::fs::set_permissions(
            f.store.config_path(),
            std::fs::Permissions::from_mode(0o604),
        )
        .unwrap();
        f.store
            .save(
                &patch(&f.store, &[("listener.max_calls", json!(4))]),
                &WriteContext::default(),
            )
            .expect("save");
        let mode = std::fs::metadata(f.store.config_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o604, "got {mode:o}");
    }

    // -- credential keys ---------------------------------------------------------------

    #[test]
    fn credential_key_maps_node_to_the_node_number_when_registered() {
        let f = fixture(FIXED_TOML);
        let key = |n: &CredentialName| f.store.credential_key(n);
        assert_eq!(key(&CredentialName::Node).unwrap(), "12345");
        assert_eq!(key(&CredentialName::Portal).unwrap(), "portal");
        assert_eq!(key(&CredentialName::Discord).unwrap(), "discord");
        assert_eq!(
            key(&CredentialName::Link("2000".to_owned())).unwrap(),
            "link:2000"
        );
        assert_eq!(
            key(&CredentialName::Peer("allstar-public".to_owned())).unwrap(),
            "allstar-public"
        );
        // Two file keys for one provider key would let the last one loaded
        // win silently; the node's own number is refused as a peer, and
        // refused is a 400, not a 500.
        assert_eq!(
            key(&CredentialName::Peer("12345".to_owned())).unwrap_err(),
            CredentialKeyError::Rejected(
                "credential \"peer:12345\" is this node's own number — set it as \"node\""
                    .to_owned()
            )
        );
        assert_eq!(
            key(&CredentialName::Peer("12345".to_owned()))
                .unwrap_err()
                .to_string(),
            "credential \"peer:12345\" is this node's own number — set it as \"node\"",
            "Display is the bare sentence the route puts in `error`"
        );
    }

    #[test]
    fn credential_key_maps_node_to_the_literal_without_register() {
        let f = fixture(NODE_TOML_TEMPLATE);
        assert_eq!(
            f.store.credential_key(&CredentialName::Node).unwrap(),
            "node"
        );
        assert_eq!(
            f.store
                .credential_key(&CredentialName::Peer("12345".to_owned()))
                .unwrap(),
            "12345"
        );
    }

    #[test]
    fn credential_key_reads_the_disk_not_a_cache() {
        let f = fixture(NODE_TOML_TEMPLATE);
        assert_eq!(
            f.store.credential_key(&CredentialName::Node).unwrap(),
            "node"
        );
        f.store
            .save(
                &patch(
                    &f.store,
                    &[
                        ("register.peer", json!("52.20.63.146:4569")),
                        ("register.node_id", json!("777")),
                    ],
                ),
                &WriteContext::default(),
            )
            .expect("[register] is created by the patch");
        assert_eq!(
            f.store.credential_key(&CredentialName::Node).unwrap(),
            "777"
        );
    }

    #[test]
    fn credential_key_without_node_toml_is_unreadable_not_rejected() {
        let f = fixture(FIXED_TOML);
        std::fs::remove_file(f.store.config_path()).unwrap();
        let err = f
            .store
            .credential_key(&CredentialName::Portal)
            .expect_err("no file to read the source from");
        let CredentialKeyError::Unreadable(msg) = err else {
            panic!("a missing file is the 500 path, got {err:?}");
        };
        assert_eq!(
            msg,
            format!(
                "cannot read {}: {}",
                f.store.config_path().display(),
                std::fs::read(f.store.config_path()).unwrap_err()
            )
        );
    }

    // -- credentials.toml -------------------------------------------------------------

    fn sorted_file_keys(path: &Path) -> Vec<String> {
        let mut keys = SecretProvider::file_keys(path).expect("file keys");
        keys.sort();
        keys
    }

    #[test]
    fn save_credential_on_a_file_source_writes_the_file_and_nothing_else() {
        let f = fixture(FIXED_TOML);
        let secrets = SecretProvider::new();
        let out = f
            .store
            .save_credential(&secrets, &CredentialName::Node, "node-pw-SENTINEL", false)
            .expect("save");
        assert_eq!(
            out,
            CredentialOutcome {
                provider_key: "12345".to_owned(),
                file_key: "node".to_owned(),
                persisted: true,
            }
        );
        assert_eq!(sorted_file_keys(f.store.credentials_path()), ["node"]);
        let fresh = SecretProvider::new();
        assert_eq!(
            fresh
                .load_file(f.store.credentials_path(), Some("12345"))
                .unwrap(),
            1
        );
        assert_eq!(fresh.resolve("12345"), "node-pw-SENTINEL");
        // The store never calls the provider: that is the route's job, after the file.
        assert_eq!(secrets.resolve("12345"), "");
        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            FIXED_TOML.as_bytes()
        );
    }

    #[test]
    fn save_credential_on_an_env_source_without_migrate_touches_no_file() {
        let toml = env_toml();
        let f = fixture(&toml);
        let out = f
            .store
            .save_credential(
                &SecretProvider::new(),
                &CredentialName::Discord,
                "bot-value",
                false,
            )
            .expect("save");
        assert_eq!(
            out,
            CredentialOutcome {
                provider_key: "discord".to_owned(),
                file_key: "discord".to_owned(),
                persisted: false,
            }
        );
        assert!(!f.store.credentials_path().exists());
        assert_eq!(
            std::fs::read(f.store.config_path()).unwrap(),
            toml.as_bytes()
        );
    }

    /// Every HTTP request runs on its own worker thread, so two Set clicks
    /// (or a Set racing a Forget) reach `save_credential` concurrently;
    /// without the write lock the two read-modify-write cycles on
    /// `credentials.toml` can lose one key.
    #[test]
    fn concurrent_credential_saves_do_not_lose_a_key() {
        let f = fixture(FIXED_TOML);
        std::thread::scope(|s| {
            for n in ["2000", "2001"] {
                let store = &f.store;
                s.spawn(move || {
                    let secrets = SecretProvider::new();
                    store
                        .save_credential(&secrets, &CredentialName::Link(n.to_owned()), "pw", false)
                        .expect("save");
                });
            }
        });
        assert_eq!(
            sorted_file_keys(f.store.credentials_path()),
            ["link:2000", "link:2001"]
        );
    }

    #[test]
    fn forget_credential_removes_the_file_key_and_names_the_provider_key() {
        let f = fixture(FIXED_TOML);
        let secrets = SecretProvider::new();
        f.store
            .save_credential(
                &secrets,
                &CredentialName::Link("2000".to_owned()),
                "dial-pw",
                false,
            )
            .unwrap();
        f.store
            .save_credential(&secrets, &CredentialName::Node, "node-pw", false)
            .unwrap();
        let out = f
            .store
            .forget_credential(&CredentialName::Link("2000".to_owned()))
            .expect("forget");
        assert_eq!(
            out,
            CredentialOutcome {
                provider_key: "link:2000".to_owned(),
                file_key: "link:2000".to_owned(),
                persisted: true,
            }
        );
        assert_eq!(sorted_file_keys(f.store.credentials_path()), ["node"]);
    }

    #[test]
    fn forget_credential_without_a_file_is_fine() {
        let f = fixture(FIXED_TOML);
        let out = f
            .store
            .forget_credential(&CredentialName::Portal)
            .expect("nothing to remove is not an error");
        assert_eq!(
            out,
            CredentialOutcome {
                provider_key: "portal".to_owned(),
                file_key: "portal".to_owned(),
                persisted: false,
            }
        );
        assert!(!f.store.credentials_path().exists());
    }

    #[test]
    fn forget_credential_error_names_the_slot_with_the_removed_sentence() {
        // The forget direction has its own prefix, so a failed forget never
        // reads as a failed save. node.toml is gone, so the read at the top
        // of the call fails.
        let f = fixture(FIXED_TOML);
        std::fs::remove_file(f.store.config_path()).unwrap();
        let err = f
            .store
            .forget_credential(&CredentialName::Discord)
            .expect_err("no node.toml to read the source from");
        assert_eq!(
            err,
            format!(
                "credential \"discord\" could not be removed: cannot read {}: {}",
                f.store.config_path().display(),
                std::fs::read(f.store.config_path()).unwrap_err()
            )
        );
    }

    #[test]
    fn migration_imports_every_running_credential_then_flips_the_source() {
        let toml = config_source_toml();
        let f = fixture(&toml);
        let secrets = SecretProvider::new();
        // What main.rs:121-127 does under source = "config" …
        secrets.load_config_secret("12345", "inline-pw-SENTINEL");
        // … and what load_env would have done for ALLSTAR_LINK_2000 and
        // DISCORD_BOT_TOKEN on an env-sourced node.
        secrets.put("link:2000", "env-link-pw");
        secrets.put("discord", "env-bot-value");

        let out = f
            .store
            .save_credential(&secrets, &CredentialName::Portal, "portal-pw", true)
            .expect("migrate");
        assert_eq!(
            out,
            CredentialOutcome {
                provider_key: "portal".to_owned(),
                file_key: "portal".to_owned(),
                persisted: true,
            }
        );
        assert_eq!(
            sorted_file_keys(f.store.credentials_path()),
            ["discord", "link:2000", "node", "portal"]
        );
        let fresh = SecretProvider::new();
        assert_eq!(
            fresh
                .load_file(f.store.credentials_path(), Some("12345"))
                .unwrap(),
            4
        );
        assert_eq!(fresh.resolve("12345"), "inline-pw-SENTINEL");
        assert_eq!(fresh.link_secret("2000"), "env-link-pw");
        assert_eq!(fresh.resolve("discord"), "env-bot-value");
        assert_eq!(fresh.resolve("portal"), "portal-pw");

        let after = std::fs::read_to_string(f.store.config_path()).unwrap();
        let cfg = NodeFileConfig::from_toml_str_redacted(&after).unwrap();
        assert_eq!(cfg.secrets.source, "file");
        assert_eq!(cfg.secrets.secret.as_deref(), Some(""));
        assert!(
            !after.contains("SENTINEL"),
            "the inline value is gone from node.toml: {after}"
        );
        // Exactly two values changed and nothing else moved: the header
        // comment, the comment sitting DIRECTLY above the `source` key and
        // every other byte are where they were. toml_edit's `Table::insert`
        // on an existing key reformats the key and drops the comment above
        // it; only an in-place value replacement keeps the file this close.
        assert_eq!(
            after,
            toml.replace(
                "source = \"config\"\nsecret = \"inline-pw-SENTINEL\"",
                "source = \"file\"\nsecret = \"\""
            ),
            "the comment above `source` survives, still above it"
        );

        // Honest afterwards: the process still reads `config` until it restarts.
        let state = f.store.state(&secrets, &facts()).unwrap();
        assert_eq!(state["credential_source"], json!("file"));
        assert_eq!(
            state["pending"],
            json!({ "restart": ["credentials.source"], "apply": [] })
        );
    }

    #[test]
    fn migration_keeps_an_inline_secret_the_provider_never_loaded() {
        // The template flow (template.rs:31-33, 62-69): `secret` is filled
        // in BEFORE [register] is uncommented. main.rs:124-126 loads the
        // inline value only under `if let Some(reg)`, so on such a node the
        // running provider holds nothing for it, export_file writes nothing
        // for it, and a migration that only exported the provider would
        // blank the one copy there is.
        let toml = config_source_toml().replace(
            "[register]\npeer    = \"52.20.63.146:4569\"\nnode_id = \"12345\"\n\n",
            "",
        );
        assert!(
            !toml.contains("[register]"),
            "the fixture edit must take: {toml}"
        );
        let f = fixture(&toml);

        let out = f
            .store
            .save_credential(
                &SecretProvider::new(),
                &CredentialName::Portal,
                "portal-pw",
                true,
            )
            .expect("migrate");
        assert!(out.persisted);
        assert_eq!(
            sorted_file_keys(f.store.credentials_path()),
            ["node", "portal"]
        );
        let fresh = SecretProvider::new();
        assert_eq!(
            fresh.load_file(f.store.credentials_path(), None).unwrap(),
            2
        );
        assert_eq!(fresh.resolve("node"), "inline-pw-SENTINEL");
        assert_eq!(fresh.resolve("portal"), "portal-pw");

        let after = std::fs::read_to_string(f.store.config_path()).unwrap();
        assert!(
            !after.contains("SENTINEL"),
            "the inline value is gone from node.toml: {after}"
        );
        let cfg = NodeFileConfig::from_toml_str_redacted(&after).unwrap();
        assert_eq!(cfg.secrets.source, "file");
        assert_eq!(cfg.secrets.secret.as_deref(), Some(""));
    }

    fn credentials_file_refusal(f: &Fixture, file: &str) -> Option<String> {
        match f.store.save(
            &patch(&f.store, &[("credentials.file", json!(file))]),
            &WriteContext::default(),
        ) {
            Ok(_) => None,
            Err(SaveError::Field(e)) => {
                assert_eq!(e.field, "credentials.file");
                Some(e.message)
            }
            Err(other) => panic!("{file}: expected a field error, got {other:?}"),
        }
    }

    /// `credentials.file` names the file every credential save rewrites at
    /// 0600, so the page may only point it beside the config or in the state
    /// directory, and never — by any spelling — at the config itself.
    #[cfg(unix)]
    #[test]
    fn credentials_file_stays_beside_the_config_or_in_the_state_dir() {
        let f = fixture(FIXED_TOML);
        let dir = f.dir.path().canonicalize().unwrap();
        std::fs::create_dir(dir.join("state")).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(dir.join("node.toml"), dir.join("alias.toml")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("other.toml");
        std::fs::write(&outside_file, "x = 1\n").unwrap();
        std::os::unix::fs::symlink(&outside_file, dir.join("away.toml")).unwrap();

        let in_dirs = |p: &str| {
            format!(
                "credentials.file: {p:?} must be a file directly in {} or {}",
                dir.display(),
                dir.join("state").display()
            )
        };
        let own = |p: &str| {
            format!("credentials.file: {p:?} is the node's own config; credentials never go in it")
        };

        for good in [
            "credentials.toml".to_string(),
            dir.join("keys.toml").display().to_string(),
            dir.join("state/keys.toml").display().to_string(),
        ] {
            assert_eq!(credentials_file_refusal(&f, &good), None, "{good}");
        }
        let abs_out = outside_file.display().to_string();
        assert_eq!(
            credentials_file_refusal(&f, &abs_out),
            Some(in_dirs(&abs_out))
        );
        assert_eq!(
            credentials_file_refusal(&f, "sub/keys.toml"),
            Some(in_dirs("sub/keys.toml")),
            "a subdirectory is not the config's directory"
        );
        assert_eq!(
            credentials_file_refusal(&f, "away.toml"),
            Some(in_dirs("away.toml")),
            "a symlink out of the directory is judged by its target"
        );
        assert_eq!(
            credentials_file_refusal(&f, "alias.toml"),
            Some(own("alias.toml"))
        );
        let via_dot = dir.join(".").join("node.toml").display().to_string();
        assert_eq!(credentials_file_refusal(&f, &via_dot), Some(own(&via_dot)));
        assert_eq!(
            credentials_file_refusal(&f, "nowhere/keys.toml"),
            Some(
                "credentials.file: \"nowhere/keys.toml\": its directory does not exist".to_string()
            )
        );
        let last_good = format!(
            "file = {:?}",
            dir.join("state/keys.toml").display().to_string()
        );
        assert!(
            std::fs::read_to_string(dir.join("node.toml"))
                .unwrap()
                .contains(&last_good),
            "nothing refused was written over the last accepted value"
        );
    }

    /// The store compares the two paths as files, not as spellings: a
    /// credentials path that reaches the config through `..` or a symlink
    /// is the config.
    #[cfg(unix)]
    #[test]
    fn a_credentials_path_that_aliases_the_config_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("node.toml");
        std::fs::write(&config, FIXED_TOML).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(&config, dir.path().join("link.toml")).unwrap();
        let cfg = NodeFileConfig::from_toml_str_redacted(FIXED_TOML).unwrap();
        for alias in [
            dir.path().join("sub/../node.toml"),
            dir.path().join("link.toml"),
        ] {
            let store = SettingsStore::new(
                config.clone(),
                alias.clone(),
                RedactedConfig::from_config(&cfg),
            );
            let err = store
                .save_credential(
                    &SecretProvider::new(),
                    &CredentialName::Node,
                    "pw-SENTINEL",
                    false,
                )
                .expect_err("the config is never the credentials file");
            assert!(
                err.contains("is the node's own config"),
                "{}: {err}",
                alias.display()
            );
            let err = store
                .forget_credential(&CredentialName::Portal)
                .expect_err("nor on forget");
            assert!(
                err.contains("is the node's own config"),
                "{}: {err}",
                alias.display()
            );
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), FIXED_TOML);
    }

    /// The store's half of the `credentials.file` rule. `validate_field`
    /// refuses a `credentials.file` whose file name is `node.toml`, but a config
    /// started as `--config <dir>/credentials.toml` (or `site.toml` with
    /// `file = "site.toml"`) resolves the credentials path onto the config
    /// itself, and `save_file` would write the password into it. The store
    /// refuses by path, before any read-modify-write of either file.
    #[test]
    fn a_credentials_path_equal_to_the_config_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("credentials.toml");
        let toml = FIXED_TOML.replace(
            "[secrets]\nsource = \"file\"\n",
            "[secrets]\nsource = \"file\"\nfile = \"credentials.toml\"\n",
        );
        assert!(
            toml.contains("file = \"credentials.toml\""),
            "the fixture edit must take: {toml}"
        );
        std::fs::write(&config, &toml).expect("write the fixture");
        let cfg = NodeFileConfig::from_toml_str_redacted(&toml).expect("the fixture parses");
        let store = SettingsStore::new(
            config.clone(),
            cfg.credentials_path(&config),
            RedactedConfig::from_config(&cfg),
        );
        assert_eq!(
            store.credentials_path(),
            config.as_path(),
            "the fixture must alias the two paths"
        );

        let err = store
            .save_credential(
                &SecretProvider::new(),
                &CredentialName::Node,
                "node-pw-SENTINEL",
                false,
            )
            .expect_err("a credential never goes into the config");
        assert_eq!(
            err,
            format!(
                "credential \"node\" could not be saved: {} is the node's own config; credentials never go in it",
                config.display()
            )
        );
        // Not a byte moved: no key appended, no mode change to 0600.
        assert_eq!(std::fs::read_to_string(&config).unwrap(), toml);
    }

    /// The forget half of the same alias. `remove_from_file` parses the
    /// credentials path as a document, removes one top-level key and writes
    /// it back at 0600 with the credentials header — on the config itself
    /// that deletes `[portal]`, `[discord]`, or through a `peer:<table>`
    /// name `[listener]`, and the node no longer boots. Refused by path
    /// before any read-modify-write, like the save.
    #[cfg(unix)]
    #[test]
    fn a_credentials_path_equal_to_the_config_is_refused_on_forget() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("credentials.toml");
        let toml = FIXED_TOML.replace(
            "[secrets]\nsource = \"file\"\n",
            "[secrets]\nsource = \"file\"\nfile = \"credentials.toml\"\n",
        );
        assert!(
            toml.contains("file = \"credentials.toml\""),
            "the fixture edit must take: {toml}"
        );
        std::fs::write(&config, &toml).expect("write the fixture");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644))
            .expect("chmod the fixture");
        let cfg = NodeFileConfig::from_toml_str_redacted(&toml).expect("the fixture parses");
        let store = SettingsStore::new(
            config.clone(),
            cfg.credentials_path(&config),
            RedactedConfig::from_config(&cfg),
        );
        assert_eq!(
            store.credentials_path(),
            config.as_path(),
            "the fixture must alias the two paths"
        );

        for (name, slot) in [
            (CredentialName::Portal, "portal"),
            (CredentialName::Discord, "discord"),
            (CredentialName::Peer("listener".to_owned()), "peer:listener"),
        ] {
            let err = store
                .forget_credential(&name)
                .expect_err("a forget never rewrites the config");
            assert_eq!(
                err,
                format!(
                    "credential \"{slot}\" could not be removed: {} is the node's own config; credentials never go in it",
                    config.display()
                )
            );
            // Not a byte moved: every table still there, the mode unchanged.
            assert_eq!(std::fs::read_to_string(&config).unwrap(), toml);
            assert_eq!(
                std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o644,
                "the config's mode must not become 0600"
            );
        }
    }

    /// `#[cfg(unix)]` because the failure it provokes is ENOTDIR: on unix,
    /// opening a path under a regular file is `NotADirectory`, which
    /// `save_file`'s first step (`read_document`) reports as `cannot read`
    /// — it treats only `NotFound` as an absent file. Windows answers the
    /// same open with `NotFound`, so there the call would reach the temp
    /// create and fail with a different sentence.
    #[cfg(unix)]
    #[test]
    fn a_credential_error_names_the_slot_and_never_the_value() {
        // credentials.toml's parent is a regular file, so save_file cannot
        // even read it.
        let f = fixture(FIXED_TOML);
        let blocker = f.dir.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("credentials.toml");
        let store = SettingsStore::new(
            f.store.config_path().to_path_buf(),
            path.clone(),
            RedactedConfig::default(),
        );
        let err = store
            .save_credential(
                &SecretProvider::new(),
                &CredentialName::Link("2000".to_owned()),
                "SENTINEL-value",
                false,
            )
            .expect_err("cannot read under a file");
        // The leak check first, so a leak is reported as a leak rather than
        // as a sentence mismatch.
        assert!(
            !err.contains("SENTINEL"),
            "the value leaked into the error: {err}"
        );
        assert_eq!(
            err,
            format!(
                "credential \"link:2000\" could not be saved: cannot read {}: {}",
                path.display(),
                std::fs::read(&path).unwrap_err()
            )
        );
    }

    /// The route's door keeps the write lock through the live apply: no
    /// other save can write between this one's write and its apply.
    #[test]
    fn save_and_apply_holds_the_write_lock_through_the_apply() {
        let f = fixture(FIXED_TOML);
        let (out, held) = f
            .store
            .save_and_apply(
                &patch(&f.store, &[("bridge.mode", json!("conference"))]),
                &WriteContext::default(),
                |_| {
                    let held = matches!(
                        f.store.write_lock.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    );
                    (held, Vec::<String>::new())
                },
                |r| &r.1,
            )
            .expect("save");
        assert!(out.written, "the fixture save writes");
        assert!(held.0, "the live apply ran without the write lock");
    }

    /// Two saves in flight reach the station in the order they reached the
    /// disk. Save A writes `bridge.mode`; save B, made on A's new etag,
    /// writes `bridge.mix_minus` while A is between its write and its
    /// apply. If B could write, apply and mark in that gap, A's apply would
    /// then put A's older `mix_minus` back on the station while the mirror
    /// said B's and `pending.apply` was empty.
    #[test]
    fn a_save_made_on_the_new_etag_applies_after_the_save_that_made_it() {
        let f = fixture(FIXED_TOML);
        // The station: every `bridge.mix_minus` the live tier pushed, in order.
        let station: Mutex<Vec<Option<Value>>> = Mutex::new(Vec::new());
        let live = |out: &SaveOutcome| {
            let from = RedactedConfig::from_config(&out.config);
            station
                .lock()
                .unwrap()
                .push(from.get("bridge.mix_minus").cloned());
            out.changed.clone()
        };
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            f.store
                .save_and_apply(
                    &patch(&f.store, &[("bridge.mode", json!("conference"))]),
                    &WriteContext::default(),
                    |out_a| {
                        // A has written. B loads what is on disk now, so it
                        // carries A's etag — read straight from the file:
                        // `state()` wants the lock this thread holds.
                        let text = std::fs::read(f.store.config_path()).expect("read");
                        let b_req = SettingsPatch {
                            etag: etag_of(&text),
                            patch: [("bridge.mix_minus".to_owned(), json!(false))]
                                .into_iter()
                                .collect(),
                            acknowledge: Vec::new(),
                        };
                        let (store, live) = (&f.store, &live);
                        s.spawn(move || {
                            store
                                .save_and_apply(&b_req, &WriteContext::default(), live, |p| p)
                                .expect("save B is on the current etag");
                            let _ = tx.send(());
                        });
                        // Give B every chance to finish first, which it can
                        // only do while this apply runs outside the lock.
                        let _ = rx.recv_timeout(std::time::Duration::from_millis(300));
                        live(out_a)
                    },
                    |p| p,
                )
                .expect("save A");
        });

        assert_eq!(
            *station.lock().unwrap(),
            [Some(json!(true)), Some(json!(false))],
            "A's config reached the station after B's"
        );
        let state = f.store.state(&SecretProvider::new(), &facts()).unwrap();
        assert_eq!(state["disk"]["bridge.mix_minus"], json!(false));
        assert_eq!(state["running"]["bridge.mix_minus"], json!(false));
        assert_eq!(state["running"]["bridge.mode"], json!("conference"));
        assert_eq!(state["pending"], json!({ "restart": [], "apply": [] }));
    }

    #[test]
    fn a_credential_save_waits_for_the_write_lock() {
        // The test holds the lock, so the only way credentials.toml can
        // exist before the guard is dropped is a save that never took it.
        let f = fixture(FIXED_TOML);
        let guard = f.store.write_lock.lock().unwrap();
        std::thread::scope(|s| {
            let store = &f.store;
            let saver = s.spawn(move || {
                store.save_credential(
                    &SecretProvider::new(),
                    &CredentialName::Link("2000".to_owned()),
                    "pw",
                    false,
                )
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            let written_early = store.credentials_path().exists();
            drop(guard);
            saver.join().expect("saver thread").expect("save");
            assert!(
                !written_early,
                "save_credential wrote while the write lock was held"
            );
        });
        assert_eq!(sorted_file_keys(f.store.credentials_path()), ["link:2000"]);
    }

    #[cfg(unix)]
    #[test]
    fn migration_exports_before_it_touches_node_toml() {
        // node.toml in a directory that cannot take the rename, credentials
        // elsewhere: the config write fails, and the order shows.
        use std::os::unix::fs::PermissionsExt;
        let toml = config_source_toml();
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let config = locked.join("node.toml");
        std::fs::write(&config, &toml).unwrap();
        let cfg = NodeFileConfig::from_toml_str_redacted(&toml).unwrap();
        let credentials = dir.path().join("credentials.toml");
        let store = SettingsStore::new(
            config.clone(),
            credentials.clone(),
            RedactedConfig::from_config(&cfg),
        );
        let secrets = SecretProvider::new();
        secrets.load_config_secret("12345", "inline-pw-SENTINEL");
        secrets.put("discord", "env-bot-value");

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.save_credential(&secrets, &CredentialName::Portal, "portal-pw", true);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        let err = result.expect_err("node.toml is not writable");
        assert!(
            err.starts_with("credential \"portal\" could not be saved: cannot write "),
            "{err}"
        );
        assert!(
            !err.contains("portal-pw") && !err.contains("SENTINEL"),
            "{err}"
        );
        // Step 1 happened: what the process runs on is already on disk …
        assert_eq!(sorted_file_keys(&credentials), ["discord", "node"]);
        // … step 2 did not: node.toml still says config, byte for byte …
        assert_eq!(std::fs::read_to_string(&config).unwrap(), toml);
        // … and step 3 did not: the new value was written nowhere.
        assert!(!sorted_file_keys(&credentials).contains(&"portal".to_owned()));
    }
}
