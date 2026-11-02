// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Web Transceiver token minting.
//!
//! Two paths, tried in that order:
//!
//! 1. **The sanctioned API** (`POST <origin>/api/v2/auth-wt-legacy`, JSON
//!    `{"username","password"}` → `{"status","auth","token","msg"}`), which is
//!    what `AllStarLink`'s developer documentation publishes. No node is
//!    involved: the token belongs to the account.
//! 2. **The legacy scrape** ([`mint_wt_token_legacy_at`]) — a native port of
//!    scripts/asl-wt-token.py, replicating `DroidStar`'s
//!    `obtain_asl_wt_creds()`: `POST <portal>/login.php` for a cookie jar, then
//!    `GET <portal>/webtransceiver.php?node=<node>` and pull `callingName` out
//!    of the HTML. It needs a node the account OWNS, so it only runs as a
//!    fallback when one is configured.
//!
//! A login the API refuses is never retried against the scrape — wrong
//! credentials are wrong on both paths.
//!
//! The token is used as the IAX2 `CALLING_NAME`; the node's dialplan resolves
//! token → callsign via authwebphone.pl. Mint fresh per session.

use crate::Asl3Error;

const DEFAULT_PORTAL: &str = "https://www.allstarlink.org/portal";

/// The documented Web Transceiver auth endpoint, relative to the portal's
/// origin (<https://allstarlink.github.io/developers/api/#webtransceiver>).
const API_PATH: &str = "/api/v2/auth-wt-legacy";

/// `AllStarLink` portal account credentials. Never logged or echoed in errors.
/// `Clone` so consumers can hold them in cloneable config (the harness's
/// `HarnessDefaults` derives `Clone`).
#[derive(Clone)]
pub struct PortalCredentials {
    /// Portal account callsign.
    pub user: String,
    /// Portal ACCOUNT password (not a node secret).
    pub password: String,
    /// A node the account owns, or empty. The API path does not need one at
    /// all — the token is the account's. It is used ONLY by the legacy scrape
    /// fallback ([`mint_wt_token_legacy_at`]), which cannot mint without it;
    /// an empty node therefore means "API only".
    pub node: String,
}

/// Extract the token from the WT page. Primary: the documented
/// `name="callingName" value="<TOKEN>"` param; fallback: a looser scan in
/// case the markup shifts (ported from the Python script).
fn extract_calling_name(html: &str) -> Option<String> {
    // Primary: name="callingName" ... value="TOKEN"
    if let Some(at) = html.find(r#"name="callingName""#) {
        let rest = &html[at..];
        if let Some(v) = rest.find(r#"value=""#) {
            let tail = &rest[v + 7..];
            let end = tail.find('"')?;
            if end > 0 {
                return Some(tail[..end].to_string());
            }
        }
    }
    // Fallback: callingName ... "..." "TOKEN" (second quoted run after the
    // key). Guard with a token-shape check so stray markup (`/>`, tags) is
    // never mistaken for a token.
    let at = html.find("callingName")?;
    let mut quotes = html[at..].split('"');
    let _before = quotes.next()?; // text up to the first quote
    let _first = quotes.next()?; // first quoted run
    let _between = quotes.next()?; // text between quotes
    let second = quotes.next()?; // second quoted run = the value
    let plausible = !second.is_empty()
        && second
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if plausible {
        Some(second.to_string())
    } else {
        None
    }
}

/// The API lives at the portal's ORIGIN, not under its path: the production
/// base `https://www.allstarlink.org/portal` yields
/// `https://www.allstarlink.org/api/v2/auth-wt-legacy`, and a test stub's
/// `http://127.0.0.1:NNNN` yields `http://127.0.0.1:NNNN/api/v2/auth-wt-legacy`.
fn api_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let origin = match base.find("://") {
        Some(i) => {
            let after = i + 3;
            match base[after..].find('/') {
                Some(j) => &base[..after + j],
                None => base,
            }
        }
        // No scheme: everything up to the first slash is the authority.
        None => base.split('/').next().unwrap_or(base),
    };
    format!("{origin}{API_PATH}")
}

