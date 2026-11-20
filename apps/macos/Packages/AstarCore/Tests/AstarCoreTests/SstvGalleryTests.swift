// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

final class SstvGalleryTests: XCTestCase {
    private var folder: URL!

    override func setUpWithError() throws {
        folder = FileManager.default.temporaryDirectory
            .appendingPathComponent("astar-sstv-gallery-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: folder)
    }

    private func touch(_ name: String) throws {
        try Data([0x89, 0x50, 0x4E, 0x47]).write(to: folder.appendingPathComponent(name))
    }

    /// One index line, in the engine's keys.
    private func line(
        _ file: String, _ timeMS: UInt64, network: String = "AllStar", target: String = "2000",
        mode: String = "PD120", talker: String = "null", partial: Bool = false
    ) -> String {
        #"{"file":"\#(file)","time_ms":\#(timeMS),"network":"\#(network)","#
            + #""target":"\#(target)","mode":"\#(mode)","talker":\#(talker),"#
            + #""partial":\#(partial)}"#
    }

    func testGalleryNewestFirstSkipsBadLinesAndMissingFiles() throws {
        defer {
            try? FileManager.default.removeItem(at: folder.appendingPathComponent("../escape.png"))
        }
        try touch("old.png")
        try touch("new.png")
        try touch("../escape.png")  // lands beside the folder, never read
        let lines = [
            line("old.png", 1_800_000_000_000),
            "not json at all",
            line("gone.png", 1_800_000_500_000),
            line("../escape.png", 1_800_000_600_000),
            line(
                "new.png", 1_800_000_900_000, network: "EchoLink", target: "KX0AAA-R",
                mode: "Robot 36", talker: #""N0CALL""#, partial: true),
            #"{"file":"torn.png","time_ms":18"#,
        ]
        try (lines.joined(separator: "\n") + "\n").write(
            to: folder.appendingPathComponent("pictures.jsonl"), atomically: true, encoding: .utf8)
        let items = SstvGallery.load(folder: folder)

        XCTAssertEqual(items.map(\.file.lastPathComponent), ["new.png", "old.png"])
        let newest = try XCTUnwrap(items.first)
        XCTAssertEqual(newest.date, Date(timeIntervalSince1970: 1_800_000_900))
        XCTAssertEqual(newest.network, "EchoLink")
        XCTAssertEqual(newest.target, "KX0AAA-R")
        XCTAssertEqual(newest.mode, "Robot 36")
        XCTAssertEqual(newest.talker, "N0CALL")
        XCTAssertTrue(newest.partial)
        XCTAssertNil(items[1].talker)
        XCTAssertFalse(items[1].partial)
    }

    func testNoIndexIsAnEmptyGallery() {
        XCTAssertEqual(SstvGallery.load(folder: folder), [])
    }
}
