// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Server-sent events: one frame per write, flushed, and a keep-alive
//! comment when the stream has been quiet.

use std::io::{self, Write};
use std::time::{Duration, Instant};

/// How often [`Writer::new`] sends a keep-alive on a quiet stream.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Frames events onto `W` (usually a [`crate::Conn`] after its head).
pub struct Writer<W: Write> {
    w: W,
    every: Duration,
    last: Instant,
}

impl<W: Write> Writer<W> {
    /// A writer that keeps a quiet stream alive every 15 s.
    pub fn new(w: W) -> Self {
        Self::with_keepalive(w, KEEPALIVE)
    }

    /// A writer that keeps a quiet stream alive every `every`.
    pub fn with_keepalive(w: W, every: Duration) -> Self {
        Self {
            w,
            every,
            last: Instant::now(),
        }
    }

    /// One event: `event: <name>` when named, a `data:` line per line of
    /// `data` (CR, LF and CRLF all end a line, as the client reads them),
    /// then the blank line. CR and LF are dropped from `name`, so neither
    /// can forge a frame.
    ///
    /// # Errors
    /// The write failed: the client has gone, or stalled past the write
    /// timeout.
    pub fn event(&mut self, name: Option<&str>, data: &str) -> io::Result<()> {
        let mut frame = String::with_capacity(data.len() + 16);
        if let Some(name) = name {
            frame.push_str("event: ");
            frame.extend(name.chars().filter(|c| !matches!(c, '\r' | '\n')));
            frame.push('\n');
        }
        for line in data.replace("\r\n", "\n").split(['\r', '\n']) {
            frame.push_str("data: ");
            frame.push_str(line);
            frame.push('\n');
        }
        frame.push('\n');
        self.send(frame.as_bytes())
    }

    /// An unnamed event.
    ///
    /// # Errors
    /// As [`Writer::event`].
    pub fn data(&mut self, data: &str) -> io::Result<()> {
        self.event(None, data)
    }

    /// Send `: keepalive` when nothing has gone out for the interval.
    /// Returns whether it did.
    ///
    /// # Errors
    /// As [`Writer::event`].
    pub fn keepalive_if_due(&mut self) -> io::Result<bool> {
        if self.last.elapsed() < self.every {
            return Ok(false);
        }
        self.send(b": keepalive\n\n")?;
        Ok(true)
    }

    /// The writer underneath.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }

    /// The writer underneath, the framing ended.
    pub fn into_inner(self) -> W {
        self.w
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.w.write_all(bytes)?;
        self.w.flush()?;
        self.last = Instant::now();
        Ok(())
    }
}
