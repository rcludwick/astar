// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// The words on the packet capture button, kept here so a test can pin them.
public enum PacketCaptureCopy {
    /// Shown beside the button while a capture runs.
    public static let title = "Capturing"
    public static let accessibilityLabel = "Packet capture"

    public static func accessibilityValue(capturing: Bool) -> String {
        capturing ? "on" : "off"
    }

    public static func help(capturing: Bool) -> String {
        capturing ? "Stop capturing packets (⇧⌘R)" : "Capture packets (⇧⌘R)"
    }
}
