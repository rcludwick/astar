// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! In-flight ceremonies: what a `begin` started and its `finish` needs.
//!
//! Kept in memory only, keyed by the SHA-256 of a random id that the
//! browser holds in the ceremony cookie, so webauthn-rs's state never needs
//! its `danger-allow-state-serialisation` feature and a restart just means
//! "try again". Each lives [`TTL_SECS`]. One client (an IPv4 address or
//! an IPv6 /64) holds at most [`PER_CLIENT`], its own oldest going first,
//! and the table at most [`CAP`]: when it is full, the oldest of whichever
//! client holds the most goes, so a flood from a few clients evicts only
//! its own and someone signing in once keeps theirs. (An attacker with
//! more than `CAP` addresses can still churn it, one each, within a TTL.)
//! Nothing is written to disk before a ceremony succeeds.

use std::collections::HashMap;

use uuid::Uuid;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, PasskeyAuthentication, PasskeyRegistration,
    SecurityKeyAuthentication, SecurityKeyRegistration,
};

use super::session;

/// The most ceremonies held at once.
pub const CAP: usize = 1024;
/// The most one client may hold at once.
pub const PER_CLIENT: usize = 8;
/// How long a ceremony may take.
pub const TTL_SECS: u64 = 300;

/// A WebAuthn sign-in for a known user.
pub enum KeyAuth {
    Passkey(PasskeyAuthentication),
    SecurityKey(SecurityKeyAuthentication),
}

/// A WebAuthn registration.
pub enum Reg {
    Passkey(PasskeyRegistration),
    SecurityKey(SecurityKeyRegistration),
}

/// Whose credential an enrolment makes.
#[derive(Clone)]
pub enum Enrol {
    /// Redeeming an invite: consumed only when the credential is saved.
    Invite {
        invite_hash: String,
        /// The name the invitee chose (a new user) or the user's name.
        name: String,
        display_name: String,
    },
    /// A signed-in user adding a credential.
    Existing { session_id: String },
}

/// What a ceremony is for, with webauthn-rs's state for it.
pub enum Kind {
    /// The "Sign in with a passkey" button.
    Discoverable(DiscoverableAuthentication),
    /// Identifier-first sign-in with a security key or passkey.
    SignIn { user_id: Uuid, auth: KeyAuth },
    /// Confirming it's you, inside a session.
    StepUp {
        session_id: String,
        user_id: Uuid,
        auth: KeyAuth,
    },
    /// Making a WebAuthn credential.
    Register {
        enrol: Enrol,
        user_id: Uuid,
        reg: Reg,
    },
    /// An identifier-first sign-in for a name with no key on this host: the
    /// same shape of answer, and a finish that always fails.
    Decoy,
    /// Making an authenticator-app credential: the secret waits here until
    /// the user proves their app has it.
    TotpEnrol {
        enrol: Enrol,
        user_id: Uuid,
        secret: Vec<u8>,
        /// Wrong confirmation codes so far.
        attempts: u8,
    },
}

/// One ceremony.
pub struct Ceremony {
    pub created: u64,
    /// The hostname it was started on; a finish on another host is refused.
    pub host: String,
    /// The client key of who started it.
    pub client: String,
    pub kind: Kind,
}

/// The table.
#[derive(Default)]
pub struct Table {
    map: HashMap<String, Ceremony>,
}

