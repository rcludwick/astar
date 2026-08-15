// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use std::fmt;

/// Errors from the ASL3 service layer. Never contains credentials.
#[derive(Debug)]
pub enum Asl3Error {
    /// HTTP transport failure (connect, TLS, timeout).
    Http(String),
    /// Portal login did not yield a session cookie — credentials likely wrong.
    Login,
    /// The web-transceiver page had no `callingName` token (login failed
    /// silently, the account lacks WT access for this node, or markup changed).
    TokenNotFound,
    /// DNS query/transport failure.
    Dns(String),
    /// Neither a TXT directory entry nor an A record exists for the node.
    NoRecords { node: String },
}

impl fmt::Display for Asl3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "portal HTTP error: {e}"),
            Self::Login => write!(f, "portal login failed (no session cookie)"),
            Self::TokenNotFound => write!(
                f,
                "no web-transceiver token in the portal response \
                 (bad credentials, no WT access for this node, or markup changed)"
            ),
            Self::Dns(e) => write!(f, "node directory DNS error: {e}"),
            Self::NoRecords { node } => write!(f, "node {node} not found in the directory"),
        }
    }
}

impl std::error::Error for Asl3Error {}
