// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Persisted state for the Discord slash-command approval scheme.
//!
//! This module is pure: no Discord, no network, no tokio, and (deliberately)
//! no knowledge of *why* a user is being approved, banned or muted — it only
//! remembers what was decided. The slash-command layer built on top of this
//! is what talks to Discord; this module's job is that a restart of the bot
//! does not forget who was approved.
//!
//! User ids are `u64` everywhere in this API because that is what Discord's
//! gateway hands the bridge, but they are stored on disk as `String`: JSON
//! numbers are IEEE-754 doubles, so a snowflake past 2^53 would silently
//! round-trip wrong if `serde_json` were allowed to write it as a number.
//! Every comparison therefore goes through [`u64::to_string`] rather than
//! parsing the stored strings back to `u64` — a value astar itself just
//! formatted is trusted without a second decode step.
//!
//! **The mutators are not symmetrical, and the asymmetry has already caused
//! one bug.** The removals — [`Approvals::revoke`], [`Approvals::unban`],
//! [`Approvals::remove_admin`] — return `Result<bool>` and save only AFTER
//! the removal has happened in memory, so their `Ok(false)` is the ONLY way
//! this module says "there was nothing to do", and their `Err` means "it
//! happened and could not be written down", never "it did not happen". A
//! caller that reads an `Err` as a failure to act reports the opposite of
//! the truth: the user really is revoked for this process, and only a
//! restart undoes it. The `Result<()>` mutators cannot express "nothing to
//! do" at all — [`Approvals::add_admin`] and [`Approvals::set_muted_by_us`]
//! return `Ok(())` both for a change they saved and for a no-op they
//! skipped saving, which is why a caller that needs to know whether
//! anything changed must ask the `is_*` predicates BEFORE calling. In every
//! case the in-memory state is already the new one by the time the result
//! is returned; the result is only ever about the file.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The `serde(default = "yes")` helper for [`ApprovalState::enabled`].
///
/// A config file written before this field existed — or one hand-edited down
/// to just `{"version":1}` — must come back enabled, not silently disabled;
/// silently disabling approval gating is the wrong failure direction for a
/// gate that exists to keep strangers off the net.
fn yes() -> bool {
    true
}

/// One person's entry in an approval, ban or pending list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserEntry {
    /// The Discord snowflake, as a string — see the module docs for why.
    pub user_id: String,
    /// The display name at the time this entry was recorded, purely for a
    /// human reading the approval list; never used for comparison.
    pub name: String,
    /// Who made this decision — an admin's user id, or empty for a decision
    /// this module made on its own (there are none yet, but the field is not
    /// optional on disk once it has shipped: an old file that predates it
    /// must still deserialize, hence `#[serde(default)]`).
    #[serde(default)]
    pub by: String,
    /// Unix seconds when this entry was recorded.
    pub at_unix: u64,
}

/// The whole persisted approval scheme for one Discord bridge.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalState {
    /// Always 1 today. [`Approvals::load`] refuses any other value rather
    /// than guess at a migration that does not exist yet — see the module
    /// name.
    pub version: u32,
    /// Whether the approval gate is enforced at all. Defaults to `true` on a
    /// file (or field) that predates this setting.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub approved: Vec<UserEntry>,
    #[serde(default)]
    pub banned: Vec<UserEntry>,
    #[serde(default)]
    pub pending: Vec<UserEntry>,
    /// Users allowed to run the admin-only slash commands (approve, ban,
    /// mute, …) themselves. Same entry shape as `approved` — an admin is
    /// still a person worth naming and dating, not just an id.
    /// `#[serde(default)]` so a file saved before this field existed still
    /// loads, with nobody an admin, rather than refusing as an unknown
    /// version.
    #[serde(default)]
    pub admins: Vec<UserEntry>,
    /// Ids this bridge itself server-muted (as opposed to a channel mute a
    /// Discord admin applied by hand). Kept as bare ids, not [`UserEntry`],
    /// because there is nothing else worth recording about a mute — who
    /// pressed it lives in the approve/ban history instead.
    #[serde(default)]
    pub muted_by_us: Vec<String>,
    /// Callsign by user id, recorded by `/register`. A fact about a person
    /// rather than about a list, which is why it is not a [`UserEntry`]
    /// field: it must survive pending → approved → revoked → approved, and
    /// an approver from `node.toml` — who is on no list at all — needs one
    /// too. Always stored uppercased and validated. `#[serde(default)]` so a
    /// file written before callsigns existed still loads, with nobody's
    /// known.
    #[serde(default)]
    pub callsigns: BTreeMap<String, String>,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            enabled: true,
            approved: Vec::new(),
            banned: Vec::new(),
            pending: Vec::new(),
            admins: Vec::new(),
            muted_by_us: Vec::new(),
            callsigns: BTreeMap::new(),
        }
    }
}

