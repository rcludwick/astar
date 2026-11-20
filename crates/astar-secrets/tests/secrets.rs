// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The credentials file and the store, through the public API only. These
//! are astar-server's file tests, moved here with the file code; astar-server
//! keeps its own copies, which go through `SecretProvider`.

use std::path::{Path, PathBuf};

use astar_secrets::{
    Secret, SecretStore, keys, load_private, load_private_with, remove, replace_all, save,
};

const HEADER: &str = "# test credentials — written by a test. Mode 0600.\n";

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("credentials.toml");
    (dir, path)
}

/// Write `text` as a file only its owner can read, so a test about parsing
/// is not tripped by the mode check.
fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn secret_debug_never_shows_the_value() {
    assert_eq!(format!("{:?}", Secret::new("abc123".into())), "Secret(***)");
    let s = Secret::new("abc123".into());
    assert_eq!(s.clone().expose(), "abc123");
}

#[test]
fn a_missing_file_loads_none_and_lists_no_keys() {
    let (_dir, path) = scratch();
    assert_eq!(load_private(&path, "the node"), Ok(None));
    assert_eq!(keys(&path), Ok(vec![]));
}

#[test]
fn save_creates_the_file_with_its_header_and_load_reads_it_back() {
    let (_dir, path) = scratch();
    save(&path, HEADER, "node", "pw-one").unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{HEADER}node = \"pw-one\"\n")
    );
    assert_eq!(
        load_private(&path, "the node"),
        Ok(Some(pairs(&[("node", "pw-one")])))
    );
}

#[test]
fn awkward_values_round_trip_and_the_header_survives_a_second_save() {
    let (_dir, path) = scratch();
    let awkward = r#"a"b\c#d 'e' "#;
    save(&path, HEADER, "portal", awkward).unwrap();
    save(&path, HEADER, "link:2000", "ünïcødé ☕").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(HEADER), "{text}");
    assert!(text.lines().any(|l| l == "\"link:2000\" = \"ünïcødé ☕\""));
    let mut got = load_private(&path, "the node").unwrap().unwrap();
    got.sort();
    assert_eq!(
        got,
        pairs(&[("link:2000", "ünïcødé ☕"), ("portal", awkward)])
    );
    assert_eq!(
        keys(&path),
        Ok(vec!["link:2000".to_owned(), "portal".to_owned()])
    );
}

#[cfg(unix)]
#[test]
fn the_file_is_created_mode_0600_and_stays_so_across_a_rewrite() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path) = scratch();
    save(&path, HEADER, "node", "pw").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!path.with_file_name("credentials.toml.tmp").exists());
    save(&path, HEADER, "portal", "pp").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn a_group_readable_file_is_refused_with_the_owner_in_the_sentence() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path) = scratch();
    save(&path, HEADER, "yt", "key-leaky-SENTINEL").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let shown = path.display();
    let err = load_private(&path, "astar-netrec").unwrap_err();
    assert_eq!(
        err,
        format!(
            "credentials file {shown} is readable by group or others (mode 0o640); astar-netrec did not load it — run: chmod 600 {shown}"
        )
    );
    assert!(!err.contains("SENTINEL"));
    // Keys stay listable, and a save heals the mode.
    assert_eq!(keys(&path), Ok(vec!["yt".to_owned()]));
    save(&path, HEADER, "tw", "x").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        load_private(&path, "astar-netrec").unwrap().unwrap().len(),
        2
    );
}

#[test]
fn a_file_that_does_not_parse_is_refused_without_quoting_its_contents() {
    let (_dir, path) = scratch();
    write_private(&path, "node = \"SENTINEL-9f3a\nportal = \n");
    let err = load_private(&path, "the node").unwrap_err();
    assert_eq!(
        err,
        format!(
            "credentials file {} does not parse; fix or delete it and set the credentials again from the settings page",
            path.display()
        )
    );
    assert!(!err.contains("SENTINEL"));
    assert_eq!(save(&path, HEADER, "portal", "x").unwrap_err(), err);
    assert_eq!(keys(&path).unwrap_err(), err);
}

