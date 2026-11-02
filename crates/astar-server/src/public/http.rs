// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public router: a closed list of read-only GET routes, and the code
//! that answers them on one request ([`handle`]).
//!
//! This is **not** the admin router with checks bolted on. It never calls
//! `NodeController::execute`, never reads a request body outside the
//! sign-in groups, and names every path it answers in [`PUBLIC_ROUTES`];
//! anything else is 404. The server under it is astar-http (one request per
//! connection, heads capped at [`MAX_HEAD_BYTES`], one deadline for the
//! request phase); a listener that stops reading is dropped after
//! [`WRITE_STALL`], and its slot is freed.
//!
//! Order of checks, for every request: parse → missing `Host` (400, answered
//! by astar-http before routing) → `Host` allowlist (421) → path (404) →
//! method (405) → route.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use astar_http::{Conn, Reply, Request, Response, StreamClass, reason, sse};

use crate::controller::NodeController;
use crate::public::status::{Activity, PublicStatus, StreamAvailability, mode_name};

// ---- assets -------------------------------------------------------------------

const LISTENER_HTML: &str = include_str!("../assets/listener.html");
const LISTENER_JS: &str = include_str!("../assets/listener.js");
const LISTENER_CSS: &str = include_str!("../assets/listener.css");
const LISTENER_WORKLET_JS: &str = include_str!("../assets/listener-worklet.js");
const ADMIN_PLACEHOLDER_HTML: &str = include_str!("../assets/admin_placeholder.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");

/// Every path the public bind answers without sign-in. Anything else is
/// 404, whatever the method — except, in a build with the `auth` feature
/// and `[web] admin_on_public`, the groups in [`SIGNED_IN_GROUPS`]. `/admin`
/// is the sign-in's way in; without sign-in it is a placeholder page.
pub const PUBLIC_ROUTES: &[&str] = &[
    "/",
    "/admin",
    "/listener.js",
    "/listener.css",
    "/listener-worklet.js",
    "/style.css",
    "/public/status",
    "/public/events",
    "/stream.mp3",
    "/stream.ws",
];

/// The path groups sign-in adds to the public bind, each answered by the
/// sign-in routes or passed through the session gate
/// ([`crate::auth::gate::admit`]) and never forwarded otherwise:
///
/// * `/auth/…` — the sign-in API (`/auth/cli/invite` is refused here);
/// * `/admin/signin`, `/admin/invite`, `/admin/account`, `/admin/users`
///   and their assets — the sign-in pages;
/// * `/admin/<route>` — an admin-router route, reached only when it is in
///   [`crate::access::ADMIN_ROUTES`] and the session's role (and step-up,
///   by class) allows it.
pub const SIGNED_IN_GROUPS: &[&str] = &["/auth/", "/admin/"];

/// The policy every public response carries. Same shape as the admin
/// assets' policy, plus the stream and the worklet: `connect-src` for the
/// WebSocket and the event feed, `media-src` for the MP3 fallback.
pub const PUBLIC_CSP: &str = "default-src 'self'; style-src 'self'; script-src 'self'; \
     connect-src 'self'; media-src 'self'; img-src 'self' data:; \
     base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The largest request head (request line plus headers) read.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;
/// A streaming response whose client has not taken a byte for this long is
/// closed (the design's "disconnected after 30 s with no progress").
pub const WRITE_STALL: Duration = Duration::from_secs(30);
/// How often `/public/events` sends `levels` (10 Hz).
pub const LEVELS_EVERY: Duration = Duration::from_millis(100);
/// How often `/public/events` re-checks the status, sending it on change.
pub const STATUS_EVERY: Duration = Duration::from_secs(1);
/// The longest `/public/events` goes without sending the status anyway.
pub const STATUS_REFRESH: Duration = Duration::from_secs(5);
/// A comment line when nothing else has been sent for this long.
pub const KEEPALIVE: Duration = Duration::from_secs(15);

// ---- the stream seam ----------------------------------------------------------

/// The two streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// 20 ms Opus packets, one per WebSocket binary message.
    Opus,
    /// A continuous MPEG Layer III byte stream.
    Mp3,
}

/// Why a stream cannot be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubRefusal {
    /// `max_listeners` reached.
    Full,
    /// No tap in this mode (handset or parrot), or the encoder failed.
    NoAudio,
    /// This server was built without the format (`stream-mp3`).
    NotBuilt,
}

impl HubRefusal {
    /// The one-line plain-text reason the 503 carries.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Full => "Full, try again shortly",
            Self::NoAudio => "No live audio in this mode",
            Self::NotBuilt => "MP3 isn't built into this server",
        }
    }
}

/// The 503 text when `[stream]` is off.
pub const STREAM_OFF_TEXT: &str = "Live audio is off on this node";

/// The latest level reading: RMS dBFS and the voice-band spectrum.
#[derive(Debug, Clone, PartialEq)]
pub struct Levels {
    pub dbfs: f32,
    pub spectrum: Vec<f32>,
    pub rate: u32,
}

/// What a listener's next pull brought.
#[derive(Debug, Clone)]
pub enum Pull {
    /// The next encoded chunk (after a skip forward, if the listener lagged).
    Chunk(Arc<[u8]>),
    /// Nothing within the timeout.
    Idle,
    /// The stream is gone (rate change, shutdown): the client reconnects.
    Closed,
}

/// One listener's cursor. Dropping it frees its `max_listeners` slot.
pub trait Listener: Send {
    fn pull(&mut self, timeout: Duration) -> Pull;
}

/// The audio side as the router sees it. `StreamHub` implements it; tests
/// use fakes, and with `[stream]` off there is none at all.
pub trait StreamSource: Send + Sync {
    /// A listener on `format`, or why not.
    ///
    /// # Errors
    /// [`HubRefusal`]: full, no audio in this mode, or not built.
    fn open(&self, format: Format) -> Result<Box<dyn Listener>, HubRefusal>;
    /// The latest levels, about 10 Hz, while audio flows.
    fn levels(&self) -> Option<Levels>;
    fn availability(&self) -> StreamAvailability;
    /// Listeners across both streams.
    fn listeners(&self) -> u32;
    /// The pipeline rate the streams run at, while live.
    fn sample_rate(&self) -> Option<u32>;
    /// Built with `stream-mp3`.
    fn mp3_built(&self) -> bool;
}

/// ACME HTTP-01 answers: token → key authorisation.
pub type ChallengeMap = Arc<Mutex<HashMap<String, String>>>;

/// Everything the public router reads.
pub struct PublicCtx {
    pub ctrl: Arc<NodeController>,
    /// `None` ⇔ `[stream] enabled = false`.
    pub hub: Option<Arc<dyn StreamSource>>,
    pub activity: Arc<Mutex<Activity>>,
    /// Expanded, normalised `public_hostnames`.
    pub hostnames: Vec<String>,
    /// A certificate is live (drives the page's banner and port 80).
    pub tls: Arc<AtomicBool>,
    /// Ends streaming responses at shutdown.
    pub stop: Arc<AtomicBool>,
    /// `[web] anonymous_status`: `false` means the listener page and its
    /// streams need a session, and a build or config with no sign-in on
    /// this bind refuses them rather than serving them openly.
    pub anonymous_status: bool,
    /// The limits that keep one client from holding the bind.
    pub guard: crate::public::guard::Guard,
}