/// What the API attempt settled.
enum ApiMint {
    /// A token came back.
    Token(String),
    /// The API answered in its own protocol and said no. Final: the scrape is
    /// NOT tried (a refused login is refused on both paths).
    Final(Asl3Error),
    /// The API could not be reached, or did not answer with a token —
    /// transport failure, a non-JSON body, an unexpected status (a 404 because
    /// the endpoint moved, a 5xx), or a 2xx that carried no token. The caller
    /// may fall back to the scrape; the string is the message for the `Http`
    /// error it raises when it cannot, and always names the path and the
    /// reason.
    Unavailable(String),
}

/// Mint through the documented API. Never returns credentials in any string.
fn mint_via_api(base_url: &str, creds: &PortalCredentials) -> ApiMint {
    let url = api_url(base_url);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    // Built with serde_json, never format!: the password is arbitrary text and
    // must be JSON-escaped by something that knows the grammar.
    let body = serde_json::json!({
        "username": creds.user,
        "password": creds.password,
    })
    .to_string();

    let (status, text) = match agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body)
    {
        Ok(resp) => {
            let status = resp.status();
            match resp.into_string() {
                Ok(text) => (status, text),
                Err(e) => {
                    return ApiMint::Unavailable(format!(
                        "POST {API_PATH}: HTTP {status}, unreadable body: {e}"
                    ));
                }
            }
        }
        // ureq treats 4xx/5xx as an error; the body is the API's own JSON.
        Err(ureq::Error::Status(code, resp)) => (code, resp.into_string().unwrap_or_default()),
        Err(e) => return ApiMint::Unavailable(format!("POST {API_PATH}: {e}")),
    };

    // A 401 is a refusal whatever the body looks like — the API's own JSON, an
    // nginx error page, a WAF challenge. Decide it BEFORE parsing: an
    // HTML-wrapped refusal must not become "unavailable" and send the password
    // off to the scrape a second time.
    if status == 401 {
        return ApiMint::Final(Asl3Error::Login);
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return ApiMint::Unavailable(format!(
            "POST {API_PATH}: HTTP {status}, response was not JSON"
        ));
    };

    // Success is a non-empty token. `status`/`auth` are read for the error
    // message only — never required to hold particular values.
    let token = json
        .get("token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if !token.is_empty() {
        return ApiMint::Token(token.to_string());
    }

    // No token. An `auth: 0` body whose msg speaks of the login is the endpoint
    // refusing these credentials with a 200-shaped answer (the live refusal is
    // the 401 handled above: {"status":"ERR","auth":0,"token":"","msg":"login
    // failed"}, probed 2026-09-08).
    let auth_zero = json.get("auth").is_some_and(|v| {
        v.as_i64() == Some(0) || v.as_bool() == Some(false) || v.as_str() == Some("0")
    });
    let msg = json
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if auth_zero && msg.to_ascii_lowercase().contains("login") {
        return ApiMint::Final(Asl3Error::Login);
    }
    // Anything else — including a 2xx that carried no token at all — is
    // "unavailable", not final. Deliberately SOFT until the live success shape
    // has been seen once: if the token turns out to live under another key, an
    // account with a node still mints through the scrape instead of failing.
    // Tighten this arm to `Final(TokenNotFound)` once a real success response
    // has been observed.
    ApiMint::Unavailable(format!(
        "POST {API_PATH}: HTTP {status}, no token in the response"
    ))
}

/// Mint against an explicit portal base URL — the testable core. Public so a
/// consumer can point the mint at a staging/stub portal (e.g. an offline test
/// harness); production callers use [`mint_wt_token`], which targets the live
/// `AllStarLink` portal.
///
/// Tries the documented API at the base URL's origin first; falls back to
/// [`mint_wt_token_legacy_at`] only when the API is unreachable AND a node is
/// configured (the scrape cannot work without one). A login the API refuses is
/// never retried.
///
/// # Errors
/// Same categories as [`mint_wt_token`].
pub fn mint_wt_token_at(base_url: &str, creds: &PortalCredentials) -> Result<String, Asl3Error> {
    match mint_via_api(base_url, creds) {
        ApiMint::Token(token) => Ok(token),
        ApiMint::Final(e) => Err(e),
        ApiMint::Unavailable(why) => {
            if creds.node.is_empty() {
                Err(Asl3Error::Http(why))
            } else {
                mint_wt_token_legacy_at(base_url, creds)
            }
        }
    }
}

