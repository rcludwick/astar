// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `/auth/*` routes, socket-independent: both binds call [`handle`].
//!
//! Every POST here carries JSON and passes the Origin rule
//! ([`check_post`]): `Content-Type: application/json` from every caller,
//! and an `Origin`, when present, matching `Host`.

use serde_json::{Value, json};

use crate::http::HttpResponse;

use super::service::{AuthService, Principal, Refusal, Req, Surface};
use super::session;
use super::{ok_json, refusal_response};

/// Answer an `/auth/*` request, or `None` when `req.path` is not one.
#[must_use]
pub fn handle(svc: &AuthService, req: &Req<'_>) -> Option<HttpResponse> {
    if !req.path.starts_with("/auth/") {
        return None;
    }
    if (req.method == "POST" || req.method == "DELETE")
        && let Err(r) = check_post(req)
    {
        return Some(refusal_response(&r));
    }
    let out = route(svc, req);
    Some(out.unwrap_or_else(|r| refusal_response(&r)))
}

fn route(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    match (req.method, req.path) {
        ("GET", "/auth/status") => Ok(status(svc, req)),
        ("GET", "/auth/me") => me(svc, req),
        ("POST", "/auth/signout") => signout(svc, req),
        _ => super::flows::route(svc, req),
    }
}

/// The Origin rule for `/auth/*` writes.
///
/// # Errors
/// 415 without JSON; 403 for an `Origin` that does not match `Host`.
pub fn check_post(req: &Req<'_>) -> Result<(), Refusal> {
    let json = req.content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    if !json {
        return Err(Refusal::new(
            415,
            "json_required",
            format!(
                "{} {} requires Content-Type: application/json",
                req.method, req.path
            ),
        ));
    }
    if let Some(origin) = req.origin {
        let authority = origin
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(""));
        let same = authority.is_some_and(|a| {
            !a.is_empty() && req.host.is_some_and(|h| a.eq_ignore_ascii_case(h.trim()))
        });
        if !same {
            return Err(Refusal::new(
                403,
                "cross_origin",
                "cross-origin requests are not accepted",
            ));
        }
    }
    Ok(())
}

/// Whether a ceremony may start on this request: the store is usable, and
/// the page is HTTPS, or `localhost`, or loopback on the control port.
///
/// # Errors
/// Why sign-in is not possible here.
pub fn check_signin_possible(svc: &AuthService, req: &Req<'_>) -> Result<(), Refusal> {
    if let Some(_why) = svc.disabled() {
        return Err(Refusal::new(
            503,
            "signin_disabled",
            "Sign-in is disabled on this node; the admin page on the node itself says why.",
        ));
    }
    let host = req.hostname();
    let local = host == "localhost";
    match req.surface {
        Surface::Public if !req.tls && !local => Err(Refusal::new(
            403,
            "https_required",
            "Sign-in needs HTTPS; the certificate is still being issued.",
        )),
        Surface::Control
            if !(crate::http::host_is_loopback(req.host)
                && crate::access::is_trusted_loopback_peer(req.peer, req.forwarded)) =>
        {
            Err(Refusal::new(
                403,
                "loopback_required",
                "On the control port, sign in from localhost (an ssh tunnel), or use the public page.",
            ))
        }
        _ => Ok(()),
    }
}

/// What the sign-in page needs before anyone is signed in.
fn status(svc: &AuthService, req: &Req<'_>) -> HttpResponse {
    let (admins, users) = svc.read(|d| (d.admin_count(), d.users.len()));
    let possible = check_signin_possible(svc, req);
    let mut v = json!({
        "signin": possible.is_ok(),
        "admin_exists": admins > 0,
        "users": users > 0,
        "host": req.hostname(),
        "node": svc.settings.node,
    });
    if let Err(r) = possible {
        v["reason"] = Value::String(r.code.to_string());
        v["message"] = Value::String(r.message);
    }
    // The console on the node may read why sign-in is off; nobody else
    // learns more than that it is.
    if let (Some(why), Some(Principal::Console)) = (svc.disabled(), svc.principal(req)) {
        v["disabled_reason"] = Value::String(why);
    }
    ok_json(&v)
}

/// The signed-in user, or 401.
fn me(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let Some(p) = svc.principal(req) else {
        return Err(Refusal::new(401, "signin_required", "Not signed in."));
    };
    let now = svc.now();
    let window = svc.settings.step_up_secs;
    let v = match &p {
        Principal::Console => json!({
            "console": true,
            "name": "console",
            "role": "admin",
            "stepped_up": true,
            "signin_disabled": svc.disabled(),
        }),
        Principal::User(u) => {
            let host = req.hostname();
            let rp = super::webauthn::rp_id_for(svc.settings.rp_id.as_deref(), &host);
            let (display, failed, kinds, here, elsewhere) = svc.read(|d| {
                let user = d.user(u.user_id);
                let creds: Vec<_> = d.credentials_of(u.user_id).collect();
                let here = creds.iter().any(|c| c.kind.rp_id() == Some(rp.as_str()));
                let mut elsewhere: Vec<String> = creds
                    .iter()
                    .filter_map(|c| c.kind.rp_id())
                    .filter(|r| *r != rp)
                    .map(str::to_string)
                    .collect();
                elsewhere.sort();
                elsewhere.dedup();
                let kinds: Vec<&'static str> = creds.iter().map(|c| c.kind.name()).collect();
                (
                    user.map(|x| x.display_name.clone()).unwrap_or_default(),
                    user.map_or(0, |x| x.totp.since_sign_in),
                    kinds,
                    here,
                    elsewhere,
                )
            });
            let totp_only = !kinds.is_empty() && kinds.iter().all(|k| *k == "totp");
            json!({
                "console": false,
                "id": u.user_id.to_string(),
                "name": u.name,
                "display_name": display,
                "role": u.role.as_str(),
                "stepped_up": p.stepped_up(now, window),
                "step_up_left": window.saturating_sub(now.saturating_sub(u.last_auth_at)),
                "host": host,
                "rp_id": rp,
                "host_has_webauthn": here,
                "webauthn_elsewhere": elsewhere,
                "totp_only": totp_only,
                "failed_codes": failed,
            })
        }
    };
    Ok(ok_json(&v))
}

/// End this session.
fn signout(svc: &AuthService, req: &Req<'_>) -> Result<HttpResponse, Refusal> {
    let token = session::cookie_value(req.cookie, session::session_cookie(req.tls));
    let name = match svc.principal(req) {
        Some(Principal::User(u)) => Some(u.name),
        _ => None,
    };
    if let Some(token) = token.filter(|t| session::is_token(t)) {
        let hash = session::hash_token(token);
        svc.mutate(|d| {
            d.sessions.retain(|s| s.hash != hash);
            Ok(())
        })?;
    }
    if let Some(name) = name {
        svc.audit("sign_out", Some(&name), "", &req.ip());
    }
    Ok(ok_json(&json!({ "ok": true })).with_header(
        "Set-Cookie",
        &session::clear_cookie(session::session_cookie(req.tls), req.tls),
    ))
}
