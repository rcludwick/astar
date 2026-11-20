// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import CoreGraphics
import Foundation

/// The SSTV picture the call view's panel shows: one airing now, or the one
/// that just ended.
///
/// The decoder draws a picture in one go when it ends, so while it airs there
/// are no pixels yet: the panel shows how far through it is (`elapsedSecs`
/// of `expectedSecs`, or `lines` of `height`), and the finished picture
/// appears in `image` at the end.
public struct SstvLive: Equatable {
    /// The picture's number; a new number is a new picture.
    public var seq: UInt64
    /// The mode's display name, such as `"PD120"` or `"Robot 36"`.
    public var mode: String
    /// Who sent it, when the network says.
    public var talker: String?
    /// When the picture started.
    public var startedAt: Date
    /// Rows heard so far. While airing it is the engine's estimate from the
    /// clock; finished, the rows the picture kept.
    public var lines: Int
    /// The picture's full height in rows.
    public var height: Int
    /// The picture's width in pixels, as the engine reports it for the mode;
    /// 0 when unknown.
    public var width: Int
    /// How long the picture takes to send, in seconds, while it airs.
    public var expectedSecs: Double?
    /// How long it has been airing, in seconds, while it airs.
    public var elapsedSecs: Double?
    /// The picture has ended: saved, partly saved, or lost.
    public var finished: Bool
    /// It ended with nothing worth keeping.
    public var lost: Bool
    /// It ended before its last row; the rows heard were kept.
    public var partial: Bool
    /// The saved PNG, once finished and kept.
    public var file: URL?
    /// The finished picture's pixels. Equality compares only whether there
    /// is one, never the pixels.
    public var image: CGImage?

    public init(
        seq: UInt64, mode: String, talker: String?, startedAt: Date, lines: Int, height: Int,
        width: Int = 0, expectedSecs: Double? = nil, elapsedSecs: Double? = nil,
        finished: Bool = false, lost: Bool = false, partial: Bool = false, file: URL? = nil,
        image: CGImage? = nil
    ) {
        self.seq = seq
        self.mode = mode
        self.talker = talker
        self.startedAt = startedAt
        self.lines = lines
        self.height = height
        self.width = width
        self.expectedSecs = expectedSecs
        self.elapsedSecs = elapsedSecs
        self.finished = finished
        self.lost = lost
        self.partial = partial
        self.file = file
        self.image = image
    }

    /// How far through the picture is, 0...1: by the clock while it airs,
    /// by rows once it has ended.
    public var progress: Double {
        if !finished, let expectedSecs, expectedSecs > 0, let elapsedSecs {
            return min(max(elapsedSecs / expectedSecs, 0), 1)
        }
        guard height > 0 else { return finished ? 1 : 0 }
        return min(max(Double(lines) / Double(height), 0), 1)
    }

    /// Width over height for the picture's frame: the pixels' own shape
    /// once there are pixels, the mode's size from the engine while it airs,
    /// and 5:4 when neither is known.
    public var aspectRatio: Double {
        Self.aspectRatio(
            imageWidth: image?.width ?? 0, imageHeight: image?.height ?? 0, width: width,
            height: height)
    }

    /// `aspectRatio` from plain sizes, for views that hold their own copy.
    public static func aspectRatio(imageWidth: Int, imageHeight: Int, width: Int, height: Int)
        -> Double
    {
        if imageWidth > 0, imageHeight > 0 { return Double(imageWidth) / Double(imageHeight) }
        if width > 0, height > 0 { return Double(width) / Double(height) }
        return 5.0 / 4.0
    }

    /// What VoiceOver says for the picture. A talker that is missing or
    /// blank is left out, rather than read as "from" and nothing.
    public static func pictureLabel(
        mode: String, talker: String?, finished: Bool, lost: Bool, partial: Bool
    ) -> String {
        let name = talker?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let who = name.isEmpty ? "" : " from \(name)"
        if lost { return "\(mode) picture\(who), lost" }
        if finished {
            return partial
                ? "\(mode) picture\(who), partly received and saved"
                : "\(mode) picture\(who), saved"
        }
        return "Receiving \(mode) picture\(who)"
    }

    public static func == (a: SstvLive, b: SstvLive) -> Bool {
        a.seq == b.seq && a.mode == b.mode && a.talker == b.talker && a.startedAt == b.startedAt
            && a.lines == b.lines && a.height == b.height && a.width == b.width
            && a.expectedSecs == b.expectedSecs
            && a.elapsedSecs == b.elapsedSecs && a.finished == b.finished && a.lost == b.lost
            && a.partial == b.partial && a.file == b.file
            && (a.image == nil) == (b.image == nil)
    }

    /// The frame's RGBA as a `CGImage`, or `nil` when its size and its bytes
    /// disagree.
    public static func image(from frame: SstvFrame) -> CGImage? {
        let width = frame.width
        let height = frame.height
        guard width > 0, height > 0, width <= 4096, height <= 4096 else { return nil }
        let rowBytes = width * 4
        guard frame.rgba.count >= rowBytes * height,
            let provider = CGDataProvider(data: frame.rgba.prefix(rowBytes * height) as CFData)
        else { return nil }
        return CGImage(
            width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
            bytesPerRow: rowBytes, space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
            provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
    }
}
