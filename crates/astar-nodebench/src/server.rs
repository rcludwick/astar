// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar-server control-channel client: `GET /status`, the `POST` commands,
//! and the SSE `/events` stream on a reader thread.
//!
//! Shapes follow `crates/astar-server/src/http.rs` and
//! `docs/site/server/control-api.md`. astar-server's own snapshot types are
//! serialize-only, so this module mirrors them with lenient `Deserialize`
//! structs (every field defaulted) and keeps the raw JSON alongside, so a new
//! server field never breaks the bench.
//!
//! The control channel is unauthenticated and can key a transmitter. The bench
//! only ever talks to it on 127.0.0.1, where the only "transmitter" is the
//! containerised node.

use std::fmt;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One link in `/status` (`astar_iax::LinkSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LinkInfo {
    pub call: u64,
    pub node: String,
    /// `transceive` / `monitor` / `local_monitor`.
    pub mode: String,
    /// `connecting` / `up`.
    pub state: String,
    pub keyed: bool,
    pub rx_active: bool,
    pub up_secs: u64,
    pub addr: Option<String>,
}

impl LinkInfo {
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.state == "up"
    }
}

/// One call in `/status` (`astar_iax::CallSnapshot`), the fields the bench
/// reads. Anything else is in [`Status::raw`].
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CallInfo {
    /// `CallId` — a bare number on the wire.
    pub id: Value,
    pub node: String,
    pub keyed: bool,
    /// `Connecting` / `Active` / `Hungup`.
    pub state: String,
    /// `astar_iax_core::VoiceFormat` as serialized (e.g. `"G711U"`); `None`
    /// while connecting. Kept as a `Value` so a representation change cannot
    /// break the whole snapshot; read it with [`CallInfo::format`].
    pub negotiated_format: Option<Value>,
    pub remote_keyed: bool,
    pub rx_frames_lost: u64,
    pub rx_frames_late: u64,
    pub rx_underruns: u64,
    pub rx_jitter_ms: u32,
}

impl CallInfo {
    /// The negotiated codec as text (`"G711U"`), if known.
    #[must_use]
    pub fn format(&self) -> Option<String> {
        match self.negotiated_format.as_ref()? {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }
}

/// `GET /status`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Status {
    pub node_id: Option<String>,
    pub listening: bool,
    pub registered: bool,
    pub calls: Vec<CallInfo>,
    pub links: Vec<LinkInfo>,
    /// The whole response, for fields this struct does not name.
    #[serde(skip)]
    pub raw: Value,
}

impl Status {
    /// Parse a snapshot value, keeping the raw JSON.
    pub fn from_value(v: Value) -> Result<Self, serde_json::Error> {
        let mut s: Self = serde_json::from_value(v.clone())?;
        s.raw = v;
        Ok(s)
    }

    /// The link to `node`, if any.
    #[must_use]
    pub fn link(&self, node: &str) -> Option<&LinkInfo> {
        self.links.iter().find(|l| l.node == node)
    }

    /// The link to `node` exists and is `up`.
    #[must_use]
    pub fn link_up(&self, node: &str) -> bool {
        self.link(node).is_some_and(LinkInfo::is_up)
    }
}

/// `POST /link` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkAction {
    Connect,
    Monitor,
    Disconnect,
}

/// The reply to a command: a snapshot, or a bare `{"ok":true}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok,
    Snapshot(Box<Status>),
}

/// A control-channel failure.
#[derive(Debug)]
pub enum ServerError {
    /// Could not reach the server.
    Transport(String),
    /// The server answered with an error status: `{"error": ...}`.
    Status { code: u16, message: String },
    /// The body was not what the API documents.
    Decode(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "astar-server unreachable: {e}"),
            Self::Status { code, message } => write!(f, "astar-server {code}: {message}"),
            Self::Decode(e) => write!(f, "astar-server reply: {e}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// Client for one astar-server control channel.
#[derive(Clone)]
pub struct ServerClient {
    base: String,
    agent: ureq::Agent,
}

impl fmt::Debug for ServerClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

fn map_err(e: ureq::Error) -> ServerError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or(body);
            ServerError::Status { code, message }
        }
        ureq::Error::Transport(t) => ServerError::Transport(t.to_string()),
    }
}

