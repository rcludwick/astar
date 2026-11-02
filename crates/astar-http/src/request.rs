// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One request as a handler sees it. The body is read only when the handler
//! asks for it, and only up to the cap.

use crate::guards::normalize_host;
use crate::parse::RequestHead;
use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;

/// What a body is read from: the connection's buffered reader, which can
/// also send the interim `100 Continue` a client waits for.
pub(crate) trait BodySource: BufRead {
    /// Write `bytes` to the client at once.
    ///
    /// # Errors
    /// The write failed.
    fn interim(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl BodySource for BufReader<Box<dyn crate::response::ReadWrite>> {
    fn interim(&mut self, bytes: &[u8]) -> io::Result<()> {
        let w = self.get_mut();
        w.write_all(bytes)?;
        w.flush()
    }
}

/// The interim answer to `Expect: 100-continue`.
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Why a body was refused: its status says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyError {
    /// 400 cut short or a bad `Content-Length`, 411 `Transfer-Encoding`,
    /// 413 over the cap.
    pub status: u16,
}

enum BodyState {
    Unread,
    Read(Vec<u8>),
    Refused(u16),
}

/// A request: its head, where it came from, and its body on demand.
pub struct Request<'a> {
    head: &'a RequestHead,
    peer: SocketAddr,
    tls: bool,
    reader: &'a mut dyn BodySource,
    max_body: usize,
    body: BodyState,
}

/// A `Content-Length` value: ASCII digits only. `Err(400)` for anything
/// else (a sign, a list, hex, spaces, empty); `Err(413)` for a value too
/// big to hold, which is over any cap.
fn content_length(v: &str) -> Result<usize, u16> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(400);
    }
    v.bytes()
        .try_fold(0usize, |n, d| {
            n.checked_mul(10)?.checked_add(usize::from(d - b'0'))
        })
        .ok_or(413)
}

impl<'a> Request<'a> {
    pub(crate) fn new(
        head: &'a RequestHead,
        peer: SocketAddr,
        tls: bool,
        reader: &'a mut dyn BodySource,
        max_body: usize,
    ) -> Self {
        Self {
            head,
            peer,
            tls,
            reader,
            max_body,
            body: BodyState::Unread,
        }
    }

    #[must_use]
    pub fn method(&self) -> &str {
        &self.head.method
    }
    /// The request target as sent: path and query.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.head.target
    }
    #[must_use]
    pub fn path(&self) -> &str {
        self.head.path()
    }
    /// The part after `?`, empty when there is none.
    #[must_use]
    pub fn query(&self) -> &str {
        self.head.query()
    }
    /// `HTTP/1.1` or `HTTP/1.0`.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.head.version
    }
    /// The first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.head.headers
    }
    /// The address the connection came from.
    #[must_use]
    pub const fn peer(&self) -> SocketAddr {
        self.peer
    }
    /// The connection is TLS.
    #[must_use]
    pub const fn tls(&self) -> bool {
        self.tls
    }
    /// The `Host` header, normalised ([`normalize_host`]).
    #[must_use]
    pub fn host(&self) -> Option<String> {
        self.header("Host").map(normalize_host)
    }
    /// A `Forwarded` or `X-Forwarded-For` header is present.
    #[must_use]
    pub fn forwarded(&self) -> bool {
        self.header("Forwarded").is_some() || self.header("X-Forwarded-For").is_some()
    }

    /// The body, read on the first call and kept. A client that sent
    /// `Expect: 100-continue` on HTTP/1.1 is told `100 Continue` first, once,
    /// and only when the body is going to be read (not for one refused on
    /// its framing or its announced length).
    ///
    /// # Errors
    /// [`BodyError`]: 411 for any `Transfer-Encoding` (with or without a
    /// `Content-Length`), 400 for a bad `Content-Length` or a body cut
    /// short (the deadline included), 413 over the cap.
    pub fn body(&mut self) -> Result<&[u8], BodyError> {
        if let BodyState::Unread = self.body {
            self.body = match self.read_body() {
                Ok(b) => BodyState::Read(b),
                Err(s) => BodyState::Refused(s),
            };
        }
        match &self.body {
            BodyState::Read(b) => Ok(b),
            BodyState::Refused(s) => Err(BodyError { status: *s }),
            BodyState::Unread => unreachable!("the body was just read"),
        }
    }

    /// `Expect: 100-continue` from an HTTP/1.1 client: an HTTP/1.0 client is
    /// never sent a 1xx (RFC 9110 15.2).
    fn expects_continue(&self) -> bool {
        self.head.version == "HTTP/1.1"
            && self
                .head
                .header("Expect")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"))
    }

    fn read_body(&mut self) -> Result<Vec<u8>, u16> {
        if self.head.header("Transfer-Encoding").is_some() {
            return Err(411);
        }
        let Some(len) = self.head.header("Content-Length") else {
            return Ok(Vec::new());
        };
        let len = content_length(len)?;
        if len > self.max_body {
            return Err(413);
        }
        if len > 0 && self.expects_continue() {
            // A client that cannot be told to go on will not send the body.
            self.reader.interim(CONTINUE).map_err(|_| 400u16)?;
        }
        let mut b = vec![0u8; len];
        self.reader.read_exact(&mut b).map_err(|_| 400u16)?;
        Ok(b)
    }
}
