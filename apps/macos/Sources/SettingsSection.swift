// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import SwiftUI

    /// One collapsible section of the Settings `List`: a header row with a
    /// disclosure triangle, an SF Symbol and the title, and the section's rows
    /// beneath it while it is open.
    ///
    /// A `Section` with a custom header rather than `Section(isExpanded:)`,
    /// because that initialiser is macOS 14+ and astar still runs on 13 — and
    /// one header on every OS means one look to keep right. Staying a real
    /// `Section` matters: Audio configs' `.onMove` drag-to-reorder and the
    /// anchors' `ScrollViewProxy.scrollTo` both need the rows to be rows of
    /// the one `List`, which a `DisclosureGroup` wrapping them would not give.
    ///
    /// Open/closed state lives in `SettingsDisclosure` (environment object),
    /// remembered per section under `ui.settings.<id>.expanded`.
    struct SettingsSection<Content: View, Accessory: View>: View {
        let id: SettingsSectionID
        @ViewBuilder let content: () -> Content
        /// Header controls that must stay reachable while the section is
        /// folded — the "+" on Vocoders, Audio configs and Mic Profiles.
        @ViewBuilder let accessory: () -> Accessory

        @EnvironmentObject private var disclosure: SettingsDisclosure

        init(
            _ id: SettingsSectionID,
            @ViewBuilder content: @escaping () -> Content,
            @ViewBuilder accessory: @escaping () -> Accessory
        ) {
            self.id = id
            self.content = content
            self.accessory = accessory
        }

        private var expanded: Bool { disclosure.isExpanded(id) }

        var body: some View {
            Section {
                if expanded {
                    content()
                }
            } header: {
                HStack(spacing: 8) {
                    Button {
                        // ⌥-click toggles every section, as in Finder's list
                        // view. Read at click time: a button's action has no
                        // event of its own to ask.
                        let all = NSEvent.modifierFlags.contains(.option)
                        withAnimation(.easeInOut(duration: 0.18)) {
                            disclosure.toggle(id, all: all)
                        }
                    } label: {
                        SettingsSectionHeaderLabel(id: id, expanded: expanded)
                    }
                    .buttonStyle(.plain)
                    .help(
                        expanded
                            ? "Collapse \(id.title) (Option-click to collapse all)"
                            : "Expand \(id.title) (Option-click to expand all)"
                    )
                    // VoiceOver: "Vocoders, collapsed, button". The symbol and
                    // the triangle say nothing a listener needs twice.
                    .accessibilityLabel(id.title)
                    .accessibilityValue(expanded ? "expanded" : "collapsed")
                    .accessibilityHint(expanded ? "Collapses this section" : "Expands this section")
                    accessory()
                }
            }
        }
    }

    extension SettingsSection where Accessory == EmptyView {
        init(_ id: SettingsSectionID, @ViewBuilder content: @escaping () -> Content) {
            self.init(id, content: content, accessory: { EmptyView() })
        }
    }

    /// The triangle, symbol and title. The whole width up to any accessory is
    /// the hit target, so clicking the title works as well as the triangle.
    private struct SettingsSectionHeaderLabel: View {
        let id: SettingsSectionID
        let expanded: Bool

        var body: some View {
            HStack(spacing: 6) {
                Image(systemName: "chevron.right")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .rotationEffect(.degrees(expanded ? 90 : 0))
                    .frame(width: 12)
                Image(systemName: id.systemImage)
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(.secondary)
                    .frame(width: 18)
                Text(id.title)
                Spacer(minLength: 8)
            }
            .frame(minHeight: 22)
            .contentShape(Rectangle())
            .accessibilityElement(children: .ignore)
        }
    }

    /// The Settings title bar's "Expand All" / "Collapse All". Offers Expand
    /// All while anything is still folded, as Finder's View menu does.
    struct SettingsExpandAllButton: View {
        @EnvironmentObject private var disclosure: SettingsDisclosure

        var body: some View {
            let open = !disclosure.allExpanded
            Button(open ? "Expand All" : "Collapse All") {
                withAnimation(.easeInOut(duration: 0.18)) { disclosure.setAll(open) }
            }
            .buttonStyle(.borderless)
            .controlSize(.small)
            .help(open ? "Open every Settings section" : "Fold every Settings section")
        }
    }
#endif
