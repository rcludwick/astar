// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// Reads and writes config archives against the app's real stores (astar-b52e).
///
/// Lives in AstarCore, not the app: this is the half that touches every store
/// and can silently corrupt a working setup, so it has to be reachable by the
/// test suite. The app keeps only the save/open panels.
public enum ConfigTransfer {

    /// `astar-config-YYYY-MM-DD.astarconfig` — plain JSON under a distinct
    /// extension so the open panel can filter to it.
    public static let fileExtension = "astarconfig"

    public static func suggestedFilename(date: Date = Date()) -> String {
        let stamp = ISO8601DateFormatter()
        stamp.formatOptions = [.withFullDate]
        return "astar-config-\(stamp.string(from: date)).\(fileExtension)"
    }

    // MARK: - Gathering

    public static func sources(
        session: CallSession,
        setupStore: SetupStore,
        profileStore: MicProfileStore,
        defaults: UserDefaults
    ) -> ConfigArchive.Sources {
        // dictionaryRepresentation() also returns NSGlobalDomain noise;
        // ConfigArchive filters by key prefix, so handing it over whole is safe
        // and keeps this side free of a key list that would go stale.
        ConfigArchive.Sources(
            defaults: defaults.dictionaryRepresentation(),
            setups: setupStore.all(),
            micProfiles: profileStore.all(),
            selectedSetupID: setupStore.loadSelectedID(),
            defaultSetupID: setupStore.loadDefaultID(),
            directory: session.directoryAll(),
            callsign: session.operatorCallsign.isEmpty ? nil : session.operatorCallsign,
            radioID: session.dmrRadioID.isEmpty ? nil : session.dmrRadioID,
            nxdnRadioID: session.nxdnRadioID.isEmpty ? nil : session.nxdnRadioID)
    }

    public static func archive(
        sections: Set<ConfigSection>,
        session: CallSession,
        setupStore: SetupStore,
        profileStore: MicProfileStore,
        defaults: UserDefaults,
        appVersion: String? = nil
    ) -> ConfigArchive {
        ConfigArchive.make(
            sections: sections,
            from: sources(
                session: session, setupStore: setupStore,
                profileStore: profileStore, defaults: defaults),
            appVersion: appVersion)
    }

    // MARK: - Applying

    /// Merge an archive into these stores. Additive only — nothing is removed.
    /// Returns what changed, counted BEFORE anything is written.
    @discardableResult
    public static func apply(
        _ archive: ConfigArchive,
        session: CallSession,
        setupStore: SetupStore,
        profileStore: MicProfileStore,
        defaults: UserDefaults
    ) -> ConfigMerge.Summary {
        let summary = ConfigMerge.summarize(
            archive,
            existingSetups: setupStore.all(),
            existingProfiles: profileStore.all(),
            existingDirectory: session.directoryAll())

        if let rigs = archive.rigs {
            // Mic profiles first: a Setup references one by id, so landing the
            // configs first would leave them briefly pointing at nothing.
            for profile in rigs.micProfiles { profileStore.save(profile) }
            let merged = ConfigMerge.byID(incoming: rigs.setups, into: setupStore.all()).merged
            for setup in merged where setup.id != SystemDefaultSetup.id {
                setupStore.save(setup)
            }
            // Adopt a default only if it exists here afterwards, or the app
            // starts up pointing at a config it does not have.
            if let wanted = rigs.defaultSetupID,
                setupStore.all().contains(where: { $0.id == wanted })
            {
                setupStore.saveDefaultID(wanted)
            }
        }

        if let incoming = archive.directory {
            let merged = ConfigMerge.directory(incoming: incoming, into: session.directoryAll())
            for entry in merged.merged { session.directoryUpsert(entry) }
        }

        for (key, value) in archive.settings ?? [:] {
            defaults.set(value.defaultsValue, forKey: key)
        }
        for (key, value) in archive.interface ?? [:] {
            defaults.set(value.defaultsValue, forKey: key)
        }
        if let callsign = archive.callsign, !callsign.isEmpty {
            session.operatorCallsign = callsign
        }
        if let radioID = archive.radioID, !radioID.isEmpty {
            session.dmrRadioID = radioID
        }
        if let nxdnRadioID = archive.nxdnRadioID, !nxdnRadioID.isEmpty {
            session.nxdnRadioID = nxdnRadioID
        }
        return summary
    }
}
