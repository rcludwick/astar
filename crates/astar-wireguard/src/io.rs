// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Data-plane I/O seam: the underlay UDP transport, with an in-memory fake so
//! the stack can be tested without privileges or a network.

use std::collections::VecDeque;
use std::net::SocketAddr;

/// A UDP transport to/from the `WireGuard` endpoint. Non-blocking semantics —
/// `recv_from` returns `WouldBlock` when no datagram is queued. The `Any`
/// supertrait lets tests downcast a `Box<dyn UdpTransport>` back to
/// [`FakeTransport`] for assertions; it is harmless in production (`Any` is
/// zero-cost).
pub trait UdpTransport: std::any::Any + Send {
    fn send_to(&mut self, data: &[u8], dst: SocketAddr) -> std::io::Result<usize>;
    fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>;
}

fn copy_out(buf: &mut [u8], src: &[u8]) -> usize {
    debug_assert!(
        src.len() <= buf.len(),
        "copy_out: dst buffer too small, packet truncated"
    );
    let n = src.len().min(buf.len());
    buf[..n].copy_from_slice(&src[..n]);
    n
}

fn would_block() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock, "no data")
}

/// In-memory [`UdpTransport`] for tests.
#[derive(Default)]
pub struct FakeTransport {
    inbound: VecDeque<(Vec<u8>, SocketAddr)>,
    pub sent: Vec<(Vec<u8>, SocketAddr)>,
}

impl FakeTransport {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Queue a datagram to be returned by the next `recv_from`.
    pub fn push_inbound(&mut self, data: Vec<u8>, src: SocketAddr) {
        self.inbound.push_back((data, src));
    }
}

impl UdpTransport for FakeTransport {
    fn send_to(&mut self, data: &[u8], dst: SocketAddr) -> std::io::Result<usize> {
        self.sent.push((data.to_vec(), dst));
        Ok(data.len())
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self.inbound.pop_front() {
            Some((data, src)) => Ok((copy_out(buf, &data), src)),
            None => Err(would_block()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_transport_recv_from_empty_is_wouldblock() {
        let mut t = FakeTransport::new();
        let mut buf = [0u8; 8];
        let err = t.recv_from(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn fake_transport_roundtrips() {
        let mut t = FakeTransport::new();
        let src: SocketAddr = "127.0.0.1:51820".parse().unwrap();
        t.push_inbound(vec![7, 7], src);
        let mut buf = [0u8; 8];
        let (n, from) = t.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[7, 7]);
        assert_eq!(from, src);
        t.send_to(&[5], src).unwrap();
        assert_eq!(t.sent, vec![(vec![5], src)]);
    }
}
