// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `state_dir/auth.json`: users, credentials, sessions, invites and the
//! audit ring, in one small JSON file.
//!
//! The rules are `credentials.toml`'s (`secrets.rs`): created 0600 before
//! the first byte, written to a 0600 temp file in the same directory,
//! fsynced and renamed over, and refused at load when group or others can
//! read it. Unlike `credentials.toml` a refused file does not stop the node:
//! sign-in is disabled and the loopback admin page says why, because a
//! world-readable session store must not be trusted and a chmod must not
//! crash-loop a container.
//!
//! Every writer — the server and the `astar-server admin` CLI — takes the
//! advisory lock on `auth.lock` around its write.
//!
//! None of these types implements `Debug`: a session hash, a TOTP seed or a
//! serialised passkey must have no way into a log line through `{:?}`.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::{Passkey, SecurityKey};

/// The `version` this build reads and writes.
pub const STORE_VERSION: u32 = 1;
/// The audit ring keeps this many entries.
pub const AUDIT_CAP: usize = 500;
/// Failures of one kind from one source within this long share an entry.
pub const ROLLUP_SECS: u64 = 3600;
/// The store's file name inside the state directory.
pub const STORE_FILE: &str = "auth.json";
/// The lock file every writer holds while it writes.
pub const LOCK_FILE: &str = "auth.lock";

/// A role. Read from the user record on every request, so a change takes
/// effect at once. (`Debug` is harmless here: two words, no secret.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Admin,
}

impl Role {
    /// The spelling the API and the store use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Admin => "admin",
        }
    }

    /// Parse the API spelling.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// TOTP failure bookkeeping for one user.
///
/// Failures are counted twice: against the source they came from (an IPv4
/// address or an IPv6 /64), whose lockout escalates, and — when that source
/// has never signed in as this user — against all such strangers together,
/// whose lockout is short and never grows. A source the user has signed in
/// from is never held back by strangers. `totp.rs` has the rules.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct TotpFailures {
    /// Strangers' failures since their last lockout.
    #[serde(default)]
    pub count: u32,
    /// Unix seconds until which strangers' codes are refused.
    #[serde(default)]
    pub locked_until: u64,
    /// Strangers' lockouts in the current run: after the first, each
    /// further failure locks again.
    #[serde(default)]
    pub level: u32,
    /// The last stranger's failure; a day without one ends the run.
    #[serde(default)]
    pub last_failure: u64,
    /// Failed codes since the last successful sign-in, for the notice.
    #[serde(default)]
    pub since_sign_in: u32,
    /// Per source, most recently active last; bounded.
    #[serde(default)]
    pub sources: Vec<SourceFailures>,
}

/// TOTP failures from one source against one user.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SourceFailures {
    /// The source: an IPv4 address or an IPv6 /64.
    pub key: String,
    /// Failures since this source's last lockout or sign-in.
    #[serde(default)]
    pub count: u32,
    /// Unix seconds until which this source's codes are refused.
    #[serde(default)]
    pub locked_until: u64,
    /// Lockouts so far; each doubles the next one, up to the cap.
    #[serde(default)]
    pub level: u32,
    /// The last failure or sign-in from here.
    #[serde(default)]
    pub last: u64,
    /// The user has signed in from here.
    #[serde(default)]
    pub trusted: bool,
}

/// One person.
#[derive(Clone, Serialize, Deserialize)]
pub struct User {
    /// Random; also the WebAuthn user handle, so no callsign goes into an
    /// authenticator.
    pub id: Uuid,
    /// Unique, case-insensitive; what identifier-first sign-in asks for.
    pub name: String,
    pub display_name: String,
    pub role: Role,
    pub created: u64,
    #[serde(default)]
    pub last_sign_in: Option<u64>,
    #[serde(default)]
    pub totp: TotpFailures,
}

