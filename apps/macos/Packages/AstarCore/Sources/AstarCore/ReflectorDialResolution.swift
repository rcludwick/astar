// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// What the dial field's text means once the directory has had first refusal
/// on it.
///
/// Four cases, one of which — `notInDirectory` — is the *only* way out to the
/// address parsers. That is what makes "directory first, address second" a
/// property of the type rather than a convention every call site has to
/// remember: a caller cannot reach `DialTarget.parse` / `M17Dial.parse`
/// without having asked the directory and been told no.
///
/// `needsModule` is not an error. Nothing has gone wrong — the operator named
/// a real reflector and has not yet said which room. On D-Star the module *is*
/// the room, and there is no data to default it from (`modules` is empty for
/// every D-Star row, because the registry does not publish it), so a guess
/// would not fail visibly: it would succeed and put an operator into someone
/// else's conversation, keyed up under their own callsign. The state is
/// "resolved, incomplete", it carries the entry so the UI can show what *is*
/// known, and Connect stays off until a letter arrives.
public enum ReflectorDialResolution: Equatable {
    /// A directory entry astar can dial right now, with a module if its
    /// protocol needs one.
    case ready(ResolvedReflector)
    /// A directory entry, resolved, with no usable module yet. Incomplete, not
    /// wrong — see the type's note.
    case needsModule(DirectoryEntry)
    /// A directory entry astar cannot dial: no `dial` object at all, or a
    /// `kind` this build has no protocol for. Listed, deliberately, and
    /// refused just as deliberately.
    case notDialable(DirectoryEntry)
    /// The text names nothing in the directory. The caller falls through to
    /// its network's address grammar — which is exactly today's behaviour, and
    /// also what an empty or never-synced directory produces for everything.
    case notInDirectory

    /// The complete target, if there is one. Sugar for the common
    /// `if case .ready(let target)`.
    public var target: ResolvedReflector? {
        if case .ready(let target) = self { return target }
        return nil
    }

    /// The entry behind this resolution, for every case that has one. What a
    /// UI shows on the resolved-target line — including for `needsModule`,
    /// where showing the reflector while the module is still blank is the
    /// whole point.
    public var entry: DirectoryEntry? {
        switch self {
        case .ready(let target): return target.entry
        case .needsModule(let entry), .notDialable(let entry): return entry
        case .notInDirectory: return nil
        }
    }

    /// The resolved-target line, or `nil` when the text named nothing and
    /// there is consequently nothing to say about it.
    ///
    ///     XLX836 · module — · 45.56.69.219:30001
    ///     XLX836 · module A · 45.56.69.219:30001
    ///
    /// The em dash is the point of the incomplete line: what is known is shown
    /// filled in, what is missing is shown missing, and the line completes
    /// itself the moment a letter is typed. Nothing here reads as an error,
    /// because nothing is wrong — the form is unfinished. Formatting lives in
    /// the core rather than the view so both clients render the same sentence
    /// and a test can pin it.
    public var statusLine: String? {
        switch self {
        case .ready(let target):
            let module = target.module.map { "module \($0)" }
            return [target.entry.id, module, "\(target.host):\(target.port)"]
                .compactMap { $0 }
                .joined(separator: " · ")
        case .needsModule(let entry):
            let endpoint = entry.dial?.endpoint.map { "\($0.host):\($0.port)" }
            return [entry.id, "module —", endpoint].compactMap { $0 }.joined(separator: " · ")
        case .notDialable(let entry):
            return "\(entry.id) · listed, but astar can’t dial it yet"
        case .notInDirectory:
            return nil
        }
    }
}

/// A directory entry plus the module the operator chose: everything needed to
/// place the call, with nothing invented.
///
/// `module` is optional because it is optional in fact — YSF, NXDN and P25
/// reflectors have no module, and for those `ready` with `module == nil` is
/// complete. For the module-bearing protocols this is never `nil`: they leave
/// via `needsModule` instead.
public struct ResolvedReflector: Equatable {
    public let entry: DirectoryEntry
    public let host: String
    public let port: UInt16
    /// The reflector's own callsign, where the protocol addresses one
    /// ("XRF836", "M17-002"). `nil` for the networks that dial by address.
    public let callsign: String?
    /// The module the operator named, upper-cased. Never defaulted.
    public let module: Character?

