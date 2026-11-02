// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Streams recordings and unamblify captures into a tar on any `Write`
//! (an HTTP response, later), one file at a time.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::capture::CaptureRow;
use crate::clock::{parse_rfc3339, unix_secs};

/// One file on disk and the name it takes inside the tar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarEntry {
    pub path_in_tar: String,
    pub file: PathBuf,
}

/// One file a download plans to send: the name it takes inside the tar,
/// where it is on disk, and the folders that must still be real folders
/// (not symlinks) when it is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEntry {
    pub path_in_tar: String,
    pub file: PathBuf,
    pub real_dirs: Vec<PathBuf>,
}

/// One line of a ledger (`index.jsonl`, a `manifest.jsonl`), sent only if
/// every entry it names (`needs`, indices into [`DownloadPlan::entries`])
/// made it into the tar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLine {
    pub needs: Vec<usize>,
    /// The line itself, without its newline.
    pub bytes: Vec<u8>,
}

/// A ledger written after the files, in memory, listing only what was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLedger {
    pub path_in_tar: String,
    pub lines: Vec<PlannedLine>,
    /// Write it even when no line survives (the recordings index).
    pub keep_if_empty: bool,
}

/// What a download will send. Made by listing names only, so planning
/// holds no file open; [`write_tar_planned`] opens each file just before
/// writing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadPlan {
    pub entries: Vec<PlannedEntry>,
    pub ledgers: Vec<PlannedLedger>,
}

/// What [`write_tar_planned`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TarSummary {
    /// Files written.
    pub sent: usize,
    /// Files skipped because they had gone (or stopped being plain files)
    /// since the plan was made.
    pub skipped: usize,
}

/// Opens `p` only if it is a regular file and not a symlink: checked with
/// `symlink_metadata` first, then confirmed on the open handle (on Unix the
/// device and inode must match, so a swap between the two is refused).
/// The open is [`open_nofollow`]. Returns the handle and the size its own
/// metadata gives. `Ok(None)` when the path is missing, is not a plain
/// file, is a symlink, or may not be read.
///
/// # Errors
/// Any other I/O error (too many open files).
pub fn open_regular(p: &Path) -> io::Result<Option<(File, u64)>> {
    let link = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !link.file_type().is_file() {
        return Ok(None);
    }
    let Some(file) = open_nofollow(p)? else {
        return Ok(None);
    };
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (meta.dev(), meta.ino()) != (link.dev(), link.ino()) {
            return Ok(None);
        }
    }
    Ok(Some((file, meta.len())))
}

/// Opens `p` for reading without following a symlink at its last
/// component (`O_NOFOLLOW`) and without waiting on a FIFO or a device
/// swapped in after the check (`O_NONBLOCK`; a regular file ignores it).
/// `Ok(None)` for a file that is gone, a symlink (`ELOOP`), or one this
/// process may not read (`EACCES`, `EPERM`): one entry to skip, not a
/// reason to abandon a whole download.
///
/// # Errors
/// Any other I/O error.
pub fn open_nofollow(p: &Path) -> io::Result<Option<File>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    match opts.open(p) {
        Ok(f) => Ok(Some(f)),
        Err(e) if skippable(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// An open error that means "this one file is not ours to send".
fn skippable(e: &io::Error) -> bool {
    if matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    ) {
        return true;
    }
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    false
}

/// A tar name must be relative and made only of plain components: no
/// leading `/`, no `..`, no `.`, nothing empty.
fn check_tar_path(p: &str) -> io::Result<()> {
    let plain = !p.is_empty()
        && !p.starts_with('/')
        && !p.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        && Path::new(p)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if plain {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe path in tar: {p:?}"),
        ))
    }
}

/// Writes `entries` (copied from disk with `append_file`, so only one read
/// buffer is ever held) and then `manifests` (held in memory, as they are
/// rebuilt) as one tar onto `out`. Every name is checked before the first
/// byte goes out.
pub fn write_tar<W: Write>(
    out: W,
    entries: &[TarEntry],
    manifests: &[(String, Vec<u8>)],
) -> io::Result<()> {
    for e in entries {
        check_tar_path(&e.path_in_tar)?;
    }
    for (p, _) in manifests {
        check_tar_path(p)?;
    }
    let mut b = tar::Builder::new(out);
    b.mode(tar::HeaderMode::Deterministic);
    for e in entries {
        let file = File::open(&e.file)?;
        let meta = file.metadata()?;
        append_exact(&mut b, &e.path_in_tar, &meta, meta.len(), file)?;
    }
    finish_tar(b, manifests)
}

