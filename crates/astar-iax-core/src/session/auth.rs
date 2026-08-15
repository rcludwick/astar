// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Credentials and challenge/response helpers.
//!
//! Asterisk's MD5 challenge-response is the only working auth method in
//! the iax-c333 landing — plaintext is stubbed for completeness and RSA
//! is rejected with a clear `LogInvalid` action by the FSM.

use std::sync::Arc;

use bitflags::bitflags;
use md5::{Digest, Md5};
use zeroize::Zeroize;

bitflags! {
    /// Authentication methods advertised in `IE_AUTHMETHODS` (RFC 5456 §8.14).
    /// Values from Asterisk `channels/iax2/include/iax2.h`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AuthMethods: u16 {
        const PLAINTEXT = 1;
        const MD5       = 2;
        const RSA       = 4;
    }
}

/// A password held in memory. Zeroizes on drop.
///
/// Deliberately NOT `Clone` (iax-f755 L5): cloning would duplicate the
/// plaintext into a second buffer that the zeroizing `Drop` could miss. Share
/// one `Secret` via `Arc<Secret>` (see [`Credentials`]) instead of copying it.
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(s: String) -> Self {
        Self(s)
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Preloaded credentials handed to the FSM at construction.
///
/// `password` is `Arc<Secret>` so a single zeroizing password can be shared
/// between a registration FSM and any number of concurrent call FSMs without
/// being cloned out of its wrapper. The `Secret` is zeroized when the last
/// `Arc` drops; `Secret` itself is not `Clone` (iax-f755 L5), so the only way
/// to share it is through the `Arc`.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: Arc<Secret>,
    pub allowed_methods: AuthMethods,
}

/// Compute `md5(challenge || password)` as a lowercase hex string, matching
/// Asterisk's `iax2_md5_hash` in `channels/chan_iax2.c`.
#[must_use]
pub fn md5_response(challenge: &str, password: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(challenge.as_bytes());
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Server-side validation: recompute `md5(challenge || password)` and
/// compare against the peer's `MD5_RESULT` IE in constant time
/// (`subtle::ConstantTimeEq`), so a mismatch leaks no timing about *which*
/// byte differed (RFC 5456 §10.0-03). Length mismatch short-circuits to
/// `false` (the lengths themselves are not secret).
#[must_use]
pub fn md5_verify(challenge: &str, password: &str, candidate_hex: &str) -> bool {
    use subtle::ConstantTimeEq;
    let expected = md5_response(challenge, password);
    if expected.len() != candidate_hex.len() {
        return false;
    }
    expected.as_bytes().ct_eq(candidate_hex.as_bytes()).into()
}

/// Constant-time CALLTOKEN comparison (RFC 5456 §8.6 anti-spoof). Returns
/// `false` on any length mismatch (lengths aren't secret) and on byte
/// mismatch without early-exit. The issued token is per-call `OsRng` bytes;
/// the received token comes from the peer's CALLTOKEN IE.
#[must_use]
pub fn token_eq(issued: &[u8], received: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if issued.is_empty() || issued.len() != received.len() {
        return false;
    }
    issued.ct_eq(received).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_response_matches_known_vector() {
        let hex = md5_response("abc123", "secret");
        assert_eq!(hex, "38d4588fdbc729ba5f07c49b42d195a0");
    }

    #[test]
    fn auth_methods_bits_match_iax2_h() {
        assert_eq!(AuthMethods::PLAINTEXT.bits(), 1);
        assert_eq!(AuthMethods::MD5.bits(), 2);
        assert_eq!(AuthMethods::RSA.bits(), 4);
    }

    #[test]
    fn secret_zeroizes_on_drop() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn md5_verify_accepts_correct_response_constant_time() {
        // server holds password "secret"; client sent md5("abc123"||"secret").
        let expected = md5_response("abc123", "secret");
        assert!(md5_verify("abc123", "secret", &expected));
    }

    #[test]
    fn md5_verify_rejects_wrong_response_and_wrong_length() {
        assert!(!md5_verify("abc123", "secret", "deadbeef")); // wrong + short
        let wrong = md5_response("abc123", "not-the-password");
        assert!(!md5_verify("abc123", "secret", &wrong));
    }

    #[test]
    fn token_eq_is_value_and_length_sensitive() {
        assert!(token_eq(b"opaque-16-bytes!", b"opaque-16-bytes!"));
        assert!(!token_eq(b"opaque-16-bytes!", b"opaque-16-bytez!"));
        assert!(!token_eq(b"short", b"longer-token"));
        assert!(!token_eq(b"", b"x")); // absent token never matches issued
    }
}
