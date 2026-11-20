// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One saved SSTV picture, as its line in `pictures.jsonl` describes it.
public struct SstvGalleryItem: Identifiable, Equatable {
    /// The PNG, inside the SSTV folder.
    public var file: URL
    /// When the picture started.
    public var date: Date
    /// The network's display name, such as `"AllStar"`.
    public var network: String
    /// The node or reflector the station was connected to.
    public var target: String
    /// The mode's display name, such as `"PD120"` or `"Robot 36"`.
    public var mode: String
    /// Who sent it, when the network says.
    public var talker: String?
    /// The signal ended before the last row; the rows heard were kept.
    public var partial: Bool

    public var id: URL { file }

    public init(
        file: URL, date: Date, network: String, target: String, mode: String,
        talker: String?, partial: Bool
    ) {
        self.file = file
        self.date = date
        self.network = network
        self.target = target
        self.mode = mode
        self.talker = talker
        self.partial = partial
    }
}

/// Reads the SSTV folder's `pictures.jsonl`, which the engine appends one
/// line to for every picture it saves.
public enum SstvGallery {
    /// The index file's name inside the SSTV folder.
    public static let indexFileName = "pictures.jsonl"

    /// One index line, as the engine writes it.
    private struct Line: Decodable {
        let file: String
        let timeMS: UInt64
        let network: String
        let target: String
        let mode: String
        let talker: String?
        let partial: Bool

        enum CodingKeys: String, CodingKey {
            case file
            case timeMS = "time_ms"
            case network, target, mode, talker, partial
        }
    }

    /// Every picture in `folder`'s index whose file is still there, newest
    /// first. A line that doesn't parse, names anything but a plain file in
    /// the folder, or names a file since deleted is skipped; a missing or
    /// unreadable index is an empty gallery.
    public static func load(folder: URL) -> [SstvGalleryItem] {
        let index = folder.appendingPathComponent(indexFileName, isDirectory: false)
        guard let data = try? Data(contentsOf: index) else { return [] }
        let decoder = JSONDecoder()
        let fm = FileManager.default
        var items: [(order: Int, item: SstvGalleryItem)] = []
        for (order, raw) in data.split(separator: UInt8(ascii: "\n")).enumerated() {
            guard let line = try? decoder.decode(Line.self, from: Data(raw)),
                isPlainName(line.file)
            else { continue }
            let file = folder.appendingPathComponent(line.file, isDirectory: false)
            var isDirectory: ObjCBool = false
            guard fm.fileExists(atPath: file.path, isDirectory: &isDirectory),
                !isDirectory.boolValue
            else { continue }
            let item = SstvGalleryItem(
                file: file, date: Date(timeIntervalSince1970: Double(line.timeMS) / 1000),
                network: line.network, target: line.target, mode: line.mode,
                talker: line.talker, partial: line.partial)
            items.append((order, item))
        }
        // Newest first; two in the same millisecond keep the later line first.
        return
            items
            .sorted { a, b in
                a.item.date != b.item.date ? a.item.date > b.item.date : a.order > b.order
            }
            .map(\.item)
    }

    /// A bare file name: no folder part, not `.` or `..`, nothing hidden.
    private static func isPlainName(_ name: String) -> Bool {
        !name.isEmpty && !name.contains("/") && !name.contains("\0") && !name.hasPrefix(".")
    }
}