/// Streams `plan` as one tar onto `out`, one file open at a time. Each
/// file is opened just before it is written, through [`open_regular`]
/// (never a symlink, never a folder that turned into one), and its header
/// is written only after that open succeeded, at the size the open handle
/// reports. A file that has vanished since planning (pruned, deleted) is
/// skipped cleanly. The ledgers come last and list only the lines whose
/// files were all sent.
///
/// # Errors
/// An unsafe name, an I/O error writing `out`, or an error opening or
/// reading a file other than it being gone.
pub fn write_tar_planned<W: Write>(out: W, plan: &DownloadPlan) -> io::Result<TarSummary> {
    for e in &plan.entries {
        check_tar_path(&e.path_in_tar)?;
    }
    for l in &plan.ledgers {
        check_tar_path(&l.path_in_tar)?;
    }
    let mut b = tar::Builder::new(out);
    b.mode(tar::HeaderMode::Deterministic);
    let mut sent = vec![false; plan.entries.len()];
    let mut summary = TarSummary {
        sent: 0,
        skipped: 0,
    };
    for (i, e) in plan.entries.iter().enumerate() {
        let dirs_ok = e
            .real_dirs
            .iter()
            .all(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.file_type().is_dir()));
        let opened = if dirs_ok {
            open_regular(&e.file)?
        } else {
            None
        };
        let Some((file, size)) = opened else {
            summary.skipped += 1;
            continue;
        };
        let meta = file.metadata()?;
        append_exact(&mut b, &e.path_in_tar, &meta, size, file)?;
        sent[i] = true;
        summary.sent += 1;
    }
    let mut ledgers = Vec::new();
    for l in &plan.ledgers {
        let mut text = Vec::new();
        for line in &l.lines {
            if line
                .needs
                .iter()
                .all(|&i| sent.get(i).copied().unwrap_or(false))
            {
                text.extend_from_slice(&line.bytes);
                text.push(b'\n');
            }
        }
        if !text.is_empty() || l.keep_if_empty {
            ledgers.push((l.path_in_tar.clone(), text));
        }
    }
    finish_tar(b, &ledgers)?;
    Ok(summary)
}

fn finish_tar<W: Write>(mut b: tar::Builder<W>, manifests: &[(String, Vec<u8>)]) -> io::Result<()> {
    let mtime = u64::try_from(unix_secs(SystemTime::now())).unwrap_or(0);
    for (p, bytes) in manifests {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(mtime);
        b.append_data(&mut h, p, bytes.as_slice())?;
    }
    b.into_inner()?.flush()
}

/// Reads exactly `left` bytes from `inner`; running dry first is an error,
/// so a file that shrank mid-copy fails the tar instead of corrupting it.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let cap = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let n = self.inner.read(&mut buf[..cap])?;
        if n == 0 && cap > 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shrank while it was being archived",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Appends `data` under `size`, the size taken when the file was opened
/// (`meta` gives only the mode and time): a file that grows is cut at that size, one that shrinks is an error. The
/// header is deterministic (no uid, gid or owner names).
fn append_exact<W: Write, R: Read>(
    b: &mut tar::Builder<W>,
    name: &str,
    meta: &std::fs::Metadata,
    size: u64,
    data: R,
) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_metadata_in_mode(meta, tar::HeaderMode::Deterministic);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(size);
    b.append_data(
        &mut h,
        name,
        Exact {
            inner: data,
            left: size,
        },
    )
}

