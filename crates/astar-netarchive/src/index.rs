// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The archive as the recordings page reads it: which sessions exist, who
//! may see each one, and the slugs that name them in URLs.
//!
//! A URL never becomes a path. [`Archive::find`] decodes the two URL
//! segments and compares them with the slugs of the folders [`Archive::scan`]
//! found; the path it hands back is the one the scan found on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::log::{LOG_FILE, read_rows};
use crate::model::{LogRow, SessionMeta};
use crate::peaks::{Peaks, compute_peaks, read_peaks};
use crate::retain::find_sessions;
use crate::summary::OverRec;

/// Who is looking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Not signed in: public sessions only.
    Anonymous,
    /// Signed in: every session.
    Viewer,
    /// Signed in, and may delete.
    Admin,
}

impl Access {
    /// Whether this viewer may see the session at all.
    #[must_use]
    pub fn may_see(self, meta: &SessionMeta) -> bool {
        match self {
            Access::Anonymous => is_public(meta),
            Access::Viewer | Access::Admin => true,
        }
    }
}

/// A session is public only when its `visibility` is exactly `"public"`.
#[must_use]
pub fn is_public(meta: &SessionMeta) -> bool {
    meta.visibility == "public"
}

/// A folder name as it appears in a URL: spaces become `-`.
#[must_use]
pub fn slug(dir_name: &str) -> String {
    dir_name.replace(' ', "-")
}

/// One session the scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub net_slug: String,
    pub session_slug: String,
    /// The session folder, under the archive's resolved root.
    pub dir: PathBuf,
    pub meta: SessionMeta,
}

/// `log.jsonl`'s length and modification time: what a cached value was
/// computed from.
type Stamp = (u64, SystemTime);

/// The archive under one root, with the in-memory caches the page needs.
#[derive(Debug)]
pub struct Archive {
    root: PathBuf,
    peaks: Mutex<HashMap<PathBuf, (u64, SystemTime, Peaks)>>,
    checkins: Mutex<HashMap<PathBuf, (u64, SystemTime, u32)>>,
}

/// Percent-decode one URL path segment. `None` for an invalid escape, text
/// that is not UTF-8, or anything that could name another folder: `/`, `\`,
/// `..` or a NUL.
pub(crate) fn decode_segment(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = char::from(*b.get(i + 1)?).to_digit(16)?;
            let lo = char::from(*b.get(i + 2)?).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(out).ok()?;
    if text.is_empty() || text.contains(['/', '\\', '\0']) || text.contains("..") {
        return None;
    }
    Some(text)
}

fn log_stamp(dir: &Path) -> Stamp {
    std::fs::symlink_metadata(dir.join(LOG_FILE)).map_or((0, SystemTime::UNIX_EPOCH), |m| {
        (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH))
    })
}

