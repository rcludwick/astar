// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The sentences Settings shows about the directory.
///
/// Pinned here because two of them are load-bearing rather than cosmetic: the
/// bundled-and-never-synced line has to read as a state and not a failure, and
/// the "checks again" line is the only place the published weekly cadence is
/// visible to the person who might otherwise think the directory is stuck.
final class ReflectorDirectoryStatusTests: XCTestCase {

    private let now = Date(timeIntervalSince1970: 1_780_000_000)

    // MARK: - Counts

    func testCountsLineOrdersKnownNetworksAndGroupsThousands() {
        let line = ReflectorDirectoryStatus.countsLine([.ysf: 1432, .dstar: 944, .m17: 106])
        XCTAssertEqual(line, "944 D-Star · 106 M17 · 1,432 YSF")
    }

    /// A network hamcall-db adds after this build shipped is searchable in
    /// this very app. Folding it into an "other" bucket would make the
    /// directory look smaller than it is.
    func testUnknownNetworksAreNamedAfterTheKnownOnes() {
        let line = ReflectorDirectoryStatus.countsLine([
            .dstar: 2, .other("dmr"): 7, .other("aprs"): 1,
        ])
        XCTAssertEqual(line, "2 D-Star · 1 APRS · 7 DMR")
    }

    func testEmptyDirectoryProducesAnEmptyLine() {
        XCTAssertEqual(ReflectorDirectoryStatus.countsLine([:]), "")
        XCTAssertEqual(ReflectorDirectoryStatus.countsLine([.dstar: 0]), "")
    }

    // MARK: - Freshness

    /// An install that has never reached the network still has a complete
    /// directory. "Never synced" alone reads as breakage; naming what it does
    /// have is the difference between reassurance and alarm.
    func testBundledAndNeverSyncedSaysSoWithoutSoundingBroken() {
        XCTAssertEqual(
            ReflectorDirectoryStatus.freshness(lastFetched: nil, isBundled: true, now: now),
            "Bundled with astar — never synced")
        XCTAssertEqual(
            ReflectorDirectoryStatus.freshness(lastFetched: nil, isBundled: false, now: now),
            "Never synced")
    }

    func testFreshnessCountsUpFromSecondsToDays() {
        func line(_ ago: TimeInterval) -> String {
            ReflectorDirectoryStatus.freshness(
                lastFetched: now.addingTimeInterval(-ago), isBundled: false, now: now)
        }
        XCTAssertEqual(line(5), "Last synced just now")
        XCTAssertEqual(line(60), "Last synced 1 minute ago")
        XCTAssertEqual(line(3_600), "Last synced 1 hour ago")
        XCTAssertEqual(line(3 * 86_400), "Last synced 3 days ago")
    }

    /// A restored backup or an edited clock can put the last sync in the
    /// future. "in -3 days" is nonsense the data does not deserve — the copy
    /// on disk is still current.
    func testAClockThatWentBackwardsDoesNotRenderNegativeTime() {
        XCTAssertEqual(
            ReflectorDirectoryStatus.freshness(
                lastFetched: now.addingTimeInterval(9_000), isBundled: false, now: now),
            "Last synced just now")
    }

    // MARK: - The cadence, made visible

    /// astar does not choose this number — `client_refresh_days` does. Showing
    /// it is how an operator can tell that a directory sitting still for a
    /// week is policy rather than breakage.
    func testAutomaticLineNamesWhenTheWeeklyCheckComesRound() {
        XCTAssertEqual(
            ReflectorDirectoryStatus.automaticLine(
                nextDue: now.addingTimeInterval(4 * 86_400), now: now),
            "Checks again in 4 days")
        XCTAssertEqual(
            ReflectorDirectoryStatus.automaticLine(nextDue: nil, now: now),
            "Checks automatically at the next launch")
        XCTAssertEqual(
            ReflectorDirectoryStatus.automaticLine(
                nextDue: now.addingTimeInterval(-1), now: now),
            "Checks automatically at the next launch")
    }

    // MARK: - What a press just did

    @MainActor
    func testSyncOutcomeMessages() {
        XCTAssertEqual(
            ReflectorDirectory.SyncOutcome.updated(count: 944, generated: "2026-08-27")
                .message(now: now),
            "Updated — 944 reflectors · published 2026-08-27")
        XCTAssertEqual(
            ReflectorDirectory.SyncOutcome.notModified.message(now: now), "Already up to date")
        XCTAssertEqual(
            ReflectorDirectory.SyncOutcome.skipped(.notDue(nextDue: now)).message(now: now),
            "Already up to date")
        XCTAssertEqual(
            ReflectorDirectory.SyncOutcome
                .skipped(.debounced(retryAfter: now.addingTimeInterval(1_800)))
                .message(now: now),
            "Checked recently — try again in 30 minutes")
    }
}
