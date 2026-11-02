// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Who may reach an admin-router route, on either bind.
//!
//! The answer comes from [`crate::access::route_class`] and the request's
//! principal, and nothing else: an unclassified route is refused, a request
//! with no principal is sent to sign in, a viewer is refused admin routes,
//! and a stale session is asked to confirm it's them before the
//! [`RouteClass::AdminStepUp`] routes.

use crate::access::{RouteClass, route_class};
use crate::http::HttpResponse;

use super::refusal_response;
use super::service::{AuthService, Principal, Refusal, Req};

/// The admin pages: a request for one with no session is redirected to
/// sign-in rather than answered 401.
const PAGES: &[&str] = &["/", "/settings", "/discord/admin", "/recordings"];

/// The gate's answer.
pub enum Admission {
    /// Go ahead; the principal, when there is one.
    Allow(Option<Principal>),
    /// Answer this instead.
    Refuse(HttpResponse),
}

/// Admit `req` to the admin route `admin_path` (the path with any `/admin`
/// prefix already removed), or say why not. `original` is the path as the
/// browser asked for it, for the sign-in redirect.
#[must_use]
pub fn admit(svc: &AuthService, req: &Req<'_>, admin_path: &str, original: &str) -> Admission {
    let Some(class) = route_class(req.method, admin_path) else {
        return Admission::Refuse(refusal_response(&Refusal::new(
            403,
            "forbidden",
            "This route is not available.",
        )));
    };
    let principal = svc.principal(req);
    if class == RouteClass::Public {
        return Admission::Allow(principal);
    }
    let Some(p) = principal else {
        return Admission::Refuse(sign_in_needed(req.method, admin_path, original));
    };
    if class != RouteClass::Viewer && !p.is_admin() {
        return Admission::Refuse(refusal_response(&Refusal::new(
            403,
            "admins_only",
            "Only an admin can do that.",
        )));
    }
    if class == RouteClass::AdminStepUp && !p.stepped_up(svc.now(), svc.settings.step_up_secs) {
        return Admission::Refuse(refusal_response(&Refusal::new(
            401,
            "reauth_required",
            "Confirm it's you to do that.",
        )));
    }
    // Saving the public hostnames or the RP ID must not strand credentials
    // unannounced, nor every admin at once.
    if req.method == "POST"
        && admin_path == "/settings"
        && let Some(refused) = super::guard::check_settings_save(svc, req.body)
    {
        return Admission::Refuse(refused);
    }
    // Every key from the web carries a name in the audit. The console is
    // curl and the CLI on the node itself, and is not a web key.
    if let Principal::User(u) = &p
        && req.method == "POST"
        && matches!(admin_path, "/key" | "/unkey")
    {
        let event = if admin_path == "/key" {
            "web_key"
        } else {
            "web_unkey"
        };
        svc.audit(event, Some(&u.name), "", &req.ip());
    }
    Admission::Allow(Some(p))
}

/// The answer to an anonymous request for an admin route: a redirect to
/// sign-in for a page, a 401 for anything else.
#[must_use]
pub fn sign_in_needed(method: &str, admin_path: &str, original: &str) -> HttpResponse {
    if method == "GET" && PAGES.contains(&admin_path) {
        return redirect(&format!("/admin/signin?next={}", encode_next(original)));
    }
    refusal_response(&Refusal::new(401, "signin_required", "Sign in to do that."))
}

/// A 303 to `to`.
#[must_use]
pub fn redirect(to: &str) -> HttpResponse {
    HttpResponse {
        status: 303,
        content_type: "text/plain; charset=utf-8",
        body: b"See Other\n".to_vec(),
        headers: vec![
            ("Location", to.to_string()),
            ("Cache-Control", "no-store".to_string()),
        ],
    }
}

