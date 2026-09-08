// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// One talkgroup on one DMR network, as a directory would publish it.
///
/// `name` is publisher free text and may carry markup, like every other
/// string in the feed — show it as text, never as markup.
public struct DmrTalkgroup: Equatable, Codable, Sendable, Identifiable {
    /// The number the operator dials. 24-bit on the wire.
    public let tg: UInt32
    public let name: String

    public var id: UInt32 { tg }

    public init(tg: UInt32, name: String) {
        self.tg = tg
        self.name = name
    }
}

/// The talkgroup list for one DMR system, when the directory has one.
///
/// **It does not have one today.** `api/v1/reflectors/dmr/<system>/talkgroups.json`
/// is a separate hamcall-db task; every decode below currently answers `[]`,
/// and that is a complete product rather than a broken one. A DMR talkgroup is
/// a number the operator types — it is how every DMR radio codeplug on earth
/// works — and a list only ever saves the typing.
public enum DmrTalkgroups {
    /// Decode `api/v1/reflectors/dmr/<system>/talkgroups.json`.
    ///
    /// An absent or malformed list is an EMPTY list, never an error: the
    /// number field stays usable either way, so there is nothing for a caller
    /// to do with a thrown error but ignore it.
    ///
    /// Both shapes are accepted — a bare array and a `{"talkgroups": [...]}`
    /// envelope — because the endpoint does not exist yet and guessing wrong
    /// about its envelope should cost a shrug, not a release. A row without a
    /// usable number is dropped; the ones around it are not.
    public static func decode(_ data: Data) -> [DmrTalkgroup] {
        guard !data.isEmpty else { return [] }
        let decoder = JSONDecoder()
        if let envelope = try? decoder.decode(Envelope.self, from: data) {
            return usable(envelope.talkgroups)
        }
        if let bare = try? decoder.decode([Row].self, from: data) {
            return usable(bare)
        }
        return []
    }

    private static func usable(_ rows: [Row]) -> [DmrTalkgroup] {
        rows.compactMap { row in
            guard let tg = row.tg, tg > 0, tg <= DmrDial.maxTalkgroup else { return nil }
            let name = (row.name ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return DmrTalkgroup(tg: tg, name: name.isEmpty ? String(tg) : name)
        }
    }

    /// Lenient by design, exactly as `DirectoryEntry`'s own decoder is: a row
    /// whose fields are the wrong type reads as absent rather than throwing,
    /// so one bad entry cannot take a whole network's list with it.
    private struct Row: Decodable {
        let tg: UInt32?
        let name: String?

        private enum CodingKeys: String, CodingKey { case tg, name }

        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            tg = (try? c.decodeIfPresent(UInt32.self, forKey: .tg)) ?? nil
            name = (try? c.decodeIfPresent(String.self, forKey: .name)) ?? nil
        }
    }

    private struct Envelope: Decodable {
        let talkgroups: [Row]
    }
}
