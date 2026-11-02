// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Who may reach which route of the admin router: one table, one
//! classification, and the loopback-peer rule.
//!
//! [`ADMIN_ROUTES`] is every `(method, path)` the admin router
//! ([`crate::http::handle_request_with_headers`]) answers, plus the event
//! stream the server layer answers before it. The router refuses anything
//! not listed with its 404, so a route added to the router's `match`
//! without a line here is dead code rather than an unguarded door, and a
//! test fails until [`route_class`] classifies every line. Nothing reaches
//! the router from a browser that has not been classified first: the
//! control port's gate and the public bind's `/admin/` group both ask
//! [`route_class`] before they forward.

/// How much a caller needs to reach a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteClass {
    /// Anyone who passes the `Host` allow-list: the shared stylesheets, the
    /// sign-in script and the favicon answer, which carry nothing about the
    /// node.
    Public,
    /// A signed-in viewer or admin.
    Viewer,
    /// A signed-in admin.
    Admin,
    /// An admin who used a credential within the step-up window: secrets,
    /// saving settings, restart and shutdown.
    AdminStepUp,
}

/// Every route the admin router answers, as `(method, path)`. `GET /events`
/// and the two recording downloads are answered by the server layer before
/// the router, and are listed so they are classified like the rest.
pub const ADMIN_ROUTES: &[(&str, &str)] = &[
    // The status page and its assets.
    ("GET", "/"),
    ("GET", "/app.js"),
    ("GET", "/style.css"),
    ("GET", "/auth.js"),
    ("GET", "/auth.css"),
    ("GET", "/favicon.ico"),
    ("GET", "/status"),
    ("GET", "/events"),
    // Call control, PTT, the listener, registration, devices.
    ("POST", "/dial"),
    ("POST", "/hangup"),
    ("POST", "/answer"),
    ("POST", "/reject"),
    ("POST", "/key"),
    ("POST", "/unkey"),
    ("POST", "/enable_inbound"),
    ("POST", "/disable_inbound"),
    ("POST", "/register"),
    ("POST", "/deregister"),
    ("POST", "/devices"),
    ("POST", "/secrets"),
    ("POST", "/link"),
    ("POST", "/m17"),
    ("POST", "/echolink"),
    ("POST", "/bridge"),
    ("POST", "/discord"),
    // The Discord admin page.
    ("GET", "/discord/admin"),
    ("GET", "/discord/admin.js"),
    ("GET", "/discord/state"),
    ("POST", "/discord/admin"),
    // The settings page.
    ("GET", "/settings"),
    ("GET", "/settings.js"),
    ("GET", "/settings/state"),
    ("POST", "/settings"),
    ("POST", "/settings/credential"),
    ("POST", "/settings/portal/test"),
    ("POST", "/settings/restart"),
    // The recordings page. The two downloads are answered by the server
    // layer as streams (`crate::recording::http::download_for`), after the
    // gate, like `/events`.
    ("GET", "/recordings"),
    ("GET", "/recordings.js"),
    ("GET", "/recordings/state"),
    ("GET", "/recordings/file"),
    ("POST", "/recordings/delete"),
    ("GET", "/recordings.tar"),
    // Lifecycle.
    ("POST", "/shutdown"),
];

/// Whether `(method, path)` is a route of the admin router.
#[must_use]
pub fn is_admin_route(method: &str, path: &str) -> bool {
    ADMIN_ROUTES.iter().any(|&(m, p)| m == method && p == path)
}

/// The class of an admin-router route, or `None` for anything not in
/// [`ADMIN_ROUTES`] — which every caller refuses.
///
/// Keying and linking are `Admin`, not `AdminStepUp`, on purpose: they are
/// the operator's everyday actions, every key is bounded by the transmit
/// time-out, `key_refusal` still applies, and every key from the web is
/// audited with the user's name.
#[must_use]
pub fn route_class(method: &str, path: &str) -> Option<RouteClass> {
    if !is_admin_route(method, path) {
        return None;
    }
    Some(match (method, path) {
        ("GET", "/style.css" | "/auth.js" | "/auth.css" | "/favicon.ico") => RouteClass::Public,
        (
            "POST",
            "/secrets" | "/settings" | "/settings/credential" | "/settings/restart" | "/shutdown",
        ) => RouteClass::AdminStepUp,
        _ => RouteClass::Admin,
    })
}

