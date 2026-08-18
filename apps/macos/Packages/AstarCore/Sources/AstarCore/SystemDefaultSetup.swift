// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The built-in **System Default** config (astar-1f7d): system input/output, no
/// serial PTT. It is always present, can't be deleted, and is what a fresh
/// install lands on.
///
/// Platform-neutral and free of any controller/session dependency on purpose —
/// the same split as `DockPresence`/`DockPolicy`. `SetupController` (macOS app
/// layer) owns the applying; the identity and the launch-default rule live here
/// where they can be tested.
///
/// Before this existed, a fresh install seeded a config named after
/// `HardwareProfileRegistry.uci150ID` — so someone who had never owned a UCI150
/// opened Settings to a config named after hardware they didn't have, on a
/// serial hardware profile. Nothing named the plain system audio path.
public enum SystemDefaultSetup {
    /// Stable id. Deliberately the historical `"__none__"` from when this entry
    /// was called "None (system default)": existing installs already have that
    /// string recorded as their selection, and minting a new id would orphan it.
    public static let id = "__none__"

    /// User-facing name. Matches the "System Default" label the device pickers
    /// already use for "whatever macOS is using" — the same idea, one config up.
    public static let name = "System Default"

    /// The config itself: no named devices (so it applies on any machine) and a
    /// non-serial hardware profile (so applying it can never assert RTS on a
    /// serial line, which is a transmit key on a USB radio interface).
    public static let setup = Setup(
        id: id,
        name: name,
        hardwareProfileID: HardwareProfileRegistry.headsetID,
        inputDevice: nil,
        outputDevice: nil
    )

    /// Which config to apply at launch, or `nil` to leave the persisted audio
    /// state alone.
    ///
    /// - Parameters:
    ///   - storedDefault: the user's ★ pick, if they set one.
    ///   - savedConfigIDs: ids in the store — the built-in is not among them.
    ///
    /// The `nil` return is the load-bearing case. An existing user who has saved
    /// configs but never set a ★ must be left untouched: applying System Default
    /// for them would reset their devices to the system pair and disable their
    /// serial PTT on every single launch. Only a genuinely empty store — a fresh
    /// install, with nothing to preserve — falls through to System Default.
    public static func launchApplyID(storedDefault: String?, savedConfigIDs: [String]) -> String? {
        if let storedDefault, storedDefault == id || savedConfigIDs.contains(storedDefault) {
            return storedDefault
        }
        return savedConfigIDs.isEmpty ? id : nil
    }
}