/// What a credential is, with what each kind needs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialKind {
    /// A passkey or a FIDO2 key with a PIN (user verification required).
    Passkey {
        rp_id: String,
        passkey: Passkey,
        #[serde(default)]
        aaguid: Option<String>,
        #[serde(default)]
        backup_eligible: bool,
        #[serde(default)]
        backup_state: bool,
    },
    /// A security key without a PIN: possession only.
    SecurityKey {
        rp_id: String,
        key: SecurityKey,
        #[serde(default)]
        aaguid: Option<String>,
    },
    /// An authenticator app.
    Totp {
        /// Base32, as the user's app has it.
        secret: String,
        /// The last time step accepted, so a code cannot be used twice.
        #[serde(default)]
        last_step: u64,
    },
}

impl CredentialKind {
    /// The API's name for the kind.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Passkey { .. } => "passkey",
            Self::SecurityKey { .. } => "security_key",
            Self::Totp { .. } => "totp",
        }
    }

    /// The RP ID a WebAuthn credential is bound to; `None` for TOTP.
    #[must_use]
    pub fn rp_id(&self) -> Option<&str> {
        match self {
            Self::Passkey { rp_id, .. } | Self::SecurityKey { rp_id, .. } => Some(rp_id),
            Self::Totp { .. } => None,
        }
    }

    /// The WebAuthn credential id, for exclude lists and lookups.
    #[must_use]
    pub fn webauthn_id(&self) -> Option<&[u8]> {
        match self {
            Self::Passkey { passkey, .. } => Some(passkey.cred_id().as_ref()),
            Self::SecurityKey { key, .. } => Some(key.cred_id().as_ref()),
            Self::Totp { .. } => None,
        }
    }
}

/// One sign-in method of one user.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Random, Base64url; what the API names it by.
    pub id: String,
    pub user_id: Uuid,
    pub nickname: String,
    pub created: u64,
    #[serde(default)]
    pub last_used: Option<u64>,
    #[serde(flatten)]
    pub kind: CredentialKind,
}

/// One signed-in browser. The cookie itself is never stored.
#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    /// Random, short; what the sessions list names it by. Not the cookie.
    pub id: String,
    /// Hex SHA-256 of the cookie value.
    pub hash: String,
    pub user_id: Uuid,
    pub created: u64,
    pub last_seen: u64,
    /// Unix seconds after which it is dead whatever `last_seen` says.
    pub expires: u64,
    /// The last time a credential was used in this session (sign-in or
    /// step-up).
    pub last_auth_at: u64,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub ip: String,
    /// The credential this session signed in with. `None` on a session
    /// from before this field existed, or one no credential made (tests):
    /// removing any of the user's credentials ends such a session, since
    /// it may have been that one.
    #[serde(default)]
    pub credential: Option<String>,
}

/// Who an invite is for.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "for", rename_all = "snake_case")]
pub enum InviteTarget {
    /// A new user with this role.
    NewUser { role: Role },
    /// Another credential for an existing user (re-enrol).
    User { user_id: Uuid },
}

/// The `created_by_id` of an invite the console made. No user has it: user
/// ids are random v4 UUIDs, never nil.
pub const CONSOLE_CREATOR: Uuid = Uuid::nil();

/// A one-time link.
#[derive(Clone, Serialize, Deserialize)]
pub struct Invite {
    /// Random, short; the list and revoke name it by this. Not the token.
    pub id: String,
    /// Hex SHA-256 of the token.
    pub hash: String,
    #[serde(flatten)]
    pub target: InviteTarget,
    /// The suggested name for a new user.
    #[serde(default)]
    pub name: Option<String>,
    /// The creator's name, for the list and the invite page.
    pub created_by: String,
    /// The creator's user id: [`CONSOLE_CREATOR`] for the console, `None`
    /// on an invite written before this field existed (matched by
    /// `created_by` instead). Deleting or demoting the creator withdraws
    /// the invites they made.
    #[serde(default)]
    pub created_by_id: Option<Uuid>,
    pub created: u64,
    pub expires: u64,
    #[serde(default)]
    pub used_at: Option<u64>,
    #[serde(default)]
    pub used_by: Option<String>,
    #[serde(default)]
    pub revoked: bool,
}

impl Invite {
    /// Whether the user `id`, named `name`, made this invite. An invite
    /// from before creator ids is matched by name.
    #[must_use]
    pub fn made_by(&self, id: Uuid, name: &str) -> bool {
        match self.created_by_id {
            Some(c) => c == id,
            None => self.created_by.eq_ignore_ascii_case(name),
        }
    }
}