/// The legacy portal scrape, kept as the fallback for when the documented API
/// is unreachable: log in for a cookie jar, fetch the web-transceiver page for
/// the node, and pull `callingName` out of the HTML. Public so a diagnostic can
/// compare the two paths against the same account; [`mint_wt_token_at`] calls it
/// only when the API failed and `creds.node` is non-empty — the portal only
/// emits tokens for nodes the account OWNS.
///
/// # Errors
/// Same categories as [`mint_wt_token`].
pub fn mint_wt_token_legacy_at(
    base_url: &str,
    creds: &PortalCredentials,
) -> Result<String, Asl3Error> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        // The portal's login.php answers with a 302 whose Set-Cookie carries
        // the PHPSESSID. ureq (without the `cookies` feature) drops that
        // header when it follows the redirect, so never follow — inspect the
        // 302 itself.
        .redirects(0)
        .build();

    // 1. Login. Success signal = a PHPSESSID cookie on the response. With
    // `redirects(0)`, ureq returns 3xx responses as `Ok` (Error::Status is
    // only for >= 400), so this one arm receives BOTH a direct 200 and the
    // usual 302-after-login — either may carry the cookie.
    let login = agent
        .post(&format!("{base_url}/login.php"))
        .send_form(&[
            ("user", creds.user.as_str()),
            ("pass", creds.password.as_str()),
        ])
        .map_err(|e| Asl3Error::Http(e.to_string()))?;
    // The portal authenticates the NEXT request with the WHOLE cookie set —
    // live login.php sets PHPSESSID *and* an `allstar_token` JWT (plus a
    // deletion for `allstar_become`). Forwarding only PHPSESSID renders the
    // WT page unauthenticated (no token in the HTML), so mirror a cookie
    // jar: keep every name=value pair except ones the server is expiring.
    // PHPSESSID presence stays the login-success signal (as in the Python
    // original).
    let cookies: Vec<String> = login
        .all("set-cookie")
        .iter()
        .filter(|c| {
            let attrs = c.to_ascii_lowercase();
            !attrs.contains("max-age=0") && !attrs.contains("expires=thu, 01 jan 1970")
        })
        .filter_map(|c| c.split(';').next())
        .map(ToString::to_string)
        .collect();
    if !cookies.iter().any(|c| c.starts_with("PHPSESSID")) {
        return Err(Asl3Error::Login);
    }
    let cookie = cookies.join("; ");

    // 2. Fetch the WT page with the session cookies. The node is optional:
    // an empty one is left off the query rather than sent as `node=`.
    let mut wt = agent.get(&format!("{base_url}/webtransceiver.php"));
    if !creds.node.is_empty() {
        wt = wt.query("node", &creds.node);
    }
    let html = wt
        .set("Cookie", &cookie)
        .call()
        .map_err(|e| Asl3Error::Http(e.to_string()))?
        .into_string()
        .map_err(|e| Asl3Error::Http(e.to_string()))?;

    // 3. Extract.
    let token = extract_calling_name(&html).ok_or(Asl3Error::TokenNotFound)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(Asl3Error::TokenNotFound);
    }
    Ok(token.to_string())
}

