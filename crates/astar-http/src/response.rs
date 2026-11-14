// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What a handler answers: a [`Response`], or the connection itself for a
//! long-lived stream ([`Reply::Stream`]).

use crate::limits::{StreamClass, StreamSlot};
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The reason phrase sent with `code`.
#[must_use]
pub const fn reason(code: u16) -> &'static str {
    match code {
        101 => "Switching Protocols",
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        303 => "See Other",
        304 => "Not Modified",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        421 => "Misdirected Request",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// The defaults every answer carries unless it sets them itself.
#[derive(Debug, Clone)]
pub(crate) struct Defaults {
    pub csp: Option<&'static str>,
}

/// A header that can go on the wire as it is: its name an RFC 9110 token,
/// its value free of CR, LF and NUL. Anything else would split the
/// response or forge its framing, so it is dropped.
fn sendable(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(crate::parse::is_tchar)
        && !value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

/// A sendable header named `name` is set: a dropped one does not displace
/// a default.
fn has(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(name) && sendable(k, v))
}

/// How a body is delimited on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    /// By the close: no `Content-Length` (event streams, live audio).
    Close,
    /// `Content-Length: n`.
    Length(u64),
    /// `Transfer-Encoding: chunked` ([`Chunked`]).
    Chunked,
}

/// The status line and every header, `\r\n\r\n` included. The framing
/// headers (`Connection`, `Content-Length`, `Transfer-Encoding`) are the
/// server's alone: a response's own are dropped.
pub(crate) fn encode_head(
    status: u16,
    headers: &[(String, String)],
    defaults: &Defaults,
    tls: bool,
    framing: Framing,
) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut h = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    let mut add = |k: &str, v: &str| {
        let _ = write!(h, "{k}: {v}\r\n");
    };
    if let Some(csp) = defaults.csp
        && !has(headers, "Content-Security-Policy")
    {
        add("Content-Security-Policy", csp);
    }
    for (k, v) in [
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
    ] {
        if !has(headers, k) {
            add(k, v);
        }
    }
    if tls && !has(headers, "Strict-Transport-Security") {
        add("Strict-Transport-Security", "max-age=31536000");
    }
    for (k, v) in headers {
        if ["Connection", "Content-Length", "Transfer-Encoding"]
            .iter()
            .any(|f| k.eq_ignore_ascii_case(f))
        {
            continue;
        }
        if !sendable(k, v) {
            // The name only: the value may be what the attacker supplied.
            tracing::warn!(
                "dropped a response header that would split the response: {}",
                k.escape_debug()
            );
            continue;
        }
        add(k, v);
    }
    add("Connection", "close");
    match framing {
        Framing::Close => {}
        Framing::Length(n) => add("Content-Length", &n.to_string()),
        Framing::Chunked => add("Transfer-Encoding", "chunked"),
    }
    h.push_str("\r\n");
    h.into_bytes()
}

