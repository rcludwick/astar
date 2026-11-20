// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The shared, interior-mutable map of name → secret.
//!
//! Deliberately no `Debug`, so the values can never leak through `{:?}`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

/// Name → secret, shared by every clone.
#[derive(Clone, Default)]
pub struct SecretStore {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl SecretStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.inner.lock().expect("SecretStore mutex poisoned")
    }

    /// Store `secret` under `username`, replacing any value already there.
    pub fn put(&self, username: impl Into<String>, secret: impl Into<String>) {
        self.lock().insert(username.into(), secret.into());
    }

    /// The secret held under `username`, if any.
    #[must_use]
    pub fn get(&self, username: &str) -> Option<String> {
        self.lock().get(username).cloned()
    }

    /// Forget `username`. A no-op when it is unknown.
    pub fn remove(&self, username: &str) {
        self.lock().remove(username);
    }

    /// The usernames held, sorted. Names only.
    #[must_use]
    pub fn usernames(&self) -> Vec<String> {
        let mut names: Vec<String> = self.lock().keys().cloned().collect();
        names.sort();
        names
    }

    /// Every `(username, secret)` pair, sorted by username.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Usernames are HashMap keys, so unique: tuple order is key order.
        entries.sort_unstable();
        entries
    }
}