/// Mint a fresh Web Transceiver token from the `AllStarLink` portal.
///
/// # Errors
/// [`Asl3Error::Login`] when the credentials are refused (the API says so, or
/// the fallback portal login issues no session cookie);
/// [`Asl3Error::TokenNotFound`] when the account is authenticated but no token
/// came back (no WT access for the node, or markup changed);
/// [`Asl3Error::Http`] for transport failures and for an API that answered
/// nothing usable when no node is configured to fall back with.
pub fn mint_wt_token(creds: &PortalCredentials) -> Result<String, Asl3Error> {
    mint_wt_token_at(DEFAULT_PORTAL, creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_http::Response;
    use astar_http::testing::{Recorded, serve};

    fn creds(user: &str, password: &str, node: &str) -> PortalCredentials {
        PortalCredentials {
            user: user.into(),
            password: password.into(),
            node: node.into(),
        }
    }

    #[test]
    fn extracts_the_documented_param_markup() {
        let html = r#"<object><param name="callingName" value="84906e5c0000"/></object>"#;
        assert_eq!(extract_calling_name(html).unwrap(), "84906e5c0000");
    }

    #[test]
    fn loose_fallback_survives_markup_drift() {
        let html = r#"var callingName = "x"; token "abc123";"#;
        assert_eq!(extract_calling_name(html).unwrap(), "abc123");
    }

    #[test]
    fn missing_token_is_none() {
        assert!(extract_calling_name("<html>Node not found</html>").is_none());
        assert!(extract_calling_name(r#"name="callingName" value=""/>"#).is_none());
    }

    /// The API hangs off the ORIGIN of the portal base, never under its path.
    #[test]
    fn api_url_is_the_origin_of_the_portal_base() {
        assert_eq!(
            api_url("https://www.allstarlink.org/portal"),
            "https://www.allstarlink.org/api/v2/auth-wt-legacy"
        );
        assert_eq!(
            api_url("http://127.0.0.1:8123"),
            "http://127.0.0.1:8123/api/v2/auth-wt-legacy"
        );
        assert_eq!(
            api_url("http://127.0.0.1:8123/"),
            "http://127.0.0.1:8123/api/v2/auth-wt-legacy"
        );
    }

    /// The documented API is the primary path: one JSON POST, a token back,
    /// and neither portal page is ever fetched.
    #[test]
    fn api_mint_succeeds_and_never_touches_the_portal_pages() {
        let stub = serve(|_| {
            Response::text(
                200,
                r#"{"status":"OK","auth":1,"token":"tok-api-1234","msg":"ok"}"#,
            )
        });
        let token = mint_wt_token_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", ""))
            .expect("API mint succeeds");
        assert_eq!(token, "tok-api-1234");
        let seen = stub.requests();
        assert_eq!(seen.len(), 1, "no portal page was fetched: {seen:?}");
        assert_eq!(seen[0].target, "/api/v2/auth-wt-legacy", "API path");
        assert_eq!(seen[0].method, "POST");
        let ctype = seen[0].header("Content-Type").unwrap_or_default();
        assert!(
            ctype.starts_with("application/json"),
            "content type: {ctype}"
        );
        let sent: serde_json::Value = serde_json::from_str(seen[0].body_str()).expect("JSON body");
        assert_eq!(
            sent,
            serde_json::json!({
                "username": "AJ7HR",
                "password": "not-a-real-password",
            }),
            "exactly the two documented fields"
        );
    }

    /// A refused login is refused on both paths: the scrape is NOT tried, so
    /// the stub sees exactly one request (a fallback would be a second).
    #[test]
    fn api_refusal_is_login_and_does_not_fall_back() {
        // The live refusal shape, probed 2026-09-08.
        let stub = serve(|_| {
            Response::text(
                401,
                r#"{"status":"ERR","auth":0,"token":"","msg":"login failed"}"#,
            )
        });
        assert!(matches!(
            mint_wt_token_at(&stub.url(), &creds("AJ7HR", "wrong", "77777")),
            Err(Asl3Error::Login)
        ));
        let seen = stub.requests();
        assert_eq!(seen.len(), 1, "no fallback: {seen:?}");
        assert_eq!(seen[0].target, "/api/v2/auth-wt-legacy");
    }

    /// The endpoint gone (404) with a node configured: fall back to the
    /// scrape, exactly as before the API existed.
    #[test]
    fn api_missing_falls_back_to_the_scrape_when_a_node_is_configured() {
        let stub = serve(|rq: &Recorded| {
            if rq.target.starts_with(API_PATH) {
                Response::text(404, "not found")
            } else if rq.target.starts_with("/login.php") {
                Response::text(200, "ok").header("Set-Cookie", "PHPSESSID=stubfb; path=/")
            } else {
                let has_cookie = rq
                    .header("Cookie")
                    .is_some_and(|c| c.contains("PHPSESSID=stubfb"));
                Response::text(
                    200,
                    if has_cookie && rq.target.contains("node=77777") {
                        r#"<param name="callingName" value="tok-scraped"/>"#
                    } else {
                        "Node not found"
                    },
                )
            }
        });
        let token = mint_wt_token_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", "77777"))
            .expect("falls back to the scrape");
        assert_eq!(token, "tok-scraped");
        let seen = stub.requests();
        assert_eq!(seen.len(), 3, "API, login, page: {seen:?}");
        let login = seen
            .iter()
            .find(|r| r.target.starts_with("/login.php"))
            .expect("login request");
        assert!(
            login.body_str().contains("user=AJ7HR"),
            "form-encoded login: {}",
            login.body_str()
        );
    }

    /// The endpoint gone and NO node: the scrape cannot work without one, so
    /// the API failure surfaces as `Http` naming the path and status — one
    /// request, no fallback attempt.
    #[test]
    fn api_missing_without_a_node_is_an_http_error() {
        let stub = serve(|_| Response::text(404, "not found"));
        let err = mint_wt_token_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", ""))
            .expect_err("no node, no fallback");
        match err {
            Asl3Error::Http(msg) => {
                assert!(msg.contains(API_PATH), "names the API path: {msg}");
                assert!(msg.contains("404"), "names the status: {msg}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        let seen = stub.requests();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].target, "/api/v2/auth-wt-legacy");
    }

    /// A 401 is a refusal whatever wrapping it arrives in: an nginx/WAF page
    /// in front of the API must not read as "unavailable" and send the
    /// password off to the scrape. One request seen — a fallback would be a
    /// second.
    #[test]
    fn api_401_with_an_html_body_is_still_login() {
        let stub = serve(|_| Response::text(401, "<html><body>401 Unauthorized</body></html>"));
        assert!(matches!(
            mint_wt_token_at(&stub.url(), &creds("AJ7HR", "wrong", "77777")),
            Err(Asl3Error::Login)
        ));
        let seen = stub.requests();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].target, "/api/v2/auth-wt-legacy");
    }

    /// A 2xx that carried no token is deliberately SOFT (see `mint_via_api`):
    /// until the live success shape has been seen once, an account with a node
    /// still mints through the scrape rather than failing outright.
    #[test]
    fn api_ok_with_empty_token_falls_back_to_the_scrape() {
        let stub = serve(|rq: &Recorded| {
            if rq.target.starts_with(API_PATH) {
                Response::text(200, r#"{"status":"OK","auth":1,"token":"","msg":"ok"}"#)
            } else if rq.target.starts_with("/login.php") {
                Response::text(200, "ok").header("Set-Cookie", "PHPSESSID=stubsoft; path=/")
            } else {
                Response::text(
                    200,
                    if rq.target.contains("node=77777") {
                        r#"<param name="callingName" value="tok-soft"/>"#
                    } else {
                        "Node not found"
                    },
                )
            }
        });
        let token = mint_wt_token_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", "77777"))
            .expect("falls back to the scrape");
        assert_eq!(token, "tok-soft");
        assert_eq!(stub.requests().len(), 3);
    }

    /// The same empty-token answer with no node to fall back with: `Http`
    /// naming the API path and why, not a silent `TokenNotFound`.
    #[test]
    fn api_ok_with_empty_token_without_a_node_is_an_http_error() {
        let stub =
            serve(|_| Response::text(200, r#"{"status":"OK","auth":1,"token":"","msg":"ok"}"#));
        let err = mint_wt_token_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", ""))
            .expect_err("no node, no fallback");
        match err {
            Asl3Error::Http(msg) => {
                assert!(msg.contains(API_PATH), "names the API path: {msg}");
                assert!(msg.contains("no token"), "names the reason: {msg}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        let seen = stub.requests();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].target, "/api/v2/auth-wt-legacy");
    }

    /// Offline integration: a stub plays the portal. Login sets the cookie;
    /// the WT page requires it and embeds the token.
    #[test]
    fn mint_flow_against_local_portal_stub() {
        let stub = serve(|rq: &Recorded| {
            if rq.target.starts_with("/login.php") {
                // Mirror the LIVE portal (verified 2026-06-12): login sets
                // the session id, an auth JWT, AND expires a stale cookie.
                Response::text(200, "ok")
                    .header("Set-Cookie", "PHPSESSID=stub123; path=/")
                    .header("Set-Cookie", "allstar_token=jwt-abc; path=/")
                    .header(
                        "Set-Cookie",
                        "allstar_become=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0; path=/",
                    )
            } else {
                // The WT page authenticates on the FULL cookie set: both
                // the session id and the JWT must arrive (only PHPSESSID
                // renders an unauthenticated page on the live portal),
                // and the expired cookie must NOT be echoed back.
                let cookie_hdr = rq.header("Cookie").unwrap_or_default();
                let authed = cookie_hdr.contains("PHPSESSID=stub123")
                    && cookie_hdr.contains("allstar_token=jwt-abc")
                    && !cookie_hdr.contains("allstar_become");
                Response::text(
                    200,
                    if authed && rq.target.contains("node=77777") {
                        r#"<param name="callingName" value="tok-456789"/>"#
                    } else {
                        "Node not found"
                    },
                )
            }
        });
        let token =
            mint_wt_token_legacy_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", "77777"))
                .expect("mint succeeds");
        assert_eq!(token, "tok-456789");
        let seen = stub.requests();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].body_str().contains("user=AJ7HR"),
            "form-encoded login: {}",
            seen[0].body_str()
        );
    }

    /// Real PHP portals answer login.php with a 302 that carries BOTH the
    /// session cookie and a Location. With `.redirects(0)` the cookie must be
    /// read off the 302 itself and `/portal/` must never be fetched: the stub
    /// must see exactly two requests, the second the WT page.
    #[test]
    fn mint_flow_with_302_login_redirect() {
        let stub = serve(|rq: &Recorded| {
            if rq.target.starts_with("/login.php") {
                Response::new(302)
                    .header("Location", "/portal/")
                    .header("Set-Cookie", "PHPSESSID=stub302; path=/")
            } else {
                let has_cookie = rq
                    .header("Cookie")
                    .is_some_and(|c| c.contains("PHPSESSID=stub302"));
                Response::text(
                    200,
                    if has_cookie && rq.target.contains("node=77777") {
                        r#"<param name="callingName" value="tok-302302"/>"#
                    } else {
                        "Node not found"
                    },
                )
            }
        });
        let token =
            mint_wt_token_legacy_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", "77777"))
                .expect("mint succeeds despite 302 login");
        assert_eq!(token, "tok-302302");
        let seen = stub.requests();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[0].body_str().contains("user=AJ7HR"),
            "form-encoded login: {}",
            seen[0].body_str()
        );
        assert!(
            seen[1].target.starts_with("/webtransceiver.php"),
            "redirect must not be followed, got {}",
            seen[1].target
        );
    }

    /// An empty node sends no `node` query at all (the portal mints without
    /// one); the stub refuses any request that still carries the parameter.
    #[test]
    fn empty_node_is_left_off_the_query() {
        let stub = serve(|rq: &Recorded| {
            if rq.target.starts_with("/login.php") {
                Response::text(200, "ok").header("Set-Cookie", "PHPSESSID=stubnode; path=/")
            } else {
                Response::text(
                    200,
                    if rq.target == "/webtransceiver.php" {
                        r#"<param name="callingName" value="tok-nonode"/>"#
                    } else {
                        "unexpected query"
                    },
                )
            }
        });
        let token =
            mint_wt_token_legacy_at(&stub.url(), &creds("AJ7HR", "not-a-real-password", ""))
                .expect("mint succeeds without a node");
        assert_eq!(token, "tok-nonode");
    }

    #[test]
    fn no_cookie_means_login_error() {
        let stub = serve(|_| Response::text(200, "nope"));
        assert!(matches!(
            mint_wt_token_legacy_at(&stub.url(), &creds("AJ7HR", "wrong", "1")),
            Err(Asl3Error::Login)
        ));
    }
}
