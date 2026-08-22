// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Turns a raw CoreAudio device-name list into the list a picker may honestly
/// offer (astar-9d41).
///
/// Two different gadgets can report the same name: an ICOM IC-7300 and an
/// AllScan UCI150 both enumerate as "USB Audio Device". That matters because a
/// device's name IS its identity in astar — `DeviceId` is `"in:<name>"`
/// (`crates/astar-audio/src/device.rs`) and `find_device` returns the FIRST
/// device whose name matches (`crates/astar-audio/src/stream.rs`). So of two
/// same-named devices exactly one is reachable, and no selection the user makes
/// can reach the other.
///
/// A picker listing both therefore offers a choice that does not exist. Worse,
/// SwiftUI needs unique `ForEach` ids and unique `.tag`s: two rows carrying the
/// same name are one identity to SwiftUI, so both render as selected — the
/// symptom that surfaced this.
///
/// The honest response is to show what is actually selectable — one entry per
/// distinct name — and to say plainly that a collision happened, because
/// renaming a device in **Audio MIDI Setup** is a real fix the user can apply
/// today: the rename is stored by CoreAudio against the device's UID, survives
/// replug, and immediately makes both devices addressable by name.
///
/// Collapsing here is a stopgap for the name-as-identity model, not a defence
/// of it. Stable per-device identity (`kAudioDevicePropertyDeviceUID`) is the
/// real repair; it needs an engine change, because cpal 0.15 keeps its
/// `audio_device_id` `pub(crate)` with no public accessor.
public enum AudioDeviceList {

    /// The device names a picker may offer: one entry per distinct name, in
    /// enumeration order.
    ///
    /// The survivor of a collision is the FIRST occurrence, matching the device
    /// `find_device` resolves to. Comparison is exact — CoreAudio names are
    /// case-sensitive and the engine compares them byte for byte, so names
    /// differing only in case are separately addressable and both survive.
    public static func selectable(from names: [String]) -> [String] {
        var seen = Set<String>()
        return names.filter { seen.insert($0).inserted }
    }

    /// Names that appeared more than once, each reported once, in order of
    /// first appearance. Empty when every name is distinct.
    public static func duplicated(in names: [String]) -> [String] {
        var counts = [String: Int]()
        for name in names { counts[name, default: 0] += 1 }
        var seen = Set<String>()
        return names.filter { counts[$0, default: 0] > 1 && seen.insert($0).inserted }
    }

    /// User-facing explanation of a name collision, or `nil` when there is
    /// none. Names the offenders and points at the fix that works today.
    public static func collisionWarning(for names: [String]) -> String? {
        let dupes = duplicated(in: names)
        guard !dupes.isEmpty else { return nil }
        let quoted = dupes.map { "\u{201C}\($0)\u{201D}" }
        let subject: String
        switch quoted.count {
        case 1: subject = "More than one device is called \(quoted[0])"
        case 2: subject = "Devices share the names \(quoted[0]) and \(quoted[1])"
        default:
            let head = quoted.dropLast().joined(separator: ", ")
            subject = "Devices share the names \(head), and \(quoted[quoted.count - 1])"
        }
        return
            "\(subject). astar tells devices apart by name, so it can only use the first one. "
            + "Rename one in Audio MIDI Setup to use either."
    }

    /// Collision warning across both device lists, or `nil` when neither has
    /// one.
    ///
    /// Duplicates are counted WITHIN each direction and then unioned — never by
    /// concatenating the two lists. A combined gadget legitimately appears once
    /// in each (that is the pairing `AudioDevicePairing` depends on), so a
    /// concatenated count would report a collision for every headset attached.
    ///
    /// A duplex pair like the IC-7300 and the UCI150 collides in inputs *and*
    /// outputs; that is one problem, so the name is reported once.
    public static func collisionWarning(inputs: [String], outputs: [String]) -> String? {
        let clashing = duplicated(in: inputs) + duplicated(in: outputs)
        var seen = Set<String>()
        let unique = clashing.filter { seen.insert($0).inserted }
        guard !unique.isEmpty else { return nil }
        // Re-use the single-list phrasing by handing it one occurrence pair per
        // clashing name, so the wording stays in exactly one place.
        return collisionWarning(for: unique.flatMap { [$0, $0] })
    }

    /// Collision warning limited to the devices actually in use.
    ///
    /// The main page uses this rather than the unconditional form: a clash
    /// among devices you have not selected does not affect the rig you are
    /// running, and a permanent warning about someone else's hardware is noise
    /// that teaches people to ignore the banner. Settings keeps the
    /// unconditional form — there you are choosing devices, so a clash you have
    /// not selected yet is exactly what you need to know.
    ///
    /// A `nil` selection is the system default: astar did not choose it and
    /// cannot name it, so there is nothing honest to warn about.
    ///
    /// Direction matters. A name duplicated among outputs but unique among
    /// inputs is perfectly addressable as an input, so selecting it there does
    /// not warn.
    public static func collisionWarning(
        inputs: [String], outputs: [String],
        selectedInput: String?, selectedOutput: String?
    ) -> String? {
        var affected = [String]()
        if let selectedInput, duplicated(in: inputs).contains(selectedInput) {
            affected.append(selectedInput)
        }
        if let selectedOutput, duplicated(in: outputs).contains(selectedOutput),
            !affected.contains(selectedOutput)
        {
            affected.append(selectedOutput)
        }
        guard !affected.isEmpty else { return nil }
        return collisionWarning(for: affected.flatMap { [$0, $0] })
    }

    /// Whether a stored selection still exists in the current device list.
    /// `nil` — the system default — is always present.
    public static func isPresent(_ selection: String?, in names: [String]) -> Bool {
        guard let selection else { return true }
        return names.contains(selection)
    }
}
