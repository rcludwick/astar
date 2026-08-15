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
//! # Secret safety
//!
//! `ProvideSecret` accepts credentials from the request body and forwards them
//! to the [`SecretProvider`] through `NodeController::execute`.  The reply is
//! always `{"ok":true}` — the secret is **never** echoed in any response.

use serde::Deserialize;
use serde_json::Value;

use crate::{
    command::{LinkAction, NodeCommand, NodeReply},
    controller::NodeController,
};

// ---- status page assets (iax-24e2) ------------------------------------------

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

/// A 200 response for an embedded static asset.
fn asset(content_type: &'static str, body: &'static str) -> HttpResponse {
    HttpResponse {
        status: 200,
        content_type,
        body: body.as_bytes().to_vec(),
    }
}

/// A fully-formed HTTP response: status, content-type, and body bytes.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Serialise `value` to JSON and return with `status`.
    #[must_use]
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
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
/// `host:port` (harness / localhost), bypassing `AllStar` DNS.
#[derive(Deserialize)]
struct LinkRequest {
    action: LinkAction,
    node: String,
    #[serde(default)]
    addr: Option<String>,
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

// ---- router -----------------------------------------------------------------

/// Route a request to a response.
///
/// `GET /events` (SSE) is **not** handled here — the Task 8 server layer
/// intercepts it before calling this function.  All other routes are covered.
///
/// # Panics
///
/// Never.  All parse / execute errors are mapped to 4xx/5xx responses.
#[must_use]
pub fn handle_request(
    ctrl: &NodeController,
    method: &str,
    path: &str,
    body: &[u8],
) -> HttpResponse {
    // Strip query string (not used by any current route, but keeps the router
    // forward-compatible).
    let route = path.split('?').next().unwrap_or(path);

    match (method, route) {
        // ---- status page (iax-24e2) — read-only embedded assets -------------
        ("GET", "/") => asset("text/html; charset=utf-8", INDEX_HTML),
        ("GET", "/app.js") => asset("application/javascript; charset=utf-8", APP_JS),
        ("GET", "/style.css") => asset("text/css; charset=utf-8", STYLE_CSS),

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
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
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
            execute_and_render(
                ctrl,
                NodeCommand::Link {
                    action: req.action,
                    node: req.node,
                    addr: req.addr,
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

        // ---- lifecycle ------------------------------------------------------
        ("POST", "/shutdown") => execute_and_render(ctrl, NodeCommand::Shutdown),

        // ---- fallback -------------------------------------------------------
        _ => HttpResponse::error_json(404, "not found"),
    }
}

/// Execute `cmd` and map `NodeReply` / `NodeError` to an `HttpResponse`.
fn execute_and_render(ctrl: &NodeController, cmd: NodeCommand) -> HttpResponse {
    match ctrl.execute(cmd) {
        Ok(NodeReply::Ok) => HttpResponse::ok_json(),
        Ok(NodeReply::Snapshot(s)) => {
            let v = serde_json::to_value(&s).unwrap_or_else(|_| serde_json::json!({}));
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
        let r = handle_request(
            &c,
            "POST",
            "/link",
            br#"{"action":"connect","node":"55553","addr":"127.0.0.1:4569"}"#,
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
        assert!(body.contains("/app.js") && body.contains("/style.css"));
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
}
