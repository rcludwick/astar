// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import Combine
    import Foundation

    /// Drives the Mic Analyzer pane: opens the monitor mic lane, polls the live
    /// spectrum ~20 Hz, runs `characterize()` (with a short stay-silent capture),
    /// and saves/recalls named per-device profiles. Holds a weak `CallSession` (the
    /// single station owner).
    @MainActor
    final class MicCharacterization: ObservableObject {
        @Published private(set) var spectrum: [Float] = []
        @Published var selectedInput: String?
        @Published var harmonicComb = false
        /// The ABSOLUTE level (dBFS) a bin must exceed to be notched. Analyze
        /// passes it to the characterizer, and the canvas draws it as a flat line
        /// on the same axis the spectrum is drawn on — the engine measures a bin
        /// in the display's own sinusoid normalisation, so a tone that shows above
        /// the line is above it for the detector too. Nothing to estimate: the two
        /// sides read one scale.
        @Published var thresholdDbfs: Double = -60
        /// The scan-band median of the live spectrum, averaged over the last
        /// second of polls (20 at 50 ms), so the background line the canvas draws
        /// from it holds still instead of jittering with the peak hold. This is
        /// the ambient noise the mic is sitting in — informational, and no longer
        /// part of the threshold, which is now absolute. `nil` until the mic
        /// delivers bins; reset when the device changes.
        @Published private(set) var floorMedianDb: Float?
        private var floorMean = RollingMean(window: 20)
        /// The characterizer scans 100–3800 Hz; the background line reads the same
        /// band of the DISPLAY spectrum. It is a display-side reading: the live
        /// analyzer's bins are coarser (2048-point FFT, max-folded into log bins,
        /// peak-held) than the detector's own FFT, so for broadband noise this
        /// line reads several dB above the detector's floor. The threshold line
        /// is exact for tones; this one is a guide.
        private static let scanLoHz = 100.0, scanHiHz = 3800.0
        /// User-entered label for the profile being saved, e.g. "fake icom".
        @Published var profileName = ""
        /// Set to ask the view to move keyboard focus to the name field, and
        /// claimed (cleared) by the view once it has. A consumed flag rather than
        /// a `.onChange`-only signal: `startNew` sets this and then shows the
        /// pane, which can mount the view synchronously in the same call — the
        /// view is BORN with the flag already true, so `.onChange` alone would
        /// never see it change. `.onAppear` claiming it too is what catches that
        /// case; mirrors `SetupController.focusNewID`.
        @Published var nameFocusPending = false
        /// True while a stay-silent capture is in progress (drives the spinner).
        @Published private(set) var analyzing = false
        @Published private(set) var lastError: String?
        @Published private(set) var lastJSON: String?
        @Published private(set) var floorReadout: String?
        /// Notch frequencies (Hz) the saved profile would filter — shown as markers.
        @Published private(set) var detectedPeaks: [Double] = []
        @Published private(set) var saved = false
        /// Whether the profile the last `save` wrote filters nothing — a clean mic
        /// at this threshold. Drives the confirmation copy; a legitimate result,
        /// not a failure.
        @Published private(set) var savedPassThrough = false

        /// Seconds of silence to buffer before characterizing.
        private static let captureSeconds: TimeInterval = 1.5
        /// Max ~0.7 s retries while the monitor warms up on a cold first open.
        private static let maxAnalyzeAttempts = 8

        /// Set by the controller; leaves the analyzer pane (`AppNavigation.goBack`).
        /// Leaving tears down the monitor via the view's `onDisappear`.
        var onClose: (() -> Void)?

        private weak var session: CallSession?
        private var timer: Timer?
        private var captureWork: DispatchWorkItem?
        private var analyzeAttempt = 0
        /// True while we hold a monitor retain, so `stop()` releases exactly once
        /// (and never under-releases if `start` failed before retaining).
        private var holdsMonitor = false

        func attach(session: CallSession) { self.session = session }

        /// Leave the analyzer pane. Teardown happens in `stop()` via the view's
        /// `onDisappear`. The pane's own way out is the header's Back chevron; this
        /// is the model-level equivalent for anything that has only the model.
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

        /// Start monitoring `input` and polling the spectrum. Safe to call repeatedly;
        /// calling it with a different device switches the mic being monitored.
        func start(input: String?) {
            // A different microphone has a different floor: start its average fresh
            // rather than dragging the previous device's second along.
            if input != selectedInput {
                floorMean.reset()
                floorMedianDb = nil
            }
            selectedInput = input
            // Best-effort: a cold first open can fail/race the capture device here.
            // The poll timer runs regardless — so the spectrum reflects the mic the
            // instant it starts delivering — and `analyze()` re-asserts the monitor on
            // each retry until it's live. (Don't early-return on failure; that left
            // the timer uninstalled, so the first analyze could never recover.)
            //
            // Retain (not raw start) on the FIRST start so the lane is shared with the
            // VOX calibration meter: whichever opens it first owns the open, and
            // neither closing pulls the mic from the other. One retain per start()
            // would leak holds, so once we hold one, a later start() asks the engine
            // to move the lane to `input` instead — that is what makes the device
            // picker change the stream and not just the label. Naming the same device
            // is a no-op down in the station.
            if !holdsMonitor {
                try? session?.monitorRetain(input: input)
                holdsMonitor = true
            } else {
                try? session?.monitorStart(input: input)
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
            floorMean.reset()
            floorMedianDb = nil
        }

        private func poll() {
            guard let s = try? session?.micSpectrum() else { return }
            spectrum = s
            // Only average bins the mic is actually delivering. Before the
            // capture device starts (or on a cold open) every bin reads the
            // display's -120 dBFS empty value; folding those into the mean would
            // drag the background line down to the floor for a second after the
            // audio arrives, and park it on the frequency ticks until then. The
            // same test `finishAnalyze` uses to decide the mic is live.
            guard s.contains(where: { $0 > -119 }) else { return }
            if let lo = SpectrumAxis.binFraction(Self.scanLoHz, binCount: s.count),
                let hi = SpectrumAxis.binFraction(Self.scanHiHz, binCount: s.count),
                let median = SpectrumFloor.scanBandMedian(s, lowFraction: lo, highFraction: hi)
            {
                floorMean.push(median)
                if floorMedianDb != floorMean.value { floorMedianDb = floorMean.value }
            }
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
            savedPassThrough = false
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
            // The operator's slider level — the same number the canvas draws its
            // threshold line at, on the same scale, so the detected notches are
            // exactly the peaks standing above the line they can see. The relative
            // margin is left unset: the absolute threshold is the one that decides.
            let json =
                (try? session?.characterize(
                    harmonicComb: harmonicComb, peakMarginDb: nil,
                    thresholdDbfs: Float(thresholdDbfs)) ?? "")
                ?? ""
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

        /// Ask the view to move keyboard focus to the name field — used by every
        /// "+" entry point, since the field itself lives in `MicAnalyzerView`.
        func requestNameFocus() { nameFocusPending = true }

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
            savedPassThrough = false
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
            savedPassThrough = p.isPassThrough
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
            // "broadband" distinguishes this from the canvas's per-bin lines: this
            // is one wideband RMS number over the whole capture.
            if let floor = (obj["noise_floor_dbfs"] as? NSNumber)?.doubleValue {
                parts.append("broadband floor \(Int(floor)) dBFS")
            }
            let notches = peaks(from: json)
            parts.append(
                notches.isEmpty
                    ? "nothing clears the threshold — pass-through"
                    : "notch " + notches.map { String(Int($0)) }.joined(separator: ", ") + " Hz")
            return parts.isEmpty ? nil : parts.joined(separator: " · ")
        }
    }
#endif
