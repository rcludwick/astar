// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The one deadline a connection's request phase shares: TLS handshake, head
//! and body together.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The one deadline a connection's request phase (TLS handshake, head and
/// body) shares. Cleared once the request is read. A stream then bounds
/// each read on its own ([`ReadDeadline::per_read`]), so a stream that
/// reads from a silent client still gets an answer.
#[derive(Debug, Clone)]
pub struct ReadDeadline(Arc<Mutex<Bound>>);

#[derive(Debug, Clone, Copy)]
enum Bound {
    /// Every read shares one deadline.
    At(Instant),
    /// Each read waits at most this long.
    PerRead(Duration),
    /// Reads wait as long as they need.
    None,
}

impl ReadDeadline {
    /// A deadline `d` from now.
    #[must_use]
    pub fn after(d: Duration) -> Self {
        Self(Arc::new(Mutex::new(Bound::At(Instant::now() + d))))
    }

    /// No deadline from here on.
    pub fn clear(&self) {
        *self.0.lock().expect("deadline poisoned") = Bound::None;
    }

    /// No shared deadline from here on; each read waits at most `d`.
    pub fn per_read(&self, d: Duration) {
        *self.0.lock().expect("deadline poisoned") = Bound::PerRead(d);
    }

    /// How long the next read may wait, `None` for as long as it needs.
    ///
    /// # Errors
    /// `TimedOut` once the deadline has passed.
    pub fn remaining(&self) -> io::Result<Option<Duration>> {
        let at = match *self.0.lock().expect("deadline poisoned") {
            Bound::At(at) => at,
            Bound::PerRead(d) => return Ok(Some(d)),
            Bound::None => return Ok(None),
        };
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request not received in time",
            ));
        }
        Ok(Some(left))
    }
}

/// A socket whose every read waits at most until the [`ReadDeadline`]: the
/// read timeout is shrunk to the time left before each one, so trickling a
/// byte at a time gains nothing. TLS sits on top of it, so the handshake is
/// covered too.
pub struct DeadlineSocket {
    tcp: TcpStream,
    deadline: ReadDeadline,
}

impl DeadlineSocket {
    #[must_use]
    pub fn new(tcp: TcpStream, deadline: ReadDeadline) -> Self {
        Self { tcp, deadline }
    }
}

impl Read for DeadlineSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.remaining()?;
        self.tcp.set_read_timeout(left)?;
        self.tcp.read(buf)
    }
}

impl Write for DeadlineSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}
