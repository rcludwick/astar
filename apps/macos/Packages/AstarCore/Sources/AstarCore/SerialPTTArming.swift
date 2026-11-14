// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Release-to-arm for the serial PTT line after the device is opened.
///
/// A serial re-open (auto-retry after a USB glitch, a config switch, launch)
/// must never key by itself. The driver releases the `.serial` source when it
/// tears the old client down, and that release also clears any latch a forced
/// stop left on it, so a line still held (or stuck) at the moment of the open
/// would otherwise key the radio again as soon as the new client reports it.
///
/// The client's debounced worker reports a line already held at open as a
/// key-down edge shortly after the open (once its debounce has run), and
/// reports nothing at all for an idle line. So this gate holds a settle
/// window from the open: a key-down that arrives before a quiet tick has been
/// seen past the window is treated as held-at-open and dropped, along with
/// every later key-down, until the line is reported released. A release
/// always passes (it is harmless) and arms the gate; once armed, every edge
/// passes through unchanged. Pure, so the timing is testable without a device.
public struct SerialPTTArming: Equatable {
    /// Slack past the debounce for the worker to start and report a held line.
    public static let settleMargin: TimeInterval = 2

    private enum Phase: Equatable {
        case settling
        case heldAtOpen
        case armed
    }

    /// When the settle window ends (same clock as `now` in `filter`).
    public let settlesAt: TimeInterval
    private var phase = Phase.settling

    /// `openedAt` is a monotonic time (for example `ProcessInfo.systemUptime`)
    /// taken when the client was created; `debounceMs` is its CTS debounce.
    public init(openedAt: TimeInterval, debounceMs: UInt32) {
        settlesAt = openedAt + Double(debounceMs) / 1000 + Self.settleMargin
    }

    /// True once a key-down from the line may key the station.
    public var isArmed: Bool { phase == .armed }

    /// Filter one tick's edge (`nil` = no change). Returns the edge to forward
    /// to the session's `.serial` source, or `nil` to leave PTT alone.
    public mutating func filter(edge: Bool?, now: TimeInterval) -> Bool? {
        switch (phase, edge) {
        case (.armed, _):
            return edge
        case (_, false?):
            phase = .armed
            return false
        case (_, true?):
            phase = .heldAtOpen
            return nil
        case (.settling, nil):
            if now >= settlesAt { phase = .armed }
            return nil
        case (.heldAtOpen, nil):
            return nil
        }
    }
}