/// The only version [`Approvals::load`] accepts today.
const CURRENT_VERSION: u32 = 1;

/// Everything that can go wrong loading or saving an [`Approvals`] file.
#[derive(Debug)]
pub enum ApprovalsError {
    /// The file exists but could not be read (permissions, I/O error — never
    /// a missing file, which [`Approvals::load`] treats as empty state).
    Read(String),
    /// The file's bytes are not the JSON this module writes.
    Parse(String),
    /// The file parsed but names a version this module does not understand.
    Version(u32),
    /// The atomic save (write-tmp, `sync_all`, rename) failed at some step.
    Write(String),
}

impl std::fmt::Display for ApprovalsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(m) => write!(f, "could not read the approvals file: {m}"),
            Self::Parse(m) => write!(f, "could not parse the approvals file: {m}"),
            Self::Version(v) => write!(f, "approvals file version {v} is not understood"),
            Self::Write(m) => write!(f, "could not save the approvals file: {m}"),
        }
    }
}

impl std::error::Error for ApprovalsError {}

/// The persisted approval scheme, plus where (if anywhere) it lives on disk.
///
/// `path: None` is [`Approvals::in_memory`] — tests and any caller that does
/// not want persistence get the same mutators as everyone else, and every
/// save through them is a no-op `Ok(())` rather than a special case the
/// caller has to remember to avoid.
#[derive(Debug)]
pub struct Approvals {
    state: ApprovalState,
    path: Option<PathBuf>,
}

/// True if `id` is already present in `list`, compared as the string astar
/// itself would format it — see the module docs.
fn contains_id(list: &[UserEntry], id: &str) -> bool {
    list.iter().any(|u| u.user_id == id)
}

/// Removes every entry matching `id` from `list`. Returns whether anything
/// was actually removed, so callers (`revoke`, `unban`) can report it and
/// skip a pointless save when nothing changed.
fn remove_by_id(list: &mut Vec<UserEntry>, id: &str) -> bool {
    let before = list.len();
    list.retain(|u| u.user_id != id);
    list.len() != before
}

