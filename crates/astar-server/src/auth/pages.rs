// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The sign-in pages, embedded: static shells that fetch `/auth/*`. Each is
//! served to anyone (the API behind it decides what they may see), under
//! the same policy as the admin pages: no inline script or style, no other
//! origin, no framing. Their shared script and stylesheet (`auth.js`,
//! `auth.css`) are the admin router's, so the node's own pages can carry
//! the user chip too.

use crate::http::HttpResponse;

use super::service::{AuthService, Req};

const SIGNIN_HTML: &str = include_str!("../assets/signin.html");
const SIGNIN_JS: &str = include_str!("../assets/signin.js");
const INVITE_HTML: &str = include_str!("../assets/invite.html");
const INVITE_JS: &str = include_str!("../assets/invite.js");
const ACCOUNT_HTML: &str = include_str!("../assets/account.html");
const ACCOUNT_JS: &str = include_str!("../assets/account.js");
const USERS_HTML: &str = include_str!("../assets/users.html");
const USERS_JS: &str = include_str!("../assets/users.js");

/// Every page and script this module serves, by path.
pub const PAGES: &[(&str, &str, &str)] = &[
    ("/admin/signin", "text/html; charset=utf-8", SIGNIN_HTML),
    (
        "/admin/signin.js",
        "application/javascript; charset=utf-8",
        SIGNIN_JS,
    ),
    ("/admin/invite", "text/html; charset=utf-8", INVITE_HTML),
    (
        "/admin/invite.js",
        "application/javascript; charset=utf-8",
        INVITE_JS,
    ),
    ("/admin/account", "text/html; charset=utf-8", ACCOUNT_HTML),
    (
        "/admin/account.js",
        "application/javascript; charset=utf-8",
        ACCOUNT_JS,
    ),
    ("/admin/users", "text/html; charset=utf-8", USERS_HTML),
    (
        "/admin/users.js",
        "application/javascript; charset=utf-8",
        USERS_JS,
    ),
];

/// Answer a request for a sign-in page or its script, or `None`.
#[must_use]
pub fn handle(_svc: &AuthService, req: &Req<'_>) -> Option<HttpResponse> {
    if req.method != "GET" && req.method != "HEAD" {
        return None;
    }
    let (_, ctype, body) = PAGES.iter().find(|(p, _, _)| *p == req.path)?;
    Some(crate::http::static_asset(ctype, body).with_header("Cache-Control", "no-cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sign-in pages keep the admin pages' bargain with the CSP: no
    /// inline script or style, no handler attributes, and no script that
    /// parses a string as HTML. Network strings (names, nicknames, user
    /// agents) are the input that matters, and they only ever reach
    /// `textContent`.
    fn ctype_is_js(path: &str) -> bool {
        PAGES
            .iter()
            .any(|(p, c, _)| *p == path && c.starts_with("application/javascript"))
    }

    #[test]
    fn the_sign_in_pages_never_build_markup() {
        for (path, _, body) in PAGES {
            if ctype_is_js(path) {
                for sink in [
                    "innerHTML",
                    "outerHTML",
                    "insertAdjacentHTML",
                    "document.write",
                    "createContextualFragment",
                    "eval(",
                    "new Function",
                ] {
                    assert!(!body.contains(sink), "{path} reaches for {sink}");
                }
                continue;
            }
            assert!(!body.contains(" style=\""), "{path} has an inline style");
            assert!(!body.contains("<style"), "{path} has a style block");
            for (i, _) in body.match_indices("<script") {
                let tag = &body[i..];
                let end = tag.find("</script>").expect("closed");
                assert!(
                    tag.starts_with("<script src=\"") && tag[..end].ends_with("\">"),
                    "{path}: inline script near byte {i}"
                );
            }
            for (i, _) in body.match_indices(" on") {
                let rest = &body[i + 3..];
                let n = rest.chars().take_while(char::is_ascii_lowercase).count();
                assert!(
                    n == 0 || !rest[n..].starts_with('='),
                    "{path}: inline handler near byte {i}"
                );
            }
            for asset in ["style.css", "auth.css", "auth.js"] {
                assert!(
                    body.contains(&format!("\"{asset}\"")),
                    "{path} loads {asset}"
                );
            }
            assert!(
                !body.contains("http://") && !body.contains("https://"),
                "{path} names no other origin"
            );
        }
    }

    /// Every colour in auth.css is a token, as in style.css.
    #[test]
    fn auth_css_names_colours_only_as_tokens() {
        let css = include_str!("../assets/auth.css");
        let (_, rest) = css
            .split_once("/* ---- end of tokens ---- */")
            .expect("the token block ends at the marker");
        // `rgba(var(--x-rgb), a)` is a token; a literal number after the
        // parenthesis is not.
        for bad in [
            "#", "rgb(0", "rgb(1", "rgb(2", "rgba(0", "rgba(1", "rgba(2", "hsl(",
        ] {
            assert!(!rest.contains(bad), "auth.css names a colour: {bad}");
        }
    }

    #[test]
    fn pages_are_served_by_get_only() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AuthService::open(super::super::AuthSettings::defaults(dir.path()));
        let mut req = super::super::service::tests::req(super::super::Surface::Public, None);
        req.path = "/admin/signin";
        let r = handle(&svc, &req).expect("served");
        assert_eq!(r.status, 200);
        assert!(
            r.headers
                .iter()
                .any(|(k, _)| *k == "Content-Security-Policy")
        );
        req.method = "POST";
        assert!(handle(&svc, &req).is_none());
        req.method = "GET";
        req.path = "/admin/nope";
        assert!(handle(&svc, &req).is_none());
    }
}
