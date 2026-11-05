// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Black-box fixtures of both ports over real sockets on 127.0.0.1.
//! Recorded once before the astar-http move (`ASTAR_GOLDEN_RECORD=1`), then
//! compared on every run: a fixture that changes is either a regression or
//! a deliberate change that its commit names.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::time::{Duration, Instant};

use crate::controller::NodeController;
use crate::public::guard::{Guard, Limits};
use crate::public::http::{ChallengeMap, Format, HubRefusal, Listener, PublicCtx, StreamSource};
use crate::public::status::{Activity, StreamAvailability};
use crate::secrets::SecretProvider;

const KEEP: &[&str] = &[
    "allow",
    "cache-control",
    "content-security-policy",
    "content-type",
    "location",
    "referrer-policy",
    "retry-after",
    "set-cookie",
    "strict-transport-security",
    "x-content-type-options",
    "x-frame-options",
];
/// How many leading body bytes a `stream-first:` line records.
const STREAM_FIRST_BYTES: usize = 7;
const HOST: &str = "node.example.net";

fn ctrl() -> Arc<NodeController> {
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    Arc::new(NodeController::new(station, SecretProvider::new()))
}

// ---- the only two functions the move may change ------------------------------

fn start_control(ctrl: &Arc<NodeController>) -> (SocketAddr, Box<dyn FnOnce() + Send>) {
    let server = crate::server::bind_control(ctrl, "127.0.0.1:0").expect("bind");
    let addr = server.local_addr();
    (addr, Box::new(move || server.stop()))
}

fn start_public(
    ctx: &Arc<PublicCtx>,
    http80: bool,
) -> (SocketAddr, Option<SocketAddr>, Box<dyn FnOnce() + Send>) {
    let challenges: ChallengeMap = Arc::new(Mutex::new(
        std::iter::once(("tok-abc".to_string(), "tok-abc.ka".to_string())).collect(),
    ));
    let l = crate::public::serve::Listeners::start(
        "127.0.0.1:0",
        http80.then_some("127.0.0.1:0"),
        ctx,
        &challenges,
        None,
    )
    .expect("bind");
    let (a, b) = (l.public_addr(), l.http80_addr());
    (a, b, Box::new(move || l.stop()))
}

// ---- capture and normalise -----------------------------------------------------

fn exchange(addr: SocketAddr, raw: &[u8], stream: bool) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(if stream { 400 } else { 5000 })))
        .unwrap();
    s.write_all(raw).expect("write");
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let until = Instant::now() + Duration::from_secs(if stream { 30 } else { 10 });
    while Instant::now() < until {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if stream && first_frame_seen(&out) {
                    break;
                }
            }
            // A quiet stream is not a finished one: keep waiting for its first frame.
            Err(e)
                if stream
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
            Err(_) => break,
        }
    }
    out
}

/// The head is complete and the (de-chunked) body holds the bytes a stream
/// fixture records.
fn first_frame_seen(resp: &[u8]) -> bool {
    let Some(i) = resp.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&resp[..i]).to_ascii_lowercase();
    let body = &resp[i + 4..];
    let n = if head.contains("transfer-encoding: chunked") {
        dechunk(body).len()
    } else {
        body.len()
    };
    n >= STREAM_FIRST_BYTES
}

fn fnv1a64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, x| {
        (h ^ u64::from(*x)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Undo chunked framing (a streamed body arrives chunked).
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(nl) = rest.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..nl])
                .unwrap_or("0")
                .split(';')
                .next()
                .unwrap_or("0")
                .trim(),
            16,
        )
        .unwrap_or(0);
        rest = &rest[nl + 2..];
        if size == 0 || rest.len() < size {
            out.extend_from_slice(&rest[..size.min(rest.len())]);
            return out;
        }
        out.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