/// The admin path under the `/admin/` prefix: `/admin/` → `/`,
/// `/admin/status` → `/status`. `None` for anything else, `/admin` itself
/// included (it redirects to `/admin/`, so relative URLs resolve).
#[must_use]
pub fn strip_admin_prefix(path: &str) -> Option<&str> {
    path.strip_prefix("/admin")
        .filter(|rest| rest.starts_with('/'))
}

/// Whether a request is from a loopback peer that may act as the built-in
/// `console` principal: a loopback address, and no sign that a proxy on
/// this box relayed it from somewhere else. The docs say never to put a
/// proxy in front of the control port. One rule for every server:
/// [`astar_http::guards::is_loopback_peer`].
pub use astar_http::guards::is_loopback_peer as is_trusted_loopback_peer;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn every_listed_route_is_classified() {
        for &(m, p) in ADMIN_ROUTES {
            assert!(route_class(m, p).is_some(), "{m} {p} is unclassified");
        }
    }

    #[test]
    fn an_unlisted_route_has_no_class() {
        assert_eq!(route_class("GET", "/nope"), None);
        assert_eq!(route_class("GET", "/key"), None, "method matters");
        assert_eq!(
            route_class("POST", "/admin/key"),
            None,
            "prefix is stripped first"
        );
    }

    #[test]
    fn the_dangerous_routes_need_step_up() {
        for p in [
            "/secrets",
            "/settings",
            "/settings/credential",
            "/settings/restart",
            "/shutdown",
        ] {
            assert_eq!(route_class("POST", p), Some(RouteClass::AdminStepUp), "{p}");
        }
        for p in ["/key", "/unkey", "/link", "/dial", "/settings/portal/test"] {
            assert_eq!(route_class("POST", p), Some(RouteClass::Admin), "{p}");
        }
        assert_eq!(
            route_class("GET", "/settings/state"),
            Some(RouteClass::Admin)
        );
        assert_eq!(route_class("GET", "/events"), Some(RouteClass::Admin));
        assert_eq!(route_class("GET", "/style.css"), Some(RouteClass::Public));
        for (m, p) in [
            ("GET", "/recordings"),
            ("GET", "/recordings.js"),
            ("GET", "/recordings/state"),
            ("GET", "/recordings/file"),
            ("POST", "/recordings/delete"),
            ("GET", "/recordings.tar"),
        ] {
            assert_eq!(route_class(m, p), Some(RouteClass::Admin), "{m} {p}");
        }
    }

    #[test]
    fn echolink_route_is_admin_class() {
        assert_eq!(route_class("POST", "/echolink"), Some(RouteClass::Admin));
        assert_eq!(route_class("GET", "/echolink"), None, "only the POST");
    }

    #[test]
    fn the_prefix_strips_once() {
        assert_eq!(strip_admin_prefix("/admin/"), Some("/"));
        assert_eq!(strip_admin_prefix("/admin/status"), Some("/status"));
        assert_eq!(strip_admin_prefix("/admin/admin/key"), Some("/admin/key"));
        assert_eq!(strip_admin_prefix("/admin"), None);
        assert_eq!(strip_admin_prefix("/administrator"), None);
        assert_eq!(strip_admin_prefix("/status"), None);
    }

    #[test]
    fn loopback_peers_without_proxy_headers_are_trusted() {
        let at = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        assert!(is_trusted_loopback_peer(at("127.0.0.1:5000"), false));
        assert!(is_trusted_loopback_peer(at("127.8.9.1:5000"), false));
        assert!(is_trusted_loopback_peer(at("[::1]:5000"), false));
        assert!(is_trusted_loopback_peer(
            at("[::ffff:127.0.0.1]:5000"),
            false
        ));
        assert!(!is_trusted_loopback_peer(at("127.0.0.1:5000"), true));
        assert!(!is_trusted_loopback_peer(at("192.168.1.5:5000"), false));
        assert!(!is_trusted_loopback_peer(
            at("[::ffff:10.0.0.1]:5000"),
            false
        ));
        assert!(!is_trusted_loopback_peer(None, false));
    }
}
