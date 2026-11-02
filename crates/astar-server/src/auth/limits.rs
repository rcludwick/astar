// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Per-client rate limits: ceremony `begin`s, and failed TOTP codes. A
//! client is an IPv4 address or an IPv6 /64 ([`super::service::Req::client_key`]).
//!
//! In memory, bounded: each client keeps only the timestamps inside its
//! window, and at most [`MAX_IPS`] clients are tracked. When the table is
//! full, only clients whose window has passed are forgotten; history still
//! inside its window is never evicted to make room, so a flood of new
//! addresses cannot wipe an attacker's own count. Clients that find it full
//! share one [`OVERFLOW`] count, so during such a flood newcomers are
//! limited together rather than not at all. A restart forgets them, which
//! is fine: the TOTP lockouts, the limit that matters, are in the store.

use std::collections::{HashMap, VecDeque};

/// `begin` calls per IP per minute.
pub const BEGINS_PER_MINUTE: usize = 20;
/// Failed TOTP codes per IP per hour, across all users.
pub const TOTP_FAILURES_PER_HOUR: usize = 30;
/// Addresses tracked at once, per limit.
pub const MAX_IPS: usize = 4096;

/// The key that clients share while the table is full.
pub const OVERFLOW: &str = "*";

#[derive(Default)]
struct Window {
    map: HashMap<String, VecDeque<u64>>,
}

impl Window {
    fn prune(&mut self, key: &str, now: u64, span: u64) -> &mut VecDeque<u64> {
        let mut key = key;
        if !self.map.contains_key(key) && self.map.len() >= MAX_IPS {
            self.map
                .retain(|_, q| q.back().is_some_and(|&t| now.saturating_sub(t) < span));
            if self.map.len() >= MAX_IPS {
                key = OVERFLOW;
            }
        }
        let q = self.map.entry(key.to_string()).or_default();
        while q.front().is_some_and(|&t| now.saturating_sub(t) >= span) {
            q.pop_front();
        }
        q
    }
}

/// The limits.
#[derive(Default)]
pub struct Limits {
    begins: Window,
    totp: Window,
}

impl Limits {
    /// Count a `begin` from `ip`; `false` when it is over the limit.
    pub fn allow_begin(&mut self, ip: &str, now: u64) -> bool {
        let q = self.begins.prune(ip, now, 60);
        if q.len() >= BEGINS_PER_MINUTE {
            return false;
        }
        q.push_back(now);
        true
    }

    /// Count a TOTP attempt from `ip` as a failure before it is evaluated;
    /// `false`, counting nothing, when `ip` has none left this hour.
    pub fn totp_reserve(&mut self, ip: &str, now: u64) -> bool {
        let q = self.totp.prune(ip, now, 3600);
        if q.len() >= TOTP_FAILURES_PER_HOUR {
            return false;
        }
        q.push_back(now);
        true
    }

    /// Give back a reserved attempt that turned out not to be a failure.
    pub fn totp_refund(&mut self, ip: &str) {
        let key = if self.totp.map.contains_key(ip) {
            ip
        } else {
            OVERFLOW
        };
        if let Some(q) = self.totp.map.get_mut(key) {
            q.pop_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begins_are_limited_per_minute_per_ip() {
        let mut l = Limits::default();
        for _ in 0..BEGINS_PER_MINUTE {
            assert!(l.allow_begin("a", 100));
        }
        assert!(!l.allow_begin("a", 100));
        assert!(l.allow_begin("b", 100), "another address is unaffected");
        assert!(l.allow_begin("a", 160), "a minute later");
    }

    #[test]
    fn totp_failures_are_limited_per_hour_per_ip() {
        let mut l = Limits::default();
        for _ in 0..TOTP_FAILURES_PER_HOUR {
            assert!(l.totp_reserve("a", 10));
        }
        assert!(!l.totp_reserve("a", 10));
        l.totp_refund("a");
        assert!(l.totp_reserve("a", 10), "a refunded attempt is free again");
        assert!(l.totp_reserve("a", 10 + 3600));
    }

    #[test]
    fn a_flood_of_other_addresses_does_not_wipe_one_s_history() {
        let mut l = Limits::default();
        for _ in 0..BEGINS_PER_MINUTE {
            assert!(l.allow_begin("attacker", 100));
        }
        for i in 0..(MAX_IPS * 2) {
            l.allow_begin(&format!("10.1.{}.{}", i / 256, i % 256), 101);
        }
        assert!(
            !l.allow_begin("attacker", 102),
            "still out of begins: live history is never evicted"
        );
        assert!(l.begins.map.len() <= MAX_IPS + 1);
        // Once the window has passed, room is made from what aged out.
        assert!(l.allow_begin("newcomer", 200));
    }

    #[test]
    fn tracked_addresses_are_bounded() {
        let mut l = Limits::default();
        for i in 0..(MAX_IPS + 10) {
            l.allow_begin(&format!("10.0.{}.{}", i / 256, i % 256), i as u64);
        }
        assert!(l.begins.map.len() <= MAX_IPS + 1, "and the shared overflow");
    }
}
