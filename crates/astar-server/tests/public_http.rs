// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public bind over a real socket on 127.0.0.1: admin routes are 404 on
//! the wire, the event feed streams, and a stream listener holds exactly one
//! slot for exactly as long as its socket is open.
#![cfg(feature = "stream")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

use astar_server::controller::NodeController;
use astar_server::public::guard::{Guard, Limits};
use astar_server::public::http::{
    ChallengeMap, Format, HubRefusal, Levels, Listener, PublicCtx, Pull, StreamSource,
};
use astar_server::public::serve::Listeners;
use astar_server::public::status::{Activity, StreamAvailability};
use astar_server::secrets::SecretProvider;

const HOST: &str = "test.invalid";

/// A hub that hands out a 40-byte "packet" every 20 ms per listener.
struct ToneHub {
    count: Arc<AtomicU32>,
    max: u32,
}

struct ToneListener {
    count: Arc<AtomicU32>,
    next: Instant,
}

impl Drop for ToneListener {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Listener for ToneListener {
    fn pull(&mut self, timeout: Duration) -> Pull {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Pull::Idle;
            }
            std::thread::sleep(wait);
        }
        self.next += Duration::from_millis(20);
        Pull::Chunk(Arc::from(vec![0xFCu8; 40]))
    }
}

impl StreamSource for ToneHub {
    fn open(&self, _f: Format) -> Result<Box<dyn Listener>, HubRefusal> {
        let mut n = self.count.load(Ordering::SeqCst);
        loop {
            if n >= self.max {
                return Err(HubRefusal::Full);
            }
            match self
                .count
                .compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(cur) => n = cur,
            }
        }
        Ok(Box::new(ToneListener {
            count: Arc::clone(&self.count),
            next: Instant::now(),
        }))
    }
    fn levels(&self) -> Option<Levels> {
        Some(Levels {
            dbfs: -20.0,
            spectrum: vec![-80.0; 32],
            rate: 16_000,
        })
    }
    fn availability(&self) -> StreamAvailability {
        StreamAvailability::Live
    }
    fn listeners(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }
    fn sample_rate(&self) -> Option<u32> {
        Some(16_000)
    }
    fn mp3_built(&self) -> bool {
        false
    }
}

struct Rig {
    listeners: Option<Listeners>,
    addr: SocketAddr,
    http80: Option<SocketAddr>,
    count: Arc<AtomicU32>,
    challenges: ChallengeMap,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(l) = self.listeners.take() {
            l.stop();
        }
    }
}

fn rig(max: u32) -> Rig {
    rig_with(max, Limits::default(), false)
}

/// A rig with `limits`, and a port-80 listener when `http80`.
fn rig_with(max: u32, limits: Limits, http80: bool) -> Rig {
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let ctrl = Arc::new(NodeController::new(station, SecretProvider::new()));
    let count = Arc::new(AtomicU32::new(0));
    let ctx = Arc::new(PublicCtx {
        ctrl,
        hub: Some(Arc::new(ToneHub {
            count: Arc::clone(&count),
            max,
        })),
        activity: Arc::new(Mutex::new(Activity::new(Instant::now()))),
        hostnames: vec![HOST.to_string()],
        tls: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
        anonymous_status: true,
        guard: Guard::new(limits),
    });
    let challenges: ChallengeMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let listeners = Listeners::start(
        "127.0.0.1:0",
        http80.then_some("127.0.0.1:0"),
        &ctx,
        &challenges,
        None,
    )
    .expect("bind 127.0.0.1:0");
    let addr = listeners.public_addr();
    Rig {
        http80: listeners.http80_addr(),
        listeners: Some(listeners),
        addr,
        count,
        challenges,
    }
}

fn request(addr: SocketAddr, raw: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ws_connect(
    addr: SocketAddr,
) -> Result<tungstenite::WebSocket<TcpStream>, Box<tungstenite::Error>> {
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = tungstenite::http::Request::builder()
        .uri(format!("ws://{HOST}/stream.ws"))
        .header("Host", HOST)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .unwrap();
    match tungstenite::client(req, tcp) {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::HandshakeError::Failure(e)) => Err(Box::new(e)),
        Err(tungstenite::HandshakeError::Interrupted(_)) => panic!("blocking socket interrupted"),
    }
}

