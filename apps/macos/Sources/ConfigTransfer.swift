// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import Foundation
    import UniformTypeIdentifiers

    /// The AppKit half of config export/import (astar-b52e): two panels.
    ///
    /// Everything that decides what is written or merged lives in AstarCore's
    /// `ConfigTransfer`, where the tests can reach it. This file is deliberately
    /// only the file picker.
    enum ConfigTransferPanels {
        static var contentTypes: [UTType] {
            [UTType(filenameExtension: ConfigTransfer.fileExtension) ?? .json, .json]
        }

        static func runSavePanel(defaultName: String) -> URL? {
            let panel = NSSavePanel()
            panel.allowedContentTypes = contentTypes
            panel.nameFieldStringValue = defaultName
            panel.canCreateDirectories = true
            panel.title = "Export astar configuration"
            return panel.runModal() == .OK ? panel.url : nil
        }

        static func runOpenPanel() -> URL? {
            let panel = NSOpenPanel()
            panel.allowedContentTypes = contentTypes
            panel.allowsMultipleSelection = false
            panel.canChooseDirectories = false
            panel.title = "Import astar configuration"
            return panel.runModal() == .OK ? panel.url : nil
        }
    }
#endif
