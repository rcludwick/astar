// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

// MARK: - The record

/// One AMBE vocoder stick the operator has added in Settings → Vocoders.
///
/// The engine opens a vocoder port only when its USB serial number is on this
/// list. USB identity alone cannot tell a DVstick from a radio's CAT/PTT
/// cable — both enumerate as FTDI FT-X parts — and opening a tty raises
/// DTR/RTS, which on a cable can key a transmitter. So the list is built one
/// stick at a time, by the operator, through enrolment.
///
/// Per-machine hardware, like an audio device's identity: it is stored in
/// this Mac's preferences under `vocoder.trusted` and is never written into an
/// exported `.astarconfig` (see `ConfigArchive`).
public struct TrustedVocoder: Codable, Equatable, Identifiable, Sendable {
    /// The USB serial number — the identity trust is recorded against.
    public var serial: String
    /// PRODID as the chip reported it at enrolment (e.g. `AMBE3000R`).
    public var prodid: String?
    /// Model label from enrolment (e.g. "DVstick 30 / ThumbDV-class").
    public var model: String?
    /// When this Mac last saw the stick attached. `nil` until it has been.
    public var lastSeen: Date?

    public var id: String { serial }

    public init(serial: String, prodid: String?, model: String?, lastSeen: Date?) {
        self.serial = serial
        self.prodid = prodid
        self.model = model
        self.lastSeen = lastSeen
    }

    /// Whether `serial` is one the engine can be handed safely.
    ///
    /// The FFI reads the list as comma-, whitespace- or newline-separated, so
    /// a "serial" carrying any of those would arrive as two serials — a way
    /// for a hand-edited preferences domain to widen the list. Real FTDI
    /// serials are short printable ASCII; anything else is refused.
    public static func isAcceptable(serial: String) -> Bool {
        guard !serial.isEmpty, serial.count <= 64 else { return false }
        return serial.unicodeScalars.allSatisfy { s in
            s.isASCII && s.value > 0x20 && s.value < 0x7F && s != ","
        }
    }

    /// Drop unacceptable serials and duplicates, keeping the first of each.
    static func sanitized(_ list: [TrustedVocoder]) -> [TrustedVocoder] {
        var seen = Set<String>()
        var out = [TrustedVocoder]()
        for var v in list {
            v.serial = v.serial.trimmingCharacters(in: .whitespacesAndNewlines)
            guard isAcceptable(serial: v.serial), seen.insert(v.serial).inserted else { continue }
            out.append(v)
        }
        return out
    }
}

// MARK: - Storage

/// Where the trust list lives. A protocol so the view-model is testable
/// without a preferences domain.
public protocol VocoderTrustStore: AnyObject {
    func load() -> [TrustedVocoder]
    func save(_ vocoders: [TrustedVocoder])
}

/// The trust list as JSON under `vocoder.trusted`.
///
/// JSON `Data`, not a plist array: `ConfigArchive` exports only scalars, so
/// even a future prefix change could not carry this key into a file. The key
/// is excluded by name as well (see `ConfigArchive.machineLocalPrefixes`).
public final class UserDefaultsVocoderTrustStore: VocoderTrustStore {
    public static let key = "vocoder.trusted"
    private let defaults: UserDefaults

    public init(_ defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// Anything unreadable reads as nothing trusted — the only safe direction.
    public func load() -> [TrustedVocoder] {
        guard let data = defaults.data(forKey: Self.key) else { return [] }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        guard let list = try? decoder.decode([TrustedVocoder].self, from: data) else { return [] }
        return TrustedVocoder.sanitized(list)
    }

    public func save(_ vocoders: [TrustedVocoder]) {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        guard let data = try? encoder.encode(vocoders) else { return }
        defaults.set(data, forKey: Self.key)
    }

    /// The serials alone, for the station config at launch.
    public func serials() -> [String] { load().map(\.serial) }
}

// MARK: - D-Star vocoder choice

/// Where D-Star's vocoder choice lives: `vocoder.dstar` in this Mac's
/// preferences, `"dongle"` or `"software"` (the engine's own spelling).
///
/// Machine-local, like the trust list: the whole `vocoder.` prefix is kept out
/// of exported configs (`ConfigArchive.machineLocalPrefixes`), which is right
/// here too — whether the software vocoder exists depends on the build, not
/// on the operator. Anything absent or unreadable reads as the dongle, the
/// default.
public enum DStarVocoderPreference {
    public static let key = "vocoder.dstar"