impl PublicCtx {
    /// The public status as of now.
    #[must_use]
    pub fn status(&self) -> PublicStatus {
        let snap = self.ctrl.snapshot();
        let (stream, listeners, mp3) = match &self.hub {
            None => (StreamAvailability::Off, 0, false),
            Some(h) => (h.availability(), h.listeners(), h.mp3_built()),
        };
        self.activity.lock().expect("activity poisoned").project(
            &snap,
            mode_name(self.ctrl.bridge_mode()),
            stream,
            listeners,
            mp3,
            now_ms(),
        )
    }
}

/// Unix time in milliseconds.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- routing ------------------------------------------------------------------

/// The router's decision for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Static {
        ctype: &'static str,
        body: &'static str,
    },
    Json(String),
    Sse,
    Mp3,
    WebSocket,
    Status {
        code: u16,
        text: &'static str,
    },
    Redirect(String),
    /// A sign-in answer, or an admin-router answer for a signed-in admin.
    Response(crate::http::HttpResponse),
    /// The admin event stream, for a signed-in admin.
    AdminSse,
    /// A recording or the recordings tar, for a signed-in admin, with the
    /// request's `Range` header.
    Download(crate::recording::http::Download, Option<String>),
}

/// `Host` as compared, and the allow-list check: astar-http's.
pub use astar_http::guards::{host_allowed_raw as host_allowed, normalize_host};

fn misdirected() -> Route {
    Route::Status {
        code: 421,
        text: "Misdirected request",
    }
}

fn not_found() -> Route {
    Route::Status {
        code: 404,
        text: "Not found",
    }
}

/// The pure public router.
#[must_use]
pub fn route(ctx: &PublicCtx, method: &str, path: &str, host: Option<&str>) -> Route {
    if !host_allowed(&ctx.hostnames, host) {
        return misdirected();
    }
    let path = path.split('?').next().unwrap_or(path);
    if !PUBLIC_ROUTES.contains(&path) {
        return not_found();
    }
    if method != "GET" && method != "HEAD" {
        return Route::Status {
            code: 405,
            text: "Method not allowed",
        };
    }
    let stream_refusal = |format: Format| -> Option<Route> {
        let Some(hub) = &ctx.hub else {
            return Some(Route::Status {
                code: 503,
                text: STREAM_OFF_TEXT,
            });
        };
        if format == Format::Mp3 && !hub.mp3_built() {
            return Some(Route::Status {
                code: 503,
                text: HubRefusal::NotBuilt.text(),
            });
        }
        (hub.availability() != StreamAvailability::Live).then_some(Route::Status {
            code: 503,
            text: HubRefusal::NoAudio.text(),
        })
    };
    match path {
        "/" => Route::Static {
            ctype: "text/html; charset=utf-8",
            body: LISTENER_HTML,
        },
        "/admin" => Route::Static {
            ctype: "text/html; charset=utf-8",
            body: ADMIN_PLACEHOLDER_HTML,
        },
        "/listener.js" => Route::Static {
            ctype: "text/javascript; charset=utf-8",
            body: LISTENER_JS,
        },
        "/listener.css" => Route::Static {
            ctype: "text/css; charset=utf-8",
            body: LISTENER_CSS,
        },
        "/listener-worklet.js" => Route::Static {
            ctype: "text/javascript; charset=utf-8",
            body: LISTENER_WORKLET_JS,
        },
        "/style.css" => Route::Static {
            ctype: "text/css; charset=utf-8",
            body: STYLE_CSS,
        },
        "/public/status" => Route::Json(status_json(ctx)),
        "/public/events" => Route::Sse,
        "/stream.mp3" => stream_refusal(Format::Mp3).unwrap_or(Route::Mp3),
        "/stream.ws" => stream_refusal(Format::Opus).unwrap_or(Route::WebSocket),
        _ => not_found(),
    }
}

/// The status as the page reads it, with the TLS flag folded in.
fn status_json(ctx: &PublicCtx) -> String {
    let mut v = serde_json::to_value(ctx.status()).unwrap_or_default();
    v["secure"] = serde_json::Value::Bool(ctx.tls.load(Ordering::Relaxed));
    if let Some(rate) = ctx.hub.as_ref().and_then(|h| h.sample_rate()) {
        v["rate"] = rate.into();
    }
    v.to_string()
}

/// What the port-80 listener does with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http80 {
    /// An ACME HTTP-01 answer: the key authorisation, as text.
    Challenge(String),
    /// No certificate yet: serve the public page itself over plain HTTP, so
    /// listeners are not redirected to an HTTPS port that cannot answer.
    Public,
    /// Everything else: `301` to the same path on HTTPS.
    Redirect(String),
    /// Refused.
    Status { code: u16, text: &'static str },
}

/// The port-80 router: the challenge (any `Host`: the CA asks by name, and
/// a token is unguessable), then the `Host` allowlist, then the redirect —
/// or, before a certificate exists, the public page itself.
#[must_use]
pub fn route_http80(
    challenges: &ChallengeMap,
    hostnames: &[String],
    tls_live: bool,
    path: &str,
    host: Option<&str>,
) -> Http80 {
    let path = path.split('?').next().unwrap_or(path);
    if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        return match challenges.lock().expect("challenges poisoned").get(token) {
            Some(ka) => Http80::Challenge(ka.clone()),
            None => Http80::Status {
                code: 404,
                text: "Not found",
            },
        };
    }
    if !host_allowed(hostnames, host) {
        return Http80::Status {
            code: 421,
            text: "Misdirected request",
        };
    }
    if !tls_live {
        return Http80::Public;
    }
    let name = normalize_host(host.unwrap_or_default());
    let path = if path.starts_with('/') { path } else { "/" };
    Http80::Redirect(format!("https://{name}{path}"))
}

// ---- one request ------------------------------------------------------------------

/// A request head as the router reads it, filled from astar-http's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    /// One header by name, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Which router a connection is answered by.
pub enum Listening<'a> {
    /// The public bind.
    Public,
    /// The port-80 listener.
    Port80 { challenges: &'a ChallengeMap },
}

/// One public request: astar's routing over astar-http's server. Parsing,
/// the deadline, the caps, the default headers (the CSP, nosniff, framing,
/// referrer, and HSTS over TLS) and the stream slots are the server's.
pub fn handle(ctx: &Arc<PublicCtx>, req: &mut Request<'_>, listening: &Listening<'_>) -> Reply {
    let head = RequestHead {
        method: req.method().to_string(),
        target: req.target().to_string(),
        headers: req.headers().to_vec(),
    };
    let (peer, tls) = (req.peer(), req.tls());
    // The `Sec-WebSocket-Accept` for an upgrade, `None` when this is not one.
    #[cfg(feature = "stream")]
    let upgrade = astar_http::ws::accept_key(req);
    #[cfg(not(feature = "stream"))]
    let upgrade: Option<String> = None;
    // A body is read only for the sign-in groups' writes; every other
    // public route is a GET and never reads one.
    let body = if wants_body(&head) {
        match req.body() {
            Ok(b) => b,
            Err(e) => {
                return Response::text(e.status, format!("{}\n", reason(e.status))).into();
            }
        }
    } else {
        &[]
    };
    let incoming = Incoming {
        head: &head,
        body,
        peer: Some(peer),
        tls,
    };
    let decision = match listening {
        Listening::Public => route_request(ctx, &incoming),
        Listening::Port80 { challenges } => {
            match route_http80(
                challenges,
                &ctx.hostnames,
                ctx.tls.load(Ordering::Relaxed),
                &head.target,
                head.header("Host"),
            ) {
                Http80::Challenge(ka) => {
                    return Response::new(200)
                        .with_body("application/octet-stream", ka)
                        .into();
                }
                Http80::Public => route_request(ctx, &incoming),
                Http80::Redirect(to) => Route::Redirect(to),
                Http80::Status { code, text } => Route::Status { code, text },
            }
        }
    };
    let recheck = Recheck::new(Arc::clone(ctx), stream_session(ctx, &incoming, &decision));
    reply(decision, recheck, upgrade)
}

