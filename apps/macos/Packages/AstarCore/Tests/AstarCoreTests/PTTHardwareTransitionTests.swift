// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import XCTest

@testable import AstarCore

/// `PTTHardwareTransition` is the whole rule that keeps one PTT device live
/// per config, and the whole rule for what a rigs import or a deletion may do
/// to live hardware. `SetupController` only runs what it returns, in order,
/// and the app target has no test bundle, so every case is pinned here with
/// exact equality.
final class PTTHardwareTransitionTests: XCTestCase {
    private let uci = SerialLineSpec.uci150
    private let custom = SerialLineSpec(
        keyLineRaw: 1, keyActiveHigh: false, radioLineRaw: 1, radioActiveHigh: false,
        debounceMs: 50, rxModeRaw: 0, rxFloorDb: -40, rxHangMs: 300)
    private let button = BluetoothPTTSpec(deviceID: "inrico-b02ptt", preferredName: "B02PTT-6522")
    private let otherButton = BluetoothPTTSpec(deviceID: "inrico-b02ptt")
    private let causes: [PTTHardwareReconcileCause] = [.configsImported, .selectedConfigDeleted]

    private func reconcile(
        _ applied: PTTHardwarePlan?, _ target: PTTHardwarePlan,
        _ cause: PTTHardwareReconcileCause
    ) -> PTTHardwareReconcile {
        PTTHardwareTransition.reconcile(applied: applied, target: target, cause: cause)
    }

    // MARK: - actions(for:), an explicit apply

    /// Serial shuts before the button attaches, so both are never live at once.
    func testBluetoothPlanShutsSerialBeforeAttaching() {
        XCTAssertEqual(
            PTTHardwareTransition.actions(for: .bluetooth(button)),
            [.closeSerial, .attachBluetooth(button)])
    }

    /// The button detaches, which releases it if keyed, before serial opens.
    func testSerialPlanDetachesTheButtonBeforeOpening() {
        XCTAssertEqual(
            PTTHardwareTransition.actions(for: .serial(uci)),
            [.detachBluetooth, .openSerial(uci)])
    }

    func testNoHardwarePlanShutsBoth() {
        XCTAssertEqual(
            PTTHardwareTransition.actions(for: .noHardware),
            [.detachBluetooth, .closeSerial])
    }

    // MARK: - launchActions(selected:), a launch with no ★ default

    /// A relaunch with no ★ default applies nothing, but a selected config
    /// that names the button must still get its button, or no PTT hardware
    /// works after the relaunch.
    func testLaunchWithoutAStarAttachesASelectedButtonConfig() {
        XCTAssertEqual(
            PTTHardwareTransition.launchActions(selected: .bluetooth(button)),
            [.closeSerial, .attachBluetooth(button)])
    }

    /// Serial reopens itself from its own saved state, and nothing else is
    /// this code's to touch at launch: no other plan produces a step.
    func testLaunchWithoutAStarLeavesSerialAndNoHardwareAlone() {
        XCTAssertEqual(PTTHardwareTransition.launchActions(selected: .serial(uci)), [])
        XCTAssertEqual(PTTHardwareTransition.launchActions(selected: .noHardware), [])
        XCTAssertEqual(PTTHardwareTransition.launchActions(selected: nil), [])
    }

    // MARK: - needsReapply

    /// An existing user with no launch default is left alone at launch, so
    /// nothing before the first apply may touch their hardware.
    func testNeedsReapplyIsFalseBeforeTheFirstApply() {
        for target in [PTTHardwarePlan.serial(uci), .bluetooth(button), .noHardware] {
            XCTAssertEqual(
                PTTHardwareTransition.needsReapply(applied: nil, target: target), false,
                "target \(target)")
        }
    }

    /// Re-applying an unchanged serial plan reopens the port, which would drop
    /// a key held on it.
    func testNeedsReapplyIsFalseForAnUnchangedPlan() {
        for plan in [PTTHardwarePlan.serial(uci), .bluetooth(button), .noHardware] {
            XCTAssertEqual(
                PTTHardwareTransition.needsReapply(applied: plan, target: plan), false,
                "plan \(plan)")
        }
    }

    func testNeedsReapplyIsTrueForAChange() {
        XCTAssertEqual(
            PTTHardwareTransition.needsReapply(applied: .serial(uci), target: .serial(custom)),
            true)
        XCTAssertEqual(
            PTTHardwareTransition.needsReapply(applied: .noHardware, target: .bluetooth(button)),
            true)
        XCTAssertEqual(
            PTTHardwareTransition.needsReapply(applied: .bluetooth(button), target: .noHardware),
            true)
    }