/// One line of the audit ring. Names users, nicknames, IPs and user agents;
/// never a token, cookie, secret or code.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: u64,
    pub event: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub ip: String,
    /// For a rolled-up failure: how many times it happened in its window
    /// (0 for an entry that is not one).
    #[serde(default)]
    pub times: u32,
    /// For a rolled-up failure seen more than once: when it first was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

impl AuditEntry {
    /// When a rolled-up failure's window began.
    #[must_use]
    pub fn first_at(&self) -> u64 {
        self.since.unwrap_or(self.at)
    }
}

/// The whole file.
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthData {
    pub version: u32,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub invites: Vec<Invite>,
    #[serde(default)]
    pub audit: VecDeque<AuditEntry>,
}

impl Default for AuthData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            users: Vec::new(),
            credentials: Vec::new(),
            sessions: Vec::new(),
            invites: Vec::new(),
            audit: VecDeque::new(),
        }
    }
}

impl AuthData {
    /// Append to the audit ring. Past [`AUDIT_CAP`] the oldest failure
    /// nobody's name is on goes first, then the oldest of anything, so a
    /// flood of anonymous failures can push out only its own kind.
    pub fn audit(&mut self, entry: AuditEntry) {
        self.audit.push_back(entry);
        while self.audit.len() > AUDIT_CAP {
            let anonymous = self
                .audit
                .iter()
                .position(|e| e.user.is_none() && e.times > 0);
            match anonymous {
                Some(i) if i + 1 < self.audit.len() => {
                    self.audit.remove(i);
                }
                _ => {
                    self.audit.pop_front();
                }
            }
        }
    }

    /// Record a failure (a sign-in or step-up that did not work): folded
    /// into the entry for the same event, user and source from the last
    /// [`ROLLUP_SECS`], which moves to the newest place with one more
    /// `times`. `entry.ip` should be the client key, so that a client
    /// rotating through its IPv6 /64 is one source.
    pub fn audit_failure(&mut self, mut entry: AuditEntry) {
        let same = self.audit.iter().rposition(|e| {
            e.times > 0
                && e.event == entry.event
                && e.user == entry.user
                && e.ip == entry.ip
                && e.detail == entry.detail
                && entry.at.saturating_sub(e.first_at()) < ROLLUP_SECS
        });
        entry.times = 1;
        if let Some(i) = same
            && let Some(old) = self.audit.remove(i)
        {
            entry.times = old.times.saturating_add(1);
            entry.since = Some(old.first_at());
        }
        self.audit(entry);
    }