    public init(
        entry: DirectoryEntry, host: String, port: UInt16, callsign: String? = nil,
        module: Character? = nil
    ) {
        self.entry = entry
        self.host = host
        self.port = port
        self.callsign = callsign
        self.module = module
    }
}

/// The dial field's name-and-module grammar, split from the text and nothing
/// more — no directory, no network, no I/O.
///
/// `XLX836`, `XLX836 A`, `XLX836/B`. Same separator rule as `M17Dial.parse`:
/// a reflector name contains neither a space nor a slash, so the first
/// occurrence of either is unambiguously where the module starts. Kept
/// separate from resolution so the grammar is testable on its own and so both
/// module-bearing networks share exactly one implementation of it — D-Star and
/// M17 differ in protocol, not in how an operator types a room.
public enum ReflectorDialText {
    /// Split trimmed text into a name and the raw module text after it.
    /// Returns `nil` only for text that cannot name anything at all (empty, or
    /// a bare separator).
    public static func split(_ raw: String) -> (name: String, module: String?)? {
        let text = raw.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return nil }
        guard let separator = text.firstIndex(where: { $0 == "/" || $0 == " " }) else {
            return (name: text, module: nil)
        }
        let name = String(text[..<separator])
        guard !name.isEmpty else { return nil }
        let rest = text[text.index(after: separator)...]
            .trimmingCharacters(in: .whitespaces)
        return (name: name, module: rest.isEmpty ? nil : rest)
    }

    /// A module letter, or `nil` if what was typed is not one.
    ///
    /// One ASCII letter, upper-cased — the same shape `M17Dial.parse` and the
    /// engine's own `connectM17` already enforce. Anything else (two letters,
    /// a digit, a word) is *not* a module, and the resolution it produces is
    /// `needsModule`, not a match: half-typed input should leave the resolved
    /// reflector on screen and the Connect button off, which is precisely what
    /// "incomplete" means.
    public static func module(_ raw: String) -> Character? {
        guard raw.count == 1, let letter = raw.first, letter.isASCII, letter.isLetter else {
            return nil
        }
        return Character(letter.uppercased())
    }

    /// Rewrite dial text to carry `module`, keeping the name exactly as typed.
    ///
    /// This is how a module picker reaches the dial field. The field stays the
    /// single source of truth for what will be dialled — the picker does not
    /// hold a module of its own beside it, because two places holding half a
    /// target each is how a UI comes to show `XLX836 A` and dial `XLX836`.
    ///
    /// The separator the operator already used is preserved, so someone typing
    /// `XLX836/` gets `XLX836/A` and someone typing `XLX836 ` gets `XLX836 A`.
    /// Text that names nothing comes back unchanged: there is no name to
    /// attach a module to, and inventing one would put a letter in an empty
    /// field.
    public static func applying(module letter: Character?, to raw: String) -> String {
        guard let parts = split(raw) else { return raw }
        // Normalised through `module(_:)` rather than trusted, so the one
        // definition of "a module letter" serves the picker and the parser
        // alike. A letter that would not parse produces the bare name — the
        // same incomplete-but-honest state as typing nothing.
        guard let letter, let normalised = module(String(letter)) else { return parts.name }
        let separator = raw.first { $0 == "/" || $0 == " " } ?? " "
        return "\(parts.name)\(separator)\(normalised)"
    }
}

extension ReflectorDial {
    /// Whether this protocol addresses a module. True for the reflector
    /// networks with rooms (D-Star/DExtra, M17, URF), false for the ones that
    /// dial a bare endpoint (YSF, NXDN, P25).
    ///
    /// Not derived from the `modules` array: that array is what the publisher
    /// *listed*, and it is empty for every D-Star row on earth. Emptiness
    /// there means "not published", never "this reflector has no rooms", and
    /// reading it the other way is how a client ends up dialling without one.
    public var addressesModule: Bool {
        switch self {
        case .dextra, .m17, .urf: return true
        // DMR's room is a talkgroup on a timeslot, and neither is a module:
        // a module separator in a DMR target is refused, not ignored.
        case .ysf, .nxdn, .mmdvm, .p25, .unsupported: return false
        }
    }
}
