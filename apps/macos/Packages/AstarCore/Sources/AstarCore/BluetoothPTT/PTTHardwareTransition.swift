// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// One step of switching the live PTT hardware. `SetupController` maps each
/// step onto the serial controller or the Bluetooth PTT controller and runs
/// them in the order given, so the order is decided, and tested, here.
public enum PTTHardwareAction: Equatable, Sendable {
    /// Shut the serial line to the USB radio interface.
    case closeSerial
    /// Open the serial line with this spec. Opening a USB radio interface's
    /// tty asserts RTS, which is its key line, so only an explicit apply
    /// produces this.
    case openSerial(SerialLineSpec)
    /// Attach the Bluetooth PTT controller for this spec. The controller
    /// creates a Bluetooth central only for a unit confirmed on this Mac, and
    /// never keys on connect.
    case attachBluetooth(BluetoothPTTSpec)
    /// Detach the Bluetooth PTT controller. It feeds Disable, which releases
    /// the button if it was keyed.
    case detachBluetooth
}

/// Why the configs changed without going through `SetupController.apply`.
public enum PTTHardwareReconcileCause: Equatable, Sendable {
    /// A rigs import rewrote the configs and the selection.
    case configsImported
    /// The selected config was deleted, so nothing is selected.
    case selectedConfigDeleted
}

/// What a reconcile does: the steps to run, and the plan that describes the
/// live hardware afterwards. `live` is not always the target, because a
/// reconcile never opens serial. A target that names serial leaves serial as
/// it was until the user applies that config.
public struct PTTHardwareReconcile: Equatable, Sendable {
    /// The steps, in the order they must run.
    public let actions: [PTTHardwareAction]
    /// The live hardware afterwards, or nil while nothing has been applied.
    public let live: PTTHardwarePlan?

    /// Public so callers and tests can build and compare whole values.
    public init(actions: [PTTHardwareAction], live: PTTHardwarePlan?) {
        self.actions = actions
        self.live = live
    }
}

/// How the live PTT hardware moves from one plan to another.
///
/// Each config has one PTT device: the USB radio interface (serial), the
/// Bluetooth PTT button, or neither, never both. This keeps it so by shutting
/// the device being switched away from before the other one opens, so there
/// is no instant with both live. It is pure and platform-neutral because the
/// app target has no test bundle: the app only executes what this returns.
public enum PTTHardwareTransition {
    /// The steps an explicit apply runs for `plan`, in order.
    public static func actions(for plan: PTTHardwarePlan) -> [PTTHardwareAction] {
        switch plan {
        case .bluetooth(let spec):
            return [.closeSerial, .attachBluetooth(spec)]
        case .serial(let spec):
            return [.detachBluetooth, .openSerial(spec)]
        case .noHardware:
            return [.detachBluetooth, .closeSerial]
        }
    }

    /// The steps to run at launch when no config is applied: an existing user
    /// with no launch default (★) is left alone, and `SerialController`
    /// reopens serial from its own saved flag. The button has no such path,
    /// so a selected config that names it attaches it here, after shutting
    /// serial. Anything else is left exactly as it is. This never opens
    /// serial.
    public static func launchActions(selected: PTTHardwarePlan?) -> [PTTHardwareAction] {
        guard case .bluetooth(let spec) = selected else { return [] }
        return [.closeSerial, .attachBluetooth(spec)]
    }

    /// Whether configs that changed behind `apply` should touch live hardware.
    ///
    /// Never before the first apply: an existing user with no launch default
    /// is left alone at launch, so their serial state is not this code's to
    /// change. Never for an unchanged plan: re-applying an unchanged serial
    /// plan reopens the port, which drops a key held on it.
    public static func needsReapply(
        applied: PTTHardwarePlan?, target: PTTHardwarePlan
    ) -> Bool {
        guard let applied else { return false }
        return applied != target
    }

    /// The steps for configs that changed behind `apply`: a rigs import, or
    /// deleting the selected config.
    ///
    /// It only moves toward quiet. It detaches the button or shuts serial when
    /// the target no longer names the live device, and it may attach a button
    /// the target names. It never opens serial: an imported config that names
    /// a USB radio interface waits until the user applies it, because opening
    /// that tty asserts RTS, the key line. Deleting the selected config leaves
    /// a serial line as it was, as astar always has; only the button detaches.
    public static func reconcile(
        applied: PTTHardwarePlan?, target: PTTHardwarePlan, cause: PTTHardwareReconcileCause
    ) -> PTTHardwareReconcile {
        guard let applied, needsReapply(applied: applied, target: target) else {
            return PTTHardwareReconcile(actions: [], live: applied)
        }
        switch (applied, target) {
        case (_, .bluetooth):
            return PTTHardwareReconcile(actions: actions(for: target), live: target)
        case (.bluetooth, _):
            // Serial is already shut: applying the button's plan shut it.
            return PTTHardwareReconcile(actions: [.detachBluetooth], live: .noHardware)
        case (.serial, .noHardware):
            switch cause {
            case .configsImported:
                return PTTHardwareReconcile(actions: [.closeSerial], live: .noHardware)
            case .selectedConfigDeleted:
                return PTTHardwareReconcile(actions: [], live: applied)
            }
        default:
            // Serial to another serial spec, or nothing to serial. Opening
            // serial is left to an explicit apply.
            return PTTHardwareReconcile(actions: [], live: applied)
        }
    }
}
