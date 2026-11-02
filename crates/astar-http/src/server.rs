// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The server: one accept thread, a thread per connection under the pool's
//! caps, one request per connection.

use crate::deadline::{DeadlineSocket, ReadDeadline};
use crate::limits::{Limits, Pool, StreamClass, StreamGuard};
use crate::parse::read_head;
use crate::request::Request;
use crate::response::{Conn, Defaults, ReadWrite, Reply, Response, reason};
use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

type Handler = dyn for<'a> Fn(&mut Request<'a>) -> Reply + Send + Sync;

/// What a server is besides its [`Limits`].
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Names the server's threads and its log lines.
    pub name: &'static str,
    /// The `Content-Security-Policy` every answer carries unless it sets
    /// its own.
    pub csp: Option<&'static str>,
    /// The stream counters, when several servers share them; `None` gives
    /// the server its own.
    pub streams: Option<Arc<StreamGuard>>,
    /// HTTPS on the same port: a first byte of `0x16` (a TLS handshake
    /// record) is served over TLS once a certificate is installed, and
    /// closed before then; anything else is plain HTTP, which
    /// [`Request::tls`] reports so the application decides what it gets.
    #[cfg(feature = "tls")]
    pub tls: Option<Arc<crate::tls::TlsConfig>>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            name: "http",
            csp: None,
            streams: None,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

struct Shared {
    name: &'static str,
    limits: Limits,
    defaults: Arc<Defaults>,
    handler: Box<Handler>,
    streams: Arc<StreamGuard>,
    #[cfg(feature = "tls")]
    tls: Option<Arc<crate::tls::TlsConfig>>,
}

/// A listening server. Dropping it, or [`Server::stop`], stops accepting.
pub struct Server {
    local: SocketAddr,
    pool: Arc<Pool>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Listen on `addr` with the default [`ServerOptions`].
    ///
    /// # Errors
    /// The bind failed.
    pub fn bind<H>(addr: &str, limits: Limits, handler: H) -> io::Result<Self>
    where
        H: for<'a> Fn(&mut Request<'a>) -> Reply + Send + Sync + 'static,
    {
        Self::bind_with(addr, limits, ServerOptions::default(), handler)
    }

    /// Listen on `addr`.
    ///
    /// # Errors
    /// The bind failed.
    pub fn bind_with<H>(
        addr: &str,
        limits: Limits,
        options: ServerOptions,
        handler: H,
    ) -> io::Result<Self>
    where
        H: for<'a> Fn(&mut Request<'a>) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let pool = Pool::new(limits.max_connections, limits.conns_per_client);
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            name: options.name,
            limits,
            defaults: Arc::new(Defaults { csp: options.csp }),
            handler: Box::new(handler),
            streams: options.streams.unwrap_or_else(|| {
                StreamGuard::new(limits.streams_per_client, limits.max_counted_streams)
            }),
            #[cfg(feature = "tls")]
            tls: options.tls,
        });
        let accept = {
            let (pool, stop) = (Arc::clone(&pool), Arc::clone(&stop));
            std::thread::Builder::new()
                .name(format!("{}-accept", options.name))
                .spawn(move || accept_loop(&listener, &pool, &stop, &shared))?
        };
        Ok(Self {
            local,
            pool,
            stop,
            accept: Some(accept),
        })
    }

    /// The address bound (the port an ephemeral bind was given).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Connections open now.
    #[must_use]
    pub fn open_connections(&self) -> usize {
        self.pool.open()
    }

    /// Stop accepting and wait for the accept thread. Open connections end
    /// on their own: their deadlines and write stalls bound them.
    pub fn stop(self) {}
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, pool: &Arc<Pool>, stop: &AtomicBool, shared: &Arc<Shared>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((tcp, peer)) => {
                let Some(slot) = pool.admit(peer.ip()) else {
                    continue; // dropped: closed at once
                };
                // The request phase's clock starts at accept.
                let deadline = ReadDeadline::after(shared.limits.deadline);
                let _ = tcp.set_nonblocking(false);
                let _ = tcp.set_nodelay(true);
                let conn_shared = Arc::clone(shared);
                let spawned = std::thread::Builder::new()
                    .name(format!("{}-conn", shared.name))
                    .spawn(move || {
                        let _slot = slot;
                        serve_one(tcp, peer, &deadline, &conn_shared);
                    });
                if spawned.is_err() {
                    tracing::warn!("{}: cannot spawn a connection thread", shared.name);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                tracing::warn!("{}: accept failed: {e}", shared.name);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// The socket a connection reads and writes through, and whether it is TLS;
/// `None` closes the connection. With TLS configured, the first byte
/// decides: `0x16` (a handshake record) is TLS once a certificate is
/// installed and closed before then; anything else is plain HTTP.
#[cfg_attr(
    not(feature = "tls"),
    expect(clippy::unnecessary_wraps, reason = "only TLS closes at the sniff")
)]
fn open(
    tcp: TcpStream,
    deadline: &ReadDeadline,
    shared: &Shared,
) -> Option<(Box<dyn ReadWrite>, bool)> {
    #[cfg(feature = "tls")]
    if let Some(cfg) = &shared.tls {
        // The peek waits under the request deadline like every read.
        tcp.set_read_timeout(deadline.remaining().ok()?).ok()?;
        let mut first = [0u8; 1];
        match tcp.peek(&mut first) {
            Ok(1) if first[0] == 0x16 => {
                // A ClientHello and no certificate: nothing to say.
                if !cfg.ready() {
                    return None;
                }
                // Under the deadline: a handshake that trickles is cut.
                let io = DeadlineSocket::new(tcp, deadline.clone());
                let tls = rustls::StreamOwned::new(cfg.connection()?, io);
                return Some((Box::new(tls), true));
            }
            Ok(1) => {}
            // Closed, or silent until the deadline.
            _ => return None,
        }
    }
    #[cfg(not(feature = "tls"))]
    let _ = shared;
    // Every read from here on, the head's and the body's, shares the one
    // deadline.
    Some((Box::new(DeadlineSocket::new(tcp, deadline.clone())), false))
}

