// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Stateless CALLTOKENs for the inbound listener (RFC 5456 §8.6 anti-spoof).
//!
//! A caller's first NEW carries no token. The listener answers it inline with
//! a CALLTOKEN and keeps nothing: no leg, no call number, no thread. The
//! token is a keyed hash (HMAC-SHA256, with a random key made when the
//! listener starts and never logged) over the caller's address and port, its
//! source call number, and the time the token was issued. A NEW that echoes
//! it is checked against the same three inputs, so only a caller that really
//! receives packets at that address can produce one, and only for
//! [`TOKEN_LIFETIME`]. This is the shape `chan_iax2` uses.
//!
//! Wire form: `<seconds>?<32 hex digits>`, all printable ASCII. `chan_iax2`
//! echoes tokens as C strings, so a NUL byte would cut it short (the
//! 2026-10 "NUL in token" interop bug): there is none.

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// How long a token is good for after it is issued. `chan_iax2`'s default
/// `calltokenoptional` delay (`MAX_CALLTOKEN_DELAY`) is 10 s. Ages are counted
/// in whole seconds of the issuer's clock and a token is accepted while its
/// age is at most this, so it lives between 10 and 11 s of real time.
pub(crate) const TOKEN_LIFETIME: Duration = Duration::from_secs(10);

/// The hash bytes the token carries (as 32 hex digits).
const MAC_BYTES: usize = 16;

type HmacSha256 = Hmac<Sha256>;

/// What checking a presented token found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenCheck {
    /// Issued by us, to this address and call number, within the lifetime.
    Valid,
    /// Ours and well-formed, but older than [`TOKEN_LIFETIME`].
    Stale,
    /// Not a token we issued to this address and call number.
    Invalid,
}

/// The listener's token issuer. Deliberately not `Debug`: it holds the key.
pub(crate) struct CallTokens {
    key: [u8; 32],
    epoch: Instant,
}

impl CallTokens {
    /// A fresh issuer with a key from the operating system's entropy.
    ///
    /// # Panics
    /// If the OS cannot produce entropy: a guessable key would make the
    /// anti-spoof check worthless, so this does not degrade.
    pub(crate) fn new() -> Self {
        use rand::TryRng as _;
        let mut key = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut key)
            .expect("system entropy");
        Self {
            key,
            epoch: Instant::now(),
        }
    }

    /// A token for a NEW from `src` with source call `peer_call`, issued now.
    pub(crate) fn issue(&self, src: SocketAddr, peer_call: u16) -> Vec<u8> {
        self.issue_at(src, peer_call, self.now())
    }

    /// Check a token a NEW from `src` with source call `peer_call` carries.
    pub(crate) fn check(&self, src: SocketAddr, peer_call: u16, token: &[u8]) -> TokenCheck {
        self.check_at(src, peer_call, token, self.now())
    }

    /// Seconds since this issuer started: a monotonic clock, so a wall-clock
    /// step can neither revive nor kill tokens.
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_secs()
    }

    fn mac(&self, src: SocketAddr, peer_call: u16, issued: u64) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC takes any key length");
        match src.ip() {
            IpAddr::V4(ip) => {
                mac.update(&[4]);
                mac.update(&ip.octets());
            }
            IpAddr::V6(ip) => {
                mac.update(&[6]);
                mac.update(&ip.octets());
            }
        }
        mac.update(&src.port().to_be_bytes());
        mac.update(&peer_call.to_be_bytes());
        mac.update(&issued.to_be_bytes());
        mac
    }

    fn issue_at(&self, src: SocketAddr, peer_call: u16, issued: u64) -> Vec<u8> {
        let tag = self.mac(src, peer_call, issued).finalize().into_bytes();
        let mut token = format!("{issued}?");
        for b in &tag[..MAC_BYTES] {
            let _ = write!(token, "{b:02x}");
        }
        token.into_bytes()
    }

    fn check_at(&self, src: SocketAddr, peer_call: u16, token: &[u8], now: u64) -> TokenCheck {
        let Some((issued, tag)) = parse(token) else {
            return TokenCheck::Invalid;
        };
        // Constant-time compare of the leading MAC_BYTES of the tag.
        if self
            .mac(src, peer_call, issued)
            .verify_truncated_left(&tag)
            .is_err()
        {
            return TokenCheck::Invalid;
        }
        if issued > now {
            // From the future: only a forger's clock gets here.
            return TokenCheck::Invalid;
        }
        if now - issued > TOKEN_LIFETIME.as_secs() {
            return TokenCheck::Stale;
        }
        TokenCheck::Valid
    }
}

/// Split `<seconds>?<32 hex>` into its parts. `None` for anything else.
fn parse(token: &[u8]) -> Option<(u64, [u8; MAC_BYTES])> {
    let text = std::str::from_utf8(token).ok()?;
    let (secs, hex) = text.split_once('?')?;
    if secs.is_empty() || secs.len() > 20 || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let issued = secs.parse().ok()?;
    if hex.len() != MAC_BYTES * 2 {
        return None;
    }
    let mut tag = [0u8; MAC_BYTES];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        tag[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some((issued, tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn a_token_checks_for_its_own_address_and_call_only() {
        let t = CallTokens::new();
        let tok = t.issue_at(addr(4569), 7, 100);
        assert_eq!(t.check_at(addr(4569), 7, &tok, 100), TokenCheck::Valid);
        assert_eq!(t.check_at(addr(4570), 7, &tok, 100), TokenCheck::Invalid);
        assert_eq!(t.check_at(addr(4569), 8, &tok, 100), TokenCheck::Invalid);
        let v6 = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 4569));
        assert_eq!(t.check_at(v6, 7, &tok, 100), TokenCheck::Invalid);
    }

    #[test]
    fn a_token_lasts_ten_seconds() {
        let t = CallTokens::new();
        let tok = t.issue_at(addr(1), 1, 100);
        assert_eq!(t.check_at(addr(1), 1, &tok, 110), TokenCheck::Valid);
        assert_eq!(t.check_at(addr(1), 1, &tok, 111), TokenCheck::Stale);
        assert_eq!(t.check_at(addr(1), 1, &tok, 99), TokenCheck::Invalid);
    }

    #[test]
    fn another_listeners_token_or_an_edited_one_is_invalid() {
        let (a, b) = (CallTokens::new(), CallTokens::new());
        let tok = a.issue_at(addr(1), 1, 5);
        assert_eq!(b.check_at(addr(1), 1, &tok, 5), TokenCheck::Invalid);
        // Moving the timestamp forward to dodge the lifetime breaks the tag.
        let text = String::from_utf8(tok).unwrap();
        let forged = text.replacen("5?", "9?", 1);
        assert_eq!(
            a.check_at(addr(1), 1, forged.as_bytes(), 9),
            TokenCheck::Invalid
        );
        for junk in [&b""[..], b"?", b"12", b"x?00", b"1?zz", &[0u8; 34][..]] {
            assert_eq!(a.check_at(addr(1), 1, junk, 5), TokenCheck::Invalid);
        }
    }

    #[test]
    fn tokens_are_printable_with_no_nul_and_fit_the_ie() {
        let t = CallTokens::new();
        for (port, secs) in [(1, 0), (65_535, u64::MAX), (4569, 1_700_000_000)] {
            let tok = t.issue_at(addr(port), 0x7fff, secs);
            assert!(tok.iter().all(|b| (0x21..=0x7e).contains(b)), "{tok:?}");
            assert!(tok.len() <= astar_iax_core::ie::MAX_CALLTOKEN_LEN);
        }
    }
}
