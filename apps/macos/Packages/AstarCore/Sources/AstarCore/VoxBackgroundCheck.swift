// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Result of a VOX background-noise measurement: the mic's noise floor (sampled
/// while the user is silent) and whether a chosen VOX threshold sits safely above
/// it. A threshold at or below the noise floor would let background noise key the
/// transmitter, so the calibration test warns and can suggest a safe value.
public struct VoxBackgroundCheck: Equatable {
    /// The loudest background sample (dBFS) — the floor the threshold must clear.
    public let noiseFloorDBFS: Float

    /// Build from a window of unkeyed mic levels. Empty input falls back to
    /// `silenceFloor` (treat "no samples" as digital silence, not a real floor).
    public init(samples: [Float], silenceFloor: Float = -120) {
        noiseFloorDBFS = samples.max() ?? silenceFloor
    }

    /// True when `threshold` sits at least `margin` dB above the noise floor.
    public func clears(threshold: Float, margin: Float = 6) -> Bool {
        threshold - noiseFloorDBFS >= margin
    }

    /// A threshold that clears the floor by `margin`, capped at 0 dBFS.
    public func suggestedThreshold(margin: Float = 6) -> Float {
        min(0, noiseFloorDBFS + margin)
    }
}
