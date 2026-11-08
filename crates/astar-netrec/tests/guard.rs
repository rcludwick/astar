// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Runs `ci/guard-netrec-receive-only.sh` against doctored copies of this
//! crate. The forbidden names are assembled at run time, so this file's own
//! text stays clean for the guard.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create dir");
    for entry in fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

/// A copy of `src` and `tests` that removes itself on drop.
struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "netrec-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(&crate_dir().join("src"), &dir.join("src"));
        copy_tree(&crate_dir().join("tests"), &dir.join("tests"));
        Self(dir)
    }

    fn add(&self, file: &str, text: &str) -> &Self {
        fs::write(self.0.join(file), text).expect("write file");
        self
    }

    fn append(&self, file: &str, text: &str) -> &Self {
        let path = self.0.join(file);
        let mut body = fs::read_to_string(&path).expect("read file");
        body.push_str(text);
        fs::write(path, body).expect("write file");
        self
    }

    fn guard(&self) -> Output {
        Command::new(crate_dir().join("../../ci/guard-netrec-receive-only.sh"))
            .env("NETREC_DIR", &self.0)
            .output()
            .expect("run the guard")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_caught(out: &Output, file: &str) {
    assert!(!out.status.success(), "guard passed: {}", text(out));
    assert!(
        text(out).contains(file),
        "output should name {file}: {}",
        text(out)
    );
}

fn assert_passed(out: &Output) {
    assert!(out.status.success(), "guard failed: {}", text(out));
}

#[test]
fn guard_passes_clean_tree() {
    assert_passed(&Tree::new().guard());

    // Longer names that merely end in a forbidden one must not trip it.
    let lookalikes = concat!(
        "fn x(s: &S) {\n",
        "    s.m17_monitor_connect(r);\n",
        "    s.m17_monitor_disconnect();\n",
        "    s.dstar_connect(h, p, m, c, None);\n",
        "    s.dstar_disconnect();\n",
        "    s.add_source_member();\n",
        "    s.remove_bridge_member(id);\n",
        "    let answered = answer_count + 1;\n",
        "}\n",
    );
    assert_passed(&Tree::new().add("src/lookalike.rs", lookalikes).guard());
}

#[test]
fn guard_catches_set_ptt() {
    let call = format!("fn x(s: &S) {{ s.{}{}(true); }}\n", "set_", "ptt");
    let tree = Tree::new();
    let out = tree.add("src/bad.rs", &call).guard();
    assert_caught(&out, "src/bad.rs:1");
}

#[test]
fn guard_catches_every_deny_listed_name() {
    let names = [
        ["set_", "ptt"].concat(),
        ["link_", "key"].concat(),
        ["link_", "set_mode"].concat(),
        ["sync_", "link_keying"].concat(),
        ["send_", "dtmf"].concat(),
        ["send_", "dtmf_string"].concat(),
        ["ann", "ounce"].concat(),
        ["ann", "ounce_to_member"].concat(),
        ["echolink_", "send_chat"].concat(),
        ["ans", "wer"].concat(),
        ["enable_", "inbound"].concat(),
        ["set_", "mode"].concat(),
        ["con", "nect"].concat(),
        ["con", "nect_wt"].concat(),
        ["con", "nect_wt_at"].concat(),
        ["m17_", "connect"].concat(),
        ["echolink_", "node_connect"].concat(),
        ["ysf_", "connect"].concat(),
        ["nxdn_", "connect"].concat(),
        ["dmr_", "connect"].concat(),
        ["link_", "connect"].concat(),
        ["link_", "connect_at"].concat(),
        ["echolink_", "connect"].concat(),
        ["start_", "inbound"].concat(),
        ["start_", "inbound_on"].concat(),
        ["answer_", "pending"].concat(),
        ["connect_", "with_vocoder"].concat(),
        ["connect_", "with_audio"].concat(),
    ];
    for name in names {
        let tree = Tree::new();
        let out = tree
            .add("tests/bad.rs", &format!("fn x() {{ s.{name}(); }}\n"))
            .guard();
        assert_caught(&out, "tests/bad.rs:1");
    }
}

#[test]
fn guard_catches_a_method_taken_by_path() {
    let line = format!("fn x() {{ let f = S::{}{}; }}\n", "set_", "ptt");
    let tree = Tree::new();
    assert_caught(&tree.add("src/bad.rs", &line).guard(), "src/bad.rs:1");
}

#[test]
fn guard_catches_a_socket_style_method_call() {
    let line = format!("fn x(s: &UdpSocket) {{ s.{}{}(a); }}\n", "con", "nect");
    let tree = Tree::new();
    assert_caught(&tree.add("tests/bad.rs", &line).guard(), "tests/bad.rs:1");
}

#[test]
fn guard_keeps_bridge_calls_in_listen_rs() {
    for name in [
        ["add_", "bridge_member"].concat(),
        ["set_", "bridge_config"].concat(),
        ["set_", "dstar_vocoder"].concat(),
    ] {
        let tree = Tree::new();
        let out = tree
            .add("src/bad.rs", &format!("fn x() {{ s.{name}(); }}\n"))
            .guard();
        assert_caught(&out, "src/bad.rs:1");
    }
}

/// The three words, assembled so that this file does not contain them.
fn holder_types() -> [String; 3] {
    [
        ["Sta", "tion"].concat(),
        ["Console", "Session"].concat(),
        ["Dstar", "Session"].concat(),
    ]
}

#[test]
fn guard_keeps_the_station_types_in_listen_rs() {
    for name in holder_types() {
        for line in [
            format!("fn x() {{ let s = {name}::with_shared_session(a, b, c); }}\n"),
            format!("struct Holder {{ s: {name} }}\n"),
            format!("use astar_station::{name};\n"),
        ] {
            let tree = Tree::new();
            let out = tree.add("src/bad.rs", &line).guard();
            assert_caught(&out, "src/bad.rs:1");
        }
        let tree = Tree::new();
        let test_file = format!("fn x(s: &{name}) {{}}\n");
        let out = tree.add("tests/bad.rs", &test_file).guard();
        assert_caught(&out, "tests/bad.rs:1");
    }
}

#[test]
fn guard_lets_those_words_stay_in_comments_and_longer_names() {
    for name in holder_types() {
        let text = format!(
            "// One {name} per daemon.\n/// A {name} is held in listen.rs.\n//! {name}.\nfn x(s: &Listen{name}, t: {name}Config) {{}} // {name}\n"
        );
        assert_passed(&Tree::new().add("src/fine.rs", &text).guard());
    }
    let reexport = "use astar_station::DstarVocoder;\n";
    assert_passed(&Tree::new().add("src/fine.rs", reexport).guard());
}

#[test]
fn guard_exempts_only_the_deny_list_block() {
    // In listen.rs, but past the block.
    let line = format!("fn x(s: &S) {{ s.{}{}(true); }}\n", "set_", "ptt");
    let tree = Tree::new();
    let out = tree.append("src/listen.rs", &line).guard();
    assert_caught(&out, "src/listen.rs:");
}

#[test]
fn guard_allows_one_deny_list_block_only() {
    let marker = format!("// {} {}\n", "RECEIVE-ONLY", "DENY LIST");
    let tree = Tree::new();
    let out = tree.append("src/listen.rs", &marker).guard();
    assert_caught(&out, "DENY LIST");
}

#[test]
fn guard_fails_on_a_missing_tree() {
    let tree = Tree::new();
    fs::remove_file(tree.0.join("src/listen.rs")).expect("remove");
    assert_caught(&tree.guard(), "listen.rs");
}
