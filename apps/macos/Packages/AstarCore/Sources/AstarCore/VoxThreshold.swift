// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// What a VOX threshold is measured against, and how an old one is carried
/// over.
///
/// Up to 0.1.20-beta VOX compared its threshold with the mic's PEAK: the single
/// loudest sample of the latest ~10 ms capture callback. One click was as loud
/// as a word, so a quiet room keyed the radio now and then. VOX now compares
/// with the mean power over the last 50 ms (the engine's `vox_db`), and a
/// number on that scale reads lower than the peak of the same sound — so a
/// threshold saved on the old scale would leave VOX about 10 dB deafer.
///
/// The fix keeps `ConfigVersion` where it is: the new scale gets its own key
/// (`audio.voxThresholdRMSDBFS`, and `voxThresholdRMS` inside a saved config),
/// and a value found only under the old key is translated once by
/// `peakToRMSOffsetDB`. Nothing old is misread, because nothing reads the old
/// key as if it were the new one.
public enum VoxThreshold {
    /// Preferences key for the threshold on the windowed-power scale.
    public static let key = "audio.voxThresholdRMSDBFS"
    /// Preferences key the peak-scale threshold lived under. Read, never
    /// written.
    public static let legacyPeakKey = "audio.voxThresholdDBFS"

    /// How much lower the 50 ms mean power reads than the per-callback peak
    /// for the sounds a VOX threshold sits between, in dB.
    ///
    /// Speech has a peak-to-RMS ratio (crest factor) of about 9–12 dB. The
    /// background a threshold has to clear is close to Gaussian noise, and
    /// the largest of the ~480 samples in a 10 ms callback lands about three
    /// standard deviations out — again about 10 dB above its RMS. Both sides
    /// of the threshold move down by roughly the same 10 dB, so moving the
    /// threshold by 10 dB keeps it where it sat between them.
    public static let peakToRMSOffsetDB: Float = -10

    /// The slider's range. The bottom sits 2 dB above the meter floor
    /// (`VoxGate.meterFloorDBFS`), which a quiet room reads exactly, so no
    /// setting and no translation can put the threshold where silence
    /// already meets it.
    public static let range: ClosedRange<Float> = (VoxGate.meterFloorDBFS + 2)...0

    /// The old default (−40 dBFS peak) carried over by the same offset.
    public static let defaultDBFS: Float = -50

    /// Translate a threshold calibrated against the peak to the windowed-power
    /// scale, clamped to the slider's range.
    public static func fromPeak(_ peakDBFS: Float) -> Float {
        clamped(peakDBFS + peakToRMSOffsetDB)
    }

    /// Clamp to `range`; a non-finite value is nobody's choice, so it reads
    /// as the default.
    public static func clamped(_ dbfs: Float) -> Float {
        guard dbfs.isFinite else { return defaultDBFS }
        return min(max(dbfs, range.lowerBound), range.upperBound)
    }
}

extension VoxThreshold {
    /// An imported settings slice with the old peak-scale threshold carried
    /// over: the old key is dropped, and if the slice has no new-scale value
    /// it gains one translated by `fromPeak`. A file from 0.1.20-beta or
    /// earlier then imports exactly as a Mac upgrading in place does; a
    /// newer file, which carries the new key, is used as is.
    static func translatingLegacy(_ settings: [String: SettingValue]) -> [String: SettingValue] {
        guard let legacy = settings[legacyPeakKey] else { return settings }
        var out = settings
        out[legacyPeakKey] = nil
        if out[key] == nil {
            let peak: Float?
            switch legacy {
            case .double(let v): peak = Float(v)
            case .int(let v): peak = Float(v)
            case .bool, .string: peak = nil
            }
            if let peak { out[key] = .double(Double(fromPeak(peak))) }
        }
        return out
    }
}