/// `Cache-Control: no-cache`, for the static assets.
fn no_cache(r: Response) -> Response {
    r.header("Cache-Control", "no-cache")
}

/// `Cache-Control: no-store`, for anything about the node's state.
fn no_store(r: Response) -> Response {
    r.header("Cache-Control", "no-store")
}

/// The router's decision as the server sends it. A long-lived stream runs
/// inside its stream slot (taken by the server before `run`), so a client
/// at its share is refused before the hub is asked for a listener.
fn reply(decision: Route, recheck: Recheck, upgrade: Option<String>) -> Reply {
    match decision {
        Route::Static { ctype, body } => no_cache(Response::new(200).with_body(ctype, body)).into(),
        Route::Json(body) => no_store(Response::json(200, body)).into(),
        Route::Status { code, text } => {
            let r = no_store(Response::text(code, format!("{text}\n")));
            if code == 405 {
                r.header("Allow", "GET, HEAD").into()
            } else {
                r.into()
            }
        }
        Route::Redirect(to) => Response::text(301, "Moved\n").header("Location", to).into(),
        Route::Response(resp) => {
            // The server adds the public policy and nosniff to every
            // response; the admin router's copies would replace them.
            let mut r = Response::new(resp.status).with_body(resp.content_type, resp.body);
            for (k, v) in resp.headers {
                if !k.eq_ignore_ascii_case("Content-Security-Policy")
                    && !k.eq_ignore_ascii_case("X-Content-Type-Options")
                {
                    r = r.header(k, v);
                }
            }
            r.into()
        }
        Route::Sse => event_stream(recheck, run_sse),
        Route::AdminSse => event_stream(recheck, run_admin_sse),
        Route::Download(d, range) => {
            crate::recording::http::stream_reply(&recheck.ctx.ctrl, d, range.as_deref())
        }
        Route::Mp3 => Reply::Stream {
            class: StreamClass::PerClientOnly,
            run: Box::new(move |conn| {
                let _ = run_mp3(conn, recheck);
            }),
        },
        Route::WebSocket => websocket(recheck, upgrade),
    }
}

/// An event stream (counted against the event streams overall): its head,
/// then `run` until it ends.
fn event_stream(mut recheck: Recheck, run: fn(&mut Conn, &mut Recheck) -> io::Result<()>) -> Reply {
    Reply::Stream {
        class: StreamClass::Counted,
        run: Box::new(move |mut conn| {
            if conn
                .write_head(
                    200,
                    &[
                        ("Content-Type", "text/event-stream"),
                        ("Cache-Control", "no-store"),
                    ],
                )
                .is_err()
                || conn.head_only()
            {
                return;
            }
            let _ = run(&mut conn, &mut recheck);
        }),
    }
}

/// The session a stream was admitted on, re-checked while it runs. Never
/// printed: it holds the cookie's token.
#[cfg(feature = "auth")]
struct StreamSession {
    token: String,
    /// The stream needs an admin, not just a session.
    admin: bool,
}

/// The session `decision` streams on, when it is a stream that needed one:
/// the admin event stream, or the listener page's streams on a private
/// page.
#[cfg(feature = "auth")]
fn stream_session(ctx: &PublicCtx, inc: &Incoming<'_>, decision: &Route) -> Option<StreamSession> {
    let path = inc
        .head
        .target
        .split('?')
        .next()
        .unwrap_or(&inc.head.target);
    let admin = match decision {
        // `/events` is admin-class (crate::access::ADMIN_ROUTES).
        Route::AdminSse => true,
        Route::Sse | Route::Mp3 | Route::WebSocket
            if !ctx.anonymous_status && needs_viewer(path) =>
        {
            false
        }
        _ => return None,
    };
    let token = crate::auth::session::cookie_value(
        inc.head.header("Cookie"),
        crate::auth::session::session_cookie(inc.tls),
    )
    .unwrap_or_default()
    .to_string();
    Some(StreamSession { token, admin })
}

/// Without sign-in no stream is admitted on a session.
#[cfg(not(feature = "auth"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature sign-in builds share"
)]
const fn stream_session(_: &PublicCtx, _: &Incoming<'_>, _: &Route) -> Option<()> {
    None
}

/// Ends a stream once the session it was admitted on stops standing
/// (signed out, user removed or demoted, expired, sign-in turned off),
/// checked every [`crate::public::guard::Limits::session_recheck`]. A stream
/// that needed no session is never ended by it.
struct Recheck {
    ctx: Arc<PublicCtx>,
    #[cfg(feature = "auth")]
    session: Option<StreamSession>,
    #[cfg(feature = "auth")]
    last: Instant,
}

impl Recheck {
    #[cfg(feature = "auth")]
    fn new(ctx: Arc<PublicCtx>, session: Option<StreamSession>) -> Self {
        Self {
            ctx,
            session,
            last: Instant::now(),
        }
    }

    #[cfg(not(feature = "auth"))]
    fn new(ctx: Arc<PublicCtx>, _: Option<()>) -> Self {
        Self { ctx }
    }

    /// The stream may go on.
    #[cfg_attr(
        not(feature = "auth"),
        allow(clippy::unused_self, reason = "nothing to re-check without sign-in")
    )]
    fn allowed(&mut self) -> bool {
        #[cfg(feature = "auth")]
        if let Some(s) = &self.session
            && self.last.elapsed() >= self.ctx.guard.limits.session_recheck
        {
            self.last = Instant::now();
            return self
                .ctx
                .ctrl
                .auth()
                .filter(|svc| svc.settings.admin_on_public)
                .and_then(|svc| svc.session_still_valid(&s.token))
                .is_some_and(|p| !s.admin || p.is_admin());
        }
        true
    }
}

/// A request as the public bind received it: head, body and peer.
pub struct Incoming<'a> {
    pub head: &'a RequestHead,
    pub body: &'a [u8],
    pub peer: Option<std::net::SocketAddr>,
    pub tls: bool,
}

/// Whether a body is read for this request: a write to a sign-in group.
fn wants_body(head: &RequestHead) -> bool {
    matches!(head.method.as_str(), "POST" | "DELETE" | "PUT" | "PATCH")
        && SIGNED_IN_GROUPS.iter().any(|g| head.target.starts_with(g))
}

