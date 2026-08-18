// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The Help-menu / About-panel links (astar-1f7d). A dead menu item is invisible
/// to every other gate in the build, so the shape of each URL is pinned here.
final class AboutLinksTests: XCTestCase {

    /// Every link must be https — these open in the user's browser.
    func testAllLinksAreHTTPS() {
        for url in [AboutLinks.homePage, AboutLinks.qrz, AboutLinks.issues] {
            XCTAssertEqual(url.scheme, "https", "\(url) must be https")
            XCTAssertNotNil(url.host, "\(url) must have a host")
        }
    }

    func testHomePageIsTheDocsSite() {
        XCTAssertEqual(AboutLinks.homePage.absoluteString, "https://rcludwick.github.io/astar")
    }

    /// The callsign is the part a typo would silently break — a wrong call sends
    /// users to someone else's QRZ page, which still resolves.
    func testQRZPointsAtTheCallsign() {
        XCTAssertEqual(AboutLinks.callsign, "AJ7HR")
        XCTAssertEqual(AboutLinks.qrz.absoluteString, "https://www.qrz.com/db/AJ7HR")
        XCTAssertTrue(AboutLinks.qrz.path.hasSuffix(AboutLinks.callsign))
    }

    /// Issues belong on the public repo, not the private one.
    func testIssuesPointAtThePublicRepo() {
        XCTAssertEqual(
            AboutLinks.issues.absoluteString, "https://github.com/rcludwick/astar/issues")
        XCTAssertFalse(
            AboutLinks.issues.absoluteString.contains("astar-private"),
            "the public repo is where users can actually file")
    }
}
