// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `EchoLink` protocol primitives. Sans-IO: nothing in this crate opens a
//! socket, except the test fakes in `fake`, which exist only under the
//! `loopback-fakes` feature and bind 127.0.0.1 only.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

pub mod directory;
#[cfg(feature = "loopback-fakes")]
pub mod fake;
pub mod qso;
pub mod rtcp;
pub mod schedule;
pub mod voice;

/// UDP port that carries audio datagrams.
pub const AUDIO_PORT: u16 = 5198;
/// UDP port that carries RTCP control datagrams (SDES, BYE).
pub const CONTROL_PORT: u16 = 5199;
/// TCP port of the directory servers.
pub const DIRECTORY_PORT: u16 = 5200;

/// Every timer the `EchoLink` protocol uses, in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timings {
    pub status_refresh: Duration,
    pub list_refresh: Duration,
    pub list_min_interval: Duration,
    pub backoff_start: Duration,
    pub backoff_cap: Duration,
    pub directory_connect_timeout: Duration,
    pub directory_read_timeout: Duration,
    pub keepalive: Duration,
    pub give_up_after: u32,
    pub connection_timeout: Duration,
    pub talk_hold: Duration,
    /// The longest one over astar sends before it stops on its own: 10
    /// minutes of continuous keying. This one is astar's, not `EchoLib`'s.
    /// It bounds a stuck key, so nothing a person can set turns it off;
    /// tests shorten it to see it fire.
    pub max_transmit: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            status_refresh: Duration::from_secs(300),
            list_refresh: Duration::from_secs(600),
            list_min_interval: Duration::from_secs(30),
            backoff_start: Duration::from_secs(20),
            backoff_cap: Duration::from_secs(600),
            directory_connect_timeout: Duration::from_secs(10),
            directory_read_timeout: Duration::from_secs(30),
            keepalive: Duration::from_secs(10),
            give_up_after: 5,
            connection_timeout: Duration::from_secs(50),
            talk_hold: Duration::from_millis(320),
            max_transmit: Duration::from_secs(600),
        }
    }
}

/// The two UDP endpoints of a peer station.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddr {
    pub audio: SocketAddr,
    pub control: SocketAddr,
}

impl PeerAddr {
    /// A peer on the standard ports (5198 audio, 5199 control).
    #[must_use]
    pub fn fixed(ip: Ipv4Addr) -> PeerAddr {
        PeerAddr {
            audio: SocketAddr::from((ip, AUDIO_PORT)),
            control: SocketAddr::from((ip, CONTROL_PORT)),
        }
    }
}