/// A name we wrote ourselves: `[A-Za-z0-9_.+-]`, not starting with `.`.
fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// A regular file, not a symlink or a folder.
fn is_regular(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

/// Every capture under `root/captured/*/manifest.jsonl` whose `captured_at`
/// is at or after `since` (all of them for `None`), as tar entries plus one
/// rebuilt manifest per mode listing only the rows whose `.ambe` and
/// `.flac` both made it in. Kept rows are copied byte for byte. A row that
/// does not parse, has an unreadable `captured_at` (when `since` is given),
/// or names a key outside `astar-air/`, is left out.
#[allow(clippy::type_complexity)] // the signature the server's download handler consumes
pub fn capture_entries(
    root: &Path,
    since: Option<SystemTime>,
) -> io::Result<(Vec<TarEntry>, Vec<(String, Vec<u8>)>)> {
    let captured = root.join("captured");
    let rd = match std::fs::read_dir(&captured) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e),
    };
    let mut modes = Vec::new();
    for e in rd {
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = e.file_name().to_str().filter(|n| plain_name(n)) {
            modes.push(name.to_owned());
        }
    }
    modes.sort();

    let (mut entries, mut manifests) = (Vec::new(), Vec::new());
    for mode in modes {
        let mode_dir = captured.join(&mode);
        let manifest = mode_dir.join("manifest.jsonl");
        if !is_regular(&manifest) {
            continue;
        }
        let text = std::fs::read_to_string(&manifest)?;
        let mut kept = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(row) = serde_json::from_str::<CaptureRow>(line) else {
                continue;
            };
            if let Some(since) = since
                && parse_rfc3339(&row.captured_at).is_none_or(|t| t < since)
            {
                continue;
            }
            let Some(stem) = row.key.strip_prefix("astar-air/").filter(|s| plain_name(s)) else {
                continue;
            };
            let files = ["ambe", "flac"].map(|ext| {
                (
                    format!("captured/{mode}/astar-air/{stem}.{ext}"),
                    mode_dir.join("astar-air").join(format!("{stem}.{ext}")),
                )
            });
            if !files.iter().all(|(_, f)| is_regular(f)) {
                continue;
            }
            for (path_in_tar, file) in files {
                entries.push(TarEntry { path_in_tar, file });
            }
            kept.extend_from_slice(line.as_bytes());
            kept.push(b'\n');
        }
        if !kept.is_empty() {
            manifests.push((format!("captured/{mode}/manifest.jsonl"), kept));
        }
    }
    Ok((entries, manifests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{append_row, tests::row};
    use crate::clock::parse_rfc3339;
    use std::collections::BTreeMap;
    use std::io::Read;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-tar-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One capture: its two files and its manifest row.
    fn capture(root: &Path, mode: &str, stem: &str, captured_at: &str) {
        let air = root.join("captured").join(mode).join("astar-air");
        std::fs::create_dir_all(&air).unwrap();
        std::fs::write(air.join(format!("{stem}.ambe")), format!("ambe {stem}")).unwrap();
        std::fs::write(air.join(format!("{stem}.flac")), format!("flac {stem}")).unwrap();
        let mut r = row(&format!("astar-air/{stem}"));
        r.mode = mode.to_owned();
        r.captured_at = captured_at.to_owned();
        append_row(&root.join("captured").join(mode).join("manifest.jsonl"), &r).unwrap();
    }

    fn untar(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut a = tar::Archive::new(bytes);
        let mut out = BTreeMap::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            let p = e.path().unwrap().to_string_lossy().into_owned();
            let mut body = Vec::new();
            e.read_to_end(&mut body).unwrap();
            out.insert(p, body);
        }
        out
    }

    #[test]
    fn tar_holds_files_and_filtered_manifests() {
        let r = root("filtered");
        capture(
            &r,
            "dstar",
            "20261026-190000-dstar-W1ABC-rx",
            "2026-10-26T19:00:00Z",
        );
        capture(
            &r,
            "dstar",
            "20261026-210000-dstar-K2XYZ-rx",
            "2026-10-26T21:00:00Z",
        );
        let since = parse_rfc3339("2026-10-26T20:00:00Z");
        let (entries, manifests) = capture_entries(&r, since).unwrap();
        let mut out = Vec::new();
        write_tar(&mut out, &entries, &manifests).unwrap();
        let files = untar(&out);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "captured/dstar/astar-air/20261026-210000-dstar-K2XYZ-rx.ambe",
                "captured/dstar/astar-air/20261026-210000-dstar-K2XYZ-rx.flac",
                "captured/dstar/manifest.jsonl",
            ]
        );
        assert_eq!(
            files["captured/dstar/astar-air/20261026-210000-dstar-K2XYZ-rx.ambe"],
            b"ambe 20261026-210000-dstar-K2XYZ-rx"
        );
        let manifest = String::from_utf8(files["captured/dstar/manifest.jsonl"].clone()).unwrap();
        assert_eq!(manifest.lines().count(), 1);
        assert!(manifest.contains("astar-air/20261026-210000-dstar-K2XYZ-rx"));
        assert!(manifest.ends_with('\n'));

        // With no `since`, both rows and all four files.
        let (all, m) = capture_entries(&r, None).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(m.len(), 1);
        assert_eq!(String::from_utf8_lossy(&m[0].1).lines().count(), 2);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn tar_drops_rows_whose_files_are_missing() {
        let r = root("missing");
        capture(
            &r,
            "dstar",
            "20261026-190000-dstar-W1ABC-rx",
            "2026-10-26T19:00:00Z",
        );
        capture(
            &r,
            "dstar",
            "20261026-190100-dstar-W1ABC-rx",
            "2026-10-26T19:01:00Z",
        );
        std::fs::remove_file(
            r.join("captured/dstar/astar-air/20261026-190000-dstar-W1ABC-rx.flac"),
        )
        .unwrap();
        let (entries, manifests) = capture_entries(&r, None).unwrap();
        assert_eq!(entries.len(), 2);
        let text = String::from_utf8(manifests[0].1.clone()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("190100"));
        assert_eq!(
            capture_entries(&root("empty"), None).unwrap().0,
            [] as [crate::tarball::TarEntry; 0]
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_file_that_shrinks_mid_copy_fails_the_tar() {
        let r = root("shrink");
        let f = r.join("a.flac");
        std::fs::write(&f, vec![7u8; 4000]).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        // Stat said 4000 bytes; by the time it is copied only 100 are left.
        let mut b = tar::Builder::new(Vec::new());
        let e = append_exact(&mut b, "a.flac", &meta, meta.len(), &[7u8; 100][..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // One that grew is cut at the size it had, and the tar is sound.
        let mut b = tar::Builder::new(Vec::new());
        append_exact(&mut b, "a.flac", &meta, meta.len(), &[7u8; 9000][..]).unwrap();
        let files = untar(&b.into_inner().unwrap());
        assert_eq!(files["a.flac"].len(), 4000);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn tar_headers_carry_no_owner() {
        let r = root("owner");
        capture(
            &r,
            "dstar",
            "20261026-190000-dstar-W1ABC-rx",
            "2026-10-26T19:00:00Z",
        );
        let (entries, manifests) = capture_entries(&r, None).unwrap();
        let mut out = Vec::new();
        write_tar(&mut out, &entries, &manifests).unwrap();
        let mut a = tar::Archive::new(&out[..]);
        for e in a.entries().unwrap() {
            let e = e.unwrap();
            let h = e.header();
            assert_eq!((h.uid().unwrap(), h.gid().unwrap()), (0, 0));
            assert_eq!(h.username().unwrap().unwrap_or(""), "");
        }
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn tar_paths_never_leave_the_archive_root() {
        let r = root("paths");
        capture(
            &r,
            "dstar",
            "20261026-190000-dstar-W1ABC-rx",
            "2026-10-26T19:00:00Z",
        );
        capture(
            &r,
            "codec2-3200",
            "20261026-190000-m17-W1ABC-tx",
            "2026-10-26T19:00:00Z",
        );
        // A row whose key tries to climb out of the archive is skipped.
        let mut evil = row("../../../etc/passwd");
        evil.captured_at = "2026-10-26T19:00:00Z".to_owned();
        append_row(&r.join("captured/dstar/manifest.jsonl"), &evil).unwrap();
        let mut evil2 = row("astar-air/../../secret");
        evil2.captured_at = "2026-10-26T19:00:00Z".to_owned();
        append_row(&r.join("captured/dstar/manifest.jsonl"), &evil2).unwrap();

        let (entries, manifests) = capture_entries(&r, None).unwrap();
        assert_eq!(entries.len(), 4);
        let mut out = Vec::new();
        write_tar(&mut out, &entries, &manifests).unwrap();
        let files = untar(&out);
        assert_eq!(files.len(), 6);
        for p in files.keys() {
            assert!(!p.starts_with('/'), "{p}");
            assert!(
                !p.split('/').any(|c| c == ".." || c == "." || c.is_empty()),
                "{p}"
            );
            assert!(p.starts_with("captured/"), "{p}");
        }
        for m in manifests
            .iter()
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
        {
            assert!(!m.contains("passwd") && !m.contains("secret"));
        }

        // write_tar itself refuses an unsafe name before writing anything.
        let file = entries[0].file.clone();
        for bad in ["../x", "/etc/x", "a/../../x", "", "./x"] {
            let mut sink = Vec::new();
            let e = [TarEntry {
                path_in_tar: bad.to_owned(),
                file: file.clone(),
            }];
            assert!(write_tar(&mut sink, &e, &[]).is_err(), "{bad:?}");
            assert!(
                write_tar(&mut sink, &[], &[(bad.to_owned(), vec![1])]).is_err(),
                "{bad:?}"
            );
            assert!(sink.is_empty(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&r);
    }

    fn planned(r: &Path, names: &[&str]) -> DownloadPlan {
        let entries: Vec<PlannedEntry> = names
            .iter()
            .map(|n| PlannedEntry {
                path_in_tar: format!("recordings/{n}"),
                file: r.join(n),
                real_dirs: vec![r.to_path_buf()],
            })
            .collect();
        let lines = (0..names.len())
            .map(|i| PlannedLine {
                needs: vec![i],
                bytes: format!("row {}", names[i]).into_bytes(),
            })
            .collect();
        DownloadPlan {
            entries,
            ledgers: vec![PlannedLedger {
                path_in_tar: "recordings/index.jsonl".into(),
                lines,
                keep_if_empty: true,
            }],
        }
    }

    #[test]
    fn a_planned_tar_skips_vanished_files_and_lists_only_what_it_sent() {
        let r = root("planned");
        for n in ["a.flac", "b.flac", "c.flac"] {
            std::fs::write(r.join(n), format!("body of {n}")).unwrap();
        }
        let plan = planned(&r, &["a.flac", "b.flac", "c.flac"]);
        std::fs::remove_file(r.join("b.flac")).unwrap();
        let mut out = Vec::new();
        let summary = write_tar_planned(&mut out, &plan).unwrap();
        assert_eq!(
            summary,
            TarSummary {
                sent: 2,
                skipped: 1
            }
        );
        let files = untar(&out);
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "recordings/a.flac",
                "recordings/c.flac",
                "recordings/index.jsonl"
            ]
        );
        assert_eq!(files["recordings/a.flac"], b"body of a.flac");
        assert_eq!(files["recordings/c.flac"], b"body of c.flac");
        assert_eq!(files["recordings/index.jsonl"], b"row a.flac\nrow c.flac\n");
        // The index is the last entry.
        let mut a = tar::Archive::new(out.as_slice());
        let last = a.entries().unwrap().last().unwrap().unwrap();
        assert_eq!(
            last.path().unwrap().to_str().unwrap(),
            "recordings/index.jsonl"
        );
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_planned_tar_refuses_a_symlink_or_a_swapped_folder() {
        let r = root("planned-links");
        let outside = root("planned-links-outside");
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), r.join("a.flac")).unwrap();
        let day = r.join("day");
        std::os::unix::fs::symlink(&outside, &day).unwrap();
        let mut plan = planned(&r, &["a.flac"]);
        plan.entries.push(PlannedEntry {
            path_in_tar: "recordings/day/secret".into(),
            file: day.join("secret"),
            real_dirs: vec![day.clone()],
        });
        let mut out = Vec::new();
        let summary = write_tar_planned(&mut out, &plan).unwrap();
        assert_eq!(
            summary,
            TarSummary {
                sent: 0,
                skipped: 2
            }
        );
        let files = untar(&out);
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec!["recordings/index.jsonl"]
        );
        assert_eq!(files["recordings/index.jsonl"], [] as [u8; 0]);
        std::fs::remove_dir_all(&r).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// Open descriptors of this process, where the platform shows them.
    fn open_fds() -> Option<usize> {
        ["/dev/fd", "/proc/self/fd"]
            .iter()
            .find_map(|d| std::fs::read_dir(d).ok().map(Iterator::count))
    }

    /// Counts the process's open descriptors on every write.
    struct FdWatch {
        out: Vec<u8>,
        max: usize,
    }

    impl Write for FdWatch {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(n) = open_fds() {
                self.max = self.max.max(n);
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn two_thousand_files_stream_with_one_open_at_a_time() {
        let r = root("many");
        let names: Vec<String> = (0..2000).map(|i| format!("f{i:04}.flac")).collect();
        for n in &names {
            std::fs::write(r.join(n), n.as_bytes()).unwrap();
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let plan = planned(&r, &refs);
        let base = open_fds();
        let mut w = FdWatch {
            out: Vec::new(),
            max: 0,
        };
        let summary = write_tar_planned(&mut w, &plan).unwrap();
        assert_eq!(summary.sent, 2000);
        if let Some(base) = base {
            // One file at a time, plus whatever other tests in this process
            // have open meanwhile: nowhere near one per file.
            assert!(
                w.max < base + 100,
                "{} fds open at peak, {base} before",
                w.max
            );
        }
        let files = untar(&w.out);
        assert_eq!(files.len(), 2001);
        assert_eq!(files["recordings/f1999.flac"], b"f1999.flac");
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn open_nofollow_skips_a_symlink_and_an_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let r = root("nofollow");
        std::fs::write(r.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(r.join("real"), r.join("link")).unwrap();
        // The open itself refuses the link (ELOOP), not only the stat before it.
        assert!(open_nofollow(&r.join("link")).unwrap().is_none());
        assert!(open_nofollow(&r.join("real")).unwrap().is_some());
        std::fs::write(r.join("locked"), b"secret").unwrap();
        std::fs::set_permissions(r.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads through mode 000; the skip is only observable otherwise.
        let unreadable = File::open(r.join("locked")).is_err();
        if unreadable {
            assert!(open_nofollow(&r.join("locked")).unwrap().is_none());
            assert!(open_regular(&r.join("locked")).unwrap().is_none());
        }
        // A FIFO is never opened (O_NONBLOCK keeps a swap from hanging).
        let fifo = r.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(open_regular(&fifo).unwrap().is_none());
        std::fs::set_permissions(r.join("locked"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_planned_tar_skips_an_unreadable_file_instead_of_aborting() {
        use std::os::unix::fs::PermissionsExt;
        let r = root("planned-locked");
        for n in ["a.flac", "b.flac"] {
            std::fs::write(r.join(n), format!("body of {n}")).unwrap();
        }
        std::fs::set_permissions(r.join("a.flac"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(r.join("a.flac")).is_ok() {
            // Running as root: mode 000 does not stop the read.
            std::fs::remove_dir_all(&r).unwrap();
            return;
        }
        let plan = planned(&r, &["a.flac", "b.flac"]);
        let mut out = Vec::new();
        let summary = write_tar_planned(&mut out, &plan).unwrap();
        assert_eq!(
            summary,
            TarSummary {
                sent: 1,
                skipped: 1
            }
        );
        let files = untar(&out);
        assert_eq!(files["recordings/index.jsonl"], b"row b.flac\n");
        assert!(!files.contains_key("recordings/a.flac"));
        std::fs::set_permissions(r.join("a.flac"), std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn open_regular_refuses_symlinks_folders_and_missing_files() {
        let r = root("open-regular");
        std::fs::write(r.join("real"), b"x").unwrap();
        assert!(open_regular(&r.join("real")).unwrap().is_some());
        assert!(open_regular(&r.join("missing")).unwrap().is_none());
        assert!(open_regular(&r).unwrap().is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(r.join("real"), r.join("link")).unwrap();
            assert!(open_regular(&r.join("link")).unwrap().is_none());
        }
        std::fs::remove_dir_all(&r).unwrap();
    }
}
