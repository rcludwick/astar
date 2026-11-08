// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Combine
import Foundation

/// One collapsible section of the Settings pane, top to bottom.
///
/// Settings grew to thirteen sections, which is several screens of scrolling
/// before you find the one you came for. Each section now folds behind a
/// disclosure triangle, and this enum is the list of them: the title and SF
/// Symbol its header shows, whether it starts open, and the preferences key
/// that remembers whether you left it open.
public enum SettingsSectionID: String, CaseIterable, Hashable, Sendable {
    case operatorIdentity = "operator"
    case account
    case echolink
    case dmr
    case nxdn
    case vocoders
    case savedConfigs
    case favorites
    case micProfiles
    case reflectorDirectory
    case spectrum
    case recording
    case backup

    /// The header text. These are the names the sections already had.
    public var title: String {
        switch self {
        case .operatorIdentity: return "Operator"
        case .account: return "AllStarLink"
        case .echolink: return "EchoLink"
        case .dmr: return "DMR"
        case .nxdn: return "NXDN"
        case .vocoders: return "Vocoders"
        case .savedConfigs: return "Saved configs"
        case .favorites: return "Favorites"
        case .micProfiles: return "Mic Profiles"
        case .reflectorDirectory: return "Reflector directory"
        case .spectrum: return "Spectrum"
        case .recording: return "Recording"
        case .backup: return "Backup"
        }
    }

    /// The SF Symbol beside the title. Every one exists on macOS 13.
    public var systemImage: String {
        switch self {
        case .operatorIdentity: return "person.crop.circle"
        case .account: return "key"
        case .echolink: return "dot.radiowaves.left.and.right"
        case .dmr: return "number.circle"
        case .nxdn: return "number.square"
        case .vocoders: return "memorychip"
        case .savedConfigs: return "square.stack.3d.up"
        case .favorites: return "star"
        case .micProfiles: return "mic"
        case .reflectorDirectory: return "list.bullet.rectangle"
        case .spectrum: return "waveform"
        case .recording: return "record.circle"
        case .backup: return "archivebox"
        }
    }

    /// Who you are starts open; everything you own starts folded, so a fresh
    /// Settings pane fits on about one screen.
    public var defaultExpanded: Bool {
        switch self {
        case .operatorIdentity, .account: return true
        default: return false
        }
    }

    /// Where the open/closed state is remembered. Under `ui.settings.`, which
    /// an exported config never carries (`ConfigArchive.localOnlyPrefixes`).
    public var defaultsKey: String { "\(Self.keyPrefix)\(rawValue).expanded" }

    /// The namespace every section's key lives in.
    public static let keyPrefix = "ui.settings."
}

/// A place inside the Settings pane that something outside it can send the
/// operator to — a connect error's "Open Vocoder Settings…", say.
public enum SettingsAnchor: Hashable, Sendable {
    case vocoders
    /// Settings → EchoLink: the callsign, password and login.
    case echolink

    /// The section the anchor sits in. A jump opens this section first: a
    /// folded section has no rows to scroll to. A `switch` with no `default`,
    /// so a new anchor cannot compile without naming its section.
    public var section: SettingsSectionID {
        switch self {
        case .vocoders: return .vocoders
        case .echolink: return .echolink
        }
    }
}

/// Which Settings sections are open, remembered across launches.
///
/// One object for all thirteen rather than an `@AppStorage` per section,
/// because Expand All, ⌥-click and anchor jumps all act on every section at
/// once, and the header's "Expand All / Collapse All" label has to know
/// whether anything is still folded. Each section still has its own
/// preferences key, so the state on disk is the same either way.
@MainActor
public final class SettingsDisclosure: ObservableObject {
    @Published public private(set) var expanded: Set<SettingsSectionID>

    private let defaults: UserDefaults

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        expanded = Set(
            SettingsSectionID.allCases.filter { id in
                (defaults.object(forKey: id.defaultsKey) as? Bool) ?? id.defaultExpanded
            })
    }

    public func isExpanded(_ id: SettingsSectionID) -> Bool { expanded.contains(id) }

    public func setExpanded(_ id: SettingsSectionID, _ open: Bool) {
        if open { expanded.insert(id) } else { expanded.remove(id) }
        defaults.set(open, forKey: id.defaultsKey)
    }

    /// Flip one section. With `all`, the ⌥-click: every section takes the
    /// state this one is moving to, as Finder's outline does.
    public func toggle(_ id: SettingsSectionID, all: Bool = false) {
        let open = !isExpanded(id)
        if all { setAll(open) } else { setExpanded(id, open) }
    }

    /// True once nothing is folded — the header then offers Collapse All.
    public var allExpanded: Bool { expanded.count == SettingsSectionID.allCases.count }

    public func setAll(_ open: Bool) {
        for id in SettingsSectionID.allCases { setExpanded(id, open) }
    }

    /// Open the section `anchor` sits in. Returns whether it had to open, so
    /// the caller knows to wait for its rows to mount before scrolling.
    @discardableResult
    public func reveal(_ anchor: SettingsAnchor) -> Bool {
        guard !isExpanded(anchor.section) else { return false }
        setExpanded(anchor.section, true)
        return true
    }
}