/// The listener page's paths that need a session when
/// `anonymous_status = false`: the page, its status and its streams. The
/// scripts and styles stay open; they say nothing about the node.
fn needs_viewer(path: &str) -> bool {
    matches!(
        path,
        "/" | "/public/status" | "/public/events" | "/stream.mp3" | "/stream.ws"
    )
}

/// The whole public router: the `Host` allow-list, then — with sign-in
/// built in and on — the sign-in groups and the viewer gate, then the
/// closed list of read-only routes ([`route`]).
#[must_use]
pub fn route_request(ctx: &PublicCtx, inc: &Incoming<'_>) -> Route {
    let head = inc.head;
    let host = head.header("Host");
    if !host_allowed(&ctx.hostnames, host) {
        return misdirected();
    }
    let path = head.target.split('?').next().unwrap_or(&head.target);
    #[cfg(feature = "auth")]
    let signin = ctx
        .ctrl
        .auth()
        .filter(|svc| svc.settings.admin_on_public)
        .cloned();
    #[cfg(feature = "auth")]
    if let Some(svc) = &signin {
        if let Some(r) = signed_in_route(ctx, svc, inc, path) {
            return r;
        }
        if !ctx.anonymous_status && needs_viewer(path) {
            let req = auth_req(inc, path);
            if svc.principal(&req).is_none() {
                return Route::Response(crate::auth::gate::sign_in_needed(
                    &head.method,
                    if path == "/" { "/" } else { path },
                    path,
                ));
            }
        }
        return route(ctx, &head.method, &head.target, host);
    }
    if !ctx.anonymous_status && needs_viewer(path) {
        // Fail closed: the operator asked for a signed-in page, and this
        // bind has no sign-in to offer.
        return Route::Status {
            code: 503,
            text: "This page needs sign-in, which this node does not offer here",
        };
    }
    route(ctx, &head.method, &head.target, host)
}

/// The request as the sign-in code reads it.
#[cfg(feature = "auth")]
fn auth_req<'a>(inc: &Incoming<'a>, path: &'a str) -> crate::auth::Req<'a> {
    let head = inc.head;
    crate::auth::Req {
        method: &head.method,
        path,
        query: head.target.split_once('?').map_or("", |(_, q)| q),
        body: inc.body,
        host: head.header("Host"),
        origin: head.header("Origin"),
        content_type: head.header("Content-Type"),
        sec_fetch_site: head.header("Sec-Fetch-Site"),
        cookie: head.header("Cookie"),
        user_agent: head.header("User-Agent"),
        peer: inc.peer,
        forwarded: head.header("Forwarded").is_some() || head.header("X-Forwarded-For").is_some(),
        surface: crate::auth::Surface::Public,
        tls: inc.tls,
    }
}

/// The sign-in groups: `None` when `path` is in none of them.
#[cfg(feature = "auth")]
fn signed_in_route(
    ctx: &PublicCtx,
    svc: &crate::auth::AuthService,
    inc: &Incoming<'_>,
    path: &str,
) -> Option<Route> {
    if path == "/admin" {
        return Some(Route::Response(crate::auth::gate::redirect("/admin/")));
    }
    let req = auth_req(inc, path);
    if let Some(resp) = crate::auth::handle(svc, &req) {
        return Some(Route::Response(resp));
    }
    let admin_path = crate::access::strip_admin_prefix(path)?;
    let admin_req = crate::auth::Req {
        path: admin_path,
        ..req
    };
    match crate::auth::gate::admit(svc, &admin_req, admin_path, path) {
        crate::auth::gate::Admission::Refuse(resp) => Some(Route::Response(resp)),
        crate::auth::gate::Admission::Allow(_) => {
            if req.method == "GET" && admin_path == "/events" {
                return Some(Route::AdminSse);
            }
            if let Some(d) =
                crate::recording::http::download_for(&ctx.ctrl, req.method, admin_path, req.query)
            {
                return Some(match d {
                    Ok(d) => Route::Download(d, inc.head.header("Range").map(str::to_owned)),
                    Err(refused) => Route::Response(refused),
                });
            }
            let target = if req.query.is_empty() {
                admin_path.to_string()
            } else {
                format!("{admin_path}?{}", req.query)
            };
            let headers = crate::http::RequestHeaders {
                origin: req.origin,
                host: req.host,
                content_type: req.content_type,
                sec_fetch_site: req.sec_fetch_site,
                bound_to: None,
                host_verified: true,
                loopback_peer: false,
            };
            Some(Route::Response(crate::http::handle_request_with_headers(
                &ctx.ctrl, req.method, &target, inc.body, &headers,
            )))
        }
    }
}

/// The `levels` event payload.
#[must_use]
pub fn levels_json(l: &Levels) -> String {
    // Through f64 at a tenth, so the JSON says -23.1 and not the f32's
    // nearest neighbour widened.
    let round = |x: f32| (f64::from(x) * 10.0).round() / 10.0;
    serde_json::json!({
        "dbfs": round(l.dbfs),
        "spectrum": l.spectrum.iter().map(|x| round(*x)).collect::<Vec<_>>(),
        "rate": l.rate,
    })
    .to_string()
}

/// `/public/events`: `status` on change (checked every second, sent at
/// least every five), `levels` at 10 Hz while audio flows, a comment line
/// when nothing else went out for fifteen seconds. Ends on a write error
/// (the viewer left, or stalled past the write timeout), at shutdown, or
/// when the session a private page's feed was opened on no longer stands.
fn run_sse(conn: &mut Conn, recheck: &mut Recheck) -> io::Result<()> {
    let ctx = Arc::clone(&recheck.ctx);
    let mut w = sse::Writer::with_keepalive(conn, KEEPALIVE);
    let mut last_status = String::new();
    let mut status_checked: Option<Instant> = None;
    let mut status_sent = Instant::now();
    loop {
        if ctx.stop.load(Ordering::Relaxed) || !recheck.allowed() {
            return Ok(());
        }
        if status_checked.is_none_or(|t| t.elapsed() >= STATUS_EVERY) {
            status_checked = Some(Instant::now());
            let s = status_json(&ctx);
            if s != last_status || status_sent.elapsed() >= STATUS_REFRESH {
                w.event(Some("status"), &s)?;
                last_status = s;
                status_sent = Instant::now();
            }
        }
        if let Some(l) = ctx.hub.as_ref().and_then(|h| h.levels()) {
            w.event(Some("levels"), &levels_json(&l))?;
        }
        w.keepalive_if_due()?;
        std::thread::sleep(LEVELS_EVERY);
    }
}

/// The admin event stream, copied to a signed-in admin until either side
/// stops or the admin's session no longer stands.
fn run_admin_sse(w: &mut Conn, recheck: &mut Recheck) -> io::Result<()> {
    let ctx = Arc::clone(&recheck.ctx);
    let mut w = sse::Writer::new(w);
    crate::sse::pump_events(&mut w, &ctx.ctrl, crate::sse::EVENT_TICK, || {
        !ctx.stop.load(Ordering::Relaxed) && recheck.allowed()
    })
}

