// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import SwiftUI

    /// Shared geometry for the labelled rows in Settings (astar-f4a2).
    ///
    /// The Operator and Account sections sit directly on top of each other, so
    /// their label columns have to be the same width or the pane reads as two
    /// unrelated forms stacked by accident. One constant, both sections.
    enum SettingsMetrics {
        /// Wide enough for the longest label in either section ("DMR Radio ID")
        /// at `.callout`, and the same 96 pt `QuickConfigView` already uses for
        /// its own rows — so a user scrolling from the call card into Settings
        /// sees one grid, not three.
        static let labelWidth: CGFloat = 96
        /// Gap between the label column and the control.
        static let gutter: CGFloat = 8
        /// How far a caption indents to sit under its field rather than under
        /// its label.
        static var captionIndent: CGFloat { labelWidth + gutter }
    }

    /// One labelled row: a fixed-width leading label, then the control.
    ///
    /// A placeholder is not a label — it vanishes exactly when the field has
    /// content, which is the moment you most want to know what you are looking
    /// at. Four fields on this pane hold values a person types once and then
    /// re-reads months later trying to work out which is which, so they get
    /// standing labels and the placeholders go away.
    struct SettingsField<Content: View>: View {
        private let label: String
        private let content: Content

        init(_ label: String, @ViewBuilder content: () -> Content) {
            self.label = label
            self.content = content()
        }

        var body: some View {
            HStack(alignment: .firstTextBaseline, spacing: SettingsMetrics.gutter) {
                Text(label)
                    .font(.callout)
                    .lineLimit(1)
                    // Never let a long label squeeze the field instead of
                    // taking its own column.
                    .fixedSize(horizontal: true, vertical: false)
                    .frame(width: SettingsMetrics.labelWidth, alignment: .leading)
                content
            }
        }
    }

    extension View {
        /// Indent a caption so it starts under the field it explains, not under
        /// the label column.
        func settingsCaptionIndent() -> some View {
            padding(.leading, SettingsMetrics.captionIndent)
        }
    }
#endif