impl Approvals {
    /// An approval scheme with nothing persisted: every mutator still works,
    /// but `save` is a no-op. Used by callers (and most tests) that do not
    /// care about surviving a restart.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            state: ApprovalState::default(),
            path: None,
        }
    }

    /// Loads `path`. A missing file is not an error — a bridge with no
    /// approvals file yet is exactly the same as one with an empty, enabled
    /// state — but a present, unreadable, unparseable, or wrong-version file
    /// is: guessing at what a corrupt or future-versioned file meant is
    /// exactly the failure mode a *persisted* ban list cannot afford.
    ///
    /// # Errors
    /// [`ApprovalsError::Read`] if the file exists but could not be read
    /// (permissions, I/O error — not for a *missing* file, which is `Ok`).
    /// [`ApprovalsError::Parse`] if its bytes are not the JSON this module
    /// writes. [`ApprovalsError::Version`] if it parses but names a version
    /// other than the one this module understands.
    pub fn load(path: &Path) -> Result<Self, ApprovalsError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    state: ApprovalState::default(),
                    path: Some(path.to_path_buf()),
                });
            }
            Err(e) => return Err(ApprovalsError::Read(e.to_string())),
        };
        let state: ApprovalState =
            serde_json::from_slice(&bytes).map_err(|e| ApprovalsError::Parse(e.to_string()))?;
        if state.version != CURRENT_VERSION {
            return Err(ApprovalsError::Version(state.version));
        }
        Ok(Self {
            state,
            path: Some(path.to_path_buf()),
        })
    }

    #[must_use]
    pub fn state(&self) -> &ApprovalState {
        &self.state
    }

    #[must_use]
    pub fn is_approved(&self, id: u64) -> bool {
        contains_id(&self.state.approved, &id.to_string())
    }

    #[must_use]
    pub fn is_banned(&self, id: u64) -> bool {
        contains_id(&self.state.banned, &id.to_string())
    }

    #[must_use]
    pub fn is_pending(&self, id: u64) -> bool {
        contains_id(&self.state.pending, &id.to_string())
    }

    #[must_use]
    pub fn is_admin(&self, id: u64) -> bool {
        contains_id(&self.state.admins, &id.to_string())
    }

    #[must_use]
    pub fn muted_by_us(&self, id: u64) -> bool {
        self.state.muted_by_us.iter().any(|m| *m == id.to_string())
    }

    /// The callsign recorded for `id`, if they have ever registered one.
    #[must_use]
    pub fn callsign(&self, id: u64) -> Option<&str> {
        self.state
            .callsigns
            .get(&id.to_string())
            .map(String::as_str)
    }

    /// The id holding `callsign` (exact, uppercase), if any.
    ///
    /// An M17 reflector knows a dialler only by the callsign on the CONN,
    /// so one callsign recorded for two members would let either present the
    /// other's identity. `/register` asks this before recording anything.
    /// The match is exact because every stored value was already validated
    /// and uppercased. A key that is not a snowflake, which only a
    /// hand-edited file can hold, names nobody and is skipped.
    #[must_use]
    pub fn callsign_holder(&self, callsign: &str) -> Option<u64> {
        self.state
            .callsigns
            .iter()
            .filter(|(_, held)| held.as_str() == callsign)
            .find_map(|(id, _)| id.parse().ok())
    }

    /// Records `callsign` for `id`, replacing any earlier one. Stored as
    /// given: the caller has already validated and uppercased it. A call
    /// that changes nothing does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the change could not be saved. The
    /// in-memory record stands regardless, as for every other mutator here.
    pub fn set_callsign(&mut self, id: u64, callsign: String) -> Result<(), ApprovalsError> {
        let id = id.to_string();
        if self.state.callsigns.get(&id) == Some(&callsign) {
            return Ok(());
        }
        self.state.callsigns.insert(id, callsign);
        self.save()
    }

    /// Switches the approval gate on or off. What "off" should mean to a
    /// caller — let everyone through, or refuse everyone — is policy this
    /// module has no opinion on; it only remembers the switch's position.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the change could not be saved. The
    /// in-memory switch has already moved and stays moved either way —
    /// only persistence failed, so the new value governs this process until
    /// (and unless) it restarts.
    pub fn set_enabled(&mut self, on: bool) -> Result<(), ApprovalsError> {
        self.state.enabled = on;
        self.save()
    }

    /// No-op (and no save) if `u.user_id` is already pending — repeated
    /// requests from the same person must not pile up duplicate entries or
    /// touch disk for nothing.
    ///
    /// This does **not** consult `approved` or `banned` — a user already
    /// approved, or already banned, can still be queued as pending by this
    /// call. Keeping those checks out of this module on purpose: the caller
    /// (the slash-command layer, which knows the request came from someone
    /// astar has never decided about) is the one place that gets to decide
    /// what a pending request from an already-approved or already-banned id
    /// means, and it must check `is_approved`/`is_banned` itself before
    /// calling this.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the change could not be saved. The
    /// in-memory pending entry stands regardless — a later retry of the
    /// same save is what recovers it, not a retry of `add_pending` itself
    /// (which would now see the entry already pending and no-op).
    pub fn add_pending(&mut self, u: UserEntry) -> Result<(), ApprovalsError> {
        if contains_id(&self.state.pending, &u.user_id) {
            return Ok(());
        }
        self.state.pending.push(u);
        self.save()
    }

    /// Approves `u`, replacing any earlier approval entry for the same id
    /// and clearing any pending request — the two lists are alternatives,
    /// never both true at once.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the change could not be saved. The
    /// in-memory approval (and the removal from `pending`) stand regardless
    /// — this call already changed what `is_approved`/`is_pending` report,
    /// and only telling the caller "will not survive a restart" is left to
    /// the returned error.
    pub fn approve(&mut self, u: UserEntry) -> Result<(), ApprovalsError> {
        remove_by_id(&mut self.state.pending, &u.user_id);
        remove_by_id(&mut self.state.approved, &u.user_id);
        self.state.approved.push(u);
        self.save()
    }

    /// Removes `id` from the approved list. Returns whether they were
    /// approved at all; a no-op revoke does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if `id` was approved and the removal could
    /// not be saved. The in-memory removal stands regardless — `is_approved`
    /// already reports `false` — the error only means it may not survive a
    /// restart. Never returned for a no-op revoke, since nothing was saved.
    pub fn revoke(&mut self, id: u64) -> Result<bool, ApprovalsError> {
        let id = id.to_string();
        let removed = remove_by_id(&mut self.state.approved, &id);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Bans `u`: removed from approved, pending and admins (a banned user is
    /// none of those), then recorded in the banned list. Stripping admin
    /// status here — rather than requiring a separate `remove_admin` first —
    /// is what keeps a ban an unconditional "this person is out", not a ban
    /// that a leftover admin entry could partially undo.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the change could not be saved. The
    /// in-memory ban (and the removals from approved/pending/admins) stand
    /// regardless — `is_banned` already reports `true` — the error only
    /// means it may not survive a restart.
    pub fn ban(&mut self, u: UserEntry) -> Result<(), ApprovalsError> {
        remove_by_id(&mut self.state.approved, &u.user_id);
        remove_by_id(&mut self.state.pending, &u.user_id);
        remove_by_id(&mut self.state.admins, &u.user_id);
        remove_by_id(&mut self.state.banned, &u.user_id);
        self.state.banned.push(u);
        self.save()
    }

    /// Removes `id` from the banned list. Returns whether they were banned
    /// at all; a no-op unban does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if `id` was banned and the removal could
    /// not be saved. The in-memory removal stands regardless — `is_banned`
    /// already reports `false` — the error only means it may not survive a
    /// restart. Never returned for a no-op unban, since nothing was saved.
    pub fn unban(&mut self, id: u64) -> Result<bool, ApprovalsError> {
        let id = id.to_string();
        let removed = remove_by_id(&mut self.state.banned, &id);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Grants `u` admin status. Idempotent: adding the same id again
    /// replaces nothing and adds no duplicate entry, and (since nothing
    /// changed) does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if `u` was newly added and the change could
    /// not be saved. The in-memory grant stands regardless — `is_admin`
    /// already reports `true` — the error only means it may not survive a
    /// restart. Never returned for a repeat grant, since nothing was saved.
    pub fn add_admin(&mut self, u: UserEntry) -> Result<(), ApprovalsError> {
        if contains_id(&self.state.admins, &u.user_id) {
            return Ok(());
        }
        self.state.admins.push(u);
        self.save()
    }

    /// Removes `id` from the admins list. Returns whether they were an admin
    /// at all; a no-op removal does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if `id` was an admin and the removal could
    /// not be saved. The in-memory removal stands regardless — `is_admin`
    /// already reports `false` — the error only means it may not survive a
    /// restart. Never returned for a no-op removal, since nothing was saved.
    pub fn remove_admin(&mut self, id: u64) -> Result<bool, ApprovalsError> {
        let id = id.to_string();
        let removed = remove_by_id(&mut self.state.admins, &id);
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Records (or clears) that this bridge itself server-muted `id`. A
    /// no-op — muting an already-muted id, or un-muting one that was not
    /// muted — does not touch disk.
    ///
    /// # Errors
    /// [`ApprovalsError::Write`] if the mute state actually changed and the
    /// change could not be saved. The in-memory state stands regardless —
    /// `muted_by_us` already reports the new value — the error only means it
    /// may not survive a restart. Never returned for a no-op call, since
    /// nothing was saved.
    pub fn set_muted_by_us(&mut self, id: u64, muted: bool) -> Result<(), ApprovalsError> {
        let id = id.to_string();
        let changed = if muted {
            if self.state.muted_by_us.contains(&id) {
                false
            } else {
                self.state.muted_by_us.push(id);
                true
            }
        } else {
            let before = self.state.muted_by_us.len();
            self.state.muted_by_us.retain(|m| *m != id);
            self.state.muted_by_us.len() != before
        };
        if changed { self.save() } else { Ok(()) }
    }

    /// Writes the current state to `path` atomically: serialize, write to a
    /// sibling `<path>.tmp`, `sync_all` before the descriptor closes, then
    /// `rename` over `path`. The rename is what makes this atomic — a reader
    /// (or a crash) never observes a half-written file, only the old
    /// complete one or the new complete one. `in_memory()` (`path: None`)
    /// saves are `Ok(())` no-ops so every mutator can call this unconditionally.
    fn save(&self) -> Result<(), ApprovalsError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_vec_pretty(&self.state)
            .map_err(|e| ApprovalsError::Write(e.to_string()))?;

        // `<path>.tmp` lives in the SAME directory as `path`, which is what
        // makes the final `rename` a same-filesystem rename rather than a
        // copy — the property that makes it atomic in the first place.
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);

        let mut file =
            std::fs::File::create(&tmp_path).map_err(|e| ApprovalsError::Write(e.to_string()))?;
        file.write_all(&json)
            .map_err(|e| ApprovalsError::Write(e.to_string()))?;
        // Flushed to storage before the rename, not just to the OS page
        // cache: a rename that lands before the data behind it does would
        // let a crash expose a zero-length (or truncated) file under `path`.
        file.sync_all()
            .map_err(|e| ApprovalsError::Write(e.to_string()))?;
        drop(file);

        std::fs::rename(&tmp_path, path).map_err(|e| ApprovalsError::Write(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path unique to this process and this test, in the system temp dir.
    /// Not cleaned up automatically at start — `std::process::id()` already
    /// makes it unique per test run, so two runs never collide, and a stray
    /// leftover from a killed test run is harmless: every test that cares
    /// about starting empty removes it first.
    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("astar-approvals-{}-{name}", std::process::id()))
    }

    fn tmp_file(path: &Path) -> PathBuf {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tmp_file(path));
    }

    #[test]
    fn callsigns_persist_and_a_file_without_them_still_loads() {
        let path = tmp_path("callsigns");
        cleanup(&path);

        // A version-1 file from before callsigns existed: no `callsigns`
        // key at all. It must load, with nobody's callsign known.
        std::fs::write(&path, br#"{"version":1,"approved":[]}"#).expect("fixture written");
        let mut approvals = Approvals::load(&path).expect("an older file loads");
        assert_eq!(approvals.callsign(7), None);

        approvals
            .set_callsign(7, "AJ7HR".to_string())
            .expect("saving a callsign");
        assert_eq!(approvals.callsign(7), Some("AJ7HR"));
        assert_eq!(
            approvals.callsign(8),
            None,
            "a callsign is per person, not per file"
        );

        // Replaced, not duplicated — and it comes back from disk.
        approvals
            .set_callsign(7, "W1AW".to_string())
            .expect("replacing a callsign");
        let mut reloaded = Approvals::load(&path).expect("reload");
        assert_eq!(reloaded.callsign(7), Some("W1AW"));
        assert_eq!(reloaded.state().callsigns.len(), 1);

        // The same value again is a no-op on disk: the file's mtime does
        // not move.
        let before = std::fs::metadata(&path)
            .expect("file")
            .modified()
            .expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        reloaded
            .set_callsign(7, "W1AW".to_string())
            .expect("no-op save");
        let after = std::fs::metadata(&path)
            .expect("file")
            .modified()
            .expect("mtime");
        assert_eq!(
            before, after,
            "an unchanged callsign must not rewrite the file"
        );

        cleanup(&path);
    }

    #[test]
    fn callsign_holder_finds_the_id_holding_an_exact_callsign() {
        let mut a = Approvals::in_memory();
        assert_eq!(
            a.callsign_holder("AJ7HR"),
            None,
            "nobody holds anything yet"
        );

        a.set_callsign(7, "AJ7HR".to_string())
            .expect("an in-memory save cannot fail");
        a.set_callsign(8, "W1AW".to_string())
            .expect("an in-memory save cannot fail");
        assert_eq!(a.callsign_holder("AJ7HR"), Some(7));
        assert_eq!(a.callsign_holder("W1AW"), Some(8));
        assert_eq!(
            a.callsign_holder("aj7hr"),
            None,
            "stored values are uppercase and the caller normalises first"
        );
        assert_eq!(a.callsign_holder("AJ7H"), None, "exact, never a prefix");

        // A key that is not a snowflake (only a hand-edited file holds one)
        // names nobody and is skipped, not the end of the search. "0x7"
        // sorts before "7" in the BTreeMap, so a `find` that stopped at the
        // first match and then failed to parse would return None here.
        a.state
            .callsigns
            .insert("0x7".to_string(), "AJ7HR".to_string());
        assert_eq!(a.callsign_holder("AJ7HR"), Some(7));

        // A replaced callsign is free again, and the new one is held.
        a.set_callsign(7, "K1ABC".to_string())
            .expect("an in-memory save cannot fail");
        assert_eq!(
            a.callsign_holder("AJ7HR"),
            None,
            "only the unparseable key still holds it, and it names nobody"
        );
        assert_eq!(a.callsign_holder("K1ABC"), Some(7));
    }

    #[test]
    fn missing_file_is_empty_and_enabled() {
        let path = tmp_path("missing");
        cleanup(&path);

        let approvals = Approvals::load(&path).expect("a missing file must not be an error");
        assert!(
            approvals.state().approved.is_empty(),
            "a bridge with no approvals file yet has approved nobody"
        );
        assert!(
            approvals.state().enabled,
            "a missing file must not be misread as the gate being off"
        );

        cleanup(&path);
    }

    #[test]
    fn round_trip_survives_reload() {
        let path = tmp_path("round-trip");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let entry = UserEntry {
            user_id: "42".to_string(),
            name: "kd0xyz".to_string(),
            by: "7".to_string(),
            at_unix: 1_726_000_000,
        };
        approvals
            .approve(entry.clone())
            .expect("the temp directory is writable");

        let reloaded = Approvals::load(&path).expect("the file just saved must load back");
        assert_eq!(
            reloaded.state().approved.first(),
            Some(&entry),
            "the reloaded entry must equal, field for field, the one that was approved"
        );

        cleanup(&path);
    }

    #[test]
    fn approving_twice_keeps_one_entry_and_the_newer_details() {
        let path = tmp_path("approve-twice");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        approvals
            .approve(UserEntry {
                user_id: "42".to_string(),
                name: "old".to_string(),
                by: "1".to_string(),
                at_unix: 1,
            })
            .expect("the temp directory is writable");
        approvals
            .approve(UserEntry {
                user_id: "42".to_string(),
                name: "new".to_string(),
                by: "2".to_string(),
                at_unix: 2,
            })
            .expect("the temp directory is writable");

        assert_eq!(
            approvals.state().approved.len(),
            1,
            "approving the same id twice must not create a second entry"
        );
        let entry = &approvals.state().approved[0];
        assert_eq!(
            entry.name, "new",
            "the surviving entry must carry the newer name, not the stale one"
        );
        assert_eq!(
            entry.by, "2",
            "the surviving entry must carry the newer approver, not the stale one"
        );

        cleanup(&path);
    }

    #[test]
    fn corrupt_file_is_an_error_not_an_empty_state() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, b"{not json").expect("the temp directory is writable");

        let result = Approvals::load(&path);
        assert!(
            matches!(result, Err(ApprovalsError::Parse(_))),
            "unparseable bytes must be reported, not silently treated as no approvals: got {result:?}"
        );

        cleanup(&path);
    }

    #[test]
    fn unknown_version_is_refused() {
        let path = tmp_path("unknown-version");
        std::fs::write(&path, br#"{"version":2}"#).expect("the temp directory is writable");

        let result = Approvals::load(&path);
        assert!(
            matches!(result, Err(ApprovalsError::Version(2))),
            "a file naming a version this module does not understand must be refused: got {result:?}"
        );

        cleanup(&path);
    }

    #[test]
    fn ban_removes_approval_and_pending() {
        let path = tmp_path("ban");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let approved = UserEntry {
            user_id: "42".to_string(),
            name: "already-approved".to_string(),
            by: "7".to_string(),
            at_unix: 1,
        };
        let pending = UserEntry {
            user_id: "43".to_string(),
            name: "still-pending".to_string(),
            by: String::new(),
            at_unix: 2,
        };
        approvals
            .approve(approved.clone())
            .expect("the temp directory is writable");
        approvals
            .add_pending(pending.clone())
            .expect("the temp directory is writable");

        approvals
            .ban(approved)
            .expect("the temp directory is writable");
        approvals
            .ban(pending)
            .expect("the temp directory is writable");

        assert!(
            !approvals.is_approved(42),
            "banning a previously-approved user must revoke the approval"
        );
        assert!(
            !approvals.is_pending(43),
            "banning a pending user must clear the pending request"
        );
        assert!(approvals.is_banned(42), "42 must now be banned");
        assert!(approvals.is_banned(43), "43 must now be banned");

        cleanup(&path);
    }

    #[test]
    fn ban_also_removes_admin_status() {
        let path = tmp_path("ban-admin");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let admin = UserEntry {
            user_id: "9".to_string(),
            name: "rogue-admin".to_string(),
            by: "1".to_string(),
            at_unix: 1,
        };
        approvals
            .add_admin(admin.clone())
            .expect("the temp directory is writable");
        assert!(
            approvals.is_admin(9),
            "setup: the admin grant must have taken"
        );

        approvals
            .ban(admin)
            .expect("the temp directory is writable");

        assert!(
            !approvals.is_admin(9),
            "banning an admin must revoke their admin status too, not just leave them banned alongside it"
        );
        assert!(
            approvals.is_banned(9),
            "the ban itself must still take effect"
        );

        cleanup(&path);
    }

    #[test]
    fn add_admin_twice_keeps_one_entry() {
        let path = tmp_path("admin-idempotent");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let admin = UserEntry {
            user_id: "9".to_string(),
            name: "kd0xyz".to_string(),
            by: "1".to_string(),
            at_unix: 1,
        };
        approvals
            .add_admin(admin.clone())
            .expect("the temp directory is writable");
        approvals
            .add_admin(admin)
            .expect("a repeat grant must not error");

        assert_eq!(
            approvals.state().admins.len(),
            1,
            "adding the same admin twice must not create a duplicate entry"
        );

        cleanup(&path);
    }

    #[test]
    fn admins_round_trip_and_old_files_without_the_key_still_load() {
        let path = tmp_path("admins-legacy");
        // A version-1 file from before `admins` existed: no such key at all.
        std::fs::write(&path, br#"{"version":1}"#).expect("the temp directory is writable");

        let loaded = Approvals::load(&path)
            .expect("a v1 file missing the admins key must still load, not be refused");
        assert!(
            loaded.state().admins.is_empty(),
            "a file predating admins must read as nobody being an admin"
        );

        let mut approvals = loaded;
        approvals
            .add_admin(UserEntry {
                user_id: "9".to_string(),
                name: "kd0xyz".to_string(),
                by: "1".to_string(),
                at_unix: 1,
            })
            .expect("the temp directory is writable");

        let reloaded = Approvals::load(&path).expect("the file just saved must load back");
        assert!(
            reloaded.is_admin(9),
            "an admin grant must survive being saved and reloaded"
        );

        cleanup(&path);
    }

    #[test]
    fn failed_write_keeps_the_in_memory_change_and_the_old_file() {
        let path = tmp_path("failed-write");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let user1 = UserEntry {
            user_id: "1".to_string(),
            name: "one".to_string(),
            by: String::new(),
            at_unix: 1,
        };
        approvals
            .approve(user1)
            .expect("the first save has a real, writable parent directory");
        let bytes_after_first_save =
            std::fs::read(&path).expect("the first save must have produced a file");

        // A parent directory that does not exist: `File::create` for the
        // `.tmp` sibling must fail before anything is written, let alone
        // renamed.
        let bad_path = std::env::temp_dir()
            .join(format!(
                "astar-approvals-{}-no-such-dir",
                std::process::id()
            ))
            .join("x.json");
        // Constructed directly rather than through a public constructor:
        // `state`/`path` are private to this module, and the test module is
        // a child of it, so this is the same access a `#[cfg(test)]`
        // constructor would grant — without adding one more thing the real
        // API has to carry.
        let mut broken = Approvals {
            state: approvals.state().clone(),
            path: Some(bad_path),
        };
        let user2 = UserEntry {
            user_id: "2".to_string(),
            name: "two".to_string(),
            by: String::new(),
            at_unix: 2,
        };
        let result = broken.approve(user2);
        assert!(
            matches!(result, Err(ApprovalsError::Write(_))),
            "a save into a directory that does not exist must fail as Write: got {result:?}"
        );
        assert!(
            broken.is_approved(2),
            "the in-memory approval must stand even though the save could not"
        );

        let bytes_after_failed_save =
            std::fs::read(&path).expect("the original file must still be there, untouched");
        assert_eq!(
            bytes_after_first_save, bytes_after_failed_save,
            "a failed save to a different path must not alter the earlier good file"
        );

        cleanup(&path);
    }

    #[test]
    fn no_tmp_file_is_left_behind() {
        let path = tmp_path("no-tmp-leftover");
        cleanup(&path);

        let mut approvals = Approvals::load(&path).expect("a missing file loads as empty state");
        let entry = UserEntry {
            user_id: "1".to_string(),
            name: "one".to_string(),
            by: String::new(),
            at_unix: 1,
        };
        approvals
            .approve(entry)
            .expect("the temp directory is writable");

        assert!(
            !tmp_file(&path).exists(),
            "a successful save must rename the .tmp file away, not leave it sitting next to the real one"
        );

        cleanup(&path);
    }
}
