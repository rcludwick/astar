// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// What the menu-bar status icon should convey about the live call, derived from
/// the local + remote keying state. Platform-neutral so it can be unit-tested;
/// the macOS status item maps it to a tint (idle = default, transmitting = pale
/// red, receiving = green).
public enum StatusIconState: Equatable {
    case idle
    case connected
    case transmitting
    case receiving

    /// Priority: transmit > receive > connected > idle. If we're keyed, that's
    /// what *we're* doing, regardless of incoming audio. `receiving` is computed
    /// by the caller from the far end keying it (`remotePTT`, when the node sends
    /// RADIO_KEY) *or* incoming audio activity — many AllStar nodes never send
    /// RADIO_KEY, so the rx audio level is the reliable signal. `connected` is the
    /// resting "on a call" state (blue) when neither side is currently talking.
    public init(transmitting: Bool, receiving: Bool, connected: Bool = false) {
        if transmitting {
            self = .transmitting
        } else if receiving {
            self = .receiving
        } else if connected {
            self = .connected
        } else {
            self = .idle
        }
    }
}
