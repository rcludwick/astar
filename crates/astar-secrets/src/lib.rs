// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The private credentials file and the in-memory secret store, shared by
//! astar-server (`credentials.toml` beside `node.toml`) and astar-netrec
//! (its stream keys).
//!
//! The file is flat TOML, one string per key. It is written mode 0600 before
//! a byte lands, through a temp file renamed over the target, and refused at
//! load when group or others can read it. No value ever appears in an error:
//! a parse error is not forwarded at all, because the parser renders the
//! offending line and every line of this file is a credential.
//!
//! What each key means is the caller's business; this crate only stores
//! strings under names.

mod file;
mod store;

pub use file::{keys, load_private, load_private_with, remove, replace_all, save};
pub use store::SecretStore;

/// One secret value. `Debug` prints `Secret(***)`; there is no `Display`
/// and no `Serialize`, so the value reaches a log line, an error or a state
/// file only through an explicit [`Secret::expose`].
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wrap a value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value itself. Call it only where the value is handed to the one
    /// thing that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}
