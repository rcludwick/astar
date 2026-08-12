// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Userspace `WireGuard` link transport for the astar engine (iax-8516).
//!
//! Gives a CGNAT'd node a guaranteed-reachable inbound UDP port by routing
//! through a public-IP VPS relay. boringtun (`Tunn`) drives the crypto; the
//! [`WgStack`] owns the data plane entirely in userspace — internal IPv4/UDP
//! sockets over one shared tunnel, no TUN device, no root. (The original
//! TUN-based `WgTunnel`/`SysTun` path from iax-99ae was retired by iax-580b.)

mod config;
mod io;
mod packet;
mod stack;
mod udp;

pub use boringtun::x25519;
pub use config::{SecretResolver, WgConfigError, WgLinkConfig};
pub use io::{FakeTransport, UdpTransport};
pub use packet::{
    IPV4_HEADER_LEN, PacketError, ParsedUdp4, UDP_HEADER_LEN, build_udp4, parse_udp4,
};
pub use stack::{WakeCallback, WgSocket, WgStack, WgStackStatus};
pub use udp::UdpSocketTransport;
