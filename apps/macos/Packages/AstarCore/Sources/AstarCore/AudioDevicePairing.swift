// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// Pairs a capture device with its matching playback device.
///
/// macOS exposes a combined USB audio gadget (Jabra Link 390, AllScan UCI150)
/// as two separate CoreAudio devices — a mic and a speaker — that share the
/// exact same name. When the user selects such an input, the speaker on the same
/// gadget is almost always the intended output, but it otherwise stays on the
/// system default. This finds that same-named output so the UI can auto-pair it.
public enum AudioDevicePairing {
    /// The output device that matches `input` by name, or `nil` if there is no
    /// same-named output (or `input` is the system default).
    public static func matchingOutput(forInput input: String?, in outputs: [String]) -> String? {
        guard let input else { return nil }
        return outputs.first { $0 == input }
    }
}
