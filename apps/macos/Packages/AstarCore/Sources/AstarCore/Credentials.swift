// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// The secrets astar holds: the AllStar portal account, and one master
/// password per DMR network.
///
/// `portalPass` is a secret: it is stored only in the Keychain, loaded into a
/// `StationConfig` solely at station construction (consumed by the binding,
/// never retained), and deliberately redacted from debug output. Honors the
/// secret-free contract (PTT/secret prime rules).
///
/// The DMR passwords live here for exactly that reason. They are the same kind
/// of thing as the portal password — a credential, not a setting — so they get
/// the same treatment: the Keychain and nowhere else, read at connect, handed
/// to the engine by value, never assigned to a published property and never
/// written into an exported `.astarconfig`.
public struct Credentials: Equatable, Codable {
    public var portalUser: String
    public var portalPass: String
    public var portalNode: String
    /// Master password per DMR network, keyed by the DIRECTORY's `system`
    /// slug — `tgif`, `freedmr-network`, `ipsc2-poland`, `brandmeister`.
    ///
    /// A dictionary rather than one field because each DMR network issues its
    /// own: a FreeDMR hotspot password is not a DMR+ one, and offering a
    /// single box would invite an operator to send one network's secret to
    /// another.
    ///
    /// Keyed by the directory's slug rather than by `DmrFamily` because that
    /// is the granularity the password is issued at — 111 systems, nine
    /// families — and because it is the string the dial already carries.
    ///
    /// Defaulted `[:]` so a payload written by an earlier build still decodes.
    /// Adding it does NOT move `ConfigVersion`: nothing here is exported, and
    /// an older reader ignores what it does not know.
    public var dmrPasswords: [String: String]

    public init(
        portalUser: String, portalPass: String, portalNode: String,
        dmrPasswords: [String: String] = [:]
    ) {
        self.portalUser = portalUser
        self.portalPass = portalPass
        self.portalNode = portalNode
        self.dmrPasswords = dmrPasswords
    }

    /// Lenient on the one key that can be absent, so a Keychain payload
    /// written before DMR existed still decodes into a usable value rather
    /// than reading as "no account at all".
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        portalUser = try c.decode(String.self, forKey: .portalUser)
        portalPass = try c.decode(String.self, forKey: .portalPass)
        portalNode = try c.decode(String.self, forKey: .portalNode)
        dmrPasswords =
            ((try? c.decodeIfPresent([String: String].self, forKey: .dmrPasswords)) ?? nil) ?? [:]
    }

    /// The master password for `system`, or `nil` when none is saved.
    /// Trimmed, and an all-whitespace entry counts as none — a login with a
    /// blank password is a refusal the operator cannot interpret.
    public func dmrPassword(system: String) -> String? {
        let key = system.trimmingCharacters(in: .whitespaces).lowercased()
        guard let raw = dmrPasswords[key] ?? dmrPasswords[system] else { return nil }
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

extension Credentials: CustomDebugStringConvertible {
    public var debugDescription: String {
        // The DMR passwords are counted, never shown — the count is the one
        // fact a support thread can use and the values are the one it must
        // never carry.
        "Credentials(user: \(portalUser), node: \(portalNode), pass: <redacted>, "
            + "dmrPasswords: \(dmrPasswords.count) <redacted>)"
    }
}
