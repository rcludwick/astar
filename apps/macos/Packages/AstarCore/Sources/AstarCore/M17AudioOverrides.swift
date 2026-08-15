// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Per-network TX audio override for M17 (astar-5d8e): M17 feeds Codec 2 (a
/// vocoder), not a repeater's RF chain — it gets its own tuned mic feed
/// rather than reusing AllStar's. Defaults are Rob's field-tested M17 recipe
/// (astar-m17defaults, 2026-08-04 on-air A/B testing): 25% mic level,
/// compression ON at 80% strength, 80% TX trim — compression beats a raw
/// feed into Codec 2, reversing the earlier "clean chain" default (Rob's
/// AllStar-tuned NR+compression+trim had produced a parrot echo that sounded
/// "like transmitting from inside a box" over M17; further testing found
/// compression alone, at these levels, doesn't). Noise reduction stays off.
///
/// Devices and VOX are deliberately NOT part of this set — those stay
/// whatever the shared `AudioSettings` says, for both networks. Output
/// (speaker) gain also stays shared; only the mic (input) gain joined this
/// override (astar-m17defaults), alongside noise reduction/compression/its
/// strength/TX trim.
public struct M17AudioOverrides: Equatable {
    public var noiseReduction: Bool
    public var compression: Bool
    public var compressionLevel: Float
    public var txTrim: Float
    /// Mic (TX input) gain multiplier (0…2, unity 1.0) — see the type doc for
    /// Rob's field-tested default (astar-m17defaults).
    public var inputGain: Float

    public init(
        noiseReduction: Bool = false, compression: Bool = true,
        compressionLevel: Float = 0.80, txTrim: Float = 0.80,
        inputGain: Float = 0.25
    ) {
        self.noiseReduction = noiseReduction
        self.compression = compression
        self.compressionLevel = compressionLevel
        self.txTrim = txTrim
        self.inputGain = inputGain
    }
}
