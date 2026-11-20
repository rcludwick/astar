// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Rule 4 of `ci/guard-netrec-receive-only.sh`: this crate's manifest names no
//! crate that can reach a radio. The names are assembled at run time so this
//! file stays clean.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn run(manifest: &str, tag: &str) -> (bool, String) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp =
        std::env::temp_dir().join(format!("netvideo-guard-{}-{tag}.toml", std::process::id()));
    fs::write(&tmp, manifest).expect("write manifest");
    let out = Command::new(dir.join("../../ci/guard-netrec-receive-only.sh"))
        .env("NETVIDEO_TOML", &tmp)
        .output()
        .expect("run the guard");
    let _ = fs::remove_file(&tmp);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn the_real_manifest_passes() {
    let real = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read manifest");
    let (ok, text) = run(&real, "real");
    assert!(ok, "{text}");
}

#[test]
fn every_radio_crate_is_refused() {
    for name in [
        "station",
        "console",
        "iax",
        "audio",
        "m17",
        "dstar",
        "echolink",
        "ptt",
        "serial-sys",
    ] {
        let krate = format!("astar-{name}");
        let manifest = format!("[dependencies]\n{krate} = {{ path = \"../{krate}\" }}\n");
        let (ok, text) = run(&manifest, name);
        assert!(
            !ok && text.contains(&krate),
            "{krate} was not refused: {text}"
        );
    }
}

#[test]
fn a_longer_name_is_not_a_match() {
    let (ok, text) = run("[dependencies]\nastar-iax-core = \"1\"\n", "longer");
    assert!(ok, "{text}");
}
