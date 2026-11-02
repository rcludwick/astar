// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![allow(dead_code)]
#[cfg(feature = "tls")]
pub mod tls;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Send `raw` on a fresh connection and read until the server closes it.
pub fn send(addr: SocketAddr, raw: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(raw).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// The public limits with a short request deadline.
pub fn quick() -> astar_http::Limits {
    astar_http::Limits {
        deadline: Duration::from_millis(300),
        ..astar_http::Limits::public()
    }
}
