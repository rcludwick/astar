// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Which microphone the analyzer opens on: the caller's explicit choice, else the
/// device the active profile is using (`AudioSettings.input`), else the system default.
public enum MicAnalyzerSeed {
    public static func input(explicit: String?, stored: String?) -> String? {
        if let explicit, !explicit.isEmpty {
            return explicit
        }
        if let stored, !stored.isEmpty {
            return stored
        }
        return nil
    }
}