impl ServerClient {
    /// `base` like `http://127.0.0.1:8730` (a trailing `/` is fine).
    #[must_use]
    pub fn new(base: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build();
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `GET /status`.
    pub fn status(&self) -> Result<Status, ServerError> {
        let v: Value = self
            .agent
            .get(&self.url("/status"))
            .call()
            .map_err(map_err)?
            .into_string()
            .map_err(|e| ServerError::Decode(e.to_string()))
            .and_then(|b| decode(&b))?;
        Status::from_value(v).map_err(|e| ServerError::Decode(e.to_string()))
    }

    /// `POST <path>` with a JSON body (or `{}`).
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply, ServerError> {
        let v: Value = self
            .agent
            .post(&self.url(path))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
            .map_err(map_err)?
            .into_string()
            .map_err(|e| ServerError::Decode(e.to_string()))
            .and_then(|b| decode(&b))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(Reply::Ok);
        }
        Status::from_value(v)
            .map(|s| Reply::Snapshot(Box::new(s)))
            .map_err(|e| ServerError::Decode(e.to_string()))
    }

    /// `POST /link {action, node, addr?}`. `addr` dials an explicit
    /// `host:port`, bypassing `AllStar` DNS — the bench always passes it.
    pub fn link(
        &self,
        action: LinkAction,
        node: &str,
        addr: Option<&str>,
    ) -> Result<Reply, ServerError> {
        self.post("/link", &link_body(action, node, addr))
    }

    /// `POST /dial {node}`.
    pub fn dial(&self, node: &str) -> Result<Reply, ServerError> {
        self.post("/dial", &serde_json::json!({ "node": node }))
    }

    pub fn hangup(&self) -> Result<Reply, ServerError> {
        self.post("/hangup", &serde_json::json!({}))
    }

    /// `POST /key`. Keys astar-server's transmit path; in the rig that path
    /// ends at the containerised node and the USRP probe, nowhere else.
    pub fn key(&self) -> Result<Reply, ServerError> {
        self.post("/key", &serde_json::json!({}))
    }

    pub fn unkey(&self) -> Result<Reply, ServerError> {
        self.post("/unkey", &serde_json::json!({}))
    }

    /// `POST /secrets {username, secret}`: provision a credential at runtime
    /// (inbound peer credentials are bare usernames). The reply never echoes
    /// it; the bench only ever passes its loopback-only test secret.
    pub fn provide_secret(&self, username: &str, secret: &str) -> Result<Reply, ServerError> {
        self.post(
            "/secrets",
            &serde_json::json!({ "username": username, "secret": secret }),
        )
    }

    pub fn register(&self) -> Result<Reply, ServerError> {
        self.post("/register", &serde_json::json!({}))
    }

    pub fn deregister(&self) -> Result<Reply, ServerError> {
        self.post("/deregister", &serde_json::json!({}))
    }

    /// Open `GET /events` on a reader thread.
    pub fn events(&self) -> Result<EventStream, ServerError> {
        // No overall timeout on the stream; the server's ~33 ms heartbeat
        // keeps reads short, so a read timeout catches a dead server.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(2))
            .timeout_read(Duration::from_secs(5))
            .build();
        let resp = agent
            .get(&self.url("/events"))
            .set("Accept", "text/event-stream")
            .call()
            .map_err(map_err)?;
        Ok(EventStream::spawn(Box::new(resp.into_reader())))
    }
}

fn decode(body: &str) -> Result<Value, ServerError> {
    serde_json::from_str(body).map_err(|e| ServerError::Decode(format!("{e}: {body:.200}")))
}

/// The `POST /link` body.
#[must_use]
pub fn link_body(action: LinkAction, node: &str, addr: Option<&str>) -> Value {
    let mut v = serde_json::json!({ "action": action, "node": node });
    if let Some(a) = addr {
        v["addr"] = Value::String(a.to_string());
    }
    v
}

// ---- events -----------------------------------------------------------------

/// A server event, typed where the bench cares.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    Snapshot(Box<Status>),
    /// `link`: `kind` is `connected` / `disconnected` / `keyed`.
    Link {
        kind: String,
        node: String,
        call: u64,
        reason: Option<String>,
        keyed: Option<bool>,
    },
    Hangup {
        reason: String,
    },
    IncomingCall {
        from: String,
    },
    Registered,
    RegisterFailed {
        reason: String,
    },
    /// Anything else (`dtmf`, `announcement_*`, future events).
    Other(Value),
}

fn str_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

impl ServerEvent {
    /// Decode one SSE `data:` JSON payload.
    #[must_use]
    pub fn from_value(v: Value) -> Self {
        match v.get("event").and_then(Value::as_str) {
            Some("snapshot") => match Status::from_value(v.clone()) {
                Ok(s) => Self::Snapshot(Box::new(s)),
                Err(_) => Self::Other(v),
            },
            Some("link") => Self::Link {
                kind: str_field(&v, "kind"),
                node: str_field(&v, "node"),
                call: v.get("call").and_then(Value::as_u64).unwrap_or(0),
                reason: v.get("reason").and_then(Value::as_str).map(str::to_string),
                keyed: v.get("keyed").and_then(Value::as_bool),
            },
            Some("hangup") => Self::Hangup {
                reason: str_field(&v, "reason"),
            },
            Some("incoming_call") => Self::IncomingCall {
                from: str_field(&v, "from"),
            },
            Some("registered") => Self::Registered,
            Some("register_failed") => Self::RegisterFailed {
                reason: str_field(&v, "reason"),
            },
            _ => Self::Other(v),
        }
    }
}

