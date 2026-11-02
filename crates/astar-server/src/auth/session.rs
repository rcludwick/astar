// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Random tokens, their hashes, and the cookies that carry them.
//!
//! A session cookie, an invite token and a ceremony id are each 256 random
//! bits in Base64url. The store keeps only SHA-256 of the session cookie and
//! the invite token, and looks them up by that hash, so there is no
//! secret-dependent comparison to time.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// The session cookie over HTTPS: `__Host-` pins it to this exact origin,
/// with `Secure`, `Path=/` and no `Domain`.
pub const SESSION_COOKIE_TLS: &str = "__Host-astar_session";
/// The session cookie over plain HTTP, which only a loopback page or
/// `localhost` ever uses (a `__Host-` cookie needs `Secure`).
pub const SESSION_COOKIE_PLAIN: &str = "astar_session";
/// The cookie that binds a WebAuthn or TOTP ceremony to the browser that
/// started it.
pub const CEREMONY_COOKIE_TLS: &str = "__Host-astar_ceremony";
/// The ceremony cookie over plain HTTP.
pub const CEREMONY_COOKIE_PLAIN: &str = "astar_ceremony";

/// `n` random bytes. Panics only if the OS has no randomness at all, which
/// no sign-in can survive anyway.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b
}

/// A new 256-bit token, Base64url without padding (43 characters).
#[must_use]
pub fn new_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// A short random id for things the API names (a session in the list, an
/// invite, a credential): 96 bits, Base64url. Not a secret.
#[must_use]
pub fn new_id() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<12>())
}

/// Hex SHA-256 of a token: the form the store keeps.
#[must_use]
pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Lower-case hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Whether a token has the shape [`new_token`] makes, so a hash is never
/// computed over arbitrary input lengths and a cookie that is not ours is
/// ignored outright.
#[must_use]
pub fn is_token(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The session cookie's name for a request over TLS or not.
#[must_use]
pub const fn session_cookie(tls: bool) -> &'static str {
    if tls {
        SESSION_COOKIE_TLS
    } else {
        SESSION_COOKIE_PLAIN
    }
}

/// The ceremony cookie's name for a request over TLS or not.
#[must_use]
pub const fn ceremony_cookie(tls: bool) -> &'static str {
    if tls {
        CEREMONY_COOKIE_TLS
    } else {
        CEREMONY_COOKIE_PLAIN
    }
}

/// One cookie's value from a `Cookie` header, or `None`.
#[must_use]
pub fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// A `Set-Cookie` value: `HttpOnly`, `SameSite=Strict`, `Path=/`, and
/// `Secure` over TLS. `max_age` of `None` is a browser-session cookie;
/// `Some(0)` deletes it.
#[must_use]
pub fn set_cookie(name: &str, value: &str, tls: bool, max_age: Option<u64>) -> String {
    let mut c = format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict");
    if tls {
        c.push_str("; Secure");
    }
    if let Some(age) = max_age {
        use std::fmt::Write as _;
        let _ = write!(c, "; Max-Age={age}");
    }
    c
}

/// The `Set-Cookie` that removes a cookie.
#[must_use]
pub fn clear_cookie(name: &str, tls: bool) -> String {
    set_cookie(name, "", tls, Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hashing_is_unchanged_across_the_sha2_bump() {
        // The store keeps hash_token(token): a change here signs everyone out.
        assert_eq!(
            hash_token("astar-session-token"),
            "86bfb48caada7bc964af17d1e4a28ff3c641b1de02267c494080f845f59fc454"
        );
        // 0xfb 0xff 0xfe exercise both url-safe alphabet differences.
        assert_eq!(URL_SAFE_NO_PAD.encode([0xfb, 0xff, 0xfe, 0x01]), "-__-AQ");
    }

    #[test]
    fn tokens_are_256_bits_and_unique() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 43);
        assert!(is_token(&a));
        assert_ne!(a, b);
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), a);
    }

    #[test]
    fn foreign_cookie_values_are_not_tokens() {
        assert!(!is_token(""));
        assert!(!is_token("short"));
        assert!(!is_token(&"a".repeat(44)));
        assert!(!is_token(&format!("{}=", "a".repeat(42))));
    }

    #[test]
    fn a_cookie_is_found_by_exact_name() {
        let h = Some("theme=dark; __Host-astar_session=abc; astar_session_x=1");
        assert_eq!(cookie_value(h, SESSION_COOKIE_TLS), Some("abc"));
        assert_eq!(cookie_value(h, SESSION_COOKIE_PLAIN), None);
        assert_eq!(cookie_value(None, SESSION_COOKIE_PLAIN), None);
    }

    #[test]
    fn the_session_cookie_has_every_attribute() {
        let c = set_cookie(SESSION_COOKIE_TLS, "v", true, Some(60));
        assert_eq!(
            c,
            "__Host-astar_session=v; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=60"
        );
        assert!(!c.contains("Domain"), "a __Host- cookie has no Domain");
        let plain = set_cookie(SESSION_COOKIE_PLAIN, "v", false, None);
        assert_eq!(plain, "astar_session=v; Path=/; HttpOnly; SameSite=Strict");
        assert!(clear_cookie(SESSION_COOKIE_TLS, true).contains("Max-Age=0"));
    }
}
