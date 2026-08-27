// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// A frozen name → entry lookup over one loaded feed.
///
/// `ReflectorDirectory` is `@MainActor` — it publishes, it syncs, it owns a
/// file. Dialling is none of those things: `CallSession.connect` runs on a
/// background thread by contract, because the AllStar path mints a portal
/// token over HTTP before it dials. So the thing the dial path holds is this:
/// an immutable, `Sendable` value the directory hands out whenever its feed
/// changes, with a pure resolution function on it. No actor hop on the dial
/// path, and a resolution test needs no directory, no storage and no clock.
///
/// `.empty` is a first-class state, not a degenerate one. A build with no
/// bundled snapshot and no sync yet holds exactly this, and every name
/// resolves to `notInDirectory` — which is today's address-only behaviour,
/// unchanged.
public struct ReflectorIndex: Equatable, Sendable {
    private let entries: [DirectoryEntry]
    /// Lowercased `id` and every alias → indices into `entries`. A name can
    /// map to several rows: ids repeat across networks ("100" is both an NXDN
    /// and a P25 reflector), which is why the network is part of the match
    /// rather than a filter applied after it.
    private let byName: [String: [Int]]

    public init(entries: [DirectoryEntry]) {
        self.entries = entries
        var byName: [String: [Int]] = [:]
        for (index, entry) in entries.enumerated() {
            for name in [entry.id] + entry.aliases {
                byName[name.lowercased(), default: []].append(index)
            }
        }
        self.byName = byName
    }

    /// What a client holds before anything has loaded.
    public static let empty = ReflectorIndex(entries: [])

    public var isEmpty: Bool { entries.isEmpty }

    /// Exact, case-insensitive match on `id` or any alias, within one network.
    ///
    /// Aliases match because they are the same box under another name —
    /// `XRF836` and `XLX836` are one reflector, and an operator who knows it
    /// by the older name should not be told it does not exist.
    public func entry(named name: String, network: ReflectorNetwork) -> DirectoryEntry? {
        let key = name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !key.isEmpty, let indices = byName[key] else { return nil }
        return indices.lazy.map { self.entries[$0] }.first { $0.network == network }
    }

    /// Interpret dial-field text against the directory — the whole of
    /// "directory first, address second".
    ///
    /// A name is resolved here or nowhere; the caller's address parser is
    /// reachable only through `notInDirectory`. That ordering is the point:
    /// `XLX836` is a perfectly well-formed *hostname*, and `DialTarget.parse`
    /// will happily classify it as one. Asking the address parser first would
    /// mean a reflector name silently becoming a DNS lookup that fails — or,
    /// worse, one that succeeds against something that is not the reflector.
    ///
    /// The module is never defaulted. See `ReflectorDialResolution`.
    public func resolveDial(_ raw: String, network: ReflectorNetwork) -> ReflectorDialResolution {
        guard let parts = ReflectorDialText.split(raw),
            let entry = entry(named: parts.name, network: network)
        else { return .notInDirectory }

        // Listed but not dialable is a legitimate published state, and one the
        // client is required to honour: show the entry, refuse to connect.
        guard let dial = entry.dial, dial.isDialable, let endpoint = dial.endpoint else {
            return .notDialable(entry)
        }

        guard dial.addressesModule else {
            return .ready(
                ResolvedReflector(
                    entry: entry, host: endpoint.host, port: endpoint.port,
                    callsign: dial.callsign, module: nil))
        }
        guard let module = parts.module.flatMap(ReflectorDialText.module) else {
            return .needsModule(entry)
        }
        return .ready(
            ResolvedReflector(
                entry: entry, host: endpoint.host, port: endpoint.port,
                callsign: dial.callsign, module: module))
    }
}
