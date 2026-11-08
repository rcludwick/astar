// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Socket-independent HTTP router for the astar-server control channel (iax-35b1).
//!
//! `handle_request` maps `(method, path, body)` to an [`HttpResponse`] value by
//! dispatching to the [`NodeController`]; it has no dependency on any socket or
//! HTTP library.  The server layer (Task 8) owns the socket and calls this
//! function.
//!
//! # Cross-origin requests
//!
//! Every request on this port, whatever its method or path, first passes
//! the **`Host` allow-list**: `Host` must be `localhost`, a loopback
//! literal, or the address the control socket is actually bound to
//! ([`host_is_this_node`]). That is the check that stops **DNS rebinding**,
//! where a page served from `http://evil.com:8730/` has its name re-pointed
//! at `127.0.0.1`, and its `Origin` and `Host` agree with each other
//! perfectly and are both wrong. It runs at the top of
//! [`handle_request_with_headers`], before the path is looked at, and again
//! in `server::dispatch` before the `GET /events` stream is opened, because
//! SSE is answered before this router is ever called. It breaks no `curl`
//! client — nothing that posts to this port sends any other `Host` — and
//! it is iax-ctlcsrf step 1.
//!
//! `POST /discord/admin` adds two more checks through [`same_origin_json`],
//! which is where the reasoning is written down in full:
//!
//! 1. **`Origin`, when present, must match `Host`.** This stops a plain
//!    **cross-origin `fetch`**. `curl` sends no `Origin` and is unaffected.
//! 2. **`Content-Type: application/json`.** This stops the **"simple"
//!    cross-origin requests** — a `<form>` post, an `<img>`, a `fetch` with
//!    a simple content type — none of which can set that header, and whose
//!    preflight gets no `Access-Control-Allow-*` back.
//!
//! **Every other mutating route answers to [`origin_rule`]** (iax-ctlcsrf
//! step 2): an `Origin`, when present, must match `Host`, and a request a
//! browser sent (`Origin` or `Sec-Fetch-Site` present) must be JSON. That
//! closes the bare cross-origin `<form method=POST>` that used to reach
//! `POST /key` from any page the operator had open, and leaves `curl`,
//! which sends neither header, exactly as it was. The routes behind
//! `same_origin_json` additionally require JSON of every caller.
//!
//! # Request bodies
//!
//! No route accepts more than [`MAX_BODY_BYTES`]. `server::dispatch` refuses
//! a `Content-Length` over the cap before reading a byte of body, and
//! astar-http reads exactly the announced length and never more; a chunked
//! body is refused with 411 unread. The router answers 413 to a body over
//! the cap before a route is looked up.
//!
//! # Secret safety
//!
//! `ProvideSecret` accepts credentials from the request body and forwards them
//! to the [`SecretProvider`] through `NodeController::execute`.  The reply is
//! always `{"ok":true}` — the secret is **never** echoed in any response.

use serde::Deserialize;
use serde_json::Value;

use crate::{
    command::{EchoLinkAction, LinkAction, M17Action, NodeCommand, NodeReply},
    controller::NodeController,
    secrets::NODE_CREDENTIAL_KEY,
    settings::{SettingsPatch, Tier, WriteContext, tier},
    settings_store::{CredentialKeyError, CredentialName, SaveError, SaveOutcome, StateFacts},
};

// ---- status page assets (iax-24e2) ------------------------------------------

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");
const DISCORD_HTML: &str = include_str!("assets/discord.html");
const DISCORD_JS: &str = include_str!("assets/discord.js");
const SETTINGS_HTML: &str = include_str!("assets/settings.html");
const SETTINGS_JS: &str = include_str!("assets/settings.js");
const AUTH_JS: &str = include_str!("assets/auth.js");
const AUTH_CSS: &str = include_str!("assets/auth.css");
const RECORDINGS_HTML: &str = include_str!("assets/recordings.html");
const RECORDINGS_JS: &str = include_str!("assets/recordings.js");

/// The policy every embedded page is served under.
///
/// The Discord admin page renders display names that their owners chose, so
/// the one thing that must never work is a string from the wire becoming
/// script. The page itself only ever builds DOM nodes, and this header is
/// the second lock on the same door: no inline script, no inline style, no
/// external origin, and no framing. It is why neither page may grow a
/// `<script>` block or a `style=""` attribute.
pub(crate) const CSP: &str = "default-src 'self'; style-src 'self'; script-src 'self'; \
                   base-uri 'none'; frame-ancestors 'none'";

/// The largest request body any route accepts: 64 KiB.
///
/// The biggest legitimate body on this port is a settings patch of a few
/// hundred bytes, so the cap is generous by two orders of magnitude; it
/// bounds what this crate will hold of a request body. Checked three times,
/// on purpose: `server::dispatch` answers 413 to a `Content-Length` over it
/// before reading a byte of body; astar-http's own body cap
/// ([`astar_http::Limits::max_body_bytes`], the same 64 KiB) refuses one it
/// would otherwise read; and the router answers 413 to a body over it, so
/// every caller of the router gets the rule with or without a socket.
/// Nothing reads or drains the unread remainder of a refused body beyond
/// astar-http's bounded lingering close.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// A 200 response for an embedded static asset.
fn asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    HttpResponse {
        status: 200,
        content_type,
        body: body.as_bytes().to_vec(),
        headers: vec![
            ("Content-Security-Policy", CSP.to_string()),
            // The page's own script is the only script that may run on this
            // origin (CSP), and this is what stops a browser deciding some
            // other response is one anyway.
            ("X-Content-Type-Options", "nosniff".to_string()),
        ],
    }
}

/// [`asset`] for the sign-in pages, which live outside this router.
#[must_use]
pub fn static_asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    asset(content_type, body)
}

/// What the router needs to know about a request beyond its method, path
/// and body.
///
/// Deliberately named fields rather than a map: the router's business here
/// is the port-wide `Host` allow-list and the JSON-only guard on the routes
/// that opt into it, and a map would invite routes to start reading
/// whatever they like.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequestHeaders<'a> {
    /// `Origin`, when the caller sent one. Browsers always do on a POST.
    pub origin: Option<&'a str>,
    /// `Host` — what the caller believes it is talking to.
    pub host: Option<&'a str>,
    /// `Content-Type`, media type and parameters as received.
    pub content_type: Option<&'a str>,
    /// `Sec-Fetch-Site`, when the caller sent one. Every current browser
    /// does, on every request, and nothing else does: it is one of the two
    /// signs ([`origin_rule`]) that a request came from a page.
    pub sec_fetch_site: Option<&'a str>,
    /// The address the control socket is actually bound to, when the caller
    /// knows it.
    ///
    /// Not a header, and it sits here anyway: it is the other half of the
    /// one decision this struct exists for — a `Host` that is neither
    /// loopback nor the address this node is listening on is a rebinding
    /// attempt (see [`same_origin_json`]). A sixth parameter threaded
    /// through `handle_request_with_headers` would touch every call site to
    /// say the same thing. `None` means "loopback only", which is what a
    /// caller that cannot say should get.
    pub bound_to: Option<std::net::SocketAddr>,
    /// The public router already checked `Host` against `[web]
    /// public_hostnames` and is forwarding a signed-in admin's request
    /// under `/admin/`. Set by nothing else: on the control port the `Host`
    /// allow-list below is the check.
    pub host_verified: bool,
    /// The socket peer is a loopback address and no proxy header says it
    /// relayed someone else (`access::is_trusted_loopback_peer`). Not a
    /// header either: `Host` is whatever the caller typed, so "this request
    /// came from the node itself" is read from the socket, never from it.
    pub loopback_peer: bool,
}

/// A fully-formed HTTP response: status, content-type, body bytes, and any
/// extra headers the route asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Extra headers, in order. `Content-Type` is not among them — it has
    /// its own field, and the server layer writes it separately.
    pub headers: Vec<(&'static str, String)>,
}

impl HttpResponse {
    /// Serialise `value` to JSON and return with `status`.
    #[must_use]
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
            headers: Vec::new(),
        }
    }

    /// Add one response header.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }

    /// Convenience: `{"ok":true}` with status 200.
    #[must_use]
    pub fn ok_json() -> Self {
        Self::json(200, &serde_json::json!({ "ok": true }))
    }

    /// Convenience: `{"error":"<msg>"}` with the given status.
    #[must_use]
    pub fn error_json(status: u16, msg: &str) -> Self {
        Self::json(status, &serde_json::json!({ "error": msg }))
    }
}

// ---- per-route request bodies -----------------------------------------------

#[derive(Deserialize)]
struct DialRequest {
    node: String,
}

#[derive(Deserialize)]
struct DevicesRequest {
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

/// `POST /link` body (iax-d829.1): node-to-node link control. `action` is one
/// of `connect` / `monitor` / `disconnect`; `addr` optionally dials an explicit
/// loopback `ip:port` (harness / localhost), bypassing `AllStar` DNS. Any
/// other `addr` is a 400: see `controller::link_addr_is_loopback`.
#[derive(Deserialize)]
struct LinkRequest {
    action: LinkAction,
    node: String,
    #[serde(default)]
    addr: Option<String>,
}

/// `POST /m17` body: the node's receive-only M17 monitor.
///
/// `deny_unknown_fields` is load-bearing. There is no `addr` here, and a
/// body that carries one is a 400 rather than a silently ignored field, so
/// no caller can come to believe they chose where the node connects: a
/// reflector is only ever a name, resolved from the operator's `[m17]`
/// config. `module` is a string so a two-letter module gets this route's
/// sentence instead of serde's "invalid length" text.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct M17Request {
    action: M17Action,
    #[serde(default)]
    reflector: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    callsign: Option<String>,
}

/// `POST /echolink` body: the node's `EchoLink` leg.
///
/// `deny_unknown_fields` is load-bearing, as on [`M17Request`]. There is no
/// `addr` or `host` here, and a body that carries one is a 400 rather than
/// a silently ignored field, so no caller can come to believe they chose
/// where the node connects: a station is only ever a node number or a
/// callsign, found in the directory the node logged in to.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoLinkRequest {
    action: EchoLinkAction,
    #[serde(default)]
    target: Option<String>,
}

/// `ProvideSecret` body — deserialized but the fields never echo'd.
#[derive(Deserialize)]
struct ProvideSecretRequest {
    username: String,
    secret: String,
}

/// `POST /bridge` body (iax-647d): re-wire the conference bridge live. All
/// fields optional — an omitted field keeps its current value.
#[derive(Deserialize)]
struct BridgeRequest {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    mix_minus: Option<bool>,
    #[serde(default)]
    include_local_radio: Option<bool>,
}

/// `POST /discord` body: `{"action": "join" | "leave"}`. An unknown action
/// is a 400 — notably `"arm"`, which does not exist.
#[derive(serde::Deserialize)]
struct DiscordRequest {
    action: crate::command::DiscordAction,
}

/// `POST /discord/admin` body: one action from the node's own admin page.
///
/// `user_id` is a **string**, and a JSON number is a deserialization failure
/// rather than a coercion: a Discord snowflake is 64 bits and a JSON number
/// is a double, so `444…` sent as a number is already a different user by
/// the time it arrives.
#[derive(Deserialize)]
struct DiscordAdminRequest {
    action: String,
    #[serde(default)]
    user_id: Option<String>,
}

/// `POST /settings/credential` body. `value` is the credential itself, so
/// `Debug` is written by hand: a derived one would print it into any
/// `{req:?}` an error path ever formats. The secret-free rule on this port
/// is kept by giving the value no way out, not by promising not to look.
#[derive(Deserialize)]
struct CredentialRequest {
    name: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    forget: bool,
    #[serde(default)]
    migrate: bool,
}

// `value` is deliberately not read by `Debug`: the field is the credential.
// Same shape as `SecretsCfg` (config.rs:486-491); the pedantic
// `missing_fields_in_debug` lint the workspace enables would otherwise fire.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for CredentialRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialRequest")
            .field("name", &self.name)
            .field("value", &format_args!("<redacted>"))
            .field("forget", &self.forget)
            .field("migrate", &self.migrate)
            .finish()
    }
}

/// `POST /settings/restart` body: `{}` asks, `{"disconnect":true}` insists
/// (the page's confirmed second click after a 409 with `busy: true`).
#[derive(Deserialize, Default)]
struct RestartRequest {
    #[serde(default)]
    disconnect: bool,
}

// ---- router -----------------------------------------------------------------

/// Route a request from the in-process caller: this crate's tests, and
/// anything that embeds the router without a socket.
///
/// Equivalent to [`handle_request_with_headers`] with a loopback `Host` and
/// nothing else — `host: Some("localhost")`, no `Origin`, no
/// `Content-Type`. The `Host` is supplied rather than left empty because
/// the allow-list runs on every route and a caller that is *in the
/// process* is, by construction, this node. A caller that can only say "no
/// headers" gets a 403 from the header-taking function, which is what a
/// request with no `Host` deserves.
///
/// `GET /events` (SSE) is **not** handled here — the server layer
/// intercepts it before calling this function. All other routes are covered.
///
/// # Panics
///
/// Never. All parse / execute errors are mapped to 4xx/5xx responses.
#[must_use]
pub fn handle_request(
    ctrl: &NodeController,
    method: &str,
    path: &str,
    body: &[u8],
) -> HttpResponse {
    handle_request_with_headers(
        ctrl,
        method,
        path,
        body,
        &RequestHeaders {
            host: Some("localhost"),
            loopback_peer: true,
            ..RequestHeaders::default()
        },
    )
}

/// Route a request to a response, with the headers the caller sent.
///
/// The `Host` allow-list runs first, before the path is even looked at: a
/// request that cannot show it is talking to this node gets the same 403
/// on every route, and nothing below that line ever sees it. The
/// header-less [`handle_request`] is this function with a loopback `Host`
/// and nothing else, so the two behave identically on every route except
/// the ones behind [`same_origin_json`] (`POST /discord/admin`, the
/// settings routes and `POST /m17`), whose cross-origin guard also needs a
/// `Content-Type` before it will act.
///
/// # Panics
///
/// Never. All parse / execute errors are mapped to 4xx/5xx responses.
#[must_use]
#[allow(clippy::too_many_lines)] // flat route match — splitting hurts readability
pub fn handle_request_with_headers(
    ctrl: &NodeController,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &RequestHeaders<'_>,
) -> HttpResponse {
    // Before the route. The guard is what makes the whole port this node's
    // rather than a rebinding page's, and any route reachable around it
    // would be the weakest route on the port.
    if !headers.host_verified && !host_is_this_node(headers.host, headers.bound_to) {
        return forbidden_cross_origin();
    }
    // Also before the route: a body nobody is going to parse is not worth
    // finding out which route it was for.
    if body.len() > MAX_BODY_BYTES {
        return payload_too_large();
    }
    // Strip query string (not used by any current route, but keeps the router
    // forward-compatible).
    let route = path.split('?').next().unwrap_or(path);
    // Before the route, for every method that can change something: a page
    // in the operator's browser must not be able to key the transmitter.
    if !matches!(method, "GET" | "HEAD")
        && let Err(refused) = origin_rule(headers, &format!("{method} {route}"))
    {
        return refused;
    }
    // Only what the route table lists is answered: a match arm with no line
    // in `access::ADMIN_ROUTES` is unreachable, so no route can ship
    // without a class.
    if !crate::access::is_admin_route(method, route) {
        return HttpResponse::error_json(404, "not found");
    }

    match (method, route) {
        // ---- status page (iax-24e2) — read-only embedded assets -------------
        ("GET", "/") => asset("text/html; charset=utf-8", INDEX_HTML),
        ("GET", "/app.js") => asset("application/javascript; charset=utf-8", APP_JS),
        ("GET", "/style.css") => asset("text/css; charset=utf-8", STYLE_CSS),
        // The user chip and the "Confirm it's you" sheet, shared with the
        // sign-in pages. Harmless without sign-in: /auth/me is then a 404
        // and the chip stays empty.
        ("GET", "/auth.js") => asset("application/javascript; charset=utf-8", AUTH_JS),
        ("GET", "/auth.css") => asset("text/css; charset=utf-8", AUTH_CSS),
        // Answered rather than 404'd: every page load asks for it, and a
        // 404 per load is noise in the log of a node whose log is what an
        // operator reads when something is wrong. 204 means "there is none"
        // without inventing an asset or reaching off this origin for one.
        ("GET", "/favicon.ico") => HttpResponse {
            status: 204,
            content_type: "text/plain",
            body: Vec::new(),
            headers: Vec::new(),
        },

        // ---- status ---------------------------------------------------------
        ("GET", "/status") => {
            let cmd = NodeCommand::Status;
            execute_and_render(ctrl, cmd)
        }

        // ---- call control ---------------------------------------------------
        ("POST", "/dial") => {
            let req: DialRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            execute_and_render(ctrl, NodeCommand::Dial { node: req.node })
        }
        ("POST", "/hangup") => execute_and_render(ctrl, NodeCommand::Hangup),
        ("POST", "/answer") => execute_and_render(ctrl, NodeCommand::Answer),
        ("POST", "/reject") => execute_and_render(ctrl, NodeCommand::Reject),

        // ---- PTT ------------------------------------------------------------
        ("POST", "/key") => execute_and_render(ctrl, NodeCommand::Key),
        ("POST", "/unkey") => execute_and_render(ctrl, NodeCommand::Unkey),

        // ---- inbound listener -----------------------------------------------
        ("POST", "/enable_inbound") => execute_and_render(ctrl, NodeCommand::EnableInbound),
        ("POST", "/disable_inbound") => execute_and_render(ctrl, NodeCommand::DisableInbound),

        // ---- registration ---------------------------------------------------
        ("POST", "/register") => execute_and_render(ctrl, NodeCommand::Register),
        ("POST", "/deregister") => execute_and_render(ctrl, NodeCommand::Deregister),

        // ---- device selection -----------------------------------------------
        ("POST", "/devices") => {
            let req: DevicesRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            execute_and_render(
                ctrl,
                NodeCommand::SetDevices {
                    input: req.input,
                    output: req.output,
                },
            )
        }

        // ---- secrets --------------------------------------------------------
        // Body carries credentials — the reply must NEVER echo them.
        ("POST", "/secrets") => {
            let req: ProvideSecretRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                // Not serde's sentence: its `invalid type` errors quote the
                // offending scalar, and here that can be the secret. The same
                // fixed reply as `POST /settings/credential`.
                Err(_) => {
                    return HttpResponse::error_json(
                        400,
                        "bad request: expected {\"username\": \"…\", \"secret\": \"…\"}",
                    );
                }
            };
            // Execute — if Ok, return a bare ok_json so the secret has no path
            // into the response even if NodeReply serialisation ever changes.
            match ctrl.execute(NodeCommand::ProvideSecret {
                username: req.username,
                secret: req.secret,
            }) {
                Ok(_) => HttpResponse::ok_json(),
                Err(e) => HttpResponse::error_json(500, &e.message),
            }
        }

        // ---- node-to-node link control (iax-d829.1) -------------------------
        ("POST", "/link") => {
            let req: LinkRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            if req
                .addr
                .as_deref()
                .is_some_and(|a| !crate::controller::link_addr_is_loopback(a))
            {
                return HttpResponse::error_json(400, crate::controller::LINK_ADDR_NOT_LOOPBACK);
            }
            execute_and_render(
                ctrl,
                NodeCommand::Link {
                    action: req.action,
                    node: req.node,
                    addr: req.addr,
                },
            )
        }

        // ---- M17 reflector monitor (receive-only) ---------------------------
        // Added after `POST /discord/admin`, so it carries the same-origin
        // JSON guard: a dial presents a callsign to someone else's reflector.
        ("POST", "/m17") => {
            if let Err(refused) = same_origin_json(headers, "POST /m17") {
                return refused;
            }
            m17_route(ctrl, body)
        }

        // ---- the node's EchoLink leg ----------------------------------------
        // Same guard as `POST /m17`: a dial puts this node's callsign on a
        // call to somebody else's station.
        ("POST", "/echolink") => {
            if let Err(refused) = same_origin_json(headers, "POST /echolink") {
                return refused;
            }
            let req: EchoLinkRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            execute_and_render(
                ctrl,
                NodeCommand::EchoLink {
                    action: req.action,
                    target: req.target,
                },
            )
        }

        // ---- conference bridge (iax-647d) -----------------------------------
        ("POST", "/bridge") => {
            let req: BridgeRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            execute_and_render(
                ctrl,
                NodeCommand::SetBridge {
                    mode: req.mode,
                    mix_minus: req.mix_minus,
                    include_local_radio: req.include_local_radio,
                },
            )
        }

        // ---- discord voice bridge -------------------------------------------
        ("POST", "/discord") => {
            let req: DiscordRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            execute_and_render(ctrl, NodeCommand::Discord { action: req.action })
        }

        // ---- discord admin page ---------------------------------------------
        ("GET", "/discord/admin") => asset("text/html; charset=utf-8", DISCORD_HTML),
        ("GET", "/discord/admin.js") => asset("application/javascript; charset=utf-8", DISCORD_JS),
        ("GET", "/discord/state") => discord_state(ctrl),
        ("POST", "/discord/admin") => discord_admin(ctrl, body, headers),

        // ---- the settings page ----------------------------------------------
        ("GET", "/settings") => asset("text/html; charset=utf-8", SETTINGS_HTML),
        ("GET", "/settings.js") => asset("application/javascript; charset=utf-8", SETTINGS_JS),
        ("GET", "/settings/state") => settings_state(ctrl),
        ("POST", "/settings") => settings_save(ctrl, body, headers),
        ("POST", "/settings/credential") => settings_credential(ctrl, body, headers),
        ("POST", "/settings/portal/test") => settings_portal_test(ctrl, body, headers),
        ("POST", "/settings/restart") => settings_restart(ctrl, body, headers),

        // ---- the recordings page --------------------------------------------
        ("GET", "/recordings") => asset("text/html; charset=utf-8", RECORDINGS_HTML),
        ("GET", "/recordings.js") => asset("application/javascript; charset=utf-8", RECORDINGS_JS),
        ("GET", "/recordings/state") => crate::recording::http::state(ctrl),
        ("POST", "/recordings/delete") => {
            if let Err(refused) = same_origin_json(headers, "POST /recordings/delete") {
                return refused;
            }
            crate::recording::http::delete(ctrl, body)
        }
        ("GET", "/packet-logs/state") => crate::recording::http::packet_log_state(ctrl),
        ("POST", "/packet-logs/delete") => {
            if let Err(refused) = same_origin_json(headers, "POST /packet-logs/delete") {
                return refused;
            }
            crate::recording::http::packet_log_delete(ctrl, body)
        }
        ("POST", "/packet-logs/capture") => {
            if let Err(refused) = same_origin_json(headers, "POST /packet-logs/capture") {
                return refused;
            }
            crate::recording::http::packet_capture(ctrl, body)
        }
        // Streamed by the server layer once the gate admits them; here only
        // the refusals (recording or the packet log off, an unknown name, a
        // bad `since`) can be answered, since this router holds a whole body
        // in memory.
        ("GET", "/recordings/file" | "/recordings.tar" | "/packet-logs/file") => {
            let query = path.split_once('?').map_or("", |(_, q)| q);
            match crate::recording::http::download_for(ctrl, method, route, query) {
                Some(Err(refused)) => refused,
                _ => HttpResponse::error_json(500, "downloads are streamed by the server"),
            }
        }

        // ---- lifecycle ------------------------------------------------------
        ("POST", "/shutdown") => execute_and_render(ctrl, NodeCommand::Shutdown),

        // ---- fallback -------------------------------------------------------
        _ => HttpResponse::error_json(404, "not found"),
    }
}

// ---- the M17 monitor --------------------------------------------------------

/// `POST /m17`: the request's shape, then [`NodeCommand::M17`].
///
/// The two 400s here are about shape, which only this layer can see: a
/// `monitor` with a field missing, and a `module` that is not one
/// character. Whether the node *will* monitor (enabled, known reflector,
/// module on that reflector, a Codec 2 decoder, nothing already up) is the
/// controller's refusal and comes back as the API's usual 500 with its
/// sentence. A `disconnect` needs nothing but its action, so it ignores the
/// other fields rather than refusing a harmless extra.
fn m17_route(ctrl: &NodeController, body: &[u8]) -> HttpResponse {
    let req: M17Request = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    let monitor = matches!(req.action, M17Action::Monitor);
    let cmd = match req.action {
        M17Action::Disconnect => NodeCommand::M17 {
            action: M17Action::Disconnect,
            reflector: None,
            module: None,
            callsign: None,
        },
        M17Action::Monitor => {
            let (Some(reflector), Some(module), Some(callsign)) =
                (req.reflector, req.module, req.callsign)
            else {
                return HttpResponse::error_json(
                    400,
                    "bad request: monitor needs reflector, module and callsign",
                );
            };
            let mut chars = module.chars();
            let (Some(module), None) = (chars.next(), chars.next()) else {
                return HttpResponse::error_json(400, "bad request: module is one letter, A to Z");
            };
            NodeCommand::M17 {
                action: M17Action::Monitor,
                reflector: Some(reflector),
                module: Some(module),
                callsign: Some(callsign),
            }
        }
    };
    let r = execute_and_render(ctrl, cmd);
    if monitor && r.status == 200 {
        // Additive: `ok` stays, and the caller learns where the reflector's
        // audio now goes. The monitor joins the conference as a contributing
        // member, so every AllStar and EchoLink member hears it.
        let reach = ctrl.m17_relay_counts();
        return HttpResponse::json(
            200,
            &serde_json::json!({
                "ok": true,
                "relays_to": reach,
                "warning": reach.warning(),
            }),
        );
    }
    r
}

// ---- the Discord admin page (iax-c9f4) --------------------------------------

/// What kind of refusal a `discord: …` message is.
///
/// The rule engine says why in a sentence meant for a human; the prefixes
/// it puts in front are the machine half of that, and this is the only
/// place that reads them. Both are constants in `discord_commands.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscordRefusal {
    /// `discord: bad request: …` — the caller sent something unusable.
    BadRequest,
    /// `discord: busy: …` — the rule engine is checked out by a dial.
    Busy,
    /// Not configured, or not built in: there is nothing here to talk to.
    Absent,
    /// The engine's own refusal about state — banned, not approved, an
    /// approver. A well-formed request that does not apply.
    Conflict,
}

impl DiscordRefusal {
    fn of(msg: &str) -> Self {
        if msg.starts_with("discord: bad request:") {
            Self::BadRequest
        } else if msg.starts_with("discord: busy:") {
            Self::Busy
        } else if msg.starts_with("discord: slash commands are not configured")
            || msg.starts_with("discord: this build has no discord feature")
        {
            Self::Absent
        } else {
            Self::Conflict
        }
    }

