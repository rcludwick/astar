// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Web Transceiver token minting — a native port of scripts/asl-wt-token.py
//! (which shells out to uv/Python and cannot ship inside an app).
//!
//! Flow (replicates `DroidStar`'s `obtain_asl_wt_creds()`):
//! 1. POST `<portal>/login.php` form `user`/`pass` → `PHPSESSID` cookie.
//! 2. GET `<portal>/webtransceiver.php?node=<node>` with that cookie.
//! 3. Extract the `callingName` token from the HTML.
//!
//! The token is used as the IAX2 `CALLING_NAME`; the node's dialplan resolves
//! token → callsign via authwebphone.pl. Mint fresh per session. The portal
//! only emits tokens for nodes the account OWNS.

use crate::Asl3Error;

const DEFAULT_PORTAL: &str = "https://www.allstarlink.org/portal";

/// `AllStarLink` portal account credentials. Never logged or echoed in errors.
/// `Clone` so consumers can hold them in cloneable config (the harness's
/// `HarnessDefaults` derives `Clone`).
#[derive(Clone)]
pub struct PortalCredentials {
    /// Portal account callsign.
    pub user: String,
    /// Portal ACCOUNT password (not a node secret).
    pub password: String,
    /// A node the account OWNS (minting requires it).
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

/// Mint against an explicit portal base URL — the testable core. Public so a
/// consumer can point the mint at a staging/stub portal (e.g. an offline test
/// harness); production callers use [`mint_wt_token`], which targets the live
/// `AllStarLink` portal.
///
/// # Errors
/// Same categories as [`mint_wt_token`].
pub fn mint_wt_token_at(base_url: &str, creds: &PortalCredentials) -> Result<String, Asl3Error> {
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

    // 2. Fetch the WT page with the session cookies.
    let html = agent
        .get(&format!("{base_url}/webtransceiver.php"))
        .query("node", &creds.node)
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
/// [`Asl3Error::Login`] when the portal issues no session cookie (bad
/// credentials); [`Asl3Error::TokenNotFound`] when the page carries no token
/// (no WT access for the node, or markup changed); [`Asl3Error::Http`] for
/// transport failures.
pub fn mint_wt_token(creds: &PortalCredentials) -> Result<String, Asl3Error> {
    mint_wt_token_at(DEFAULT_PORTAL, creds)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let token = mint_wt_token_at(&base, &creds).expect("mint succeeds");
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
        let token = mint_wt_token_at(&base, &creds).expect("mint succeeds despite 302 login");
        assert_eq!(token, "tok-302302");
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
            mint_wt_token_at(&base, &creds),
            Err(Asl3Error::Login)
        ));
        handle.join().unwrap();
    }
}
