// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The sentences Settings shows about the reflector directory.
///
/// In the core rather than the view for the usual reason — both clients render
/// the same words and a test can pin them — and hand-rolled rather than handed
/// to `RelativeDateTimeFormatter` because these strings are asserted on. The
/// formatter's output is locale- and OS-version-dependent, so a test that
/// pinned it would pass on the machine that wrote it and fail elsewhere.
public enum ReflectorDirectoryStatus {

    /// `944 D-Star · 106 M17 · 1,432 YSF` — the networks actually held, in the
    /// order `ReflectorNetwork.known` lists them, with anything unrecognised
    /// after them in alphabetical order.
    ///
    /// Unknown networks are counted and named, not folded into an "other"
    /// bucket: a network hamcall-db adds after this build shipped is
    /// searchable in this very app, and a summary that hid it would make the
    /// directory look smaller than it is.
    public static func countsLine(_ counts: [ReflectorNetwork: Int]) -> String {
        let known = ReflectorNetwork.known.filter { counts[$0] != nil }
        let unknown = counts.keys
            .filter { !ReflectorNetwork.known.contains($0) }
            .sorted { $0.rawValue < $1.rawValue }
        let parts = (known + unknown).compactMap { network -> String? in
            guard let count = counts[network], count > 0 else { return nil }
            return "\(grouped(count)) \(network.displayName)"
        }
        return parts.joined(separator: " · ")
    }

    /// How fresh the loaded copy is.
    ///
    /// The bundled case gets its own sentence because "never synced" on its
    /// own reads as a failure, and it is not one: an install that has never
    /// reached the network still has a complete directory, and saying which
    /// one it has is the difference between reassurance and alarm.
    public static func freshness(lastFetched: Date?, isBundled: Bool, now: Date) -> String {
        guard let lastFetched else {
            return isBundled ? "Bundled with astar — never synced" : "Never synced"
        }
        return "Last synced \(elapsed(since: lastFetched, now: now))"
    }

    /// When the unattended refresh comes round again — the published cadence,
    /// made visible. astar does not choose this number; the feed does
    /// (`client_refresh_days`), and showing it is how an operator can tell
    /// that a directory sitting still is policy rather than breakage.
    public static func automaticLine(nextDue: Date?, now: Date) -> String {
        guard let nextDue else { return "Checks automatically at the next launch" }
        if nextDue <= now { return "Checks automatically at the next launch" }
        return "Checks again \(remaining(until: nextDue, now: now))"
    }

    /// "3 days ago", "just now" — the tail of the freshness line.
    static func elapsed(since date: Date, now: Date) -> String {
        let seconds = now.timeIntervalSince(date)
        // A clock that moved backwards (a timezone edit, a restored backup)
        // must not render as "in -3 days". The data is still current; only the
        // arithmetic is nonsense.
        guard seconds > 0 else { return "just now" }
        switch seconds {
        case ..<60: return "just now"
        case ..<3_600: return plural(Int(seconds / 60), "minute") + " ago"
        case ..<86_400: return plural(Int(seconds / 3_600), "hour") + " ago"
        default: return plural(Int(seconds / 86_400), "day") + " ago"
        }
    }

    /// "in 4 days" — the tail of the automatic-sync line.
    static func remaining(until date: Date, now: Date) -> String {
        let seconds = date.timeIntervalSince(now)
        guard seconds > 0 else { return "now" }
        switch seconds {
        case ..<3_600: return "in " + plural(max(1, Int(seconds / 60)), "minute")
        case ..<86_400: return "in " + plural(Int(seconds / 3_600), "hour")
        default: return "in " + plural(Int(seconds / 86_400), "day")
        }
    }

    private static func plural(_ count: Int, _ noun: String) -> String {
        count == 1 ? "1 \(noun)" : "\(count) \(noun)s"
    }

    /// Thousands separators, so 1,432 does not read as 1432.
    private static func grouped(_ count: Int) -> String {
        count.formatted(.number.grouping(.automatic))
    }
}

extension ReflectorDirectory.SyncOutcome {
    /// One line saying what a sync just did, for the status slot beside the
    /// Sync Now button. Transient — it belongs to the press that produced it,
    /// and the freshness line above it is what persists.
    public func message(now: Date = Date()) -> String {
        switch self {
        case .updated(let count, let generated):
            let published = generated.map { " · published \($0)" } ?? ""
            return "Updated — \(count) reflectors\(published)"
        case .notModified:
            return "Already up to date"
        // Only an automatic sync can be skipped as not-due; the button is
        // always permitted. Said plainly rather than as a refusal, because
        // from where the operator sits nothing was refused — the copy on disk
        // is current.
        case .skipped(.notDue):
            return "Already up to date"
        case .skipped(.debounced(let retryAfter)):
            return "Checked recently — try again "
                + ReflectorDirectoryStatus.remaining(until: retryAfter, now: now)
        }
    }
}