    fn status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Busy => 503,
            // The same 500 `POST /discord` already answers for both of
            // these, so the page's two dead ends look to a client exactly
            // like the refusal the bridge route has always given.
            Self::Absent => 500,
            Self::Conflict => 409,
        }
    }
}

/// Render a `NodeError` from either Discord route.
///
/// The body always carries `error`. An `Absent` refusal also carries the
/// machine-readable pair the page needs to choose which explanation to
/// show, because "this build has no discord feature" and "nobody is an
/// approver yet" want completely different sentences and neither should be
/// chosen by matching on English.
fn discord_refusal(msg: &str) -> HttpResponse {
    let kind = DiscordRefusal::of(msg);
    let mut body = serde_json::json!({ "error": msg });
    if kind == DiscordRefusal::Absent {
        body["configured"] = serde_json::json!(false);
        body["commands"] = serde_json::json!(false);
        body["connected"] = serde_json::json!(false);
        body["feature"] = serde_json::json!(cfg!(feature = "discord"));
    }
    let r = HttpResponse::json(kind.status(), &body);
    if kind == DiscordRefusal::Busy {
        // A second is the pump's own worst case behind a dial that is
        // resolving DNS; the page retries once on its own anyway.
        return r.with_header("Retry-After", "1");
    }
    r
}

/// `POST /discord/admin` — one action from the node's own admin page.
fn discord_admin(ctrl: &NodeController, body: &[u8], headers: &RequestHeaders<'_>) -> HttpResponse {
    // Before anything is parsed, let alone executed: this route changes who
    // may key a mic and where the node listens.
    if let Err(refused) = same_origin_json(headers, "POST /discord/admin") {
        return refused;
    }
    let req: DiscordAdminRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    match ctrl.execute(NodeCommand::DiscordAdmin {
        action: req.action,
        user_id: req.user_id,
    }) {
        Ok(_) => HttpResponse::ok_json(),
        Err(e) => discord_refusal(&e.message),
    }
}

/// `GET /discord/state` — the admin view, plus the three facts about the
/// bot itself that the view does not carry.
fn discord_state(ctrl: &NodeController) -> HttpResponse {
    match ctrl.execute(NodeCommand::DiscordState) {
        #[cfg(feature = "discord")]
        Ok(NodeReply::DiscordState(view)) => {
            let mut v = serde_json::to_value(&*view).unwrap_or_else(|_| serde_json::json!({}));
            // `AdminView` describes the rule engine. Whether the bot is in
            // the channel, whether audio is actually reaching it, and
            // whether its commands are registered all live in the
            // snapshot, and the page's status strip needs them in one poll
            // — so they are merged in here rather than making the page poll
            // `/status` as well. `AdminView` has no field of any of these
            // names, so nothing is overwritten.
            //
            // `audio_ok` is carried separately from `connected` because it
            // is a separate fault: the audio track can die with the bridge
            // still installed, and `connected` alone said everything was
            // fine while the channel heard silence.
            let facts = discord_bot_facts(ctrl);
            v["configured"] = serde_json::json!(true);
            v["feature"] = serde_json::json!(true);
            v["connected"] = serde_json::json!(facts.connected);
            v["audio_ok"] = serde_json::json!(facts.audio_ok);
            v["commands"] = serde_json::json!(facts.commands);
            HttpResponse::json(200, &v)
        }
        Ok(_) => HttpResponse::error_json(500, "discord: unexpected reply"),
        Err(e) => discord_refusal(&e.message),
    }
}

// ---- the settings page's routes ----------------------------------------------

/// 503 for a controller `main` gave no store to: there is no file to edit.
fn settings_unavailable() -> HttpResponse {
    HttpResponse::error_json(503, "settings are not available on this node")
}

/// The facts the store cannot know, read off the controller.
fn state_facts(ctrl: &NodeController) -> StateFacts {
    let snap = ctrl.snapshot();
    StateFacts {
        node_id: snap.node_id,
        registered: snap.registered,
        busy: ctrl.busy_on_a_net(),
        discord_feature: cfg!(feature = "discord"),
    }
}

/// `GET /settings/state` — disk, running, pending, credential presence,
/// etag, file facts, restart facts. Re-reads node.toml on every call and
/// is never folded into the snapshot the SSE heartbeat re-serialises.
fn settings_state(ctrl: &NodeController) -> HttpResponse {
    let Some(store) = ctrl.settings_store() else {
        return settings_unavailable();
    };
    match store.state(ctrl.secrets(), &state_facts(ctrl)) {
        Ok(v) => HttpResponse::json(200, &v),
        Err(e) => HttpResponse::error_json(500, &e),
    }
}

/// `POST /settings` — one patch, written, then the live tier applied.
fn settings_save(ctrl: &NodeController, body: &[u8], headers: &RequestHeaders<'_>) -> HttpResponse {
    // Guard first, store second: a cross-origin caller learns nothing, not
    // even that this node has no settings page.
    if let Err(refused) = same_origin_json(headers, "POST /settings") {
        return refused;
    }
    let Some(store) = ctrl.settings_store() else {
        return settings_unavailable();
    };
    let req: SettingsPatch = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    // Whether THIS request came from the node itself is the fact the
    // control.bind rule needs. Read from the socket peer, not the Host: a
    // LAN caller can send `Host: localhost` as easily as anything else.
    let ctx = WriteContext {
        loopback_peer: headers.loopback_peer,
    };
    // Write, live apply and the running mirror in one critical section:
    // two saves in flight must reach the station in the order they reached
    // the disk, or the mirror names a value the station is not running.
    let saved = store.save_and_apply(
        &req,
        &ctx,
        |outcome| apply_live_tier(ctrl, outcome),
        |live| &live.applied,
    );
    match saved {
        Ok((outcome, live)) => HttpResponse::json(200, &save_reply(&outcome.etag, live)),
        Err(e) => save_error(&e),
    }
}

/// The 200 body of `POST /settings`. `apply_error` is present only when a
/// live apply failed; out of line so the key is pinned by a test that can
/// build a failing `LiveOutcome`, which no route request can.
fn save_reply(etag: &str, live: LiveOutcome) -> Value {
    let mut v = serde_json::json!({
        "ok": true,
        "applied": live.applied,
        "on_restart": live.on_restart,
        "etag": etag,
    });
    if let Some(msg) = live.apply_error {
        // The file is written and the etag is new; the paths that did not
        // take stay in pending.apply for the page's Apply button.
        v["apply_error"] = serde_json::json!(msg);
    }
    v
}

/// What the live tier did with one save.
struct LiveOutcome {
    /// Live-tier paths now running — what `mark_applied` is told.
    applied: Vec<String>,
    /// Changed paths that wait for a restart.
    on_restart: Vec<String>,
    /// The first live apply that failed, if any.
    apply_error: Option<String>,
}

/// `bridge.*`, `parrot.*` and the two removable tables themselves.
fn is_bridge_path(path: &str) -> bool {
    path == "bridge"
        || path == "parrot"
        || path.starts_with("bridge.")
        || path.starts_with("parrot.")
}

/// A live `[m17]` path. Only live-tier paths reach this test, and the only
/// live `[m17]` keys are `enabled` and the `[m17.reflectors]` table.
fn is_m17_path(path: &str) -> bool {
    path.starts_with("m17.")
}

/// `packet_log.*` and the removable table itself.
fn is_packet_log_path(path: &str) -> bool {
    path == "packet_log" || path.starts_with("packet_log.")
}

/// Apply the live tier of a written save: one `apply_bridge_config` for
/// any bridge/parrot change, one `SetDevices` for any device change, one
/// `apply_m17_settings` for any `[m17]` change, one
/// `set_packet_log_settings` for any `[packet_log]` change. They are
/// independent, so a failure in one does not skip the others.
/// `outcome.changed` also names a live path whose value already matched
/// the disk but which the running node had not taken (the page's Apply on
/// a hand-edited field), so that case reaches the same two doors.
fn apply_live_tier(ctrl: &NodeController, outcome: &SaveOutcome) -> LiveOutcome {
    let (live, on_restart): (Vec<String>, Vec<String>) = outcome
        .changed
        .iter()
        .cloned()
        .partition(|p| tier(p) == Some(Tier::Live));
    let mut applied = Vec::new();
    let mut apply_error = None;

    let bridge_paths: Vec<String> = live.iter().filter(|p| is_bridge_path(p)).cloned().collect();
    if !bridge_paths.is_empty() {
        let result = outcome
            .config
            .to_bridge_config()
            .map_err(|message| crate::command::NodeError { message })
            .and_then(|cfg| ctrl.apply_bridge_config(cfg));
        match result {
            Ok(()) => applied.extend(bridge_paths),
            Err(e) => apply_error = Some(e.message),
        }
    }

    let audio_paths: Vec<String> = live
        .iter()
        .filter(|p| *p == "audio.input" || *p == "audio.output")
        .cloned()
        .collect();
    if !audio_paths.is_empty() {
        // The whole [audio] table, not just the changed key: SetDevices
        // takes both and "unchanged" must mean "what the file says".
        let (input, output) = outcome
            .config
            .audio
            .as_ref()
            .map_or((None, None), |a| (a.input.clone(), a.output.clone()));
        match ctrl.execute(NodeCommand::SetDevices { input, output }) {
            Ok(_) => applied.extend(audio_paths),
            Err(e) => {
                if apply_error.is_none() {
                    apply_error = Some(e.message);
                }
            }
        }
    }

    let m17_paths: Vec<String> = live.iter().filter(|p| is_m17_path(p)).cloned().collect();
    if !m17_paths.is_empty() {
        // The whole section as the file now has it; the door keeps what is
        // restart-tier (the running directory) and never loads a library.
        match ctrl.apply_m17_settings(outcome.config.m17.as_ref()) {
            Ok(()) => applied.extend(m17_paths),
            Err(e) => {
                if apply_error.is_none() {
                    apply_error = Some(e.message);
                }
            }
        }
    }

    let packet_log_paths: Vec<String> = live
        .iter()
        .filter(|p| is_packet_log_path(p))
        .cloned()
        .collect();
    if !packet_log_paths.is_empty() {
        // Waits out any capture's writer, which costs this one connection.
        ctrl.set_packet_log_settings(outcome.config.packet_log.as_ref());
        applied.extend(packet_log_paths);
    }

    LiveOutcome {
        applied,
        on_restart,
        apply_error,
    }
}

/// The store's refusals on the wire. A field error carries `field` (and
/// `acknowledge` when the fix is a confirmation), an oracle error carries
/// only its sentence, a conflict carries `conflict: true` so the page can
/// tell "reload" from "fix the value".
fn save_error(e: &SaveError) -> HttpResponse {
    match e {
        SaveError::Conflict => HttpResponse::json(
            409,
            &serde_json::json!({
                "error": "node.toml changed since the page loaded",
                "conflict": true,
            }),
        ),
        SaveError::Field(f) => {
            let mut v = serde_json::json!({ "error": f.message, "field": f.field });
            if let Some(key) = &f.acknowledge {
                v["acknowledge"] = serde_json::json!(key);
            }
            HttpResponse::json(400, &v)
        }
        SaveError::Oracle(msg) => HttpResponse::error_json(400, msg),
        SaveError::Io(msg) => HttpResponse::error_json(500, msg),
    }
}

/// `POST /settings/credential` — set or forget one credential. The file is
/// written BEFORE the provider is told, so a value the node could not keep
/// is never one it is briefly using; the reply is bare `{"ok":true}` as
/// `/secrets` gives, so the value has no path back out even if a reply
/// type ever changes.
fn settings_credential(
    ctrl: &NodeController,
    body: &[u8],
    headers: &RequestHeaders<'_>,
) -> HttpResponse {
    if let Err(refused) = same_origin_json(headers, "POST /settings/credential") {
        return refused;
    }
    let Some(store) = ctrl.settings_store() else {
        return settings_unavailable();
    };
    let req: CredentialRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        // Not serde's sentence: its `invalid type` errors quote the
        // offending scalar, and on THIS route the scalar can be the
        // credential — a numeric password posted as a JSON number would
        // come straight back in the 400. Nothing from the body is echoed.
        Err(_) => {
            return HttpResponse::error_json(
                400,
                "bad request: expected {\"name\": \"…\", \"value\": \"…\"}",
            );
        }
    };
    let name = match CredentialName::parse(&req.name) {
        Ok(n) => n,
        Err(msg) => return HttpResponse::error_json(400, &msg),
    };
    // A name this node refuses is the caller's mistake (400); a node.toml
    // that cannot be read is the node's (500).
    let key = match store.credential_key(&name) {
        Ok(k) => k,
        Err(CredentialKeyError::Rejected(msg)) => return HttpResponse::error_json(400, &msg),
        Err(CredentialKeyError::Unreadable(msg)) => return HttpResponse::error_json(500, &msg),
    };
    if req.forget {
        if let Err(msg) = store.forget_credential(&name) {
            return HttpResponse::error_json(500, &msg);
        }
        ctrl.secrets().remove(&key);
        // The node password can sit under three provider keys, and
        // `credentials.node` reads `set` when any of them is non-empty,
        // so forget clears all three: the on-disk number
        // (`key`), the literal "node" (a password saved before the number
        // existed), and the number the process is RUNNING as — a
        // file-sourced node loaded its password under that id at start, and
        // an on-disk renumbering saved since must not leave it behind for
        // the registrar to keep using.
        if matches!(name, CredentialName::Node) {
            ctrl.secrets().remove(NODE_CREDENTIAL_KEY);
            if let Some(id) = ctrl.snapshot().node_id {
                ctrl.secrets().remove(&id);
            }
        }
        return HttpResponse::ok_json();
    }
    let Some(value) = req.value.filter(|v| !v.is_empty()) else {
        return HttpResponse::error_json(
            400,
            &format!("credential \"{}\" needs a value", req.name),
        );
    };
    if let Err(msg) = store.save_credential(ctrl.secrets(), &name, &value, req.migrate) {
        return HttpResponse::error_json(500, &msg);
    }
    match ctrl.execute(NodeCommand::ProvideSecret {
        username: key,
        secret: value,
    }) {
        Ok(_) => HttpResponse::ok_json(),
        Err(e) => HttpResponse::error_json(500, &e.message),
    }
}

/// `POST /settings/portal/test` — mint and drop a WT token with the
/// RUNNING portal credentials. Blocking HTTPS, on the per-request worker
/// thread `server::dispatch` already gives every route.
fn settings_portal_test(
    ctrl: &NodeController,
    body: &[u8],
    headers: &RequestHeaders<'_>,
) -> HttpResponse {
    if let Err(refused) = same_origin_json(headers, "POST /settings/portal/test") {
        return refused;
    }
    if ctrl.settings_store().is_none() {
        return settings_unavailable();
    }
    // `{}` or nothing; any JSON is accepted, anything else is not.
    if !body.is_empty() && serde_json::from_slice::<Value>(body).is_err() {
        return HttpResponse::error_json(400, "bad request: the body is not JSON");
    }
    match ctrl.execute(NodeCommand::PortalTest) {
        Ok(_) => HttpResponse::ok_json(),
        // 409, not 500: the node is fine, the credentials are not.
        Err(e) => HttpResponse::json(409, &serde_json::json!({ "error": e.message })),
    }
}

/// `POST /settings/restart` — a shutdown that ends in a re-exec. 409 with
/// `busy: true` while the node is on a net, unless the body insists.
fn settings_restart(
    ctrl: &NodeController,
    body: &[u8],
    headers: &RequestHeaders<'_>,
) -> HttpResponse {
    if let Err(refused) = same_origin_json(headers, "POST /settings/restart") {
        return refused;
    }
    if ctrl.settings_store().is_none() {
        return settings_unavailable();
    }
    let req: RestartRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
    };
    match ctrl.execute(NodeCommand::Restart {
        disconnect: req.disconnect,
    }) {
        Ok(_) => HttpResponse::ok_json(),
        Err(e) if e.message.starts_with("restart refused:") => HttpResponse::json(
            409,
            &serde_json::json!({ "error": e.message, "busy": true }),
        ),
        Err(e) => HttpResponse::error_json(500, &e.message),
    }
}

/// The three facts about the bot itself that `AdminView` does not carry.
///
/// A struct rather than a tuple because they are three independent
/// booleans and a caller that swapped two of them would compile: the page's
/// status strip would then report "no audio" as "not connected".
#[cfg(feature = "discord")]
struct DiscordBotFacts {
    connected: bool,
    audio_ok: bool,
    commands: bool,
}

/// Those facts from the node snapshot — all false when there is no
/// `[discord]` section to report on.
#[cfg(feature = "discord")]
fn discord_bot_facts(ctrl: &NodeController) -> DiscordBotFacts {
    let absent = DiscordBotFacts {
        connected: false,
        audio_ok: false,
        commands: false,
    };
    match ctrl.execute(NodeCommand::Status) {
        Ok(NodeReply::Snapshot(s)) => s.discord.map_or(absent, |d| DiscordBotFacts {
            connected: d.connected,
            audio_ok: d.audio_ok,
            commands: d.commands,
        }),
        _ => absent,
    }
}

/// The guard on `POST /discord/admin`.
///
/// The control port is this node's root of trust — it can key a
/// transmitter — and it listens on loopback, which does not make it private:
/// every page in the operator's browser can reach loopback, and DNS can be
/// re-pointed at it. Three checks, each of which stops a different attack
/// and none of which is redundant:
///
/// * **`Host` must name this node.** `localhost`, any loopback IP literal,
///   or the exact address the control socket is bound to when that is not
///   loopback. This is the one that stops **DNS rebinding**: a page served
///   from `http://evil.com:8730/` whose DNS is re-pointed to `127.0.0.1`
///   sends `Host: evil.com:8730` and `Origin: http://evil.com:8730`, which
///   agree with each other perfectly — the browser believes it is talking
///   to its own origin, sends no preflight, and its script may set any
///   `Content-Type` it likes. Both of the checks below pass. Only the name
///   in `Host` gives it away.
/// * **`Origin` must match `Host`** when an `Origin` is present. This stops
///   a **cross-origin `fetch`** from a page that has not rebound anything.
///   `curl` and the node's own tooling send no `Origin` and are allowed —
///   anything that can set arbitrary headers can talk to the port directly,
///   so it was never the threat; the `Host` check above still applies to
///   them.
/// * **`Content-Type: application/json`.** This stops a **cross-origin
///   "simple" request** — a `<form>` post, an `<img>`, a `fetch` with a
///   simple content type. None of them can set it, and the preflight needed
///   to set it gets no `Access-Control-Allow-*` back, so the browser never
///   sends the real request.
///
/// The `Host` allow-list already ran for every route in
/// [`handle_request_with_headers`] and in `server::dispatch`; it is checked
/// again here because this function is the whole contract for the routes
/// that opt in, and a caller that reached one some other way must get the
/// same answer. The other two checks are NOT applied to the routes that
/// shipped before this one (see the module docs): requiring JSON of them is
/// a contract change for every existing client and is iax-ctlcsrf step 2.
///
/// `route` is the route as a client spells it — `"POST /discord/admin"`,
/// `"POST /settings"` — and it goes into the 415 sentence, so a client that
/// sent the wrong media type is told which route refused it rather than
/// having to guess from a sentence that names some other one.
///
/// # Errors
/// 415 for a body that is not JSON; 403 for a missing or foreign `Host`, or
/// for an `Origin` that does not match it.
fn same_origin_json(headers: &RequestHeaders<'_>, route: &str) -> Result<(), HttpResponse> {
    // Host first, so the no-`Origin` caller is covered by it too.
    if !headers.host_verified && !host_is_this_node(headers.host, headers.bound_to) {
        return Err(forbidden_cross_origin());
    }
    // JSON of every caller here, not only of a browser; then the same
    // Origin rule as every other mutating route.
    if !astar_http::guards::is_json(headers.content_type) {
        return Err(origin_refusal(
            astar_http::guards::OriginRefusal::NotJson,
            route,
        ));
    }
    origin_rule(headers, route)
}

/// iax-ctlcsrf step 2: the rule every mutating route answers to, whoever
/// the caller is.
///
/// * **`Origin`, when present, must match `Host`.** A browser sends
///   `Origin` on every cross-site POST, and `Origin: null` (a sandboxed
///   frame, a `data:` page) matches nothing.
/// * **A request from a browser must be `Content-Type: application/json`.**
///   "From a browser" is `Origin` or `Sec-Fetch-Site` present. A cross-site
///   `<form>` can only send the three "simple" content types, and the
///   preflight a page needs to send JSON cross-origin gets no
///   `Access-Control-Allow-*` back, so the form attack is closed even on
///   a browser too old to send `Origin` on a same-site POST.
///
/// `curl` and the node's own tooling send neither header, so their
/// contract is unchanged: any content type, or none, on every route. That
/// is deliberate — anything that can set arbitrary headers could talk to the
/// port directly anyway; the threat this closes is a page.
///
/// The JSON check runs first, as in [`same_origin_json`], so a form post
/// from a foreign page is told about the media type, which is the thing a
/// legitimate page author got wrong.
///
/// # Errors
/// 415 for a browser request that is not JSON; 403 for an `Origin` that
/// does not match `Host`.
fn origin_rule(headers: &RequestHeaders<'_>, route: &str) -> Result<(), HttpResponse> {
    astar_http::guards::origin_rule_raw(
        headers.origin,
        headers.sec_fetch_site,
        headers.content_type,
        headers.host,
    )
    .map_err(|refusal| origin_refusal(refusal, route))
}

/// The answer to an [`astar_http::guards::OriginRefusal`] on this port:
/// the route named for a media type, the allow-list's one sentence for a
/// foreign `Origin`.
fn origin_refusal(refusal: astar_http::guards::OriginRefusal, route: &str) -> HttpResponse {
    match refusal {
        astar_http::guards::OriginRefusal::NotJson => HttpResponse::error_json(
            415,
            &format!("{route} requires Content-Type: application/json"),
        ),
        astar_http::guards::OriginRefusal::CrossOrigin => forbidden_cross_origin(),
    }
}

/// The allow-list's refusal: one sentence, one status, on every route and
/// in `server::dispatch`, so a refused caller learns nothing about which
/// route it was refused from.
#[must_use]
pub(crate) fn forbidden_cross_origin() -> HttpResponse {
    HttpResponse::error_json(403, "cross-origin requests are not accepted on this port")
}

/// 413 for a body over [`MAX_BODY_BYTES`]. The sentence carries the number
/// so a client that hit the cap learns the limit from the refusal rather
/// than from this source.
#[must_use]
pub(crate) fn payload_too_large() -> HttpResponse {
    HttpResponse::error_json(413, &format!("request body exceeds {MAX_BODY_BYTES} bytes"))
}

