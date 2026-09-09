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

    /// Read a request's body and its content type.
    fn body_and_type(rq: &mut tiny_http::Request) -> (String, String) {
        let ctype = rq
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Type"))
            .map(|h| h.value.as_str().to_string())
            .unwrap_or_default();
        let mut body = String::new();
        let _ = rq.as_reader().read_to_string(&mut body);
        (body, ctype)
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
    /// and neither portal page is ever fetched (the stub serves exactly one
    /// request, so a scrape would hang the join on a closed server).
    #[test]
    fn api_mint_succeeds_and_never_touches_the_portal_pages() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let Ok(mut rq) = server.recv() else { return };
            assert_eq!(rq.url(), "/api/v2/auth-wt-legacy", "API path");
            assert_eq!(rq.method().as_str(), "POST");
            let (body, ctype) = body_and_type(&mut rq);
            assert!(
                ctype.starts_with("application/json"),
                "content type: {ctype}"
            );
            let sent: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
            assert_eq!(
                sent,
                serde_json::json!({
                    "username": "AJ7HR",
                    "password": "not-a-real-password",
                }),
                "exactly the two documented fields"
            );
            let _ = rq.respond(tiny_http::Response::from_string(
                r#"{"status":"OK","auth":1,"token":"tok-api-1234","msg":"ok"}"#,
            ));
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: String::new(),
        };
        let token = mint_wt_token_at(&base, &creds).expect("API mint succeeds");
        assert_eq!(token, "tok-api-1234");
        handle.join().unwrap();
    }

    /// A refused login is refused on both paths: the scrape is NOT tried, so
    /// the stub serves exactly one request (a fallback would be a second).
    #[test]
    fn api_refusal_is_login_and_does_not_fall_back() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let Ok(rq) = server.recv() else { return };
            assert_eq!(rq.url(), "/api/v2/auth-wt-legacy");
            // The live refusal shape, probed 2026-09-08.
            let _ = rq.respond(
                tiny_http::Response::from_string(
                    r#"{"status":"ERR","auth":0,"token":"","msg":"login failed"}"#,
                )
                .with_status_code(401),
            );
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "wrong".into(),
            node: "77777".into(),
        };
        assert!(matches!(
            mint_wt_token_at(&base, &creds),
            Err(Asl3Error::Login)
        ));
        handle.join().unwrap();
    }

    /// The endpoint gone (404) with a node configured: fall back to the
    /// scrape, exactly as before the API existed. Note the request count is a
    /// ceiling, not an assertion: if a change made the engine send FEWER
    /// requests than the stub is waiting for, this test hangs at
    /// `handle.join()` rather than failing with a message.
    #[test]
    fn api_missing_falls_back_to_the_scrape_when_a_node_is_configured() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let Ok(mut rq) = server.recv() else { return };
                let url = rq.url().to_string();
                if url.starts_with(API_PATH) {
                    let _ = rq.respond(
                        tiny_http::Response::from_string("not found").with_status_code(404),
                    );
                } else if url.starts_with("/login.php") {
                    let mut body = String::new();
                    let _ = rq.as_reader().read_to_string(&mut body);
                    assert!(body.contains("user=AJ7HR"), "form-encoded login: {body}");
                    let resp = tiny_http::Response::from_string("ok").with_header(
                        tiny_http::Header::from_bytes(
                            &b"Set-Cookie"[..],
                            &b"PHPSESSID=stubfb; path=/"[..],
                        )
                        .unwrap(),
                    );
                    let _ = rq.respond(resp);
                } else {
                    let has_cookie = rq.headers().iter().any(|h| {
                        h.field.equiv("Cookie") && h.value.as_str().contains("PHPSESSID=stubfb")
                    });
                    let body = if has_cookie && url.contains("node=77777") {
                        r#"<param name="callingName" value="tok-scraped"/>"#
                    } else {
                        "Node not found"
                    };
                    let _ = rq.respond(tiny_http::Response::from_string(body));
                }
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: "77777".into(),
        };
        let token = mint_wt_token_at(&base, &creds).expect("falls back to the scrape");
        assert_eq!(token, "tok-scraped");
        handle.join().unwrap();
    }

    /// The endpoint gone and NO node: the scrape cannot work without one, so
    /// the API failure surfaces as `Http` naming the path and status — one
    /// request, no fallback attempt.
    #[test]
    fn api_missing_without_a_node_is_an_http_error() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let Ok(rq) = server.recv() else { return };
            assert_eq!(rq.url(), "/api/v2/auth-wt-legacy");
            let _ = rq.respond(tiny_http::Response::from_string("not found").with_status_code(404));
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: String::new(),
        };
        let err = mint_wt_token_at(&base, &creds).expect_err("no node, no fallback");
        match err {
            Asl3Error::Http(msg) => {
                assert!(msg.contains(API_PATH), "names the API path: {msg}");
                assert!(msg.contains("404"), "names the status: {msg}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        handle.join().unwrap();
    }

    /// A 401 is a refusal whatever wrapping it arrives in: an nginx/WAF page
    /// in front of the API must not read as "unavailable" and send the
    /// password off to the scrape. One request served — a fallback would be a
    /// second.
    #[test]
    fn api_401_with_an_html_body_is_still_login() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let Ok(rq) = server.recv() else { return };
            assert_eq!(rq.url(), "/api/v2/auth-wt-legacy");
            let _ = rq.respond(
                tiny_http::Response::from_string("<html><body>401 Unauthorized</body></html>")
                    .with_status_code(401),
            );
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "wrong".into(),
            node: "77777".into(),
        };
        assert!(matches!(
            mint_wt_token_at(&base, &creds),
            Err(Asl3Error::Login)
        ));
        handle.join().unwrap();
    }

    /// A 2xx that carried no token is deliberately SOFT (see `mint_via_api`):
    /// until the live success shape has been seen once, an account with a node
    /// still mints through the scrape rather than failing outright.
    #[test]
    fn api_ok_with_empty_token_falls_back_to_the_scrape() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let Ok(rq) = server.recv() else { return };
                let url = rq.url().to_string();
                if url.starts_with(API_PATH) {
                    let _ = rq.respond(tiny_http::Response::from_string(
                        r#"{"status":"OK","auth":1,"token":"","msg":"ok"}"#,
                    ));
                } else if url.starts_with("/login.php") {
                    let resp = tiny_http::Response::from_string("ok").with_header(
                        tiny_http::Header::from_bytes(
                            &b"Set-Cookie"[..],
                            &b"PHPSESSID=stubsoft; path=/"[..],
                        )
                        .unwrap(),
                    );
                    let _ = rq.respond(resp);
                } else {
                    let body = if url.contains("node=77777") {
                        r#"<param name="callingName" value="tok-soft"/>"#
                    } else {
                        "Node not found"
                    };
                    let _ = rq.respond(tiny_http::Response::from_string(body));
                }
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: "77777".into(),
        };
        let token = mint_wt_token_at(&base, &creds).expect("falls back to the scrape");
        assert_eq!(token, "tok-soft");
        handle.join().unwrap();
    }

    /// The same empty-token answer with no node to fall back with: `Http`
    /// naming the API path and why, not a silent `TokenNotFound`.
    #[test]
    fn api_ok_with_empty_token_without_a_node_is_an_http_error() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            let Ok(rq) = server.recv() else { return };
            assert_eq!(rq.url(), "/api/v2/auth-wt-legacy");
            let _ = rq.respond(tiny_http::Response::from_string(
                r#"{"status":"OK","auth":1,"token":"","msg":"ok"}"#,
            ));
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: String::new(),
        };
        let err = mint_wt_token_at(&base, &creds).expect_err("no node, no fallback");
        match err {
            Asl3Error::Http(msg) => {
                assert!(msg.contains(API_PATH), "names the API path: {msg}");
                assert!(msg.contains("no token"), "names the reason: {msg}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        handle.join().unwrap();
    }

    /// Offline integration: a `tiny_http` stub plays the portal. Login sets
    /// the cookie; the WT page requires it and embeds the token.
    #[test]
    fn mint_flow_against_local_portal_stub() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok(mut rq) = server.recv() else { return };
                let url = rq.url().to_string();
                if url.starts_with("/login.php") {
                    let mut body = String::new();
                    let _ = rq.as_reader().read_to_string(&mut body);
                    assert!(body.contains("user=AJ7HR"), "form-encoded login: {body}");
                    // Mirror the LIVE portal (verified 2026-06-12): login sets
                    // the session id, an auth JWT, AND expires a stale cookie.
                    let cookie =
                        |v: &[u8]| tiny_http::Header::from_bytes(&b"Set-Cookie"[..], v).unwrap();
                    let resp = tiny_http::Response::from_string("ok")
                        .with_header(cookie(b"PHPSESSID=stub123; path=/"))
                        .with_header(cookie(b"allstar_token=jwt-abc; path=/"))
                        .with_header(cookie(
                            b"allstar_become=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0; path=/",
                        ));
                    let _ = rq.respond(resp);
                } else {
                    // The WT page authenticates on the FULL cookie set: both
                    // the session id and the JWT must arrive (only PHPSESSID
                    // renders an unauthenticated page on the live portal),
                    // and the expired cookie must NOT be echoed back.
                    let cookie_hdr = rq
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Cookie"))
                        .map(|h| h.value.as_str().to_string())
                        .unwrap_or_default();
                    let authed = cookie_hdr.contains("PHPSESSID=stub123")
                        && cookie_hdr.contains("allstar_token=jwt-abc")
                        && !cookie_hdr.contains("allstar_become");
                    let body = if authed && url.contains("node=77777") {
                        r#"<param name="callingName" value="tok-456789"/>"#
                    } else {
                        "Node not found"
                    };
                    let _ = rq.respond(tiny_http::Response::from_string(body));
                }
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: "77777".into(),
        };
        let token = mint_wt_token_legacy_at(&base, &creds).expect("mint succeeds");
        assert_eq!(token, "tok-456789");
        handle.join().unwrap();
    }

    /// Real PHP portals answer login.php with a 302 that carries BOTH the
    /// session cookie and a Location. With `.redirects(0)` the cookie must be
    /// read off the 302 itself and `/portal/` must never be fetched: the stub
    /// serves exactly two requests, so a stray redirect-follow would consume
    /// the WT slot and the mint would fail.
    #[test]
    fn mint_flow_with_302_login_redirect() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok(mut rq) = server.recv() else { return };
                let url = rq.url().to_string();
                if url.starts_with("/login.php") {
                    let mut body = String::new();
                    let _ = rq.as_reader().read_to_string(&mut body);
                    assert!(body.contains("user=AJ7HR"), "form-encoded login: {body}");
                    let resp = tiny_http::Response::empty(302)
                        .with_header(
                            tiny_http::Header::from_bytes(&b"Location"[..], &b"/portal/"[..])
                                .unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Set-Cookie"[..],
                                &b"PHPSESSID=stub302; path=/"[..],
                            )
                            .unwrap(),
                        );
                    let _ = rq.respond(resp);
                } else {
                    assert!(
                        url.starts_with("/webtransceiver.php"),
                        "redirect must not be followed, got {url}"
                    );
                    let has_cookie = rq.headers().iter().any(|h| {
                        h.field.equiv("Cookie") && h.value.as_str().contains("PHPSESSID=stub302")
                    });
                    let body = if has_cookie && url.contains("node=77777") {
                        r#"<param name="callingName" value="tok-302302"/>"#
                    } else {
                        "Node not found"
                    };
                    let _ = rq.respond(tiny_http::Response::from_string(body));
                }
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: "77777".into(),
        };
        let token =
            mint_wt_token_legacy_at(&base, &creds).expect("mint succeeds despite 302 login");
        assert_eq!(token, "tok-302302");
        handle.join().unwrap();
    }

    /// An empty node sends no `node` query at all (the portal mints without
    /// one); the stub refuses any request that still carries the parameter.
    #[test]
    fn empty_node_is_left_off_the_query() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok(rq) = server.recv() else { return };
                let url = rq.url().to_string();
                if url.starts_with("/login.php") {
                    let resp = tiny_http::Response::from_string("ok").with_header(
                        tiny_http::Header::from_bytes(
                            &b"Set-Cookie"[..],
                            &b"PHPSESSID=stubnode; path=/"[..],
                        )
                        .unwrap(),
                    );
                    let _ = rq.respond(resp);
                } else {
                    let body = if url == "/webtransceiver.php" {
                        r#"<param name="callingName" value="tok-nonode"/>"#
                    } else {
                        "unexpected query"
                    };
                    let _ = rq.respond(tiny_http::Response::from_string(body));
                }
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "not-a-real-password".into(),
            node: String::new(),
        };
        let token = mint_wt_token_legacy_at(&base, &creds).expect("mint succeeds without a node");
        assert_eq!(token, "tok-nonode");
        handle.join().unwrap();
    }

    #[test]
    fn no_cookie_means_login_error() {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
        let base = format!("http://{}", server.server_addr());
        let handle = std::thread::spawn(move || {
            if let Ok(rq) = server.recv() {
                let _ = rq.respond(tiny_http::Response::from_string("nope"));
            }
        });
        let creds = PortalCredentials {
            user: "AJ7HR".into(),
            password: "wrong".into(),
            node: "1".into(),
        };
        assert!(matches!(
            mint_wt_token_legacy_at(&base, &creds),
            Err(Asl3Error::Login)
        ));
        handle.join().unwrap();
    }
}
