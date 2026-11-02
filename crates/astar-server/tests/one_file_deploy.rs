// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Deploy is one executable plus one config file (Rob, 2026-10-12): the
//! binary, copied alone into an empty directory beside a `node.toml` that
//! carries its credentials inline, starts, serves its control port and its
//! public page, and needs nothing else on disk.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(port: u16, host: &str, path: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").ok()?;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    Some(out)
}

#[test]
fn one_binary_and_one_node_toml_is_a_whole_node() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("astar-server");
    std::fs::copy(env!("CARGO_BIN_EXE_astar-server"), &bin).unwrap();
    let (control, public) = (free_port(), free_port());
    let config = dir.path().join("node.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[listener]
bind = "127.0.0.1:0"
answer = "auto"
max_calls = 2
auth = "required"
[register]
peer = "127.0.0.1:9"
node_id = "69586"
[audio]
backend = "none"
[control]
bind = "127.0.0.1:{control}"
[secrets]
source = "config"
secret = "not-a-real-password"
porkbun_pk = "pk1_not_real"
porkbun_sk = "sk1_not_real"
[web]
public_bind = "127.0.0.1:{public}"
public_hostnames = ["{{node}}.test.invalid"]
acme_challenge = "http-01"
acme_http_bind = "127.0.0.1:0"
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut child = Command::new(&bin)
        .args(["serve", "--config"])
        .arg(&config)
        .current_dir(dir.path())
        .env_remove("STATE_DIRECTORY")
        // Never Let's Encrypt from a test: nothing listens on port 9.
        .env("ASTAR_ACME_DIRECTORY", "http://127.0.0.1:9/directory")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status = None;
    while Instant::now() < deadline {
        if let Some(r) =
            get(control, "localhost", "/status").filter(|r| r.starts_with("HTTP/1.1 200"))
        {
            status = Some(r);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let page = get(public, "69586.test.invalid", "/");
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    let status = status.unwrap_or_else(|| panic!("the control port never answered:\n{log}"));
    assert!(status.contains("\"listening\":true"), "{status}");
    let page = page.unwrap_or_default();
    assert!(
        page.starts_with("HTTP/1.1 200") && page.contains("id=\"hero\""),
        "{page}\n{log}"
    );
    assert!(
        !log.contains("letsencrypt"),
        "a test must never reach Let's Encrypt:\n{log}"
    );
    for leaked in ["not-a-real-password", "pk1_not_real", "sk1_not_real"] {
        assert!(!log.contains(leaked), "{leaked} reached the log:\n{log}");
        assert!(!status.contains(leaked));
    }
    // Nothing else was needed, and nothing but runtime state was made.
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.retain(|n| n != "state");
    assert_eq!(names, ["astar-server", "node.toml"]);
}
