// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// The EchoLink values the app supplies and the engine does not.
///
/// The engine names no EchoLink host at all (`ci/guard-echolink.sh` keeps it
/// that way), so the directory servers a login tries live here and in
/// astar-server's config default, and nowhere else.
public enum EchoLinkDefaults {
    /// The public directory servers, tried in this order: the main one first,
    /// then the regional ones, then the backup.
    public static let servers = [
        "servers.echolink.org", "naeast.echolink.org", "nasouth.echolink.org",
        "backup.echolink.org",
    ]
}

/// Which slice of the directory's station list to show.
public enum EchoLinkFilter: CaseIterable, Sendable {
    /// Every station, kinds this build does not recognise included.
    case all
    /// The stations the operator starred, listed or not. See
    /// `echolinkListRows`.
    case favorites
    /// Single operators: a plain callsign.
    case people
    /// Links: a callsign ending in `-L`.
    case links
    /// Repeaters: a callsign ending in `-R`.
    case repeaters
    /// Conferences: a callsign in stars, like `*ECHOTEST*`.
    case conferences

    /// The segmented control's label.
    public var title: String {
        switch self {
        case .all: return "All"
        case .favorites: return "Favorites"
        case .people: return "People"
        case .links: return "Links"
        case .repeaters: return "Repeaters"
        case .conferences: return "Conferences"
        }
    }

    fileprivate func admits(_ kind: EchoLinkStationKind) -> Bool {
        switch self {
        case .all, .favorites: return true
        case .people: return kind == .user
        case .links: return kind == .link
        case .repeaters: return kind == .repeater
        case .conferences: return kind == .conference
        }
    }
}

/// The stations in `stations` that `filter` admits and `query` matches,
/// sorted by callsign.
///
/// `query` is trimmed first; an empty one matches everything. Otherwise a
/// station matches when its callsign contains the query in any case, or when
/// its node number starts with it — so `"echo"` finds `*ECHOTEST*` and `"99"`
/// finds node 9999 but not node 1999.
///
/// `.favorites` admits the stations `favorites` names — the EchoLink
/// favorites' saved targets, each a callsign or a node number (see
/// `echolinkStationIsFavorite`). Only the listed ones come back from here,
/// and `echolinkListRows` adds the rest.
public func filterEchoLinkStations(
    _ stations: [EchoLinkStationEntry], filter: EchoLinkFilter, query: String,
    favorites: [String] = []
) -> [EchoLinkStationEntry] {
    let needle = query.trimmingCharacters(in: .whitespacesAndNewlines)
    let keys = filter == .favorites ? Set(favorites.map(echolinkKey)) : []
    return
        stations
        .filter { filter.admits($0.kind) }
        .filter { filter != .favorites || echolinkStationIsFavorite($0, keys: keys) }
        .filter { station in
            needle.isEmpty
                || station.call.range(of: needle, options: .caseInsensitive) != nil
                || String(station.node).hasPrefix(needle)
        }
        .sorted { a, b in
            let order = a.call.caseInsensitiveCompare(b.call)
            return order == .orderedSame ? a.node < b.node : order == .orderedAscending
        }
}

/// Where an on-demand refresh of the EchoLink station list stands: what the
/// pane's Refresh control shows.
public enum EchoLinkRefreshState: Equatable, Sendable {
    /// Nothing asked lately: the control reads "Refresh".
    case idle
    /// The request is on its way to the directory.
    case refreshing
    /// The directory was asked; the new list follows in a moment.
    case refreshed
    /// The last list came less than 30 seconds ago, so nothing was asked:
    /// "Try again in a moment".
    case tooSoon
}

/// Which of its states the EchoLink pane shows.
///
/// The login and the call are read together because a call can only exist
/// on top of a login, and the pane shows the most specific thing in hand.
public enum EchoLinkPaneMode: Equatable, Sendable {
    /// The login form: logged out, or refused for good (the form then offers
    /// "Log in again").
    case signIn
    /// A login in flight, including one that found no server and is waiting
    /// to try again by itself — a fresh form there would only restart a wait
    /// already running.
    case signingIn
    /// Logged in with no call: the station list.
    case stations
    /// A call connecting or connected: the far station and the chat.
    case call

    public init(login: EchoLinkLoginState, retrying: Bool, link: EchoLinkLink?) {
        if let link, link != .closed {
            self = .call
            return
        }
        switch login {
        case .loggedIn: self = .stations
        case .loggingIn: self = .signingIn
        case .failed: self = retrying ? .signingIn : .signIn
        case .loggedOut: self = .signIn
        }
    }
}

extension EchoLinkStationStatus {
    /// What VoiceOver reads for the station's dot, so its colour is never the
    /// only thing saying whether the station can be called.
    public var spokenName: String {
        switch self {
        case .online: return "Free"
        case .busy: return "Busy"
        case .unknown: return "Status unknown"
        }
    }
}

/// One line of the EchoLink chat with an id that stays with it.
///
/// The engine hands over the whole chat on every read, oldest first and
/// capped at 256 lines, so neither a line's position nor the count says which
/// lines are new: at the cap the oldest drops as the newest arrives and the
/// count stays put. The ids here follow the lines themselves.
public struct EchoLinkChatEntry: Identifiable, Equatable, Sendable {
    public let id: Int
    public let line: EchoLinkChatLine
}

/// Give `next` (the chat as the engine now reports it) ids, keeping each id
/// from `previous` for the lines the two share.
///
/// The shared lines are the longest run that ends `previous` and starts
/// `next` — what is left after the cap dropped lines off the front. Lines
/// after that run are new and take ids from `nextID` upward. A chat sharing
/// nothing (a new call) gets all-new ids.
public func echolinkChatEntries(
    previous: [EchoLinkChatEntry], next lines: [EchoLinkChatLine], nextID: inout Int
) -> [EchoLinkChatEntry] {
    let previousLines = previous.map(\.line)
    var shared = 0
    for k in stride(from: min(previousLines.count, lines.count), to: 0, by: -1)
    where previousLines.suffix(k).elementsEqual(lines.prefix(k)) {
        shared = k
        break
    }
    var entries = Array(previous.suffix(shared))
    for line in lines.dropFirst(shared) {
        entries.append(EchoLinkChatEntry(id: nextID, line: line))
        nextID += 1
    }
    return entries
}
