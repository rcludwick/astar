// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Whether a credential field should be drawn normally or flagged in red, and
/// why (astar-4e8a).
public enum CredentialFieldStatus: Equatable {
    /// Nothing to say — draw the field normally.
    case neutral
    /// Draw it red and show this reason underneath.
    case invalid(String)

    public var isInvalid: Bool { self != .neutral }

    /// The reason, or `nil` when there is nothing wrong. A red field with no
    /// explanation is a puzzle, so callers show this alongside the highlight.
    public var message: String? {
        switch self {
        case .neutral: return nil
        case .invalid(let text): return text
        }
    }
}

/// Red-highlight rules for the AllStarLink account fields.
///
/// Pure and platform-neutral so the Iced client can apply the same rules, and so
/// the one genuinely dangerous case below is pinned by a test rather than by
/// whoever next edits the view.
public enum CredentialsValidation {

    /// Shown when the password box is empty and no account is saved. Names
    /// `allstarlink.org` deliberately: entering the node's IAX secret here
    /// instead of the portal account password is the most common mix-up, and it
    /// fails in a way that looks like a wrong password.
    public static let missingPassword =
        "Enter your allstarlink.org account password — not the node's IAX secret."

    /// Headline of the Settings callout shown while no account is saved.
    /// States the consequence rather than the omission: "no account" is a fact
    /// about the form, "AllStarLink is unavailable" is what it costs you.
    public static let allStarUnavailable = "AllStarLink is unavailable until you add an account."

    /// The detail under that headline. Says *why* there is no way around it —
    /// guest dialling was removed (au-1517), so every call goes on air as the
    /// user's own node rather than anonymously — and scopes the damage: M17 is a
    /// separate network that needs a callsign, not a portal login.
    public static let allStarUnavailableDetail =
        "Dialling an AllStarLink node signs in to your allstarlink.org account — "
        + "there is no guest access. M17 does not need one."

    /// Whether Settings should show that callout.
    public static func showsAllStarUnavailable(hasCredentials: Bool) -> Bool {
        !hasCredentials
    }

    /// Shown after a token-mint test comes back rejected.
    public static let portalRejected =
        "The AllStarLink portal rejected these credentials. Check the callsign, password, and node."

    /// How to draw the password field.
    ///
    /// - Parameters:
    ///   - text: what is currently typed in the box.
    ///   - hasSavedCredentials: whether an account is already in the Keychain.
    ///   - portalRejected: whether the last token-mint test was rejected.
    ///
    /// The load-bearing rule is the first one. astar never pre-fills the
    /// password — it lives in the Keychain and the field reads "re-enter to
    /// change" — so an empty box on a saved account is the *normal* resting
    /// state, not a fault. Flagging it red would tell someone whose credentials
    /// work perfectly that they are broken, which is worse than no highlight at
    /// all.
    ///
    /// A rejection outranks a filled-in box, because that is precisely the case
    /// where the user cannot see the stored password to check it themselves. But
    /// "missing" outranks "rejected", since nobody can act on a rejection while
    /// the field is empty.
    public static func password(
        text: String, hasSavedCredentials: Bool, portalRejected: Bool
    ) -> CredentialFieldStatus {
        let isBlank = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        if isBlank {
            // Saved account + blank box = "unchanged", unless the portal has
            // actually told us the stored credentials are bad.
            if hasSavedCredentials {
                return portalRejected ? .invalid(Self.portalRejected) : .neutral
            }
            return .invalid(Self.missingPassword)
        }
        return portalRejected ? .invalid(Self.portalRejected) : .neutral
    }
}
