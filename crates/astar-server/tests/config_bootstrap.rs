// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration tests for config bootstrap (iax-4703 Task 9).
//!
//! `NodeFileConfig::load_or_bootstrap` is the shared load path used by both
//! `serve` and `tui` (both dispatch through the same `main.rs` load before
//! branching on subcommand). Contract:
//!   - missing `--config` path: write a commented template there (creating
//!     parent dirs as needed), log prominently, then load the just-written
//!     file and return it — never generate-and-exit.
//!   - existing path: read + parse, byte-untouched.
//!   - unwritable parent: a clear error naming the path.
//!
//! Also covers the template's own round-trip: it must parse to safe defaults
//! (listener up, no registration, no live secret) with every commented
//! section staying commented.

use astar_server::config::NodeFileConfig;
use astar_server::template::NODE_TOML_TEMPLATE;

// ---------------------------------------------------------------------------
// Template round-trip
// ---------------------------------------------------------------------------

#[test]
fn template_parses_to_safe_defaults() {
    let cfg = NodeFileConfig::from_toml_str(NODE_TOML_TEMPLATE).expect("template must parse");

    assert_eq!(cfg.listener.bind, "0.0.0.0:4569");
    assert_eq!(cfg.listener.answer, "auto");
    assert_eq!(cfg.listener.max_calls, 8);
    assert_eq!(cfg.listener.auth, "off");

    assert!(
        cfg.register.is_none(),
        "[register] must stay commented out in the template"
    );
    assert!(
        cfg.audio.is_none(),
        "[audio] must stay commented out in the template"
    );
    assert!(
        cfg.announce.is_none(),
        "[announce] must stay commented out in the template"
    );

    assert_eq!(cfg.control.bind, "127.0.0.1:8730");

    assert_eq!(cfg.secrets.source, "config");
    assert_eq!(
        cfg.secrets.secret.as_deref(),
        Some(""),
        "template ships an empty (warn, not error) secret placeholder"
    );

    // Round-trip through the higher-level builders `main.rs` actually calls.
    let inbound = cfg.to_inbound().expect("inbound builds from the template");
    let expected_bind: std::net::SocketAddr = "0.0.0.0:4569".parse().unwrap();
    assert_eq!(inbound.bind, expected_bind);
    assert!(cfg.to_register().expect("register builds").is_none());

    let bridge = cfg.to_bridge_config().expect("bridge builds");
    assert_eq!(bridge.mode, astar_iax::BridgeMode::Bridge);
    assert!(bridge.mix_minus);
    assert!(!bridge.include_local_radio);
}

// ---------------------------------------------------------------------------
// Bootstrap-on-missing
// ---------------------------------------------------------------------------

#[test]
fn missing_path_generates_template_and_loads_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.toml");
    assert!(!path.exists(), "precondition: path must not exist yet");

    let cfg = NodeFileConfig::load_or_bootstrap(&path).expect("bootstrap must succeed");

    assert!(path.exists(), "template file must have been written");
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        written, NODE_TOML_TEMPLATE,
        "written file must match the template exactly"
    );

    // The returned config is the loaded template — safe defaults.
    assert_eq!(cfg.listener.bind, "0.0.0.0:4569");
    assert!(cfg.register.is_none());
    assert_eq!(cfg.control.bind, "127.0.0.1:8730");
    assert_eq!(cfg.secrets.source, "config");
}

#[test]
fn missing_path_creates_parent_dirs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("deeper").join("node.toml");
    assert!(!path.parent().unwrap().exists());

    let cfg = NodeFileConfig::load_or_bootstrap(&path);
    assert!(
        cfg.is_ok(),
        "bootstrap should create missing parent dirs: {:?}",
        cfg.err()
    );
    assert!(path.exists());
}

#[test]
fn existing_file_is_untouched_byte_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.toml");
    let original = "[listener]\n\
        bind = \"127.0.0.1:5000\"\n\
        answer = \"auto\"\n\
        max_calls = 1\n\
        auth = \"off\"\n\
        [control]\n\
        bind = \"127.0.0.1:9999\"\n\
        [secrets]\n\
        source = \"env\"\n";
    std::fs::write(&path, original).unwrap();

    let cfg = NodeFileConfig::load_or_bootstrap(&path).expect("existing file must load");
    assert_eq!(cfg.listener.bind, "127.0.0.1:5000");

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, original,
        "existing config must be byte-identical after load"
    );
}

#[cfg(unix)]
#[test]
fn unwritable_parent_is_a_clear_error_naming_the_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    // Read + execute only — no write permission, so creating a file inside
    // must fail with a permission error.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

    let path = locked.join("node.toml");
    let result = NodeFileConfig::load_or_bootstrap(&path);

    // Restore perms unconditionally so tempdir cleanup can remove the dir,
    // even if the assertion below fails.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

    let err = result.expect_err("write into a read-only directory must fail");
    assert!(
        err.contains(&path.display().to_string()),
        "error should name the path {path:?}: {err}"
    );
}
