// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pushing the live stream to `YouTube` and Twitch over RTMP(S).
//!
//! One [`Publisher`] per destination, each on its own thread with its own
//! socket, so a destination that fails or stalls never holds up the other.
//! `ffmpeg` encodes once to FLV; the caller reads that into [`FlvTag`]s and
//! [`Publisher::offer`]s each one to every publisher. An offer never blocks:
//! each publisher holds four seconds of tags, and when that fills it drops
//! tags until the next key frame, so a viewer never sees a smeared picture.
//!
//! `rml_rtmp` does the protocol and nothing else; the socket, TLS, timeouts
//! and reconnects are here. The stream key is a secret. It goes to the
//! server as the publish name and nowhere else: every error string is built
//! here from fixed text plus host and port, never from `rml_rtmp`'s own
//! errors and never from what the server says, because a server refusing a
//! publish often quotes the stream name back.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_secrets::Secret;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::rml_amf0::Amf0Value;
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType,
};
use rml_rtmp::time::RtmpTimestamp;
use serde::{Deserialize, Serialize};

use crate::flv::{FlvTag, TagKind};

/// How many tags a publisher holds: four seconds at 30 frames plus audio.
pub const QUEUE_TAGS: usize = 240;

// ---- the ingest URL ----

/// Where a destination's stream goes: `rtmps://host[:port]/app`, without the
/// stream key, which is kept apart as a [`Secret`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestUrl {
    /// `rtmps://`: RTMP inside TLS.
    pub tls: bool,
    /// The host name or IP address, without brackets.
    pub host: String,
    /// The TCP port: 443 for `rtmps://` and 1935 for `rtmp://` unless given.
    pub port: u16,
    /// The RTMP application, the single path segment (`live2`, `app`).
    pub app: String,
}

impl IngestUrl {
    /// Read an ingest URL. It must be `rtmps://`, or plain `rtmp://` to a
    /// loopback host (a local listener); have no `user@` part, no query and
    /// no fragment; and name exactly one path segment, the application. A
    /// second segment is where a pasted stream key would sit, so it is
    /// refused rather than sent in the clear of a config file.
    ///
    /// # Errors
    /// A sentence saying what is wrong. It names the scheme and host at most,
    /// never the path, the query or anything before an `@`.
    pub fn parse(url: &str) -> Result<Self, String> {
        // Nothing before `://` is echoed unless it is plainly a scheme: a bare
        // pasted key, or `live2/KEY://x`, must never reach the message.
        let not_url = || "not an rtmp:// or rtmps:// URL".to_string();
        let (scheme, rest) = url.split_once("://").ok_or_else(not_url)?;
        let mut chars = scheme.chars();
        let scheme_ok = (1..=10).contains(&scheme.len())
            && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
        if !scheme_ok {
            return Err(not_url());
        }
        let odd = || {
            format!(
                "{scheme}:// URL has an unusable host or port (no user@ part, a plain host or [IPv6] with an optional numeric port)"
            )
        };
        if url.contains('@') {
            return Err(odd());
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let port_of = |p: &str| {
            ((1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u16>().ok())
                .flatten()
                .filter(|&n| n != 0)
        };
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner.split_once(']').ok_or_else(odd)?;
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(odd());
            }
            match after {
                "" => (host, None),
                _ => (
                    host,
                    Some(after.strip_prefix(':').and_then(port_of).ok_or_else(odd)?),
                ),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port_of(port).ok_or_else(odd)?)),
                None => (authority, None),
            }
        };
        if host.is_empty() || has_odd_chars(host) {
            return Err(odd());
        }
        let tls = if scheme.eq_ignore_ascii_case("rtmps") {
            true
        } else if scheme.eq_ignore_ascii_case("rtmp") && is_loopback_host(host) {
            false
        } else {
            return Err(format!(
                "{scheme}://{host} must be an rtmps:// URL (plain rtmp:// is only for a loopback host, with no user@ part)"
            ));
        };
        let path = &rest[authority.len()..];
        if path.contains(['?', '#']) {
            return Err(format!(
                "{scheme}://{host}: the URL may not carry a query or fragment; the stream key belongs in credentials.toml"
            ));
        }
        let app = path.strip_prefix('/').unwrap_or(path);
        let app = app.strip_suffix('/').unwrap_or(app);
        if app.is_empty() {
            return Err(format!(
                "{scheme}://{host}: the URL names no application (such as /live2 or /app)"
            ));
        }
        if has_odd_chars(app) {
            return Err(format!(
                "{scheme}://{host}: the application name may not hold %, \\ or spaces"
            ));
        }
        if app.contains('/') {
            return Err(format!(
                "{scheme}://{host}: the URL has more than one path segment; give only the application, and put the stream key in credentials.toml"
            ));
        }
        Ok(Self {
            tls,
            host: host.to_string(),
            port: port.unwrap_or(if tls { 443 } else { 1935 }),
            app: app.to_string(),
        })
    }

    /// `host:port`, with an IPv6 address in brackets: the only part of a
    /// destination an error or a log line names.
    #[must_use]
    pub fn endpoint(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The `tcUrl` the server is told: the URL without any key.
    fn tc_url(&self) -> String {
        let scheme = if self.tls { "rtmps" } else { "rtmp" };
        format!("{scheme}://{}/{}", self.endpoint(), self.app)
    }
}

/// A percent escape, a backslash or any whitespace: nothing a real ingest
/// host or application has, and each a way to smuggle a second meaning past
/// the checks above.
fn has_odd_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c == '%' || c == '\\' || c.is_whitespace())
}

/// `localhost` (any case) or a loopback IP literal.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

// ---- destinations and their status ----

/// Which site a destination is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestName {
    /// `YouTube` Live.
    YouTube,
    /// Twitch.
    Twitch,
}

impl DestName {
    /// The lowercase name the state file and the page use.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::YouTube => "youtube",
            Self::Twitch => "twitch",
        }
    }
}

impl fmt::Display for DestName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::YouTube => "YouTube",
            Self::Twitch => "Twitch",
        })
    }
}