/// A listener on `format` from the hub, with the connection back; or the
/// 503 that says why not, answered on `conn` (whose stream slot is already
/// held) as a whole response.
fn open_or_503(conn: Conn, ctx: &PublicCtx, format: Format) -> Option<(Conn, Box<dyn Listener>)> {
    let refusal = match &ctx.hub {
        None => STREAM_OFF_TEXT,
        Some(hub) => match hub.open(format) {
            Ok(l) => return Some((conn, l)),
            Err(r) => r.text(),
        },
    };
    conn.respond(&no_store(
        Response::text(503, format!("{refusal}\n")).header("Retry-After", "10"),
    ));
    None
}

/// `/stream.mp3`: the ring's chunks, written as they come, until the client
/// goes, stalls past the write timeout, or the stream closes.
fn run_mp3(conn: Conn, mut recheck: Recheck) -> io::Result<()> {
    let ctx = Arc::clone(&recheck.ctx);
    let Some((mut conn, mut listener)) = open_or_503(conn, &ctx, Format::Mp3) else {
        return Ok(());
    };
    conn.write_head(
        200,
        &[
            ("Content-Type", "audio/mpeg"),
            ("Cache-Control", "no-store"),
            ("icy-name", "astar-server"),
        ],
    )?;
    if conn.head_only() {
        return Ok(());
    }
    let mut idle_since = Instant::now();
    loop {
        if ctx.stop.load(Ordering::Relaxed) || !recheck.allowed() {
            return Ok(());
        }
        match listener.pull(Duration::from_millis(250)) {
            Pull::Chunk(c) => {
                conn.write_all(&c)?;
                conn.flush()?;
                idle_since = Instant::now();
            }
            Pull::Idle if idle_since.elapsed() >= WRITE_STALL => return Ok(()),
            Pull::Idle => {}
            Pull::Closed => return Ok(()),
        }
    }
}

/// `/stream.ws`: `400` unless the request is an upgrade; then, inside the
/// stream slot, the hub's listener (or its `503`), the `101`, a text
/// header, and one Opus packet per binary message.
///
/// A `Reply::Stream` that upgrades with `ws::upgrade` rather than a
/// `Reply::WebSocket`, which sends its `101` before the handler could ask
/// the hub: a full hub must still be a `503`, and a client at its share a
/// `429` before the hub is asked.
#[cfg(feature = "stream")]
fn websocket(recheck: Recheck, accept: Option<String>) -> Reply {
    let Some(accept) = accept else {
        return Response::text(400, "WebSocket upgrade expected\n").into();
    };
    Reply::Stream {
        class: StreamClass::PerClientOnly,
        run: Box::new(move |conn| {
            let _ = run_websocket(conn, recheck, &accept);
        }),
    }
}

/// Inbound frames are never read: a closed page shows up as a write error,
/// and nothing the client could send changes what it receives.
#[cfg(feature = "stream")]
fn run_websocket(conn: Conn, mut recheck: Recheck, accept: &str) -> io::Result<()> {
    let ctx = Arc::clone(&recheck.ctx);
    let Some((conn, mut listener)) = open_or_503(conn, &ctx, Format::Opus) else {
        return Ok(());
    };
    let mut ws = astar_http::ws::upgrade(conn, accept, None)?;
    let rate = ctx
        .hub
        .as_ref()
        .and_then(|h| h.sample_rate())
        .unwrap_or(8000);
    let header = serde_json::json!({ "codec": "opus", "rate": rate, "frame_ms": 20 });
    let to_io = |e: tungstenite::Error| io::Error::other(e.to_string());
    ws.send(tungstenite::Message::text(header.to_string()))
        .map_err(to_io)?;
    let mut idle_since = Instant::now();
    loop {
        if ctx.stop.load(Ordering::Relaxed) || !recheck.allowed() {
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }
        match listener.pull(Duration::from_millis(250)) {
            Pull::Chunk(c) => {
                ws.send(tungstenite::Message::binary(c.to_vec()))
                    .map_err(to_io)?;
                idle_since = Instant::now();
            }
            Pull::Idle if idle_since.elapsed() >= WRITE_STALL => return Ok(()),
            Pull::Idle => {}
            Pull::Closed => {
                let _ = ws.close(None);
                let _ = ws.flush();
                return Ok(());
            }
        }
    }
}

