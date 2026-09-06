// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Which digital-voice network a reflector directory row belongs to.
///
/// Deliberately NOT `Network`. `Network` answers "where can astar place the
/// next call", and its cases are the app's own picker segments; this answers
/// "what did the publisher say this row is", and the publisher covers networks
/// astar will never dial (P25, NXDN). Folding the two together would either
/// pollute the picker with un-dialable segments or throw away rows the
/// directory is supposed to list-but-refuse.
///
/// `other` is the whole point: a network hamcall-db adds after this build
/// shipped must still decode, still be searchable, and still be reportable to
/// the user as "astar can see this, astar can't dial it". Dropping unknown
/// rows would make a growing directory look like a shrinking one.
public enum ReflectorNetwork: Hashable, Sendable {
    /// D-Star — XLX/XRF reflectors, from the XLX registry.
    case dstar
    /// M17 reflectors.
    case m17
    /// System Fusion / YSF reflectors.
    case ysf
    /// NXDN reflectors.
    case nxdn
    /// P25 reflectors.
    case p25
    /// URF (the XLX successor) reflectors.
    case urf
    /// A network this build has never heard of, with the publisher's own
    /// string kept verbatim so the UI can name it and a later build can
    /// recognise it without a data migration.
    case other(String)

    /// The wire value. Round-trips: `ReflectorNetwork(rawValue: n.rawValue) == n`
    /// for every case, `other` included.
    public var rawValue: String {
        switch self {
        case .dstar: return "dstar"
        case .m17: return "m17"
        case .ysf: return "ysf"
        case .nxdn: return "nxdn"
        case .p25: return "p25"
        case .urf: return "urf"
        case .other(let raw): return raw
        }
    }

    /// Never fails. An unrecognised string is a fact about the data, not an
    /// error in it — see `other`.
    public init(rawValue: String) {
        switch rawValue.lowercased() {
        case "dstar": self = .dstar
        case "m17": self = .m17
        case "ysf": self = .ysf
        case "nxdn": self = .nxdn
        case "p25": self = .p25
        case "urf": self = .urf
        default: self = .other(rawValue)
        }
    }

    /// The networks this build knows by name, in the order the settings
    /// summary line lists them. Excludes `other`, which is unbounded.
    public static let known: [ReflectorNetwork] = [.dstar, .m17, .ysf, .nxdn, .p25, .urf]

    /// User-facing label. An unknown network gets its raw string uppercased —
    /// wrong-ish is better than blank, and blank is what "unknown" would give.
    public var displayName: String {
        switch self {
        case .dstar: return "D-Star"
        case .m17: return "M17"
        case .ysf: return "YSF"
        case .nxdn: return "NXDN"
        case .p25: return "P25"
        case .urf: return "URF"
        case .other(let raw): return raw.uppercased()
        }
    }
}

extension ReflectorNetwork: Codable {
    public init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(rawValue)
    }
}

extension Network {
    /// The directory network this app network dials, when there is one.
    ///
    /// `allstar` is not a reflector network at all, and `hamlink`
    /// (SvxReflector) is not in hamcall-db — those two have no counterpart.
    /// M17 and D-Star do, and gaining D-Star's line here is what makes the
    /// directory's 944 D-Star rows dialable rather than merely listed.
    public var reflectorNetwork: ReflectorNetwork? {
        switch self {
        case .m17: return .m17
        case .dstar: return .dstar
        case .ysf: return .ysf
        case .allstar, .hamlink: return nil
        }
    }
}

extension Network {
    /// The app network that dials `directory`, or `nil` when astar has no
    /// case for it.
    ///
    /// The inverse of `reflectorNetwork`, and deliberately partial: the
    /// directory covers networks astar will never dial (NXDN, P25, URF), and
    /// `nil` for those is the honest answer rather than a fallback to
    /// something dialable. Used where a directory row has to reach the network
    /// picker — the search sheet browsing past the selected network.
    public static func matching(_ directory: ReflectorNetwork) -> Network? {
        switch directory {
        case .m17: return .m17
        case .dstar: return .dstar
        case .ysf: return .ysf
        case .nxdn, .p25, .urf, .other: return nil
        }
    }
}
