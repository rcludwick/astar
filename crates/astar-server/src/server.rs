// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The astar-server control channel (iax-35b1) on `astar-http`.
//!
//! [`bind_control`] binds and serves on `astar-http`'s own threads, one per
//! connection, one request per connection. Every request first passes the
//! `Host` allow-list — a foreign `Host` gets the router's 403 and nothing
//! else, the event stream included. Then `GET /events` → a close-delimited
//! event stream ([`crate::sse::pump_events`]); the recording and packet log
//! downloads → a stream ([`crate::recording::http::stream_reply`]); all other
//! requests → `handle_request_with_headers` → one whole answer.
//! [`ControlServer::stop`] stops accepting; an open event stream ends at the
//! controller's stop flag.

use crate::{
    access::strip_admin_prefix,
    controller::NodeController,
    http::{
        HttpResponse, MAX_BODY_BYTES, RequestHeaders, forbidden_cross_origin,
        handle_request_with_headers, host_is_this_node, payload_too_large,
    },
};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

// ---- public API -------------------------------------------------------------

/// The control port, listening. Dropping it, or [`ControlServer::stop`],
/// stops accepting.
pub struct ControlServer {
    server: astar_http::Server,
}

impl ControlServer {
    /// The address bound (the port an ephemeral bind was given).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// Stop accepting and wait for the accept thread to finish. Requests
    /// already being answered finish on their own; an event stream ends at
    /// the controller's stop flag, or when its client leaves.
    pub fn stop(self) {
        self.server.stop();
    }
}

/// The control port's limits: [`astar_http::Limits::loopback`]. Every local
/// tool (the app, the TUI, nodebench, a script) is the same client here, so
/// the per-client caps must be the wide ones: the public numbers' four
/// streams per client would refuse the fifth `/events`.
#[must_use]
pub const fn control_limits() -> astar_http::Limits {
    astar_http::Limits::loopback()
}

// astar-http's own body cap and the router's are one number.
const _: () = assert!(control_limits().max_body_bytes == MAX_BODY_BYTES);

/// Bind the control port on `bind` (e.g. `"127.0.0.1:8730"`, or
/// `"127.0.0.1:0"` for an ephemeral port) and serve it until
/// [`ControlServer::stop`].
///
/// # Errors
/// The address cannot be bound.
pub fn bind_control(ctrl: &Arc<NodeController>, bind: &str) -> io::Result<ControlServer> {
    // What the socket is really bound to: the router needs it to tell "this
    // node's own page" from a `Host` a DNS answer was pointed at us. Known
    // only once the bind returns; a request that beats it sees `None`,
    // which means "loopback only" — the safe reading.
    let bound = Arc::new(OnceLock::<SocketAddr>::new());
    let (c, b) = (Arc::clone(ctrl), Arc::clone(&bound));
    let server = astar_http::Server::bind_with(
        bind,
        control_limits(),
        astar_http::ServerOptions {
            name: "control",
            csp: Some(crate::http::CSP),
            ..astar_http::ServerOptions::default()
        },
        move |req| dispatch(req, &c, b.get().copied()),
    )?;
    let _ = bound.set(server.local_addr());
    Ok(ControlServer { server })
}

// ---- dispatch ---------------------------------------------------------------

