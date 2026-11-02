// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pluggable secret store for IAX2 credentials.
//!
//! `SecretProvider` is the single source of truth for username→secret mappings.
//! Credentials can be pushed at runtime (via a control-channel `ProvideSecret`
//! command), loaded at start-up from well-known environment variables, or
//! loaded from the credentials file beside `node.toml` (`[secrets] source =
//! "file"`), which the settings page writes.
//!
//! The provider intentionally does **not** implement `Debug` (or derive it) so
//! that secrets can never be leaked through `{:?}` formatting or log macros.
//!
//! ## Namespaces
//!
//! Secrets are organized in flat key namespaces:
//! - **Inbound peer credentials:** bare username (e.g., `"55553"`), fed by
//!   `ALLSTAR_PEER_<NODE>` env vars, a bare key in the credentials file, or
//!   `POST /secrets`.
//! - **Outbound link credentials (iax-5029):** `"link:<node>"` namespace, fed by
//!   `ALLSTAR_LINK_<NODE>` env vars, `"link:<node>"` in the credentials file, or
//!   `POST /secrets` with username `"link:<node>"`.
//! - **Discord bot token:** the `"discord"` username, fed by `DISCORD_BOT_TOKEN`,
//!   `discord` in the credentials file, or `POST /secrets` with username
//!   `"discord"`. Never written to `node.toml`.
//! - **Porkbun API keys:** `porkbun_pk` and `porkbun_sk` (the `pk1_…` and
//!   `sk1_…` keys), fed by `PORKBUN_API_KEY` / `PORKBUN_SECRET_API_KEY`, the
//!   same keys in the credentials file or `[secrets]` inline, or
//!   `POST /secrets`. Only the public page's DNS automation reads them.
//! - **DNS provider tokens:** `dns:<provider>` (`dns:cloudflare`, ...; see
//!   `config::DNS_UPDATE_PROVIDERS`), fed by `ASTAR_DNS_<PROVIDER>_TOKEN`, the
//!   same key in the credentials file or `POST /secrets`. Only the public
//!   page's DNS automation reads them; never inbound.
//! - **`EchoLink` password:** `echolink:<CALL>` (`echolink:KX7ABC-L`), fed by
//!   `ASTAR_ECHOLINK_PASSWORD` (under any source), the same key in the
//!   credentials file, or `POST /secrets`. Never inbound, and the settings
//!   page has no slot for it.
//! - **Portal password:** the `"portal"` username, fed by `portal` in the
//!   credentials file, or — under the other sources — seeded at startup
//!   (`NodeFileConfig::load_credentials`) from the env var `[portal] credential_env` names, so the store sees it
//!   like every other credential.
//!
//! Only the first namespace authenticates inbound calls: the inbound
//! resolver ([`SecretProvider::resolver_arc`]) answers nothing for `portal`,
//! `discord`, `node` or any `link:` key, so a caller cannot guess them
//! through the MD5 challenge.
//!
//! ## The credentials file
//!
//! Flat TOML, one string per key. `node` is the registration password and is
//! mapped onto `register.node_id` at load — a NAME, not the node number, so
//! renumbering the node cannot orphan its password. Every other key is
//! stored under its own name. The file is created mode 0600 before a byte is
//! written and is refused at load when group or others can read it; the
//! node boots either way, because a chmod must not crash-loop a container.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use toml_edit::DocumentMut;

/// Default name of the credentials file, beside `node.toml`. Not
/// `secrets.toml`: the settings page reports this path on the wire, and the
/// control port's guard test refuses any key or value containing "secret".
pub const CREDENTIALS_FILE_NAME: &str = "credentials.toml";
/// File key of the registration password. A name rather than the node
/// number, so renumbering the node from the settings page cannot orphan the
/// password; [`SecretProvider::load_file`] maps it onto `register.node_id`.
pub const NODE_CREDENTIAL_KEY: &str = "node";
/// File key and provider key of the allstarlink.org portal password.
pub const PORTAL_CREDENTIAL_KEY: &str = "portal";
/// File key and provider key of the Discord bot token.
pub const DISCORD_CREDENTIAL_KEY: &str = "discord";
/// File key and provider key of the Porkbun API key (`pk1_…`), for the
/// public page's dynamic DNS and ACME DNS-01. Named after Porkbun's own key
/// prefix, and never spelled with the word the settings routes refuse.
pub const PORKBUN_PK_KEY: &str = "porkbun_pk";
/// File key and provider key of the Porkbun secret API key (`sk1_…`).
pub const PORKBUN_SK_KEY: &str = "porkbun_sk";
/// The environment variables the `env` source reads them from.
pub const PORKBUN_PK_ENV: &str = "PORKBUN_API_KEY";
pub const PORKBUN_SK_ENV: &str = "PORKBUN_SECRET_API_KEY";

/// Prefix of an outbound link secret, `link:<node>`, in the file and in the
/// provider alike (iax-5029).
pub const LINK_PREFIX: &str = "link:";
/// Prefix of a dns-update provider's token in the provider and the file:
/// `dns:cloudflare`, ... (`config::DNS_UPDATE_PROVIDERS`). Never inbound.
pub const DNS_CREDENTIAL_PREFIX: &str = "dns:";
/// `echolink:<CALLSIGN>`, the callsign trimmed and uppercased.
fn echolink_key(callsign: &str) -> String {
    format!("{ECHOLINK_PREFIX}{}", callsign.trim().to_ascii_uppercase())
}

/// Prefix of the node's `EchoLink` password, `echolink:<CALL>` (`KX7ABC-L`),
/// in the provider and the file. Never inbound, and not a name the settings
/// page accepts.
pub const ECHOLINK_PREFIX: &str = "echolink:";
/// The variable [`SecretProvider::load_echolink_env`] reads the `EchoLink`
/// password from.
pub const ECHOLINK_PASSWORD_ENV: &str = "ASTAR_ECHOLINK_PASSWORD";

