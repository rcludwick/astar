// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Which module letters to offer for a reflector, and what a chosen letter is
/// worth remembering.
///
/// Both halves exist because of the same fact: `dial.modules` is empty for
/// every D-Star row, and emptiness there means "the registry does not publish
/// this", never "this reflector has no rooms" (see `ReflectorDial.modules`).
/// So the offer cannot be derived from the data alone, and the recollection is
/// the only per-reflector knowledge astar will ever have about modules.
public enum ReflectorModuleOptions {
    /// The full DExtra/XLX module range, used when the publisher lists none.
    /// A–Z is the protocol's range, not a guess about which are active — the
    /// picker offers the alphabet and the operator supplies the knowledge.
    public static let alphabet: [Character] = (UInt8(ascii: "A")...UInt8(ascii: "Z")).map {
        Character(UnicodeScalar($0))
    }

    /// The letters to put in front of the operator for this dial.
    ///
    /// Empty for a dial that addresses no module at all (YSF, NXDN, P25) and
    /// for `nil` — asking for a room on a network that has none is not a
    /// smaller version of the question, it is a different one, and the UI
    /// should show no picker rather than an empty one.
    ///
    /// A published list wins when there is one: M17 and URF rows do carry
    /// modules, and offering exactly what the reflector advertises is better
    /// than offering 26 letters of which 24 are dead.
    public static func options(for dial: ReflectorDial?) -> [Character] {
        guard let dial, dial.addressesModule else { return [] }
        let published = dial.modules.compactMap(ReflectorDialText.module(_:))
        // Deduplicated in publication order — a feed that lists "A" twice
        // should not produce two identical buttons.
        var seen = Set<Character>()
        let unique = published.filter { seen.insert($0).inserted }
        return unique.isEmpty ? alphabet : unique
    }
}

/// The last module used per reflector, so the cost of choosing one lands once
/// per reflector rather than once per call.
///
/// A recollection, never a default. The distinction is the whole reason the
/// directory refuses to guess a module (see `ReflectorDialResolution`): a
/// remembered letter is a thing the operator actually did, and it is offered
/// pre-selected in a picker they can see and change — not silently substituted
/// into a dial. Nothing here is ever consulted by `resolveDial`.
public final class ReflectorModuleMemory {
    public static let defaultsKey = "reflector.modules"

    private let defaults: UserDefaults
    private let key: String

    public init(defaults: UserDefaults = .standard, key: String = ReflectorModuleMemory.defaultsKey)
    {
        self.defaults = defaults
        self.key = key
    }

    /// Keyed on `DirectoryEntry.key` — `network:id`, not `id`. Bare ids
    /// collide across networks (NXDN "100" and P25 "100" both exist), and a
    /// collision here would hand one network's remembered room to another's.
    public func module(for entry: DirectoryEntry) -> Character? {
        stored[entry.key].flatMap(ReflectorDialText.module(_:))
    }

    public func remember(_ module: Character, for entry: DirectoryEntry) {
        guard let letter = ReflectorDialText.module(String(module)) else { return }
        var table = stored
        table[entry.key] = String(letter)
        defaults.set(table, forKey: key)
    }

    public func forget(_ entry: DirectoryEntry) {
        var table = stored
        guard table.removeValue(forKey: entry.key) != nil else { return }
        defaults.set(table, forKey: key)
    }

    /// The raw table. A value that is not a single letter is dropped on read
    /// rather than trusted — this is a preferences domain, and it is editable
    /// by hand.
    private var stored: [String: String] {
        (defaults.dictionary(forKey: key) as? [String: String]) ?? [:]
    }
}
