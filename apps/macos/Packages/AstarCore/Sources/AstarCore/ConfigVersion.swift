// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The version of astar's configuration — the shape of the saved preferences
/// and of an exported `.astarconfig` (astar-b52e).
///
/// One number with two homes: stamped into the preferences domain under
/// `config.version`, and written into every archive's envelope. A file and the
/// Mac that wrote it must never claim different versions, which is why
/// `ConfigArchive.currentVersion` is this constant rather than its own.
///
/// ## When to bump it
///
/// **It marks translation, not change.** Bump it only when existing saved data
/// would be *misread* by the new code unless something rewrites it — a key
/// renamed, a unit changed, a meaning inverted, a type swapped.
///
/// **Adding is not a bump.** New fields and whole new sections leave it at the
/// current number, because both directions already cope: an older reader
/// ignores what it does not recognise, and a newer reader treats what is absent
/// as unset. That tolerance is what keeps files portable, and spending a
/// version number on an addition throws it away for nothing.
///
/// Anything that does bump it owes a translation from the previous version,
/// because old files and old preference domains do not stop existing.
public enum ConfigVersion {
    /// The version this build reads and writes.
    public static let current = 1

    /// Where the version lives in the preferences domain.
    ///
    /// Deliberately outside the `audio.` / `m17.` / `serial.` / `ui.` prefixes
    /// the export sweeps, so it can never ride along inside a section — an
    /// imported file must not be able to overwrite the reader's own version.
    public static let defaultsKey = "config.version"

    /// Record the current version in a preferences domain. Idempotent; call it
    /// at launch.
    public static func stamp(_ defaults: UserDefaults = .standard) {
        defaults.set(current, forKey: defaultsKey)
    }

    /// The version a preferences domain was last written by.
    ///
    /// An unstamped domain reads as **1**, not 0: everything written before the
    /// version existed *is* version 1 — that is what version 1 describes — and
    /// reporting 0 would invent a migration that has nothing to do.
    public static func installed(in defaults: UserDefaults = .standard) -> Int {
        let raw = defaults.integer(forKey: defaultsKey)
        return raw == 0 ? 1 : raw
    }
}