/// One place to stream to. `Debug` shows the key as `Secret(***)`.
#[derive(Clone, Debug)]
pub struct Destination {
    /// Which site.
    pub name: DestName,
    /// Where its ingest server is.
    pub ingest: IngestUrl,
    /// The stream key, sent only as the RTMP publish name.
    pub key: Secret,
}

/// Where a destination stands.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DestState {
    /// Opening the connection.
    Connecting,
    /// Publishing.
    Live,
    /// Waiting to try again after a failure.
    Retrying,
    /// Stopped.
    Off,
}

/// A destination's status, for the state file and the page. Holds no key.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DestStatus {
    /// `youtube` or `twitch`.
    pub name: String,
    /// Where it stands.
    pub state: DestState,
    /// How many times it has had to connect again.
    pub reconnects: u32,
    /// The latest failure, cleared when it goes live again.
    pub last_error: Option<String>,
}

/// The latest AVC and AAC sequence headers, shared by every publisher so a
/// (re)connection can start a decoder before the first frame it sends.
#[derive(Clone, Debug, Default)]
pub struct SeqHeaders {
    /// The AVC decoder configuration.
    pub video: Option<Arc<FlvTag>>,
    /// The AAC decoder configuration.
    pub audio: Option<Arc<FlvTag>>,
}

impl SeqHeaders {
    /// Keep `tag` if it is a sequence header.
    pub fn observe(&mut self, tag: &Arc<FlvTag>) {
        if tag.seq_header {
            match tag.kind {
                TagKind::Video => self.video = Some(Arc::clone(tag)),
                TagKind::Audio => self.audio = Some(Arc::clone(tag)),
                TagKind::Script => {}
            }
        }
    }
}

// ---- timing ----

/// Timeouts and backoff.
#[derive(Clone, Debug)]
pub(crate) struct Tuning {
    /// Connect, read and write timeout on the socket.
    pub io_timeout: Duration,
    /// The first wait before connecting again.
    pub backoff_min: Duration,
    /// The longest wait.
    pub backoff_max: Duration,
    /// Live this long and the next wait starts from `backoff_min` again.
    pub reset_after: Duration,
    /// The TLS setup in place of the Mozilla roots: a test's own
    /// self-signed root. Test builds only; a shipped build always verifies
    /// against `webpki-roots`.
    #[cfg(test)]
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            io_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
            #[cfg(test)]
            tls: None,
        }
    }
}

/// The wait after `cur`: double it, up to the cap.
pub(crate) fn next_backoff(cur: Duration, t: &Tuning) -> Duration {
    (cur * 2).min(t.backoff_max)
}

/// A jump back larger than this is an encoder restart, not interleaving.
const RESTART_JUMP_MS: i64 = 1000;

/// Maps encoder timestamps onto the published timeline so it never goes
/// backwards: not across a reconnect (the timeline simply carries on), and
/// not across an encoder restart, whose stream starts again from zero.
#[derive(Default)]
pub(crate) struct Rebase {
    offset: i64,
    last_out: Option<i64>,
}

impl Rebase {
    pub(crate) fn map(&mut self, ts: u32) -> u32 {
        let ts = i64::from(ts);
        let mut out = ts + self.offset;
        if let Some(last) = self.last_out {
            if out < last - RESTART_JUMP_MS {
                self.offset = last + 1 - ts;
                out = last + 1;
            } else if out < last {
                // Audio a little behind video in the interleave: hold.
                out = last;
            }
        }
        self.last_out = Some(out);
        u32::try_from(out).unwrap_or(u32::MAX)
    }

    /// The latest time sent, for the sequence headers of a new connection.
    fn now(&self) -> u32 {
        u32::try_from(self.last_out.unwrap_or(0)).unwrap_or(u32::MAX)
    }
}

// ---- the producer side ----

/// The never-blocking end of a publisher's queue.
pub(crate) struct Feed {
    tx: SyncSender<Arc<FlvTag>>,
    /// Set when a tag was dropped: nothing but sequence headers goes in
    /// until the next key frame does.
    waiting_for_key: Mutex<bool>,
}

impl Feed {
    pub(crate) fn new(tx: SyncSender<Arc<FlvTag>>) -> Self {
        Self {
            tx,
            waiting_for_key: Mutex::new(false),
        }
    }