/// A whole answer: status, headers and body, sent in one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// `status` with no headers and an empty body.
    #[must_use]
    pub const fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A `text/plain; charset=utf-8` answer.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status).with_body("text/plain; charset=utf-8", body.into())
    }

    /// An `application/json` answer.
    #[must_use]
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status).with_body("application/json", body)
    }

    /// Replace the body and its `Content-Type`.
    #[must_use]
    pub fn with_body(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        self.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Type"));
        self.headers
            .push(("Content-Type".into(), content_type.into()));
        self.body = body.into();
        self
    }

    /// Add a header (a default of the same name is then not sent).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Head and body as one buffer: one write, so no reader sees a head
    /// without its body.
    pub(crate) fn encode(&self, defaults: &Defaults, tls: bool, head_only: bool) -> Vec<u8> {
        let mut out = encode_head(
            self.status,
            &self.headers,
            defaults,
            tls,
            Framing::Length(self.body.len() as u64),
        );
        if !head_only {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// What a handler answers with.
pub enum Reply {
    /// A whole answer.
    Response(Response),
    /// Take the connection over for a long-lived stream (SSE, audio). `run`
    /// writes its own head ([`Conn::write_head`]); the stream ends when it
    /// returns.
    Stream {
        class: StreamClass,
        run: Box<dyn FnOnce(Conn) + Send>,
    },
    /// Upgrade to a WebSocket inside a stream slot of `class`: the server
    /// answers `101` and hands `run` the socket, or answers
    /// `400 WebSocket upgrade expected` when the request is not an upgrade
    /// ([`crate::ws::is_upgrade`]). `config` `None` is
    /// [`crate::ws::default_config`]: 64 KiB per message and per frame.
    #[cfg(feature = "ws")]
    WebSocket {
        class: StreamClass,
        config: Option<crate::ws::WebSocketConfig>,
        run: Box<dyn FnOnce(crate::ws::WebSocket) + Send>,
    },
}

impl From<Response> for Reply {
    fn from(r: Response) -> Self {
        Self::Response(r)
    }
}

/// How long the lingering close reads what a client is still sending.
const LINGER_FOR: Duration = Duration::from_secs(1);
/// The most the lingering close reads.
const LINGER_BYTES: usize = 256 * 1024;

/// Write `bytes` (a whole answer) in one go, then close gently: TLS's
/// `close_notify`, a half-close, and a read of what the client is still
/// sending (at most [`LINGER_BYTES`], for at most [`LINGER_FOR`]) so the
/// kernel does not reset the connection under the answer. Under TLS the
/// drained bytes are ciphertext: discarded all the same.
pub(crate) fn close_gently(
    w: &mut Box<dyn ReadWrite>,
    sock: &TcpStream,
    bytes: &[u8],
) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()?;
    w.close_notify();
    let _ = sock.shutdown(std::net::Shutdown::Write);
    let Ok(mut raw) = sock.try_clone() else {
        return Ok(());
    };
    let until = Instant::now() + LINGER_FOR;
    let mut left = LINGER_BYTES;
    let mut buf = [0u8; 4096];
    while left > 0 {
        let now = Instant::now();
        if now >= until {
            break;
        }
        let _ = raw.set_read_timeout(Some(until - now));
        match raw.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
    Ok(())
}

/// The socket under a connection, plain or TLS.
pub(crate) trait ReadWrite: Read + Write + Send {
    /// The answer is complete: TLS says so (`close_notify`) before the
    /// close, so a client can tell it from a cut connection. Nothing to say
    /// on plain TCP.
    fn close_notify(&mut self) {}
}
impl ReadWrite for crate::deadline::DeadlineSocket {}
#[cfg(feature = "tls")]
impl ReadWrite for rustls::StreamOwned<rustls::ServerConnection, crate::deadline::DeadlineSocket> {
    fn close_notify(&mut self) {
        self.conn.send_close_notify();
        let _ = self.flush();
    }
}

/// A connection handed to a [`Reply::Stream`]: reads come through the
/// buffer the request was read from, writes go straight out. It holds the
/// stream's slot, freed when it is dropped.
///
/// **Reads are bounded.** A read waits at most [`crate::Limits::write_stall`]
/// and then fails with `WouldBlock` (Unix) or `TimedOut` (Windows). That
/// means the client has sent nothing yet, not that it left: a stream that
/// reads treats those two as "nothing yet" and carries on. A client that
/// left shows up as `Ok(0)` or another error, and as a failed write.
pub struct Conn {
    // Declared first, so dropped first: the slot is free by the time the
    // socket closes, and a client reconnecting at once finds it.
    _slot: StreamSlot,
    reader: BufReader<Box<dyn ReadWrite>>,
    _sock: TcpStream,
    peer: SocketAddr,
    tls: bool,
    head_only: bool,
    defaults: Arc<Defaults>,
}

impl Conn {
    pub(crate) fn new(
        reader: BufReader<Box<dyn ReadWrite>>,
        sock: TcpStream,
        peer: SocketAddr,
        tls: bool,
        head_only: bool,
        defaults: Arc<Defaults>,
        slot: StreamSlot,
    ) -> Self {
        Self {
            _slot: slot,
            reader,
            _sock: sock,
            peer,
            tls,
            head_only,
            defaults,
        }
    }

    /// Write the status line and headers, the defaults included. No
    /// `Content-Length`: the stream is delimited by the close.
    ///
    /// # Errors
    /// The write failed or stalled past the write timeout.
    pub fn write_head(&mut self, status: u16, headers: &[(&str, &str)]) -> io::Result<()> {
        self.head(status, headers, Framing::Close)
    }

    /// [`Conn::write_head`] with `Content-Length: length`: a body whose size
    /// is known before it is sent (a file). A client that gets fewer bytes
    /// knows the answer was cut, so a stream that cannot finish its body
    /// just returns, and the close says it.
    ///
    /// # Errors
    /// The write failed or stalled past the write timeout.
    pub fn write_head_sized(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        length: u64,
    ) -> io::Result<()> {
        self.head(status, headers, Framing::Length(length))
    }

    /// [`Conn::write_head`] with `Transfer-Encoding: chunked`: write the
    /// body through [`Chunked`], whose [`Chunked::finish`] sends the last
    /// chunk. A stream dropped before then never sends it, so a client can
    /// tell a cut answer from a whole one (an archive built as it is sent).
    ///
    /// # Errors
    /// The write failed or stalled past the write timeout.
    pub fn write_head_chunked(&mut self, status: u16, headers: &[(&str, &str)]) -> io::Result<()> {
        self.head(status, headers, Framing::Chunked)
    }

    fn head(&mut self, status: u16, headers: &[(&str, &str)], framing: Framing) -> io::Result<()> {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let head = encode_head(status, &headers, &self.defaults, self.tls, framing);
        let w = self.reader.get_mut();
        w.write_all(&head)?;
        w.flush()
    }

    /// Answer with a whole [`Response`] instead of a stream (a refusal
    /// decided inside the slot): `Content-Length` and the default headers,
    /// as any answer, then the same gentle close. The stream slot is freed
    /// before the close lingers.
    pub fn respond(self, resp: &Response) {
        let Self {
            _slot: slot,
            mut reader,
            _sock: sock,
            tls,
            head_only,
            defaults,
            ..
        } = self;
        drop(slot);
        let _ = close_gently(
            reader.get_mut(),
            &sock,
            &resp.encode(&defaults, tls, head_only),
        );
    }

    /// The request was `HEAD`: write the head and nothing more.
    #[must_use]
    pub const fn head_only(&self) -> bool {
        self.head_only
    }

    /// The connection is TLS.
    #[must_use]
    pub const fn tls(&self) -> bool {
        self.tls
    }

    /// The client's address.
    #[must_use]
    pub const fn peer(&self) -> SocketAddr {
        self.peer
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.reader.get_mut().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.reader.get_mut().flush()
    }
}

/// A chunked body (`Transfer-Encoding: chunked`, after
/// [`Conn::write_head_chunked`]): each write is one chunk, and
/// [`Chunked::finish`] sends the zero-length last chunk. Dropping it
/// unfinished sends nothing more, which is how a stream that failed midway
/// tells the client its answer is incomplete. Wrap it in a `BufWriter` when
/// the writes are small.
pub struct Chunked<W: Write> {
    inner: W,
}

impl<W: Write> Chunked<W> {
    pub const fn new(inner: W) -> Self {
        Self { inner }
    }

    /// The last chunk, then a flush: the body is complete.
    ///
    /// # Errors
    /// The write failed.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.write_all(b"0\r\n\r\n")?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for Chunked<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // An empty chunk would end the body.
        if buf.is_empty() {
            return Ok(0);
        }
        self.inner
            .write_all(format!("{:x}\r\n", buf.len()).as_bytes())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
