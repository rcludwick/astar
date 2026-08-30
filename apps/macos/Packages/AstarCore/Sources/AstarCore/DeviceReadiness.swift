// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Whether the selected audio device can actually be opened, shown beside the
/// picker so a rig that will fail at dial time says so before you dial.
///
/// # Why this can be answered without opening the device
///
/// On macOS, CoreAudio shares a device between clients: if it is present and
/// no process holds it in hog mode, an open succeeds. So presence plus hog
/// owner answers the question exactly, and the alternative — opening a stream
/// every few seconds to find out — would be both slower and worse behaved,
/// since some USB interfaces click audibly when a stream starts or stops.
///
/// The classification is separated from the CoreAudio queries so it can be
/// tested; `AudioDeviceMonitor` supplies the facts.
public enum DeviceReadiness: Equatable, Sendable {
    /// Present, and nothing else is holding it exclusively.
    case ready
    /// No device with this name. Unplugged, renamed, or never existed.
    case notFound
    /// Present, but another process has it in hog mode.
    case busy

    /// Classify from what CoreAudio can be asked without opening anything.
    ///
    /// - `present`: a device with the wanted name (or a system default, when
    ///   none was named) exists in the current device list.
    /// - `hogOwner`: the pid holding hog mode, `-1` for nobody, or `nil` when
    ///   the device does not support the query — aggregate and virtual devices
    ///   commonly refuse it, and "cannot tell" must not read as "busy".
    /// - `ourPID`: astar's own pid. astar hogs nothing today, but a device we
    ///   held ourselves would be available to us, not busy.
    public static func classify(present: Bool, hogOwner: pid_t?, ourPID: pid_t) -> DeviceReadiness {
        guard present else { return .notFound }
        guard let hogOwner, hogOwner != -1, hogOwner != ourPID else { return .ready }
        return .busy
    }

    /// The line shown under the picker, or `nil` when there is nothing wrong.
    ///
    /// Names the device, because a rig has several and "not found" against the
    /// wrong one sends someone hunting.
    public func note(for device: String?) -> String? {
        let name = device ?? "the system default device"
        switch self {
        case .ready: return nil
        case .notFound: return "\(name) isn’t connected. Plug it back in, or pick another device."
        case .busy: return "\(name) is in use by another app. Quit it, or pick another device."
        }
    }
}