    pub(crate) fn offer(&self, tag: Arc<FlvTag>) {
        let mut waiting = lock(&self.waiting_for_key);
        if *waiting && !(tag.keyframe || tag.seq_header) {
            return;
        }
        let key = tag.keyframe;
        match self.tx.try_send(tag) {
            Ok(()) => {
                if key {
                    *waiting = false;
                }
            }
            Err(TrySendError::Full(_)) => *waiting = true,
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- the publisher ----

struct Shared {
    status: Mutex<DestStatus>,
    stop: AtomicBool,
    /// A handle on the live socket, so `stop` can cut a blocked read or write.
    sock: Mutex<Option<TcpStream>>,
}

/// One destination's RTMP(S) connection, kept up on its own thread.
pub struct Publisher {
    feed: Option<Feed>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Publisher {
    /// Start publishing to `dest`. Returns at once; the connection is made,
    /// and remade after any failure, on the publisher's own thread.
    #[must_use]
    pub fn spawn(dest: Destination, headers: Arc<Mutex<SeqHeaders>>) -> Self {
        Self::spawn_with(dest, headers, Tuning::default())
    }

    pub(crate) fn spawn_with(
        dest: Destination,
        headers: Arc<Mutex<SeqHeaders>>,
        tuning: Tuning,
    ) -> Self {
        // Tests reach 127.0.0.1 and nothing else, checked before any socket.
        // Other crates' tests get the same guard through `testing`.
        #[cfg(any(test, feature = "testing"))]
        assert!(
            is_loopback_host(&dest.ingest.host),
            "tests may publish only to a loopback host, not {}",
            dest.ingest.host
        );
        let (tx, rx) = sync_channel(QUEUE_TAGS);
        let shared = Arc::new(Shared {
            status: Mutex::new(DestStatus {
                name: dest.name.slug().to_string(),
                state: DestState::Connecting,
                reconnects: 0,
                last_error: None,
            }),
            stop: AtomicBool::new(false),
            sock: Mutex::new(None),
        });
        // The thread logs to whatever subscriber its starter had.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let worker = Worker {
            dest,
            headers,
            rx,
            shared: Arc::clone(&shared),
            tuning,
            clock: Rebase::default(),
        };
        let handle = std::thread::Builder::new()
            .name(format!("rtmp-{}", worker.dest.name.slug()))
            .spawn(move || tracing::dispatcher::with_default(&dispatch, || worker.run()));
        if handle.is_err() {
            let mut s = lock(&shared.status);
            s.state = DestState::Off;
            s.last_error = Some("could not start the publisher thread".to_string());
        }
        Self {
            feed: Some(Feed::new(tx)),
            shared,
            worker: handle.ok(),
        }
    }

    /// Queue a tag for sending. Never blocks; see the module notes.
    pub fn offer(&self, tag: Arc<FlvTag>) {
        if let Some(feed) = &self.feed {
            feed.offer(tag);
        }
    }

    /// Where this destination stands now.
    #[must_use]
    pub fn status(&self) -> DestStatus {
        lock(&self.shared.status).clone()
    }

    /// Close the connection and wait for the thread to finish.
    pub fn stop(mut self) {
        self.halt();
    }

    /// Tell the thread to stop and cut its socket, without waiting for it.
    /// Signalling every publisher first and then dropping them lets a stop
    /// wait for the slowest one rather than for the sum of them all.
    pub(crate) fn begin_stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(sock) = lock(&self.shared.sock).as_ref() {
            let _ = sock.shutdown(Shutdown::Both);
        }
    }

    fn halt(&mut self) {
        self.begin_stop();
        self.feed = None;
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        lock(&self.shared.status).state = DestState::Off;
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.halt();
    }
}

/// A failure, already worded for the log and the page. Built only from fixed
/// text and the endpoint.
struct Failure(String);

/// Why a live session ended without a failure.
enum Ended {
    /// `stop` was called, or the feed is gone.
    Stopped,
}

trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}

/// The phase of the opening exchange.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connect,
    Publish,
}

struct Worker {
    dest: Destination,
    headers: Arc<Mutex<SeqHeaders>>,
    rx: Receiver<Arc<FlvTag>>,
    shared: Arc<Shared>,
    tuning: Tuning,
    clock: Rebase,
}

impl Worker {
    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    fn fail(&self, what: &str) -> Failure {
        Failure(format!("{what} ({})", self.dest.ingest.endpoint()))
    }

    fn set_state(&self, state: DestState) {
        lock(&self.shared.status).state = state;
    }

    fn run(mut self) {
        let mut backoff = self.tuning.backoff_min;
        let mut first = true;
        while !self.stopped() {
            if !first {
                lock(&self.shared.status).reconnects += 1;
            }
            first = false;
            self.set_state(DestState::Connecting);
            let mut live_since = None;
            let result = self.session(&mut live_since);
            *lock(&self.shared.sock) = None;
            let failure = match result {
                Ok(Ended::Stopped) => break,
                Err(_) if self.stopped() => break,
                Err(f) => f,
            };
            tracing::warn!(
                destination = %self.dest.name,
                "live stream to {}: {}; trying again",
                self.dest.name,
                failure.0
            );
            {
                let mut s = lock(&self.shared.status);
                s.state = DestState::Retrying;
                s.last_error = Some(failure.0);
            }
            if live_since.is_some_and(|t: Instant| t.elapsed() >= self.tuning.reset_after) {
                backoff = self.tuning.backoff_min;
            }
            if !self.wait(backoff) {
                break;
            }
            backoff = next_backoff(backoff, &self.tuning);
        }
        self.set_state(DestState::Off);
    }

