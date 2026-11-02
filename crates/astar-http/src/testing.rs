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