/// Percent-encode a path for the `next` parameter: unreserved characters
/// and `/` pass, everything else is escaped.
#[must_use]
pub fn encode_next(path: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Whether `next` is a path the sign-in page may send a browser to: this
/// origin's `/`, or a path under `/admin/`. Nothing with a scheme, a host,
/// a backslash or a `//`.
#[must_use]
pub fn safe_next(next: &str) -> bool {
    (next == "/" || next.starts_with("/admin/"))
        && !next.contains("//")
        && !next.contains('\\')
        && !next.contains("/../")
        && next.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::service::Surface;
    use crate::auth::service::tests::{Fixture, req};
    use crate::auth::session;
    use crate::auth::store::Role;

    fn status(a: &Admission) -> u16 {
        match a {
            Admission::Allow(_) => 200,
            Admission::Refuse(r) => r.status,
        }
    }

    fn body(a: &Admission) -> serde_json::Value {
        match a {
            Admission::Allow(_) => serde_json::Value::Null,
            Admission::Refuse(r) => serde_json::from_slice(&r.body).unwrap_or_default(),
        }
    }

    #[test]
    fn anonymous_pages_redirect_and_api_calls_get_401() {
        let f = Fixture::new();
        let mut r = req(Surface::Public, None);
        let a = admit(&f.svc, &r, "/settings", "/admin/settings");
        let Admission::Refuse(resp) = &a else {
            panic!()
        };
        assert_eq!(resp.status, 303);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| *k == "Location" && v == "/admin/signin?next=/admin/settings")
        );
        r.method = "POST";
        let a = admit(&f.svc, &r, "/key", "/admin/key");
        assert_eq!(status(&a), 401);
        assert_eq!(body(&a)["error"], "signin_required");
    }

    #[test]
    fn a_viewer_is_refused_and_an_admin_admitted() {
        let f = Fixture::new();
        let viewer = format!(
            "{}={}",
            session::SESSION_COOKIE_TLS,
            f.svc.test_sign_in("V1EW", Role::Viewer)
        );
        let admin = format!(
            "{}={}",
            session::SESSION_COOKIE_TLS,
            f.svc.test_sign_in("A1DM", Role::Admin)
        );
        let mut r = req(Surface::Public, Some(&viewer));
        r.method = "POST";
        assert_eq!(status(&admit(&f.svc, &r, "/key", "/admin/key")), 403);
        r.cookie = Some(&admin);
        assert_eq!(status(&admit(&f.svc, &r, "/key", "/admin/key")), 200);
        let audit = f.svc.read(|d| {
            d.audit
                .iter()
                .map(|e| (e.event.clone(), e.user.clone()))
                .collect::<Vec<_>>()
        });
        assert!(
            audit.contains(&("web_key".to_string(), Some("A1DM".to_string()))),
            "{:?}",
            audit.len()
        );
    }

    #[test]
    fn every_step_up_route_needs_a_fresh_credential() {
        let f = Fixture::new();
        let admin = format!(
            "{}={}",
            session::SESSION_COOKIE_TLS,
            f.svc.test_sign_in("A1DM", Role::Admin)
        );
        let mut r = req(Surface::Public, Some(&admin));
        r.method = "POST";
        let step_up: Vec<&str> = crate::access::ADMIN_ROUTES
            .iter()
            .filter(|(m, p)| route_class(m, p) == Some(RouteClass::AdminStepUp))
            .map(|(_, p)| *p)
            .collect();
        assert_eq!(step_up.len(), 5);
        for p in &step_up {
            assert_eq!(status(&admit(&f.svc, &r, p, p)), 200, "fresh: {p}");
        }
        f.advance(f.svc.settings.step_up_secs + 1);
        for p in &step_up {
            let a = admit(&f.svc, &r, p, p);
            assert_eq!(status(&a), 401, "stale: {p}");
            assert_eq!(body(&a)["error"], "reauth_required");
        }
        assert_eq!(
            status(&admit(&f.svc, &r, "/key", "/key")),
            200,
            "keying needs no step-up"
        );
    }

    #[test]
    fn recordings_refuse_viewers_and_anonymous() {
        let f = Fixture::new();
        let viewer = format!(
            "{}={}",
            session::SESSION_COOKIE_TLS,
            f.svc.test_sign_in("V1EW", Role::Viewer)
        );
        let admin = format!(
            "{}={}",
            session::SESSION_COOKIE_TLS,
            f.svc.test_sign_in("A1DM", Role::Admin)
        );
        let routes = [
            ("GET", "/recordings"),
            ("GET", "/recordings.js"),
            ("GET", "/recordings/state"),
            ("GET", "/recordings/file"),
            ("POST", "/recordings/delete"),
            ("GET", "/recordings.tar"),
        ];
        for (m, p) in routes {
            let original = format!("/admin{p}");
            let mut r = req(Surface::Public, Some(&viewer));
            r.method = m;
            let a = admit(&f.svc, &r, p, &original);
            assert_eq!(status(&a), 403, "viewer: {m} {p}");
            assert_eq!(body(&a)["error"], "admins_only", "viewer: {m} {p}");

            let mut r = req(Surface::Public, None);
            r.method = m;
            let a = admit(&f.svc, &r, p, &original);
            if p == "/recordings" {
                let Admission::Refuse(resp) = &a else {
                    panic!("anonymous page admitted")
                };
                assert_eq!(resp.status, 303);
                assert!(
                    resp.headers
                        .iter()
                        .any(|(k, v)| *k == "Location"
                            && v == "/admin/signin?next=/admin/recordings")
                );
            } else {
                assert_eq!(status(&a), 401, "anonymous: {m} {p}");
                assert_eq!(body(&a)["error"], "signin_required", "anonymous: {m} {p}");
            }

            let mut r = req(Surface::Public, Some(&admin));
            r.method = m;
            assert_eq!(
                status(&admit(&f.svc, &r, p, &original)),
                200,
                "admin: {m} {p}"
            );
        }
    }

    #[test]
    fn an_unclassified_route_is_refused() {
        let f = Fixture::new();
        let mut r = req(Surface::Control, None);
        r.peer = Some("127.0.0.1:1".parse().unwrap());
        assert_eq!(status(&admit(&f.svc, &r, "/nope", "/nope")), 403);
        assert_eq!(
            status(&admit(&f.svc, &r, "/status", "/status")),
            200,
            "console"
        );
    }

    #[test]
    fn next_is_validated() {
        for ok in ["/", "/admin/", "/admin/settings", "/admin/discord/admin"] {
            assert!(safe_next(ok), "{ok}");
        }
        for bad in [
            "https://evil.example/",
            "//evil.example",
            "/admin//evil",
            "/\\evil",
            "/admin/../x",
            "/other",
            "",
            "/admin/ x",
        ] {
            assert!(!safe_next(bad), "{bad}");
        }
        assert_eq!(encode_next("/admin/a b?c"), "/admin/a%20b%3Fc");
    }
}