    public static func load(_ defaults: UserDefaults = .standard) -> DStarVocoder {
        defaults.string(forKey: key).flatMap(DStarVocoder.init(rawValue:)) ?? .dongle
    }

    public static func save(_ vocoder: DStarVocoder, _ defaults: UserDefaults = .standard) {
        defaults.set(vocoder.rawValue, forKey: key)
    }
}

/// The modes Settings → Vocoders lists, and what each may use. Pure, so the
/// rule — a software vocoder for D-Star only — is pinned by tests rather than
/// buried in a view.
public enum VocoderModeRow: String, CaseIterable, Identifiable, Sendable {
    case dstar, ysf, nxdn, dmr

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .dstar: return "D-Star"
        case .ysf: return "System Fusion"
        case .nxdn: return "NXDN"
        case .dmr: return "DMR"
        }
    }

    /// Only D-Star's AMBE 3600×2400 is out of patent, so only D-Star may
    /// offer the software vocoder.
    public var allowsSoftwareVocoder: Bool { self == .dstar }

    /// Why a mode is dongle-only, shown beside it.
    public static let ambePlus2Reason =
        "Dongle only. These modes use AMBE+2, which is covered by US patent "
        + "8,359,197 until 2028-05-20, so astar has no software vocoder for them."

    /// Why Software is greyed out in a build without it.
    public static let softwareNotInBuild =
        "This build of astar doesn’t include the software vocoder."
}

// MARK: - Engine seam

/// The two engine calls Settings → Vocoders makes. A protocol so tests run
/// against a fake: **the real `enrol` opens a serial port.**
public protocol VocoderEnrolling: Sendable {
    /// Vocoder-class USB ports by serial. Enumeration only; opens nothing.
    func candidates() -> [VocoderCandidate]
    /// Open the port, drive DTR/RTS low, ask for PRODID, close, classify.
    /// Blocks up to about two seconds. Only ever from an explicit user action.
    func enrol(serial: String) throws -> VocoderEnrolment
}

/// The engine itself, through the AstarStation binding.
public struct LiveVocoderEnroller: VocoderEnrolling {
    public init() {}
    public func candidates() -> [VocoderCandidate] { Station.listVocoderCandidates() }
    public func enrol(serial: String) throws -> VocoderEnrolment {
        try Station.enrolVocoder(serial: serial)
    }
}

// MARK: - View-model

/// Settings → Vocoders: the trusted sticks, what is attached now, and the
/// add/remove actions.
///
/// Listing is cheap and opens nothing, so `refresh()` may run whenever the
/// pane appears. Enrolment is the one call that opens a port that is not yet
/// trusted; it runs only from `enrol(_:)`, which the UI calls only after the
/// operator has confirmed the CAT-cable warning.
@MainActor
public final class VocoderSettings: ObservableObject {
    @Published public private(set) var trusted: [TrustedVocoder]
    /// Every vocoder-class port attached at the last `refresh()`.
    @Published public private(set) var candidates: [VocoderCandidate] = []
    /// The serial being enrolled right now, if any. One at a time.
    @Published public private(set) var enrollingSerial: String?

    private let store: VocoderTrustStore
    private let enroller: VocoderEnrolling
    private let now: () -> Date
    private let onTrustChanged: ([String]) -> Void