fn normalise(raw_request: &str, resp: &[u8], dynamic_json: bool, stream: bool) -> String {
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(resp.len(), |i| i + 4);
    let head = String::from_utf8_lossy(&resp[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("none")
        .to_string();
    let mut chunked = false;
    let mut headers: Vec<String> = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
        if !KEEP.contains(&k.as_str()) {
            continue;
        }
        let v = if k == "set-cookie" {
            let (name, attrs) = v.split_once(';').unwrap_or((v, ""));
            format!("{}=<v>;{attrs}", name.split('=').next().unwrap_or(""))
        } else {
            v.to_string()
        };
        headers.push(format!("header: {k}: {v}"));
    }
    headers.sort();
    let body = if chunked {
        dechunk(&resp[split..])
    } else {
        resp[split..].to_vec()
    };
    let first_line = raw_request.lines().next().unwrap_or("");
    let mut out = format!("request: {first_line}\nstatus: {status}\n");
    for h in headers {
        out.push_str(&h);
        out.push('\n');
    }
    if stream {
        let _ = writeln!(
            out,
            "stream-first: {}",
            String::from_utf8_lossy(&body[..body.len().min(STREAM_FIRST_BYTES)])
        );
    } else if dynamic_json {
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let mut keys: Vec<&String> = v
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        keys.sort();
        let _ = writeln!(
            out,
            "body-json-keys: {}",
            keys.iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
    } else if body.len() > 2048 {
        let _ = writeln!(
            out,
            "body-len: {} fnv1a64: {:016x}",
            body.len(),
            fnv1a64(&body)
        );
    } else {
        let _ = writeln!(out, "body: {}", String::from_utf8_lossy(&body));
    }
    out
}

fn check(group: &str, name: &str, got: &str) {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "golden",
        group,
        &format!("{name}.txt"),
    ]
    .iter()
    .collect();
    if std::env::var_os("ASTAR_GOLDEN_RECORD").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{}: no fixture; record with ASTAR_GOLDEN_RECORD=1",
            path.display()
        )
    });
    assert_eq!(got, want, "{group}/{name} changed");
}

