// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The network seam (astar-9b3e, extended astar-c2e5/iax-f2b8 Task 8): which
/// networks exist, which are available, and the per-network dial metadata the
/// popover renders from.
final class NetworkTests: XCTestCase {
    func testOnlyAllStarIsAvailableWithoutM17() {
        // The engine has no reflector support yet (iax-b3d7) — hamlink stays
        // hidden regardless. Without the M17 capability flag, only AllStar.
        XCTAssertEqual(Network.available(m17: false), [.allstar])
    }

    func testM17AppendsWhenFlagged() {
        // M17 lights up ONLY behind its own capability flag — hamlink never
        // does (no engine support yet), and AllStar is always first.
        XCTAssertEqual(Network.available(m17: true), [.allstar, .m17])
    }

    func testResolveFallsBackToAllStarWithoutM17() {
        XCTAssertEqual(Network.resolve("allstar", m17: false), .allstar)
        // hamlink exists as a case but is NOT available yet → fallback.
        XCTAssertEqual(Network.resolve("hamlink", m17: false), .allstar)
        XCTAssertEqual(
            Network.resolve("m17", m17: false), .allstar,
            "m17 is a known case but unavailable without the flag")
        XCTAssertEqual(Network.resolve("dmr", m17: false), .allstar, "unknown strings fall back")
        XCTAssertEqual(Network.resolve(nil, m17: false), .allstar)
        XCTAssertEqual(Network.resolve("", m17: false), .allstar)
    }

    func testResolveM17WhenFlagged() {
        XCTAssertEqual(Network.resolve("m17", m17: true), .m17, "flagged → m17 resolves")
        XCTAssertEqual(
            Network.resolve("hamlink", m17: true), .allstar,
            "hamlink stays unavailable regardless of the m17 flag")
        XCTAssertEqual(Network.resolve("allstar", m17: true), .allstar)
    }

    func testDialMetadataPerNetwork() {
        XCTAssertEqual(Network.allstar.displayName, "AllStar")
        XCTAssertEqual(Network.allstar.badge, "ASL")
        XCTAssertEqual(Network.allstar.dialPlaceholder, "Node or IP address")
        XCTAssertTrue(Network.allstar.showsDialpad)
        XCTAssertEqual(Network.hamlink.displayName, "Hamlink")
        XCTAssertEqual(Network.hamlink.badge, "SVX")
        XCTAssertEqual(Network.hamlink.dialPlaceholder, "Reflector host / talkgroup")
        XCTAssertFalse(Network.hamlink.showsDialpad, "DTMF dialpad is an AllStar concern")
        XCTAssertEqual(Network.m17.displayName, "M17")
        XCTAssertEqual(Network.m17.badge, "M17")
        XCTAssertEqual(Network.m17.symbol, "waveform")
        XCTAssertEqual(Network.m17.dialPlaceholder, "Reflector host:port / module")
        XCTAssertFalse(Network.m17.showsDialpad, "DTMF dialpad is an AllStar concern")
    }

    func testAllStarAdmitsExactlyTodaysSmartFieldCharacters() {
        // Verbatim the MenuPopover filter (astar-427f): ASCII letters,
        // numbers, and ".:-*#" — nothing else. This must NOT change behavior.
        for c: Character in ["a", "Z", "5", ".", ":", "-", "*", "#"] {
            XCTAssertTrue(Network.allstar.admitsDialCharacter(c), "\(c) must be admitted")
        }
        // `é` is dropped — the pre-9b3e MenuPopover filter gates the whole
        // clause on `isASCII`, so accented/non-ASCII letters never reached
        // the node field even though `Character.isLetter` is true for them.
        // Keeping that behavior verbatim, not "fixing" it.
        for c: Character in [" ", "+", "/", "\n", "é"] {
            XCTAssertFalse(Network.allstar.admitsDialCharacter(c), "\(c) must be dropped")
        }
    }

    func testHamlinkAdmitsReflectorAddressCharacters() {
        // Host[:port] plus talkgroup-ish tokens: letters, numbers, ".:-/#".
        for c: Character in ["a", "5", ".", ":", "-", "/", "#"] {
            XCTAssertTrue(Network.hamlink.admitsDialCharacter(c), "\(c) must be admitted")
        }
        for c: Character in [" ", "*", "é"] {
            XCTAssertFalse(Network.hamlink.admitsDialCharacter(c), "\(c) must be dropped")
        }
    }

    func testM17AdmitsHostPortModuleCharactersIncludingSpace() {
        // `host[:port]/module` or `host[:port] module` (M17Dial) — unlike
        // every other network, the space IS part of the grammar.
        for c: Character in ["a", "5", ".", ":", "-", "/", " "] {
            XCTAssertTrue(Network.m17.admitsDialCharacter(c), "\(c) must be admitted")
        }
        for c: Character in ["*", "#", "é"] {
            XCTAssertFalse(Network.m17.admitsDialCharacter(c), "\(c) must be dropped")
        }
    }
}