#[test]
fn a_non_string_value_is_refused_by_key_name() {
    let (_dir, path) = scratch();
    write_private(&path, "node = 5\n");
    assert_eq!(
        load_private(&path, "the node").unwrap_err(),
        format!(
            "credentials file {}: key \"node\" is not a string",
            path.display()
        )
    );
}

#[test]
fn removing_the_first_key_keeps_the_header_and_a_later_save_does_not_double_it() {
    let (_dir, path) = scratch();
    save(&path, HEADER, "node", "pw").unwrap();
    // Absent file and absent key: Ok, nothing written.
    let nope = path.with_file_name("nope.toml");
    remove(&nope, HEADER, "node").unwrap();
    assert!(!nope.exists());
    remove(&path, HEADER, "portal").unwrap();

    remove(&path, HEADER, "node").unwrap();
    assert_eq!(keys(&path), Ok(vec![]));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), HEADER);
    save(&path, HEADER, "portal", "pp").unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{HEADER}portal = \"pp\"\n")
    );
}

#[test]
fn replace_all_writes_exactly_the_entries_given() {
    let (_dir, path) = scratch();
    save(&path, HEADER, "old", "gone").unwrap();
    assert_eq!(
        replace_all(&path, HEADER, &pairs(&[("a", "1"), ("b", "2")])),
        Ok(2)
    );
    assert_eq!(keys(&path), Ok(vec!["a".to_owned(), "b".to_owned()]));
    assert!(std::fs::read_to_string(&path).unwrap().starts_with(HEADER));
}

#[test]
fn a_failed_write_leaves_the_old_file_untouched() {
    let (dir, path) = scratch();
    save(&path, HEADER, "node", "pw").unwrap();
    let before = std::fs::read(&path).unwrap();
    std::fs::create_dir(dir.path().join("credentials.toml.tmp")).unwrap();
    let err = save(&path, HEADER, "portal", "SENTINEL-portal-pw").unwrap_err();
    assert!(
        err.starts_with(&format!("cannot write {}: ", path.display())),
        "{err}"
    );
    assert!(!err.contains("SENTINEL"), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn a_failed_rename_removes_the_temp_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("credentials.toml");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("x"), "").unwrap();
    let err =
        replace_all(&target, HEADER, &pairs(&[("portal", "SENTINEL-rename-pw")])).unwrap_err();
    assert!(
        err.starts_with(&format!("cannot write {}: ", target.display())),
        "{err}"
    );
    assert!(!err.contains("SENTINEL"), "{err}");
    assert!(!dir.path().join("credentials.toml.tmp").exists());
    assert!(target.join("x").exists());
}

#[test]
fn the_store_is_shared_between_clones() {
    let a = SecretStore::default();
    let b = a.clone();
    a.put("zeta", "1");
    a.put("alpha", "2");
    assert_eq!(b.get("zeta").as_deref(), Some("1"));
    assert_eq!(b.get("nobody"), None);
    assert_eq!(b.usernames(), vec!["alpha".to_owned(), "zeta".to_owned()]);
    assert_eq!(b.entries(), pairs(&[("alpha", "2"), ("zeta", "1")]));
    b.remove("zeta");
    b.remove("never-there");
    assert_eq!(a.usernames(), vec!["alpha".to_owned()]);
}

#[test]
fn a_caller_can_name_where_to_fix_an_unparseable_file() {
    let (_dir, path) = scratch();
    write_private(&path, "node = \"SENTINEL-9f3a\nportal = \n");
    let err = load_private_with(&path, "astar-netrec", "in credentials.toml").unwrap_err();
    assert_eq!(
        err,
        format!(
            "credentials file {} does not parse; fix or delete it and set the credentials again in credentials.toml",
            path.display()
        )
    );
    assert!(!err.contains("SENTINEL"));
}
