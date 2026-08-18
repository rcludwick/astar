// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// The built-in **System Default** config (astar-1f7d): the always-present entry
/// that uses the system's own input/output and no serial PTT, and the config a
/// fresh install lands on.
final class SystemDefaultSetupTests: XCTestCase {

    // MARK: - Identity

    /// The id must stay the historical `__none__`: installs from before the
    /// rename already have it recorded as their selection, and changing the id
    /// would orphan that selection.
    func testIDIsStableAcrossTheRename() {
        XCTAssertEqual(SystemDefaultSetup.id, "__none__")
    }

    func testNameIsSystemDefault() {
        XCTAssertEqual(SystemDefaultSetup.name, "System Default")
        XCTAssertEqual(SystemDefaultSetup.setup.name, "System Default")
    }

    /// System default devices (nil = whatever macOS is using) and a non-serial
    /// hardware profile, so applying it can never assert RTS on a serial line.
    func testUsesSystemDevicesAndNoSerial() {
        let s = SystemDefaultSetup.setup
        XCTAssertNil(s.inputDevice)
        XCTAssertNil(s.outputDevice)
        XCTAssertEqual(s.hardwareProfileID, HardwareProfileRegistry.headsetID)
        XCTAssertFalse(
            HardwareProfileRegistry().resolve(id: s.hardwareProfileID).usesSerial,
            "System Default must not enable serial PTT")
    }

    /// It names no devices, so it is applicable on any machine — it can never be
    /// refused for a missing device.
    func testIsNeverMissingDevices() {
        XCTAssertTrue(SystemDefaultSetup.setup.missingDevices(inputs: [], outputs: []).isEmpty)
    }

    // MARK: - What to apply at launch

    /// Fresh install: no saved configs at all, so land on System Default rather
    /// than fabricating a config named after hardware the user may not own.
    func testFreshInstallAppliesSystemDefault() {
        XCTAssertEqual(
            SystemDefaultSetup.launchApplyID(storedDefault: nil, savedConfigIDs: []),
            SystemDefaultSetup.id)
    }

    /// The user's ★ pick wins whenever it still exists.
    func testStoredDefaultWins() {
        XCTAssertEqual(
            SystemDefaultSetup.launchApplyID(storedDefault: "abc", savedConfigIDs: ["abc", "def"]),
            "abc")
    }

    /// ★ set to System Default is honored even though it is not in the store —
    /// the built-in lives outside the saved-config list.
    func testStoredDefaultMayBeSystemDefaultItself() {
        XCTAssertEqual(
            SystemDefaultSetup.launchApplyID(
                storedDefault: SystemDefaultSetup.id, savedConfigIDs: ["abc"]),
            SystemDefaultSetup.id)
    }

    /// The load-bearing case: an existing user with saved configs who never set a
    /// ★ must be left alone. Applying System Default here would reset their
    /// devices and disable their serial PTT on every launch.
    func testExistingUserWithoutADefaultIsNotClobbered() {
        XCTAssertNil(
            SystemDefaultSetup.launchApplyID(storedDefault: nil, savedConfigIDs: ["abc"]))
    }

    /// A ★ pointing at a deleted config falls back to leaving things alone, not
    /// to silently applying System Default over the user's devices.
    func testStaleDefaultIsIgnoredWhenOtherConfigsExist() {
        XCTAssertNil(
            SystemDefaultSetup.launchApplyID(storedDefault: "gone", savedConfigIDs: ["abc"]))
    }

    /// A ★ pointing at a deleted config on an otherwise empty store still lands
    /// on System Default — there is nothing else to preserve.
    func testStaleDefaultOnEmptyStoreFallsBackToSystemDefault() {
        XCTAssertEqual(
            SystemDefaultSetup.launchApplyID(storedDefault: "gone", savedConfigIDs: []),
            SystemDefaultSetup.id)
    }
}
