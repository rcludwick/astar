// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// How to connect to a directory entry — the discriminated `dial` object from
/// hamcall-db's reflector feed, decoded on its `kind`.
///
/// The design doc calls this type `DialTarget`; that name was already taken by
/// the smart dial field's node-or-address classifier (astar-427f), which is a
/// different thing entirely — what the *user typed* versus what the
/// *publisher published*. Renamed rather than merged.
///
/// `unsupported` is load-bearing. Any `kind` this build has no shape for
/// decodes into it, carrying the publisher's string, so the row stays listed
/// and the UI can say why Connect is off. That is the extension mechanism: a
/// new network appears in the feed and today's build lists it honestly
/// instead of pretending it does not exist.
///
/// A *known* kind whose payload does not hold up — a `dextra` row with no
/// port, a host that is blank — also lands in `unsupported`, for the same
/// reason: the entry is real, its dial is not usable, and dropping it would
/// make the directory look broken rather than incomplete.
public enum ReflectorDial: Hashable, Sendable {
    /// D-Star, DExtra linking. `callsign` is the reflector's own callsign
    /// (e.g. "XRF836") that the protocol addresses; `modules` is empty for
    /// every XLX row today — the registry does not publish which are active.
    case dextra(host: String, port: UInt16, callsign: String, modules: [String])
    /// M17 reflector. `callsign` is the reflector designator ("M17-002").
    case m17(host: String, port: UInt16, callsign: String, modules: [String])
    /// System Fusion / YSF reflector.
    case ysf(host: String, port: UInt16)
    /// NXDN reflector.
    case nxdn(host: String, port: UInt16)
    /// P25 reflector.
    case p25(host: String, port: UInt16)
    /// URF reflector. The port is genuinely optional here and only here — all
    /// 89 URF rows publish a host and no port, so requiring one would drop the
    /// whole network. Inventing a default would be worse: a guessed port is a
    /// connection attempt against something that is not the reflector.
    case urf(host: String, port: UInt16?, modules: [String])
    /// A kind this build cannot dial, named as the publisher named it.
    case unsupported(kind: String)

    /// The wire `kind`, preserved for every case including `unsupported`.
    public var kind: String {
        switch self {
        case .dextra: return "dextra"
        case .m17: return "m17"
        case .ysf: return "ysf"
        case .nxdn: return "nxdn"
        case .p25: return "p25"
        case .urf: return "urf"
        case .unsupported(let kind): return kind
        }
    }

    /// Whether this build knows how to turn the dial into a connection. False
    /// for `unsupported`, and for a `urf` row with no port — there is nothing
    /// to connect to.
    public var isDialable: Bool {
        switch self {
        case .dextra, .m17, .ysf, .nxdn, .p25: return true
        case .urf(_, let port, _): return port != nil
        case .unsupported: return false
        }
    }

    /// Host and port, when both are known. `nil` for `unsupported` and for a
    /// portless `urf` row.
    public var endpoint: (host: String, port: UInt16)? {
        switch self {
        case .dextra(let host, let port, _, _): return (host, port)
        case .m17(let host, let port, _, _): return (host, port)
        case .ysf(let host, let port): return (host, port)
        case .nxdn(let host, let port): return (host, port)
        case .p25(let host, let port): return (host, port)
        case .urf(let host, let port, _): return port.map { (host, $0) }
        case .unsupported: return nil
        }
    }

    /// The reflector's own callsign, where the protocol needs one (D-Star and
    /// M17 address the far end by callsign, not by host).
    public var callsign: String? {
        switch self {
        case .dextra(_, _, let callsign, _), .m17(_, _, let callsign, _): return callsign
        default: return nil
        }
    }

    /// Modules the publisher lists as active. Empty is the normal case for
    /// D-Star — see `dextra`. Empty means "not published", never "none".
    public var modules: [String] {
        switch self {
        case .dextra(_, _, _, let modules), .m17(_, _, _, let modules),
            .urf(_, _, let modules):
            return modules
        default: return []
        }
    }
}

extension ReflectorDial: Codable {
    private enum CodingKeys: String, CodingKey {
        case kind, host, port, callsign, modules
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        // `kind` is the discriminator; without it there is nothing to decode
        // on, and the row's own decoder turns that into "listed, not dialable".
        let kind = try c.decode(String.self, forKey: .kind)

        // Every field below is read leniently — a wrong *type* reads as absent
        // rather than throwing. A known kind whose payload does not hold up
        // then degrades to `.unsupported(kind:)`, because that failure belongs
        // to one dial object, not to the 3,000 rows around it.
        func text(_ key: CodingKeys) -> String? {
            guard let raw = (try? c.decodeIfPresent(String.self, forKey: key)) ?? nil else {
                return nil
            }
            let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        }
        let host = text(.host)
        let callsign = text(.callsign)
        let port = ((try? c.decodeIfPresent(UInt16.self, forKey: .port)) ?? nil)
            .flatMap { $0 > 0 ? $0 : nil }
        let modules = ((try? c.decodeIfPresent([String].self, forKey: .modules)) ?? nil) ?? []

        guard let host else {
            self = .unsupported(kind: kind)
            return
        }

        switch kind.lowercased() {
        // The one kind where a missing port is data, not damage.
        case "urf":
            self = .urf(host: host, port: port, modules: modules)
        case "dextra":
            guard let port, let callsign else {
                self = .unsupported(kind: kind)
                return
            }
            self = .dextra(host: host, port: port, callsign: callsign, modules: modules)
        case "m17":
            guard let port, let callsign else {
                self = .unsupported(kind: kind)
                return
            }
            self = .m17(host: host, port: port, callsign: callsign, modules: modules)
        case "ysf":
            guard let port else {
                self = .unsupported(kind: kind)
                return
            }
            self = .ysf(host: host, port: port)
        case "nxdn":
            guard let port else {
                self = .unsupported(kind: kind)
                return
            }
            self = .nxdn(host: host, port: port)
        case "p25":
            guard let port else {
                self = .unsupported(kind: kind)
                return
            }
            self = .p25(host: host, port: port)
        default:
            self = .unsupported(kind: kind)
        }
    }

    /// Re-emits the wire shape, so a decoded feed can be written back out
    /// unchanged enough to serve as a cache. `unsupported` keeps only its
    /// `kind` — the payload it could not read is not ours to reconstruct.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(kind, forKey: .kind)
        if case .unsupported = self { return }
        if case .urf(let host, let port, _) = self {
            try c.encode(host, forKey: .host)
            try c.encodeIfPresent(port, forKey: .port)
        } else if let endpoint {
            try c.encode(endpoint.host, forKey: .host)
            try c.encode(endpoint.port, forKey: .port)
        }
        try c.encodeIfPresent(callsign, forKey: .callsign)
        if !modules.isEmpty { try c.encode(modules, forKey: .modules) }
    }
}
