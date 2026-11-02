// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public bind's numbers. The machinery (pools with a per-address cap,
//! the one request-phase deadline, the stream counters) is astar-http's;
//! these are the values this node runs with, and the stream accounting both
//! public listeners share.
//!
//! An address is counted as itself for IPv4 and as its `/64` for IPv6, since
//! one IPv6 host is normally handed a whole `/64`.

use std::sync::Arc;
use std::time::Duration;

pub use astar_http::deadline::{DeadlineSocket, ReadDeadline};
pub use astar_http::limits::{StreamRefusal, peer_key};

/// How long a client gets for the TLS handshake, its request head and its
/// body together. A client sending a byte every few seconds is cut off here
/// all the same.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// The most connections open at once on the public bind. Past it a new
/// connection is closed at once; `max_listeners` (at most 200) plus page
/// viewers fit well inside.
pub const MAX_CONNECTIONS: usize = 1024;
/// The port-80 listener's own pool, so a full public bind never blocks
/// Let's Encrypt's HTTP-01 check (or the redirect to HTTPS).
pub const PORT80_CONNECTIONS: usize = 256;
/// The most connections one address may hold open at once, per listener: a
/// browser opens six or so per host, and a few tabs fit.
pub const CONNS_PER_IP: usize = 16;
/// The most long-lived streams (`/public/events`, the admin event stream,
/// `/stream.ws`, `/stream.mp3`) one address may hold at once: two tabs, each
/// with its player and its event feed. With the default `max_listeners` of
/// 25, no one address can take every audio slot.
pub const STREAMS_PER_IP: usize = 4;
/// The most event streams (`/public/events` and the admin one) open at once,
/// so viewers who never press play cannot fill the connection pool.
pub const MAX_EVENT_STREAMS: usize = 256;
/// How often a stream admitted on a session checks that the session still
/// stands: sign-out, removal, demotion and expiry end it within this.
pub const SESSION_RECHECK: Duration = Duration::from_secs(5);

/// The numbers the public bind enforces. [`Limits::default`] is what a node
/// runs; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// [`HEAD_TIMEOUT`].
    pub head_deadline: Duration,
    /// [`MAX_CONNECTIONS`].
    pub max_connections: usize,
    /// [`PORT80_CONNECTIONS`].
    pub port80_connections: usize,
    /// [`CONNS_PER_IP`].
    pub conns_per_ip: usize,
    /// [`STREAMS_PER_IP`].
    pub streams_per_ip: usize,
    /// [`MAX_EVENT_STREAMS`].
    pub max_event_streams: usize,
    /// [`SESSION_RECHECK`].
    pub session_recheck: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            head_deadline: HEAD_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
            port80_connections: PORT80_CONNECTIONS,
            conns_per_ip: CONNS_PER_IP,
            streams_per_ip: STREAMS_PER_IP,
            max_event_streams: MAX_EVENT_STREAMS,
            session_recheck: SESSION_RECHECK,
        }
    }
}

impl Limits {
    /// The astar-http limits for one listener: the public bind's, or the
    /// port-80 listener's own pool when `port80`.
    #[must_use]
    pub const fn http(&self, port80: bool) -> astar_http::Limits {
        astar_http::Limits {
            max_connections: if port80 {
                self.port80_connections
            } else {
                self.max_connections
            },
            conns_per_client: self.conns_per_ip,
            deadline: self.head_deadline,
            max_head_bytes: crate::public::http::MAX_HEAD_BYTES,
            max_body_bytes: crate::http::MAX_BODY_BYTES,
            write_stall: crate::public::http::WRITE_STALL,
            streams_per_client: self.streams_per_ip,
            max_counted_streams: self.max_event_streams,
        }
    }
}

/// The limits, and the stream accounting the public bind and port 80 share.
#[derive(Debug)]
pub struct Guard {
    pub limits: Limits,
    /// Long-lived streams per address, and event streams overall.
    pub(crate) streams: Arc<astar_http::StreamGuard>,
}

impl Default for Guard {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Guard {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            streams: astar_http::StreamGuard::new(limits.streams_per_ip, limits.max_event_streams),
            limits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_http::StreamClass;
    use std::net::IpAddr;

    #[test]
    fn ipv6_is_counted_per_64_and_mapped_v4_as_v4() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(peer_key(a), peer_key(b));
        assert_ne!(peer_key(a), peer_key(c));
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert_eq!(peer_key(mapped), "192.0.2.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn streams_are_capped_per_address_and_events_overall() {
        let g = Guard::new(Limits {
            streams_per_ip: 2,
            max_event_streams: 1,
            ..Limits::default()
        });
        let x: IpAddr = "192.0.2.1".parse().unwrap();
        let y: IpAddr = "192.0.2.2".parse().unwrap();
        let ev = g.streams.open(x, StreamClass::Counted).unwrap();
        let _audio = g.streams.open(x, StreamClass::PerClientOnly).unwrap();
        assert_eq!(
            g.streams.open(x, StreamClass::PerClientOnly).unwrap_err(),
            StreamRefusal::PerClient
        );
        assert_eq!(
            g.streams.open(y, StreamClass::Counted).unwrap_err(),
            StreamRefusal::Full
        );
        assert_eq!(g.streams.held(y), 0, "a refusal holds nothing");
        drop(ev);
        assert!(g.streams.open(y, StreamClass::Counted).is_ok());
    }

    #[test]
    fn each_listener_gets_its_own_pool_and_the_shared_numbers() {
        let l = Limits::default();
        let public = l.http(false);
        let port80 = l.http(true);
        assert_eq!(public.max_connections, MAX_CONNECTIONS);
        assert_eq!(port80.max_connections, PORT80_CONNECTIONS);
        for h in [public, port80] {
            assert_eq!(h.conns_per_client, CONNS_PER_IP);
            assert_eq!(h.deadline, HEAD_TIMEOUT);
            assert_eq!(h.streams_per_client, STREAMS_PER_IP);
            assert_eq!(h.max_counted_streams, MAX_EVENT_STREAMS);
            assert_eq!(h.write_stall, Duration::from_secs(30));
            assert_eq!(h.max_head_bytes, 8 * 1024);
        }
    }
}