fn serve_one(tcp: TcpStream, peer: SocketAddr, deadline: &ReadDeadline, shared: &Shared) {
    let Ok(sock) = tcp.try_clone() else { return };
    let _ = sock.set_write_timeout(Some(shared.limits.write_stall));
    let Some((io, tls)) = open(tcp, deadline, shared) else {
        return;
    };
    let mut reader = BufReader::new(io);
    let head = match read_head(&mut reader, shared.limits.max_head_bytes) {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            if let Some(code) = e.status() {
                deadline.clear();
                let resp = Response::text(code, format!("{}\n", reason(code)));
                let _ = finish(&mut reader, &sock, &resp, shared, tls, false);
            }
            return;
        }
    };
    let head_only = head.method == "HEAD";
    // RFC 9112 requires Host on HTTP/1.1; it is refused without one on
    // HTTP/1.0 too, since every answer depends on it.
    if head.header("Host").is_none_or(str::is_empty) {
        deadline.clear();
        let resp = Response::text(400, "Host required\n");
        let _ = finish(&mut reader, &sock, &resp, shared, tls, head_only);
        return;
    }
    let reply = {
        // The body is read through the same buffered reader the head came
        // from: part of it may already sit in its buffer.
        let mut req = Request::new(&head, peer, tls, &mut reader, shared.limits.max_body_bytes);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (shared.handler)(&mut req)))
            .unwrap_or_else(|_| {
                tracing::warn!("{}: a handler panicked", shared.name);
                Reply::Response(Response::text(500, "Internal error\n"))
            })
    };
    match reply {
        Reply::Response(resp) => {
            deadline.clear();
            let _ = finish(&mut reader, &sock, &resp, shared, tls, head_only);
        }
        Reply::Stream { class, run } => {
            let link = Link {
                sock,
                peer,
                tls,
                head_only,
            };
            stream(reader, link, deadline, class, run, shared);
        }
        #[cfg(feature = "ws")]
        Reply::WebSocket { class, config, run } => {
            let Some(accept) = crate::ws::accept_key_of(&head) else {
                deadline.clear();
                let resp = Response::text(400, "WebSocket upgrade expected\n");
                let _ = finish(&mut reader, &sock, &resp, shared, tls, head_only);
                return;
            };
            let link = Link {
                sock,
                peer,
                tls,
                head_only,
            };
            let run: Box<dyn FnOnce(Conn) + Send> =
                Box::new(move |conn| crate::ws::accept(conn, &accept, config, run));
            stream(reader, link, deadline, class, run, shared);
        }
    }
}

/// The connection a stream takes over, besides its reader.
struct Link {
    sock: TcpStream,
    peer: SocketAddr,
    tls: bool,
    head_only: bool,
}

/// Run a [`Reply::Stream`] inside its stream slot, or refuse it when the
/// client or the server is at its cap. The slot lives in the [`Conn`], so
/// it is freed however the stream ends, a panic included.
fn stream(
    mut reader: BufReader<Box<dyn ReadWrite>>,
    link: Link,
    deadline: &ReadDeadline,
    class: StreamClass,
    run: Box<dyn FnOnce(Conn) + Send>,
    shared: &Shared,
) {
    let slot = match shared.streams.open(link.peer.ip(), class) {
        Ok(s) => s,
        Err(r) => {
            deadline.clear();
            let resp = Response::text(r.code(), format!("{}\n", r.text()))
                .header("Retry-After", "10")
                .header("Cache-Control", "no-store");
            let _ = finish(
                &mut reader,
                &link.sock,
                &resp,
                shared,
                link.tls,
                link.head_only,
            );
            return;
        }
    };
    // The request deadline is over. A stream's writes are bounded by the
    // write stall, which is what finds a dead client; any read it makes
    // waits no longer than that either.
    deadline.per_read(shared.limits.write_stall);
    let _ = link.sock.set_write_timeout(Some(shared.limits.write_stall));
    let conn = Conn::new(
        reader,
        link.sock,
        link.peer,
        link.tls,
        link.head_only,
        Arc::clone(&shared.defaults),
        slot,
    );
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || run(conn))).is_err() {
        // The slot went with the unwound Conn.
        tracing::warn!("{}: a stream panicked", shared.name);
    }
}

/// Write the whole answer in one go, then close gently
/// ([`crate::response::close_gently`]).
fn finish(
    reader: &mut BufReader<Box<dyn ReadWrite>>,
    sock: &TcpStream,
    resp: &Response,
    shared: &Shared,
    tls: bool,
    head_only: bool,
) -> io::Result<()> {
    crate::response::close_gently(
        reader.get_mut(),
        sock,
        &resp.encode(&shared.defaults, tls, head_only),
    )
}
