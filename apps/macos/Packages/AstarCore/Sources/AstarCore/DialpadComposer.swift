// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// Pure compose-then-send dialpad rules (astar-7d21): what the connected-mode
/// command field may contain, when Send is allowed, and how a playing command
/// splits for the progress render. UI state (SwiftUI bindings) stays in the
/// view; this owns the rules so they're testable in AstarCore.
public enum DialpadComposer {
    /// The 16 DTMF keys — the only characters a command may contain.
    public static let validKeys = Set("0123456789*#ABCD")

    /// Uppercase and strip anything that isn't a DTMF key. Paste-safe: spaces,
    /// dashes, and stray characters vanish; lowercase a-d become the A-D keys.
    public static func filtered(_ raw: String) -> String {
        String(raw.uppercased().filter { validKeys.contains($0) })
    }

    /// Send is allowed exactly when there is a command, the call is answered
    /// (no on-air tones before answer), and nothing is already playing (one
    /// sequence at a time — the engine would reject the overlap anyway).
    public static func canSend(command: String, answered: Bool, playing: Bool) -> Bool {
        !command.isEmpty && answered && !playing
    }

    /// Split a playing command into the (played, pending) halves for the
    /// progress render — played digits dim, pending stay primary. `played`
    /// clamps to the command length (snapshot races are benign).
    public static func progressSplit(command: String, played: Int) -> (
        played: String, pending: String
    ) {
        let n = max(0, min(played, command.count))
        let cut = command.index(command.startIndex, offsetBy: n)
        return (String(command[..<cut]), String(command[cut...]))
    }
}
