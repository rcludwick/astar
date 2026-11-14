// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A stub server for other crates' tests: it binds `127.0.0.1:0` and nothing
//! else, records every request it gets and answers with a closure.

use crate::{Limits, Reply, Request, Response, Server, ServerOptions};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// One request a [`Stub`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub method: String,
    /// The request target as sent: path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    /// Empty when the body was refused (too large, chunked, cut short).
    pub body: Vec<u8>,
}

impl Recorded {
    /// The first header called `name`, matched without regard to case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The target without its query.
    #[must_use]
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    /// The body as text; empty when it is not UTF-8.
    #[must_use]
    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }
}

/// A running stub server. Dropping it stops it.
pub struct Stub {
    server: Server,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

/// Start a stub on `127.0.0.1:0` that answers every request with `f`.
///
/// # Panics
/// The loopback bind failed.
pub fn serve<F>(f: F) -> Stub
where
    F: Fn(&Recorded) -> Response + Send + Sync + 'static,
{
    let seen: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let log = Arc::clone(&seen);
    let server = Server::bind_with(
        "127.0.0.1:0",
        Limits::loopback(),
        ServerOptions {
            name: "stub",
            csp: None,
            ..ServerOptions::default()
        },
        move |req: &mut Request<'_>| {
            let body = req.body().map(<[u8]>::to_vec).unwrap_or_default();
            let rec = Recorded {
                method: req.method().to_owned(),
                target: req.target().to_owned(),
                headers: req.headers().to_vec(),
                body,
            };
            let answer = f(&rec);
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(rec);
            Reply::Response(answer)
        },
    )
    .expect("bind the loopback stub");
    Stub { server, seen }
}

impl Stub {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr())
    }

    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    /// Every request so far, oldest first.
    #[must_use]
    pub fn requests(&self) -> Vec<Recorded> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// `css` with every `/* ... */` comment replaced by one space, so that
/// prose ("a wall of red", the button comment) is never mistaken for a
/// declaration. An unterminated comment swallows the rest of the text.
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some((before, after)) = rest.split_once("/*") {
        out.push_str(before);
        out.push(' ');
        rest = after.split_once("*/").map_or("", |(_, tail)| tail);
    }
    out.push_str(rest);
    out
}

/// Colour literals in a CSS fragment, comments excluded: hex colours
/// (`#fff`, `#1e3a8a`), numeric `rgb(`/`rgba(`/`hsl(`/`hsla(` calls in
/// any case, and the bare names a contributor reaches for by reflex
/// (`white`, `black`, `red`, ...). `transparent` and `currentColor` are
/// scheme-neutral and allowed. A `#` followed by an identifier that
/// merely starts with hex digits (`#badge`, `#dead-end`) is a selector,
/// not a colour: the digit run does not end at a word boundary. A name
/// inside a longer identifier (`--green-rgb`, `.red-x`, `border`) is not
/// a colour either: a name must start a word that no `.`, `#`, `-`, `_`
/// or alphanumeric precedes. `rgba(var(--green-rgb), ...)` is not a
/// literal because a token, not a number, follows the paren. Only ASCII
/// bytes are used as slice starts, so a multi-byte character cannot
/// split a char.
#[must_use]
pub fn colour_literals(css: &str) -> Vec<String> {
    const NAMED: [&str; 9] = [
        "white", "black", "red", "green", "blue", "gray", "grey", "orange", "yellow",
    ];
    let css = strip_comments(css);
    let bytes = css.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_';
    let mut out = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' {
            let run = bytes[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .count();
            let next = bytes.get(i + 1 + run).copied().unwrap_or(b' ');
            if matches!(run, 3 | 4 | 6 | 8) && !is_word(next) {
                out.push(css[i..=i + run].to_string());
            }
            continue;
        }
        let prev = if i == 0 { b' ' } else { bytes[i - 1] };
        if !b.is_ascii_alphabetic() || is_word(prev) || prev == b'.' || prev == b'#' {
            continue;
        }
        let tail = &css[i..];
        let len = tail.bytes().take_while(|&c| is_word(c)).count();
        let word = tail[..len].to_ascii_lowercase();
        let numeric_call = tail[len..]
            .strip_prefix('(')
            .is_some_and(|args| args.trim_start().starts_with(|c: char| c.is_ascii_digit()));
        if matches!(word.as_str(), "rgb" | "rgba" | "hsl" | "hsla") && numeric_call {
            out.push(tail.chars().take(24).collect());
        } else if NAMED.contains(&word.as_str()) {
            out.push(tail[..len].to_string());
        }
    }
    out
}