impl Archive {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            peaks: Mutex::new(HashMap::new()),
            checkins: Mutex::new(HashMap::new()),
        }
    }

    /// The root as it was given.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every session whose `session.json` parses. A net or session folder
    /// that is a symlink is skipped, as retention skips it.
    #[must_use]
    pub fn scan(&self) -> Vec<SessionEntry> {
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let found = match find_sessions(&root) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("cannot list the archive at {}: {e}", root.display());
                return Vec::new();
            }
        };
        found
            .into_iter()
            .filter_map(|f| {
                let meta = f.meta?;
                let net = f.net_dir.file_name()?.to_str()?;
                let session = f.dir.file_name()?.to_str()?;
                Some(SessionEntry {
                    net_slug: slug(net),
                    session_slug: slug(session),
                    dir: f.dir,
                    meta,
                })
            })
            .collect()
    }

    /// The session named by two URL segments, each still percent-encoded.
    /// A decoded segment is slugged too, so the folder's own name
    /// (`2026-11-03%201855`) finds it as well as its slug does. `None` when either segment is refused by the decoder, nothing matches,
    /// or more than one folder has these slugs.
    #[must_use]
    pub fn find(&self, net: &str, session: &str) -> Option<SessionEntry> {
        let net = slug(&decode_segment(net)?);
        let session = slug(&decode_segment(session)?);
        let mut hits = self
            .scan()
            .into_iter()
            .filter(|e| e.net_slug == net && e.session_slug == session);
        let hit = hits.next()?;
        hits.next().is_none().then_some(hit)
    }

    /// [`Archive::find`], then [`Access::may_see`]: the one lookup the web
    /// layer makes. A session `access` may not see is `None`, exactly as a
    /// missing one is, so no answer can tell the two apart.
    #[must_use]
    pub fn find_for(&self, access: Access, net: &str, session: &str) -> Option<SessionEntry> {
        self.find(net, session).filter(|e| access.may_see(&e.meta))
    }

    /// The session's log rows; empty when the log cannot be read.
    #[must_use]
    pub fn rows(&self, e: &SessionEntry) -> Vec<LogRow> {
        read_rows(&e.dir).unwrap_or_else(|err| {
            tracing::warn!("cannot read the log of {}: {err}", e.dir.display());
            Vec::new()
        })
    }

    /// `peaks.json`, else peaks computed from the over files and kept in
    /// memory until `log.jsonl` changes. Nothing is written to the archive.
    #[must_use]
    pub fn peaks(&self, e: &SessionEntry, overs: &[OverRec]) -> Peaks {
        if let Ok(Some(p)) = read_peaks(&e.dir) {
            return p;
        }
        let (len, mtime) = log_stamp(&e.dir);
        {
            let cache = self
                .peaks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((l, m, p)) = cache.get(&e.dir)
                && *l == len
                && *m == mtime
            {
                return p.clone();
            }
        }
        // Computed with the lock released: an hour of audio takes seconds
        // in a debug build, and other sessions must not wait behind it. Two
        // requests for the same session may both compute; the second insert
        // wins, and both hold the same answer.
        let p = compute_peaks(&e.dir, overs);
        self.peaks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(e.dir.clone(), (len, mtime, p.clone()));
        p
    }

    /// How many distinct base callsigns spoke, kept in memory until
    /// `log.jsonl` changes.
    #[must_use]
    pub fn checkins(&self, e: &SessionEntry) -> u32 {
        let (len, mtime) = log_stamp(&e.dir);
        let mut cache = self
            .checkins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((l, m, n)) = cache.get(&e.dir)
            && *l == len
            && *m == mtime
        {
            return *n;
        }
        let mut seen: Vec<String> = Vec::new();
        for o in crate::summary::overs_from_rows(&self.rows(e)) {
            if let Some(b) = o.talker.as_deref().and_then(crate::callsign::base_call)
                && !seen.contains(&b)
            {
                seen.push(b);
            }
        }
        let n = u32::try_from(seen.len()).unwrap_or(u32::MAX);
        cache.insert(e.dir.clone(), (len, mtime, n));
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::write_fixture;
    use crate::log::{read_meta, write_meta};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) fn temp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "netarchive-index-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn find_decodes_percent_and_refuses_tricks() {
        let root = temp("find");
        let f = write_fixture(&root);
        let a = Archive::new(root.clone());
        let hit = a.find(&f.public_net, "2026-11-03%201855").unwrap();
        assert_eq!(hit.session_slug, f.public_session);
        let hit = a.find(&f.public_net, &f.public_session).unwrap();
        assert_eq!(hit.meta.net, "Tuesday M17 net");
        // A folder whose name holds a space-free slug can be named with escapes.
        let hit = a.find("tuesday%2Dm17%2dnet", "2026-11-03-1855").unwrap();
        assert_eq!(hit.session_slug, "2026-11-03-1855");
        for bad in [
            "..", "%2e%2e", "%2E%2E", "a%2fb", "a%5cb", "%zz", "%2", "%", "",
        ] {
            assert!(a.find(bad, &f.public_session).is_none(), "{bad}");
            assert!(a.find(&f.public_net, bad).is_none(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slug_collision_finds_neither() {
        let root = temp("collide");
        let f = write_fixture(&root);
        // A second folder whose slug is the same as the public session's.
        let net = root.join("tuesday-m17-net");
        let copy = net.join("2026-11-03-1855");
        std::fs::create_dir_all(&copy).unwrap();
        let m = read_meta(&net.join("2026-11-03 1855")).unwrap();
        write_meta(&copy, &m).unwrap();
        let a = Archive::new(root.clone());
        assert!(a.find(&f.public_net, &f.public_session).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn access_sees_public_only_when_anonymous() {
        let root = temp("access");
        let f = write_fixture(&root);
        let a = Archive::new(root.clone());
        let public = a.find(&f.public_net, &f.public_session).unwrap();
        let private = a.find("thursday-d-star-net", &f.private_session).unwrap();
        assert!(Access::Anonymous.may_see(&public.meta));
        assert!(!Access::Anonymous.may_see(&private.meta));
        assert!(Access::Viewer.may_see(&private.meta));
        assert!(Access::Admin.may_see(&private.meta));
        let mut m = public.meta.clone();
        m.visibility = "Public".into();
        assert!(!is_public(&m));
        let _ = std::fs::remove_dir_all(&root);
    }
}