impl Table {
    /// Store a ceremony and return the id for the cookie.
    pub fn insert(&mut self, c: Ceremony, now: u64) -> String {
        self.map
            .retain(|_, v| now.saturating_sub(v.created) <= TTL_SECS);
        // This client's own oldest first, past its quota.
        while self.map.values().filter(|v| v.client == c.client).count() >= PER_CLIENT {
            if !self.evict_oldest_of(&c.client) {
                break;
            }
        }
        // Then, while full, the oldest of whoever holds the most (of
        // those, whoever started theirs first).
        while self.map.len() >= CAP {
            let mut held: HashMap<&str, (usize, u64)> = HashMap::new();
            for v in self.map.values() {
                let e = held.entry(v.client.as_str()).or_insert((0, u64::MAX));
                e.0 += 1;
                e.1 = e.1.min(v.created);
            }
            let Some(biggest) = held
                .into_iter()
                .max_by_key(|&(_, (n, oldest))| (n, std::cmp::Reverse(oldest)))
                .map(|(k, _)| k.to_string())
            else {
                break;
            };
            if !self.evict_oldest_of(&biggest) {
                break;
            }
        }
        let id = session::new_token();
        self.map.insert(session::hash_token(&id), c);
        id
    }

    /// Drop `client`'s oldest ceremony; `false` when it has none.
    fn evict_oldest_of(&mut self, client: &str) -> bool {
        let oldest = self
            .map
            .iter()
            .filter(|(_, v)| v.client == client)
            .min_by_key(|(_, v)| v.created)
            .map(|(k, _)| k.clone());
        oldest.is_some_and(|k| self.map.remove(&k).is_some())
    }

    /// Take a ceremony by its cookie id: removed whether or not it is still
    /// fresh, so an id is good for one finish.
    pub fn take(&mut self, id: &str, now: u64) -> Option<Ceremony> {
        if !session::is_token(id) {
            return None;
        }
        let c = self.map.remove(&session::hash_token(id))?;
        (now.saturating_sub(c.created) <= TTL_SECS).then_some(c)
    }

    /// Put a ceremony back under the id it was taken by: a TOTP enrolment
    /// whose confirmation code was mistyped gets another try.
    pub fn put_back(&mut self, id: &str, c: Ceremony) {
        if session::is_token(id) && self.map.len() < CAP {
            self.map.insert(session::hash_token(id), c);
        }
    }

    /// How many are held (tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether none are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totp(now: u64) -> Ceremony {
        from("c", now)
    }

    fn from(client: &str, now: u64) -> Ceremony {
        Ceremony {
            created: now,
            host: "h".into(),
            client: client.into(),
            kind: Kind::TotpEnrol {
                enrol: Enrol::Existing {
                    session_id: "s".into(),
                },
                user_id: Uuid::nil(),
                secret: vec![1, 2, 3],
                attempts: 0,
            },
        }
    }

    #[test]
    fn an_id_is_good_once_and_only_while_fresh() {
        let mut t = Table::default();
        let id = t.insert(totp(100), 100);
        assert!(t.take(&id, 101).is_some());
        assert!(t.take(&id, 101).is_none(), "used");
        let id = t.insert(totp(100), 100);
        assert!(t.take(&id, 100 + TTL_SECS + 1).is_none(), "expired");
        assert!(t.take("not-a-token", 100).is_none());
    }

    #[test]
    fn one_client_cannot_crowd_out_another() {
        let mut t = Table::default();
        let mine = t.insert(from("me", 1), 1);
        for _ in 0..(CAP * 2) {
            t.insert(from("flood", 2), 2);
        }
        assert!(t.len() <= PER_CLIENT + 1, "{}", t.len());
        // Many clients, each at its own limit, still leave a lone ceremony.
        for c in 0..(CAP / PER_CLIENT + 10) {
            for _ in 0..PER_CLIENT {
                t.insert(from(&format!("f{c}"), 5), 5);
            }
        }
        assert!(t.len() <= CAP);
        assert!(t.take(&mine, 6).is_some(), "mine survived");
    }

    #[test]
    fn the_table_is_capped_oldest_first() {
        let mut t = Table::default();
        let first = t.insert(totp(1), 1);
        for i in 0..CAP as u64 {
            t.insert(from(&format!("c{i}"), 2 + i), 2);
        }
        assert_eq!(t.len(), CAP);
        assert!(t.take(&first, 3).is_none(), "the oldest went");
    }
}