/// First line of every credentials file this crate writes. `toml_edit`
/// renders a fresh document's root prefix first, but on re-parse it hangs a
/// leading comment on the FIRST KEY's prefix, not the root table — so
/// removing that key would take the header with it — and a header with no
/// key after it at all parses into the document's *trailing* text, which
/// renders LAST. `write_document` covers both.
const CREDENTIALS_HEADER: &str =
    "# astar-server credentials — written by the settings page. Mode 0600.\n";

/// Shared, interior-mutable secret store the Station resolver consults.
#[derive(Clone, Default)]
pub struct SecretProvider {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl SecretProvider {
    /// Create an empty provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one credential (control-channel `ProvideSecret`).
    pub fn put(&self, username: impl Into<String>, secret: impl Into<String>) {
        self.inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .insert(username.into(), secret.into());
    }

    /// Resolve `username` → secret.  Returns `""` if the username is unknown.
    #[must_use]
    pub fn resolve(&self, username: &str) -> String {
        self.inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .get(username)
            .cloned()
            .unwrap_or_default()
    }

    /// Resolve the OUTBOUND dial secret for linking to `node` (iax-5029):
    /// the `link:<node>` namespace, fed by `ALLSTAR_LINK_<NODE>` env vars or
    /// `POST /secrets` with username `link:<node>`. Returns `""` on miss —
    /// today's empty-secret dial, so unconfigured targets are unchanged.
    #[must_use]
    pub fn link_secret(&self, node: &str) -> String {
        self.resolve(&format!("{LINK_PREFIX}{node}"))
    }

    /// Load `ALLSTAR_NODE`/`ALLSTAR_SECRET`, any `ALLSTAR_PEER_<NODE>`, any
    /// `ALLSTAR_LINK_<NODE>` and `DISCORD_BOT_TOKEN` from the current process
    /// environment into the store.
    ///
    /// Variable semantics:
    /// - `ALLSTAR_NODE` + `ALLSTAR_SECRET` → `put(node, secret)`
    /// - `ALLSTAR_PEER_<NODE>=<secret>` → `put(node, secret)` for each match
    /// - `ALLSTAR_LINK_<NODE>=<secret>` → `put("link:<NODE>", secret)` for each match (iax-5029)
    /// - `DISCORD_BOT_TOKEN=<token>` → `put("discord", token)`
    ///
    /// Nothing here echoes a value: the variable NAMES appear in diagnostics,
    /// never what they hold.
    pub fn load_env(&self) {
        // Primary node credential.
        if let (Ok(node), Ok(secret)) = (
            std::env::var("ALLSTAR_NODE"),
            std::env::var("ALLSTAR_SECRET"),
        ) {
            self.put(node, secret);
        }

        // Peer credentials: ALLSTAR_PEER_<NODE>=<secret>
        for (key, value) in std::env::vars() {
            if let Some(node) = key.strip_prefix("ALLSTAR_PEER_")
                && !node.is_empty()
            {
                self.put(node.to_owned(), value);
            }
        }

        // Outbound link credentials: ALLSTAR_LINK_<NODE>=<secret> — the
        // password WE present when dialing <NODE> (iax-5029). Stored under
        // the "link:" namespace so it can never collide with the inbound
        // ALLSTAR_PEER_<NODE> username keys.
        for (key, value) in std::env::vars() {
            if let Some(node) = key.strip_prefix("ALLSTAR_LINK_")
                && !node.is_empty()
            {
                self.put(format!("{LINK_PREFIX}{node}"), value);
            }
        }

        // Discord bot token: DISCORD_BOT_TOKEN → the "discord" username, which
        // is what `NodeController::handle_discord` resolves and what
        // `POST /secrets` expects. `node.toml` deliberately has no token key,
        // so a token reaches the daemon three ways: this variable (read here,
        // under the `env` source), that POST (under any source),
        // or the `discord` key of credentials.toml (under the `file` source,
        // read by `load_file`).
        if let Ok(token) = std::env::var("DISCORD_BOT_TOKEN")
            && !token.is_empty()
        {
            self.put("discord", token);
        }

        // Porkbun's two API keys, for `[web.dns] provider = "porkbun"`.
        for (var, key) in [
            (PORKBUN_PK_ENV, PORKBUN_PK_KEY),
            (PORKBUN_SK_ENV, PORKBUN_SK_KEY),
        ] {
            if let Ok(v) = std::env::var(var)
                && !v.is_empty()
            {
                self.put(key, v);
            }
        }

        // Every dns-update provider's token, for `[web.dns] provider = "<name>"`.
        for spec in crate::config::DNS_UPDATE_PROVIDERS {
            if let Ok(v) = std::env::var(spec.env)
                && !v.is_empty()
            {
                self.put(spec.credential, v);
            }
        }
    }

    /// Load the inline credential from `[secrets] source = "config"`
    /// (iax-4703): `node_id` is the username to register as (mirrors
    /// `register.node_id`, same as the env path), `secret` is the value read
    /// straight from `node.toml`. A no-op when `secret` is empty — the config
    /// loader has already warned in that case at parse time
    /// (`NodeFileConfig::from_toml_str`); this just avoids seeding the store
    /// with an empty credential.
    pub fn load_config_secret(&self, node_id: &str, secret: &str) {
        if !secret.is_empty() {
            self.put(node_id.to_string(), secret.to_string());
        }
    }

    /// Load every entry of the credentials file — the `[secrets] source =
    /// "file"` startup path, where `load_env` runs for `"env"`.
    ///
    /// `node` is stored under `node_id` (the username the registrar and the
    /// resolver look up), or under the literal `node` when there is no
    /// `[register]` yet, so the bootstrap page can save the number and the
    /// password in either order; every other key is stored under its own
    /// name. Returns the number of entries loaded; `Ok(0)` when the file
    /// does not exist.
    ///
    /// `Err`, with the store untouched, when the file cannot be read, does
    /// not parse, or is readable by group or others — that last one is
    /// exactly `credentials file <path> is readable by group or others
    /// (mode 0o644); the node did not load it — run: chmod 600 <path>`.
    /// Refusing is the middle ground: failing startup would crash-loop the
    /// node under `--restart=always` over a chmod, and a warning would keep
    /// serving a token other users can read. No value ever appears in an
    /// error, and a parse error is not forwarded at all: `toml_edit` renders
    /// the offending line, and on this file that line is a credential.
    pub fn load_file(&self, path: &Path, node_id: Option<&str>) -> Result<usize, String> {
        if !exists_and_is_private(path)? {
            return Ok(0);
        }
        let Some(doc) = read_document(path)? else {
            return Ok(0);
        };
        let entries = string_entries(path, &doc)?;
        let count = entries.len();
        for (key, value) in entries {
            let username = if key == NODE_CREDENTIAL_KEY {
                node_id.unwrap_or(NODE_CREDENTIAL_KEY).to_owned()
            } else {
                key
            };
            self.put(username, value);
        }
        Ok(count)
    }