    /// The user named `name`, case-insensitively.
    #[must_use]
    pub fn user_by_name(&self, name: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.name.eq_ignore_ascii_case(name.trim()))
    }

    /// The user with this id.
    #[must_use]
    pub fn user(&self, id: Uuid) -> Option<&User> {
        self.users.iter().find(|u| u.id == id)
    }

    /// The user with this id, to change.
    pub fn user_mut(&mut self, id: Uuid) -> Option<&mut User> {
        self.users.iter_mut().find(|u| u.id == id)
    }

    /// Every credential of one user.
    pub fn credentials_of(&self, user: Uuid) -> impl Iterator<Item = &Credential> {
        self.credentials.iter().filter(move |c| c.user_id == user)
    }

    /// Withdraw the pending invites `user` made, and return how many.
    /// `keep_own` spares the re-enrol links they made for themselves: a
    /// demotion takes away the right to invite others, not to add a
    /// credential of one's own.
    pub fn withdraw_invites_by(&mut self, user: Uuid, keep_own: bool) -> usize {
        let Some(u) = self.user(user) else {
            return 0;
        };
        let name = u.name.clone();
        let mut n = 0;
        for i in &mut self.invites {
            if i.used_at.is_some() || i.revoked || !i.made_by(user, &name) {
                continue;
            }
            if keep_own && matches!(i.target, InviteTarget::User { user_id } if user_id == user) {
                continue;
            }
            i.revoked = true;
            n += 1;
        }
        n
    }

    /// Whether whoever made `inv` still stands behind it: the console
    /// always does; a user only while they exist, and — for an invite to
    /// anyone but themselves — while they are an admin. Checked at every
    /// use, so an invite whose creator went some way other than through
    /// [`Self::withdraw_invites_by`] still dies with them.
    #[must_use]
    pub fn creator_vouches(&self, inv: &Invite) -> bool {
        let creator = match inv.created_by_id {
            Some(CONSOLE_CREATOR) => return true,
            Some(id) => self.user(id),
            // Written before creator ids: go by the name, as it was then.
            None if inv.created_by == "console" => return true,
            None => self.user_by_name(&inv.created_by),
        };
        creator.is_some_and(|u| {
            u.role == Role::Admin
                || matches!(inv.target, InviteTarget::User { user_id } if user_id == u.id)
        })
    }

    /// How many passkeys and security keys `user` has.
    #[must_use]
    pub fn keys_of(&self, user: Uuid) -> usize {
        self.credentials_of(user)
            .filter(|c| c.kind.webauthn_id().is_some())
            .count()
    }

    /// Whether an authenticator-app code may sign `user` in. A viewer's
    /// always may. An admin's may not: since codes are only rate-limited
    /// (about 100 guesses a day at one name, from enough addresses), admins
    /// sign in with a passkey or security key, and codes only confirm it's
    /// them in a session they already have (step-up).
    ///
    /// The one exception is an admin with no passkey or key at all, which
    /// only a store from before this rule can hold: refusing their codes
    /// would lock them out silently, and if they are the only admin, strand
    /// the node until someone at its console noticed. So their codes keep
    /// working until they add a key — the account page asks them to, the
    /// server warns at start-up ([`Self::admins_without_a_key`]), and once
    /// they have one it can never be removed (the last key is kept).
    #[must_use]
    pub fn codes_sign_in(&self, user: Uuid) -> bool {
        self.user(user)
            .is_some_and(|u| u.role != Role::Admin || self.keys_of(user) == 0)
    }

    /// The admins who sign in with codes and have no passkey or security
    /// key, by name: written before admins had to have one.
    #[must_use]
    pub fn admins_without_a_key(&self) -> Vec<String> {
        self.users
            .iter()
            .filter(|u| {
                u.role == Role::Admin
                    && self.keys_of(u.id) == 0
                    && self
                        .credentials_of(u.id)
                        .any(|c| matches!(c.kind, CredentialKind::Totp { .. }))
            })
            .map(|u| u.name.clone())
            .collect()
    }

    /// How many admins there are.
    #[must_use]
    pub fn admin_count(&self) -> usize {
        self.users.iter().filter(|u| u.role == Role::Admin).count()
    }
}

/// What loading found.
pub enum Loaded {
    /// The file, or an empty store when there is none yet.
    Ok(AuthData),
    /// Sign-in is disabled; the sentence says why and how to fix it. Never
    /// contains anything from the file.
    Disabled(String),
}

/// Just the `version` field, read before the rest.
#[derive(Deserialize)]
struct StoreVersion {
    version: u32,
}

/// The store's path in `dir`.
#[must_use]
pub fn store_path(dir: &Path) -> PathBuf {
    dir.join(STORE_FILE)
}

