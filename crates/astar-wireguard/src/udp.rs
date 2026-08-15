// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Real UDP transport over `std::net::UdpSocket` (non-blocking).

use std::net::{SocketAddr, UdpSocket};

use crate::io::UdpTransport;

/// UDP transport bound to an ephemeral local port.
pub struct UdpSocketTransport(UdpSocket);

impl UdpSocketTransport {
    /// Bind `0.0.0.0:0` in non-blocking mode.
    pub fn bound() -> std::io::Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_nonblocking(true)?;
        Ok(Self(sock))
    }
}

impl UdpTransport for UdpSocketTransport {
    fn send_to(&mut self, data: &[u8], dst: SocketAddr) -> std::io::Result<usize> {
        self.0.send_to(data, dst)
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_socket_recv_would_block_when_empty() {
        let mut t = UdpSocketTransport::bound().unwrap();
        let mut buf = [0u8; 16];
        let err = t.recv_from(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }
}