    /// Seed the portal password from the env var `[portal] credential_env`
    /// names — the non-file sources' path — so the store, and therefore
    /// `export_file` and the settings page's presence chip, sees it like
    /// every other credential. Names only in diagnostics; a no-op when the
    /// variable is unset or empty.
    pub fn load_portal_env(&self, var: &str) {
        if let Ok(pw) = std::env::var(var)
            && !pw.is_empty()
        {
            self.put(PORTAL_CREDENTIAL_KEY, pw);
        }
    }

    /// Seed `echolink:<CALLSIGN>` from [`ECHOLINK_PASSWORD_ENV`], under any
    /// `[secrets] source`, the way the portal password is seeded. The
    /// callsign is uppercased, as `[echolink] callsign` is at load. Names
    /// only in diagnostics; a no-op when the variable is unset or empty.
    pub fn load_echolink_env(&self, callsign: &str) {
        if let Ok(pw) = std::env::var(ECHOLINK_PASSWORD_ENV)
            && !pw.is_empty()
        {
            self.put(echolink_key(callsign), pw);
        }
    }

    /// The `EchoLink` password for `callsign`, or `""` when none is held.
    ///
    /// The key is `echolink:<CALLSIGN>`, uppercase. A key written by hand in
    /// another case (`echolink:kx7abc-l` in the credentials file, or in a
    /// `POST /secrets`) matches too: a callsign has one spelling whatever
    /// its case, and a password that silently failed to match would read as
    /// "not set". The exact uppercase key wins when both exist.
    #[must_use]
    pub fn echolink_password(&self, callsign: &str) -> String {
        let key = echolink_key(callsign);
        let store = self.inner.lock().expect("SecretProvider mutex poisoned");
        if let Some(pw) = store.get(&key) {
            return pw.clone();
        }
        store
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// Set one key in the credentials file, creating the file when absent.
    ///
    /// Read-modify-write through `toml_edit`, so the header, the other
    /// entries and any hand edit survive. The bytes go to `<file>.tmp` in
    /// the same directory, opened with mode `0o600` before the first byte
    /// is written, fsynced, then renamed over `path`: a crash leaves the
    /// old file or the new one, and there is no moment at which the value
    /// sits in a file anyone else can read. `Err` is `cannot write <path>:
    /// <io error>` and never carries the value.
    pub fn save_file(path: &Path, key: &str, value: &str) -> Result<(), String> {
        let mut doc = read_document(path)?.unwrap_or_else(new_document);
        doc.insert(key, toml_edit::value(value));
        write_document(path, doc)
    }

    /// The keys in the credentials file, sorted, values discarded.
    /// `Ok(vec![])` when the file is absent. The mode is deliberately not
    /// checked: this is how the settings page still shows which chips exist
    /// on a node that refused to load the file, beside the chmod to run.
    pub fn file_keys(path: &Path) -> Result<Vec<String>, String> {
        let Some(doc) = read_document(path)? else {
            return Ok(Vec::new());
        };
        let mut keys: Vec<String> = doc.iter().map(|(k, _)| k.to_owned()).collect();
        keys.sort();
        Ok(keys)
    }

    /// Remove one key from the credentials file. `Ok(())` when the file or
    /// the key is absent, and nothing is rewritten in that case. Same atomic
    /// write as [`Self::save_file`], which also keeps the header on the file
    /// when the key removed was the first one.
    pub fn remove_from_file(path: &Path, key: &str) -> Result<(), String> {
        let Some(mut doc) = read_document(path)? else {
            return Ok(());
        };
        if doc.remove(key).is_none() {
            return Ok(());
        }
        write_document(path, doc)
    }

    /// Write every entry the store holds to `path`, replacing the file —
    /// the first step of the migration a node makes off `env`, `control` or
    /// `config`, before its config is flipped to `source = "file"`. The
    /// entry whose username is `node_id` is written under `node`, so the
    /// file reads the same as one the settings page built from scratch and
    /// survives a renumbering.
    ///
    /// When `node_id` is `Some` and the store holds it, a literal `node`
    /// username (a password saved before `[register]` existed) is skipped:
    /// both would land on the file key `node`, and the node-id entry is the
    /// one the running node registers with. Returns the number of distinct
    /// keys written.
    pub fn export_file(&self, path: &Path, node_id: Option<&str>) -> Result<usize, String> {
        let mut entries: Vec<(String, String)> = self
            .inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Usernames are HashMap keys, so unique: tuple order is key order.
        entries.sort_unstable();
        let holds_node_id = node_id.is_some_and(|id| entries.iter().any(|(k, _)| k == id));
        let mut doc = new_document();
        for (username, value) in &entries {
            let key = if node_id == Some(username.as_str()) {
                NODE_CREDENTIAL_KEY
            } else if holds_node_id && username == NODE_CREDENTIAL_KEY {
                continue;
            } else {
                username.as_str()
            };
            doc.insert(key, toml_edit::value(value.as_str()));
        }
        let written = doc.len();
        write_document(path, doc)?;
        Ok(written)
    }

    /// Forget one credential. A no-op when the username is unknown. This is
    /// what `Forget` on the settings page reaches; a page that can set a
    /// credential but not revoke it is the more dangerous one.
    pub fn remove(&self, username: &str) {
        self.inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .remove(username);
    }

    /// The usernames currently held, sorted. Names only — the settings page
    /// lists these as peers; the values never leave the store.
    #[must_use]
    pub fn usernames(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Return a `Box<dyn Fn(&str) -> String + Send + Sync>` over this provider,
    /// suitable for passing to `Station::set_secret_resolver`.
    pub fn resolver(&self) -> Box<dyn Fn(&str) -> String + Send + Sync> {
        let provider = self.clone();
        Box::new(move |username: &str| provider.resolve(username))
    }

    /// Return an `Arc<dyn Fn(&str) -> String + Send + Sync>` over this provider,
    /// suitable for `IncomingCallPolicy::credential_resolver` (iax-99cd). Shares
    /// the same backing store the env loader and `POST /secrets` feed, so
    /// inbound `auth=Required`/`Optional` authenticate against runtime creds.
    ///
    /// This is the INBOUND resolver, so it answers `""` for every key that is
    /// not an inbound peer credential — see [`is_inbound_username`]. A caller
    /// may name any username in its NEW, and the MD5 challenge that follows
    /// answers ACCEPT or REJECT per guess: resolving `portal` here would let
    /// anyone reaching the port guess the allstarlink.org password, and the
    /// same goes for the bot token, a link password, and a registration
    /// password stored under the literal `node`. [`Self::resolver`], which
    /// the node's own outbound dials use, is unaffected.
    #[must_use]
    pub fn resolver_arc(&self) -> Arc<dyn Fn(&str) -> String + Send + Sync> {
        let provider = self.clone();
        Arc::new(move |username: &str| {
            if is_inbound_username(username) {
                provider.resolve(username)
            } else {
                String::new()
            }
        })
    }
}

/// Whether `username` may authenticate an INBOUND call. The provider's
/// namespace is flat, so it also holds keys that are never an inbound
/// peer's: the portal password, the Discord bot token, Porkbun's keys, the
/// outbound `link:<node>` passwords, the `echolink:<CALL>` password, and the
/// registration password while it still sits under the literal `node` (no
/// `[register]` yet). Those stay out of
/// [`SecretProvider::resolver_arc`] so a remote caller cannot use the
/// inbound challenge to guess them.
#[must_use]
pub fn is_inbound_username(username: &str) -> bool {
    !(username == PORTAL_CREDENTIAL_KEY
        || username == DISCORD_CREDENTIAL_KEY
        || username == NODE_CREDENTIAL_KEY
        || username == PORKBUN_PK_KEY
        || username == PORKBUN_SK_KEY
        || username.starts_with(LINK_PREFIX)
        || username.starts_with(DNS_CREDENTIAL_PREFIX)
        || username.starts_with(ECHOLINK_PREFIX))
}

// ---------------------------------------------------------------------------
// The credentials file
// ---------------------------------------------------------------------------

/// A fresh document carrying the header as the root table's leading decor.
/// That is where it renders from on the first write; after a re-parse it
/// rides on the first key's prefix instead, which `write_document` covers.
fn new_document() -> DocumentMut {
    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix(CREDENTIALS_HEADER);
    doc
}

/// `Ok(true)` when the file exists and only its owner can read it,
/// `Ok(false)` when it does not exist. On non-unix targets existence is all
/// there is to check.
fn exists_and_is_private(path: &Path) -> Result<bool, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let shown = path.display();
            return Err(format!(
                "credentials file {shown} is readable by group or others (mode 0o{mode:03o}); the node did not load it — run: chmod 600 {shown}"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(true)
}

/// Parse the file. `Ok(None)` when it does not exist. A parse failure is
/// reported without the parser's own message: `toml_edit` prints the
/// offending line under a caret, and every line of this file is a secret.
fn read_document(path: &Path) -> Result<Option<DocumentMut>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let doc = text.parse::<DocumentMut>().map_err(|_| {
        format!(
            "credentials file {} does not parse; fix or delete it and set the credentials again from the settings page",
            path.display()
        )
    })?;
    Ok(Some(doc))
}

/// Every top-level `key = "string"` pair. Anything else — a table, a number
/// — is refused by key name; key names are not material.
fn string_entries(path: &Path, doc: &DocumentMut) -> Result<Vec<(String, String)>, String> {
    doc.iter()
        .map(|(key, item)| {
            item.as_str()
                .map(|v| (key.to_owned(), v.to_owned()))
                .ok_or_else(|| {
                    format!(
                        "credentials file {}: key {key:?} is not a string",
                        path.display()
                    )
                })
        })
        .collect()
}

/// `<file>.tmp` beside `path`.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || CREDENTIALS_FILE_NAME.into(),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".tmp");
    path.with_file_name(name)
}

/// Write `doc` over `path` atomically: a 0600 temp file in the same
/// directory (mode set on the open, before any byte lands — a later chmod
/// would leave a window), fsync, rename. The umask can only remove bits from
/// 0600, never add them. On failure the temp file is removed and the old
/// file is still there.
///
/// Takes the document by value so the header can be re-attached. Two
/// re-parse shapes need it: a parsed document keeps a leading comment on
/// its first key, so removing that key removes the comment; and a file that
/// is the header alone (after the last key was forgotten) parses with the
/// header in the document's *trailing* text, which `toml_edit` renders
/// LAST — so the next key would render above it and a second header would
/// be prepended. The trailing copy is dropped first, then the root prefix
/// is set whenever the rendered text does not already start with the header.
fn write_document(path: &Path, mut doc: DocumentMut) -> Result<(), String> {
    use std::io::Write;

    if doc
        .trailing()
        .as_str()
        .is_some_and(|t| t.contains(CREDENTIALS_HEADER.trim_end()))
    {
        doc.set_trailing("");
    }
    if !doc.to_string().starts_with(CREDENTIALS_HEADER) {
        doc.decor_mut().set_prefix(CREDENTIALS_HEADER);
    }
    let rendered = doc.to_string();

    let tmp = tmp_path(path);
    // A stale temp file from an interrupted write would defeat create_new;
    // it holds nothing the new write does not replace.
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(rendered.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_resolve() {
        let p = SecretProvider::new();
        p.put("55553", "hunter2");
        assert_eq!(p.resolve("55553"), "hunter2");
        assert_eq!(p.resolve("nobody"), "");
    }

    #[test]
    fn load_env_reads_allstar_vars() {
        // SAFETY: single-threaded test; set then clear.
        unsafe {
            std::env::set_var("ALLSTAR_NODE", "1234");
            std::env::set_var("ALLSTAR_SECRET", "sekret");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve("1234"), "sekret");
        unsafe {
            std::env::remove_var("ALLSTAR_NODE");
            std::env::remove_var("ALLSTAR_SECRET");
        }
    }

    #[test]
    fn load_env_reads_peer_vars() {
        unsafe {
            std::env::set_var("ALLSTAR_PEER_9876", "peersecret");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve("9876"), "peersecret");
        unsafe {
            std::env::remove_var("ALLSTAR_PEER_9876");
        }
    }

    #[test]
    fn load_env_reads_the_discord_bot_token() {
        // Every doc that names DISCORD_BOT_TOKEN promises this exact landing
        // spot: the "discord" username, which is what `handle_discord`
        // resolves. Before this was wired, `load_env` ignored the variable
        // entirely and the documented advice could not work.
        unsafe {
            std::env::set_var("DISCORD_BOT_TOKEN", "bot-token-value");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve("discord"), "bot-token-value");
        // It must not leak into the link namespace or a bare-node key.
        assert_eq!(p.link_secret("discord"), "");
        assert_eq!(p.resolve("DISCORD_BOT_TOKEN"), "");
        unsafe {
            std::env::remove_var("DISCORD_BOT_TOKEN");
        }
    }

    #[test]
    fn load_env_reads_the_porkbun_keys_and_the_inbound_resolver_never_answers_them() {
        unsafe {
            std::env::set_var(PORKBUN_PK_ENV, "pk1_env_value");
            std::env::set_var(PORKBUN_SK_ENV, "sk1_env_value");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve(PORKBUN_PK_KEY), "pk1_env_value");
        assert_eq!(p.resolve(PORKBUN_SK_KEY), "sk1_env_value");
        unsafe {
            std::env::remove_var(PORKBUN_PK_ENV);
            std::env::remove_var(PORKBUN_SK_ENV);
        }
        // A caller cannot use the inbound challenge to guess them.
        let inbound = p.resolver_arc();
        assert_eq!(inbound(PORKBUN_PK_KEY), "");
        assert_eq!(inbound(PORKBUN_SK_KEY), "");
        assert!(!is_inbound_username(PORKBUN_PK_KEY) && !is_inbound_username(PORKBUN_SK_KEY));
    }

    #[test]
    fn dns_credentials_are_never_inbound_usernames() {
        for spec in crate::config::DNS_UPDATE_PROVIDERS {
            assert!(
                spec.credential.starts_with(DNS_CREDENTIAL_PREFIX),
                "{}",
                spec.credential
            );
            assert!(!is_inbound_username(spec.credential), "{}", spec.credential);
            assert!(
                !spec.credential.contains("secret"),
                "the settings routes refuse that word"
            );
        }
        let p = SecretProvider::new();
        p.put("dns:cloudflare", "tok");
        assert_eq!((p.resolver_arc())("dns:cloudflare"), "");
    }

    #[test]
    fn dns_tokens_load_from_their_env_vars() {
        let spec = &crate::config::DNS_UPDATE_PROVIDERS[0];
        // SAFETY: test-only env mutation, as the Porkbun test above does.
        unsafe { std::env::set_var(spec.env, "env-token-value") };
        let p = SecretProvider::new();
        p.load_env();
        unsafe { std::env::remove_var(spec.env) };
        assert_eq!(p.resolve(spec.credential), "env-token-value");
    }

    #[test]
    fn resolver_closure_shares_state() {
        let p = SecretProvider::new();
        let resolver = p.resolver();
        // Insert after creating the resolver; they share the same Arc.
        p.put("live-node", "live-secret");
        assert_eq!(resolver("live-node"), "live-secret");
        assert_eq!(resolver("ghost"), "");
    }

    #[test]
    fn inbound_resolver_refuses_keys_that_are_not_inbound_peers() {
        // A remote caller picks the username in its NEW, and the MD5
        // challenge answers each guess. The provider must still hold these
        // keys for the node's own use; only the inbound resolver hides them.
        let p = SecretProvider::new();
        let hidden = [
            (PORTAL_CREDENTIAL_KEY, "portal-pw"),
            (DISCORD_CREDENTIAL_KEY, "bot-token"),
            (NODE_CREDENTIAL_KEY, "reg-pw"),
            ("link:2000", "link-pw"),
        ];
        for (key, value) in hidden {
            p.put(key, value);
        }
        p.put("55553", "peer-pw");
        let inbound = p.resolver_arc();
        let outbound = p.resolver();
        for (key, value) in hidden {
            assert_eq!(p.resolve(key), value, "the store must still hold {key}");
            assert_eq!(
                outbound(key),
                value,
                "the outbound resolver must find {key}"
            );
            assert_eq!(
                inbound(key),
                "",
                "the inbound resolver must not answer for {key}"
            );
        }
        // An ordinary inbound peer still authenticates.
        assert_eq!(inbound("55553"), "peer-pw");
        assert!(is_inbound_username("55553"));
        assert!(!is_inbound_username("link:"));
    }

    #[test]
    fn echolink_password_key_is_not_inbound() {
        let p = SecretProvider::new();
        p.put("echolink:KX7ABC-L", "el-pw-SENTINEL");
        assert!(!is_inbound_username("echolink:KX7ABC-L"));
        assert_eq!(p.resolver_arc()("echolink:KX7ABC-L"), "");
        assert_eq!(p.echolink_password("KX7ABC-L"), "el-pw-SENTINEL");
        assert_eq!(p.echolink_password("W1AW-L"), "", "a miss is empty");
    }

    #[test]
    fn echolink_password_matches_whatever_the_case() {
        let p = SecretProvider::new();
        p.put("echolink:kx7abc-l", "hand-written");
        assert_eq!(p.echolink_password("KX7ABC-L"), "hand-written");
        assert_eq!(p.echolink_password(" kx7abc-l "), "hand-written");
        p.put("echolink:KX7ABC-L", "exact");
        assert_eq!(
            p.echolink_password("kx7abc-l"),
            "exact",
            "the exact key wins"
        );
    }

    #[test]
    fn echolink_env_seeds_the_namespaced_key() {
        // SAFETY: the only test that touches this variable.
        unsafe { std::env::set_var(ECHOLINK_PASSWORD_ENV, "el-env-pw-7c21") };
        let p = SecretProvider::new();
        p.load_echolink_env("KX7ABC-L");
        unsafe { std::env::remove_var(ECHOLINK_PASSWORD_ENV) };
        assert_eq!(ECHOLINK_PASSWORD_ENV, "ASTAR_ECHOLINK_PASSWORD");
        assert_eq!(p.echolink_password("KX7ABC-L"), "el-env-pw-7c21");
        assert_eq!(p.usernames(), vec![format!("{ECHOLINK_PREFIX}KX7ABC-L")]);
        // A lower-case callsign is stored under the uppercase key.
        // SAFETY: as above.
        unsafe { std::env::set_var(ECHOLINK_PASSWORD_ENV, "el-env-pw-7c21") };
        let lower = SecretProvider::new();
        lower.load_echolink_env("kx7abc-l");
        unsafe { std::env::remove_var(ECHOLINK_PASSWORD_ENV) };
        assert_eq!(
            lower.usernames(),
            vec![format!("{ECHOLINK_PREFIX}KX7ABC-L")]
        );
        let unset = SecretProvider::new();
        unset.load_echolink_env("KX7ABC-L");
        assert!(unset.usernames().is_empty(), "unset is a no-op");
    }

    #[test]
    fn clone_shares_store() {
        let p1 = SecretProvider::new();
        let p2 = p1.clone();
        p1.put("shared", "value");
        assert_eq!(p2.resolve("shared"), "value");
    }

    // -- iax-4703: [secrets] source = "config" (inline secret) -------------

    #[test]
    fn load_config_secret_reaches_resolver() {
        // Mirrors `load_env_reads_allstar_vars`: a config-sourced secret must
        // reach the same seam (`resolve`) that the env path feeds.
        let p = SecretProvider::new();
        p.load_config_secret("77777", "hunter2xyz");
        assert_eq!(p.resolve("77777"), "hunter2xyz");
    }

    #[test]
    fn load_config_secret_empty_is_noop() {
        let p = SecretProvider::new();
        p.load_config_secret("77777", "");
        assert_eq!(p.resolve("77777"), "");
    }

    // -- iax-5029: outbound link-secret namespace ---------------------------

    #[test]
    fn load_env_reads_link_vars_into_link_namespace() {
        unsafe {
            std::env::set_var("ALLSTAR_LINK_1999", "outbound-pw");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.link_secret("1999"), "outbound-pw");
        // The namespaces must not collide: no bare-username entry appears.
        assert_eq!(p.resolve("1999"), "");
        unsafe {
            std::env::remove_var("ALLSTAR_LINK_1999");
        }
    }

    #[test]
    fn link_secret_miss_is_empty() {
        let p = SecretProvider::new();
        assert_eq!(p.link_secret("nobody"), "");
    }

    #[test]
    fn link_secret_reachable_via_provide_secret_key() {
        // POST /secrets with username "link:<node>" feeds the same seam.
        let p = SecretProvider::new();
        p.put("link:55000", "pushed-pw");
        assert_eq!(p.link_secret("55000"), "pushed-pw");
    }

    // -- the credentials file: [secrets] source = "file" --------------------

    use std::path::{Path, PathBuf};

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CREDENTIALS_FILE_NAME);
        (dir, path)
    }

