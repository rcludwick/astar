// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The outbound links astar shows in its Help menu and About panel (astar-1f7d).
///
/// Constants rather than string literals scattered through the menu builder: a
/// mistyped callsign or a broken scheme would ship as a dead menu item that
/// nothing else in the build would catch. Platform-neutral so the Iced client
/// can present the same two links.
public enum AboutLinks {
    /// The author's amateur-radio callsign.
    public static let callsign = "AJ7HR"

    /// astar's documentation site — the same page the DMG download button is on.
    public static let homePage = URL(string: "https://rcludwick.github.io/astar")!

    /// The author's QRZ profile. QRZ serves callsign lookups from `/db/<call>`.
    public static let qrz = URL(string: "https://www.qrz.com/db/\(callsign)")!

    /// Where to file a bug. Michael's report arrived by email because nothing in
    /// the app said where else to put it.
    public static let issues = URL(string: "https://github.com/rcludwick/astar/issues")!
}
