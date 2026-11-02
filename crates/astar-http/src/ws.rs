// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! WebSocket upgrades ([`crate::Reply::WebSocket`]): the handshake is the
//! server's, the frames are tungstenite's.
//!
//! A WebSocket runs over a [`Conn`], so it inherits its read contract: a
//! read waits at most the write stall and then fails with `WouldBlock`
//! (Unix) or `TimedOut` (Windows). That means the client sent nothing yet,
//! not that it left: [`is_quiet`] tells the two apart.

use crate::parse::RequestHead;
use crate::request::Request;
use crate::response::Conn;
use std::io::{self, Write};

pub use tungstenite;
pub use tungstenite::protocol::WebSocketConfig;

/// The socket a [`crate::Reply::WebSocket`] stream is handed.
pub type WebSocket = tungstenite::WebSocket<Conn>;

/// The largest message, and the largest frame, a client may send by
/// default: 64 KiB. A browser listening to a stream sends only small
/// control messages; tungstenite's own defaults (64 MiB, 16 MiB) would let
/// any client make the server buffer megabytes per socket.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// The config a [`crate::Reply::WebSocket`] with `config: None` runs
/// under: [`MAX_MESSAGE`] for a message and for a frame, tungstenite's
/// defaults otherwise. Start from it to raise the limits.
#[must_use]
pub fn default_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

/// The key of a WebSocket upgrade: a `GET` with `Upgrade: websocket` and a
/// `Sec-WebSocket-Key`.
fn upgrade_key<'h>(header: impl Fn(&str) -> Option<&'h str>, method: &str) -> Option<&'h str> {
    let upgrade = header("Upgrade").is_some_and(|u| u.trim().eq_ignore_ascii_case("websocket"));
    let key = header("Sec-WebSocket-Key").filter(|k| !k.is_empty());
    (method == "GET" && upgrade).then_some(key).flatten()
}

/// `req` asks for a WebSocket: a `GET` with `Upgrade: websocket` and a
/// `Sec-WebSocket-Key`.
#[must_use]
pub fn is_upgrade(req: &Request<'_>) -> bool {
    upgrade_key(|n| req.header(n), req.method()).is_some()
}

/// The `Sec-WebSocket-Accept` value for an upgrade, `None` when `head` is
/// not one.
pub(crate) fn accept_key_of(head: &RequestHead) -> Option<String> {
    upgrade_key(|n| head.header(n), &head.method)
        .map(|k| tungstenite::handshake::derive_accept_key(k.as_bytes()))
}

/// The `Sec-WebSocket-Accept` value for `req`, `None` when it is not an
/// upgrade ([`is_upgrade`]). For a [`crate::Reply::Stream`] that decides
/// inside its slot whether to upgrade, then calls [`upgrade`].
#[must_use]
pub fn accept_key(req: &Request<'_>) -> Option<String> {
    upgrade_key(|n| req.header(n), req.method())
        .map(|k| tungstenite::handshake::derive_accept_key(k.as_bytes()))
}

/// Answer `101` on `conn` with `accept` (from [`accept_key`]) and hand back
/// the WebSocket. `config` `None` is [`default_config`].
///
/// # Errors
/// The `101` could not be written.
pub fn upgrade(
    mut conn: Conn,
    accept: &str,
    config: Option<WebSocketConfig>,
) -> io::Result<WebSocket> {
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    conn.write_all(head.as_bytes())?;
    conn.flush()?;
    Ok(tungstenite::WebSocket::from_raw_socket(
        conn,
        tungstenite::protocol::Role::Server,
        Some(config.unwrap_or_else(default_config)),
    ))
}

/// Answer `101` and hand the connection to `run` as a WebSocket.
pub(crate) fn accept(
    conn: Conn,
    accept: &str,
    config: Option<WebSocketConfig>,
    run: Box<dyn FnOnce(WebSocket) + Send>,
) {
    if let Ok(ws) = upgrade(conn, accept, config) {
        run(ws);
    }
}

/// `e` only says the client has sent nothing within the read's bound
/// (`WouldBlock` on Unix, `TimedOut` on Windows): keep going.
#[must_use]
pub fn is_quiet(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(e)
        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}