#[test]
fn opus_stream_delivers_packets_and_counts_listeners() {
    let r = rig(25);
    let mut ws = ws_connect(r.addr).expect("upgrade");
    let first = ws.read().expect("header");
    let header: serde_json::Value =
        serde_json::from_str(first.to_text().expect("text header")).unwrap();
    assert_eq!(header["codec"], "opus");
    assert_eq!(header["rate"], 16_000);
    assert_eq!(header["frame_ms"], 20);
    for _ in 0..10 {
        let m = ws.read().expect("packet");
        assert!(m.is_binary(), "{m:?}");
        assert_eq!(m.into_data().len(), 40);
    }
    assert_eq!(r.count.load(Ordering::SeqCst), 1);
    drop(ws);
    wait_for("the slot to be freed", || {
        r.count.load(Ordering::SeqCst) == 0
    });
}

#[test]
fn listener_over_cap_gets_503_with_the_reason() {
    let r = rig(1);
    let _first = ws_connect(r.addr).expect("first listener");
    // Read the refusal raw, to the close: tungstenite's client keeps only the
    // body bytes that arrived with the head, so a body in its own segment
    // reads as empty.
    let resp = request(
        r.addr,
        &format!(
            "GET /stream.ws HTTP/1.1\r\nHost: {HOST}\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
    assert!(resp.contains("Full, try again shortly"), "{resp}");
    assert_eq!(r.count.load(Ordering::SeqCst), 1);
}

#[test]
fn admin_routes_are_404_over_the_wire() {
    let r = rig(25);
    for (method, path) in [
        ("POST", "/key"),
        ("GET", "/status"),
        ("GET", "/settings"),
        ("POST", "/secrets"),
        ("GET", "/events"),
    ] {
        let resp = request(
            r.addr,
            &format!("{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 2\r\n\r\n{{}}"),
        );
        assert!(resp.starts_with("HTTP/1.1 404"), "{method} {path}: {resp}");
    }
    let resp = request(r.addr, "POST / HTTP/1.1\r\nHost: test.invalid\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 405"), "{resp}");
    assert!(resp.contains("Allow: GET, HEAD"), "{resp}");
    let resp = request(r.addr, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 421"), "{resp}");
}

#[test]
fn the_page_is_served_with_its_security_headers() {
    let r = rig(25);
    let resp = request(r.addr, "GET / HTTP/1.1\r\nHost: TEST.invalid.:443\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("id=\"hero\""), "{resp}");
    for h in [
        "Content-Security-Policy: default-src 'self'",
        "X-Content-Type-Options: nosniff",
        "X-Frame-Options: DENY",
    ] {
        assert!(resp.contains(h), "{h}: {resp}");
    }
    assert!(
        !resp.to_ascii_lowercase().contains("access-control-"),
        "{resp}"
    );
}

#[test]
fn public_events_streams_status_and_levels() {
    let r = rig(25);
    let mut s = TcpStream::connect(r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /public/events HTTP/1.1\r\nHost: test.invalid\r\n\r\n")
        .unwrap();
    let mut lines = BufReader::new(s).lines();
    let (mut status, mut levels) = (false, false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(status && levels) && Instant::now() < deadline {
        let l = lines.next().expect("stream open").expect("line");
        status |= l == "event: status";
        levels |= l == "event: levels";
    }
    assert!(status && levels, "status {status}, levels {levels}");
}

#[test]
fn a_silent_client_cannot_hold_a_connection_forever() {
    // Sends nothing: the head timeout closes it; the page still answers.
    let r = rig(25);
    let _idle = TcpStream::connect(r.addr).unwrap();
    let resp = request(
        r.addr,
        "GET /public/status HTTP/1.1\r\nHost: test.invalid\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("\"stream\":\"live\""), "{resp}");
}

/// Whether the server closes a fresh connection at once (refused) rather
/// than waiting for a request.
fn refused_at_once(addr: SocketAddr) -> bool {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut b = [0u8; 1];
    match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

/// Idle connections, each accepted before the next is opened (one request
/// through behind them proves the accept loop reached them).
fn hold(addr: SocketAddr, n: usize) -> Vec<TcpStream> {
    (0..n).map(|_| TcpStream::connect(addr).unwrap()).collect()
}

#[test]
fn a_trickling_client_is_cut_off_at_the_deadline() {
    let r = rig_with(
        25,
        Limits {
            head_deadline: Duration::from_millis(300),
            ..Limits::default()
        },
        false,
    );
    let mut s = TcpStream::connect(r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut w = s.try_clone().unwrap();
    // One byte every 50 ms: each read finishes well inside any per-read
    // timeout, and the head never ends.
    let trickle = std::thread::spawn(move || {
        let _ = w.write_all(b"GET / HTTP/1.1\r\nX-Slow: ");
        for _ in 0..80 {
            if w.write_all(b"a").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(1500),
        "held for {took:?}: {}",
        String::from_utf8_lossy(&out)
    );
    drop(s);
    let _ = trickle.join();
}

#[test]
fn one_address_cannot_hold_more_than_its_share() {
    let r = rig_with(
        25,
        Limits {
            conns_per_ip: 2,
            ..Limits::default()
        },
        false,
    );
    let mut idle = hold(r.addr, 2);
    assert!(refused_at_once(r.addr), "the third from 127.0.0.1");
    idle.pop();
    wait_for("a freed slot", || {
        request(
            r.addr,
            "GET /public/status HTTP/1.1\r\nHost: test.invalid\r\n\r\n",
        )
        .starts_with("HTTP/1.1 200")
    });
}

#[test]
fn port_80_answers_while_the_public_bind_is_full() {
    let r = rig_with(
        25,
        Limits {
            max_connections: 2,
            ..Limits::default()
        },
        true,
    );
    r.challenges
        .lock()
        .unwrap()
        .insert("tok".into(), "tok.thumb".into());
    let _idle = hold(r.addr, 2);
    assert!(refused_at_once(r.addr), "the public bind is full");
    let resp = request(
        r.http80.unwrap(),
        "GET /.well-known/acme-challenge/tok HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with("tok.thumb"), "{resp}");
}

/// The status line of a GET for `path` that is left open, and the stream.
fn open_stream(addr: SocketAddr, path: &str) -> (String, BufReader<TcpStream>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {HOST}\r\n\r\n").unwrap();
    let mut r = BufReader::new(s);
    let mut status = String::new();
    r.read_line(&mut status).unwrap();
    (status, r)
}

#[test]
fn one_address_cannot_hold_every_audio_slot() {
    let r = rig_with(
        25,
        Limits {
            streams_per_ip: 2,
            ..Limits::default()
        },
        false,
    );
    let _a = ws_connect(r.addr).expect("first listener");
    let _b = ws_connect(r.addr).expect("second listener");
    match ws_connect(r.addr).map_err(|e| *e) {
        Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status().as_u16(), 429),
        other => panic!("expected an HTTP 429, got {other:?}"),
    }
    assert_eq!(r.count.load(Ordering::SeqCst), 2, "no third audio slot");
    // The event feed counts against the same share.
    let (status, _) = open_stream(r.addr, "/public/events");
    assert!(status.starts_with("HTTP/1.1 429"), "{status}");
}

#[test]
fn event_streams_have_a_cap_of_their_own() {
    let r = rig_with(
        25,
        Limits {
            max_event_streams: 1,
            ..Limits::default()
        },
        false,
    );
    let (status, first) = open_stream(r.addr, "/public/events");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let (status, _) = open_stream(r.addr, "/public/events");
    assert!(status.starts_with("HTTP/1.1 503"), "{status}");
    // Audio is not an event stream.
    let _ws = ws_connect(r.addr).expect("a listener still gets in");
    drop(first);
    wait_for("the event slot to be freed", || {
        open_stream(r.addr, "/public/events")
            .0
            .starts_with("HTTP/1.1 200")
    });
}