    /// Sit out `d`, throwing away what is queued (it would be stale by the
    /// time it went out). `false` when stopped meanwhile.
    fn wait(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        loop {
            if self.stopped() {
                return false;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            match self.rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn open(&self) -> Result<(TcpStream, Box<dyn Conn>), Failure> {
        let ingest = &self.dest.ingest;
        let addrs: Vec<SocketAddr> = (ingest.host.as_str(), ingest.port)
            .to_socket_addrs()
            .map_err(|_| self.fail("could not look up the host"))?
            .collect();
        #[cfg(any(test, feature = "testing"))]
        assert!(
            addrs.iter().all(|a| a.ip().is_loopback()),
            "tests may publish only to a loopback address"
        );
        // Each address can take the whole timeout, so a stop is looked for
        // between them rather than after all of them.
        let mut tcp = None;
        for a in &addrs {
            if self.stopped() {
                return Err(self.fail("stopped"));
            }
            if let Ok(s) = TcpStream::connect_timeout(a, self.tuning.io_timeout) {
                tcp = Some(s);
                break;
            }
        }
        let tcp = tcp.ok_or_else(|| self.fail("could not connect"))?;
        let t = Some(self.tuning.io_timeout);
        tcp.set_read_timeout(t)
            .and_then(|()| tcp.set_write_timeout(t))
            .and_then(|()| tcp.set_nodelay(true))
            .map_err(|_| self.fail("could not set up the socket"))?;
        let handle = tcp
            .try_clone()
            .map_err(|_| self.fail("could not set up the socket"))?;
        *lock(&self.shared.sock) = Some(
            handle
                .try_clone()
                .map_err(|_| self.fail("could not set up the socket"))?,
        );
        if self.stopped() {
            let _ = handle.shutdown(Shutdown::Both);
        }
        let conn: Box<dyn Conn> = if ingest.tls {
            let name = rustls::pki_types::ServerName::try_from(ingest.host.clone())
                .map_err(|_| self.fail("the host is not a valid TLS name"))?;
            #[cfg(test)]
            let config = self.tuning.tls.clone().unwrap_or_else(tls_config);
            #[cfg(not(test))]
            let config = tls_config();
            let tls = rustls::ClientConnection::new(config, name)
                .map_err(|_| self.fail("could not start TLS"))?;
            Box::new(rustls::StreamOwned::new(tls, tcp))
        } else {
            Box::new(tcp)
        };
        Ok((handle, conn))
    }

    fn io_fail(&self, e: &io::Error, doing: &str) -> Failure {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                self.fail(&format!("timed out {doing}"))
            }
            io::ErrorKind::InvalidData => self.fail("TLS failed"),
            _ => self.fail("lost the connection"),
        }
    }

    fn write(&self, conn: &mut dyn Conn, bytes: &[u8]) -> Result<(), Failure> {
        conn.write_all(bytes)
            .and_then(|()| conn.flush())
            .map_err(|e| self.io_fail(&e, "sending to the server"))
    }

    fn read(&self, conn: &mut dyn Conn, buf: &mut [u8]) -> Result<usize, Failure> {
        match conn.read(buf) {
            Ok(0) => Err(self.fail("the server closed the connection")),
            Ok(n) => Ok(n),
            Err(e) => Err(self.io_fail(&e, "waiting for the server")),
        }
    }

    /// Write what the session wants sent, in order; return its events.
    fn send(
        &self,
        conn: &mut dyn Conn,
        results: Vec<ClientSessionResult>,
    ) -> Result<Vec<ClientSessionEvent>, Failure> {
        let mut events = Vec::new();
        for r in results {
            match r {
                ClientSessionResult::OutboundResponse(p) => self.write(conn, &p.bytes)?,
                ClientSessionResult::RaisedEvent(e) => events.push(e),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(events)
    }

    /// One connection, from the TCP connect to the end of publishing.
    fn session(&mut self, live_since: &mut Option<Instant>) -> Result<Ended, Failure> {
        let (sock, mut conn) = self.open()?;
        let conn = conn.as_mut();
        let mut buf = vec![0u8; 16 * 1024];

        let mut hs = Handshake::new(PeerType::Client);
        let hello = hs
            .generate_outbound_p0_and_p1()
            .map_err(|_| self.fail("the RTMP handshake failed"))?;
        self.write(conn, &hello)?;
        let mut input = loop {
            let n = self.read(conn, &mut buf)?;
            match hs.process_bytes(&buf[..n]) {
                Ok(HandshakeProcessResult::InProgress { response_bytes }) => {
                    if !response_bytes.is_empty() {
                        self.write(conn, &response_bytes)?;
                    }
                }
                Ok(HandshakeProcessResult::Completed {
                    response_bytes,
                    remaining_bytes,
                }) => {
                    if !response_bytes.is_empty() {
                        self.write(conn, &response_bytes)?;
                    }
                    break remaining_bytes;
                }
                Err(_) => return Err(self.fail("the RTMP handshake failed")),
            }
        };

        let mut config = ClientSessionConfig::new();
        config.tc_url = Some(self.dest.ingest.tc_url());
        let protocol = || self.fail("the server sent something unexpected");
        let (mut session, initial) = ClientSession::new(config).map_err(|_| protocol())?;
        self.send(conn, initial)?;
        let r = session
            .request_connection(self.dest.ingest.app.clone())
            .map_err(|_| protocol())?;
        self.send(conn, vec![r])?;
        let mut phase = Phase::Connect;
        loop {
            if !input.is_empty() {
                let results = session.handle_input(&input).map_err(|_| protocol())?;
                input.clear();
                for event in self.send(conn, results)? {
                    match self.classify(phase, &event)? {
                        Some(Phase::Connect) => {
                            // The publish name is the stream key: here, and
                            // in no other place.
                            let r = session
                                .request_publishing(
                                    self.dest.key.expose().to_string(),
                                    PublishRequestType::Live,
                                )
                                .map_err(|_| protocol())?;
                            self.send(conn, vec![r])?;
                            phase = Phase::Publish;
                        }
                        Some(Phase::Publish) => return self.live(conn, &sock, session, live_since),
                        None => {}
                    }
                }
            }
            let n = self.read(conn, &mut buf)?;
            input.extend_from_slice(&buf[..n]);
        }
    }

    /// What an event during the opening exchange means: a phase completed,
    /// nothing, or a refusal. The server's own words are never kept.
    fn classify(&self, phase: Phase, event: &ClientSessionEvent) -> Result<Option<Phase>, Failure> {
        match event {
            ClientSessionEvent::ConnectionRequestAccepted if phase == Phase::Connect => {
                Ok(Some(Phase::Connect))
            }
            ClientSessionEvent::ConnectionRequestRejected { .. } => {
                Err(self.fail("the server refused the connection to the application"))
            }
            ClientSessionEvent::PublishRequestAccepted if phase == Phase::Publish => {
                Ok(Some(Phase::Publish))
            }
            ClientSessionEvent::UnhandleableOnStatusCode { code } if phase == Phase::Publish => {
                if code.contains("Publish") && !code.ends_with(".Start") {
                    Err(self.refusal(Some(code)))
                } else {
                    Ok(None)
                }
            }
            ClientSessionEvent::UnknownTransactionResultReceived {
                additional_values, ..
            } if phase == Phase::Publish => {
                let status = additional_values.iter().find_map(|v| match v {
                    Amf0Value::Object(o) => Some(o),
                    _ => None,
                });
                let text = |k: &str| match status.and_then(|o| o.get(k)) {
                    Some(Amf0Value::Utf8String(s)) => Some(s.as_str()),
                    _ => None,
                };
                if text("level").is_some_and(|l| l.contains("error")) {
                    Err(self.refusal(text("code")))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn refusal(&self, code: Option<&str>) -> Failure {
        if code.is_some_and(|c| c.ends_with(".BadName")) {
            self.fail("the server refused the stream key")
        } else {
            self.fail("the server refused to publish the stream")
        }
    }

    /// Publishing: sequence headers, then wait for a key frame, then every
    /// tag as it comes.
    fn live(
        &mut self,
        conn: &mut dyn Conn,
        sock: &TcpStream,
        mut session: ClientSession,
        live_since: &mut Option<Instant>,
    ) -> Result<Ended, Failure> {
        {
            let mut s = lock(&self.shared.status);
            s.state = DestState::Live;
            s.last_error = None;
        }
        *live_since = Some(Instant::now());
        tracing::info!(
            destination = %self.dest.name,
            "live stream to {} is up ({})",
            self.dest.name,
            self.dest.ingest.endpoint()
        );
        // A slow connect leaves up to `QUEUE_TAGS` (four seconds) queued,
        // and they go out below in one burst, as fast as the socket takes
        // them. That is on purpose: the ingest buffers, and their timestamps
        // keep the picture at the right pace; dropping them instead would
        // open the stream on a gap. Script tags (`onMetaData`) are never
        // forwarded: the ingests read size and rate from the AVC and AAC
        // sequence headers.
        let headers = lock(&self.headers).clone();
        let now = self.clock.now();
        for tag in [headers.video, headers.audio].into_iter().flatten() {
            self.publish(conn, &mut session, &tag, now)?;
        }
        let mut need_key = true;
        let mut last_poll = Instant::now();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if self.stopped() {
                return Ok(Ended::Stopped);
            }
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(tag) => {
                    let send = match tag.kind {
                        TagKind::Script => false,
                        _ if tag.seq_header => true,
                        TagKind::Video if need_key && tag.keyframe => {
                            need_key = false;
                            true
                        }
                        _ => !need_key,
                    };
                    if send {
                        let ts = self.clock.map(tag.ts_ms);
                        self.publish(conn, &mut session, &tag, ts)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Stopped),
            }
            if last_poll.elapsed() >= Duration::from_millis(100) {
                self.poll(conn, sock, &mut session, &mut buf)?;
                last_poll = Instant::now();
            }
        }
    }

    fn publish(
        &self,
        conn: &mut dyn Conn,
        session: &mut ClientSession,
        tag: &FlvTag,
        ts: u32,
    ) -> Result<(), Failure> {
        let ts = RtmpTimestamp::new(ts);
        let r = match tag.kind {
            TagKind::Video => session.publish_video_data(tag.data.clone(), ts, false),
            TagKind::Audio => session.publish_audio_data(tag.data.clone(), ts, false),
            TagKind::Script => return Ok(()),
        }
        .map_err(|_| self.fail("the stream could not be packed for sending"))?;
        self.send(conn, vec![r]).map(|_| ())
    }

    /// Read whatever the server has sent (acknowledgement windows, pings, a
    /// close) without waiting for more.
    fn poll(
        &self,
        conn: &mut dyn Conn,
        sock: &TcpStream,
        session: &mut ClientSession,
        buf: &mut [u8],
    ) -> Result<(), Failure> {
        let short = Some(Duration::from_millis(1));
        sock.set_read_timeout(short)
            .map_err(|_| self.fail("lost the connection"))?;
        let got = conn.read(buf);
        sock.set_read_timeout(Some(self.tuning.io_timeout))
            .map_err(|_| self.fail("lost the connection"))?;
        match got {
            Ok(0) => Err(self.fail("the server closed the connection")),
            Ok(n) => {
                let results = session
                    .handle_input(&buf[..n])
                    .map_err(|_| self.fail("the server sent something unexpected"))?;
                self.send(conn, results).map(|_| ())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(self.io_fail(&e, "waiting for the server")),
        }
    }
}

/// The TLS client setup: ring, the Mozilla roots from `webpki-roots`, and
/// SNI from the ingest host. Built once.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap_or_else(|_| unreachable!("ring supports the default protocol versions"))
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(config)
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::sync_channel;
    use std::time::Instant;

    use bytes::Bytes;
    use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
    use rml_rtmp::sessions::{
        ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult,
    };

    pub(crate) const KEY: &str = "N0CALL-SENTINEL-KEY-7Q2";

    // ---- the fake RTMP server, on 127.0.0.1 only ----

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Mode {
        Accept,
        RefuseEchoingKey,
        CloseAfter(Duration),
        Blackhole,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Got {
        Video { key: bool, seq: bool, ts: u32 },
        Audio { seq: bool, ts: u32 },
    }

    #[derive(Default, Debug)]
    pub(crate) struct Conn {
        pub(crate) key: Option<String>,
        pub(crate) got: Vec<Got>,
    }

    pub(crate) struct Fake {
        pub(crate) addr: SocketAddr,
        conns: Arc<Mutex<Vec<Conn>>>,
        stop: Arc<AtomicBool>,
        tls: bool,
    }

    /// A self-signed certificate for 127.0.0.1: the server's TLS setup, and
    /// a client setup that trusts that certificate and nothing else.
    pub(crate) fn loopback_tls() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let kp = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .self_signed(&kp)
            .unwrap();
        let der = CertificateDer::from(cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(kp.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(server), Arc::new(client))
    }

    impl Fake {
        pub(crate) fn start(mode: Mode) -> Self {
            Self::start_with(mode, None)
        }

        /// An RTMPS fake, behind `tls`.
        fn start_tls(mode: Mode, tls: Arc<rustls::ServerConfig>) -> Self {
            Self::start_with(mode, Some(tls))
        }

        fn start_with(mode: Mode, tls: Option<Arc<rustls::ServerConfig>>) -> Self {
            let is_tls = tls.is_some();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let conns = Arc::new(Mutex::new(Vec::<Conn>::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (c, s) = (Arc::clone(&conns), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            sock.set_nonblocking(false).unwrap();
                            let tls = tls.clone();
                            let idx = {
                                let mut g = c.lock().unwrap();
                                g.push(Conn::default());
                                g.len() - 1
                            };
                            let (c, s) = (Arc::clone(&c), Arc::clone(&s));
                            std::thread::spawn(move || {
                                sock.set_read_timeout(Some(Duration::from_millis(50)))
                                    .unwrap();
                                match tls {
                                    Some(cfg) => {
                                        let conn = rustls::ServerConnection::new(cfg).unwrap();
                                        let tls = rustls::StreamOwned::new(conn, sock);
                                        serve(tls, idx, &c, &s, mode);
                                    }
                                    None => serve(sock, idx, &c, &s, mode),
                                }
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
            Self {
                addr,
                conns,
                stop,
                tls: is_tls,
            }
        }

        pub(crate) fn url(&self) -> IngestUrl {
            let scheme = if self.tls { "rtmps" } else { "rtmp" };
            IngestUrl::parse(&format!("{scheme}://127.0.0.1:{}/app", self.addr.port())).unwrap()
        }

        pub(crate) fn conns(&self) -> std::sync::MutexGuard<'_, Vec<Conn>> {
            self.conns.lock().unwrap()
        }

        pub(crate) fn kill(&self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.kill();
        }
    }

    #[allow(clippy::too_many_lines)] // one fake server, read top to bottom
    fn serve<S: Read + Write>(
        mut s: S,
        idx: usize,
        conns: &Mutex<Vec<Conn>>,
        stop: &AtomicBool,
        mode: Mode,
    ) {
        let started = Instant::now();
        if mode == Mode::Blackhole {
            // Accept, then never read a byte: the socket stays open.
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            return;
        }
        let mut buf = vec![0u8; 65536];
        let mut read = |s: &mut S| -> Option<Vec<u8>> {
            loop {
                if stop.load(Ordering::SeqCst) {
                    return None;
                }
                if let Mode::CloseAfter(d) = mode
                    && started.elapsed() >= d
                {
                    return None;
                }
                match s.read(&mut buf) {
                    Ok(0) => return None,
                    Ok(n) => return Some(buf[..n].to_vec()),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return None,
                }
            }
        };
        let mut hs = Handshake::new(PeerType::Server);
        let mut input = loop {
            let Some(bytes) = read(&mut s) else { return };
            match hs.process_bytes(&bytes) {
                Ok(HandshakeProcessResult::InProgress { response_bytes }) => {
                    if s.write_all(&response_bytes).is_err() {
                        return;
                    }
                }
                Ok(HandshakeProcessResult::Completed {
                    response_bytes,
                    remaining_bytes,
                }) => {
                    if s.write_all(&response_bytes).is_err() {
                        return;
                    }
                    break remaining_bytes;
                }
                Err(_) => return,
            }
        };
        let (mut sess, init) = ServerSession::new(ServerSessionConfig::new()).unwrap();
        let mut queue: VecDeque<ServerSessionResult> = init.into();
        loop {
            if !input.is_empty() {
                match sess.handle_input(&input) {
                    Ok(r) => queue.extend(r),
                    Err(_) => return,
                }
                input.clear();
            }
            while let Some(r) = queue.pop_front() {
                match r {
                    ServerSessionResult::OutboundResponse(p) => {
                        if s.write_all(&p.bytes).is_err() {
                            return;
                        }
                    }
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. } => {
                            queue.extend(sess.accept_request(request_id).unwrap());
                        }
                        ServerSessionEvent::PublishStreamRequested {
                            request_id,
                            stream_key,
                            ..
                        } => {
                            conns.lock().unwrap()[idx].key = Some(stream_key.clone());
                            if mode == Mode::RefuseEchoingKey {
                                let why = format!("Stream key {stream_key} is not valid");
                                queue.extend(
                                    sess.reject_request(
                                        request_id,
                                        "NetStream.Publish.BadName",
                                        &why,
                                    )
                                    .unwrap(),
                                );
                            } else {
                                queue.extend(sess.accept_request(request_id).unwrap());
                            }
                        }
                        ServerSessionEvent::VideoDataReceived {
                            data, timestamp, ..
                        } => {
                            let seq = data.get(1) == Some(&0);
                            let key = data[0] >> 4 == 1 && !seq;
                            conns.lock().unwrap()[idx].got.push(Got::Video {
                                key,
                                seq,
                                ts: timestamp.value,
                            });
                        }
                        ServerSessionEvent::AudioDataReceived {
                            data, timestamp, ..
                        } => {
                            conns.lock().unwrap()[idx].got.push(Got::Audio {
                                seq: data.get(1) == Some(&0),
                                ts: timestamp.value,
                            });
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
            let Some(bytes) = read(&mut s) else { return };
            input = bytes;
        }
    }

    // ---- a synthetic encoder ----

    fn video(ts: u32, key: bool) -> Arc<FlvTag> {
        let first = if key { 0x17 } else { 0x27 };
        Arc::new(FlvTag::new(
            TagKind::Video,
            ts,
            Bytes::from(vec![first, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88]),
        ))
    }

    fn audio(ts: u32) -> Arc<FlvTag> {
        Arc::new(FlvTag::new(
            TagKind::Audio,
            ts,
            Bytes::from_static(&[0xaf, 1, 0x21, 0x10]),
        ))
    }

    pub(crate) fn headers() -> Arc<Mutex<SeqHeaders>> {
        let mut h = SeqHeaders::default();
        h.observe(&Arc::new(FlvTag::new(
            TagKind::Video,
            0,
            Bytes::from_static(&[0x17, 0, 0, 0, 0, 1, 0x64, 0, 0x1f]),
        )));
        h.observe(&Arc::new(FlvTag::new(
            TagKind::Audio,
            0,
            Bytes::from_static(&[0xaf, 0, 0x12, 0x10]),
        )));
        assert!(h.video.is_some() && h.audio.is_some());
        Arc::new(Mutex::new(h))
    }

    /// Offer frames at about 30 a second, a key frame every 10, until `done`.
    fn feed_until(pubs: &[&Publisher], limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        let mut n = 0u32;
        while start.elapsed() < limit {
            if done() {
                return true;
            }
            for p in pubs {
                p.offer(video(n * 33, n.is_multiple_of(10)));
                p.offer(audio(n * 33));
            }
            n += 1;
            std::thread::sleep(Duration::from_millis(33));
        }
        done()
    }

    pub(crate) fn dest(url: IngestUrl) -> Destination {
        Destination {
            name: DestName::YouTube,
            ingest: url,
            key: Secret::new(KEY.to_string()),
        }
    }

    pub(crate) fn quick(io: Duration) -> Tuning {
        // Every test that starts a publisher logs into the one capture,
        // installed before its first line so no call site caches "nobody".
        let _ = captured_log();
        Tuning {
            io_timeout: io,
            backoff_min: Duration::from_millis(200),
            backoff_max: Duration::from_secs(2),
            reset_after: Duration::from_secs(60),
            tls: None,
        }
    }

    // ---- the tests ----

    #[test]
    fn ingest_url_rules() {
        let u = IngestUrl::parse("rtmps://a.rtmps.youtube.com/live2").unwrap();
        assert_eq!(
            u,
            IngestUrl {
                tls: true,
                host: "a.rtmps.youtube.com".into(),
                port: 443,
                app: "live2".into()
            }
        );
        let u = IngestUrl::parse("rtmp://127.0.0.1/app").unwrap();
        assert_eq!((u.tls, u.port, u.app.as_str()), (false, 1935, "app"));
        let u = IngestUrl::parse("rtmp://[::1]:19350/x/").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.app.as_str()),
            ("::1", 19350, "x")
        );
        let u = IngestUrl::parse("RTMPS://Host.Example:8443/app").unwrap();
        assert_eq!((u.tls, u.port), (true, 8443));
        assert!(IngestUrl::parse("rtmp://LOCALHOST/app").is_ok());

        for bad in [
            "rtmp://example.com/app",
            "rtmps://host/app/KEY",
            "rtmps://host",
            "rtmps://host/",
            "rtmps://host/app?k=KEY",
            "rtmps://host/app#KEY",
            "rtmps://user@host/app",
            "rtmp://localhost:x@evil.example/app",
            "rtmp://127.0.0.1.evil.com/app",
            "rtmps://host:0/app",
            "rtmps://host:99999/app",
            "http://host/app",
            "host/app",
            "rtmps://host/li%76e2",
            "rtmps://ho%73t/app",
            "rtmps://host/a\\b",
            "rtmps://ho\\st/app",
            "rtmps://host/a b",
            "rtmps://host/app\t",
            "rtmps://ho st/app",
        ] {
            assert!(IngestUrl::parse(bad).is_err(), "{bad} must be refused");
        }

        // An error names the scheme and host at most, never the path.
        let e = IngestUrl::parse("rtmps://a.rtmps.youtube.com/live2/SENTINELKEY").unwrap_err();
        assert!(!e.contains("SENTINEL"), "{e}");
        assert!(e.contains("a.rtmps.youtube.com"), "{e}");
        for k in [
            "rtmp://evil.example/live2/SENTINELKEY",
            "rtmps://u:SENTINELKEY@host/app",
            "rtmps://host/app?SENTINELKEY",
            "SENTINELKEY",
            "live2/SENTINELKEY://x",
        ] {
            let e = IngestUrl::parse(k).unwrap_err();
            assert!(!e.contains("SENTINEL"), "{e}");
        }
    }

    #[test]
    fn backoff_doubles_to_a_cap() {
        let t = Tuning::default();
        assert_eq!(t.io_timeout, Duration::from_secs(10));
        let mut b = t.backoff_min;
        let mut seen = vec![b.as_secs()];
        for _ in 0..7 {
            b = next_backoff(b, &t);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(t.reset_after, Duration::from_secs(60));
    }

    #[test]
    fn timestamps_never_go_backwards() {
        let mut r = Rebase::default();
        assert_eq!(r.map(0), 0);
        assert_eq!(r.map(1000), 1000);
        // Audio a few milliseconds behind video: held, not rebased.
        assert_eq!(r.map(990), 1000);
        assert_eq!(r.map(1033), 1033);
        // The encoder restarted from zero: carry on just after.
        let after = r.map(0);
        assert!(after > 1033, "{after}");
        assert_eq!(r.map(500), after + 500);
    }

    #[test]
    fn publishes_to_the_fake_server() {
        let fake = Fake::start(Mode::Accept);
        let p = Publisher::spawn_with(dest(fake.url()), headers(), quick(Duration::from_secs(2)));
        let ok = feed_until(&[&p], Duration::from_secs(10), || {
            fake.conns().first().is_some_and(|c| {
                c.got
                    .iter()
                    .any(|g| matches!(g, Got::Video { key: true, .. }))
            })
        });
        assert!(ok, "no key frame reached the fake");
        assert_eq!(fake.conns()[0].key.as_deref(), Some(KEY));
        let s = p.status();
        assert_eq!((s.name.as_str(), s.state), ("youtube", DestState::Live));
        assert_eq!(s.last_error, None);
        p.stop();
    }

    #[test]
    fn publishes_over_tls_to_a_loopback_fake() {
        let (server, client) = loopback_tls();
        let fake = Fake::start_tls(Mode::Accept, server);
        let url = fake.url();
        assert!(url.tls);
        let mut tuning = quick(Duration::from_secs(2));
        tuning.tls = Some(client);
        let p = Publisher::spawn_with(dest(url), headers(), tuning);
        let ok = feed_until(&[&p], Duration::from_secs(10), || {
            fake.conns().first().is_some_and(|c| {
                c.got
                    .iter()
                    .any(|g| matches!(g, Got::Video { key: true, .. }))
            })
        });
        assert!(ok, "no key frame reached the TLS fake: {:?}", p.status());
        assert_eq!(fake.conns()[0].key.as_deref(), Some(KEY));
        assert_eq!(p.status().state, DestState::Live);
        p.stop();
    }

    #[test]
    fn tls_refuses_a_certificate_it_does_not_trust() {
        let (server, _) = loopback_tls();
        let (_, other_client) = loopback_tls();
        let fake = Fake::start_tls(Mode::Accept, server);
        let mut tuning = quick(Duration::from_secs(2));
        tuning.tls = Some(other_client);
        let p = Publisher::spawn_with(dest(fake.url()), headers(), tuning);
        let failed = feed_until(&[&p], Duration::from_secs(10), || {
            p.status().state == DestState::Retrying
        });
        let s = p.status();
        p.stop();
        assert!(failed, "{s:?}");
        assert!(
            fake.conns().iter().all(|c| c.key.is_none()),
            "the key went out"
        );
        let err = s.last_error.unwrap();
        assert!(
            err.contains("TLS failed") || err.contains("lost the connection"),
            "{err}"
        );
    }

    #[test]
    fn refused_publish_never_shows_the_key() {
        let fake = Fake::start(Mode::RefuseEchoingKey);
        let log = captured_log();
        let d = dest(fake.url());
        assert!(!format!("{d:?}").contains(KEY));
        let p = Publisher::spawn_with(d, headers(), quick(Duration::from_secs(2)));
        feed_until(&[&p], Duration::from_secs(10), || {
            p.status().last_error.is_some()
        });
        let status = p.status();
        p.stop();
        // The fake really did hear the key, and quoted it back.
        assert_eq!(fake.conns()[0].key.as_deref(), Some(KEY));
        let err = status.last_error.clone().expect("an error");
        assert!(err.contains("refused"), "{err}");
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(!err.contains(KEY), "{err}");
        assert!(!format!("{status:?}").contains(KEY));
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"state\":\"retrying\""), "{json}");
        assert!(!json.contains(KEY), "{json}");
        // Every test's log lines land here, and none may hold the key.
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let port = format!("127.0.0.1:{}", fake.addr.port());
        assert!(
            text.lines()
                .any(|l| l.contains("refused") && l.contains(&port)),
            "the refusal is logged: {text}"
        );
        assert!(!text.contains(KEY), "{text}");
    }

    /// The log of every test in this binary. A process-wide subscriber, not
    /// a scoped one: tracing caches each call site's interest globally, and a
    /// scoped subscriber races the other tests' threads for that cache.
    pub(crate) fn captured_log() -> Arc<Mutex<Vec<u8>>> {
        static LOG: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
        Arc::clone(LOG.get_or_init(|| {
            let log = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&log);
            let sub = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(move || Sink(Arc::clone(&sink)))
                .finish();
            tracing::subscriber::set_global_default(sub).expect("one global subscriber");
            log
        }))
    }

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn dropped_connection_reconnects_with_backoff() {
        let fake = Fake::start(Mode::CloseAfter(Duration::from_secs(1)));
        let p = Publisher::spawn_with(dest(fake.url()), headers(), quick(Duration::from_secs(2)));
        let ok = feed_until(&[&p], Duration::from_secs(15), || {
            fake.conns().get(1).is_some_and(|c| c.got.len() >= 3)
        });
        assert!(ok, "no second session");
        let s = p.status();
        p.stop();
        assert!(s.reconnects >= 1, "{s:?}");
        let conns = fake.conns();
        let second = &conns[1].got;
        assert!(
            second[..2]
                .iter()
                .any(|g| matches!(g, Got::Video { seq: true, .. })),
            "{second:?}"
        );
        assert!(
            second[..2]
                .iter()
                .any(|g| matches!(g, Got::Audio { seq: true, .. })),
            "{second:?}"
        );
        assert!(
            matches!(second[2], Got::Video { key: true, .. }),
            "{second:?}"
        );
        let last_first = conns[0].got.iter().map(|g| ts_of(*g)).max().unwrap();
        let first_second = second.iter().map(|g| ts_of(*g)).min().unwrap();
        assert!(first_second >= last_first, "{first_second} < {last_first}");
    }

    pub(crate) fn ts_of(g: Got) -> u32 {
        match g {
            Got::Video { ts, .. } | Got::Audio { ts, .. } => ts,
        }
    }

    #[test]
    fn blackholed_destination_times_out_and_retries() {
        let fake = Fake::start(Mode::Blackhole);
        let p = Publisher::spawn_with(
            dest(fake.url()),
            headers(),
            quick(Duration::from_millis(500)),
        );
        let start = Instant::now();
        let ok = feed_until(&[&p], Duration::from_secs(2), || {
            p.status().state == DestState::Retrying
        });
        assert!(ok, "still {:?} after {:?}", p.status(), start.elapsed());
        let err = p.status().last_error.unwrap();
        assert!(err.contains("timed out"), "{err}");
        p.stop();
    }

    #[test]
    fn one_destination_failing_leaves_the_other_live() {
        let good = Fake::start(Mode::Accept);
        let bad = Fake::start(Mode::Accept);
        let a = Publisher::spawn_with(dest(good.url()), headers(), quick(Duration::from_secs(2)));
        let mut d = dest(bad.url());
        d.name = DestName::Twitch;
        let b = Publisher::spawn_with(d, headers(), quick(Duration::from_secs(2)));
        let both = feed_until(&[&a, &b], Duration::from_secs(10), || {
            a.status().state == DestState::Live && b.status().state == DestState::Live
        });
        assert!(both, "{:?} {:?}", a.status(), b.status());
        bad.kill();
        let down = feed_until(&[&a, &b], Duration::from_secs(10), || {
            b.status().state == DestState::Retrying
        });
        assert!(down, "{:?}", b.status());
        assert_eq!(b.status().name, "twitch");
        let before = good.conns()[0].got.len();
        feed_until(&[&a, &b], Duration::from_millis(700), || false);
        assert_eq!(a.status().state, DestState::Live);
        assert_eq!(good.conns().len(), 1, "the good one never reconnected");
        assert!(
            good.conns()[0].got.len() > before,
            "the good one kept receiving"
        );
        a.stop();
        b.stop();
    }

    #[test]
    fn queue_full_drops_to_the_next_keyframe() {
        let (tx, rx) = sync_channel(QUEUE_TAGS);
        let feed = Feed::new(tx);
        feed.offer(video(0, true));
        for n in 1..300 {
            feed.offer(video(n, false));
        }
        assert_eq!(rx.try_iter().count(), QUEUE_TAGS);
        // Room again, but the gap must close on a key frame.
        feed.offer(video(400, false));
        feed.offer(audio(400));
        let seq = Arc::new(FlvTag::new(
            TagKind::Video,
            401,
            Bytes::from_static(&[0x17, 0, 0, 0, 0, 1]),
        ));
        feed.offer(Arc::clone(&seq));
        feed.offer(video(402, true));
        feed.offer(audio(402));
        feed.offer(video(403, false));
        let got: Vec<(u32, bool, bool)> = rx
            .try_iter()
            .map(|t| (t.ts_ms, t.keyframe, t.seq_header))
            .collect();
        assert_eq!(
            got,
            [
                (401, false, true),
                (402, true, false),
                (402, false, false),
                (403, false, false)
            ]
        );
    }

    #[test]
    #[should_panic(expected = "loopback")]
    fn remote_host_panics_in_tests() {
        let url = IngestUrl::parse("rtmps://example.com/app").unwrap();
        let _ = Publisher::spawn(dest(url), headers());
    }
}