    /// Write `text` as a file only its owner can read, the way `save_file`
    /// would have — so a test about parsing is not tripped by the mode check.
    fn write_private(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn save_file_creates_the_file_with_its_header_and_load_file_reads_it_back() {
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "pw-one").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# astar-server credentials — written by the settings page. Mode 0600.\nnode = \"pw-one\"\n"
        );
        let p = SecretProvider::new();
        assert_eq!(p.load_file(&path, Some("12345")), Ok(1));
        assert_eq!(p.resolve("12345"), "pw-one");
        assert_eq!(p.resolve("node"), "", "the file key is not a username");
    }

    #[test]
    fn values_with_quotes_backslashes_and_hashes_round_trip() {
        let (_dir, path) = scratch();
        let awkward = r#"a"b\c#d 'e' "#;
        SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, awkward).unwrap();
        SecretProvider::save_file(&path, DISCORD_CREDENTIAL_KEY, "tok#en\\with\"stuff").unwrap();
        let p = SecretProvider::new();
        assert_eq!(p.load_file(&path, None), Ok(2));
        assert_eq!(p.resolve("portal"), awkward);
        assert_eq!(p.resolve("discord"), "tok#en\\with\"stuff");
        // The second save kept the first entry and the header.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(
                "# astar-server credentials — written by the settings page. Mode 0600.\n"
            ),
            "{text}"
        );
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["discord".to_owned(), "portal".to_owned()])
        );

        // Every shape a real AllStar password or bot token takes, under the
        // one key that also needs quoting. Each round trip is exact.
        for value in [
            "pa\"ss",
            "back\\slash",
            "hash#tag",
            "eq=ual",
            "ünïcødé ☕",
            "  spaces  ",
        ] {
            SecretProvider::save_file(&path, &format!("{LINK_PREFIX}2000"), value).unwrap();
            let fresh = SecretProvider::new();
            assert_eq!(fresh.load_file(&path, None), Ok(3), "{value:?}");
            assert_eq!(
                fresh.link_secret("2000"),
                value,
                "{value:?} must reload byte for byte"
            );
        }
    }

    #[test]
    fn a_link_key_is_quoted_in_the_file_and_reloads_into_the_link_namespace() {
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, &format!("{LINK_PREFIX}2000"), "dial-pw").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().any(|l| l == "\"link:2000\" = \"dial-pw\""),
            "a key with a colon must be a quoted key: {text}"
        );
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["link:2000".to_owned()])
        );
        let p = SecretProvider::new();
        assert_eq!(p.load_file(&path, None), Ok(1));
        assert_eq!(p.link_secret("2000"), "dial-pw");
        assert_eq!(p.resolve("2000"), "", "never the inbound peer namespace");
    }

    #[test]
    fn a_missing_file_loads_nothing_and_lists_no_keys() {
        let (_dir, path) = scratch();
        let p = SecretProvider::new();
        assert_eq!(p.load_file(&path, Some("12345")), Ok(0));
        assert_eq!(SecretProvider::file_keys(&path), Ok(vec![]));
        assert_eq!(p.usernames(), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_created_mode_0600_and_stays_so_across_a_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "pw").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !path.with_file_name("credentials.toml.tmp").exists(),
            "the temp file is renamed over the target, never left behind"
        );
        SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "pp").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn load_portal_env_seeds_the_portal_key() {
        // The non-file sources read the portal password from the env var
        // [portal] credential_env names; seeding the store with it is what
        // lets export_file carry it and the settings page's chip see it.
        let p = SecretProvider::new();
        // SAFETY: tests in this module already set and remove process env
        // vars (`load_env_reads_*`); this one is uniquely named.
        unsafe { std::env::set_var("ASTAR_TEST_PORTAL_PASS", "portal-pw-9f3a") };
        p.load_portal_env("ASTAR_TEST_PORTAL_PASS");
        unsafe { std::env::remove_var("ASTAR_TEST_PORTAL_PASS") };
        assert_eq!(p.resolve(PORTAL_CREDENTIAL_KEY), "portal-pw-9f3a");
        assert_eq!(
            p.resolve("ASTAR_TEST_PORTAL_PASS"),
            "",
            "the variable's NAME is never a username"
        );
        let unset = SecretProvider::new();
        unset.load_portal_env("ASTAR_TEST_PORTAL_PASS");
        assert_eq!(
            unset.usernames(),
            Vec::<String>::new(),
            "unset or empty is a no-op"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_file_is_refused_and_the_store_stays_empty() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "pw-leaky-SENTINEL").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let p = SecretProvider::new();
        let err = p.load_file(&path, Some("12345")).unwrap_err();
        let shown = path.display();
        assert_eq!(
            err,
            format!(
                "credentials file {shown} is readable by group or others (mode 0o644); the node did not load it — run: chmod 600 {shown}"
            )
        );
        assert!(!err.contains("SENTINEL"));
        assert_eq!(p.resolve("12345"), "");
        assert_eq!(p.usernames(), Vec::<String>::new());
        // The keys are still listable: the page shows the chip and the fix.
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["node".to_owned()])
        );
        // Saving through the API heals the mode: the rename replaces the
        // 0644 file with a 0600 one and the next load succeeds.
        SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "pp").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(p.load_file(&path, Some("12345")), Ok(2));
        assert_eq!(p.resolve("12345"), "pw-leaky-SENTINEL");
    }

    #[test]
    fn a_file_that_does_not_parse_is_refused_without_quoting_its_contents() {
        let (_dir, path) = scratch();
        // An unterminated string: the parser's own message would print this
        // line under a caret, sentinel included.
        write_private(&path, "node = \"SENTINEL-9f3a\nportal = \n");
        let p = SecretProvider::new();
        let err = p.load_file(&path, Some("12345")).unwrap_err();
        assert_eq!(
            err,
            format!(
                "credentials file {} does not parse; fix or delete it and set the credentials again from the settings page",
                path.display()
            )
        );
        assert_eq!(p.resolve("12345"), "");
        // save_file and file_keys refuse the same way rather than clobber a
        // file someone may be half-way through editing by hand.
        assert_eq!(
            SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "x").unwrap_err(),
            err
        );
        assert_eq!(SecretProvider::file_keys(&path).unwrap_err(), err);
    }

    #[test]
    fn a_non_string_value_is_refused_by_key_name() {
        let (_dir, path) = scratch();
        write_private(&path, "node = 5\n");
        let p = SecretProvider::new();
        let err = p.load_file(&path, None).unwrap_err();
        assert_eq!(
            err,
            format!(
                "credentials file {}: key \"node\" is not a string",
                path.display()
            )
        );
        assert_eq!(p.usernames(), Vec::<String>::new());
    }

    #[test]
    fn remove_forgets_the_store_entry_and_remove_from_file_drops_the_key() {
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "pw").unwrap();
        SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "pp").unwrap();
        let p = SecretProvider::new();
        assert_eq!(p.load_file(&path, Some("12345")), Ok(2));
        assert_eq!(p.usernames(), vec!["12345".to_owned(), "portal".to_owned()]);

        p.remove("portal");
        assert_eq!(p.resolve("portal"), "");
        assert_eq!(p.usernames(), vec!["12345".to_owned()]);
        p.remove("never-there");
        assert_eq!(p.usernames(), vec!["12345".to_owned()]);

        SecretProvider::remove_from_file(&path, PORTAL_CREDENTIAL_KEY).unwrap();
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["node".to_owned()])
        );
        let before = std::fs::read(&path).unwrap();
        // An absent key: Ok, and the file is not rewritten. Equal bytes
        // alone cannot prove that — re-rendering an unchanged document gives
        // the same bytes — but `write_document` renames a new file over the
        // path, so a rewrite changes the inode.
        #[cfg(unix)]
        let ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };
        SecretProvider::remove_from_file(&path, PORTAL_CREDENTIAL_KEY).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().ino(),
                ino,
                "an absent key must not rewrite the file"
            );
        }
        // An absent file: Ok, and none is created.
        let nope = path.with_file_name("nope.toml");
        SecretProvider::remove_from_file(&nope, NODE_CREDENTIAL_KEY).unwrap();
        assert!(!nope.exists());

        // Removing the FIRST key: after a re-parse the header comment hangs
        // on that key, so a naive remove would drop the header with it. The
        // Forget flow does exactly this on a node whose only entry is `node`.
        SecretProvider::remove_from_file(&path, NODE_CREDENTIAL_KEY).unwrap();
        assert_eq!(SecretProvider::file_keys(&path), Ok(vec![]));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# astar-server credentials — written by the settings page. Mode 0600.\n"
        );

        // And the ordinary page flow after that: Forget the only credential,
        // then Set one. A header-only file re-parses with the header in the
        // document's TRAILING text, which renders after the keys; without
        // the trailing fix the file would come back header / key / header.
        SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "pp").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# astar-server credentials — written by the settings page. Mode 0600.\nportal = \"pp\"\n"
        );
    }

    #[test]
    fn node_maps_to_the_node_id_and_to_the_literal_key_without_a_register_section() {
        let (_dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "reg-pw").unwrap();

        let with_id = SecretProvider::new();
        assert_eq!(with_id.load_file(&path, Some("54321")), Ok(1));
        assert_eq!(with_id.resolve("54321"), "reg-pw");
        assert_eq!(with_id.resolve("node"), "");

        let without = SecretProvider::new();
        assert_eq!(without.load_file(&path, None), Ok(1));
        assert_eq!(without.resolve("node"), "reg-pw");
        assert_eq!(without.usernames(), vec!["node".to_owned()]);
    }

    #[test]
    fn export_file_writes_every_entry_with_the_node_id_under_node() {
        let (_dir, path) = scratch();
        let p = SecretProvider::new();
        p.put("12345", "reg-pw");
        p.put("portal", "portal-pw");
        p.put("link:2000", "dial-pw");
        p.put("allstar-public", "peer-pw");

        assert_eq!(p.export_file(&path, Some("12345")), Ok(4));
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec![
                "allstar-public".to_owned(),
                "link:2000".to_owned(),
                "node".to_owned(),
                "portal".to_owned(),
            ])
        );
        assert!(std::fs::read_to_string(&path).unwrap().starts_with(
            "# astar-server credentials — written by the settings page. Mode 0600.\n"
        ));

        // Renumbering-safe: the entry is keyed `node`, so a node that
        // reloads as 99999 still finds its password.
        let renumbered = SecretProvider::new();
        assert_eq!(renumbered.load_file(&path, Some("99999")), Ok(4));
        assert_eq!(renumbered.resolve("99999"), "reg-pw");
        assert_eq!(renumbered.resolve("12345"), "");
        assert_eq!(renumbered.link_secret("2000"), "dial-pw");
        assert_eq!(renumbered.resolve("allstar-public"), "peer-pw");
        assert_eq!(renumbered.resolve("portal"), "portal-pw");

        // Replaces, never merges: a key the store does not hold is gone.
        SecretProvider::save_file(&path, DISCORD_CREDENTIAL_KEY, "old-token").unwrap();
        assert_eq!(p.export_file(&path, Some("12345")), Ok(4));
        assert!(
            !SecretProvider::file_keys(&path)
                .unwrap()
                .contains(&"discord".to_owned())
        );
    }

    #[test]
    fn export_file_without_a_node_id_keeps_every_username_as_written() {
        let (_dir, path) = scratch();
        let p = SecretProvider::new();
        p.put("12345", "reg-pw");
        assert_eq!(p.export_file(&path, None), Ok(1));
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["12345".to_owned()])
        );
    }

    #[test]
    fn export_file_prefers_the_node_id_entry_over_a_literal_node_entry() {
        // A password saved before [register] existed sits under the literal
        // `node`; once the node has an id, both map to the file key `node`.
        // The node-id entry wins, and the count is the keys actually written.
        let (_dir, path) = scratch();
        let p = SecretProvider::new();
        p.put("12345", "a");
        p.put("node", "b");
        assert_eq!(p.export_file(&path, Some("12345")), Ok(1));
        assert_eq!(
            SecretProvider::file_keys(&path),
            Ok(vec!["node".to_owned()])
        );
        let reloaded = SecretProvider::new();
        assert_eq!(reloaded.load_file(&path, Some("12345")), Ok(1));
        assert_eq!(reloaded.resolve("12345"), "a");
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_untouched() {
        let (dir, path) = scratch();
        SecretProvider::save_file(&path, NODE_CREDENTIAL_KEY, "pw").unwrap();
        let before = std::fs::read(&path).unwrap();

        // A directory where the temp file goes: `remove_file` cannot clear
        // it and `create_new` refuses it, so the rename never happens and
        // the old bytes stay. Unlike a read-only directory, this fails for
        // root too, so the test holds wherever it runs.
        std::fs::create_dir(dir.path().join("credentials.toml.tmp")).unwrap();
        let result = SecretProvider::save_file(&path, PORTAL_CREDENTIAL_KEY, "SENTINEL-portal-pw");

        let err = result.unwrap_err();
        assert!(
            err.starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert!(!err.contains("SENTINEL"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // No temp-file assertion here: create_new fails before one exists,
        // so it could not fail. The cleanup is pinned by the next test.
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        // A non-empty directory where the file should be: the temp file IS
        // created, written and fsynced, and only the rename fails — the one
        // path on which `write_document`'s error-arm cleanup has work to do.
        // `export_file` does not read the target first, so it gets that far.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join(CREDENTIALS_FILE_NAME);
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("x"), "").unwrap();

        let p = SecretProvider::new();
        p.put("portal", "SENTINEL-rename-pw");
        let err = p.export_file(&target, None).unwrap_err();
        assert!(
            err.starts_with(&format!("cannot write {}: ", target.display())),
            "{err}"
        );
        assert!(!err.contains("SENTINEL"), "{err}");
        assert!(
            !dir.path().join("credentials.toml.tmp").exists(),
            "a failed rename must not leave the value behind in the temp file"
        );
        assert!(target.join("x").exists(), "the directory is untouched");
    }
}
