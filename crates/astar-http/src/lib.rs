// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar-http: the one synchronous HTTP/1.1 server every astar server uses.
//! A thread per connection under a global and a per-client cap, one
//! deadline for the TLS handshake, head and body together, one request per
//! connection (`Connection: close`), the security headers on every answer.
//! A server, not a framework: routing stays in each application.

pub mod deadline;
pub mod guards;
pub mod limits;
pub mod parse;
mod request;
mod response;
mod server;
pub mod sse;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(feature = "tls")]
pub mod tls;
#[cfg(feature = "ws")]
pub mod ws;

pub use limits::{Limits, StreamClass, StreamGuard, StreamRefusal};
pub use request::{BodyError, Request};
pub use response::{Chunked, Conn, Reply, Response, reason};
pub use server::{Server, ServerOptions};
#[cfg(feature = "tls")]
pub use tls::TlsConfig;