#[allow(
    clippy::too_many_lines,
    reason = "one request's checks in the order they run: Host, cap, prefix, body, gate, streams, router"
)]
fn dispatch(
    req: &mut astar_http::Request<'_>,
    ctrl: &Arc<NodeController>,
    bound_to: Option<SocketAddr>,
) -> astar_http::Reply {
    let method = req.method().to_string();
    let url = req.target().to_string();
    let path_no_query = req.path().to_string();

    // The Host allow-list, before anything else — including the event
    // stream below, which never reaches the router. A page whose DNS was
    // pointed at this node gets neither the event stream nor a route, and
    // learns nothing from the answer about which it asked for. (A request
    // with no `Host` at all never gets here: astar-http answers it 400.)
    let host = req.header("Host").map(str::to_string);
    if !host_is_this_node(host.as_deref(), bound_to) {
        return to_reply(forbidden_cross_origin());
    }

    // The cap, on the announcement: a Content-Length already over it is a
    // 413 before a byte of the body is read — a request known to be refused
    // is not worth reading any of. Second, after Host, so a foreign caller
    // learns nothing about the cap; before the event stream, so the order
    // here is the router's own (Host, then cap, then route).
    // A length that is not plain digits is its own fault, told apart from a
    // body that stops short.
    if req
        .header("Content-Length")
        .is_some_and(|v| v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()))
    {
        return to_reply(HttpResponse::error_json(400, "bad Content-Length"));
    }
    let announced = req
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok());
    if announced.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return to_reply(payload_too_large());
    }

    // `/admin/<route>` is `<route>` on this port too, so the admin pages
    // work at the same paths on both binds. `/admin` itself redirects, so
    // the page's relative URLs resolve under `/admin/`.
    if path_no_query == "/admin" {
        return to_reply(HttpResponse {
            status: 303,
            content_type: "text/plain; charset=utf-8",
            body: b"See /admin/\n".to_vec(),
            headers: vec![("Location", "/admin/".to_string())],
        });
    }

    // Read before routing: the sign-in routes and the gate run before the
    // router does. astar-http holds the body to the cap and reads exactly
    // the announced length, never more; a chunked body is refused unread.
    let body = if method != "GET" && method != "HEAD" {
        match req.body() {
            Ok(b) => b.to_vec(),
            Err(e) if e.status == 411 => {
                return to_reply(HttpResponse::error_json(
                    411,
                    "chunked request bodies are not accepted; send Content-Length",
                ));
            }
            Err(e) if e.status == 413 => return to_reply(payload_too_large()),
            Err(_) => return to_reply(HttpResponse::error_json(400, "request body cut short")),
        }
    } else {
        Vec::new()
    };

    let original_path = path_no_query;
    let (path_no_query, url) = match strip_admin_prefix(&original_path) {
        Some(rest) => {
            let query = url.find('?').map_or("", |i| &url[i..]);
            (rest.to_string(), format!("{rest}{query}"))
        }
        None => (original_path.clone(), url.clone()),
    };
    #[cfg(feature = "auth")]
    let seen = Seen {
        method: &method,
        original_path: &original_path,
        admin_path: &path_no_query,
        url: &url,
        body: &body,
        host: host.as_deref(),
    };

    // The net recordings (`[netrec]`): their own pages, answered here, before
    // the gate, with what this request may see. Every session they name is
    // looked up with that access, so one it may not see answers exactly as a
    // missing one does. Their one write is `/admin/nets/delete`, an admin
    // route: it is not a `/nets` path, so it goes through the gate and the
    // router's guards like every other.
    if astar_netarchive::Pages::is_nets_path(&original_path) {
        let Some(pages) = ctrl.nets() else {
            return to_reply(HttpResponse::error_json(404, "not found"));
        };
        #[cfg(feature = "auth")]
        let access = ctrl.auth().map_or(astar_netarchive::Access::Admin, |auth| {
            crate::access::nets_access(auth.principal(&control_req(req, &seen)).as_ref())
        });
        // No sign-in in this build: the port's open policy, as everywhere.
        #[cfg(not(feature = "auth"))]
        let access = astar_netarchive::Access::Admin;
        let nets = astar_netarchive::NetsRequest {
            method: &method,
            path: &original_path,
            access,
            // The control port always serves `/admin/signin` when the pages
            // were built with a sign-in URL (see `run::nets_pages`).
            offer_sign_in: true,
        };
        return pages
            .handle(&nets)
            .into_reply(req.header("Range").map(str::to_owned));
    }

    // Sign-in, when this build has it and `serve` installed it: the
    // `/auth/*` routes and the sign-in pages first, then the gate, which
    // every admin route — the event stream included — passes or is
    // refused at. A loopback peer is the console and passes; anyone else
    // needs a session.
    #[cfg(feature = "auth")]
    if let Some(auth) = ctrl.auth()
        && let Some(resp) = sign_in_or_gate(auth, req, &seen)
    {
        return to_reply(resp);
    }

    // The recording and packet log downloads, streamed: past the gate, like the event
    // stream, and never through the router, which holds a whole body.
    if let Some(reply) = download(ctrl, &method, &path_no_query, &url, req.header("Range")) {
        return reply;
    }

    // The event stream: until the client leaves or the node stops.
    if method == "GET" && path_no_query == "/events" {
        let c = Arc::clone(ctrl);
        return astar_http::Reply::Stream {
            class: astar_http::StreamClass::Counted,
            run: Box::new(move |mut conn| {
                if conn
                    .write_head(
                        200,
                        &[
                            ("Content-Type", "text/event-stream"),
                            ("Cache-Control", "no-cache"),
                        ],
                    )
                    .is_err()
                    || conn.head_only()
                {
                    return;
                }
                let mut w = astar_http::sse::Writer::new(conn);
                let _ = crate::sse::pump_events(&mut w, &c, crate::sse::EVENT_TICK, || true);
            }),
        };
    }

    let headers = RequestHeaders {
        origin: req.header("Origin"),
        host: host.as_deref(),
        content_type: req.header("Content-Type"),
        sec_fetch_site: req.header("Sec-Fetch-Site"),
        bound_to,
        host_verified: false,
        // The socket's peer, never a header: see `astar_http::guards`.
        loopback_peer: astar_http::guards::is_loopback(req),
    };
    to_reply(handle_request_with_headers(
        ctrl, &method, &url, &body, &headers,
    ))
}