    // MARK: - reconcile, configs changed behind apply

    func testReconcileBeforeTheFirstApplyDoesNothing() {
        for cause in causes {
            XCTAssertEqual(
                reconcile(nil, .serial(uci), cause), PTTHardwareReconcile(actions: [], live: nil))
            XCTAssertEqual(
                reconcile(nil, .bluetooth(button), cause),
                PTTHardwareReconcile(actions: [], live: nil))
        }
    }

    func testReconcileOfAnUnchangedPlanDoesNothing() {
        XCTAssertEqual(
            reconcile(.serial(uci), .serial(uci), .configsImported),
            PTTHardwareReconcile(actions: [], live: .serial(uci)))
        XCTAssertEqual(
            reconcile(.bluetooth(button), .bluetooth(button), .configsImported),
            PTTHardwareReconcile(actions: [], live: .bluetooth(button)))
    }

    /// An imported config that names a serial radio must not open its port:
    /// opening a USB radio interface's tty asserts RTS, which is the radio's
    /// key line. Serial opens only when the user applies the config.
    func testReconcileNeverOpensSerial() {
        XCTAssertEqual(
            reconcile(.noHardware, .serial(uci), .configsImported),
            PTTHardwareReconcile(actions: [], live: .noHardware))
        XCTAssertEqual(
            reconcile(.bluetooth(button), .serial(uci), .configsImported),
            PTTHardwareReconcile(actions: [.detachBluetooth], live: .noHardware))
        XCTAssertEqual(
            reconcile(.serial(uci), .serial(custom), .configsImported),
            PTTHardwareReconcile(actions: [], live: .serial(uci)))
    }

    /// Every applied, target and cause combination, behind the exact cases.
    func testNoReconcileEverOpensSerial() {
        let plans: [PTTHardwarePlan] = [
            .serial(uci), .serial(custom), .bluetooth(button), .noHardware,
        ]
        let applied: [PTTHardwarePlan?] = [nil] + plans.map { Optional($0) }
        for from in applied {
            for target in plans {
                for cause in causes {
                    let opens = reconcile(from, target, cause).actions.filter {
                        if case .openSerial = $0 { return true }
                        return false
                    }
                    XCTAssertEqual(
                        opens, [],
                        "from \(String(describing: from)) to \(target), \(cause)")
                }
            }
        }
    }

    /// Attaching is safe from a reconcile: the controller creates a Bluetooth
    /// central only for a unit confirmed on this Mac, and never keys on connect.
    func testImportFromSerialToTheButtonShutsSerialThenAttaches() {
        XCTAssertEqual(
            reconcile(.serial(uci), .bluetooth(button), .configsImported),
            PTTHardwareReconcile(
                actions: [.closeSerial, .attachBluetooth(button)], live: .bluetooth(button)))
    }

    func testImportOfAnotherButtonSpecReattaches() {
        XCTAssertEqual(
            reconcile(.bluetooth(button), .bluetooth(otherButton), .configsImported),
            PTTHardwareReconcile(
                actions: [.closeSerial, .attachBluetooth(otherButton)],
                live: .bluetooth(otherButton)))
    }

    func testImportFromSerialToNoHardwareShutsSerial() {
        XCTAssertEqual(
            reconcile(.serial(uci), .noHardware, .configsImported),
            PTTHardwareReconcile(actions: [.closeSerial], live: .noHardware))
    }

    func testImportFromTheButtonToNoHardwareDetaches() {
        XCTAssertEqual(
            reconcile(.bluetooth(button), .noHardware, .configsImported),
            PTTHardwareReconcile(actions: [.detachBluetooth], live: .noHardware))
    }

    /// Deleting the selected config leaves nothing selected. astar has always
    /// left a serial line open then, and that does not change here.
    func testDeletingTheSelectedSerialConfigLeavesSerialAlone() {
        XCTAssertEqual(
            reconcile(.serial(uci), .noHardware, .selectedConfigDeleted),
            PTTHardwareReconcile(actions: [], live: .serial(uci)))
    }

    /// The button, though, must not stay attached with no config naming it.
    func testDeletingTheSelectedButtonConfigDetaches() {
        XCTAssertEqual(
            reconcile(.bluetooth(button), .noHardware, .selectedConfigDeleted),
            PTTHardwareReconcile(actions: [.detachBluetooth], live: .noHardware))
    }
}