    /// D-Star's vocoder: the dongle unless the operator chose software.
    @Published public private(set) var dstarVocoder: DStarVocoder
    /// Whether this build contains the software D-Star vocoder. When it does
    /// not, Software is offered greyed out with the reason.
    public let softwareVocoderCompiled: Bool
    private let dstarDefaults: UserDefaults
    private let onDStarVocoderChanged: (DStarVocoder) -> Void

    public init(
        store: VocoderTrustStore = UserDefaultsVocoderTrustStore(),
        enroller: VocoderEnrolling = LiveVocoderEnroller(),
        now: @escaping () -> Date = Date.init,
        onTrustChanged: @escaping ([String]) -> Void = { _ in },
        dstarDefaults: UserDefaults = .standard,
        softwareVocoderCompiled: Bool = Station.dstarSoftwareVocoderCompiled,
        onDStarVocoderChanged: @escaping (DStarVocoder) -> Void = { _ in }
    ) {
        self.store = store
        self.enroller = enroller
        self.now = now
        self.onTrustChanged = onTrustChanged
        self.trusted = TrustedVocoder.sanitized(store.load())
        self.dstarDefaults = dstarDefaults
        self.softwareVocoderCompiled = softwareVocoderCompiled
        self.onDStarVocoderChanged = onDStarVocoderChanged
        self.dstarVocoder = DStarVocoderPreference.load(dstarDefaults)
    }

    /// The software choice is stored, but this build cannot honour it (a
    /// preference written by a build that had it). D-Star connects will be
    /// refused until the operator picks the dongle; the pane says so.
    public var softwareChosenButMissing: Bool {
        dstarVocoder == .software && !softwareVocoderCompiled
    }

    /// Choose D-Star's vocoder. Software is refused in a build without it, so
    /// the picker can never store a choice that cannot work.
    public func setDStarVocoder(_ vocoder: DStarVocoder) {
        guard vocoder != dstarVocoder else { return }
        if vocoder == .software && !softwareVocoderCompiled { return }
        dstarVocoder = vocoder
        DStarVocoderPreference.save(vocoder, dstarDefaults)
        onDStarVocoderChanged(vocoder)
    }

    /// The serials, in the order they were added.
    public var serials: [String] { trusted.map(\.serial) }

    /// Attached ports not yet trusted — what "Add vocoder…" offers.
    public var addableCandidates: [VocoderCandidate] {
        let known = Set(serials)
        return candidates.filter { !known.contains($0.serial) }
    }

    /// Nothing trusted, but something vocoder-shaped is plugged in: the case
    /// an upgrade from a scanning build lands in. Drives a gentle hint.
    public var suggestsAdding: Bool { trusted.isEmpty && !candidates.isEmpty }

    public func isConnected(serial: String) -> Bool {
        candidates.contains { $0.serial == serial }
    }

    public func port(serial: String) -> String? {
        candidates.first { $0.serial == serial }?.port
    }

    /// Re-list attached ports and stamp "last seen" on trusted sticks that
    /// are present. Opens nothing.
    public func refresh() {
        let listed = enroller.candidates()
        if listed != candidates { candidates = listed }
        let present = Set(listed.map(\.serial))
        guard trusted.contains(where: { present.contains($0.serial) }) else { return }
        let stamp = now()
        trusted = trusted.map { v in
            var v = v
            if present.contains(v.serial) { v.lastSeen = stamp }
            return v
        }
        store.save(trusted)
    }

