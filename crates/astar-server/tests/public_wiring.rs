// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `[web]` from a real `node.toml` to a live public bind: the page answers on
//! it, the admin routes do not, and the admin snapshot reports the TLS
//! state. The ACME directory points at a closed port on 127.0.0.1, so no
//! certificate is ever requested from anyone.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_server::config::NodeFileConfig;
use astar_server::controller::NodeController;
use astar_server::http::handle_request;
use astar_server::public::run::PublicSide;
use astar_server::secrets::SecretProvider;

const CONFIG: &str = r#"
[listener]
bind = "127.0.0.1:0"
answer = "auto"
max_calls = 2
auth = "off"
[register]
peer = "127.0.0.1:4569"
node_id = "69586"
[control]
bind = "127.0.0.1:8730"
[secrets]
source = "env"
[web]
public_bind = "127.0.0.1:0"
public_hostnames = ["{node}.test.invalid"]
acme_http_bind = "127.0.0.1:0"
"#;

fn get(addr: SocketAddr, host: &str, method: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn web_config_starts_the_public_page_beside_the_admin_router() {
    let cfg = NodeFileConfig::from_toml_str(CONFIG).unwrap();
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let ctrl = Arc::new(NodeController::new(station, SecretProvider::new()));
    let state = tempfile::tempdir().unwrap();
    let side = PublicSide::start(
        &cfg,
        state.path(),
        &ctrl,
        None,
        Some("http://127.0.0.1:9/directory"),
    )
    .unwrap();
    let addr = side.public_addr().expect("a public bind");
    let host = "69586.test.invalid";

    let page = get(addr, host, "GET", "/");
    assert!(
        page.starts_with("HTTP/1.1 200") && page.contains("id=\"hero\""),
        "{page}"
    );
    let status = get(addr, host, "GET", "/public/status");
    assert!(status.contains("\"stream\":\"off\""), "{status}");
    for (m, p) in [("POST", "/key"), ("GET", "/status"), ("GET", "/settings")] {
        let r = get(addr, host, m, p);
        assert!(r.starts_with("HTTP/1.1 404"), "{m} {p}: {r}");
    }
    let r = get(addr, "test.invalid", "GET", "/");
    assert!(r.starts_with("HTTP/1.1 421"), "the expanded name only: {r}");
    let r = get(side.http80_addr().unwrap(), host, "GET", "/admin");
    assert!(
        r.starts_with("HTTP/1.1 200"),
        "port 80 serves the page before a certificate: {r}"
    );

    // The admin /status carries the public state; it is not on the public bind.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = handle_request(&ctrl, "GET", "/status", b"");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        let public = &v["public"];
        assert_eq!(public["serving_tls"], false, "{v}");
        assert_eq!(public["hosts"], serde_json::json!([host]));
        if public["last_error"].is_string() || !cfg!(feature = "public-tls") {
            break;
        }
        assert!(Instant::now() < deadline, "no ACME error reported: {v}");
        std::thread::sleep(Duration::from_millis(50));
    }
    side.stop();
}

#[test]
fn no_web_section_starts_nothing() {
    let text = CONFIG.split("[web]").next().unwrap();
    let cfg = NodeFileConfig::from_toml_str(text).unwrap();
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let ctrl = Arc::new(NodeController::new(station, SecretProvider::new()));
    let state = tempfile::tempdir().unwrap();
    let side = PublicSide::start(&cfg, state.path(), &ctrl, None, None).unwrap();
    assert!(side.public_addr().is_none());
    assert!(ctrl.snapshot().public.is_none());
    side.stop();
}

/// `[stream] enabled` end to end: the tap enrols on the node's real
/// conference, `/stream.ws` sends Opus packets, and a switch to handset
/// turns the stream into a 503 that says why.
#[cfg(feature = "stream")]
#[test]
fn stream_config_taps_the_conference_and_follows_the_mode() {
    use astar_server::command::NodeCommand;
    let text = CONFIG.replace("[web]", "[stream]\nenabled = true\n[web]");
    let cfg = NodeFileConfig::from_toml_str(&text).unwrap();
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let ctrl = Arc::new(NodeController::new(station, SecretProvider::new()));
    ctrl.execute(NodeCommand::EnableInbound).unwrap();
    let bridge = |mode: &str| {
        ctrl.execute(NodeCommand::SetBridge {
            mode: Some(mode.into()),
            mix_minus: Some(true),
            include_local_radio: Some(false),
        })
        .unwrap();
    };
    bridge("bridge");
    let state = tempfile::tempdir().unwrap();
    let side = PublicSide::start(
        &cfg,
        state.path(),
        &ctrl,
        None,
        Some("http://127.0.0.1:9/directory"),
    )
    .unwrap();
    let addr = side.public_addr().unwrap();
    let host = "69586.test.invalid";

    let deadline = Instant::now() + Duration::from_secs(10);
    while !get(addr, host, "GET", "/public/status").contains("\"stream\":\"live\"") {
        assert!(Instant::now() < deadline, "the tap never went live");
        std::thread::sleep(Duration::from_millis(50));
    }
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = tungstenite::http::Request::builder()
        .uri(format!("ws://{host}/stream.ws"))
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .unwrap();
    let (mut ws, _) = tungstenite::client(req, tcp).expect("upgrade");
    let header = ws.read().unwrap();
    assert!(header.to_text().unwrap().contains("\"codec\":\"opus\""));
    for _ in 0..10 {
        assert!(
            ws.read().unwrap().is_binary(),
            "an Opus packet every 20 ms, silence included"
        );
    }
    drop(ws);

    bridge("handset");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = get(addr, host, "GET", "/stream.ws");
        if r.starts_with("HTTP/1.1 503") {
            assert!(r.contains("No live audio in this mode"), "{r}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the stream did not notice handset: {r}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    side.stop();
}