#[cfg(not(feature = "stream"))]
fn websocket(_: Recheck, _: Option<String>) -> Reply {
    Response::text(503, "Live audio isn't built into this server\n").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SecretProvider;

    fn test_ctrl() -> Arc<NodeController> {
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(NodeController::new(station, SecretProvider::new()))
    }

    struct FakeHub {
        state: StreamAvailability,
        mp3: bool,
    }

    impl StreamSource for FakeHub {
        fn open(&self, _f: Format) -> Result<Box<dyn Listener>, HubRefusal> {
            Err(HubRefusal::Full)
        }
        fn levels(&self) -> Option<Levels> {
            None
        }
        fn availability(&self) -> StreamAvailability {
            self.state
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

    fn test_ctx(hosts: &[&str]) -> PublicCtx {
        PublicCtx {
            ctrl: test_ctrl(),
            hub: Some(Arc::new(FakeHub {
                state: StreamAvailability::Live,
                mp3: true,
            })),
            activity: Arc::new(Mutex::new(Activity::new(Instant::now()))),
            hostnames: hosts.iter().map(|h| (*h).to_string()).collect(),
            tls: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            anonymous_status: true,
            guard: crate::public::guard::Guard::default(),
        }
    }

    const H: Option<&str> = Some("108-175-15-153.sslip.io");

    fn code(r: &Route) -> Option<u16> {
        match r {
            Route::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    #[test]
    fn only_allowlisted_get_routes_exist() {
        let ctx = test_ctx(&["108-175-15-153.sslip.io"]);
        for p in PUBLIC_ROUTES {
            assert_ne!(code(&route(&ctx, "GET", p, H)), Some(404), "{p}");
            assert_ne!(code(&route(&ctx, "HEAD", p, H)), Some(404), "{p}");
        }
        for p in [
            "/status",
            "/events",
            "/key",
            "/link",
            "/unlink",
            "/secrets",
            "/settings",
            "/settings.js",
            "/settings/state",
            "/settings/credential",
            "/discord",
            "/discord/admin",
            "/m17",
            "/bridge",
            "/restart",
            "/index.html",
            "/app.js",
            "/../etc/passwd",
            "//admin",
            "/admin/",
            "/.well-known/acme-challenge/x",
        ] {
            assert_eq!(code(&route(&ctx, "GET", p, H)), Some(404), "{p}");
            assert_eq!(code(&route(&ctx, "POST", p, H)), Some(404), "{p}");
        }
        for m in ["POST", "PUT", "DELETE", "OPTIONS", "PATCH"] {
            assert_eq!(code(&route(&ctx, m, "/", H)), Some(405), "{m}");
            assert_eq!(code(&route(&ctx, m, "/public/status", H)), Some(405), "{m}");
        }
        assert_eq!(code(&route(&ctx, "POST", "/key", H)), Some(404));
        assert!(matches!(
            route(&ctx, "GET", "/?x=1", H),
            Route::Static { .. }
        ));
    }

    #[test]
    fn host_allowlist_normalises_and_refuses() {
        let ctx = test_ctx(&["108-175-15-153.sslip.io"]);
        for ok in [
            "108-175-15-153.sslip.io",
            "108-175-15-153.SSLIP.io:443",
            "108-175-15-153.sslip.io.",
            "108-175-15-153.SSLIP.io:443.",
            "108-175-15-153.sslip.io.:8443",
        ] {
            assert_ne!(code(&route(&ctx, "GET", "/", Some(ok))), Some(421), "{ok}");
        }
        for bad in [
            None,
            Some("108.175.15.153"),
            Some("108.175.15.153:443"),
            Some("[::1]:443"),
            Some("evil.example"),
            Some("localhost"),
            Some("x108-175-15-153.sslip.io"),
            Some(""),
        ] {
            assert_eq!(code(&route(&ctx, "GET", "/", bad)), Some(421), "{bad:?}");
            assert_eq!(code(&route(&ctx, "GET", "/key", bad)), Some(421), "{bad:?}");
        }
    }

    #[test]
    fn stream_refusals_have_reasons() {
        let mut ctx = test_ctx(&["a.example.net"]);
        let h = Some("a.example.net");
        ctx.hub = None;
        for p in ["/stream.mp3", "/stream.ws"] {
            assert_eq!(
                route(&ctx, "GET", p, h),
                Route::Status {
                    code: 503,
                    text: STREAM_OFF_TEXT
                }
            );
        }
        ctx.hub = Some(Arc::new(FakeHub {
            state: StreamAvailability::Unavailable,
            mp3: true,
        }));
        for p in ["/stream.mp3", "/stream.ws"] {
            assert_eq!(
                route(&ctx, "GET", p, h),
                Route::Status {
                    code: 503,
                    text: "No live audio in this mode"
                }
            );
        }
        ctx.hub = Some(Arc::new(FakeHub {
            state: StreamAvailability::Live,
            mp3: false,
        }));
        assert_eq!(
            route(&ctx, "GET", "/stream.mp3", h),
            Route::Status {
                code: 503,
                text: "MP3 isn't built into this server"
            }
        );
        assert_eq!(route(&ctx, "GET", "/stream.ws", h), Route::WebSocket);
        assert_eq!(HubRefusal::Full.text(), "Full, try again shortly");
    }

    #[test]
    fn public_status_route_is_the_projection_plus_secure_and_rate() {
        let ctx = test_ctx(&["a.example.net"]);
        let Route::Json(body) = route(&ctx, "GET", "/public/status", Some("a.example.net")) else {
            panic!("json expected");
        };
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["secure"], false);
        assert_eq!(v["rate"], 16_000);
        assert_eq!(v["stream"], "live");
        assert_eq!(v["mode"], "handset");
        for bad in ["secret", "password", "token", "127.0.0.1", "registered"] {
            assert!(!body.contains(bad), "{bad}: {body}");
        }
    }

    #[test]
    fn port80_serves_challenge_else_redirects() {
        let ch: ChallengeMap = Arc::new(Mutex::new(HashMap::new()));
        ch.lock()
            .unwrap()
            .insert("tok123".into(), "tok123.thumb".into());
        let hosts = vec!["a.example.net".to_string()];
        assert_eq!(
            route_http80(
                &ch,
                &hosts,
                true,
                "/.well-known/acme-challenge/tok123",
                Some("whatever")
            ),
            Http80::Challenge("tok123.thumb".into())
        );
        assert!(matches!(
            route_http80(
                &ch,
                &hosts,
                true,
                "/.well-known/acme-challenge/nope",
                Some("a.example.net")
            ),
            Http80::Status { code: 404, .. }
        ));
        assert_eq!(
            route_http80(&ch, &hosts, true, "/stream.mp3?x", Some("A.example.net:80")),
            Http80::Redirect("https://a.example.net/stream.mp3".into())
        );
        assert_eq!(
            route_http80(&ch, &hosts, false, "/", Some("a.example.net")),
            Http80::Public
        );
        assert!(matches!(
            route_http80(&ch, &hosts, true, "/", Some("evil.example")),
            Http80::Status { code: 421, .. }
        ));
    }

    #[test]
    fn the_pages_carry_no_inline_script_or_style_and_name_no_other_origin() {
        // The CSP forbids both; this keeps the files honest before a browser
        // has to refuse them.
        for (name, page) in [
            ("listener.html", LISTENER_HTML),
            ("admin", ADMIN_PLACEHOLDER_HTML),
        ] {
            assert!(!page.contains("<style"), "{name}: inline <style>");
            assert!(!page.contains(" style="), "{name}: a style attribute");
            let handler = page.match_indices(" on").any(|(i, _)| {
                let rest = &page[i + 3..];
                let word = rest.bytes().take_while(u8::is_ascii_lowercase).count();
                word > 0 && rest[word..].starts_with('=')
            });
            assert!(!handler, "{name}: an inline handler");
            for tag in page.split("<script").skip(1) {
                assert!(
                    tag.trim_start().starts_with("src=\"/"),
                    "{name}: inline script"
                );
            }
            for attr in ["src=\"http", "href=\"http", "src=\"//", "href=\"//"] {
                assert!(!page.contains(attr), "{name}: loads from another origin");
            }
        }
        for (name, js) in [
            ("listener.js", LISTENER_JS),
            ("worklet", LISTENER_WORKLET_JS),
        ] {
            assert!(!js.contains("innerHTML") && !js.contains("eval("), "{name}");
        }
        assert!(LISTENER_HTML.contains("id=\"hero\"") && LISTENER_HTML.contains("href=\"/admin\""));
        assert!(LISTENER_HTML.contains("aria-live=\"polite\""));
        assert!(ADMIN_PLACEHOLDER_HTML.contains("ssh -L 8730:127.0.0.1:8730"));
    }

    #[test]
    fn levels_are_rounded_to_a_tenth() {
        let s = levels_json(&Levels {
            dbfs: -23.149,
            spectrum: vec![-80.0, -12.345],
            rate: 8000,
        });
        assert_eq!(s, r#"{"dbfs":-23.1,"rate":8000,"spectrum":[-80.0,-12.3]}"#);
    }

    // ---- sign-in on the public bind (feature `auth`) ------------------------

    /// Read and write, as one object, for a plain or a TLS client.
    trait Io: std::io::Read + Write + Send {}
    impl<T: std::io::Read + Write + Send> Io for T {}

    const TEST_HOST: &str = "node.example.test";

    /// The public router behind a real astar-http server on 127.0.0.1, with
    /// a self-signed certificate for [`TEST_HOST`]: HTTPS and plain HTTP on
    /// the one port, as the public bind serves them.
    struct Wire {
        server: astar_http::Server,
        client: Arc<rustls::ClientConfig>,
    }

    fn wire(ctx: &Arc<PublicCtx>) -> Wire {
        use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let kp = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![TEST_HOST.to_string()])
            .unwrap()
            .self_signed(&kp)
            .unwrap();
        let der = CertificateDer::from(cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(kp.serialize_der()));
        let signing = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
        let tls = astar_http::TlsConfig::new();
        tls.install(rustls::sign::CertifiedKey::new(vec![der.clone()], signing));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        // As Listeners::start builds it: `tls` is set apart, since the
        // field exists only where astar-http's `tls` feature is on.
        let mut options = astar_http::ServerOptions {
            name: "public-test",
            csp: Some(PUBLIC_CSP),
            streams: Some(Arc::clone(&ctx.guard.streams)),
            ..astar_http::ServerOptions::default()
        };
        options.tls = Some(tls);
        let c = Arc::clone(ctx);
        let server = astar_http::Server::bind_with(
            "127.0.0.1:0",
            ctx.guard.limits.http(false),
            options,
            move |req| handle(&c, req, &Listening::Public),
        )
        .unwrap();
        Wire {
            server,
            client: Arc::new(client),
        }
    }

    impl Wire {
        /// A connection to the server, over TLS or not.
        fn connect(&self, tls: bool) -> Box<dyn Io> {
            let tcp = std::net::TcpStream::connect(self.server.local_addr()).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            if !tls {
                return Box::new(tcp);
            }
            let name = rustls_pki_types::ServerName::try_from(TEST_HOST).unwrap();
            let conn = rustls::ClientConnection::new(Arc::clone(&self.client), name).unwrap();
            Box::new(rustls::StreamOwned::new(conn, tcp))
        }
    }

    /// One raw request over the wire, over TLS or not: everything the
    /// server sent before it closed.
    fn serve_raw(w: &Wire, raw: &str, tls: bool) -> String {
        let mut s = w.connect(tls);
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        // Read to the close; a TLS close without close_notify is an error
        // after the last byte, not a loss of it.
        while let Ok(n) = s.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[cfg(feature = "auth")]
    struct SignInRig {
        ctx: Arc<PublicCtx>,
        svc: Arc<crate::auth::AuthService>,
        wire: Wire,
        _dir: tempfile::TempDir,
    }

    #[cfg(feature = "auth")]
    fn sign_in_rig(
        tweak: impl FnOnce(&mut crate::auth::AuthSettings, &mut PublicCtx),
    ) -> SignInRig {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = crate::auth::AuthSettings::defaults(dir.path());
        let mut ctx = test_ctx(&["node.example.test"]);
        settings.public_hostnames = vec!["node.example.test".into()];
        tweak(&mut settings, &mut ctx);
        let svc = Arc::new(crate::auth::AuthService::open(settings));
        ctx.ctrl.set_auth(Arc::clone(&svc));
        let ctx = Arc::new(ctx);
        SignInRig {
            wire: wire(&ctx),
            ctx,
            svc,
            _dir: dir,
        }
    }

    #[cfg(feature = "auth")]
    fn get(w: &Wire, path: &str, cookie: Option<&str>) -> String {
        let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
        serve_raw(
            w,
            &format!("GET {path} HTTP/1.1\r\nHost: node.example.test\r\n{cookie}\r\n"),
            true,
        )
    }

    #[cfg(feature = "auth")]
    fn post(w: &Wire, path: &str, cookie: Option<&str>, body: &str) -> String {
        let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
        serve_raw(
            w,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: node.example.test\r\nOrigin: https://node.example.test\r\n\
                 Content-Type: application/json\r\n{cookie}Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            true,
        )
    }

    #[cfg(feature = "auth")]
    fn session(
        svc: &crate::auth::AuthService,
        name: &str,
        role: crate::auth::store::Role,
    ) -> String {
        format!(
            "{}={}",
            crate::auth::session::SESSION_COOKIE_TLS,
            svc.test_sign_in(name, role)
        )
    }

    #[cfg(feature = "auth")]
    #[test]
    fn every_admin_route_under_admin_needs_an_admin_session() {
        use crate::access::{ADMIN_ROUTES, RouteClass, route_class};
        let rig = sign_in_rig(|_, _| {});
        let viewer = session(&rig.svc, "V1EW", crate::auth::store::Role::Viewer);
        for &(method, p) in ADMIN_ROUTES {
            let path = format!("/admin{p}");
            let public = route_class(method, p) == Some(RouteClass::Public);
            let anon = if method == "GET" {
                get(&rig.wire, &path, None)
            } else {
                post(&rig.wire, &path, None, "{}")
            };
            if public {
                assert!(anon.starts_with("HTTP/1.1 2"), "{method} {path}: {anon}");
                continue;
            }
            assert!(
                anon.starts_with("HTTP/1.1 303") || anon.starts_with("HTTP/1.1 401"),
                "anonymous {method} {path}: {anon}"
            );
            let as_viewer = if method == "GET" {
                get(&rig.wire, &path, Some(&viewer))
            } else {
                post(&rig.wire, &path, Some(&viewer), "{}")
            };
            assert!(
                as_viewer.starts_with("HTTP/1.1 403"),
                "viewer {method} {path}: {as_viewer}"
            );
        }
        assert!(
            !rig.ctx.ctrl.should_stop(),
            "nothing reached the controller"
        );
    }

    #[cfg(feature = "auth")]
    #[test]
    fn unprefixed_admin_routes_stay_404_with_sign_in_on() {
        let rig = sign_in_rig(|_, _| {});
        let admin = session(&rig.svc, "A1DM", crate::auth::store::Role::Admin);
        for &(method, p) in crate::access::ADMIN_ROUTES {
            if p == "/" || p == "/style.css" {
                continue; // the listener's own page and stylesheet
            }
            let r = if method == "GET" {
                get(&rig.wire, p, Some(&admin))
            } else {
                post(&rig.wire, p, Some(&admin), "{}")
            };
            assert!(r.starts_with("HTTP/1.1 404"), "{method} {p}: {r}");
        }
        let r = post(&rig.wire, "/auth/cli/invite", None, "{}");
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }

    #[cfg(feature = "auth")]
    #[test]
    fn a_signed_in_admin_reaches_the_admin_router() {
        let rig = sign_in_rig(|_, _| {});
        let admin = session(&rig.svc, "A1DM", crate::auth::store::Role::Admin);
        let r = get(&rig.wire, "/admin/status", Some(&admin));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("listening"), "{r}");
        let r = get(&rig.wire, "/admin/", Some(&admin));
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("links-body"),
            "{r}"
        );
        // A write passes the Origin rule with the public Host, and its body
        // is read: /dial with no call configured answers from the controller.
        let r = post(&rig.wire, "/admin/dial", Some(&admin), "{\"node\":\"x\"}");
        assert!(
            !r.starts_with("HTTP/1.1 403") && !r.starts_with("HTTP/1.1 415"),
            "{r}"
        );
        // A foreign Origin is still refused.
        let r = serve_raw(
            &rig.wire,
            &format!(
                "POST /admin/unkey HTTP/1.1\r\nHost: node.example.test\r\nOrigin: https://evil.example\r\n\
                 Content-Type: application/json\r\nCookie: {admin}\r\nContent-Length: 0\r\n\r\n"
            ),
            true,
        );
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        let r = get(&rig.wire, "/admin", None);
        assert!(
            r.starts_with("HTTP/1.1 303") && r.contains("Location: /admin/"),
            "{r}"
        );
    }

    #[cfg(feature = "auth")]
    #[test]
    fn bodies_are_capped_and_chunked_ones_refused() {
        let rig = sign_in_rig(|_, _| {});
        let r = serve_raw(
            &rig.wire,
            "POST /auth/signout HTTP/1.1\r\nHost: node.example.test\r\nContent-Length: 999999\r\n\r\n",
            true,
        );
        assert!(r.starts_with("HTTP/1.1 413"), "{r}");
        let r = serve_raw(
            &rig.wire,
            "POST /auth/signout HTTP/1.1\r\nHost: node.example.test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            true,
        );
        assert!(r.starts_with("HTTP/1.1 411"), "{r}");
    }

    #[cfg(feature = "auth")]
    #[test]
    fn a_private_listener_page_needs_a_session() {
        let rig = sign_in_rig(|_, ctx| ctx.anonymous_status = false);
        let r = get(&rig.wire, "/", None);
        assert!(
            r.starts_with("HTTP/1.1 303") && r.contains("Location: /admin/signin?next=/"),
            "{r}"
        );
        for p in ["/public/status", "/stream.mp3", "/public/events"] {
            let r = get(&rig.wire, p, None);
            assert!(r.starts_with("HTTP/1.1 401"), "{p}: {r}");
        }
        let r = get(&rig.wire, "/listener.js", None);
        assert!(r.starts_with("HTTP/1.1 200"), "assets stay open: {r}");
        let viewer = session(&rig.svc, "V1EW", crate::auth::store::Role::Viewer);
        let r = get(&rig.wire, "/public/status", Some(&viewer));
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    }

    #[test]
    fn hsts_is_sent_over_tls_only() {
        let ctx = wire(&Arc::new(test_ctx(&["node.example.test"])));
        let hsts = "\r\nStrict-Transport-Security: max-age=31536000\r\n";
        for raw in [
            "GET / HTTP/1.1\r\nHost: node.example.test\r\n\r\n",
            "GET /nope HTTP/1.1\r\nHost: node.example.test\r\n\r\n",
            "BREW /pot\r\n\r\n",
        ] {
            let tls = serve_raw(&ctx, raw, true);
            assert!(tls.contains(hsts), "{tls}");
            assert!(!tls.contains("includeSubDomains"), "{tls}");
            let plain = serve_raw(&ctx, raw, false);
            assert!(plain.starts_with("HTTP/1.1 "), "{plain}");
            assert!(!plain.contains("Strict-Transport-Security"), "{plain}");
        }
    }

    #[test]
    fn a_private_page_without_sign_in_fails_closed() {
        let mut ctx = test_ctx(&["node.example.test"]);
        ctx.anonymous_status = false;
        let head = RequestHead {
            method: "GET".into(),
            target: "/".into(),
            headers: vec![("Host".into(), "node.example.test".into())],
        };
        let r = route_request(
            &ctx,
            &Incoming {
                head: &head,
                body: b"",
                peer: None,
                tls: true,
            },
        );
        assert_eq!(code(&r), Some(503));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn sign_in_needs_https_except_on_localhost() {
        let rig = sign_in_rig(|s, ctx| {
            s.public_hostnames.push("localhost".into());
            ctx.hostnames.push("localhost".into());
        });
        let plain = serve_raw(
            &rig.wire,
            "GET /auth/status HTTP/1.1\r\nHost: node.example.test\r\n\r\n",
            false,
        );
        assert!(
            plain.contains("\"signin\":false") && plain.contains("https_required"),
            "{plain}"
        );
        let tls = get(&rig.wire, "/auth/status", None);
        assert!(tls.contains("\"signin\":true"), "{tls}");
        let local = serve_raw(
            &rig.wire,
            "GET /auth/status HTTP/1.1\r\nHost: localhost:8443\r\n\r\n",
            false,
        );
        assert!(local.contains("\"signin\":true"), "{local}");
    }

    /// Open `path` as a stream with `cookie` over TLS, call `revoke` once it
    /// is flowing, and say whether the server then ended it by itself.
    #[cfg(feature = "auth")]
    fn stream_ends_after(rig: &SignInRig, path: &str, cookie: &str, revoke: impl FnOnce()) -> bool {
        let output = Arc::new(Mutex::new(Vec::new()));
        let raw =
            format!("GET {path} HTTP/1.1\r\nHost: node.example.test\r\nCookie: {cookie}\r\n\r\n");
        let wait = |what: &str, f: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !f() {
                if Instant::now() >= deadline {
                    rig.ctx.stop.store(true, Ordering::Relaxed);
                    panic!("timed out waiting for {what}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let mut s = rig.wire.connect(true);
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        std::thread::scope(|sc| {
            let out = Arc::clone(&output);
            // Reads until the server closes: the thread finishing is the
            // stream ending. A read that only timed out is not an end.
            let h = sc.spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => return,
                    }
                }
            });
            wait("the stream to start", &|| {
                output.lock().unwrap().starts_with(b"HTTP/1.1 200")
            });
            // Several re-checks pass while the session stands.
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !h.is_finished(),
                "{}",
                String::from_utf8_lossy(&output.lock().unwrap())
            );
            revoke();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let ended = h.is_finished();
            rig.ctx.stop.store(true, Ordering::Relaxed);
            ended
        })
    }

    #[cfg(feature = "auth")]
    fn quick_recheck(ctx: &mut PublicCtx) {
        ctx.guard = crate::public::guard::Guard::new(crate::public::guard::Limits {
            session_recheck: Duration::from_millis(50),
            ..crate::public::guard::Limits::default()
        });
    }

    #[cfg(feature = "auth")]
    fn user_id(svc: &crate::auth::AuthService, name: &str) -> uuid::Uuid {
        svc.read(|d| d.users.iter().find(|u| u.name == name).unwrap().id)
    }

    #[cfg(feature = "auth")]
    #[test]
    fn the_admin_event_stream_ends_at_sign_out() {
        let rig = sign_in_rig(|_, ctx| quick_recheck(ctx));
        let admin = session(&rig.svc, "A1DM", crate::auth::store::Role::Admin);
        assert!(stream_ends_after(&rig, "/admin/events", &admin, || {
            let r = post(&rig.wire, "/auth/signout", Some(&admin), "{}");
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        }));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn the_admin_event_stream_ends_at_demotion() {
        let rig = sign_in_rig(|_, ctx| quick_recheck(ctx));
        let admin = session(&rig.svc, "A1DM", crate::auth::store::Role::Admin);
        let id = user_id(&rig.svc, "A1DM");
        assert!(stream_ends_after(&rig, "/admin/events", &admin, || {
            rig.svc
                .mutate(|d| {
                    d.users.iter_mut().find(|u| u.id == id).unwrap().role =
                        crate::auth::store::Role::Viewer;
                    Ok(())
                })
                .unwrap();
        }));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn a_private_event_feed_ends_when_its_user_is_removed() {
        let rig = sign_in_rig(|_, ctx| {
            ctx.anonymous_status = false;
            quick_recheck(ctx);
        });
        let viewer = session(&rig.svc, "V1EW", crate::auth::store::Role::Viewer);
        let id = user_id(&rig.svc, "V1EW");
        assert!(stream_ends_after(&rig, "/public/events", &viewer, || {
            rig.svc
                .mutate(|d| {
                    crate::auth::service::remove_user(d, id);
                    Ok(())
                })
                .unwrap();
        }));
    }

    #[cfg(feature = "auth")]
    #[test]
    fn admin_on_public_off_keeps_the_placeholder() {
        let rig = sign_in_rig(|s, _| s.admin_on_public = false);
        let r = get(&rig.wire, "/admin", None);
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let r = get(&rig.wire, "/admin/status", None);
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let r = get(&rig.wire, "/auth/me", None);
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    }
}
