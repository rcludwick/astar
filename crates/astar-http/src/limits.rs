// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What keeps one client from holding the server: connection pools with a
//! per-client cap, a guard for long-lived streams, and the numbers they read
//! ([`Limits`]).
//!
//! An address is counted as itself for IPv4 and as its `/64` for IPv6, since
//! one IPv6 host is normally handed a whole `/64`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// The numbers a server enforces. Pick [`Limits::public`] or
/// [`Limits::loopback`]; tests shrink fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most connections open at once; past it a new one is closed at once.
    pub max_connections: usize,
    /// The most connections one client may hold open at once.
    pub conns_per_client: usize,
    /// How long a client gets for the TLS handshake, its request head and its
    /// body together.
    pub deadline: Duration,
    /// The largest request head accepted.
    pub max_head_bytes: usize,
    /// The largest request body accepted.
    pub max_body_bytes: usize,
    /// How long one write may stall before the connection is dropped.
    pub write_stall: Duration,
    /// The most long-lived streams one client may hold at once.
    pub streams_per_client: usize,
    /// The most [`StreamClass::Counted`] streams open at once.
    pub max_counted_streams: usize,
}

impl Limits {
    /// The numbers for a bind open to the internet: a browser opens six or so
    /// connections per host and a few tabs fit; two tabs each hold a player
    /// and an event feed.
    #[must_use]
    pub const fn public() -> Self {
        Self {
            max_connections: 1024,
            conns_per_client: 16,
            deadline: Duration::from_secs(10),
            max_head_bytes: 8 * 1024,
            max_body_bytes: 64 * 1024,
            write_stall: Duration::from_secs(30),
            streams_per_client: 4,
            max_counted_streams: 256,
        }
    }

    /// The numbers for a loopback bind: every local tool is one client, so
    /// the per-client caps are wide.
    #[must_use]
    pub const fn loopback() -> Self {
        Self {
            max_connections: 128,
            conns_per_client: 128,
            deadline: Duration::from_secs(10),
            max_head_bytes: 8 * 1024,
            max_body_bytes: 64 * 1024,
            write_stall: Duration::from_secs(30),
            streams_per_client: 64,
            max_counted_streams: 64,
        }
    }
}

/// How a long-lived stream counts against the caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamClass {
    /// Per client and in the overall [`Limits::max_counted_streams`] (event
    /// streams: viewers who never press play must not fill the pool).
    Counted,
    /// Per client only (audio).
    PerClientOnly,
}

/// Why a long-lived stream was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamRefusal {
    /// The client already holds [`Limits::streams_per_client`].
    PerClient,
    /// [`Limits::max_counted_streams`] are open.
    Full,
}

impl StreamRefusal {
    /// The status the refusal is answered with.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::PerClient => 429,
            Self::Full => 503,
        }
    }

    /// The one-line plain-text reason.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::PerClient => "Too many streams open from your address",
            Self::Full => "Full, try again shortly",
        }
    }
}

/// The counters behind long-lived streams.
#[derive(Debug)]
pub struct StreamGuard {
    streams_per_client: usize,
    max_counted: usize,
    streams: Arc<PeerCounts>,
    counted: Arc<AtomicUsize>,
}

/// A long-lived stream's places; dropping it frees them.
#[derive(Debug)]
pub struct StreamSlot {
    _peer: PeerSlot,
    _counted: Option<TotalSlot>,
}

impl StreamGuard {
    #[must_use]
    pub fn new(streams_per_client: usize, max_counted: usize) -> Arc<Self> {
        Arc::new(Self {
            streams_per_client,
            max_counted,
            streams: Arc::default(),
            counted: Arc::default(),
        })
    }

    /// Places for one long-lived stream from `ip`.
    ///
    /// # Errors
    /// The client holds its share, or the counted streams are full.
    pub fn open(&self, ip: IpAddr, class: StreamClass) -> Result<StreamSlot, StreamRefusal> {
        let peer = self
            .streams
            .try_acquire(ip, self.streams_per_client)
            .ok_or(StreamRefusal::PerClient)?;
        let counted = match class {
            StreamClass::Counted => {
                if self.counted.fetch_add(1, Ordering::Relaxed) >= self.max_counted {
                    self.counted.fetch_sub(1, Ordering::Relaxed);
                    return Err(StreamRefusal::Full);
                }
                Some(TotalSlot(Arc::clone(&self.counted)))
            }
            StreamClass::PerClientOnly => None,
        };
        Ok(StreamSlot {
            _peer: peer,
            _counted: counted,
        })
    }

    /// Streams `ip` holds now.
    #[must_use]
    pub fn held(&self, ip: IpAddr) -> usize {
        self.streams.held(ip)
    }
}

// ---- per-client counts -----------------------------------------------------

/// The address a peer is counted under: itself for IPv4 (an IPv4-mapped
/// IPv6 address included), its `/64` for IPv6.
#[must_use]
pub fn peer_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (!0u128 << 64))),
            IpAddr::V4,
        ),
    }
}

/// Open things per client.
#[derive(Debug, Default)]
pub struct PeerCounts(Mutex<HashMap<IpAddr, usize>>);

impl PeerCounts {
    /// One more for `ip`, unless it already holds `cap`.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr, cap: usize) -> Option<PeerSlot> {
        let key = peer_key(ip);
        let mut m = self.0.lock().expect("peer counts poisoned");
        let n = m.entry(key).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(PeerSlot {
            counts: Arc::clone(self),
            key,
        })
    }

    /// What `ip` holds now.
    #[must_use]
    pub fn held(&self, ip: IpAddr) -> usize {
        let m = self.0.lock().expect("peer counts poisoned");
        m.get(&peer_key(ip)).copied().unwrap_or(0)
    }
}

/// One address's hold on one thing; dropping it gives the thing back.
#[derive(Debug)]
pub struct PeerSlot {
    counts: Arc<PeerCounts>,
    key: IpAddr,
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut m = self.counts.0.lock().expect("peer counts poisoned");
        if let Some(n) = m.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.key);
            }
        }
    }
}

// ---- connection pools ---------------------------------------------------------

/// One listener's connections: a total and a per-client cap.
#[derive(Debug)]
pub struct Pool {
    open: Arc<AtomicUsize>,
    max: usize,
    per_client: Arc<PeerCounts>,
    per_client_max: usize,
}

impl Pool {
    #[must_use]
    pub fn new(max: usize, per_client_max: usize) -> Arc<Self> {
        Arc::new(Self {
            open: Arc::new(AtomicUsize::new(0)),
            max,
            per_client: Arc::default(),
            per_client_max,
        })
    }

    /// A slot for a connection from `ip`, or `None` when the pool or the
    /// address is full.
    #[must_use]
    pub fn admit(&self, ip: IpAddr) -> Option<ConnSlot> {
        if self.open.fetch_add(1, Ordering::Relaxed) >= self.max {
            self.open.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        let total = TotalSlot(Arc::clone(&self.open));
        let peer = self.per_client.try_acquire(ip, self.per_client_max)?;
        Some(ConnSlot {
            _total: total,
            _peer: peer,
        })
    }

    /// Connections open now.
    #[must_use]
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct TotalSlot(Arc<AtomicUsize>);

impl Drop for TotalSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A connection's place in its pool, held for as long as its thread runs.
#[derive(Debug)]
pub struct ConnSlot {
    _total: TotalSlot,
    _peer: PeerSlot,
}
