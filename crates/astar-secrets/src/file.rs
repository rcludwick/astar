// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The credentials file: flat TOML, one string per key, mode 0600.
//!
//! Every write is a read-modify-write through `toml_edit`, so the header, the
//! other entries and any hand edit survive, and lands through a 0600 temp
//! file in the same directory renamed over the target: a crash leaves the old
//! file or the new one, and there is no moment at which a value sits in a
//! file anyone else can read.
//!
//! The header is the caller's: the first line of every file it writes.
//! `toml_edit` renders a fresh document's root prefix first, but on re-parse
//! it hangs a leading comment on the FIRST KEY's prefix, not the root table —
//! so removing that key would take the header with it — and a header with no
//! key after it at all parses into the document's *trailing* text, which
//! renders LAST. `write_document` covers both.

use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

/// The temp file's name when `path` has no file name of its own.
const FALLBACK_FILE_NAME: &str = "credentials.toml";

/// Where astar-server's parse refusal sends the reader.
const SETTINGS_PAGE: &str = "from the settings page";

/// Every `key = "string"` pair in the file at `path`, in file order.
/// `Ok(None)` when the file does not exist.
///
/// `Err` when the file cannot be read, does not parse, holds a value that is
/// not a string, or is readable by group or others — that last one is exactly
/// `credentials file <path> is readable by group or others (mode 0o644);
/// <owner> did not load it — run: chmod 600 <path>`. No value ever appears in
/// an error.
pub fn load_private(path: &Path, owner: &str) -> Result<Option<Vec<(String, String)>>, String> {
    load_private_with(path, owner, SETTINGS_PAGE)
}

/// [`load_private`] for a caller with no settings page: `again` finishes the
/// parse refusal's sentence, `... set the credentials again <again>`, e.g.
/// `"in credentials.toml"`.
pub fn load_private_with(
    path: &Path,
    owner: &str,
    again: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    if !exists_and_is_private(path, owner)? {
        return Ok(None);
    }
    let Some(doc) = read_document(path, again)? else {
        return Ok(None);
    };
    string_entries(path, &doc).map(Some)
}

/// Set one key, creating the file (with `header` as its first line) when
/// absent. `Err` is `cannot write <path>: <io error>`, or the parse refusal
/// [`load_private`] gives, and never carries the value.
pub fn save(path: &Path, header: &str, key: &str, value: &str) -> Result<(), String> {
    let mut doc = read_document(path, SETTINGS_PAGE)?.unwrap_or_else(|| new_document(header));
    doc.insert(key, toml_edit::value(value));
    write_document(path, header, doc)
}

/// Remove one key. `Ok(())` when the file or the key is absent, and nothing
/// is rewritten in that case. Same atomic write as [`save`], which also keeps
/// the header on the file when the key removed was the first one.
pub fn remove(path: &Path, header: &str, key: &str) -> Result<(), String> {
    let Some(mut doc) = read_document(path, SETTINGS_PAGE)? else {
        return Ok(());
    };
    if doc.remove(key).is_none() {
        return Ok(());
    }
    write_document(path, header, doc)
}

/// The keys in the file, sorted, values discarded. `Ok(vec![])` when the file
/// is absent. The mode is deliberately not checked, so a caller can still
/// show which keys exist on a file it refused to load.
pub fn keys(path: &Path) -> Result<Vec<String>, String> {
    let Some(doc) = read_document(path, SETTINGS_PAGE)? else {
        return Ok(Vec::new());
    };
    let mut keys: Vec<String> = doc.iter().map(|(k, _)| k.to_owned()).collect();
    keys.sort();
    Ok(keys)
}

/// Replace the whole file with `entries`, in the order given, under
/// `header`. The file is not read first. Returns the number of distinct keys
/// written (a repeated key keeps its last value).
pub fn replace_all(
    path: &Path,
    header: &str,
    entries: &[(String, String)],
) -> Result<usize, String> {
    let mut doc = new_document(header);
    for (key, value) in entries {
        doc.insert(key, toml_edit::value(value.as_str()));
    }
    let written = doc.len();
    write_document(path, header, doc)?;
    Ok(written)
}

/// A fresh document carrying the header as the root table's leading decor.
/// That is where it renders from on the first write; after a re-parse it
/// rides on the first key's prefix instead, which `write_document` covers.
fn new_document(header: &str) -> DocumentMut {
    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix(header);
    doc
}

/// `Ok(true)` when the file exists and only its owner can read it,
/// `Ok(false)` when it does not exist. On non-unix targets existence is all
/// there is to check.
fn exists_and_is_private(path: &Path, owner: &str) -> Result<bool, String> {
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
                "credentials file {shown} is readable by group or others (mode 0o{mode:03o}); {owner} did not load it — run: chmod 600 {shown}"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (meta, owner);
    Ok(true)
}

/// Parse the file. `Ok(None)` when it does not exist. A parse failure is
/// reported without the parser's own message: `toml_edit` prints the
/// offending line under a caret, and every line of this file is a secret.
fn read_document(path: &Path, again: &str) -> Result<Option<DocumentMut>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let doc = text.parse::<DocumentMut>().map_err(|_| {
        format!(
            "credentials file {} does not parse; fix or delete it and set the credentials again {again}",
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
    let mut name = path
        .file_name()
        .map_or_else(|| FALLBACK_FILE_NAME.into(), std::ffi::OsStr::to_os_string);
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
/// is the header alone (after the last key was removed) parses with the
/// header in the document's *trailing* text, which `toml_edit` renders
/// LAST — so the next key would render above it and a second header would
/// be prepended. The trailing copy is dropped first, then the root prefix
/// is set whenever the rendered text does not already start with the header.
fn write_document(path: &Path, header: &str, mut doc: DocumentMut) -> Result<(), String> {
    use std::io::Write;

    if doc
        .trailing()
        .as_str()
        .is_some_and(|t| t.contains(header.trim_end()))
    {
        doc.set_trailing("");
    }
    if !doc.to_string().starts_with(header) {
        doc.decor_mut().set_prefix(header);
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