/// Whether `Host` is `localhost` (any case) or a loopback IP literal, with
/// any `:port` stripped by [`host_without_port`]. `None` is `false`.
///
/// The half of [`host_is_this_node`] that needs no knowledge of the socket.
/// It stands alone because the settings page's `[control] bind` rule asks a
/// different question of the same header: not "may this request reach the
/// port" but "did it arrive over loopback" — which a node bound to a LAN
/// address answers *no* to even for a request the allow-list admits.
#[must_use]
pub(crate) fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(name) = host.map(str::trim).and_then(host_without_port) else {
        return false;
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether `Host` names this node rather than somewhere a DNS answer has
/// been pointed at it.
///
/// A missing `Host` is a no: HTTP/1.1 requires one, and a request without
/// one cannot be shown to be talking to this node on purpose.
#[must_use]
pub(crate) fn host_is_this_node(
    host: Option<&str>,
    bound_to: Option<std::net::SocketAddr>,
) -> bool {
    if host_is_loopback(host) {
        return true;
    }
    let Some(name) = host.map(str::trim).and_then(host_without_port) else {
        return false;
    };
    let Ok(ip) = name.parse::<std::net::IpAddr>() else {
        // A name, and not `localhost`. `127.0.0.1.evil.com` lands here, as
        // does every rebinding host — resolving it would be asking the
        // attacker's own DNS whether the attacker is allowed in.
        return false;
    };
    // A node whose `[control] bind` is a LAN address is reached at that
    // address, and its own page's `Host` is that literal. Compared against
    // what the socket is ACTUALLY bound to rather than against the config
    // string: it needs no threading through the controller, it cannot drift
    // from reality, and a config that failed to bind can never authorise
    // anything.
    // A WILDCARD bind reports its own address as `0.0.0.0` (or `::`), which
    // is nobody's `Host`: a node bound wide and reached at its LAN address
    // sends `Host: 192.168.x.y` and is refused here. That is deliberate —
    // this guard fails closed rather than trusting whatever name a request
    // arrived under, and the documented way to reach the admin page from
    // another machine is an SSH tunnel, which makes the browser's `Host`
    // `localhost` and needs no exception at all.
    // A wildcard `Host` is refused outright too, even on the (opted-in)
    // wildcard bind whose own address it would match.
    bound_to.is_some_and(|b| !b.ip().is_loopback() && !b.ip().is_unspecified() && b.ip() == ip)
}

/// `Host` with any `:port` removed, IPv6 literals included — or `None` when
/// what follows the name is not a port.
///
/// `[::1]:8730` → `::1`, `[::1]` → `::1`, `::1` → `::1` (a bare IPv6
/// literal has colons of its own and must never be split on the last one),
/// `localhost:8730` → `localhost`.
///
/// The remainder is CHECKED rather than discarded. Splitting on the first
/// colon and throwing away the rest would read `localhost:8730@evil.com` as
/// `localhost` — an authority whose real host is `evil.com`. No browser can
/// produce that (userinfo is stripped before the request line is built, and
/// `Host` is a forbidden header `fetch` may not set), so this is hardening
/// rather than a fix; a guard that decides who may key a transmitter should
/// not be the place where an unparsed tail is trusted.
fn host_without_port(host: &str) -> Option<&str> {
    if let Some(rest) = host.strip_prefix('[') {
        let (literal, after) = rest.split_once(']')?;
        return port_suffix_ok(after).then_some(literal);
    }
    // More than one colon means a bare IPv6 literal, which carries no port.
    if host.matches(':').count() > 1 {
        return Some(host);
    }
    match host.split_once(':') {
        Some((name, port)) => is_port(port).then_some(name),
        None => Some(host),
    }
}

/// What may follow a bracketed IPv6 literal: nothing, or a real `:port`.
fn port_suffix_ok(suffix: &str) -> bool {
    match suffix.strip_prefix(':') {
        Some(port) => is_port(port),
        None => suffix.is_empty(),
    }
}

/// One to five ASCII digits — the only shape a port has. An EMPTY port
/// (`localhost:`) is refused too: it is not a port, and accepting it would
/// mean the tail was never really looked at.
fn is_port(port: &str) -> bool {
    (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
}

/// Execute `cmd` and map `NodeReply` / `NodeError` to an `HttpResponse`.
fn execute_and_render(ctrl: &NodeController, cmd: NodeCommand) -> HttpResponse {
    match ctrl.execute(cmd) {
        Ok(NodeReply::Ok) => HttpResponse::ok_json(),
        Ok(NodeReply::Snapshot(s)) => {
            let v = serde_json::to_value(&s).unwrap_or_else(|_| serde_json::json!({}));
            HttpResponse::json(200, &v)
        }
        // The INNER view, not the tagged reply — the same shape the
        // snapshot arm above uses, so the admin page reads
        // `{ enabled, approvers, admins, … }` at the top level.
        #[cfg(feature = "discord")]
        Ok(NodeReply::DiscordState(view)) => {
            let v = serde_json::to_value(&*view).unwrap_or_else(|_| serde_json::json!({}));
            HttpResponse::json(200, &v)
        }
        Err(e) => HttpResponse::error_json(500, &e.message),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{controller::NodeController, secrets::SecretProvider};

    /// Build a test controller backed by `NullBackend` (no audio hardware),
    /// mirroring the private `test_controller()` in `controller.rs`.
    fn test_controller() -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        NodeController::new(station, secrets)
    }

    // ---- required tests from the brief --------------------------------------

    #[test]
    fn status_route_returns_200_secret_free() {
        let c = test_controller();
        let r = handle_request(&c, "GET", "/status", b"");
        assert_eq!(r.status, 200);
        let body = String::from_utf8(r.body).unwrap();
        assert!(
            body.contains("listening"),
            "status body should contain 'listening': {body}"
        );
        for bad in ["secret", "password", "token"] {
            assert!(
                !body.contains(bad),
                "status body must not contain '{bad}': {body}"
            );
        }
    }

    // ---- iax-d829.1 (ported from iax-213f): /link route + roster in /status

    #[test]
    fn link_connect_at_addr_returns_200_and_shows_in_status() {
        let c = test_controller();
        // `_guard` and not `_`: a bare `_` drops the socket at once, which
        // closes the port and lets ICMP tear the call down mid-assertion.
        let (_guard, addr) = crate::test_support::dead_port();
        let r = handle_request(
            &c,
            "POST",
            "/link",
            format!(r#"{{"action":"connect","node":"55553","addr":"{addr}"}}"#).as_bytes(),
        );
        assert_eq!(r.status, 200, "connect link returns 200");

        // /status now carries the link roster.
        let s = handle_request(&c, "GET", "/status", b"");
        let v: serde_json::Value = serde_json::from_slice(&s.body).unwrap();
        let links = v["links"].as_array().expect("status has a links array");
        assert_eq!(links.len(), 1, "one link in status: {v}");
        assert_eq!(links[0]["node"], serde_json::json!("55553"));
    }

    #[test]
    fn link_addr_off_loopback_is_refused_before_any_dial() {
        let c = test_controller();
        for addr in [
            "192.0.2.1:4569",
            "[2001:db8::1]:4569",
            "0.0.0.0:4569",
            "localhost:4569",
            "node.example.net:4569",
        ] {
            let r = handle_request(
                &c,
                "POST",
                "/link",
                format!(r#"{{"action":"connect","node":"55553","addr":"{addr}"}}"#).as_bytes(),
            );
            assert_eq!(
                r.status,
                400,
                "{addr}: {}",
                String::from_utf8_lossy(&r.body)
            );
            let body = String::from_utf8_lossy(&r.body);
            assert!(body.contains("loopback"), "{addr}: {body}");
            let s = handle_request(&c, "GET", "/status", b"");
            let v: serde_json::Value = serde_json::from_slice(&s.body).unwrap();
            assert!(
                v["links"].as_array().is_none_or(Vec::is_empty),
                "{addr}: nothing was dialled: {v}"
            );
        }
    }

    /// serde's `invalid type` sentence quotes the offending scalar, and on
    /// this route the scalar can be the secret itself.
    #[test]
    fn a_mistyped_secret_is_never_echoed_in_the_400() {
        let c = test_controller();
        for body in [
            r#"{"username":"u","secret":918273645}"#,
            r#"{"username":"u","secret":9182.73645}"#,
            r#"{"username":918273645,"secret":"x"}"#,
        ] {
            let r = handle_request(&c, "POST", "/secrets", body.as_bytes());
            assert_eq!(r.status, 400, "{body}");
            let text = String::from_utf8_lossy(&r.body);
            assert!(!text.contains("918273645"), "{body} -> {text}");
            assert!(!text.contains("9182.73645"), "{body} -> {text}");
            assert_eq!(
                body_json(&r),
                serde_json::json!({
                    "error": "bad request: expected {\"username\": \"…\", \"secret\": \"…\"}"
                })
            );
        }
    }

    #[test]
    fn link_disconnect_unknown_node_is_500() {
        let c = test_controller();
        let r = handle_request(
            &c,
            "POST",
            "/link",
            br#"{"action":"disconnect","node":"99999"}"#,
        );
        assert_eq!(r.status, 500, "disconnecting an unknown node maps to 500");
    }

    #[test]
    fn link_bad_json_is_400() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/link", b"not json");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn link_unknown_action_is_400() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/link", br#"{"action":"warp","node":"42"}"#);
        assert_eq!(r.status, 400, "unknown action is a deserialization 400");
    }

    #[test]
    fn unknown_route_404_and_bad_json_400() {
        let c = test_controller();
        assert_eq!(handle_request(&c, "GET", "/nope", b"").status, 404);
        assert_eq!(handle_request(&c, "POST", "/dial", b"not json").status, 400);
    }

    #[test]
    fn provide_secret_route_does_not_echo_secret() {
        let c = test_controller();
        let r = handle_request(
            &c,
            "POST",
            "/secrets",
            br#"{"username":"1234","secret":"swordfish"}"#,
        );
        assert_eq!(r.status, 200);
        assert!(
            !String::from_utf8(r.body).unwrap().contains("swordfish"),
            "secret must not appear in response body"
        );
        assert_eq!(c.secrets().resolve("1234"), "swordfish");
    }

    // ---- additional route coverage -----------------------------------------

    #[test]
    fn hangup_when_idle_is_ok() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/hangup", b"");
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
    }

    #[test]
    fn key_and_unkey_when_idle_are_errors_not_panics() {
        // Key/Unkey with no active call returns a NodeError (409-ish mapped to 500);
        // the important thing is it doesn't panic.
        let c = test_controller();
        let rk = handle_request(&c, "POST", "/key", b"");
        // Could be 200 or 500 depending on Station implementation; just ensure no panic.
        let _ = rk.status;
        let ru = handle_request(&c, "POST", "/unkey", b"");
        let _ = ru.status;
    }

    #[test]
    fn enable_and_disable_inbound_round_trips() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/enable_inbound", b"");
        assert_eq!(r.status, 200, "enable_inbound should return 200");
        let r2 = handle_request(&c, "POST", "/disable_inbound", b"");
        assert_eq!(r2.status, 200, "disable_inbound should return 200");
    }

    #[test]
    fn devices_with_valid_body_is_ok() {
        let c = test_controller();
        let r = handle_request(
            &c,
            "POST",
            "/devices",
            br#"{"input":"Mic A","output":"Speaker B"}"#,
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn devices_with_bad_json_is_400() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/devices", b"not json");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn secrets_with_bad_json_is_400() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/secrets", b"not json");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn shutdown_sets_stop_flag() {
        let c = test_controller();
        assert!(!c.should_stop());
        let r = handle_request(&c, "POST", "/shutdown", b"");
        assert_eq!(r.status, 200);
        assert!(c.should_stop(), "shutdown must set the stop flag");
    }

    #[test]
    fn register_without_config_is_500() {
        // Register with no register_cfg set returns NodeError → 500.
        let c = test_controller();
        let r = handle_request(&c, "POST", "/register", b"");
        assert_eq!(r.status, 500);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("no register config"),
            "unexpected error: {:?}",
            v["error"]
        );
    }

    #[test]
    fn bridge_route_switches_mode_live() {
        let c = test_controller();
        let r = handle_request(
            &c,
            "POST",
            "/bridge",
            br#"{"mode":"conference","mix_minus":false,"include_local_radio":true}"#,
        );
        assert_eq!(r.status, 200, "bridge switch returns 200");
        let cfg = c.bridge_config_for_test();
        assert_eq!(cfg.mode, astar_iax::BridgeMode::Conference);
        assert!(!cfg.mix_minus);
        assert!(cfg.include_local_radio);
    }

    #[test]
    fn bridge_route_partial_body_keeps_other_fields() {
        let c = test_controller();
        // Switch to handset first so we have a known starting point.
        let _ = handle_request(&c, "POST", "/bridge", br#"{"mode":"handset"}"#);
        // Now toggle only mix_minus; mode must remain handset.
        let r = handle_request(&c, "POST", "/bridge", br#"{"mix_minus":false}"#);
        assert_eq!(r.status, 200);
        let cfg = c.bridge_config_for_test();
        assert_eq!(cfg.mode, astar_iax::BridgeMode::Handset, "mode unchanged");
        assert!(!cfg.mix_minus, "mix_minus updated");
    }

    #[test]
    fn bridge_route_unknown_mode_is_500() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/bridge", br#"{"mode":"mesh"}"#);
        assert_eq!(r.status, 500);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("bridge.mode"));
    }

    #[test]
    fn bridge_route_bad_json_is_400() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/bridge", b"not json");
        assert_eq!(r.status, 400);
    }

    // Two distinct causes both surface as a 500 through the same route, so
    // each build asserts the ONE cause it can actually exercise, on the
    // message and not just the status — otherwise a discord-feature build
    // with a broken `discord_cfg` check could pass by accidentally hitting
    // the not-feature refusal message instead, and vice versa.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_join_is_refused_when_unconfigured() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/discord", br#"{"action":"join"}"#);
        assert_eq!(r.status, 500, "no [discord] section: {r:?}");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("no [discord] section"),
            "unexpected error: {:?}",
            v["error"]
        );
    }

    #[cfg(not(feature = "discord"))]
    #[test]
    fn discord_join_is_refused_when_unconfigured() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/discord", br#"{"action":"join"}"#);
        assert_eq!(r.status, 500, "no discord feature compiled in: {r:?}");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("no discord feature"),
            "unexpected error: {:?}",
            v["error"]
        );
    }

    #[test]
    fn discord_rejects_a_bad_body() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/discord", b"not json");
        assert_eq!(r.status, 400);
    }

    #[test]
    fn discord_has_no_arm_action() {
        let c = test_controller();
        let r = handle_request(&c, "POST", "/discord", br#"{"action":"arm"}"#);
        assert_eq!(r.status, 400, "arm must not deserialize into an action");
    }

    #[test]
    fn status_response_content_type_is_json() {
        let c = test_controller();
        let r = handle_request(&c, "GET", "/status", b"");
        assert_eq!(r.content_type, "application/json");
    }

    #[test]
    fn error_response_is_valid_json_with_error_key() {
        let c = test_controller();
        let r = handle_request(&c, "GET", "/nope", b"");
        assert_eq!(r.status, 404);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            v["error"].is_string(),
            "error response must have 'error' key"
        );
    }

    #[test]
    fn index_is_served_as_html() {
        let ctrl = test_controller();
        let resp = handle_request(&ctrl, "GET", "/", b"");
        assert_eq!(resp.status, 200);
        assert!(resp.content_type.starts_with("text/html"));
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            body.contains("id=\"links-body\""),
            "index has the links table"
        );
        assert!(body.contains("src=\"app.js\"") && body.contains("href=\"style.css\""));
    }

    #[test]
    fn app_js_and_style_css_are_served() {
        let ctrl = test_controller();
        let js = handle_request(&ctrl, "GET", "/app.js", b"");
        assert_eq!(js.status, 200);
        assert!(js.content_type.contains("javascript"));
        assert!(
            String::from_utf8(js.body).unwrap().contains("EventSource"),
            "page consumes /events via EventSource"
        );
        let css = handle_request(&ctrl, "GET", "/style.css", b"");
        assert_eq!(css.status, 200);
        assert!(css.content_type.contains("css"));
    }

    /// `css` with every `/* ... */` comment replaced by one space, so that
    /// prose ("a wall of red", the button comment) is never mistaken for a
    /// declaration. An unterminated comment swallows the rest of the text.
    fn strip_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some((before, after)) = rest.split_once("/*") {
            out.push_str(before);
            out.push(' ');
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        }
        out.push_str(rest);
        out
    }

    /// Colour literals in a CSS fragment, comments excluded: hex colours
    /// (`#fff`, `#1e3a8a`), numeric `rgb(`/`rgba(`/`hsl(`/`hsla(` calls in
    /// any case, and the bare names a contributor reaches for by reflex
    /// (`white`, `black`, `red`, ...). `transparent` and `currentColor` are
    /// scheme-neutral and allowed. A `#` followed by an identifier that
    /// merely starts with hex digits (`#badge`, `#dead-end`) is a selector,
    /// not a colour: the digit run does not end at a word boundary. A name
    /// inside a longer identifier (`--green-rgb`, `.red-x`, `border`) is not
    /// a colour either: a name must start a word that no `.`, `#`, `-`, `_`
    /// or alphanumeric precedes. `rgba(var(--green-rgb), ...)` is not a
    /// literal because a token, not a number, follows the paren. Only ASCII
    /// bytes are used as slice starts, so a multi-byte character cannot
    /// split a char.
    fn colour_literals(css: &str) -> Vec<String> {
        const NAMED: [&str; 9] = [
            "white", "black", "red", "green", "blue", "gray", "grey", "orange", "yellow",
        ];
        let css = strip_comments(css);
        let bytes = css.as_bytes();
        let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_';
        let mut out = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'#' {
                let run = bytes[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .count();
                let next = bytes.get(i + 1 + run).copied().unwrap_or(b' ');
                if matches!(run, 3 | 4 | 6 | 8) && !is_word(next) {
                    out.push(css[i..=i + run].to_string());
                }
                continue;
            }
            let prev = if i == 0 { b' ' } else { bytes[i - 1] };
            if !b.is_ascii_alphabetic() || is_word(prev) || prev == b'.' || prev == b'#' {
                continue;
            }
            let tail = &css[i..];
            let len = tail.bytes().take_while(|&c| is_word(c)).count();
            let word = tail[..len].to_ascii_lowercase();
            let numeric_call = tail[len..]
                .strip_prefix('(')
                .is_some_and(|args| args.trim_start().starts_with(|c: char| c.is_ascii_digit()));
            if matches!(word.as_str(), "rgb" | "rgba" | "hsl" | "hsla") && numeric_call {
                out.push(tail.chars().take(24).collect());
            } else if NAMED.contains(&word.as_str()) {
                out.push(tail[..len].to_string());
            }
        }
        out
    }

    /// The stylesheet's contract with the system appearance setting: both
    /// schemes are opted into, every token has a dark and a light value, and
    /// nothing below the token marker names a colour (a hex literal, a
    /// numeric `rgb()`/`hsl()` call, or a bare colour name). The last assertion
    /// is what keeps the light theme from rotting one hard-coded `#fff` at a
    /// time (iax-ctlstyle-light).
    #[test]
    fn style_css_carries_a_light_theme_on_root_tokens() {
        assert!(
            STYLE_CSS.contains("html { color-scheme: light dark; }"),
            "the html rule opts into both colour schemes"
        );
        assert!(
            !STYLE_CSS.contains("color-scheme: dark;"),
            "the dark-only declaration is gone"
        );
        assert!(
            STYLE_CSS.contains("@media (prefers-color-scheme: light)"),
            "there is a light branch"
        );
        assert!(
            STYLE_CSS.contains("background: var(--bg);"),
            "body paints the page background from the token"
        );

        let (tokens, rest) = STYLE_CSS
            .split_once("/* ---- end of tokens ---- */")
            .expect("the token blocks end at the marker comment");
        for name in [
            "--bg",
            "--text",
            "--text-dim",
            "--card",
            "--edge",
            "--edge-glow",
            "--row-edge",
            "--code-bg",
            "--btn-bg",
            "--btn-hover",
            "--banner-bg",
            "--err-text",
            "--glow-a",
            "--glow-b",
            "--glow-c",
            "--green",
            "--amber",
            "--red",
            "--blue",
        ] {
            let decl = format!("{name}:");
            assert_eq!(
                tokens.matches(&decl).count(),
                2,
                "{name} is declared once for dark and once for light"
            );
        }
        assert_eq!(
            colour_literals(rest),
            Vec::<String>::new(),
            "every colour lives in the token blocks; these literals are below the marker"
        );
    }

    /// The class names settings.js builds its rows from (Tasks 7 and 8 use
    /// them by name), and the reduced-motion rule reaching the two
    /// pseudo-element transitions that the `*` selector never matched.
    #[test]
    fn style_css_defines_the_settings_page_idioms() {
        // Only the rules above the narrow-width block count: that block
        // re-states `.field {`, `.seg {`, `.seg button {` and `.card-foot {`,
        // and must not stand in for a main rule that was dropped.
        let (wide, _) = STYLE_CSS
            .split_once("@media (max-width: 460px)")
            .expect("the narrow-width block exists");
        for selector in [
            "input, select, textarea {",
            ".seg {",
            ".seg button {",
            ".seg button.on {",
            ".chip {",
            ".chip.set {",
            ".chip.unset {",
            ".chips {",
            ".chip-input {",
            ".field {",
            ".field-label {",
            ".field-control {",
            ".field-control > p {",
            ".apply-tag {",
            ".apply-tag.live {",
            ".apply-tag.restart {",
            ".apply-tag.apply {",
            ".was {",
            ".card-foot {",
            ".checklist {",
            ".step {",
            ".step .dot {",
            ".step.done .dot {",
            ".advanced {",
            ".confirm {",
            ".badge.warn {",
        ] {
            // Anchored at a line start: `.chip {` must not be satisfied by
            // the tail of `.chips .chip {`, nor `.apply-tag {` by
            // `.field .apply-tag {`.
            assert!(
                wide.contains(&format!("\n{selector}")),
                "style.css must define `{selector}`; settings.js builds rows from it"
            );
        }

        let (_, reduced) = STYLE_CSS
            .split_once("@media (prefers-reduced-motion: reduce)")
            .expect("the reduced-motion block exists");
        for selector in [
            ".ambient",
            ".tag.live, .pill.rx",
            ".advanced > summary::before",
            ".switch .track::after",
        ] {
            assert!(
                reduced.contains(selector),
                "reduced motion must switch off `{selector}`"
            );
        }
    }

    #[test]
    fn status_json_carries_node_id_field() {
        let ctrl = test_controller();
        let resp = handle_request(&ctrl, "GET", "/status", b"");
        assert!(
            String::from_utf8(resp.body)
                .unwrap()
                .contains("\"node_id\"")
        );
    }

    // =========================================================================
    // POST /m17 — the receive-only M17 monitor
    // =========================================================================

    /// The exact `{"error": …}` body the route answers with.
    fn error_body(msg: &str) -> serde_json::Value {
        serde_json::json!({ "error": msg })
    }

    fn json_of(r: &HttpResponse) -> serde_json::Value {
        serde_json::from_slice(&r.body).expect("every /m17 reply is JSON")
    }

    /// The route as `curl -H 'Content-Type: application/json'` reaches it.
    fn m17_post(c: &NodeController, body: &str) -> HttpResponse {
        handle_request_with_headers(c, "POST", "/m17", body.as_bytes(), &json_headers())
    }

    /// A controller with `[m17]` on and one table entry, `M17-TEST`, pointing
    /// at `reflector` (always a 127.0.0.1 socket), in Bridge mode with the
    /// inbound listener up — the topology a monitor needs to find a
    /// conference to join. `NodeController::new` binds inbound to
    /// 127.0.0.1:0, never udp/4569.
    fn m17_controller(reflector: std::net::SocketAddr) -> NodeController {
        let mut c = test_controller();
        c.set_m17_config(Some(crate::m17_directory::M17Settings {
            enabled: true,
            reflectors: std::collections::BTreeMap::from([(
                "M17-TEST".to_string(),
                (reflector.ip().to_string(), reflector.port()),
            )]),
            directory: None,
        }));
        assert_eq!(
            handle_request(&c, "POST", "/enable_inbound", b"").status,
            200,
            "inbound listener starts on 127.0.0.1:0"
        );
        assert_eq!(
            handle_request(&c, "POST", "/bridge", br#"{"mode":"bridge"}"#).status,
            200,
            "bridge mode is accepted"
        );
        c
    }

    fn status_json(c: &NodeController) -> serde_json::Value {
        let r = handle_request(c, "GET", "/status", b"");
        assert_eq!(r.status, 200);
        json_of(&r)
    }

    /// The first CONN the reflector recorded, if any.
    fn first_conn(
        rec: &crate::test_support::RecordingReflector,
    ) -> Option<astar_m17::ControlPacket> {
        rec.datagrams()
            .iter()
            .filter_map(|d| astar_m17::ControlPacket::parse(d))
            .find(|p| matches!(p, astar_m17::ControlPacket::Conn { .. }))
    }

    /// A well-formed monitor body that the controller would accept: every
    /// refusal below is the guard's, not the parser's or the pre-flight's.
    const M17_MONITOR_BODY: &[u8] =
        br#"{"action":"monitor","reflector":"M17-TEST","module":"A","callsign":"AJ7HR"}"#;

    // =========================================================================
    // POST /echolink — the node's EchoLink leg
    // =========================================================================

    #[test]
    fn post_echolink_needs_same_origin_json() {
        let c = test_controller();
        let body: &[u8] = br#"{"action":"dial","target":"9999"}"#;
        for ct in [None, Some("text/plain")] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/echolink",
                body,
                &RequestHeaders {
                    content_type: ct,
                    ..json_headers()
                },
            );
            assert_eq!(r.status, 415, "{ct:?}");
            assert_eq!(
                r.body,
                br#"{"error":"POST /echolink requires Content-Type: application/json"}"#
            );
        }
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/echolink",
            body,
            &RequestHeaders {
                origin: Some("https://evil.example"),
                ..json_headers()
            },
        );
        assert_eq!(r.status, 403, "a page on another origin gets nothing");
        // Its own origin reaches the controller, which refuses a node with
        // no [echolink] section.
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/echolink",
            body,
            &RequestHeaders {
                origin: Some("http://127.0.0.1:8730"),
                ..json_headers()
            },
        );
        assert_eq!(r.status, 500, "{:?}", json_of(&r));
        assert_eq!(
            json_of(&r),
            error_body(
                "EchoLink isn't enabled on this node — its operator can add an [echolink] section."
            )
        );
    }

    #[test]
    fn post_echolink_rejects_an_address_field() {
        let c = test_controller();
        for body in [
            br#"{"action":"dial","target":"9999","addr":"1.2.3.4"}"#.as_slice(),
            br#"{"action":"dial","target":"9999","host":"node.example"}"#.as_slice(),
            br#"{"action":"call","target":"9999"}"#.as_slice(),
            b"not json".as_slice(),
        ] {
            let r = handle_request_with_headers(&c, "POST", "/echolink", body, &json_headers());
            assert_eq!(r.status, 400, "{:?}", String::from_utf8_lossy(body));
            assert!(
                json_of(&r)["error"]
                    .as_str()
                    .is_some_and(|e| e.starts_with("bad request: ")),
                "{:?}",
                json_of(&r)
            );
        }
    }

    #[test]
    fn m17_post_without_a_json_content_type_is_415_and_links_nothing() {
        // A cross-origin fetch with mode 'no-cors' and a text/plain body is a
        // CORS "simple" request: no preflight, and its Host is this node's own.
        let rec =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        let c = m17_controller(rec.addr);
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/m17",
                M17_MONITOR_BODY,
                &RequestHeaders {
                    content_type: ct,
                    ..json_headers()
                },
            );
            assert_eq!(r.status, 415, "{ct:?} is refused: {:?}", json_of(&r));
            assert_eq!(
                r.body, br#"{"error":"POST /m17 requires Content-Type: application/json"}"#,
                "the 415 names the route that refused: {ct:?}"
            );
            assert!(
                status_json(&c)["m17"].is_null(),
                "no monitor exists after a refused {ct:?} POST: {}",
                status_json(&c)
            );
        }
        assert!(
            rec.datagrams().is_empty(),
            "nothing reached the reflector: {:?}",
            rec.datagrams()
        );
    }

    #[test]
    fn m17_post_from_a_foreign_origin_is_403_and_links_nothing() {
        let rec =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        let c = m17_controller(rec.addr);
        for body in [M17_MONITOR_BODY, br#"{"action":"disconnect"}"#.as_slice()] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/m17",
                body,
                &RequestHeaders {
                    origin: Some("https://evil.example"),
                    ..json_headers()
                },
            );
            assert_eq!(r.status, 403, "a page on another origin gets nothing");
            assert_eq!(
                r.body, br#"{"error":"cross-origin requests are not accepted on this port"}"#,
                "the refusal is the port's one sentence"
            );
        }
        assert!(
            status_json(&c)["m17"].is_null(),
            "no monitor exists after a refused cross-origin POST: {}",
            status_json(&c)
        );
        assert!(
            rec.datagrams().is_empty(),
            "nothing reached the reflector: {:?}",
            rec.datagrams()
        );
    }

    #[test]
    fn m17_post_from_its_own_origin_goes_through() {
        // The guard must not refuse the node's own pages: an Origin that
        // matches Host reaches the controller, which then refuses a disconnect
        // with nothing up — a 500 from the controller, not a 403 or 415.
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/m17",
            br#"{"action":"disconnect"}"#,
            &RequestHeaders {
                origin: Some("http://127.0.0.1:8730"),
                ..json_headers()
            },
        );
        assert_eq!(r.status, 500, "the controller answered: {:?}", json_of(&r));
        assert_eq!(json_of(&r), error_body("No M17 link is up."));
    }

    /// `POST /m17` says who will hear the reflector: three loopback links in
    /// transceive count, a monitor-only link does not (the conference never
    /// sends it anything), and `/status` carries the same warning.
    #[test]
    fn m17_monitor_reply_warns_and_counts_the_members_it_relays_to() {
        let rec =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        let c = m17_controller(rec.addr);
        // Kept alive to the end: dropping a socket closes its port, and ICMP
        // would tear the link down mid-test. Every address is 127.0.0.1.
        let mut sinks = Vec::new();
        for (node, action) in [
            ("1999", LinkAction::Connect),
            ("1998", LinkAction::Connect),
            ("1997", LinkAction::Connect),
            ("1996", LinkAction::Monitor),
        ] {
            let (sink, addr) = crate::test_support::dead_port();
            sinks.push(sink);
            c.execute(NodeCommand::Link {
                action,
                node: node.into(),
                addr: Some(addr),
            })
            .expect("a loopback link registers");
        }
        assert_eq!(c.snapshot().links.len(), 4);

        let r = m17_post(&c, std::str::from_utf8(M17_MONITOR_BODY).unwrap());
        assert_eq!(r.status, 200, "{:?}", json_of(&r));
        let v = json_of(&r);
        assert_eq!(v["ok"], serde_json::json!(true), "the old field stays");
        assert_eq!(
            v["relays_to"],
            serde_json::json!({ "allstar": 3, "echolink": 0 }),
            "three transceive links; the monitor-only link is never sent the mix"
        );
        let warning = v["warning"].as_str().expect("a warning string");
        assert!(
            warning.starts_with(crate::m17_relay::M17_RELAY_WARNING),
            "{warning}"
        );
        assert!(
            warning.contains("Right now that is 3 AllStar connections, and any of them with a radio will transmit what this reflector carries."),
            "{warning}"
        );

        let st = status_json(&c);
        assert_eq!(st["m17_relay"]["relays_to"], v["relays_to"]);
        assert_eq!(st["m17_relay"]["warning"], v["warning"]);

        c.teardown_m17();
        rec.stop();
        drop(sinks);
    }

    /// With M17 off, `/status` has no `m17_relay` at all, so the golden key
    /// lists and every client that never turned M17 on see nothing new.
    #[test]
    fn status_has_no_m17_relay_while_m17_is_off() {
        let c = test_controller();
        assert!(status_json(&c).get("m17_relay").is_none());
    }

    #[test]
    fn m17_monitor_links_by_name_and_presents_the_uppercased_identity() {
        let rec =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        let c = m17_controller(rec.addr);

        // Lowercase on purpose: name, module and callsign are all uppercased
        // before anything is looked up or sent.
        let r = m17_post(
            &c,
            r#"{"action":"monitor","reflector":"m17-test","module":"a","callsign":"aj7hr"}"#,
        );
        assert_eq!(r.status, 200, "monitor is accepted: {:?}", json_of(&r));
        assert_eq!(
            json_of(&r),
            serde_json::json!({
                "ok": true,
                "relays_to": { "allstar": 0, "echolink": 0 },
                "warning": crate::m17_relay::RelayCounts::default().warning(),
            })
        );

        assert!(
            crate::test_support::wait_until(|| first_conn(&rec).is_some(), 3000),
            "the monitor sent a CONN to the table's address: {:?}",
            rec.datagrams()
        );
        assert_eq!(
            first_conn(&rec),
            Some(astar_m17::ControlPacket::Conn {
                callsign: astar_m17::encode_callsign("AJ7HR").expect("AJ7HR encodes"),
                module: b'A',
            }),
            "the CONN names module A and callsign AJ7HR"
        );

        // /status renders the leg once the pump has seen the ACKN.
        assert!(
            crate::test_support::wait_until(
                || {
                    c.pump();
                    let v = status_json(&c);
                    v["m17"]["state"] == serde_json::json!("linked") && v["m17"]["up_secs"].is_u64()
                },
                3000,
            ),
            "the leg shows as linked with an up time: {}",
            status_json(&c)
        );
        let v = status_json(&c);
        let mut m17 = v["m17"].clone();
        // The only time-dependent field: pinned as a number above, then
        // normalised so the rest of the object is compared exactly.
        m17["up_secs"] = serde_json::json!(0);
        assert_eq!(
            m17,
            serde_json::json!({
                "reflector": "M17-TEST",
                "module": "A",
                "target": "M17-TEST A",
                "callsign": "AJ7HR",
                "state": "linked",
                "receiving": false,
                "talker": null,
                "talker_secs_ago": null,
                "up_secs": 0,
                "dropped_packets": 0,
                "directory_generated": null
            })
        );
        assert_eq!(v["m17_enabled"], serde_json::json!(true));
        let raw = v.to_string();
        for bad in ["secret", "password", "token"] {
            assert!(!raw.contains(bad), "status must not contain '{bad}': {raw}");
        }

        let d = m17_post(&c, r#"{"action":"disconnect"}"#);
        assert_eq!(d.status, 200, "disconnect is accepted: {:?}", json_of(&d));
        assert_eq!(json_of(&d), serde_json::json!({ "ok": true }));
        assert_eq!(
            status_json(&c).get("m17"),
            None,
            "no m17 object once the leg is down"
        );
        rec.stop();
    }

    #[test]
    fn m17_monitor_without_a_callsign_is_400_before_the_controller_is_asked() {
        // M17 is OFF on this controller. Reaching the controller would be
        // the 500 "isn't enabled" sentence; the shape check must answer first.
        let c = test_controller();
        let r = m17_post(
            &c,
            r#"{"action":"monitor","reflector":"M17-TEST","module":"A"}"#,
        );
        assert_eq!(r.status, 400);
        assert_eq!(
            json_of(&r),
            error_body("bad request: monitor needs reflector, module and callsign")
        );
        for body in [
            r#"{"action":"monitor","module":"A","callsign":"AJ7HR"}"#,
            r#"{"action":"monitor","reflector":"M17-TEST","callsign":"AJ7HR"}"#,
        ] {
            let r = m17_post(&c, body);
            assert_eq!(r.status, 400, "{body}");
            assert_eq!(
                json_of(&r),
                error_body("bad request: monitor needs reflector, module and callsign"),
                "{body}"
            );
        }
    }

    #[test]
    fn m17_module_must_be_exactly_one_character() {
        let c = test_controller();
        for module in ["AB", ""] {
            let body = format!(
                r#"{{"action":"monitor","reflector":"M17-TEST","module":"{module}","callsign":"AJ7HR"}}"#
            );
            let r = m17_post(&c, &body);
            assert_eq!(r.status, 400, "module {module:?}");
            assert_eq!(
                json_of(&r),
                error_body("bad request: module is one letter, A to Z"),
                "module {module:?}"
            );
        }
    }

    #[test]
    fn m17_has_no_transmit_action() {
        let c = test_controller();
        let r = m17_post(&c, r#"{"action":"transmit"}"#);
        assert_eq!(
            r.status, 400,
            "transmit must not deserialize into an action"
        );
        let err = json_of(&r)["error"]
            .as_str()
            .expect("error is a string")
            .to_string();
        // serde_json appends " at line L column C"; everything before it is pinned exactly.
        let msg = err.split(" at line ").next().expect("serde text");
        assert_eq!(
            msg,
            "bad request: unknown variant `transmit`, expected `monitor` or `disconnect`"
        );
    }

    #[test]
    fn m17_refuses_an_address_field_and_contacts_nobody() {
        let rec =
            crate::test_support::RecordingReflector::start(crate::test_support::FakeReply::Ackn);
        // M17 is enabled and M17-TEST resolves to the recorder, so the ONLY
        // thing standing between this body and a CONN is the refusal of `addr`.
        let c = m17_controller(rec.addr);
        let body = format!(
            r#"{{"action":"monitor","reflector":"M17-TEST","module":"A","callsign":"AJ7HR","addr":"{}"}}"#,
            rec.addr
        );
        let r = m17_post(&c, &body);
        assert_eq!(r.status, 400, "an addr field is a 400: {:?}", json_of(&r));
        let err = json_of(&r)["error"]
            .as_str()
            .expect("error is a string")
            .to_string();
        let msg = err.split(" at line ").next().expect("serde text");
        assert_eq!(
            msg,
            "bad request: unknown field `addr`, expected one of `action`, `reflector`, `module`, `callsign`"
        );
        assert!(
            !crate::test_support::wait_until(|| !rec.datagrams().is_empty(), 300),
            "nothing may reach the reflector: {:?}",
            rec.datagrams()
        );
        assert_eq!(status_json(&c).get("m17"), None, "no leg was recorded");
        rec.stop();
    }

    #[test]
    fn m17_bad_json_is_400() {
        let c = test_controller();
        let r = m17_post(&c, "not json");
        assert_eq!(r.status, 400);
        let err = json_of(&r)["error"]
            .as_str()
            .expect("error is a string")
            .to_string();
        // A parse failure, not one of the route's own shape sentences.
        let msg = err.split(" at line ").next().expect("serde text");
        assert_eq!(msg, "bad request: expected ident");
    }

    #[test]
    fn m17_monitor_on_a_node_without_m17_is_500_with_the_operator_sentence() {
        const OFF: &str =
            "M17 isn't enabled on this node — its operator can add an `[m17]` section.";
        let full = r#"{"action":"monitor","reflector":"M17-TEST","module":"A","callsign":"AJ7HR"}"#;

        // No [m17] section at all.
        let c = test_controller();
        let r = m17_post(&c, full);
        assert_eq!(r.status, 500);
        assert_eq!(json_of(&r), error_body(OFF));

        // A section with enabled = false.
        let mut c = test_controller();
        c.set_m17_config(Some(crate::m17_directory::M17Settings {
            enabled: false,
            reflectors: std::collections::BTreeMap::new(),
            directory: None,
        }));
        let r = m17_post(&c, full);
        assert_eq!(r.status, 500);
        assert_eq!(json_of(&r), error_body(OFF));
    }

    #[test]
    fn m17_disconnect_with_nothing_up_is_500() {
        let c = test_controller();
        // The other fields are ignored on disconnect, including a module
        // that would be a 400 on monitor.
        for body in [
            r#"{"action":"disconnect"}"#,
            r#"{"action":"disconnect","reflector":"M17-TEST","module":"AB"}"#,
        ] {
            let r = m17_post(&c, body);
            assert_eq!(r.status, 500, "{body}");
            assert_eq!(json_of(&r), error_body("No M17 link is up."), "{body}");
        }
    }

    #[test]
    fn status_carries_the_m17_flags_and_no_leg_when_idle() {
        let c = test_controller();
        let v = status_json(&c);
        assert_eq!(v["m17_enabled"], serde_json::json!(false), "{v}");
        // The Codec 2 dev-dependency override makes a decoder available in
        // every test build of this crate.
        assert_eq!(v["m17_available"], serde_json::json!(true), "{v}");
        assert_eq!(v.get("m17"), None, "no leg, no m17 object: {v}");
        let raw = v.to_string();
        for bad in ["secret", "password", "token"] {
            assert!(!raw.contains(bad), "status must not contain '{bad}': {raw}");
        }
    }

    #[test]
    fn the_status_page_has_the_m17_codec_badge_hidden_by_default() {
        let c = test_controller();
        let html =
            String::from_utf8(handle_request(&c, "GET", "/", b"").body).expect("the page is utf-8");
        assert!(
            html.contains(
                r#"<span class="badge err" id="badge-m17codec" hidden><i class="dot"></i>M17: no codec</span>"#
            ),
            "index carries the hidden M17 codec badge: {html}"
        );
    }

    #[test]
    fn the_status_script_renders_m17_through_text_only() {
        let c = test_controller();
        let js = String::from_utf8(handle_request(&c, "GET", "/app.js", b"").body)
            .expect("the script is utf-8");
        for needle in [
            "function m17Row(m)",
            "if (snap.m17) rows.push(m17Row(snap.m17));",
            r#"$("badge-m17codec").hidden = !(snap.m17_enabled && !snap.m17_available);"#,
            r#"case "m17":"#,
        ] {
            assert!(js.contains(needle), "app.js is missing {needle:?}");
        }
        // Reflector names, callsigns and end reasons reach this page from the
        // wire: no string may ever become markup.
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
        ] {
            assert!(!js.contains(forbidden), "app.js must not use {forbidden}");
        }
    }

    // =========================================================================
    // The Discord admin page (iax-c9f4)
    // =========================================================================

    /// The headers a `curl -H 'Content-Type: application/json'` sends on the
    /// admin POST: a loopback `Host`, no `Origin`.
    fn json_headers() -> RequestHeaders<'static> {
        RequestHeaders {
            host: Some("127.0.0.1:8730"),
            content_type: Some("application/json"),
            loopback_peer: true,
            ..RequestHeaders::default()
        }
    }

    fn admin_post(c: &NodeController, body: &str) -> HttpResponse {
        handle_request_with_headers(
            c,
            "POST",
            "/discord/admin",
            body.as_bytes(),
            &json_headers(),
        )
    }

    fn error_of(r: &HttpResponse) -> String {
        let v: serde_json::Value =
            serde_json::from_slice(&r.body).expect("a refusal is a JSON object");
        v["error"]
            .as_str()
            .unwrap_or_else(|| panic!("a refusal carries an error string: {v}"))
            .to_string()
    }

    // =========================================================================
    // The settings page's routes
    // =========================================================================

    /// A test controller with a `SettingsStore` over a fresh copy of the
    /// template in a scratch directory. The store's running mirror and the
    /// station's bridge are both set from that same template, the way
    /// `main` sets them, so "pending" starts empty and honest.
    fn settings_controller() -> (NodeController, tempfile::TempDir) {
        settings_controller_with("credentials.toml")
    }

    /// Same, with the credentials file at `credentials_file` relative to
    /// the scratch directory — a path under a directory that does not
    /// exist is how a test makes the credential write fail.
    fn settings_controller_with(credentials_file: &str) -> (NodeController, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let config_path = dir.path().join("node.toml");
        std::fs::write(&config_path, crate::template::NODE_TOML_TEMPLATE)
            .expect("the template is written");
        let cfg = crate::config::NodeFileConfig::from_toml_str(crate::template::NODE_TOML_TEMPLATE)
            .expect("the template parses");
        let c = test_controller();
        c.apply_bridge_config(
            cfg.to_bridge_config()
                .expect("the template's bridge is valid"),
        )
        .expect("the bridge follows the template, as it does at start-up");
        let mut c = c;
        c.set_settings_store(crate::settings_store::SettingsStore::new(
            config_path,
            dir.path().join(credentials_file),
            crate::settings::RedactedConfig::from_config(&cfg),
        ));
        (c, dir)
    }

    fn body_json(r: &HttpResponse) -> serde_json::Value {
        serde_json::from_slice(&r.body)
            .unwrap_or_else(|e| panic!("a JSON body: {e}: {:?}", String::from_utf8_lossy(&r.body)))
    }

    fn settings_state_of(c: &NodeController) -> serde_json::Value {
        let r = handle_request(c, "GET", "/settings/state", b"");
        assert_eq!(
            r.status, 200,
            "the state route on a node with a store: {r:?}"
        );
        body_json(&r)
    }

    fn etag_of_state(c: &NodeController) -> String {
        settings_state_of(c)["etag"]
            .as_str()
            .expect("state carries an etag")
            .to_string()
    }

    /// A POST the way the page (or `curl -H 'Content-Type: application/json'`)
    /// sends it: loopback `Host`, JSON content type, no `Origin`.
    fn settings_post(c: &NodeController, path: &str, body: &str) -> HttpResponse {
        handle_request_with_headers(c, "POST", path, body.as_bytes(), &json_headers())
    }

    const SETTINGS_POSTS: [&str; 4] = [
        "/settings",
        "/settings/credential",
        "/settings/portal/test",
        "/settings/restart",
    ];

    #[test]
    fn settings_routes_are_503_without_a_store() {
        // A test or embedded controller has no node.toml to edit. Every
        // settings route says so with one sentence, after the guard.
        let c = test_controller();
        let want = serde_json::json!({ "error": "settings are not available on this node" });
        let state = handle_request(&c, "GET", "/settings/state", b"");
        assert_eq!(state.status, 503, "{state:?}");
        assert_eq!(body_json(&state), want);
        for path in SETTINGS_POSTS {
            let r = settings_post(&c, path, "{}");
            assert_eq!(r.status, 503, "POST {path}: {r:?}");
            assert_eq!(body_json(&r), want, "POST {path}");
        }
        assert!(
            !c.should_stop(),
            "a 503 on the restart route did not shut anything down"
        );
    }

    #[test]
    fn every_settings_post_is_refused_cross_origin_before_anything_happens() {
        let (c, dir) = settings_controller();
        let before = std::fs::read(dir.path().join("node.toml")).expect("node.toml");
        let forbidden =
            serde_json::json!({ "error": "cross-origin requests are not accepted on this port" });
        for path in SETTINGS_POSTS {
            // A rebinding Host: Origin and Host agree, the name is wrong.
            let r = handle_request_with_headers(
                &c,
                "POST",
                path,
                b"{}",
                &RequestHeaders {
                    origin: Some("http://evil.com:8730"),
                    host: Some("evil.com:8730"),
                    content_type: Some("application/json"),
                    bound_to: None,
                    ..RequestHeaders::default()
                },
            );
            assert_eq!(r.status, 403, "POST {path} from a foreign Host: {r:?}");
            assert_eq!(body_json(&r), forbidden, "POST {path}");

            // A plain cross-origin fetch.
            let r = handle_request_with_headers(
                &c,
                "POST",
                path,
                b"{}",
                &RequestHeaders {
                    origin: Some("https://evil.example"),
                    ..json_headers()
                },
            );
            assert_eq!(r.status, 403, "POST {path} from a foreign Origin: {r:?}");
            assert_eq!(body_json(&r), forbidden, "POST {path}");

            // A "simple" request that cannot set application/json.
            let r = handle_request_with_headers(
                &c,
                "POST",
                path,
                b"{}",
                &RequestHeaders {
                    content_type: Some("text/plain"),
                    ..json_headers()
                },
            );
            assert_eq!(r.status, 415, "POST {path} without JSON: {r:?}");
            assert_eq!(
                body_json(&r),
                serde_json::json!({
                    "error": format!("POST {path} requires Content-Type: application/json")
                }),
                "the 415 names the route"
            );
        }
        assert!(!c.should_stop(), "no refusal reached the restart command");
        assert_eq!(
            std::fs::read(dir.path().join("node.toml")).expect("node.toml"),
            before,
            "no refusal reached the file"
        );
    }

    #[test]
    fn no_settings_route_sends_cors_headers() {
        // An Access-Control-Allow-Origin here would hand every page in the
        // operator's browser the node's config and credentials.
        let (c, _dir) = settings_controller();
        let mut responses = vec![handle_request(&c, "GET", "/settings/state", b"")];
        responses.extend(SETTINGS_POSTS.iter().map(|p| settings_post(&c, p, "{}")));
        for r in &responses {
            for (name, _) in &r.headers {
                assert!(
                    !name.to_ascii_lowercase().starts_with("access-control-"),
                    "no settings route may send a CORS header, got {name}: {r:?}"
                );
            }
        }
    }

    /// The `/status` substring assertion (`status_route_returns_200_secret_free`),
    /// copied onto the state route with every credential the node can hold
    /// set — and presence reported for each, as the strings "set"/"unset"
    /// and nothing else.
    #[test]
    fn settings_state_is_secret_free_with_every_credential_set_and_reports_presence() {
        let (c, dir) = settings_controller();
        let before = settings_state_of(&c);
        assert_eq!(
            before["credentials"],
            serde_json::json!({
                "node": "unset", "portal": "unset", "discord": "unset",
                "porkbun_pk": "unset", "porkbun_sk": "unset",
                "links": {}, "peers": {}
            })
        );

        // No [register] in the template, so the node password is keyed
        // under the literal "node". Each value is a sentinel the body is
        // then searched for.
        for (username, value) in [
            ("node", "SENTINEL-node-1a2b"),
            ("portal", "SENTINEL-portal-3c4d"),
            ("discord", "SENTINEL-bot-5e6f"),
            ("porkbun_pk", "SENTINEL-pk1-1f2e"),
            ("porkbun_sk", "SENTINEL-sk1-3d4c"),
            ("link:2000", "SENTINEL-link-7a8b"),
            ("allstar-public", "SENTINEL-peer-9c0d"),
        ] {
            c.execute(NodeCommand::ProvideSecret {
                username: username.into(),
                secret: value.into(),
            })
            .expect("the provider takes any username");
        }

        let r = handle_request(&c, "GET", "/settings/state", b"");
        assert_eq!(r.status, 200, "{r:?}");
        let body = String::from_utf8(r.body.clone()).expect("utf-8");
        for bad in ["secret", "password", "token", "SENTINEL"] {
            assert!(
                !body.contains(bad),
                "state body must not contain '{bad}': {body}"
            );
        }
        let v = body_json(&r);
        assert_eq!(
            v["credentials"],
            serde_json::json!({
                "node": "set", "portal": "set", "discord": "set",
                "porkbun_pk": "set", "porkbun_sk": "set",
                "links": { "2000": "set" },
                "peers": { "allstar-public": "set" }
            }),
            "{v}"
        );
        assert_eq!(v["credential_source"], serde_json::json!("file"));
        assert_eq!(
            v["credential_file"],
            serde_json::json!(dir.path().join("credentials.toml").display().to_string())
        );
        assert_eq!(
            v["live"],
            serde_json::json!([
                "bridge.",
                "parrot.",
                "audio.input",
                "audio.output",
                "m17.enabled",
                "m17.reflectors",
                "packet_log."
            ])
        );
        assert_eq!(
            v["pending"],
            serde_json::json!({ "restart": [], "apply": [] })
        );
        assert_eq!(
            v["restart"],
            serde_json::json!({ "busy": false, "why": null })
        );
        assert_eq!(v["node_id"], serde_json::Value::Null);
        assert_eq!(v["registered"], serde_json::json!(false));
        assert_eq!(
            v["feature"],
            serde_json::json!({ "discord": cfg!(feature = "discord") })
        );
        assert_eq!(
            v["file"],
            serde_json::json!({
                "path": dir.path().join("node.toml").display().to_string(),
                "writable": true
            })
        );
        assert_eq!(
            v["disk"]["control.bind"],
            serde_json::json!("127.0.0.1:8730")
        );
        assert_eq!(v["disk"], v["running"], "nothing has been edited yet");
    }

    #[test]
    fn a_credential_post_writes_the_file_then_the_provider_and_echoes_nothing() {
        let (c, dir) = settings_controller();
        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":"SENTINEL-hunter2-9f3a"}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            String::from_utf8(r.body).expect("utf-8"),
            r#"{"ok":true}"#,
            "the reply is bare: no key a value could ride on"
        );
        assert_eq!(c.secrets().resolve("portal"), "SENTINEL-hunter2-9f3a");
        let path = dir.path().join("credentials.toml");
        let file =
            std::fs::read_to_string(&path).expect("credentials.toml was created beside node.toml");
        assert!(
            file.contains("portal = \"SENTINEL-hunter2-9f3a\""),
            "the file holds the credential under its file key: {file}"
        );
        assert_eq!(
            crate::secrets::SecretProvider::file_keys(&path).expect("the file parses"),
            vec!["portal".to_string()],
            "and exactly that one key"
        );
        let state = handle_request(&c, "GET", "/settings/state", b"");
        let body = String::from_utf8(state.body).expect("utf-8");
        assert!(!body.contains("SENTINEL"), "presence only: {body}");
        assert_eq!(
            settings_state_of(&c)["credentials"]["portal"],
            serde_json::json!("set")
        );
    }

    /// The order is file first, provider second. Point the store at a
    /// credentials file under a directory that does not exist: the write
    /// fails, and the provider must still know nothing.
    #[test]
    fn a_credential_that_cannot_be_saved_never_reaches_the_provider() {
        let (c, _dir) = settings_controller_with("no-such-dir/credentials.toml");
        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":"SENTINEL-hunter2-9f3a"}"#,
        );
        assert_eq!(r.status, 500, "{r:?}");
        let err = error_of(&r);
        assert!(
            err.starts_with("credential \"portal\" could not be saved: "),
            "the failure names the credential: {err}"
        );
        assert!(!err.contains("SENTINEL"), "and never the value: {err}");
        assert_eq!(
            c.secrets().resolve("portal"),
            "",
            "a value the node could not keep is not one it is briefly using"
        );
    }

    #[test]
    fn forgetting_a_credential_clears_provider_file_and_state() {
        let (c, dir) = settings_controller();
        assert_eq!(
            settings_post(
                &c,
                "/settings/credential",
                r#"{"name":"portal","value":"SENTINEL-1"}"#
            )
            .status,
            200
        );
        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","forget":true}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(String::from_utf8(r.body).expect("utf-8"), r#"{"ok":true}"#);
        assert_eq!(c.secrets().resolve("portal"), "");
        assert_eq!(
            crate::secrets::SecretProvider::file_keys(&dir.path().join("credentials.toml"))
                .expect("the file still parses"),
            Vec::<String>::new(),
            "the key is gone from the file"
        );
        assert_eq!(
            settings_state_of(&c)["credentials"]["portal"],
            serde_json::json!("unset")
        );
    }

    /// Append a `[register]` with node number 12345 to the scratch node.toml,
    /// so the node password's provider key is the number, not `node`.
    fn register_as_12345(dir: &tempfile::TempDir) {
        let path = dir.path().join("node.toml");
        let before = std::fs::read_to_string(&path).expect("node.toml");
        let after =
            format!("{before}\n[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"12345\"\n");
        assert_eq!(
            crate::config::NodeFileConfig::from_toml_str(&after)
                .expect("loads")
                .register
                .expect("[register] took")
                .node_id,
            "12345"
        );
        std::fs::write(&path, &after).expect("rewrite node.toml");
    }

    /// A node password saved before the number (or
    /// before a renumbering) still sits under the literal "node" in the
    /// running provider. Forgetting the node password must clear that key
    /// as well as the number's, or the chip would read "set" from a value
    /// the page can no longer reach.
    #[test]
    fn forgetting_the_node_password_clears_both_provider_keys() {
        let (c, dir) = settings_controller();
        register_as_12345(&dir);
        // The stale value, as a password set before [register] existed.
        c.execute(NodeCommand::ProvideSecret {
            username: "node".into(),
            secret: "SENTINEL-old".into(),
        })
        .expect("the provider takes any username");

        assert_eq!(
            settings_post(
                &c,
                "/settings/credential",
                r#"{"name":"node","value":"SENTINEL-n"}"#
            )
            .status,
            200
        );
        assert_eq!(
            c.secrets().resolve("12345"),
            "SENTINEL-n",
            "keyed by the number"
        );
        assert_eq!(
            c.secrets().resolve("node"),
            "SENTINEL-old",
            "the stale key is still there"
        );

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"node","forget":true}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(c.secrets().resolve("12345"), "");
        assert_eq!(
            c.secrets().resolve("node"),
            "",
            "the stale key is forgotten too"
        );
        assert_eq!(c.secrets().usernames(), Vec::<String>::new());
        assert_eq!(
            settings_state_of(&c)["credentials"]["node"],
            serde_json::json!("unset")
        );
    }

    /// node.toml renumbered on disk to 777 while the process
    /// still runs as 12345, whose password it loaded at start. Presence
    /// counts the running id, so Forget must clear it too — otherwise a
    /// Forget that answered 200 would still read `set`, and the registrar
    /// would keep registering 12345 with the password the operator revoked.
    #[test]
    fn forgetting_the_node_password_clears_the_running_id_after_a_renumbering() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let config_path = dir.path().join("node.toml");
        let text = format!(
            "{}\n[register]\npeer = \"127.0.0.1:4569\"\nnode_id = \"777\"\n",
            crate::template::NODE_TOML_TEMPLATE
        );
        std::fs::write(&config_path, &text).expect("node.toml");
        let cfg = crate::config::NodeFileConfig::from_toml_str(&text).expect("loads");
        assert_eq!(
            cfg.register.as_ref().expect("[register] took").node_id,
            "777",
            "the fixture edit must take"
        );
        // The daemon as it started, before the renumbering: registered as
        // 12345. Constructing the controller binds nothing and dials nothing
        // (the same shape as controller.rs's
        // `snapshot_node_id_defaults_none_and_follows_register_username`).
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let mut c = NodeController::with_configs(
            station,
            SecretProvider::new(),
            astar_station::InboundConfig {
                bind: "127.0.0.1:0".parse().expect("a socket address"),
                ..astar_station::InboundConfig::default()
            },
            Some(astar_station::RegisterConfig {
                fallbacks: Vec::new(),
                peer: "127.0.0.1:4569".parse().expect("a socket address"),
                username: "12345".into(),
                refresh: std::time::Duration::from_secs(60),
            }),
            None,
            std::collections::HashMap::new(),
            false,
            3000,
        );
        c.set_settings_store(crate::settings_store::SettingsStore::new(
            config_path,
            dir.path().join("credentials.toml"),
            crate::settings::RedactedConfig::from_config(&cfg),
        ));
        assert_eq!(c.snapshot().node_id.as_deref(), Some("12345"));
        c.secrets().put("12345", "pw");
        assert_eq!(
            settings_state_of(&c)["credentials"]["node"],
            serde_json::json!("set"),
            "the running id's password counts before the forget"
        );

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"node","forget":true}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            c.secrets().resolve("12345"),
            "",
            "the running id is forgotten"
        );
        assert_eq!(c.secrets().usernames(), Vec::<String>::new());
        assert_eq!(
            settings_state_of(&c)["credentials"]["node"],
            serde_json::json!("unset")
        );
    }

    /// A peer named as the node's own
    /// number is the caller's mistake, refused with the store's sentence.
    #[test]
    fn a_peer_named_as_the_node_number_is_400() {
        let (c, dir) = settings_controller();
        register_as_12345(&dir);
        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"peer:12345","value":"x"}"#,
        );
        assert_eq!(r.status, 400, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "credential \"peer:12345\" is this node's own number — set it as \"node\""
            })
        );
        assert_eq!(
            c.secrets().usernames(),
            Vec::<String>::new(),
            "nothing was stored"
        );
    }

    /// A node.toml that does not parse is the node's fault, not the caller's,
    /// on the state route and the credential route alike.
    /// The sentence is the loader's own, so it is computed rather than
    /// copied.
    #[test]
    fn an_unreadable_node_toml_is_500_on_state_and_credential() {
        let (c, dir) = settings_controller();
        std::fs::write(dir.path().join("node.toml"), "[listener\n").expect("break node.toml");
        let want = serde_json::json!({
            "error": crate::config::NodeFileConfig::from_toml_str_redacted("[listener\n")
                .expect_err("an unterminated table header does not parse")
        });

        let state = handle_request(&c, "GET", "/settings/state", b"");
        assert_eq!(state.status, 500, "{state:?}");
        assert_eq!(body_json(&state), want);

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":"SENTINEL-x"}"#,
        );
        assert_eq!(r.status, 500, "{r:?}");
        assert_eq!(body_json(&r), want);
        assert!(
            !String::from_utf8_lossy(&r.body).contains("SENTINEL"),
            "{r:?}"
        );
        assert_eq!(c.secrets().resolve("portal"), "", "a 500 stores nothing");
    }

    /// A forget that cannot touch the file is a 500
    /// with the `could not be removed` prefix, and the running provider
    /// keeps the value — the file is the first thing a forget changes. A
    /// directory squatting on the credentials path makes the read fail.
    #[test]
    fn a_forget_that_cannot_touch_the_file_is_500_and_keeps_the_value() {
        let (c, dir) = settings_controller_with("creds-dir");
        let path = dir.path().join("creds-dir");
        std::fs::create_dir(&path).expect("a directory where the file would be");
        c.execute(NodeCommand::ProvideSecret {
            username: "portal".into(),
            secret: "SENTINEL-keep".into(),
        })
        .expect("the provider takes any username");

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","forget":true}"#,
        );
        assert_eq!(r.status, 500, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": format!(
                    "credential \"portal\" could not be removed: cannot read {}: {}",
                    path.display(),
                    std::fs::read_to_string(&path).expect_err("a directory is not a file")
                )
            })
        );
        assert_eq!(c.secrets().resolve("portal"), "SENTINEL-keep");
    }

    /// The migration off a non-file source, end to end through the route:
    /// the file is created, the source is flipped in place, and the running
    /// provider takes the value.
    #[test]
    fn a_credential_with_migrate_flips_the_source_to_file() {
        let (c, dir) = settings_controller();
        let config_toml = crate::template::NODE_TOML_TEMPLATE.replace(
            "source = \"file\"\n",
            "source = \"config\"\nsecret = \"\"\n",
        );
        assert_ne!(
            config_toml,
            crate::template::NODE_TOML_TEMPLATE,
            "the replace must take"
        );
        std::fs::write(dir.path().join("node.toml"), &config_toml).expect("rewrite node.toml");
        assert_eq!(
            settings_state_of(&c)["credential_source"],
            serde_json::json!("config")
        );

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":"SENTINEL-mig-1","migrate":true}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(String::from_utf8(r.body).expect("utf-8"), r#"{"ok":true}"#);
        assert_eq!(
            settings_state_of(&c)["credential_source"],
            serde_json::json!("file")
        );
        assert_eq!(
            crate::secrets::SecretProvider::file_keys(&dir.path().join("credentials.toml"))
                .expect("the file exists and parses"),
            vec!["portal".to_string()]
        );
        assert_eq!(c.secrets().resolve("portal"), "SENTINEL-mig-1");
        let text = std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml");
        assert!(
            text.contains("# Uncomment and fill in node_id to register"),
            "node.toml was edited in place, not regenerated: {text}"
        );
        assert!(
            text.contains("secret = \"\""),
            "the inline slot is blanked, not removed: {text}"
        );
    }

    /// Without `migrate` a non-file source keeps its source and writes no
    /// file: the credential is applied to the running node until restart.
    #[test]
    fn a_credential_without_migrate_on_a_config_source_is_applied_but_not_persisted() {
        let (c, dir) = settings_controller();
        let config_toml = crate::template::NODE_TOML_TEMPLATE.replace(
            "source = \"file\"\n",
            "source = \"config\"\nsecret = \"\"\n",
        );
        assert_ne!(
            config_toml,
            crate::template::NODE_TOML_TEMPLATE,
            "the replace must take"
        );
        std::fs::write(dir.path().join("node.toml"), &config_toml).expect("rewrite node.toml");
        assert_eq!(
            settings_state_of(&c)["credential_source"],
            serde_json::json!("config")
        );

        let r = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":"SENTINEL-mig-2"}"#,
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            settings_state_of(&c)["credential_source"],
            serde_json::json!("config")
        );
        assert!(
            !dir.path().join("credentials.toml").exists(),
            "no file is written under a non-file source without migrate"
        );
        assert_eq!(c.secrets().resolve("portal"), "SENTINEL-mig-2");
    }

    #[test]
    fn credential_requests_that_cannot_be_acted_on_are_400_and_change_nothing() {
        let (c, _dir) = settings_controller();
        let unknown = settings_post(&c, "/settings/credential", r#"{"name":"root","value":"x"}"#);
        assert_eq!(unknown.status, 400, "{unknown:?}");
        assert_eq!(
            body_json(&unknown),
            serde_json::json!({ "error": "unknown credential \"root\"" })
        );
        let empty = settings_post(&c, "/settings/credential", r#"{"name":"portal"}"#);
        assert_eq!(empty.status, 400, "{empty:?}");
        assert_eq!(
            body_json(&empty),
            serde_json::json!({ "error": "credential \"portal\" needs a value" })
        );
        let blank = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"portal","value":""}"#,
        );
        assert_eq!(
            body_json(&blank),
            body_json(&empty),
            "an empty string is not a value"
        );

        // An unparsable body gets one fixed sentence, never serde's — serde
        // quotes the scalar it choked on, and here that scalar can be the
        // credential.
        let want_bad = serde_json::json!({
            "error": "bad request: expected {\"name\": \"…\", \"value\": \"…\"}"
        });
        let junk = settings_post(&c, "/settings/credential", "not json");
        assert_eq!(junk.status, 400, "{junk:?}");
        assert_eq!(body_json(&junk), want_bad);
        // A numeric registration password sent as a JSON number, which a
        // hand-written script or a form-to-JSON shim will do.
        let numeric = settings_post(
            &c,
            "/settings/credential",
            r#"{"name":"node","value":424242}"#,
        );
        assert_eq!(numeric.status, 400, "{numeric:?}");
        assert_eq!(body_json(&numeric), want_bad);
        assert!(
            !String::from_utf8_lossy(&numeric.body).contains("424242"),
            "the value never comes back, whatever type it arrived as: {numeric:?}"
        );

        assert_eq!(
            c.secrets().usernames(),
            Vec::<String>::new(),
            "nothing was stored"
        );
    }

    #[test]
    fn the_credential_request_debug_redacts_the_value() {
        let req: CredentialRequest =
            serde_json::from_str(r#"{"name":"portal","value":"SENTINEL-9f3a"}"#)
                .expect("a valid body");
        assert_eq!(
            format!("{req:?}"),
            "CredentialRequest { name: \"portal\", value: <redacted>, forget: false, migrate: false }"
        );
    }

    fn patch_body(etag: &str, patch: serde_json::Value, acks: &[&str]) -> String {
        let mut body = serde_json::json!({ "etag": etag, "acknowledge": acks });
        body["patch"] = patch;
        body.to_string()
    }

    #[test]
    fn a_wildcard_control_bind_is_refused_outright() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        for wildcard in ["0.0.0.0:8730", "[::]:8730"] {
            let r = settings_post(
                &c,
                "/settings",
                &patch_body(
                    &etag,
                    serde_json::json!({ "control.bind": wildcard }),
                    &["control.bind.public"],
                ),
            );
            assert_eq!(r.status, 400, "{wildcard}: {r:?}");
            assert_eq!(
                body_json(&r),
                serde_json::json!({
                    "error": "control.bind: a wildcard bind (0.0.0.0 or ::) is refused — it exposes an unauthenticated keying port on every interface",
                    "field": "control.bind"
                }),
                "no acknowledgement is offered for a wildcard: {wildcard}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE,
            "nothing was written"
        );
    }

    #[test]
    fn a_lan_control_bind_needs_the_acknowledgement() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "control.bind": "192.168.1.9:8730" }),
                &[],
            ),
        );
        assert_eq!(r.status, 400, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "control.bind: a non-loopback bind needs the control.bind.public acknowledgement",
                "field": "control.bind",
                "acknowledge": "control.bind.public"
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE
        );
        assert_eq!(etag_of_state(&c), etag, "and the etag did not move");
    }

    #[test]
    fn a_lan_control_bind_is_accepted_with_the_acknowledgement_from_loopback() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "control.bind": "192.168.1.9:8730" }),
                &["control.bind.public"],
            ),
        );
        assert_eq!(r.status, 200, "{r:?}");
        let v = body_json(&r);
        let new_etag = v["etag"].as_str().expect("a new etag").to_string();
        assert_eq!(
            v,
            serde_json::json!({
                "ok": true,
                "applied": [],
                "on_restart": ["control.bind"],
                "etag": new_etag
            })
        );
        assert_ne!(new_etag, etag, "the bytes changed, so the etag did");
        let state = settings_state_of(&c);
        assert_eq!(state["etag"], serde_json::json!(new_etag));
        assert_eq!(
            state["disk"]["control.bind"],
            serde_json::json!("192.168.1.9:8730")
        );
        assert_eq!(
            state["running"]["control.bind"],
            serde_json::json!("127.0.0.1:8730")
        );
        assert_eq!(
            state["pending"],
            serde_json::json!({ "restart": ["control.bind"], "apply": [] }),
            "restart-tier, so it is pending until the re-exec"
        );
        let text = std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml");
        assert!(
            text.contains("# Uncomment and fill in node_id to register"),
            "comments survive an edit in place: {text}"
        );
        assert_eq!(
            crate::config::NodeFileConfig::from_toml_str(&text)
                .expect("the written file loads")
                .control
                .bind,
            "192.168.1.9:8730"
        );
    }

    /// A request from a non-loopback peer may never change the
    /// bind, acknowledgement or not. `bound_to` is that LAN address so the
    /// port-wide guard passes and the rule is what refuses.
    #[test]
    fn a_lan_control_bind_is_never_accepted_from_a_non_loopback_host() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        let bound: std::net::SocketAddr = "192.168.1.9:8730".parse().expect("a socket address");
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/settings",
            patch_body(
                &etag,
                serde_json::json!({ "control.bind": "192.168.1.9:8730" }),
                &["control.bind.public"],
            )
            .as_bytes(),
            &RequestHeaders {
                origin: None,
                host: Some("192.168.1.9:8730"),
                content_type: Some("application/json"),
                bound_to: Some(bound),
                ..RequestHeaders::default()
            },
        );
        assert_eq!(r.status, 400, "the rule, not the guard, refuses: {r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "control.bind: may only be changed by a request from this host (a loopback peer)",
                "field": "control.bind"
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE
        );
        // The same request with the same headers can still edit something
        // that is not the bind: the Host rule is about the bind alone.
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/settings",
            patch_body(&etag, serde_json::json!({ "listener.max_calls": 4 }), &[]).as_bytes(),
            &RequestHeaders {
                origin: None,
                host: Some("192.168.1.9:8730"),
                content_type: Some("application/json"),
                bound_to: Some(bound),
                ..RequestHeaders::default()
            },
        );
        assert_eq!(r.status, 200, "{r:?}");
    }

    /// `Host: localhost` is the caller's say-so. A request whose socket peer
    /// is not loopback may not change the bind, whatever Host it sends.
    #[test]
    fn a_localhost_host_from_a_lan_peer_cannot_change_the_control_bind() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/settings",
            patch_body(
                &etag,
                serde_json::json!({ "control.bind": "192.168.1.9:8730" }),
                &["control.bind.public"],
            )
            .as_bytes(),
            &RequestHeaders {
                loopback_peer: false,
                ..json_headers()
            },
        );
        assert_eq!(r.status, 400, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "control.bind: may only be changed by a request from this host (a loopback peer)",
                "field": "control.bind"
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE
        );
    }

    /// A `sudo -e` edit behind the page's back is a 409, never a clobber:
    /// the hand edit stays on disk byte for byte and nothing is applied.
    #[test]
    fn a_stale_etag_is_a_409_and_writes_nothing() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        let edited = format!(
            "{}\n# edited by hand while the page was open\n",
            crate::template::NODE_TOML_TEMPLATE
        );
        std::fs::write(dir.path().join("node.toml"), &edited).expect("the hand edit");
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "bridge.mode": "conference" }),
                &[],
            ),
        );
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({ "error": "node.toml changed since the page loaded", "conflict": true })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            edited,
            "the hand edit is untouched"
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Bridge,
            "and the live tier did not run"
        );
        // The page reloads, gets the new etag, and the same patch goes in.
        let fresh = etag_of_state(&c);
        assert_ne!(fresh, etag);
        assert_eq!(
            settings_post(
                &c,
                "/settings",
                &patch_body(
                    &fresh,
                    serde_json::json!({ "bridge.mode": "conference" }),
                    &[]
                )
            )
            .status,
            200
        );
    }

    #[test]
    fn a_live_save_of_bridge_mode_rewires_the_bridge_and_is_not_pending() {
        let (c, dir) = settings_controller();
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Bridge
        );
        let etag = etag_of_state(&c);
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "bridge.mode": "conference" }),
                &[],
            ),
        );
        assert_eq!(r.status, 200, "{r:?}");
        let v = body_json(&r);
        let new_etag = v["etag"].as_str().expect("a new etag").to_string();
        assert_eq!(
            v,
            serde_json::json!({
                "ok": true,
                "applied": ["bridge.mode"],
                "on_restart": [],
                "etag": new_etag
            })
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Conference,
            "the bridge was re-wired live"
        );
        let state = settings_state_of(&c);
        assert_eq!(
            state["disk"]["bridge.mode"],
            serde_json::json!("conference")
        );
        assert_eq!(
            state["running"]["bridge.mode"],
            serde_json::json!("conference")
        );
        assert_eq!(
            state["pending"],
            serde_json::json!({ "restart": [], "apply": [] }),
            "applied live means not pending: {state}"
        );
        let text = std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml");
        assert_eq!(
            crate::config::NodeFileConfig::from_toml_str(&text)
                .expect("the written file loads")
                .to_bridge_config()
                .expect("a valid bridge")
                .mode,
            astar_iax::BridgeMode::Conference
        );
    }

    /// The page's Apply on a
    /// live-tier field that was hand-edited on disk. The patch carries the
    /// file's own value, no byte changes, and the live tier still runs.
    #[test]
    fn a_hand_edited_bridge_mode_is_applied_by_a_patch_equal_to_disk() {
        let (c, dir) = settings_controller();
        let hand_edited = crate::template::NODE_TOML_TEMPLATE.replace(
            "mode                = \"bridge\"",
            "mode                = \"parrot\"",
        );
        assert_ne!(
            hand_edited,
            crate::template::NODE_TOML_TEMPLATE,
            "the fixture edit must take"
        );
        std::fs::write(dir.path().join("node.toml"), &hand_edited).expect("the hand edit");
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": [], "apply": ["bridge.mode"] }),
            "a hand-edited live field reads apply, not restart"
        );
        let etag = etag_of_state(&c);

        let r = settings_post(
            &c,
            "/settings",
            &patch_body(&etag, serde_json::json!({ "bridge.mode": "parrot" }), &[]),
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "ok": true,
                "applied": ["bridge.mode"],
                "on_restart": [],
                "etag": etag
            }),
            "no byte changed, so the etag is the same — and the path still applied"
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Parrot
        );
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": [], "apply": [] })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            hand_edited,
            "the file was not rewritten"
        );
    }

    /// The other live mechanism. The template's `[audio]` is commented out
    /// (template.rs:43), so this patch creates the table; the route then
    /// reads the whole table for `SetDevices` — a mutex write on the
    /// station, no hardware — and the path leaves `pending.apply`.
    #[test]
    fn a_live_save_of_audio_input_is_applied_and_not_pending() {
        let (c, dir) = settings_controller();
        assert_eq!(
            c.selected_devices_for_test(),
            (None, None),
            "the station starts on defaults"
        );
        let etag = etag_of_state(&c);
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(&etag, serde_json::json!({ "audio.input": "null-in" }), &[]),
        );
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(
            c.selected_devices_for_test(),
            (Some("null-in".to_string()), None),
            "the station, not just the file, took the devices — input as input, output unset"
        );
        let v = body_json(&r);
        let new_etag = v["etag"].as_str().expect("a new etag").to_string();
        assert_eq!(
            v,
            serde_json::json!({
                "ok": true,
                "applied": ["audio.input"],
                "on_restart": [],
                "etag": new_etag
            })
        );
        assert_ne!(new_etag, etag);
        let state = settings_state_of(&c);
        assert_eq!(state["disk"]["audio.input"], serde_json::json!("null-in"));
        assert_eq!(
            state["running"]["audio.input"],
            serde_json::json!("null-in")
        );
        assert_eq!(
            state["pending"],
            serde_json::json!({ "restart": [], "apply": [] }),
            "a device change applied live is not pending: {state}"
        );
        let text = std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml");
        let audio = crate::config::NodeFileConfig::from_toml_str(&text)
            .expect("the written file loads")
            .audio
            .expect("the patch created [audio]");
        assert_eq!(audio.input.as_deref(), Some("null-in"));
        assert_eq!(audio.output, None, "only the patched key was written");
    }

    #[test]
    fn a_live_and_a_restart_change_in_one_save_are_split_by_tier() {
        let (c, _dir) = settings_controller();
        let etag = etag_of_state(&c);
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "bridge.mix_minus": false, "listener.max_calls": 4 }),
                &[],
            ),
        );
        assert_eq!(r.status, 200, "{r:?}");
        let v = body_json(&r);
        assert_eq!(v["applied"], serde_json::json!(["bridge.mix_minus"]));
        assert_eq!(v["on_restart"], serde_json::json!(["listener.max_calls"]));
        assert!(!c.bridge_config_for_test().mix_minus);
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": ["listener.max_calls"], "apply": [] })
        );
    }

    #[test]
    fn an_unparsable_patch_body_is_400() {
        let (c, dir) = settings_controller();
        let r = settings_post(&c, "/settings", "not json");
        assert_eq!(r.status, 400, "{r:?}");
        // serde_json reads `n` as the start of `null` and stops at `o`.
        assert_eq!(
            body_json(&r),
            serde_json::json!({ "error": "bad request: expected ident at line 1 column 2" })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE
        );
    }

    /// `SaveError::Io` on the wire: a directory squatting on
    /// `node.toml.tmp` makes the atomic write fail
    /// after every check has passed. 500, the store's sentence, the file
    /// untouched, and the live tier never reached.
    #[test]
    fn a_save_that_cannot_write_is_500() {
        let (c, dir) = settings_controller();
        let etag = etag_of_state(&c);
        std::fs::create_dir(dir.path().join("node.toml.tmp")).expect("squat on the temp name");
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &etag,
                serde_json::json!({ "bridge.mode": "conference" }),
                &[],
            ),
        );
        assert_eq!(r.status, 500, "{r:?}");
        let err = error_of(&r);
        assert!(
            err.starts_with(&format!(
                "cannot write {}: ",
                dir.path().join("node.toml").display()
            )),
            "the store's Io sentence names the file: {err}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("node.toml")).expect("node.toml"),
            crate::template::NODE_TOML_TEMPLATE.as_bytes(),
            "the file is byte-identical to the template"
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Bridge,
            "a save that did not write applies nothing"
        );
    }

    /// A node.toml that no longer loads, under an etag that matches its
    /// bytes, is the loader's verdict: 400 with the loader's own sentence
    /// and no `field`, the file untouched, and the live tier never reached.
    #[test]
    fn a_node_toml_that_no_longer_loads_is_a_400_oracle_error() {
        let (c, dir) = settings_controller();
        let broken = "[listener\n";
        std::fs::write(dir.path().join("node.toml"), broken).expect("break node.toml");
        let r = settings_post(
            &c,
            "/settings",
            &patch_body(
                &crate::settings::etag_of(broken.as_bytes()),
                serde_json::json!({ "bridge.mode": "conference" }),
                &[],
            ),
        );
        assert_eq!(r.status, 400, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": crate::config::NodeFileConfig::from_toml_str_redacted(broken)
                    .expect_err("an unterminated table header does not parse")
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml"),
            broken,
            "the broken file is left as it was"
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Bridge,
            "a refused save applies nothing"
        );
    }

    /// The failure branch of the live tier, which no route test can reach:
    /// `store.save` runs the oracle, so a bridge.mode the bridge refuses
    /// never gets as far as `apply_live_tier` through `POST /settings`.
    /// Build the `SaveOutcome` by hand from a config that `toml::from_str`
    /// accepts and `to_bridge_config` does not — `BridgeCfg.mode` is a
    /// `String`, and only `from_toml_str` runs the validators (config.rs:645)
    /// — and the branch is exercised directly: nothing applied, the
    /// sentence carried, the running bridge untouched.
    #[test]
    fn a_live_apply_that_fails_reports_apply_error_and_applies_nothing() {
        let (c, _dir) = settings_controller();
        let bogus = crate::template::NODE_TOML_TEMPLATE.replace(
            "mode                = \"bridge\"",
            "mode                = \"bogus\"",
        );
        assert_ne!(
            bogus,
            crate::template::NODE_TOML_TEMPLATE,
            "the fixture edit must take"
        );
        let cfg: crate::config::NodeFileConfig =
            toml::from_str(&bogus).expect("deserialises without validators");
        let outcome = SaveOutcome {
            etag: String::new(),
            config: cfg,
            written: true,
            changed: vec!["bridge.mode".to_string()],
        };
        let live = apply_live_tier(&c, &outcome);
        assert_eq!(
            live.applied,
            Vec::<String>::new(),
            "a refused apply marks nothing"
        );
        assert_eq!(
            live.on_restart,
            Vec::<String>::new(),
            "bridge.mode is live-tier"
        );
        assert_eq!(
            live.apply_error,
            Some(
                "bridge.mode: unknown value \"bogus\" (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
                    .to_string()
            ),
            "the sentence is to_bridge_config's own"
        );
        assert_eq!(
            c.bridge_config_for_test().mode,
            astar_iax::BridgeMode::Bridge,
            "the running bridge is what the template set"
        );

        // And the 200 body `settings_save` builds from it carries the key.
        assert_eq!(
            save_reply("e1", live),
            serde_json::json!({
                "ok": true,
                "applied": [],
                "on_restart": [],
                "etag": "e1",
                "apply_error": "bridge.mode: unknown value \"bogus\" (expected \"handset\", \"bridge\", \"conference\", or \"parrot\")"
            })
        );
        // Without a failure the key is absent, not null.
        assert_eq!(
            save_reply(
                "e2",
                LiveOutcome {
                    applied: vec![],
                    on_restart: vec![],
                    apply_error: None,
                }
            ),
            serde_json::json!({ "ok": true, "applied": [], "on_restart": [], "etag": "e2" })
        );
    }

    // ---- the settings page: [m17] ---------------------------------------------
    //
    // Nothing here dials. Every POST /m17 below is refused by the controller's
    // pre-flight at step 1 (not enabled) or step 3 (unknown name), before any
    // socket exists, and the 127.0.0.1 table entries are only ever resolved,
    // never contacted.

    const M17_OFF: &str =
        "M17 isn't enabled on this node — its operator can add an `[m17]` section.";
    const M17_OPT_IN: &str = "m17.enabled: a save that creates [m17] must set m17.enabled too — the section turns M17 on by default";
    const M17_CODEC_ACK: &str = "m17.codec_dirs loads a libcodec2 from these directories into the node at its next start; it needs the m17.codec_dirs acknowledgement";

    fn unknown_reflector(name: &str) -> String {
        format!(
            "I don't know a reflector called {name}. The node's operator can add it under `[m17.reflectors]`."
        )
    }

    /// `settings_controller`, over the template plus `extra`, with `[m17]`
    /// installed the way `main.rs` installs it, so the running mirror and
    /// the controller's M17 state start from the same file.
    fn m17_settings_controller(extra: &str) -> (NodeController, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let text = format!("{}\n{extra}", crate::template::NODE_TOML_TEMPLATE);
        let config_path = dir.path().join("node.toml");
        std::fs::write(&config_path, &text).expect("the fixture is written");
        let cfg = crate::config::NodeFileConfig::from_toml_str(&text).expect("the fixture parses");
        let mut c = test_controller();
        c.apply_bridge_config(
            cfg.to_bridge_config()
                .expect("the template's bridge is valid"),
        )
        .expect("the bridge follows the file, as it does at start-up");
        c.set_m17_config(
            cfg.m17
                .as_ref()
                .map(crate::m17_directory::M17Settings::from_cfg)
                .transpose()
                .expect("the fixture's [m17] is valid"),
        );
        c.set_settings_store(crate::settings_store::SettingsStore::new(
            config_path,
            dir.path().join("credentials.toml"),
            crate::settings::RedactedConfig::from_config(&cfg),
        ));
        (c, dir)
    }

    /// `POST /settings` with the page's current etag.
    fn m17_save(
        c: &NodeController,
        patch: &serde_json::Value,
        acks: &[&str],
    ) -> (u16, serde_json::Value) {
        let body =
            serde_json::json!({ "etag": etag_of_state(c), "patch": patch, "acknowledge": acks });
        let r = settings_post(c, "/settings", &body.to_string());
        (r.status, body_json(&r))
    }

    /// Nothing about `[packet_log]` waits for a restart: the running node
    /// holds the new table when the save answers.
    #[test]
    fn saving_debug_mode_needs_no_restart() {
        let (c, _dir) = settings_controller();
        assert!(!c.packet_debug_mode());
        let (status, v) = m17_save(
            &c,
            &serde_json::json!({ "packet_log.debug_mode": true }),
            &[],
        );
        assert_eq!(status, 200, "{v}");
        let applied = v["applied"].as_array().expect("the paths applied");
        assert!(applied.contains(&"packet_log.debug_mode".into()), "{v}");
        assert_eq!(v["on_restart"], serde_json::json!([]));
        assert!(c.packet_debug_mode());
    }

    /// A save stops a capture, and `null` takes the whole table away.
    #[test]
    fn a_save_stops_a_capture_and_null_removes_the_table() {
        let (c, dir) = settings_controller();
        let logs = dir.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        let (status, v) = m17_save(
            &c,
            &serde_json::json!({
                "packet_log.debug_mode": true,
                "packet_log.dir": logs.to_str().unwrap(),
            }),
            &[],
        );
        assert_eq!(status, 200, "{v}");
        c.packet_capture(true).expect("the folder is usable");
        assert!(c.packet_log_status().capturing);

        let (status, v) = m17_save(
            &c,
            &serde_json::json!({ "packet_log.max_bytes": "2 GB" }),
            &[],
        );
        assert_eq!(status, 200, "{v}");
        assert!(!c.packet_log_status().capturing);
        assert_eq!(c.packet_log_dir().as_deref(), Some(logs.as_path()));

        let (status, v) = m17_save(&c, &serde_json::json!({ "packet_log": null }), &[]);
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["on_restart"], serde_json::json!([]));
        assert_eq!(c.packet_log_dir(), None);
    }

    /// `POST /m17` monitor. Every call in this module is refused before a
    /// socket opens.
    fn m17_dial(c: &NodeController, reflector: &str) -> (u16, serde_json::Value) {
        let body = serde_json::json!({
            "action": "monitor", "reflector": reflector, "module": "A", "callsign": "AJ7HR"
        });
        let r = settings_post(c, "/m17", &body.to_string());
        (r.status, body_json(&r))
    }

    fn running_m17(c: &NodeController) -> Option<crate::m17_directory::M17Settings> {
        c.m17
            .lock()
            .expect("m17 state mutex poisoned")
            .settings
            .clone()
    }

    fn disk_m17(dir: &tempfile::TempDir) -> Option<crate::config::M17Cfg> {
        let text = std::fs::read_to_string(dir.path().join("node.toml")).expect("node.toml reads");
        crate::config::NodeFileConfig::from_toml_str(&text)
            .expect("node.toml loads")
            .m17
    }

    fn table_entry(port: u16) -> crate::m17_directory::ReflectorEntry {
        crate::m17_directory::ReflectorEntry {
            host: "127.0.0.1".to_string(),
            port,
            modules: None,
        }
    }

    #[test]
    fn turning_m17_on_from_the_settings_page_applies_at_the_next_dial() {
        let (c, dir) = settings_controller();
        let file = dir.path().join("node.toml");
        let before = std::fs::read(&file).expect("node.toml reads");
        assert_eq!(
            m17_dial(&c, "M17-NOPE"),
            (500, serde_json::json!({ "error": M17_OFF })),
            "the template has no [m17]: M17 is off"
        );

        // The section is the opt-in, and it is on by default once it exists,
        // so a save that would create it has to say which it wants.
        assert_eq!(
            m17_save(
                &c,
                &serde_json::json!({ "m17.reflectors.\"M17-TEST\"": "127.0.0.1:40404" }),
                &[]
            ),
            (
                400,
                serde_json::json!({ "error": M17_OPT_IN, "field": "m17.enabled" })
            )
        );
        assert_eq!(
            std::fs::read(&file).expect("node.toml reads"),
            before,
            "a refused save writes nothing"
        );

        let got = m17_save(
            &c,
            &serde_json::json!({
                "m17.enabled": true,
                "m17.reflectors.\"M17-TEST\"": "127.0.0.1:40404"
            }),
            &[],
        );
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": ["m17.enabled", "m17.reflectors.\"M17-TEST\""],
                    "on_restart": [],
                    "etag": etag_of_state(&c),
                })
            )
        );
        let disk = disk_m17(&dir).expect("the section is on disk");
        assert!(disk.enabled);
        assert_eq!(
            disk.reflectors,
            std::collections::BTreeMap::from([(
                "M17-TEST".to_string(),
                "127.0.0.1:40404".to_string()
            )])
        );

        let running = running_m17(&c).expect("the section is running without a restart");
        assert!(running.enabled);
        assert_eq!(
            running.resolve("m17-test"),
            Some(table_entry(40404)),
            "the dial path resolves the new entry"
        );
        assert!(running.directory.is_none());
        assert!(c.snapshot().m17_enabled, "/status says so too");
        assert_eq!(
            m17_dial(&c, "M17-NOPE"),
            (
                500,
                serde_json::json!({ "error": unknown_reflector("M17-NOPE") })
            ),
            "the next dial gets past `enabled` to the table"
        );
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": [], "apply": [] }),
            "nothing waits: an empty codec_dirs is no change"
        );
    }

    #[test]
    fn turning_m17_off_or_dropping_a_reflector_applies_at_once() {
        let (c, dir) = m17_settings_controller(
            "[m17]\n[m17.reflectors]\n\"M17-TEST\" = \"127.0.0.1:40404\"\n\"M17-TWO\" = \"127.0.0.1:40405\"\n",
        );

        let got = m17_save(
            &c,
            &serde_json::json!({ "m17.reflectors.\"M17-TEST\"": null }),
            &[],
        );
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": ["m17.reflectors.\"M17-TEST\""],
                    "on_restart": [],
                    "etag": etag_of_state(&c),
                })
            )
        );
        let running = running_m17(&c).expect("still on");
        assert_eq!(running.resolve("M17-TEST"), None);
        assert_eq!(running.resolve("M17-TWO"), Some(table_entry(40405)));
        assert_eq!(
            m17_dial(&c, "M17-TEST"),
            (
                500,
                serde_json::json!({ "error": unknown_reflector("M17-TEST") })
            )
        );

        let got = m17_save(&c, &serde_json::json!({ "m17.enabled": false }), &[]);
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": ["m17.enabled"],
                    "on_restart": [],
                    "etag": etag_of_state(&c),
                })
            )
        );
        assert!(!running_m17(&c).expect("the section stays").enabled);
        assert!(!c.snapshot().m17_enabled);
        assert_eq!(
            m17_dial(&c, "M17-TWO"),
            (500, serde_json::json!({ "error": M17_OFF }))
        );

        let got = m17_save(&c, &serde_json::json!({ "m17": null }), &[]);
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": ["m17.enabled", "m17.reflectors.\"M17-TWO\""],
                    "on_restart": [],
                    "etag": etag_of_state(&c),
                })
            )
        );
        assert!(
            running_m17(&c).is_none(),
            "no section: off, the way a node without one is"
        );
        assert!(disk_m17(&dir).is_none());
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": [], "apply": [] })
        );
    }

    #[test]
    fn the_m17_directory_waits_for_a_restart_and_a_live_save_keeps_the_running_one() {
        // Running with the bundled snapshot; the file drops it and gains a
        // table entry in the same save.
        let (c, _dir) = m17_settings_controller("[m17]\ndirectory = \"bundled\"\n");
        assert_eq!(
            running_m17(&c).expect("on").directory_generated(),
            Some("2026-09-15")
        );
        let got = m17_save(
            &c,
            &serde_json::json!({
                "m17.directory": null,
                "m17.reflectors.\"M17-TEST\"": "127.0.0.1:40404"
            }),
            &[],
        );
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": ["m17.reflectors.\"M17-TEST\""],
                    "on_restart": ["m17.directory"],
                    "etag": etag_of_state(&c),
                })
            )
        );
        let running = running_m17(&c).expect("on");
        assert_eq!(
            running.directory_generated(),
            Some("2026-09-15"),
            "the directory is restart-tier: the running one stays"
        );
        assert!(
            running.resolve("M17-USA").is_some(),
            "names still resolve from it until the restart"
        );
        assert_eq!(running.resolve("M17-TEST"), Some(table_entry(40404)));
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": ["m17.directory"], "apply": [] })
        );

        // The other way round: a directory named on disk is not loaded by a
        // live save.
        let (c, _dir) = m17_settings_controller("[m17]\n");
        let got = m17_save(&c, &serde_json::json!({ "m17.directory": "bundled" }), &[]);
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": [],
                    "on_restart": ["m17.directory"],
                    "etag": etag_of_state(&c),
                })
            )
        );
        let running = running_m17(&c).expect("on");
        assert!(running.directory.is_none());
        assert_eq!(running.resolve("M17-USA"), None);
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": ["m17.directory"], "apply": [] })
        );
    }

    #[test]
    fn a_codec_dir_waits_for_its_acknowledgement_and_for_a_restart() {
        let (c, dir) = m17_settings_controller("[m17]\n");
        let file = dir.path().join("node.toml");
        let before = std::fs::read(&file).expect("node.toml reads");

        assert_eq!(
            m17_save(&c, &serde_json::json!({ "m17.codec_dirs": ["lib"] }), &[]),
            (
                400,
                serde_json::json!({
                    "error": "m17.codec_dirs: \"lib\" is not an absolute path",
                    "field": "m17.codec_dirs"
                })
            )
        );
        assert_eq!(
            m17_save(
                &c,
                &serde_json::json!({ "m17.codec_dirs": ["/opt/astar/lib"] }),
                &[]
            ),
            (
                400,
                serde_json::json!({
                    "error": M17_CODEC_ACK,
                    "field": "m17.codec_dirs",
                    "acknowledge": "m17.codec_dirs"
                })
            )
        );
        assert_eq!(
            std::fs::read(&file).expect("node.toml reads"),
            before,
            "neither refusal wrote a byte"
        );

        let got = m17_save(
            &c,
            &serde_json::json!({ "m17.codec_dirs": ["/opt/astar/lib"] }),
            &["m17.codec_dirs"],
        );
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": [],
                    "on_restart": ["m17.codec_dirs"],
                    "etag": etag_of_state(&c),
                })
            )
        );
        assert_eq!(
            disk_m17(&dir).expect("on disk").codec_dirs,
            vec!["/opt/astar/lib".to_string()]
        );
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": ["m17.codec_dirs"], "apply": [] })
        );
        let running = running_m17(&c).expect("on");
        assert!(running.enabled && running.reflectors.is_empty() && running.directory.is_none());

        // Emptying the list loads nothing new: no question, and nothing waits.
        let got = m17_save(&c, &serde_json::json!({ "m17.codec_dirs": [] }), &[]);
        assert_eq!(
            got,
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "applied": [],
                    "on_restart": ["m17.codec_dirs"],
                    "etag": etag_of_state(&c),
                })
            )
        );
        assert_eq!(
            settings_state_of(&c)["pending"],
            serde_json::json!({ "restart": [], "apply": [] })
        );
    }

    #[test]
    fn restart_is_409_with_a_link_up_and_200_with_disconnect() {
        let (c, _dir) = settings_controller();
        let (_guard, addr) = crate::test_support::dead_port();
        assert_eq!(
            handle_request(
                &c,
                "POST",
                "/link",
                format!(r#"{{"action":"connect","node":"55553","addr":"{addr}"}}"#).as_bytes(),
            )
            .status,
            200
        );
        assert_eq!(
            settings_state_of(&c)["restart"],
            serde_json::json!({ "busy": true, "why": "1 link" }),
            "the state route says so before the page even asks"
        );

        let r = settings_post(&c, "/settings/restart", "{}");
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "restart refused: this node is on a net (1 link)",
                "busy": true
            })
        );
        assert!(!c.should_stop());
        assert!(!c.restart_requested());

        let r = settings_post(&c, "/settings/restart", r#"{"disconnect":true}"#);
        assert_eq!(r.status, 200, "{r:?}");
        assert_eq!(String::from_utf8(r.body).expect("utf-8"), r#"{"ok":true}"#);
        assert!(c.should_stop(), "the restart begins as a shutdown");
        assert!(c.restart_requested(), "that ends in an exec");
    }

    #[test]
    fn restart_when_idle_is_200_and_sets_both_flags() {
        let (c, _dir) = settings_controller();
        let r = settings_post(&c, "/settings/restart", "{}");
        assert_eq!(r.status, 200, "{r:?}");
        assert!(c.should_stop());
        assert!(c.restart_requested());
    }

    #[test]
    fn restart_with_a_bad_body_is_400_and_does_nothing() {
        let (c, _dir) = settings_controller();
        let r = settings_post(&c, "/settings/restart", "");
        assert_eq!(
            r.status, 400,
            "an empty body is not the {{}} the page sends: {r:?}"
        );
        assert_eq!(
            body_json(&r),
            serde_json::json!({ "error": "bad request: EOF while parsing a value at line 1 column 0" })
        );
        assert!(!c.should_stop());
        assert!(!c.restart_requested());
    }

    /// No `[portal]` on the test station, so the mint fails before any
    /// HTTPS: hermetic, and the sentence is the station's own.
    #[test]
    fn portal_test_without_a_portal_is_409_with_the_portal_refusal() {
        let (c, _dir) = settings_controller();
        let r = settings_post(&c, "/settings/portal/test", "{}");
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(
            body_json(&r),
            serde_json::json!({
                "error": "portal/token error: no web-transceiver token in the portal response (bad credentials, no WT access for this node, or markup changed)"
            })
        );
        let bare = settings_post(&c, "/settings/portal/test", "");
        assert_eq!(bare.status, 409, "an empty body is accepted too: {bare:?}");
        assert_eq!(body_json(&bare), body_json(&r), "and gets the same refusal");
        let junk = settings_post(&c, "/settings/portal/test", "not json");
        assert_eq!(junk.status, 400, "{junk:?}");
        assert_eq!(
            body_json(&junk),
            serde_json::json!({ "error": "bad request: the body is not JSON" })
        );
    }

    // ---- assets --------------------------------------------------------------

    #[test]
    fn the_admin_page_and_its_script_are_served() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/discord/admin", b"");
        assert_eq!(page.status, 200);
        assert!(
            page.content_type.starts_with("text/html"),
            "content type: {}",
            page.content_type
        );
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        assert!(
            html.contains("src=\"admin.js\"") && html.contains("href=\"../style.css\""),
            "the page loads its own script and the shared stylesheet: {html}"
        );
        assert!(
            html.contains("id=\"list-pending\""),
            "the page has the pending list the script fills: {html}"
        );

        let js = handle_request(&c, "GET", "/discord/admin.js", b"");
        assert_eq!(js.status, 200);
        assert!(
            js.content_type.contains("javascript"),
            "content type: {}",
            js.content_type
        );
        assert!(
            String::from_utf8(js.body)
                .expect("the script is utf-8")
                .contains("/discord/state"),
            "the script polls the state route"
        );
    }

    // ---- the settings page ---------------------------------------------------

    #[test]
    fn the_settings_page_and_its_script_are_served() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        assert_eq!(page.status, 200);
        assert_eq!(page.content_type, "text/html; charset=utf-8");
        assert_eq!(
            page.body,
            SETTINGS_HTML.as_bytes(),
            "the route serves the embedded page byte for byte"
        );
        assert_eq!(
            page.headers,
            vec![
                ("Content-Security-Policy", CSP.to_string()),
                ("X-Content-Type-Options", "nosniff".to_string()),
            ],
            "the page is served under the same policy as the other two"
        );
        for needle in [
            "id=\"card-identity\"",
            "id=\"badge-restart\"",
            "id=\"badge-registered\"",
            "src=\"settings.js\"",
            "href=\"style.css\"",
        ] {
            assert!(
                SETTINGS_HTML.contains(needle),
                "settings.html lacks {needle}"
            );
        }

        let js = handle_request(&c, "GET", "/settings.js", b"");
        assert_eq!(js.status, 200);
        assert_eq!(js.content_type, "application/javascript; charset=utf-8");
        assert_eq!(js.body, SETTINGS_JS.as_bytes());
        assert_eq!(js.headers, page.headers);
        for needle in [
            "fetch(\"settings/state\"",
            "postJson(\"settings\", {",
            "method: \"POST\"",
        ] {
            assert!(SETTINGS_JS.contains(needle), "settings.js lacks {needle}");
        }
    }

    /// The pages' half of the CSP bargain. The policy refuses inline script
    /// and style; what it cannot see is a string off the wire — a device
    /// name, an error sentence — handed to a sink that parses HTML. So no
    /// page script may name one, and no page may carry the inline script
    /// or style the policy would have refused anyway.
    #[test]
    fn the_page_scripts_never_build_markup() {
        for (name, js) in [
            ("settings.js", SETTINGS_JS),
            ("discord.js", DISCORD_JS),
            ("app.js", APP_JS),
            ("auth.js", AUTH_JS),
        ] {
            for sink in [
                "innerHTML",
                "outerHTML",
                "insertAdjacentHTML",
                "document.write",
                "createContextualFragment",
            ] {
                assert!(!js.contains(sink), "{name} reaches for {sink}");
            }
        }
        for (name, html) in [
            ("settings.html", SETTINGS_HTML),
            ("discord.html", DISCORD_HTML),
            ("index.html", INDEX_HTML),
        ] {
            // Two scripts: the sign-in chip's, then the page's own. Each is
            // `<script src="…"></script>`, never a block of inline code.
            assert_eq!(
                html.matches("<script").count(),
                2,
                "{name} has the sign-in script and its own"
            );
            for (i, _) in html.match_indices("<script") {
                let tag = &html[i..];
                let end = tag.find("</script>").expect("closed");
                assert!(
                    tag.starts_with("<script src=\"") && tag[..end].ends_with("\">"),
                    "{name}: a script tag with inline code near byte {i}"
                );
            }
            assert!(
                !html.contains(" style=\""),
                "{name} carries an inline style"
            );
            // An inline handler is ` on<letters>=`: `onclick="…"`, `onload="…"`.
            // Prose containing " on" — "every caller on the internet" — is not.
            for (i, _) in html.match_indices(" on") {
                let rest = &html[i + 3..];
                let name_len = rest.chars().take_while(char::is_ascii_lowercase).count();
                assert!(
                    name_len == 0 || !rest[name_len..].starts_with('='),
                    "{name} carries an inline event handler near byte {i}"
                );
            }
        }
    }

    /// The other two pages used to end in "edit node.toml and restart". Now
    /// they point at the page that does it, and the sentence that said
    /// approvers cannot be changed from here is gone.
    #[test]
    fn the_other_pages_link_to_the_settings_page() {
        assert!(
            INDEX_HTML.contains("<a class=\"manage\" href=\"settings\">Settings &rarr;</a>"),
            "the node page links to settings from its header"
        );
        assert!(
            DISCORD_HTML.contains("href=\"../settings#discord\""),
            "the approvers hint links to the Discord card"
        );
        assert!(
            !DISCORD_HTML.contains("They can't be changed from here"),
            "the hint no longer says approvers are out of reach"
        );
        assert!(
            DISCORD_JS.contains("\"../settings#discord\""),
            "the setup card links to the Discord card"
        );
        assert!(
            !DISCORD_JS.contains("approvers = ["),
            "the setup card no longer tells the operator to hand-edit node.toml"
        );
    }

    #[test]
    fn served_pages_carry_a_content_security_policy() {
        let c = test_controller();
        for route in ["/", "/discord/admin", "/settings"] {
            let r = handle_request(&c, "GET", route, b"");
            let csp = r
                .headers
                .iter()
                .find(|(name, _)| *name == "Content-Security-Policy")
                .map_or_else(
                    || panic!("{route} must carry a CSP: {:?}", r.headers),
                    |(_, v)| v.clone(),
                );
            assert!(
                csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"),
                "{route} CSP is too weak: {csp}"
            );
            assert!(
                r.headers
                    .iter()
                    .any(|(n, v)| *n == "X-Content-Type-Options" && v == "nosniff"),
                "{route} must be served nosniff: {:?}",
                r.headers
            );
        }
    }

    // ---- the settings page, part B: cards 4–7, credentials, restart ---------

    #[test]
    fn the_settings_script_drives_the_credential_portal_and_restart_routes() {
        let c = test_controller();
        let js = handle_request(&c, "GET", "/settings.js", b"");
        assert_eq!(js.status, 200);
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "\"settings/credential\"",
            "\"settings/portal/test\"",
            "\"settings/restart\"",
            "beforeunload",
            "Test the running credentials",
            "Portal credentials work.",
            "Move credentials to credentials.toml",
            "Restart now? This node is on a net — the calls and links will drop.",
            "The node did not come back — a config that fails at startup exits non-zero; check the log",
            "Applied until restart — this node reads credentials from the environment.",
            "Applied until restart — this node reads credentials from node.toml.",
            "Applied until restart — this node reads credentials from the control port.",
            // Esc cancels an open confirm; ⌘/Ctrl+S saves the
            // focused card. Both live in one keydown listener, and without
            // these two needles deleting that listener leaves every test green.
            "e.key === \"Escape\"",
            "(e.metaKey || e.ctrlKey)",
        ] {
            assert!(js.contains(needle), "settings.js must contain {needle:?}");
        }
        // The Restart button is disabled for its POST, and a disabled button
        // drops focus to <body>. The busy branch focuses it again before
        // opening the confirm, which records the focused element as the place
        // Esc and Cancel return to; without this the keyboard user lands on
        // <body>.
        assert!(
            js.contains(
                "btn.focus({ preventScroll: true });\n    openConfirm(btn.closest(\".row-actions\"), RESTART_CONFIRM,"
            ),
            "requestRestart must refocus the Restart button before the busy confirm opens"
        );
        // Every unbound input is marked data-no-patch: the password input in
        // credentialRow, the two add-row inputs in linkAddRow, the two in
        // reflectorAddRow, and the chip list's text input. A count, not
        // contains(): the chip list alone would satisfy a contains(), and then
        // dropping the mark from the one input that holds a password would
        // leave this test green.
        assert_eq!(
            js.matches("dataset.noPatch = \"1\"").count(),
            6,
            "credentialRow, linkAddRow (2), reflectorAddRow (2) and chipList each mark their unbound input"
        );
        // collectPatch walks bound controls only, rooted at the card body;
        // nothing in the file walks bare inputs or selects, where a password
        // input would be picked up.
        assert!(
            js.contains("bodyOf(card).querySelectorAll(\"[data-path][data-kind]\")"),
            "collectPatch must walk [data-path][data-kind] under #body-<card>"
        );
        for bare in [
            "querySelectorAll(\"input\"",
            "querySelectorAll(\"input,",
            "querySelectorAll(\"select\"",
            "querySelectorAll(\"textarea\"",
        ] {
            assert!(!js.contains(bare), "settings.js must not walk {bare:?}");
        }
        // The two reserved [discord] keys are parsed and preserved by
        // toml_edit, and never offered for editing. The bare quoted tokens
        // are checked as well as the dotted paths, because this file builds
        // paths by concatenation and `"discord." + "model"` would pass the
        // first check alone.
        for absent in [
            "discord.model",
            "discord.latency",
            "\"model\"",
            "\"latency\"",
        ] {
            assert!(
                !js.contains(absent),
                "settings.js must not offer {absent:?}"
            );
        }
    }

    #[test]
    fn the_settings_script_asks_before_every_acknowledged_change() {
        let c = test_controller();
        let js = handle_request(&c, "GET", "/settings.js", b"");
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "It must not be reached by binding [control] to a public address. Bind to ",
            "auth = \"off\" with no allowed nodes admits every caller on the internet. Turn authentication off anyway?",
            "DTMF commands let any connected member link and unlink this node. Enable them?",
            "This puts the local microphone on the air. Include the local radio?",
            "The transmitter will key to send a CW ID even when nothing is connected. Allow that?",
            "Without a hard time-out an open mic stays open until the holder unkeys. Remove the time-out?",
            "Removing an approver leaves their slash commands registered in Discord until you remove them there. Remove anyway?",
            "At its next start the node will load and run a libcodec2 it finds in ",
            ". List only directories nobody else can write to. Load code from there?",
        ] {
            assert!(js.contains(needle), "settings.js must ask {needle:?}");
        }
        for key in [
            "control.bind.public",
            "listener.auth.off",
            "dtmf.enabled",
            "bridge.include_local_radio",
            "announce.cw_keys_when_idle",
            "discord.mic_timeout.off",
            "discord.approvers.remove",
            "m17.codec_dirs",
            "vocoder.trusted_serials",
        ] {
            assert!(
                js.contains(&format!("\"{key}\":")),
                "settings.js must map the acknowledgement key {key:?} to its question"
            );
        }
    }

    #[test]
    fn the_settings_page_has_the_later_cards_the_restart_button_and_a_write_only_password_field() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        assert_eq!(page.status, 200);
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        for needle in [
            "id=\"card-announce\"",
            "id=\"card-discord\"",
            "id=\"card-linking\"",
            "id=\"card-advanced\"",
            "id=\"btn-restart\"",
            "id=\"restart-status\"",
            "id=\"tpl-credential\"",
            "autocomplete=\"new-password\"",
            ">Restart node<",
        ] {
            assert!(html.contains(needle), "settings.html must contain {needle}");
        }
        assert!(
            !html.contains("<script>"),
            "no inline script: the CSP forbids it and the page builds DOM only"
        );
    }

    #[test]
    fn the_settings_page_has_an_m17_card_that_keeps_the_opt_in() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        assert_eq!(page.status, 200);
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        for needle in [
            "<section class=\"card\" id=\"card-m17\" hidden>",
            "<a id=\"m17\"></a>",
            "<div class=\"card-head\"><h2>M17</h2></div>",
            "<div id=\"body-m17\"></div>",
            "<button type=\"button\" class=\"btn\" id=\"save-m17\" disabled>Save</button>",
        ] {
            assert!(html.contains(needle), "settings.html must contain {needle}");
        }
        let at = |id: &str| html.find(id).expect("the card is on the page");
        assert!(
            at("id=\"card-linking\"") < at("id=\"card-m17\"")
                && at("id=\"card-m17\"") < at("id=\"card-advanced\""),
            "the M17 card sits between Linking and Advanced"
        );
        assert!(
            at("id=\"card-m17\"") < at("id=\"card-public\"")
                && at("id=\"card-public\"") < at("id=\"card-advanced\""),
            "the Public page card ([web], [stream]) sits between M17 and Advanced, in node.toml order"
        );

        let js = handle_request(&c, "GET", "/settings.js", b"");
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "const CARD_NAMES = [\"identity\", \"listener\", \"audio\", \"announce\", \"discord\", \"linking\", \"m17\", \"vocoder\", \"public\", \"recording\", \"packet_log\", \"advanced\"];",
            "  m17: renderCardM17,",
            "  m17: m17Patch,",
            "patch[\"m17.enabled\"] = false;",
            "Off: nobody can dial M17 from Discord or POST /m17. A dial presents the dialler's callsign to someone else's reflector.",
            "Applies to the next dial. A monitor that is already up stays up until it is hung up.",
            "The node loads libcodec2 from these directories at its next start. Absolute paths only, and only directories nobody else can write to.",
            "A reflector name is letters, digits and '-' only.",
            "A reflector name is 3 to 8 characters.",
            "IPv6 literals are not supported",
            "host is empty",
        ] {
            assert!(js.contains(needle), "settings.js must contain {needle:?}");
        }
        // [m17] has no callsign, transmit, ptt or arm key and the page must
        // not invent one; the bare quoted tokens catch a path built by
        // concatenation.
        for absent in [
            "m17.callsign",
            "m17.transmit",
            "m17.ptt",
            "\"callsign\"",
            "\"transmit\"",
        ] {
            assert!(
                !js.contains(absent),
                "settings.js must not offer {absent:?}"
            );
        }
    }

    #[test]
    fn the_settings_page_has_a_vocoder_card_that_asks_before_trusting_a_stick() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        assert_eq!(page.status, 200);
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        for needle in [
            "<section class=\"card\" id=\"card-vocoder\" hidden>",
            "<a id=\"vocoder\"></a>",
            "<div class=\"card-head\"><h2>Vocoders</h2></div>",
            "<div id=\"body-vocoder\"></div>",
            "<button type=\"button\" class=\"btn\" id=\"save-vocoder\" disabled>Save</button>",
        ] {
            assert!(html.contains(needle), "settings.html must contain {needle}");
        }
        let at = |id: &str| html.find(id).expect("the card is on the page");
        assert!(
            at("id=\"card-m17\"") < at("id=\"card-vocoder\"")
                && at("id=\"card-vocoder\"") < at("id=\"card-public\""),
            "the Vocoders card sits between M17 and Public page, in node.toml order"
        );

        let js = handle_request(&c, "GET", "/settings.js", b"");
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "  vocoder: renderCardVocoder,",
            "field(card, \"vocoder.trusted_serials\", {",
            "\"vocoder.trusted_serials\": (patch) =>",
            "Opening a radio's CAT/PTT cable keys its transmitter, and this page cannot check — astar-server vocoder add can. Trust it anyway?",
            "astar-server vocoder add <serial>",
        ] {
            assert!(js.contains(needle), "settings.js must contain {needle:?}");
        }
        // Nothing on the page may widen the list beyond the serials written.
        for absent in ["vocoder.port", "vocoder.scan", "vocoder.any"] {
            assert!(
                !js.contains(absent),
                "settings.js must not offer {absent:?}"
            );
        }
    }

    #[test]
    fn the_settings_page_has_a_recordings_card_after_the_public_page() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        for needle in [
            "<section class=\"card\" id=\"card-recording\" hidden>",
            "<div class=\"card-head\"><h2>Recordings</h2></div>",
            "<div id=\"body-recording\"></div>",
            "<button type=\"button\" class=\"btn\" id=\"save-recording\" disabled>Save</button>",
        ] {
            assert!(html.contains(needle), "settings.html must contain {needle}");
        }
        let at = |id: &str| html.find(id).expect("the card is on the page");
        assert!(
            at("id=\"card-public\"") < at("id=\"card-recording\"")
                && at("id=\"card-recording\"") < at("id=\"card-advanced\""),
            "the Recordings card sits between Public page and Advanced, in node.toml order"
        );
        let js = handle_request(&c, "GET", "/settings.js", b"");
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "  recording: renderCardRecording,",
            "\"recording.enabled\"",
            "\"recording.dir\"",
            "\"recording.max_bytes\"",
            "\"recording.max_age_days\"",
            "\"recording.frames\"",
            "Off by default. Recordings include everything the conference carries, Discord too.",
            "Oldest recordings are deleted first. For example 2 GB.",
            "\"Save codec frames\"",
        ] {
            assert!(js.contains(needle), "settings.js must contain {needle:?}");
        }
    }

    #[test]
    fn the_settings_page_has_a_packet_log_card() {
        let c = test_controller();
        let page = handle_request(&c, "GET", "/settings", b"");
        let html = String::from_utf8(page.body).expect("the page is utf-8");
        for needle in [
            "<section class=\"card\" id=\"card-packet_log\" hidden>",
            "<div class=\"card-head\"><h2>Packet log</h2></div>",
            "<div id=\"body-packet_log\"></div>",
            "<button type=\"button\" class=\"btn\" id=\"save-packet_log\" disabled>Save</button>",
        ] {
            assert!(html.contains(needle), "settings.html must contain {needle}");
        }
        let at = |id: &str| html.find(id).expect("the card is on the page");
        assert!(
            at("id=\"card-recording\"") < at("id=\"card-packet_log\"")
                && at("id=\"card-packet_log\"") < at("id=\"card-advanced\""),
            "the Packet log card sits between Recordings and Advanced, in node.toml order"
        );
        let js = handle_request(&c, "GET", "/settings.js", b"");
        let js = String::from_utf8(js.body).expect("the script is utf-8");
        for needle in [
            "  packet_log: renderCardPacketLog,",
            "\"packet_log.debug_mode\"",
            "\"packet_log.dir\"",
            "\"packet_log.max_bytes\"",
            "Debug mode",
            "Off by default. Adds a Capture packets button to the Recordings page. Nothing is captured until you press it, and a capture holds other people's voices. Takes effect when you save, and saving this card stops a capture in progress.",
            "label: \"Folder\"",
            "label: \"Keep at most\"",
            "Oldest logs are deleted first. For example 1 GB.",
        ] {
            assert!(js.contains(needle), "settings.js must contain {needle:?}");
        }
        assert!(!js.contains("Log received packets"));
    }

    #[test]
    fn the_recordings_page_has_a_capture_button_beside_the_downloads() {
        let at = |id: &str| RECORDINGS_HTML.find(id).expect("on the page");
        assert!(
            at("id=\"packet-capture\"") < at("id=\"download-all\"")
                && at("id=\"badge-capture\"") < at("id=\"download-all\""),
            "the capture controls sit before Download all, in .badges"
        );
        assert!(at("class=\"badges\"") < at("id=\"packet-capture\""));
        assert!(RECORDINGS_HTML.contains("id=\"packet-capture\" aria-pressed=\"false\" hidden"));
    }

    #[test]
    fn the_capture_button_posts_json_and_says_what_it_is_doing() {
        for needle in [
            "packet-logs/capture",
            "Capture packets",
            "Stop capture",
            "Capturing packets",
            "aria-pressed",
            "announce(",
            "Capturing every live call until you press Stop capture",
            "Could not change the capture: ",
        ] {
            assert!(
                RECORDINGS_JS.contains(needle) || RECORDINGS_HTML.contains(needle),
                "the capture button needs {needle:?}"
            );
        }
    }

    /// A log's name came off the wire: the card shows it through `el` and
    /// no line of the page's script can turn a string into markup.
    #[test]
    fn the_recordings_page_lists_packet_logs_as_text() {
        let at = |id: &str| RECORDINGS_HTML.find(id).expect("on the page");
        assert!(
            at("id=\"card-list\"") < at("id=\"card-packet-logs\"")
                && at("id=\"card-packet-logs\"") < at("id=\"player\""),
            "the Packet logs card sits under Overs"
        );
        for needle in [
            "<section class=\"card\" id=\"card-packet-logs\" hidden>",
            "<h2>Packet logs</h2>",
            ">Capturing every live call until you press Stop capture<",
            "id=\"packet-error\"",
        ] {
            assert!(RECORDINGS_HTML.contains(needle), "{needle}");
        }
        let start = RECORDINGS_JS
            .find("function renderPacketLogs")
            .expect("the card's renderer");
        let tail = &RECORDINGS_JS[start..];
        let card = &tail[..tail.find("\n}\n").expect("its end")];
        for needle in [
            "$(\"packet-error\").textContent = error",
            "!debug && rows.length === 0 && !error",
        ] {
            assert!(card.contains(needle), "renderPacketLogs needs {needle}");
        }
        let start = RECORDINGS_JS
            .find("function renderPacketRow")
            .expect("the renderer");
        let tail = &RECORDINGS_JS[start..];
        let body = &tail[..tail.find("\n}\n").expect("its end")];
        for needle in [
            "el(\"td\", \"mono\", row.name)",
            "\"Download pcap\"",
            "\"Download jitter log\"",
            "logDownload(row.name,",
            "logDownload(row.companion,",
            "openConfirm(",
        ] {
            assert!(body.contains(needle), "renderPacketRow needs {needle}");
        }
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
        ] {
            assert!(!RECORDINGS_JS.contains(sink), "recordings.js uses {sink}");
        }
        assert!(
            RECORDINGS_JS.contains("encodeURIComponent(name)"),
            "a name goes into a URL encoded"
        );
    }

    /// The refusal for an open log needs a live call:
    /// `an_open_packet_log_refuses_delete_with_a_409`.
    #[test]
    fn packet_log_delete_needs_same_origin() {
        let dir = tempfile::tempdir().unwrap();
        let c = test_controller();
        c.set_packet_log_settings(Some(&crate::test_support::packet_log_cfg(dir.path())));
        c.packet_capture(true).unwrap();
        let name = crate::test_support::seed_packet_log(dir.path(), "call", b"pcap");
        let body = serde_json::json!({ "name": name }).to_string();
        let post = |headers: &RequestHeaders<'_>| {
            handle_request_with_headers(&c, "POST", "/packet-logs/delete", body.as_bytes(), headers)
        };
        let refused = post(&RequestHeaders {
            origin: Some("https://evil.example"),
            ..json_headers()
        });
        assert_eq!(refused.status, 403, "{refused:?}");
        let refused = post(&RequestHeaders {
            host: Some("evil.example"),
            ..json_headers()
        });
        assert_eq!(refused.status, 403, "{refused:?}");
        let refused = post(&RequestHeaders {
            content_type: Some("text/plain"),
            ..json_headers()
        });
        assert_eq!(refused.status, 415, "{refused:?}");
        assert!(dir.path().join(&name).exists(), "nothing deleted");
        let ok = post(&RequestHeaders {
            origin: Some("http://127.0.0.1:8730"),
            ..json_headers()
        });
        assert_eq!(ok.status, 200, "{ok:?}");
        assert!(!dir.path().join(&name).exists());
    }

    fn capture_post(c: &NodeController, body: &str, headers: &RequestHeaders<'_>) -> HttpResponse {
        handle_request_with_headers(c, "POST", "/packet-logs/capture", body.as_bytes(), headers)
    }

    #[test]
    fn capture_needs_same_origin_json() {
        let dir = tempfile::tempdir().unwrap();
        let c = test_controller();
        c.set_packet_log_settings(Some(&crate::test_support::packet_log_cfg(dir.path())));
        let body = r#"{"capturing":true}"#;
        let refused = capture_post(
            &c,
            body,
            &RequestHeaders {
                content_type: Some("text/plain"),
                ..json_headers()
            },
        );
        assert_eq!(refused.status, 415, "{refused:?}");
        let refused = capture_post(
            &c,
            body,
            &RequestHeaders {
                origin: Some("https://evil.example"),
                ..json_headers()
            },
        );
        assert_eq!(refused.status, 403, "{refused:?}");
        assert!(!c.packet_log_status().capturing, "nothing started");
    }

    #[test]
    fn capture_answers_each_refusal() {
        let h = json_headers();
        let c = test_controller();
        for bad in [
            "",
            "{}",
            r#"{"capturing":"yes"}"#,
            r#"{"capturing":true,"x":1}"#,
        ] {
            assert_eq!(capture_post(&c, bad, &h).status, 400, "{bad}");
        }
        // No table: asking to start is 404, but stopping is never refused.
        let r = capture_post(&c, r#"{"capturing":true}"#, &h);
        assert_eq!(
            (r.status, error_of(&r).as_str()),
            (404, "packet log is off")
        );
        assert_eq!(capture_post(&c, r#"{"capturing":false}"#, &h).status, 200);

        let dir = tempfile::tempdir().unwrap();
        c.set_packet_log_settings(Some(&crate::config::PacketLogCfg {
            debug_mode: false,
            ..crate::test_support::packet_log_cfg(dir.path())
        }));
        let r = capture_post(&c, r#"{"capturing":true}"#, &h);
        assert_eq!(
            (r.status, error_of(&r).as_str()),
            (409, "debug mode is off")
        );

        let not_a_folder = tempfile::NamedTempFile::new().unwrap();
        c.set_packet_log_settings(Some(&crate::test_support::packet_log_cfg(
            not_a_folder.path(),
        )));
        let r = capture_post(&c, r#"{"capturing":true}"#, &h);
        assert_eq!(r.status, 503, "{r:?}");
        let said = error_of(&r);
        assert!(said.starts_with("Couldn't start the capture"), "{said}");
        assert!(!said.contains("recording"), "{said}");
    }

    #[test]
    fn capture_starts_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let c = test_controller();
        c.set_packet_log_settings(Some(&crate::test_support::packet_log_cfg(dir.path())));
        let h = json_headers();
        let on = capture_post(&c, r#"{"capturing":true}"#, &h);
        assert_eq!(on.status, 200, "{on:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&on.body).unwrap(),
            serde_json::json!({ "capturing": true })
        );
        assert!(c.packet_log_status().capturing);
        let off = capture_post(&c, r#"{"capturing":false}"#, &h);
        assert_eq!(off.status, 200, "{off:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&off.body).unwrap(),
            serde_json::json!({ "capturing": false })
        );
        assert!(!c.packet_log_status().capturing);
    }

    // ---- the cross-origin guard ---------------------------------------------
    //
    // These four hold in every build: the guard runs before the controller is
    // asked anything, so a refusal never reaches the rule engine.

    #[test]
    fn admin_post_without_an_origin_is_allowed() {
        // curl and the node's own tooling send no Origin. Whatever the node
        // answers, it must not be the guard's 403.
        let c = test_controller();
        let r = admin_post(&c, r#"{"action":"enable"}"#);
        assert_ne!(r.status, 403, "a caller with no Origin is not cross-origin");
        assert_ne!(r.status, 415, "and it did send application/json");
    }

    #[test]
    fn admin_post_from_the_same_origin_is_allowed() {
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"enable"}"#,
            &RequestHeaders {
                origin: Some("http://127.0.0.1:8730"),
                ..json_headers()
            },
        );
        assert_ne!(r.status, 403, "the page's own fetch must go through: {r:?}");
    }

    #[test]
    fn admin_post_from_a_foreign_origin_is_403() {
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"enable"}"#,
            &RequestHeaders {
                origin: Some("https://evil.example"),
                ..json_headers()
            },
        );
        assert_eq!(r.status, 403, "a page on another origin gets nothing");
        assert!(
            error_of(&r).contains("cross-origin"),
            "the refusal says why: {}",
            error_of(&r)
        );
    }

    #[test]
    fn admin_post_without_a_json_content_type_is_415() {
        let c = test_controller();
        for ct in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/discord/admin",
                br#"{"action":"enable"}"#,
                &RequestHeaders {
                    content_type: ct,
                    ..json_headers()
                },
            );
            assert_eq!(
                r.status, 415,
                "a simple cross-origin POST cannot set application/json, so {ct:?} is refused"
            );
            assert_eq!(
                r.body,
                br#"{"error":"POST /discord/admin requires Content-Type: application/json"}"#,
                "the 415 names the route that refused: {ct:?}"
            );
        }
    }

    #[test]
    fn a_json_content_type_with_a_charset_is_still_json() {
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"enable"}"#,
            &RequestHeaders {
                content_type: Some("application/json; charset=utf-8"),
                ..json_headers()
            },
        );
        assert_ne!(r.status, 415, "parameters do not change the media type");
    }

    #[test]
    fn a_host_this_node_answers_to_gets_past_the_guard() {
        // 500 is the unconfigured node's refusal — the point is that it is
        // the CONTROLLER refusing, not the guard: the request got through.
        let c = test_controller();
        for host in [
            "127.0.0.1:8730",
            "localhost:8730",
            "LOCALHOST:8730",
            "[::1]:8730",
            "localhost",
            "127.0.0.1",
        ] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/discord/admin",
                br#"{"action":"enable"}"#,
                &RequestHeaders {
                    host: Some(host),
                    content_type: Some("application/json"),
                    ..RequestHeaders::default()
                },
            );
            assert_eq!(
                r.status, 500,
                "Host {host} names this node and must reach the controller: {r:?}"
            );
        }
    }

    #[test]
    fn a_host_that_is_not_this_node_is_403() {
        // Every one of these is a DNS answer pointed at loopback. The name
        // is the only thing that gives it away — a rebinding request's
        // Origin and Host agree perfectly.
        let c = test_controller();
        for host in [
            "evil.com:8730",
            "evil.com",
            "127.0.0.1.evil.com:8730",
            "localhost.evil.com:8730",
            "192.168.1.9:8730",
            "[2001:db8::1]:8730",
            "",
        ] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/discord/admin",
                br#"{"action":"enable"}"#,
                &RequestHeaders {
                    host: Some(host),
                    content_type: Some("application/json"),
                    ..RequestHeaders::default()
                },
            );
            assert_eq!(r.status, 403, "Host {host:?} must be refused: {r:?}");
        }
    }

    #[test]
    fn a_request_with_no_host_at_all_is_403() {
        // HTTP/1.1 requires one. Without it there is no way to show the
        // caller meant to reach this node.
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"enable"}"#,
            &RequestHeaders {
                content_type: Some("application/json"),
                ..RequestHeaders::default()
            },
        );
        assert_eq!(r.status, 403, "{r:?}");
    }

    #[test]
    fn the_rebinding_shape_is_refused_even_though_origin_matches_host() {
        // The whole attack in one request: a page served from evil.com
        // whose DNS now answers 127.0.0.1. Origin == Host, so the browser
        // sends no preflight and its script may set any Content-Type. Only
        // the name in Host is wrong.
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"disable"}"#,
            &RequestHeaders {
                origin: Some("http://evil.com:8730"),
                host: Some("evil.com:8730"),
                content_type: Some("application/json"),
                bound_to: None,
                ..RequestHeaders::default()
            },
        );
        assert_eq!(r.status, 403, "{r:?}");
    }

    #[test]
    fn a_null_origin_is_403() {
        // What a sandboxed iframe sends. It has no authority, so it can
        // never match a Host.
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/discord/admin",
            br#"{"action":"enable"}"#,
            &RequestHeaders {
                origin: Some("null"),
                ..json_headers()
            },
        );
        assert_eq!(r.status, 403, "{r:?}");
    }

    #[test]
    fn the_address_the_socket_is_bound_to_is_accepted_and_nothing_else_is() {
        // A node whose [control] bind is a LAN address is reached at that
        // address and its own page's Host is that literal.
        let c = test_controller();
        let bound: std::net::SocketAddr = "192.168.1.9:8730".parse().expect("a socket address");
        let post = |host: &str, bound_to| {
            handle_request_with_headers(
                &c,
                "POST",
                "/discord/admin",
                br#"{"action":"enable"}"#,
                &RequestHeaders {
                    host: Some(host),
                    content_type: Some("application/json"),
                    bound_to,
                    ..RequestHeaders::default()
                },
            )
            .status
        };
        assert_eq!(
            post("192.168.1.9:8730", Some(bound)),
            500,
            "the address this node is listening on is this node"
        );
        assert_eq!(
            post("192.168.1.9:8730", None),
            403,
            "and a caller that cannot say what it bound gets loopback only"
        );
        assert_eq!(
            post("192.168.1.10:8730", Some(bound)),
            403,
            "a different address on the same LAN is not this node"
        );
    }

    #[test]
    fn a_host_whose_port_is_not_a_port_is_refused() {
        // `host_without_port` used to split on the first colon and throw
        // the rest away, so `localhost:8730@evil.com` read as `localhost`.
        // No browser can send that — userinfo never reaches the request
        // line and `Host` is a forbidden header — but the guard that
        // decides who may reach this port must not trust an unparsed tail.
        let c = test_controller();
        let post = |host: &str| {
            handle_request_with_headers(
                &c,
                "POST",
                "/discord/admin",
                br#"{"action":"enable"}"#,
                &RequestHeaders {
                    host: Some(host),
                    content_type: Some("application/json"),
                    ..RequestHeaders::default()
                },
            )
            .status
        };
        for bad in [
            "localhost:8730@evil.com",
            "localhost:evil",
            "localhost:",
            "[::1]:x",
        ] {
            assert_eq!(post(bad), 403, "{bad} is not a host this node answers to");
        }
        // 500 is the controller refusing (no [discord] section in a test
        // controller) — i.e. the request got PAST the guard, which is what
        // these assert.
        for good in ["localhost:8730", "[::1]:8730", "[::1]", "::1", "127.0.0.1"] {
            assert_eq!(post(good), 500, "{good} is this node");
        }
    }

    #[test]
    fn the_favicon_is_answered_rather_than_404d() {
        let c = test_controller();
        let r = handle_request(&c, "GET", "/favicon.ico", b"");
        assert_eq!(r.status, 204, "204 is 'there is none', not 'wrong URL'");
        assert!(r.body.is_empty(), "a 204 carries no body");
    }

    #[test]
    fn the_admin_route_never_sends_cors_headers() {
        // An Access-Control-Allow-Origin here would hand every page in the
        // operator's browser the node's root of trust.
        let c = test_controller();
        let r = admin_post(&c, r#"{"action":"enable"}"#);
        for (name, _) in &r.headers {
            assert!(
                !name.to_ascii_lowercase().starts_with("access-control-"),
                "the admin route must send no CORS header, got {name}"
            );
        }
    }

    // ---- the port-wide Host allow-list (iax-ctlcsrf step 1) ------------------
    //
    // The guard runs before the route is looked at. These hold in every
    // build: nothing here needs a [discord] section.

    /// A request from a page whose DNS was re-pointed at this node. `Origin`
    /// is deliberately absent: `curl` sends none either, and the guard must
    /// not need it.
    fn foreign() -> RequestHeaders<'static> {
        RequestHeaders {
            host: Some("evil.com:8730"),
            ..RequestHeaders::default()
        }
    }

    #[test]
    fn every_route_refuses_a_foreign_host() {
        // GET /status first: it is 200 for anyone today, so the failure this
        // test reports before the guard exists is `left: 200, right: 403`.
        let c = test_controller();
        for (method, route) in [
            ("GET", "/status"),
            ("POST", "/key"),
            ("POST", "/hangup"),
            ("POST", "/m17"),
            ("GET", "/"),
            ("GET", "/discord/state"),
            ("POST", "/shutdown"),
            ("GET", "/no-such-route"),
        ] {
            let r = handle_request_with_headers(&c, method, route, b"", &foreign());
            assert_eq!(
                r.status, 403,
                "{method} {route} with Host \"evil.com:8730\" must be refused: {r:?}"
            );
            assert_eq!(
                r.body, br#"{"error":"cross-origin requests are not accepted on this port"}"#,
                "{method} {route}: the refusal is the same sentence on every route"
            );
        }
        // Nothing above reached the controller: the shutdown route was in
        // the list and the stop flag must not have moved.
        assert!(
            !c.should_stop(),
            "a refused POST /shutdown must not have stopped the node"
        );
    }

    #[test]
    fn post_key_on_a_host_this_node_answers_to_is_unchanged() {
        // Key on an idle NullBackend station keys nothing (there is no
        // session to key; `key_and_unkey_when_idle_are_errors_not_panics`
        // already does this). Each call gets a FRESH controller so the
        // second Key is not answered differently from the first.
        let expected = handle_request(&test_controller(), "POST", "/key", b"");
        let key_with = |host: &str, bound_to: Option<std::net::SocketAddr>| {
            handle_request_with_headers(
                &test_controller(),
                "POST",
                "/key",
                b"",
                &RequestHeaders {
                    host: Some(host),
                    bound_to,
                    ..RequestHeaders::default()
                },
            )
        };
        for host in [
            "localhost",
            "127.0.0.1:8730",
            "[::1]:8730",
            "LOCALHOST:8730",
        ] {
            let r = key_with(host, None);
            assert_eq!(
                (r.status, &r.body),
                (expected.status, &expected.body),
                "Host {host} is this node and POST /key must answer as before"
            );
        }
        let bound: std::net::SocketAddr = "192.168.1.9:8730".parse().expect("a socket address");
        let r = key_with("192.168.1.9:8730", Some(bound));
        assert_eq!(
            (r.status, &r.body),
            (expected.status, &expected.body),
            "the address the socket is bound to is this node"
        );
        let r = key_with("192.168.1.9:8730", None);
        assert_eq!(
            r.status, 403,
            "a caller that cannot say what it bound gets loopback only: {r:?}"
        );
    }

    #[test]
    fn a_host_this_node_answers_to_lets_hangup_through() {
        // An absolute value, not a comparison: hangup when idle is 200 ok.
        let c = test_controller();
        for host in ["localhost", "127.0.0.1:8730", "[::1]:8730"] {
            let r = handle_request_with_headers(
                &c,
                "POST",
                "/hangup",
                b"",
                &RequestHeaders {
                    host: Some(host),
                    ..RequestHeaders::default()
                },
            );
            assert_eq!(r.status, 200, "Host {host}: {r:?}");
            assert_eq!(r.body, br#"{"ok":true}"#, "Host {host}");
        }
    }

    #[test]
    fn handle_request_is_the_in_process_caller_with_a_loopback_host() {
        // The header-less entry point is defined as "this node, from inside
        // the process": byte for byte the same answer as an explicit
        // `Host: localhost`. GET / is the pair to compare on — a static
        // asset, stateless, and it carries extra headers (CSP, nosniff), so
        // every field of the response takes part in the comparison. A
        // caller that really has NO Host is not the in-process caller, and
        // gets the guard's 403 on a route that is 200 for everyone else.
        let c = test_controller();
        let plain = handle_request(&c, "GET", "/", b"");
        let localhost = handle_request_with_headers(
            &c,
            "GET",
            "/",
            b"",
            &RequestHeaders {
                host: Some("localhost"),
                ..RequestHeaders::default()
            },
        );
        assert_eq!(plain.status, 200, "{plain:?}");
        assert_eq!(
            (
                plain.status,
                plain.content_type,
                &plain.body,
                &plain.headers
            ),
            (
                localhost.status,
                localhost.content_type,
                &localhost.body,
                &localhost.headers
            ),
            "handle_request is handle_request_with_headers with Host: localhost, field for field"
        );
        let none =
            handle_request_with_headers(&c, "GET", "/status", b"", &RequestHeaders::default());
        assert_eq!(
            none.status, 403,
            "no Host at all is not this node: {none:?}"
        );
        assert_eq!(
            none.body,
            br#"{"error":"cross-origin requests are not accepted on this port"}"#
        );
    }

    #[test]
    fn a_wildcard_host_is_never_this_node() {
        for (host, bound) in [
            ("0.0.0.0:8730", "0.0.0.0:8730"),
            ("0.0.0.0", "0.0.0.0:8730"),
            ("[::]:8730", "[::]:8730"),
        ] {
            let bound: std::net::SocketAddr = bound.parse().unwrap();
            assert!(
                !host_is_this_node(Some(host), Some(bound)),
                "Host {host} on a socket bound to {bound}"
            );
        }
        let lan: std::net::SocketAddr = "192.168.1.9:8730".parse().unwrap();
        assert!(host_is_this_node(Some("192.168.1.9:8730"), Some(lan)));
    }

    #[test]
    fn same_origin_json_names_the_route_in_its_415() {
        // Called directly: no other route opts in yet, and the settings
        // routes that will opt in need the sentence to name THEM.
        let no_json = RequestHeaders {
            content_type: Some("text/plain"),
            ..json_headers()
        };
        let refused =
            same_origin_json(&no_json, "POST /settings").expect_err("text/plain is not JSON");
        assert_eq!(refused.status, 415);
        assert_eq!(
            refused.body,
            br#"{"error":"POST /settings requires Content-Type: application/json"}"#
        );
        assert!(
            same_origin_json(&json_headers(), "POST /settings").is_ok(),
            "the same headers with application/json pass"
        );
    }

    #[test]
    fn host_is_loopback_reads_only_the_name() {
        for yes in [
            "localhost",
            "LOCALHOST:8730",
            "127.0.0.1",
            "127.0.0.1:8730",
            "127.5.5.5:1",
            "[::1]:8730",
            "[::1]",
            "::1",
        ] {
            assert!(host_is_loopback(Some(yes)), "{yes} is loopback");
        }
        for no in [
            "192.168.1.9:8730",
            "[2001:db8::1]:8730",
            "evil.com",
            "localhost.evil.com",
            "localhost:8730@evil.com",
            "localhost:",
            "",
        ] {
            assert!(!host_is_loopback(Some(no)), "{no} is not loopback");
        }
        assert!(!host_is_loopback(None), "no Host is not loopback");
    }

    // ---- the body cap -------------------------------------------------------

    #[test]
    fn a_body_at_the_cap_is_accepted_and_one_byte_over_is_413() {
        // /hangup ignores its body, so the size is the only thing under test.
        let c = test_controller();
        let at = handle_request(&c, "POST", "/hangup", &vec![b' '; MAX_BODY_BYTES]);
        assert_eq!(at.status, 200, "{at:?}");
        assert_eq!(at.body, br#"{"ok":true}"#);

        let over = handle_request(&c, "POST", "/hangup", &vec![b' '; MAX_BODY_BYTES + 1]);
        assert_eq!(over.status, 413, "{over:?}");
        assert_eq!(
            over.body,
            br#"{"error":"request body exceeds 65536 bytes"}"#
        );
        assert_eq!(
            MAX_BODY_BYTES, 65536,
            "the sentence above spells the cap out on purpose; change both"
        );
    }

    #[test]
    fn the_cap_is_checked_before_the_route_and_after_the_host() {
        let c = test_controller();
        let big = vec![b' '; MAX_BODY_BYTES + 1];
        // Before the route: an unknown path with an oversize body is 413,
        // not 404 — nothing is looked up for a body that is not going to
        // be read.
        let r = handle_request(&c, "POST", "/no-such-route", &big);
        assert_eq!(r.status, 413, "{r:?}");
        // After the Host: a foreign caller learns nothing about the cap.
        let r = handle_request_with_headers(&c, "POST", "/hangup", &big, &foreign());
        assert_eq!(r.status, 403, "{r:?}");
        assert_eq!(
            r.body,
            br#"{"error":"cross-origin requests are not accepted on this port"}"#
        );
    }

    // ---- the refusals a build without the feature gives ----------------------

    // ---- a configured node --------------------------------------------------

    /// A controller with slash commands configured, over a hardware-free
    /// station. No gateway is ever started and nothing reaches Discord:
    /// `Commander` is a pure state machine and this is all of it the page
    /// ever sees.
    #[cfg(feature = "discord")]
    fn discord_controller() -> NodeController {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        let mut c = NodeController::new(station, secrets);
        c.set_discord_config(Some(crate::config::DiscordCfg {
            guild_id: "111".into(),
            channel_id: "222".into(),
            approvers: vec!["333".into()],
            ..crate::config::DiscordCfg::default()
        }));
        c.set_discord_approvals(astar_discord::Approvals::in_memory())
            .expect("222 is a snowflake");
        c
    }

    #[cfg(feature = "discord")]
    fn state_of(c: &NodeController) -> serde_json::Value {
        let r = handle_request(c, "GET", "/discord/state", b"");
        assert_eq!(r.status, 200, "the state route on a configured node: {r:?}");
        serde_json::from_slice(&r.body).expect("the state body is JSON")
    }

    #[cfg(feature = "discord")]
    fn ids(v: &serde_json::Value, list: &str) -> Vec<String> {
        v[list]
            .as_array()
            .unwrap_or_else(|| panic!("{list} is an array: {v}"))
            .iter()
            .map(|e| {
                e["user_id"]
                    .as_str()
                    .expect("user_id is a string")
                    .to_string()
            })
            .collect()
    }

    #[cfg(feature = "discord")]
    #[test]
    fn the_state_route_carries_every_key_the_page_reads() {
        // Named one by one on purpose: the page reads each of these, and a
        // rename on the Rust side must break a test rather than a browser.
        let c = discord_controller();
        let v = state_of(&c);
        for key in [
            "configured",
            "feature",
            "connected",
            "commands",
            "enabled",
            "approvers",
            "admins",
            "pending",
            "approved",
            "banned",
            "present",
            "mic_holder",
            "link",
            "last_error",
            "audio_ok",
            "mic_timeout_secs",
            "mic_silence_secs",
            "talking",
        ] {
            assert!(v.get(key).is_some(), "/discord/state must carry {key}: {v}");
        }
        assert_eq!(v["configured"], serde_json::json!(true));
        assert_eq!(v["approvers"], serde_json::json!(["333"]));
        assert!(v["mic_holder"].is_null(), "nobody holds the mic: {v}");
        assert!(v["link"].is_null(), "an idle node has no Discord link: {v}");
        assert_eq!(
            v["commands"],
            serde_json::json!(false),
            "approvers are configured but no bot is online: {v}"
        );
        assert_eq!(
            v["audio_ok"],
            serde_json::json!(false),
            "no bridge is installed, so no audio is reaching Discord: {v}"
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn approve_puts_a_user_in_the_approved_list() {
        let c = discord_controller();
        let r = admin_post(&c, r#"{"action":"approve","user_id":"444"}"#);
        assert_eq!(r.status, 200, "approve from the page: {r:?}");
        assert_eq!(ids(&state_of(&c), "approved"), vec!["444".to_string()]);
    }

    #[cfg(feature = "discord")]
    #[test]
    fn banning_an_approved_user_moves_them() {
        let c = discord_controller();
        assert_eq!(
            admin_post(&c, r#"{"action":"approve","user_id":"444"}"#).status,
            200
        );
        let r = admin_post(&c, r#"{"action":"ban","user_id":"444"}"#);
        assert_eq!(r.status, 200, "ban from the page: {r:?}");
        let v = state_of(&c);
        assert_eq!(ids(&v, "banned"), vec!["444".to_string()]);
        assert!(
            ids(&v, "approved").is_empty(),
            "a ban revokes as well as bans: {v}"
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn approving_a_banned_user_is_409_with_the_engines_reason() {
        let c = discord_controller();
        assert_eq!(
            admin_post(&c, r#"{"action":"ban","user_id":"444"}"#).status,
            200
        );
        let before = state_of(&c);

        let r = admin_post(&c, r#"{"action":"approve","user_id":"444"}"#);
        assert_eq!(r.status, 409, "a well-formed request that does not apply");
        let why = error_of(&r);
        assert!(
            why.contains("is banned on this node"),
            "the page shows this verbatim, so it has to be the engine's own sentence: {why}"
        );
        assert_eq!(state_of(&c), before, "a refusal must change nothing at all");
    }

    #[cfg(feature = "discord")]
    #[test]
    fn an_unknown_action_is_400_and_changes_nothing() {
        let c = discord_controller();
        let before = state_of(&c);
        let r = admin_post(&c, r#"{"action":"delete_everything","user_id":"444"}"#);
        assert_eq!(r.status, 400, "{r:?}");
        assert!(
            error_of(&r).contains("unknown action"),
            "unexpected error: {}",
            error_of(&r)
        );
        assert_eq!(state_of(&c), before);
    }

    #[cfg(feature = "discord")]
    #[test]
    fn an_action_that_needs_a_user_id_is_400_without_one() {
        let c = discord_controller();
        let before = state_of(&c);
        let r = admin_post(&c, r#"{"action":"approve"}"#);
        assert_eq!(r.status, 400, "{r:?}");
        assert!(
            error_of(&r).contains("needs a user_id"),
            "unexpected error: {}",
            error_of(&r)
        );
        assert_eq!(state_of(&c), before);
    }

    #[cfg(feature = "discord")]
    #[test]
    fn a_numeric_user_id_is_400_and_changes_nothing() {
        // A snowflake does not survive a JSON number. It must be refused at
        // the body, not rounded into a decision about somebody else.
        let c = discord_controller();
        let before = state_of(&c);
        let r = admin_post(&c, r#"{"action":"approve","user_id":444}"#);
        assert_eq!(r.status, 400, "a numeric user_id is not a user id: {r:?}");
        assert_eq!(state_of(&c), before);
        assert!(
            ids(&state_of(&c), "approved").is_empty(),
            "and nobody was approved by it"
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn a_busy_rule_engine_is_503_with_a_retry_after() {
        // The pump holds the commander for microseconds, except behind a
        // dial. The page must be told to come back, not told "no".
        let c = discord_controller();
        let held = c
            .discord_commander
            .lock()
            .expect("discord commander mutex poisoned")
            .take()
            .expect("the commander was built at construction");

        let r = admin_post(&c, r#"{"action":"enable"}"#);
        assert_eq!(r.status, 503, "{r:?}");
        assert_eq!(
            r.headers
                .iter()
                .find(|(n, _)| *n == "Retry-After")
                .map(|(_, v)| v.as_str()),
            Some("1"),
            "a 503 that does not say when to retry is a dead end: {:?}",
            r.headers
        );

        *c.discord_commander
            .lock()
            .expect("discord commander mutex poisoned") = Some(held);
    }

    #[cfg(feature = "discord")]
    #[test]
    fn an_unconfigured_node_refuses_both_routes_the_way_post_discord_does() {
        // No approvers: the [discord] section is the audio-only bridge and
        // there is no rule engine to administer.
        let c = test_controller();
        let post = admin_post(&c, r#"{"action":"approve","user_id":"444"}"#);
        assert_eq!(post.status, 500, "same status as POST /discord: {post:?}");
        assert!(
            error_of(&post).contains("not configured"),
            "unexpected error: {}",
            error_of(&post)
        );

        let state = handle_request(&c, "GET", "/discord/state", b"");
        assert_eq!(state.status, 500);
        let v: serde_json::Value = serde_json::from_slice(&state.body).unwrap();
        assert_eq!(v["configured"], serde_json::json!(false));
        assert_eq!(
            v["feature"],
            serde_json::json!(true),
            "this build HAS the feature; it is the node that is not set up: {v}"
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn a_refused_request_never_reaches_the_rule_engine() {
        // Each of the three guard refusals, against a node that has
        // something to lose: nothing may move.
        let c = discord_controller();
        let before = state_of(&c);
        let ban = br#"{"action":"ban","user_id":"444"}"#;
        let cases = [
            (
                403,
                RequestHeaders {
                    origin: Some("http://evil.com:8730"),
                    host: Some("evil.com:8730"),
                    content_type: Some("application/json"),
                    bound_to: None,
                    ..RequestHeaders::default()
                },
            ),
            (
                403,
                RequestHeaders {
                    origin: Some("https://evil.example"),
                    ..json_headers()
                },
            ),
            (
                415,
                RequestHeaders {
                    content_type: Some("text/plain"),
                    ..json_headers()
                },
            ),
        ];
        for (status, headers) in cases {
            let r = handle_request_with_headers(&c, "POST", "/discord/admin", ban, &headers);
            assert_eq!(r.status, status, "{headers:?} -> {r:?}");
            assert_eq!(
                state_of(&c),
                before,
                "a refused request must not have banned anybody: {headers:?}"
            );
        }
    }

    #[cfg(feature = "discord")]
    #[test]
    fn a_hostile_display_name_comes_back_as_an_inert_json_string() {
        // Display names belong to the people who choose them. This one is
        // the classic; it must survive the round trip byte for byte, as a
        // JSON string, and the page puts it in a text node.
        const NASTY: &str = r#"<img src="x" onerror=alert(1)>"#;
        let c = discord_controller();
        let (tx, rx) = std::sync::mpsc::channel();
        c.install_discord_events(rx);
        tx.send(astar_discord::GatewayEvent::Voice(
            astar_discord::VoiceEvent {
                user: astar_discord::UserRef {
                    id: 9,
                    name: NASTY.to_string(),
                },
                channel: Some(222),
                server_muted: false,
                is_bot: false,
            },
        ))
        .expect("the receiver is installed");
        c.pump();

        let r = handle_request(&c, "GET", "/discord/state", b"");
        assert_eq!(r.status, 200);
        let raw = String::from_utf8(r.body).expect("the body is utf-8");
        assert!(
            raw.contains(r#"<img src=\"x\" onerror=alert(1)>"#),
            "the quotes must be JSON-escaped, which is what makes it a string \
             and not markup: {raw}"
        );
        let v: serde_json::Value = serde_json::from_str(&raw).expect("and it is still JSON");
        assert_eq!(
            v["present"][0]["name"],
            serde_json::json!(NASTY),
            "and it parses back to exactly what was sent: {v}"
        );
    }

    #[cfg(not(feature = "discord"))]
    #[test]
    fn without_the_feature_both_discord_routes_say_so() {
        let c = test_controller();
        let post = admin_post(&c, r#"{"action":"approve","user_id":"444"}"#);
        assert_eq!(post.status, 500, "same status POST /discord uses: {post:?}");
        assert!(
            error_of(&post).contains("no discord feature"),
            "unexpected error: {}",
            error_of(&post)
        );

        let state = handle_request(&c, "GET", "/discord/state", b"");
        assert_eq!(state.status, 500);
        let v: serde_json::Value = serde_json::from_slice(&state.body).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("no discord feature"),
            "unexpected error: {v}"
        );
        assert_eq!(
            v["feature"],
            serde_json::json!(false),
            "the page needs to tell 'not built in' from 'not configured': {v}"
        );
        assert_eq!(v["configured"], serde_json::json!(false));
    }

    // ---- iax-ctlcsrf step 2: the Origin rule on every mutating route -------

    /// Every route that changes something, from the route table.
    fn mutating() -> impl Iterator<Item = &'static str> {
        crate::access::ADMIN_ROUTES
            .iter()
            .filter(|(m, _)| *m != "GET")
            .map(|(_, p)| *p)
    }

    fn loopback_headers() -> RequestHeaders<'static> {
        RequestHeaders {
            host: Some("127.0.0.1:8730"),
            ..RequestHeaders::default()
        }
    }

    #[test]
    fn a_foreign_origin_is_refused_on_every_mutating_route() {
        let forbidden =
            serde_json::json!({ "error": "cross-origin requests are not accepted on this port" });
        for path in mutating() {
            for origin in ["https://evil.example", "null", "http://127.0.0.1:9999"] {
                let c = test_controller();
                let r = handle_request_with_headers(
                    &c,
                    "POST",
                    path,
                    b"{}",
                    &RequestHeaders {
                        origin: Some(origin),
                        content_type: Some("application/json"),
                        ..loopback_headers()
                    },
                );
                assert_eq!(r.status, 403, "POST {path} with Origin {origin}: {r:?}");
                assert_eq!(json_of(&r), forbidden, "POST {path} with Origin {origin}");
                assert!(!c.should_stop(), "POST {path}: nothing ran");
            }
        }
    }

    #[test]
    fn a_browser_form_post_is_refused_on_every_mutating_route() {
        // What a cross-site `<form method=POST>` can send: one of the three
        // "simple" content types, with an Origin (always, on a cross-site
        // POST) or at least Sec-Fetch-Site. A form on the node's OWN origin
        // would match Host, and it is refused anyway: no page of ours posts
        // a form, so a form is never ours.
        for path in mutating() {
            for ct in [
                Some("text/plain"),
                Some("application/x-www-form-urlencoded"),
                Some("multipart/form-data; boundary=x"),
                None,
            ] {
                for (origin, fetch_site) in [
                    (Some("http://127.0.0.1:8730"), None),
                    (None, Some("cross-site")),
                    (Some("http://127.0.0.1:8730"), Some("same-origin")),
                ] {
                    let c = test_controller();
                    let r = handle_request_with_headers(
                        &c,
                        "POST",
                        path,
                        b"{}",
                        &RequestHeaders {
                            origin,
                            sec_fetch_site: fetch_site,
                            content_type: ct,
                            ..loopback_headers()
                        },
                    );
                    assert_eq!(
                        r.status, 415,
                        "POST {path} as {ct:?} from a browser ({origin:?}, {fetch_site:?}): {r:?}"
                    );
                    assert_eq!(
                        json_of(&r),
                        serde_json::json!({
                            "error": format!("POST {path} requires Content-Type: application/json")
                        }),
                        "the 415 names the route"
                    );
                    assert!(!c.should_stop(), "POST {path}: nothing ran");
                }
            }
        }
    }

    #[test]
    fn a_curl_shaped_request_keeps_working() {
        // No Origin, no Sec-Fetch-Site: not a browser, so the contract every
        // existing client relies on is unchanged — any content type, or none.
        for ct in [
            None,
            Some("application/x-www-form-urlencoded"),
            Some("text/plain"),
        ] {
            for path in ["/key", "/unkey", "/hangup", "/register"] {
                let c = test_controller();
                let r = handle_request_with_headers(
                    &c,
                    "POST",
                    path,
                    b"",
                    &RequestHeaders {
                        content_type: ct,
                        ..loopback_headers()
                    },
                );
                assert!(
                    r.status != 403 && r.status != 415,
                    "POST {path} from curl ({ct:?}) reached the route: {r:?}"
                );
            }
        }
    }

    #[test]
    fn the_nodes_own_page_still_posts_json() {
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "POST",
            "/unkey",
            b"",
            &RequestHeaders {
                origin: Some("http://127.0.0.1:8730"),
                sec_fetch_site: Some("same-origin"),
                content_type: Some("application/json"),
                ..loopback_headers()
            },
        );
        // The controller answered (no call is up): the rule let it through.
        assert_eq!(json_of(&r), error_body("no active call"), "{r:?}");
    }

    #[test]
    fn reads_are_not_subject_to_the_origin_rule() {
        // A GET changes nothing, and a cross-origin page cannot read the
        // answer (no route sends CORS headers), so the rule leaves GETs be.
        let c = test_controller();
        let r = handle_request_with_headers(
            &c,
            "GET",
            "/status",
            b"",
            &RequestHeaders {
                origin: Some("https://evil.example"),
                sec_fetch_site: Some("cross-site"),
                ..loopback_headers()
            },
        );
        assert_eq!(r.status, 200, "{r:?}");
    }

    // ---- the route table ----------------------------------------------------

    #[test]
    fn the_router_answers_every_listed_route() {
        // `GET /events` is the server layer's; everything else is the
        // router's, and its unknown-route 404 here means the table names a
        // route the router does not have. (The recordings routes answer
        // their own 404, "recording is off", on a node with no logger, and
        // the packet log routes "packet log is off".)
        for &(method, path) in crate::access::ADMIN_ROUTES {
            if path == "/events" {
                continue;
            }
            let c = test_controller();
            let r = handle_request_with_headers(
                &c,
                method,
                path,
                b"{}",
                &RequestHeaders {
                    content_type: Some("application/json"),
                    ..loopback_headers()
                },
            );
            assert!(
                r.status != 404 || r.body != br#"{"error":"not found"}"#,
                "{method} {path}: {r:?}"
            );
        }
    }

    /// The admin page shows the node's relay warning beside the links, read
    /// from `/status`'s `m17_relay`, through textContent only.
    #[test]
    fn admin_page_renders_the_m17_relay_warning_from_status() {
        assert!(INDEX_HTML.contains("id=\"m17-relay-warning\""));
        assert!(APP_JS.contains("renderM17Relay(snap.m17_relay)"));
        let start = APP_JS
            .find("function renderM17Relay")
            .expect("the renderer");
        let tail = &APP_JS[start..];
        let body = &tail[..tail.find("\n}\n").expect("its end")];
        assert!(body.contains("relay.warning"), "{body}");
        for sink in ["innerHTML", "outerHTML", "insertAdjacentHTML"] {
            assert!(!body.contains(sink), "renderM17Relay uses {sink}");
        }
    }

    /// The settings page's M17 card leads with the warning, word for word
    /// the node's own, and swaps in the counted version from `/status`.
    #[test]
    fn settings_m17_card_renders_the_relay_warning() {
        let quoted = format!("\"{}\"", crate::m17_relay::M17_RELAY_WARNING);
        assert!(
            SETTINGS_JS.contains(&quoted),
            "settings.js carries M17_RELAY_WARNING verbatim"
        );
        let start = SETTINGS_JS
            .find("function renderCardM17")
            .expect("the card");
        let tail = &SETTINGS_JS[start..];
        let body = &tail[..tail.find("\n}\n").expect("its end")];
        assert!(body.contains("m17RelayWarning()"), "{body}");
        assert!(SETTINGS_JS.contains("st.m17_relay.warning"));
    }

    #[test]
    fn admin_page_has_an_echolink_card_and_never_uses_inner_html_for_it() {
        assert!(INDEX_HTML.contains("id=\"echolink\""));
        // Relative, like every admin fetch: the page is also served under
        // `/admin/`, so the brief's absolute "/echolink" would 404 there.
        assert!(APP_JS.contains("fetch(\"echolink\""));
        let start = APP_JS.find("const EL_LOGIN").expect("EchoLink section");
        let tail = &APP_JS[start..];
        let end = tail.find("\nfunction render()").expect("next fn");
        let body = &tail[..end];
        assert!(body.contains("function echoLinkRow") && body.contains("function sendEchoLink"));
        // The server matches a disconnect by callsign, never node number.
        assert!(body.contains("action: \"disconnect\", target: p.call"));
        assert!(!body.contains("target: String(p.node)"));
        for sink in ["innerHTML", "outerHTML", "insertAdjacentHTML"] {
            assert!(!body.contains(sink), "renderEchoLink uses {sink}");
        }
    }

    #[test]
    fn admin_pages_use_relative_urls() {
        // The same files are served at `/` on the control port and under
        // `/admin/` on the public bind, so no page may name an absolute path.
        for (name, text) in [
            ("index.html", INDEX_HTML),
            ("app.js", APP_JS),
            ("discord.html", DISCORD_HTML),
            ("discord.js", DISCORD_JS),
            ("settings.html", SETTINGS_HTML),
            ("settings.js", SETTINGS_JS),
        ] {
            for bad in [
                "href=\"/",
                "src=\"/",
                "fetch(\"/",
                "EventSource(\"/",
                "postJson(\"/",
                "href = \"/",
            ] {
                assert!(!text.contains(bad), "{name} contains {bad}");
            }
        }
    }
}
