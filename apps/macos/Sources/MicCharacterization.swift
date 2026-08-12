// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import Combine
    import Foundation

    /// Drives the Mic Analyzer window: opens the monitor mic lane, polls the live
    /// spectrum ~20 Hz, runs `characterize()` (with a short stay-silent capture),
    /// and saves/recalls named per-device profiles. Holds a weak `CallSession` (the
    /// single station owner).
    @MainActor
    final class MicCharacterization: ObservableObject {
        @Published private(set) var spectrum: [Float] = []
        @Published var selectedInput: String?
        @Published var harmonicComb = false
        /// User-entered label for the profile being saved, e.g. "fake icom".
        @Published var profileName = ""
        /// True while a stay-silent capture is in progress (drives the spinner).
        @Published private(set) var analyzing = false
        @Published private(set) var lastError: String?
        @Published private(set) var lastJSON: String?
        @Published private(set) var floorReadout: String?
        /// Notch frequencies (Hz) the saved profile would filter — shown as markers.
        @Published private(set) var detectedPeaks: [Double] = []
        @Published private(set) var saved = false

        /// Seconds of silence to buffer before characterizing.
        private static let captureSeconds: TimeInterval = 1.5
        /// Max ~0.7 s retries while the monitor warms up on a cold first open.
        private static let maxAnalyzeAttempts = 8

        /// Set by the controller; closes the analyzer window. Closing tears down the
        /// monitor via the view's `onDisappear`.
        var onClose: (() -> Void)?

        private weak var session: CallSession?
        private var timer: Timer?
        private var captureWork: DispatchWorkItem?
        private var analyzeAttempt = 0
        /// True while we hold a monitor retain, so `stop()` releases exactly once
        /// (and never under-releases if `start` failed before retaining).
        private var holdsMonitor = false

        func attach(session: CallSession) { self.session = session }

        /// Close the analyzer window (Cancel). Teardown happens in `stop()` via the
        /// view's `onDisappear`.
        func requestClose() { onClose?() }

        /// Whether there's a fresh characterization ready to save.
        var hasResult: Bool { lastJSON != nil }

        /// Save is allowed once a capture has produced a result, a device is chosen,
        /// and the profile has a name.
        var canSave: Bool {
            !analyzing && hasResult && selectedInput != nil
                && !profileName.trimmingCharacters(in: .whitespaces).isEmpty
        }

        // MARK: - Monitor lifecycle

        /// Start monitoring `input` and polling the spectrum. Safe to call repeatedly.
        func start(input: String?) {
            selectedInput = input
            // Best-effort: a cold first open can fail/race the capture device here.
            // The poll timer runs regardless — so the spectrum reflects the mic the
            // instant it starts delivering — and `analyze()` re-asserts the monitor on
            // each retry until it's live. (Don't early-return on failure; that left
            // the timer uninstalled, so the first analyze could never recover.)
            //
            // Retain (not raw start) so the lane is shared with the VOX calibration
            // meter: whichever opens it first owns the open, and neither closing pulls
            // the mic from the other. Retain only on the first start() so repeated
            // calls don't leak retains; mid-session device changes still re-assert the
            // input via the idempotent `monitorStart` in `analyze()`/`finishAnalyze()`.
            if !holdsMonitor {
                try? session?.monitorRetain(input: input)
                holdsMonitor = true
            }
            timer?.invalidate()
            timer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { [weak self] _ in
                Task { @MainActor in self?.poll() }
            }
        }

        /// Stop polling, cancel any capture, and release the monitor mic lane.
        func stop() {
            cancel()
            timer?.invalidate()
            timer = nil
            if holdsMonitor {
                try? session?.monitorRelease()
                holdsMonitor = false
            }
            spectrum = []
        }

        private func poll() {
            guard let s = try? session?.micSpectrum() else { return }
            spectrum = s
        }

        // MARK: - Analyze / cancel

        /// Begin a stay-silent capture: spin for `captureSeconds`, then characterize.
        func analyze() {
            // Re-assert the monitor lane before capturing. The very first open can
            // race the capture device / mic-permission warmup, leaving no monitor —
            // so the first characterize returned nothing. This is idempotent (the
            // engine guards double-open) and recovers a failed/late initial start.
            start(input: selectedInput)
            captureWork?.cancel()
            saved = false
            lastError = nil
            lastJSON = nil
            floorReadout = nil
            detectedPeaks = []
            analyzeAttempt = 0
            analyzing = true
            let work = DispatchWorkItem { [weak self] in self?.finishAnalyze() }
            captureWork = work
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.captureSeconds, execute: work)
        }

        private func finishAnalyze() {
            // On a cold first open the capture device hasn't started delivering yet —
            // the ring is empty and characterize returns nothing. The live spectrum
            // is the proof the mic is actually flowing (a real mic always has a noise
            // floor above the -120 dBFS empty value); retry until it's delivering or
            // we hit the attempt cap.
            let delivering = spectrum.contains { $0 > -119 }
            let json = (try? session?.characterize(harmonicComb: harmonicComb) ?? "") ?? ""
            if !delivering || json.isEmpty, analyzeAttempt < Self.maxAnalyzeAttempts {
                analyzeAttempt += 1
                // Re-attempt the monitor each retry — recovers a cold/failed initial
                // open (device warmup or the first-access mic-permission prompt).
                try? session?.monitorStart(input: selectedInput)
                let work = DispatchWorkItem { [weak self] in self?.finishAnalyze() }
                captureWork = work
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.7, execute: work)
                return
            }
            analyzing = false
            captureWork = nil
            analyzeAttempt = 0
            if json.isEmpty {
                lastError = "Couldn't analyze — try again in a quiet moment."
                return
            }
            lastJSON = json
            floorReadout = Self.readout(from: json)
            detectedPeaks = Self.peaks(from: json)
        }

        /// Clear the current analysis (detected notches, readout, name) so a
        /// different mic can be plugged in and tested. Leaves the monitor running.
        func clear() {
            cancel()
            profileName = ""
        }

        /// Abort an in-progress capture and discard any unsaved result.
        func cancel() {
            captureWork?.cancel()
            captureWork = nil
            analyzeAttempt = 0
            analyzing = false
            lastJSON = nil
            floorReadout = nil
            detectedPeaks = []
            saved = false
            lastError = nil
        }

        // MARK: - Save / default

        /// Save the captured characterization as a new named profile in the library
        /// and select it (applies it live). The new profile's id is returned via the
        /// live selection.
        func save(now: Date) {
            guard let json = lastJSON else { return }
            let trimmed = profileName.trimmingCharacters(in: .whitespaces)
            let p = MicProfile(
                name: trimmed.isEmpty ? "Untitled" : trimmed,
                deviceName: selectedInput ?? "",
                characterizationJSON: json,
                characterizedAt: now)
            session?.saveMicProfile(p)
            session?.setMicProfileSelection(id: p.id)
            saved = true
        }

        // MARK: - JSON readouts (display only — application stays opaque)

        /// Notch frequencies (Hz) from the opaque engine JSON, for the spectrum
        /// markers. The engine shape is `{ "notches": [{ "freq_hz", "q" }, …] }`.
        private static func peaks(from json: String) -> [Double] {
            guard let data = json.data(using: .utf8),
                let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                let notches = obj["notches"] as? [[String: Any]]
            else { return [] }
            return notches.compactMap { ($0["freq_hz"] as? NSNumber)?.doubleValue }
        }

        /// Best-effort human readout of the opaque JSON (noise floor + notches), purely
        /// for display. Tolerates unknown shapes.
        private static func readout(from json: String) -> String? {
            guard let data = json.data(using: .utf8),
                let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { return nil }
            var parts: [String] = []
            if let floor = (obj["noise_floor_dbfs"] as? NSNumber)?.doubleValue {
                parts.append("floor \(Int(floor)) dBFS")
            }
            let notches = peaks(from: json)
            parts.append(
                notches.isEmpty
                    ? "no tones to notch"
                    : "notch " + notches.map { String(Int($0)) }.joined(separator: ", ") + " Hz")
            return parts.isEmpty ? nil : parts.joined(separator: " · ")
        }
    }
#endif