/// A recording or packet log download
/// (`crate::recording::http::download_for`) as a stream, or its refusal;
/// `None` for any other route.
fn download(
    ctrl: &Arc<NodeController>,
    method: &str,
    admin_path: &str,
    url: &str,
    range: Option<&str>,
) -> Option<astar_http::Reply> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    Some(
        match crate::recording::http::download_for(ctrl, method, admin_path, query)? {
            Ok(d) => crate::recording::http::stream_reply(ctrl, d, range),
            Err(refused) => to_reply(refused),
        },
    )
}

/// What `dispatch` has already read of a request, for the sign-in code.
#[cfg(feature = "auth")]
struct Seen<'a> {
    method: &'a str,
    original_path: &'a str,
    admin_path: &'a str,
    url: &'a str,
    body: &'a [u8],
    host: Option<&'a str>,
}

/// The request as the sign-in code reads it.
#[cfg(feature = "auth")]
fn control_req<'a>(request: &'a astar_http::Request<'_>, seen: &Seen<'a>) -> crate::auth::Req<'a> {
    crate::auth::Req {
        method: seen.method,
        path: seen.original_path,
        query: seen.url.split_once('?').map_or("", |(_, q)| q),
        body: seen.body,
        host: seen.host,
        origin: request.header("Origin"),
        content_type: request.header("Content-Type"),
        sec_fetch_site: request.header("Sec-Fetch-Site"),
        cookie: request.header("Cookie"),
        user_agent: request.header("User-Agent"),
        peer: Some(request.peer()),
        forwarded: request.forwarded(),
        surface: crate::auth::Surface::Control,
        tls: request.tls(),
    }
}

