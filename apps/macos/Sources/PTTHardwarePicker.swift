// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import SwiftUI

    /// Which PTT device a config uses: None, the AllScan UCI150, Custom
    /// serial, or a Bluetooth button. One PTT device per config, so serial
    /// and the B02PTT never hold the key together.
    ///
    /// Shared by Quick settings (the active config, saved immediately) and
    /// each Settings config card. The caller supplies the "PTT" label so the
    /// row lines up with its neighbours, and decides what a choice does
    /// through `onChoose`. Picks on a non-editable row (System Default, or no
    /// config at all) never reach `onChoose`. The System Default caption is
    /// the caller's: each call site renders
    /// `PTTHardwarePickerModel.caption(for:)` under the row and sets it as
    /// this picker's accessibility hint.
    struct PTTHardwarePicker: View {
        let selection: PTTHardwareChoice
        let isEditable: Bool
        let onChoose: (PTTHardwareChoice) -> Void

        var body: some View {
            Picker(PTTHardwarePickerModel.rowLabel, selection: binding) {
                ForEach(PTTHardwarePickerModel.options(selection: selection)) { choice in
                    Text(choice.title).tag(choice)
                }
            }
            .labelsHidden()
            .fixedSize()
            .disabled(!isEditable)
            .help(selection == PTTHardwareChoice.none ? PTTHardwarePickerModel.noneHelp : "")
            .accessibilityLabel(PTTHardwarePickerModel.rowLabel)
        }

        private var binding: Binding<PTTHardwareChoice> {
            Binding(
                get: { selection },
                set: { picked in
                    if let choice = PTTHardwarePickerModel.choiceToApply(
                        picked, current: selection, isEditable: isEditable)
                    {
                        onChoose(choice)
                    }
                })
        }
    }
#endif