/// An event with when it arrived (relative to the stream opening).
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub at: Duration,
    pub event: ServerEvent,
}

/// Feed SSE lines; returns the JSON of each completed `data:` event.
/// Handles multi-line `data:` fields and ignores comments / other fields.
#[derive(Debug, Default)]
pub struct SseParser {
    data: String,
}

impl SseParser {
    /// One line without its terminator. Returns a complete event on the
    /// blank line that ends it.
    pub fn line(&mut self, line: &str) -> Option<String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data));
        }
        if let Some(d) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
        None
    }
}

/// The SSE stream on its reader thread.
///
/// Heartbeat snapshots (the server sends one every ~33 ms when idle) do not
/// go down the channel; the latest one is kept in [`EventStream::latest`]
/// instead. Every other event is sent, timestamped.
pub struct EventStream {
    rx: Receiver<Stamped>,
    latest: Arc<Mutex<Option<Stamped>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Option<String>>>,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

impl EventStream {
    /// Read SSE from any reader on a new thread.
    #[must_use]
    pub fn spawn(reader: Box<dyn std::io::Read + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (l2, s2) = (latest.clone(), stop.clone());
        let handle = std::thread::Builder::new()
            .name("nodebench-sse".into())
            .spawn(move || {
                let origin = Instant::now();
                let mut parser = SseParser::default();
                let mut r = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    if s2.load(Ordering::Relaxed) {
                        return None;
                    }
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) => return Some("stream closed".to_string()),
                        Ok(_) => {}
                        Err(e) => return Some(e.to_string()),
                    }
                    let Some(data) = parser.line(&line) else {
                        continue;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    let st = Stamped {
                        at: origin.elapsed(),
                        event: ServerEvent::from_value(v),
                    };
                    if matches!(st.event, ServerEvent::Snapshot(_)) {
                        if let Ok(mut g) = l2.lock() {
                            *g = Some(st);
                        }
                    } else if tx.send(st).is_err() {
                        return None;
                    }
                }
            })
            .ok();
        Self {
            rx,
            latest,
            stop,
            handle,
        }
    }

    /// Next non-snapshot event, waiting up to `timeout`.
    #[must_use]
    pub fn next(&self, timeout: Duration) -> Option<Stamped> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Everything queued right now.
    #[must_use]
    pub fn drain(&self) -> Vec<Stamped> {
        self.rx.try_iter().collect()
    }

    /// The most recent snapshot (heartbeat or change).
    #[must_use]
    pub fn latest(&self) -> Option<Stamped> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }

    /// Whether the reader thread has ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Ask the reader to stop. It exits at its next line (the heartbeat
    /// makes that prompt); a blocked read ends at the read timeout.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop();
        // Do not join: a read can block up to the read timeout.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "node_id": "1998", "listening": true, "registered": false,
      "calls": [{"id": 3, "node": "1999", "mode": "Transceive", "keyed": false,
                 "state": "Active", "routed_mic": null, "output": "bus0",
                 "tx_reanchors": 0, "tx_capture_overruns": 0,
                 "negotiated_format": "G711U", "remote_keyed": true,
                 "rx_frames_lost": 2, "a_future_field": 1}],
      "links": [{"call": 3, "node": "1999", "mode": "transceive", "state": "up",
                 "keyed": false, "rx_active": true, "up_secs": 61,
                 "addr": "127.0.0.1:4569"}]
    }"#;

    #[test]
    fn status_parses_and_keeps_raw() {
        let s = Status::from_value(serde_json::from_str(STATUS).unwrap()).unwrap();
        assert_eq!(s.node_id.as_deref(), Some("1998"));
        assert!(s.listening && !s.registered);
        assert!(s.link_up("1999"));
        assert!(!s.link_up("2000"));
        assert_eq!(s.link("1999").unwrap().up_secs, 61);
        assert_eq!(s.calls[0].format().as_deref(), Some("G711U"));
        assert_eq!(s.calls[0].rx_frames_lost, 2);
        assert!(s.calls[0].remote_keyed);
        assert_eq!(s.raw["calls"][0]["a_future_field"], 1);
    }

    #[test]
    fn minimal_status_defaults() {
        let s = Status::from_value(serde_json::json!({"listening": true})).unwrap();
        assert!(s.links.is_empty() && s.calls.is_empty());
        assert_eq!(s.node_id, None);
    }

    #[test]
    fn link_body_shape() {
        assert_eq!(
            link_body(LinkAction::Connect, "1999", Some("127.0.0.1:4569")),
            serde_json::json!({"action":"connect","node":"1999","addr":"127.0.0.1:4569"})
        );
        assert_eq!(
            link_body(LinkAction::Disconnect, "1999", None),
            serde_json::json!({"action":"disconnect","node":"1999"})
        );
        assert_eq!(
            link_body(LinkAction::Monitor, "1", None)["action"],
            "monitor"
        );
    }

    #[test]
    fn sse_parser_frames() {
        let mut p = SseParser::default();
        assert_eq!(p.line(": comment"), None);
        assert_eq!(p.line("data: {\"a\":"), None);
        assert_eq!(p.line("data: 1}"), None);
        assert_eq!(p.line(""), Some("{\"a\":\n1}".to_string()));
        assert_eq!(p.line(""), None);
    }

    #[test]
    fn events_decode() {
        let e = ServerEvent::from_value(serde_json::json!({
            "event":"link","kind":"disconnected","node":"1999","call":4,"reason":"peer hangup"}));
        assert_eq!(
            e,
            ServerEvent::Link {
                kind: "disconnected".into(),
                node: "1999".into(),
                call: 4,
                reason: Some("peer hangup".into()),
                keyed: None
            }
        );
        assert!(matches!(
            ServerEvent::from_value(
                serde_json::json!({"event":"snapshot","listening":true,"calls":[],"links":[]})
            ),
            ServerEvent::Snapshot(_)
        ));
        assert!(matches!(
            ServerEvent::from_value(serde_json::json!({"event":"dtmf","call":1,"digit":"5"})),
            ServerEvent::Other(_)
        ));
    }

    #[test]
    fn event_stream_splits_snapshots_from_events() {
        let body = concat!(
            "data: {\"event\":\"snapshot\",\"listening\":true,\"calls\":[],\"links\":[]}\n\n",
            "data: {\"event\":\"link\",\"kind\":\"connected\",\"node\":\"1999\",\"call\":1}\n\n",
            "data: {\"event\":\"snapshot\",\"listening\":false,\"calls\":[],\"links\":[]}\n\n",
            "data: {\"event\":\"hangup\",\"reason\":\"bye\"}\n\n",
        );
        let es = EventStream::spawn(Box::new(std::io::Cursor::new(body.as_bytes().to_vec())));
        let a = es.next(Duration::from_secs(2)).unwrap();
        assert!(matches!(a.event, ServerEvent::Link { ref kind, .. } if kind == "connected"));
        let b = es.next(Duration::from_secs(2)).unwrap();
        assert_eq!(
            b.event,
            ServerEvent::Hangup {
                reason: "bye".into()
            }
        );
        // Wait for the reader to hit EOF, then the last snapshot is latest.
        let t0 = Instant::now();
        while !es.is_finished() && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let ServerEvent::Snapshot(s) = es.latest().unwrap().event else {
            panic!("latest is a snapshot");
        };
        assert!(!s.listening);
    }

    /// Round trip against a tiny HTTP server on 127.0.0.1 (no container).
    #[test]
    fn client_against_fake_server() {
        let stub = astar_http::testing::serve(|req| {
            let (code, reply) = match (req.method.as_str(), req.target.as_str()) {
                ("GET", "/status") | ("POST", "/link") => (200, STATUS.to_string()),
                ("POST", "/key") => (500, r#"{"error":"no active call"}"#.to_string()),
                _ => (404, r#"{"error":"not found"}"#.to_string()),
            };
            astar_http::Response::json(code, reply)
        });
        let c = ServerClient::new(&format!("{}/", stub.url()));
        assert!(c.status().unwrap().link_up("1999"));
        let r = c
            .link(LinkAction::Connect, "1999", Some("127.0.0.1:4569"))
            .unwrap();
        assert!(matches!(r, Reply::Snapshot(_)));
        match c.key() {
            Err(ServerError::Status { code, message }) => {
                assert_eq!(code, 500);
                assert_eq!(message, "no active call");
            }
            other => panic!("expected a 500, got {other:?}"),
        }
        let seen = stub.requests();
        assert_eq!(seen.len(), 3, "{seen:?}");
        let link = seen.iter().find(|r| r.target == "/link").unwrap();
        let v: Value = serde_json::from_str(link.body_str()).unwrap();
        assert_eq!(v["addr"], "127.0.0.1:4569");
    }
}