/// The sign-in routes and pages, then the gate: `Some` is the answer
/// (a sign-in response or a refusal), `None` lets the request through to
/// the event stream or the router.
#[cfg(feature = "auth")]
fn sign_in_or_gate(
    auth: &crate::auth::AuthService,
    request: &astar_http::Request<'_>,
    seen: &Seen<'_>,
) -> Option<HttpResponse> {
    let req = control_req(request, seen);
    if let Some(resp) = crate::auth::handle(auth, &req) {
        return Some(resp);
    }
    let admin_req = crate::auth::Req {
        path: seen.admin_path,
        ..req
    };
    match crate::auth::gate::admit(auth, &admin_req, seen.admin_path, seen.original_path) {
        crate::auth::gate::Admission::Refuse(resp) => Some(resp),
        crate::auth::gate::Admission::Allow(_) => None,
    }
}

/// An admin-router answer on the wire: its status, content type and the
/// route's extra headers, and astar-http's defaults (the security headers,
/// the CSP) where the route set none. Both the guard's early 403 and the
/// router's answer go through here so they are indistinguishable on the
/// wire except for what the router chose.
fn to_reply(resp: HttpResponse) -> astar_http::Reply {
    let mut r = astar_http::Response::new(resp.status).with_body(resp.content_type, resp.body);
    for (name, value) in resp.headers {
        r = r.header(name, value);
    }
    r.into()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{controller::NodeController, secrets::SecretProvider};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    fn test_controller() -> Arc<NodeController> {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(NodeController::new(station, secrets))
    }

    #[test]
    fn http_server_serves_status_over_a_socket() {
        let ctrl = test_controller();
        let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind ephemeral server");
        let port = server.local_addr().port();

        // Connect with a short timeout so the test cannot hang.
        let addr = format!("127.0.0.1:{port}");
        let mut stream = TcpStream::connect(&addr).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Send a minimal HTTP/1.1 GET /status request.
        stream
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write request");

        // Read the full response.
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");

        // Assert the response includes 200 and the word "listening".
        assert!(
            response.contains("200"),
            "response must contain 200: {response:?}"
        );
        assert!(
            response.contains("listening"),
            "response body must contain 'listening': {response:?}"
        );

        server.stop();
    }

    /// One raw request against a fresh server; the whole response back.
    fn roundtrip(raw: &str) -> String {
        let ctrl = test_controller();
        let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind ephemeral server");
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        server.stop();
        response
    }

    #[test]
    fn eight_event_streams_and_a_status_from_loopback_all_answer() {
        // Review Focus #1: every local tool is one client on this port.
        let ctrl = test_controller();
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        let streams: Vec<TcpStream> = (0..8)
            .map(|_| {
                let mut s = TcpStream::connect(addr).unwrap();
                s.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .unwrap();
                s
            })
            .collect();
        // Every stream is open and answered before /status is asked for.
        for mut s in &streams {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut head = [0u8; 12];
            s.read_exact(&mut head).unwrap();
            assert_eq!(&head, b"HTTP/1.1 200", "every stream opens");
        }
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        drop(streams);
        server.stop();
    }

    #[test]
    fn nodebench_reads_the_event_stream_from_the_new_server() {
        // Review Focus #5: close-delimited SSE, read by nodebench's own
        // client (ureq 2 and its SSE reader) the way the bench reads it.
        let ctrl = test_controller();
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let client =
            astar_nodebench::server::ServerClient::new(&format!("http://{}", server.local_addr()));
        let mut events = client.events().expect("GET /events");
        // The heartbeat snapshot is a frame like any other; nodebench keeps
        // the latest one.
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while events.latest().is_none() {
            assert!(
                std::time::Instant::now() < until && !events.is_finished(),
                "nodebench read no frame from /events"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        events.stop();
        server.stop();
    }

    #[test]
    fn a_cross_site_form_post_to_key_is_refused_over_the_socket() {
        // The iax-ctlcsrf attack, as a browser sends it: a <form> on another
        // site, posting text/plain to /key. Sec-Fetch-Site alone (no Origin)
        // is enough to mark it as a page's request.
        let response = roundtrip(
            "POST /key HTTP/1.1\r\nHost: 127.0.0.1\r\nSec-Fetch-Site: cross-site\r\n\
             Content-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        assert!(response.starts_with("HTTP/1.1 415"), "{response}");
    }

    #[test]
    fn a_curl_post_to_unkey_still_reaches_the_route_over_the_socket() {
        let response = roundtrip(
            "POST /unkey HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(
            !response.starts_with("HTTP/1.1 415") && !response.starts_with("HTTP/1.1 403"),
            "{response}"
        );
    }

    #[test]
    fn the_admin_prefix_reaches_the_same_routes() {
        let response =
            roundtrip("GET /admin/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("listening"), "{response}");
        let response =
            roundtrip("GET /admin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 303"), "{response}");
        assert!(response.contains("Location: /admin/"), "{response}");
        let response = roundtrip(
            "GET /admin/admin/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "stripped once only: {response}"
        );
    }

    // ---- /nets/ on the control port ----------------------------------------

    use crate::test_support::{NETS_PRIVATE_NET, mount_nets_fixture, nets_session_exists};

    /// One raw request to `addr`; everything the server sent before it closed.
    fn send(addr: std::net::SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(raw.as_bytes()).expect("write");
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The values of every `name` header in a raw response's head.
    fn header_values(response: &str, name: &str) -> Vec<String> {
        let head = response.split("\r\n\r\n").next().unwrap_or_default();
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .collect()
    }

    /// The body of a raw response.
    fn body_of(response: &str) -> &str {
        response.split_once("\r\n\r\n").map_or("", |(_, b)| b)
    }

    fn nets_get(addr: std::net::SocketAddr, path: &str, extra: &str) -> String {
        send(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{extra}Connection: close\r\n\r\n"),
        )
    }

    /// A delete as a page on this port would post it, plus `extra` headers.
    fn nets_delete(addr: std::net::SocketAddr, body: &str, extra: &str) -> String {
        let host = format!("127.0.0.1:{}", addr.port());
        send(
            addr,
            &format!(
                "POST /admin/nets/delete HTTP/1.1\r\nHost: {host}\r\n{extra}\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    fn delete_body(net: &str, session: &str) -> String {
        serde_json::json!({ "net": net, "session": session }).to_string()
    }

    #[test]
    fn control_port_loopback_console_sees_everything() {
        // No sign-in in this build or on this node: the port's open policy.
        let ctrl = test_controller();
        let (_dir, f) = mount_nets_fixture(&ctrl, false);
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        let r = nets_get(addr, "/nets/index.json", "");
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(
            r.contains(&f.private_session) && r.contains(&f.public_session),
            "{r}"
        );
        let private = format!("/nets/{NETS_PRIVATE_NET}/{}/", f.private_session);
        for path in [private.clone(), format!("{private}data.json")] {
            let r = nets_get(addr, &path, "");
            assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
        }
        let r = nets_get(addr, "/nets", "");
        assert!(r.starts_with("HTTP/1.1 308"), "{r}");
        // The audio streams with byte ranges, under the pages' own policy.
        let r = nets_get(
            addr,
            &format!("{private}session.flac"),
            "Range: bytes=0-3\r\n",
        );
        assert!(r.starts_with("HTTP/1.1 206"), "{r}");
        assert_eq!(body_of(&r), "fLaC");
        assert_eq!(
            header_values(&r, "Content-Security-Policy"),
            vec![astar_netarchive::NETS_CSP.to_string()]
        );
        server.stop();
    }

    #[cfg(feature = "auth")]
    #[test]
    fn control_port_loopback_console_sees_everything_with_sign_in_on() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Arc::new(crate::auth::AuthService::open(
            crate::auth::AuthSettings::defaults(dir.path()),
        ));
        let ctrl = test_controller();
        ctrl.set_auth(auth);
        let (_nets, f) = mount_nets_fixture(&ctrl, true);
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let r = nets_get(
            server.local_addr(),
            &format!("/nets/{NETS_PRIVATE_NET}/{}/data.json", f.private_session),
            "",
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        server.stop();
    }

    #[test]
    fn nets_carry_their_own_policy_and_other_routes_keep_theirs() {
        let ctrl = test_controller();
        let (_dir, _f) = mount_nets_fixture(&ctrl, false);
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        for path in ["/nets/", "/nets/index.json", "/nets/nets.css"] {
            let r = nets_get(addr, path, "");
            assert_eq!(
                header_values(&r, "Content-Security-Policy"),
                vec![astar_netarchive::NETS_CSP.to_string()],
                "{path}"
            );
            assert_eq!(header_values(&r, "X-Content-Type-Options"), vec!["nosniff"]);
        }
        for path in ["/status", "/", "/admin/", "/app.js"] {
            let r = nets_get(addr, path, "");
            assert!(r.starts_with("HTTP/1.1 200"), "{path}: {r}");
            assert_eq!(
                header_values(&r, "Content-Security-Policy"),
                vec![crate::http::CSP.to_string()],
                "{path}"
            );
        }
        server.stop();
    }

    #[test]
    fn control_port_nets_are_off_without_a_netrec_table() {
        let ctrl = test_controller();
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        for path in ["/nets/", "/nets", "/nets/index.json"] {
            let r = nets_get(addr, path, "");
            assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
        }
        let r = nets_delete(addr, "{}", "Content-Type: application/json\r\n");
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        assert!(body_of(&r).contains("net recordings are off"), "{r}");
        server.stop();
    }

    #[test]
    fn control_port_nets_delete_without_sign_in_still_needs_same_origin_json() {
        let ctrl = test_controller();
        let (dir, f) = mount_nets_fixture(&ctrl, false);
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        let body = delete_body(NETS_PRIVATE_NET, &f.private_session);
        let exists = || nets_session_exists(dir.path(), NETS_PRIVATE_NET, &f.private_session);
        let r = nets_delete(
            addr,
            &body,
            "Origin: http://evil.example\r\nContent-Type: application/json\r\n",
        );
        assert!(r.starts_with("HTTP/1.1 403"), "cross-origin: {r}");
        assert!(exists());
        let r = nets_delete(addr, &body, "");
        assert!(r.starts_with("HTTP/1.1 415"), "no JSON: {r}");
        assert!(exists());
        // Unprefixed, the URL is a page route: GET and HEAD only.
        let r = send(
            addr,
            &format!(
                "POST /nets/delete HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");
        assert!(exists());
        let origin = format!("Origin: http://127.0.0.1:{}\r\n", addr.port());
        let r = nets_delete(
            addr,
            &body,
            &format!("{origin}Content-Type: application/json\r\n"),
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(!exists(), "deleted");
        server.stop();
    }

    /// The control port with sign-in on and `loopback_login`, so the
    /// console bypass is off and every request is who its cookie says.
    #[cfg(feature = "auth")]
    fn strict_nets_port() -> (
        ControlServer,
        Arc<crate::auth::AuthService>,
        tempfile::TempDir,
        tempfile::TempDir,
        astar_netarchive::fixture::Fixture,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = crate::auth::AuthSettings::defaults(dir.path());
        settings.loopback_login = true;
        let auth = Arc::new(crate::auth::AuthService::open(settings));
        let ctrl = test_controller();
        ctrl.set_auth(Arc::clone(&auth));
        let (nets, f) = mount_nets_fixture(&ctrl, true);
        let server = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        (server, auth, dir, nets, f)
    }

    #[cfg(feature = "auth")]
    fn cookie_for(
        auth: &crate::auth::AuthService,
        name: &str,
        role: crate::auth::store::Role,
    ) -> String {
        format!(
            "Cookie: astar_session={}\r\n",
            auth.test_sign_in(name, role)
        )
    }

    #[cfg(feature = "auth")]
    #[test]
    fn control_port_hides_private_nets_sessions_from_anonymous() {
        // Anonymous two ways: a proxied request on a node with the console
        // bypass, and any cookieless request with `loopback_login`.
        let dir = tempfile::tempdir().unwrap();
        let auth = Arc::new(crate::auth::AuthService::open(
            crate::auth::AuthSettings::defaults(dir.path()),
        ));
        let ctrl = test_controller();
        ctrl.set_auth(auth);
        let (_nets, f) = mount_nets_fixture(&ctrl, true);
        let proxied = bind_control(&ctrl, "127.0.0.1:0").unwrap();
        let (strict, strict_auth, _d, _n, f2) = strict_nets_port();
        assert_eq!(f, f2, "the fixture is deterministic");
        for (addr, extra) in [
            (proxied.local_addr(), "X-Forwarded-For: 198.51.100.7\r\n"),
            (strict.local_addr(), ""),
        ] {
            let private = format!("/nets/{NETS_PRIVATE_NET}/{}", f.private_session);
            let missing = format!("/nets/{NETS_PRIVATE_NET}/2001-01-01-0000");
            for suffix in ["/data.json", "/session.flac", "/", ""] {
                let hidden = nets_get(addr, &format!("{private}{suffix}"), extra);
                let absent = nets_get(addr, &format!("{missing}{suffix}"), extra);
                assert!(hidden.starts_with("HTTP/1.1 404"), "{suffix}: {hidden}");
                assert_eq!(
                    body_of(&hidden),
                    body_of(&absent),
                    "{suffix}: hidden answers as missing"
                );
            }
            let public = format!("/nets/{}/{}/data.json", f.public_net, f.public_session);
            let r = nets_get(addr, &public, extra);
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
            let r = nets_get(addr, "/nets/index.json", extra);
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
            assert!(
                !r.contains(&f.private_session) && !r.contains(&f.open_session),
                "{r}"
            );
            assert!(r.contains("/admin/signin?next=/nets/"), "{r}");
        }
        // A viewer's session sees it.
        let viewer = cookie_for(&strict_auth, "V1EW", crate::auth::store::Role::Viewer);
        let r = nets_get(
            strict.local_addr(),
            &format!("/nets/{NETS_PRIVATE_NET}/{}/data.json", f.private_session),
            &viewer,
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        proxied.stop();
        strict.stop();
    }

    #[cfg(feature = "auth")]
    #[test]
    fn control_port_nets_delete_passes_the_gate_and_the_mutating_guards() {
        use crate::auth::store::Role;
        let (server, auth, _d, nets, f) = strict_nets_port();
        let addr = server.local_addr();
        let body = delete_body(NETS_PRIVATE_NET, &f.private_session);
        let exists = || nets_session_exists(nets.path(), NETS_PRIVATE_NET, &f.private_session);
        let admin = cookie_for(&auth, "A1DM", Role::Admin);
        let viewer = cookie_for(&auth, "V1EW", Role::Viewer);
        let origin = format!("Origin: http://127.0.0.1:{}\r\n", addr.port());
        let json = "Content-Type: application/json\r\n";

        let r = nets_delete(
            addr,
            &body,
            &format!("{admin}Origin: http://evil.example\r\n{json}"),
        );
        assert!(r.starts_with("HTTP/1.1 403"), "cross-origin: {r}");
        let r = nets_delete(addr, &body, &format!("{admin}{origin}"));
        assert!(r.starts_with("HTTP/1.1 415"), "no JSON: {r}");
        let r = nets_delete(addr, &body, &format!("{viewer}{origin}{json}"));
        assert!(r.starts_with("HTTP/1.1 403"), "viewer: {r}");
        assert!(body_of(&r).contains("admins_only"), "{r}");
        let r = nets_delete(addr, &body, &format!("{origin}{json}"));
        assert!(r.starts_with("HTTP/1.1 401"), "anonymous: {r}");
        assert!(exists(), "nothing deleted yet");

        let open = delete_body(NETS_PRIVATE_NET, &f.open_session);
        let r = nets_delete(addr, &open, &format!("{admin}{origin}{json}"));
        assert!(r.starts_with("HTTP/1.1 409"), "open: {r}");
        let r = nets_delete(addr, &body, &format!("{admin}{origin}{json}"));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(body_of(&r).contains("\"deleted\":true"), "{r}");
        assert!(!exists(), "deleted");
        let audit = auth.read(|d| {
            d.audit
                .iter()
                .map(|e| (e.event.clone(), e.user.clone()))
                .collect::<Vec<_>>()
        });
        assert!(
            audit.contains(&("web_nets_delete".to_string(), Some("A1DM".to_string()))),
            "{audit:?}"
        );
        server.stop();
    }

    // ---- the sign-in gate on the control port (feature `auth`) -------------

    #[cfg(feature = "auth")]
    fn auth_roundtrip(
        raw: &str,
        tweak: impl FnOnce(&mut crate::auth::AuthSettings),
    ) -> (String, Arc<crate::auth::AuthService>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = crate::auth::AuthSettings::defaults(dir.path());
        tweak(&mut settings);
        let auth = Arc::new(crate::auth::AuthService::open(settings));
        let ctrl = test_controller();
        ctrl.set_auth(Arc::clone(&auth));
        let raw = raw.replace("{cookie}", &auth_cookie(&auth));
        let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind ephemeral server");
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(raw.as_bytes()).expect("write");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        server.stop();
        (response, auth, dir)
    }

    /// A session for an admin, as the plain-HTTP cookie the control port reads.
    #[cfg(feature = "auth")]
    fn auth_cookie(auth: &crate::auth::AuthService) -> String {
        format!(
            "astar_session={}",
            auth.test_sign_in("A1DM", crate::auth::store::Role::Admin)
        )
    }

    #[cfg(feature = "auth")]
    #[test]
    fn a_loopback_peer_is_the_console() {
        let (r, _, _d) = auth_roundtrip(
            "GET /admin/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            |_| {},
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let (r, _, _d) = auth_roundtrip(
            "GET /auth/me HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            |_| {},
        );
        assert!(r.contains("\"console\":true"), "{r}");
    }

    #[cfg(feature = "auth")]
    #[test]
    fn a_proxy_header_ends_the_loopback_bypass() {
        for h in [
            "X-Forwarded-For: 198.51.100.7",
            "Forwarded: for=198.51.100.7",
        ] {
            let (r, _, _d) = auth_roundtrip(
                &format!(
                    "GET /status HTTP/1.1\r\nHost: localhost\r\n{h}\r\nConnection: close\r\n\r\n"
                ),
                |_| {},
            );
            assert!(r.starts_with("HTTP/1.1 401"), "{h}: {r}");
            assert!(r.contains("signin_required"), "{r}");
            let (r, _, _d) = auth_roundtrip(
                &format!(
                    "GET /admin/ HTTP/1.1\r\nHost: localhost\r\n{h}\r\nConnection: close\r\n\r\n"
                ),
                |_| {},
            );
            assert!(r.starts_with("HTTP/1.1 303"), "{r}");
            assert!(r.contains("Location: /admin/signin?next=/admin/"), "{r}");
        }
    }

    #[cfg(feature = "auth")]
    #[test]
    fn loopback_login_needs_a_session_for_every_route_and_the_stream() {
        let strict = |s: &mut crate::auth::AuthSettings| s.loopback_login = true;
        for path in ["/status", "/events", "/admin/settings/state"] {
            let (r, _, _d) = auth_roundtrip(
                &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
                strict,
            );
            assert!(r.starts_with("HTTP/1.1 401"), "{path}: {r}");
        }
        let (r, _, _d) = auth_roundtrip(
            "GET /status HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n",
            strict,
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // Shutdown needs a fresh credential; a fresh test sign-in is one.
        let (r, _, _d) = auth_roundtrip(
            "POST /shutdown HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            strict,
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    }
}