/// Read `dir/auth.json`. A missing file is an empty store.
#[must_use]
pub fn load(dir: &Path) -> Loaded {
    let path = store_path(dir);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded::Ok(AuthData::default());
        }
        Err(e) => {
            return Loaded::Disabled(format!(
                "cannot read {}: {}; sign-in is disabled",
                path.display(),
                e.kind()
            ));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let shown = path.display();
            return Loaded::Disabled(format!(
                "{shown} is readable by group or others (mode 0o{mode:03o}), so sign-in is disabled — run: chmod 600 {shown}"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let text = match std::fs::read(&path) {
        Ok(t) => t,
        Err(e) => {
            return Loaded::Disabled(format!(
                "cannot read {}: {}; sign-in is disabled",
                path.display(),
                e.kind()
            ));
        }
    };
    // The version first, on its own, so a newer file is named as such
    // rather than as a parse error.
    let version = match serde_json::from_slice::<StoreVersion>(&text) {
        Ok(v) => v.version,
        Err(_) => {
            return Loaded::Disabled(format!(
                "{} does not parse, so sign-in is disabled; restore it from a backup, or move it aside to start again",
                path.display()
            ));
        }
    };
    if version != STORE_VERSION {
        return Loaded::Disabled(format!(
            "{} is version {version}, which this build does not know, so sign-in is disabled",
            path.display()
        ));
    }
    match serde_json::from_slice::<AuthData>(&text) {
        Ok(d) => Loaded::Ok(d),
        // serde_json's message quotes the offending input, which here may be
        // a secret; the sentence names the file and nothing from it.
        Err(_) => Loaded::Disabled(format!(
            "{} does not parse, so sign-in is disabled; restore it from a backup, or move it aside to start again",
            path.display()
        )),
    }
}

/// Create `dir` 0700 if it does not exist.
///
/// # Errors
/// The directory cannot be created.
pub fn ensure_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// The advisory lock on `dir/auth.lock`, held until dropped.
pub struct StoreLock {
    _file: File,
}

/// Take the lock, blocking until it is free.
///
/// # Errors
/// The lock file cannot be opened or locked.
pub fn lock(dir: &Path) -> Result<StoreLock, String> {
    ensure_dir(dir)?;
    let path = dir.join(LOCK_FILE);
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.lock()
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    Ok(StoreLock { _file: file })
}

/// Write `data` over `dir/auth.json` atomically, under the lock.
///
/// # Errors
/// The directory, the lock or the write fails; the old file is untouched.
pub fn save(dir: &Path, data: &AuthData) -> Result<(), String> {
    let _held = lock(dir)?;
    write_locked(dir, data)
}

/// [`save`] for a caller that already holds the lock.
///
/// # Errors
/// As [`save`].
pub fn write_locked(dir: &Path, data: &AuthData) -> Result<(), String> {
    let path = store_path(dir);
    let rendered = serde_json::to_vec_pretty(data)
        .map_err(|_| "cannot serialise the auth store".to_string())?;
    let tmp = dir.join(format!("{STORE_FILE}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(&rendered)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
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
    use std::sync::mpsc;
    use std::time::Duration;

    // Inherent consts win over trait consts, so `Probe::<T>::IS_DEBUG` is
    // the inherent `true` exactly when `T: Debug`.
    trait NotDebug {
        const IS_DEBUG: bool = false;
    }
    struct Probe<T: ?Sized>(std::marker::PhantomData<T>);
    impl<T: ?Sized> NotDebug for Probe<T> {}
    #[allow(dead_code)]
    impl<T: ?Sized + std::fmt::Debug> Probe<T> {
        const IS_DEBUG: bool = true;
    }

    /// Checked at compile time: deriving `Debug` on any of these breaks the
    /// test build.
    #[test]
    fn no_store_type_implements_debug() {
        const { assert!(!Probe::<AuthData>::IS_DEBUG) };
        const { assert!(!Probe::<User>::IS_DEBUG) };
        const { assert!(!Probe::<Credential>::IS_DEBUG) };
        const { assert!(!Probe::<CredentialKind>::IS_DEBUG) };
        const { assert!(!Probe::<Session>::IS_DEBUG) };
        const { assert!(!Probe::<Invite>::IS_DEBUG) };
        const { assert!(!Probe::<AuditEntry>::IS_DEBUG) };
        // And the probe does see Debug when it is there.
        const { assert!(Probe::<String>::IS_DEBUG) };
    }

    fn sample() -> AuthData {
        let mut d = AuthData::default();
        let id = Uuid::new_v4();
        d.users.push(User {
            id,
            name: "W1ABC".into(),
            display_name: "Ann".into(),
            role: Role::Admin,
            created: 10,
            last_sign_in: None,
            totp: TotpFailures::default(),
        });
        d.credentials.push(Credential {
            id: "c1".into(),
            user_id: id,
            nickname: "Phone app".into(),
            created: 11,
            last_used: None,
            kind: CredentialKind::Totp {
                secret: "JBSWY3DPEHPK3PXP".into(),
                last_step: 0,
            },
        });
        d
    }

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        let Loaded::Ok(back) = load(dir.path()) else {
            panic!("loads")
        };
        assert_eq!(back.users.len(), 1);
        assert_eq!(back.user_by_name("w1abc").unwrap().display_name, "Ann");
        assert!(matches!(
            back.credentials[0].kind,
            CredentialKind::Totp { .. }
        ));
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let Loaded::Ok(d) = load(dir.path()) else {
            panic!("loads")
        };
        assert!(d.users.is_empty());
        assert_eq!(d.version, STORE_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn created_0600_in_a_0700_directory_and_stays_so() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("state");
        save(&dir, &sample()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&store_path(&dir)), 0o600);
        assert_eq!(mode(&dir.join(LOCK_FILE)), 0o600);
        let mut d = sample();
        d.users[0].display_name = "Changed".into();
        save(&dir, &d).unwrap();
        assert_eq!(mode(&store_path(&dir)), 0o600, "a rewrite keeps 0600");
        assert!(!dir.join("auth.json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_readable_file_disables_sign_in() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        std::fs::set_permissions(
            store_path(dir.path()),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let Loaded::Disabled(why) = load(dir.path()) else {
            panic!("refused")
        };
        assert!(why.contains("chmod 600"), "{why}");
        assert!(!why.contains("JBSWY3DP"), "nothing from the file: {why}");
    }

    #[test]
    fn an_unknown_version_disables_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = sample();
        d.version = 2;
        save(dir.path(), &d).unwrap();
        let Loaded::Disabled(why) = load(dir.path()) else {
            panic!("refused")
        };
        assert!(why.contains("version 2"), "{why}");
    }

    #[test]
    fn a_broken_file_disables_sign_in_without_quoting_it() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        let p = store_path(dir.path());
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("\"users\"", "\"users\" JBSWY3DP")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let Loaded::Disabled(why) = load(dir.path()) else {
            panic!("refused")
        };
        assert!(why.contains("does not parse"), "{why}");
        assert!(!why.contains("JBSWY3DP"), "{why}");
    }

    #[test]
    fn a_writer_waits_for_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock(dir.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let path = dir.path().to_path_buf();
        let t = std::thread::spawn(move || {
            save(&path, &sample()).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the save waited for the lock"
        );
        drop(held);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("and finished once it was free");
        t.join().unwrap();
    }

    fn failure(at: u64, ip: &str) -> AuditEntry {
        AuditEntry {
            at,
            event: "sign_in_failed".into(),
            user: None,
            detail: "authenticator app".into(),
            ip: ip.into(),
            times: 0,
            since: None,
        }
    }

    #[test]
    fn failures_from_one_source_roll_up() {
        let mut d = AuthData::default();
        for i in 0..50 {
            d.audit_failure(failure(100 + i, "2001:db8:1:2::/64"));
        }
        assert_eq!(d.audit.len(), 1);
        let e = d.audit.back().unwrap();
        assert_eq!((e.times, e.at), (50, 149));
        d.audit_failure(failure(100 + ROLLUP_SECS + 1, "2001:db8:1:2::/64"));
        assert_eq!(d.audit.len(), 2, "a new window, a new entry");
    }

    #[test]
    fn anonymous_failures_cannot_flush_the_ring() {
        let mut d = AuthData::default();
        d.audit(AuditEntry {
            at: 1,
            event: "user_delete".into(),
            user: Some("BOSS".into()),
            detail: "ALICE".into(),
            ip: String::new(),
            times: 0,
            since: None,
        });
        for i in 0..(AUDIT_CAP as u64 * 3) {
            d.audit_failure(failure(2 + i, &format!("10.0.{}.{}", i / 250, i % 250)));
        }
        assert!(d.audit.len() <= AUDIT_CAP);
        assert!(
            d.audit.iter().any(|e| e.event == "user_delete"),
            "the deletion is still on record"
        );
    }

    #[test]
    fn the_audit_ring_is_capped() {
        let mut d = AuthData::default();
        for i in 0..(AUDIT_CAP as u64 + 10) {
            d.audit(AuditEntry {
                at: i,
                event: "sign_in".into(),
                user: None,
                detail: String::new(),
                ip: String::new(),
                times: 0,
                since: None,
            });
        }
        assert_eq!(d.audit.len(), AUDIT_CAP);
        assert_eq!(d.audit.front().unwrap().at, 10, "oldest dropped");
    }
}