    /// Enrol `candidate`. Returns the engine's outcome, or `nil` when another
    /// enrolment is already running. Trusts the stick only on `.trusted`.
    @discardableResult
    public func enrol(_ candidate: VocoderCandidate) async -> VocoderEnrolment? {
        guard enrollingSerial == nil else { return nil }
        guard TrustedVocoder.isAcceptable(serial: candidate.serial) else {
            return VocoderEnrolment(
                result: .notFound, serial: candidate.serial, port: candidate.port, prodid: nil,
                model: nil, message: "That device has no usable USB serial number.")
        }
        enrollingSerial = candidate.serial
        defer { enrollingSerial = nil }
        let enroller = self.enroller
        let serial = candidate.serial
        let outcome: VocoderEnrolment
        do {
            outcome = try await Task.detached(priority: .userInitiated) {
                try enroller.enrol(serial: serial)
            }.value
        } catch {
            outcome = VocoderEnrolment(
                result: .openFailed, serial: serial, port: candidate.port, prodid: nil,
                model: nil, message: "Enrolment failed. Nothing was added.")
        }
        if outcome.isTrusted, !serials.contains(serial) {
            trusted.append(
                TrustedVocoder(
                    serial: serial, prodid: outcome.prodid, model: outcome.model,
                    lastSeen: now()))
            store.save(trusted)
            onTrustChanged(serials)
        }
        return outcome
    }

    /// Stop trusting `serial`. The engine will not open it again.
    public func remove(serial: String) {
        let before = trusted.count
        trusted.removeAll { $0.serial == serial }
        guard trusted.count != before else { return }
        store.save(trusted)
        onTrustChanged(serials)
    }
}

// MARK: - Presentation

/// A headline, detail and symbol for an enrolment outcome. Pure, so the
/// wording is pinned by tests rather than buried in a view.
public struct VocoderOutcomeSummary: Equatable {
    public let title: String
    public let detail: String
    public let systemImage: String
    public let isSuccess: Bool

    public init(_ outcome: VocoderEnrolment) {
        detail = outcome.message
        isSuccess = outcome.isTrusted
        switch outcome.result {
        case .trusted:
            title = "Vocoder added"
            systemImage = "checkmark.circle.fill"
        case .unsupported:
            title = "DVstick 33 isn’t supported yet"
            systemImage = "clock.badge.exclamationmark"
        case .wrongDevice:
            title = "Not a supported vocoder"
            systemImage = "xmark.octagon"
        case .noAnswer:
            title = "Not a vocoder"
            systemImage = "exclamationmark.triangle.fill"
        case .openFailed:
            title = "Couldn’t open the device"
            systemImage = "exclamationmark.triangle.fill"
        case .notFound:
            title = "Device not found"
            systemImage = "questionmark.circle"
        case .notCompiled:
            title = "No vocoder support"
            systemImage = "xmark.octagon"
        }
    }
}

/// Whether a connect failure should carry an "Open Settings → Vocoders"
/// button: true exactly when the failure is the engine's (or the session's)
/// "no trusted vocoder" refusal.
public enum VocoderSetupPrompt {
    public static func offersSettings(for message: String?) -> Bool {
        guard let message else { return false }
        return message.localizedCaseInsensitiveContains("No trusted vocoder")
    }

    /// D-Star's "no trusted vocoder" refusal in a build with the software
    /// vocoder: the shared sentence, plus the other way to get D-Star going.
    /// The same words as the engine's `NO_TRUSTED_VOCODER_DSTAR`.
    public static let dstarNoTrustedVocoderMessage =
        Station.noTrustedVocoderMessage
        + ", or switch D-Star to the software vocoder there"

    /// Whether a connect failure should also carry "Use the Software Vocoder
    /// for D-Star": true exactly for D-Star's own no-trusted-vocoder refusal,
    /// which only a build with the software vocoder produces.
    public static func offersSoftwareSwitch(for message: String?) -> Bool {
        guard let message else { return false }
        return message.localizedCaseInsensitiveContains("switch D-Star to the software vocoder")
    }

    /// The refusal for `network` when nothing is trusted. D-Star names the
    /// software vocoder when this build has it; the AMBE+2 networks never do.
    public static func noTrustedVocoderMessage(
        for network: Network, softwareCompiled: Bool = Station.dstarSoftwareVocoderCompiled
    ) -> String {
        if network == .dstar && softwareCompiled {
            return dstarNoTrustedVocoderMessage + "."
        }
        return Station.noTrustedVocoderMessage + "."
    }
}