/// One case: (name, raw request, dynamic json, stream).
type Case = (&'static str, String, bool, bool);

fn get(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

fn control_cases() -> Vec<Case> {
    let h = "127.0.0.1";
    vec![
        ("root", get("/", h), false, false),
        ("app_js", get("/app.js", h), false, false),
        ("style_css", get("/style.css", h), false, false),
        ("favicon", get("/favicon.ico", h), false, false),
        ("status", get("/status", h), true, false),
        ("status_host_with_port", get("/status", "localhost:8730"), true, false),
        ("status_host_ipv6_port", get("/status", "[::1]:8730"), true, false),
        ("status_host_empty_port", get("/status", "localhost:"), false, false),
        ("admin_redirect", get("/admin", h), false, false),
        ("admin_status", get("/admin/status", h), true, false),
        ("admin_admin_404", get("/admin/admin/status", h), false, false),
        ("unknown_404", get("/nope", h), false, false),
        ("foreign_host_403", get("/status", "evil.example"), false, false),
        ("no_host", "GET /status HTTP/1.1\r\nConnection: close\r\n\r\n".into(), false, false),
        ("http10_no_host", "GET /status HTTP/1.0\r\n\r\n".into(), false, false),
        ("cross_site_form_key_415", "POST /key HTTP/1.1\r\nHost: 127.0.0.1\r\nSec-Fetch-Site: cross-site\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(), false, false),
        ("foreign_origin_key_403", "POST /key HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into(), false, false),
        ("unkey_curl", "POST /unkey HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(), false, false),
        ("announced_413", "POST /hangup HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n".into(), false, false),
        ("settings_state", get("/settings/state", h), true, false),
        ("recordings_page", get("/recordings", h), false, false),
        ("recordings_state_off", get("/recordings/state", h), false, false),
        ("recordings_tar_off", get("/recordings.tar", h), false, false),
        ("events", get("/events", h), false, true),
    ]
}

#[test]
fn control_port_fixtures() {
    let c = ctrl();
    let (addr, stop) = start_control(&c);
    for (name, raw, dynamic, stream) in control_cases() {
        let resp = exchange(addr, raw.as_bytes(), stream);
        check("control", name, &normalise(&raw, &resp, dynamic, stream));
    }
    stop();
}

fn public_cases() -> Vec<Case> {
    vec![
        ("root", get("/", HOST), false, false),
        (
            "root_head",
            format!("HEAD / HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
            false,
            false,
        ),
        ("listener_js", get("/listener.js", HOST), false, false),
        ("style_css", get("/style.css", HOST), false, false),
        ("public_status", get("/public/status", HOST), true, false),
        (
            "post_405",
            format!(
                "POST / HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            false,
            false,
        ),
        ("admin", get("/admin", HOST), false, false),
        ("unknown_404", get("/nope", HOST), false, false),
        ("wrong_host_421", get("/", "other.example"), false, false),
        (
            "no_host",
            "GET / HTTP/1.1\r\nConnection: close\r\n\r\n".into(),
            false,
            false,
        ),
        (
            "oversize_head",
            format!(
                "GET / HTTP/1.1\r\nHost: {HOST}\r\nX-Pad: {}\r\n\r\n",
                "a".repeat(9000)
            ),
            false,
            false,
        ),
        ("mp3_not_built_503", get("/stream.mp3", HOST), false, false),
        // `/stream.ws` answers per feature set: with `stream` the upgrade
        // reaches the hub; without it every request gets the one 503 saying
        // live audio is not built in (`public::http::websocket`). Each build
        // checks the fixtures its own code produces.
        #[cfg(feature = "stream")]
        ("ws_no_upgrade_400", get("/stream.ws", HOST), false, false),
        #[cfg(feature = "stream")]
        ("ws_full_503", ws_upgrade(), false, false),
        #[cfg(not(feature = "stream"))]
        ("ws_not_built_503", ws_upgrade(), false, false),
        ("events", get("/public/events", HOST), false, true),
    ]
}

/// A hub whose `open` always refuses: the stream routes answer 503.
struct FullHub {
    mp3: bool,
}

impl StreamSource for FullHub {
    fn open(&self, _f: Format) -> Result<Box<dyn Listener>, HubRefusal> {
        Err(HubRefusal::Full)
    }
    fn levels(&self) -> Option<crate::public::http::Levels> {
        None
    }
    fn availability(&self) -> StreamAvailability {
        StreamAvailability::Live
    }
    fn listeners(&self) -> u32 {
        0
    }
    fn sample_rate(&self) -> Option<u32> {
        Some(16_000)
    }
    fn mp3_built(&self) -> bool {
        self.mp3
    }
}

/// A WebSocket upgrade for `/stream.ws`: gets as far as the hub.
fn ws_upgrade() -> String {
    format!(
        "GET /stream.ws HTTP/1.1\r\nHost: {HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
}

fn public_ctx(ctrl: &Arc<NodeController>, limits: Limits, mp3: bool) -> Arc<PublicCtx> {
    Arc::new(PublicCtx {
        ctrl: Arc::clone(ctrl),
        hub: Some(Arc::new(FullHub { mp3 })),
        activity: Arc::new(Mutex::new(Activity::new(Instant::now()))),
        hostnames: vec![HOST.to_string()],
        tls: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
        anonymous_status: true,
        guard: Guard::new(limits),
    })
}

fn run_public(
    group: &str,
    ctrl: &Arc<NodeController>,
    limits: Limits,
    mp3: bool,
    cases: Vec<Case>,
) {
    let ctx = public_ctx(ctrl, limits, mp3);
    let (addr, _, stop) = start_public(&ctx, false);
    for (name, raw, dynamic, stream) in cases {
        let resp = exchange(addr, raw.as_bytes(), stream);
        check(group, name, &normalise(&raw, &resp, dynamic, stream));
    }
    stop();
}

#[test]
fn public_bind_fixtures() {
    run_public("public", &ctrl(), Limits::default(), false, public_cases());
}

/// The per-address stream limit sits behind the WebSocket upgrade, which a
/// build without `stream` never performs.
#[cfg(feature = "stream")]
#[test]
fn public_stream_429_fixture() {
    let limits = Limits {
        streams_per_ip: 0,
        ..Limits::default()
    };
    run_public(
        "public",
        &ctrl(),
        limits,
        false,
        vec![("ws_stream_429", ws_upgrade(), false, false)],
    );
}

/// A hub that has MP3: `/stream.mp3` reaches the stream slot and the hub.
#[test]
fn public_mp3_fixtures() {
    let c = ctrl();
    let mp3 = |name, limits| {
        run_public(
            "public",
            &c,
            limits,
            true,
            vec![(name, get("/stream.mp3", HOST), false, false)],
        );
    };
    mp3("mp3_full_503", Limits::default());
    mp3(
        "stream_429",
        Limits {
            streams_per_ip: 0,
            ..Limits::default()
        },
    );
}

#[test]
fn port80_fixtures() {
    let ctx = public_ctx(&ctrl(), Limits::default(), false);
    let (_, p80, stop) = start_public(&ctx, true);
    let p80 = p80.expect("port 80 listener");
    for (name, raw) in [
        (
            "challenge",
            get("/.well-known/acme-challenge/tok-abc", HOST),
        ),
        ("root", get("/", HOST)),
    ] {
        let resp = exchange(p80, raw.as_bytes(), false);
        check("port80", name, &normalise(&raw, &resp, false, false));
    }
    stop();
}

#[cfg(feature = "auth")]
mod auth_cases {
    use super::*;
    use crate::auth::store::Role;
    use crate::auth::{AuthService, AuthSettings};

    fn with_auth() -> (Arc<NodeController>, Arc<AuthService>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let auth = Arc::new(AuthService::open(AuthSettings::defaults(dir.path())));
        let c = ctrl();
        c.set_auth(Arc::clone(&auth));
        (c, auth, dir)
    }

    #[test]
    fn public_bind_auth_fixtures() {
        let (c, auth, _d) = with_auth();
        let cookie = format!("astar_session={}", auth.test_sign_in("A1DM", Role::Admin));
        let viewer = format!("astar_session={}", auth.test_sign_in("V1EW", Role::Viewer));
        let cases: Vec<Case> = vec![
            (
                "admin_status_no_cookie",
                get("/admin/status", HOST),
                false,
                false,
            ),
            (
                "admin_status_admin",
                format!(
                    "GET /admin/status HTTP/1.1\r\nHost: {HOST}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
                ),
                true,
                false,
            ),
            (
                "recordings_no_cookie",
                get("/admin/recordings", HOST),
                false,
                false,
            ),
            (
                "recordings_viewer_403",
                format!(
                    "GET /admin/recordings.tar HTTP/1.1\r\nHost: {HOST}\r\nCookie: {viewer}\r\nConnection: close\r\n\r\n"
                ),
                false,
                false,
            ),
        ];
        run_public("auth", &c, Limits::default(), false, cases);
    }

    #[test]
    fn control_port_auth_fixtures() {
        let (c, _auth, _d) = with_auth();
        let (addr, stop) = start_control(&c);
        let cases: Vec<Case> = vec![
            ("control_auth_me", get("/auth/me", "127.0.0.1"), false, false),
            (
                "control_proxy_status_401",
                "GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Forwarded-For: 198.51.100.7\r\nConnection: close\r\n\r\n".into(),
                false,
                false,
            ),
        ];
        for (name, raw, dynamic, stream) in cases {
            let resp = exchange(addr, raw.as_bytes(), stream);
            check("auth", name, &normalise(&raw, &resp, dynamic, stream));
        }
        stop();
    }
}
