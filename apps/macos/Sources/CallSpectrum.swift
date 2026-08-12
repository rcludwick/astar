// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import Combine
    import Foundation

    /// The app-global "Spectrum decay" preference (astar-68a6): a single dB/second
    /// value that drives BOTH the engine peak-hold decay (via
    /// `CallSession.setSpectrumDecay`) and astar's inactive-trace fade in
    /// `CallSpectrum`, so the two stay in lockstep. Stored as a global UserDefaults
    /// key (`@AppStorage`), NOT per-Setup / per-mic-profile. Default 100 dB/s.
    enum SpectrumDecayPref {
        /// Global UserDefaults key. The Settings slider binds this via `@AppStorage`.
        static let key = "ui.spectrumDecayDbPerSec"
        /// Default on a fresh install (matches the engine's own default,
        /// iax-fbea: 800 dB/s — full-scale to floor in ~0.15 s).
        static let defaultValue: Double = 800
        /// Slider bounds + step. (Engine clamps at 1600 dB/s.)
        static let range: ClosedRange<Double> = 50...1600
        static let step: Double = 25

        /// Read the persisted value, falling back to the default when unset (the
        /// key is absent on first launch → `double(forKey:)` returns 0).
        static func current(_ defaults: UserDefaults = .standard) -> Double {
            defaults.object(forKey: key) == nil ? defaultValue : defaults.double(forKey: key)
        }
    }

    /// Drives the in-call "Levels & Spectrum" overlaid FFT (astar-8b5b): polls the
    /// live TX/RX voice-band spectra ~20 Hz, but ONLY while the disclosure is open
    /// AND a call is connected. Mirrors `MicCharacterization`'s poll loop. Zero cost
    /// when idle: `stop()` invalidates the timer and clears the published bins so the
    /// canvas draws nothing on collapse / disconnect / disappear.
    @MainActor
    final class CallSpectrum: ObservableObject {
        @Published private(set) var tx: [Float] = []
        @Published private(set) var rx: [Float] = []

        private weak var session: CallSession?
        private var timer: Timer?

        /// ~20 Hz, matching the mic-analyzer poll and the engine's peak-hold cadence.
        private static let interval: TimeInterval = 0.05

        func attach(session: CallSession) { self.session = session }

        /// Begin polling the TX/RX spectra. Safe to call repeatedly (re-arms the
        /// timer). Callers gate this on "expanded AND connected".
        func start() {
            timer?.invalidate()
            timer = Timer.scheduledTimer(withTimeInterval: Self.interval, repeats: true) {
                [weak self] _ in
                Task { @MainActor in self?.poll() }
            }
        }

        /// Stop polling and clear the bins so the overlaid canvas goes blank.
        func stop() {
            timer?.invalidate()
            timer = nil
            tx = []
            rx = []
        }

        private func poll() {
            guard let session else {
                tx = []
                rx = []
                return
            }
            // TX: the engine HOLDS the tx spectrum when unkeyed (the tap is only fed
            // while transmitting), so gate on the clean `ptt` boolean and fade it out
            // here when unkeyed — at the app-global "Spectrum decay" rate (astar-68a6)
            // so the fade matches the engine's active decay.
            let fadePerTick = Float(SpectrumDecayPref.current()) * Float(Self.interval)
            tx = session.ptt ? ((try? session.txSpectrum()) ?? []) : Self.decayed(tx, fadePerTick)
            // RX: the engine now keeps the rx analyzer fed across underruns
            // (iax-f53f), so the live spectrum decays to the floor on its own when
            // the far end stops. Just show it live and hide it once it's all at the
            // floor — no gating or fade hacks, so no stutter / freeze / abrupt stop.
            rx = Self.hideIfFloor((try? session.rxSpectrum()) ?? [])
        }

        /// `bins`, or `[]` once they've all reached the floor so the canvas hides the
        /// trace. Empty in → empty out.
        static func hideIfFloor(_ bins: [Float]) -> [Float] {
            bins.allSatisfy { $0 <= floorDBFS } ? [] : bins
        }

        /// Floor (dBFS) a faded bin clamps to — matches the engine's
        /// `SPECTRUM_FLOOR_DBFS`. A trace of all-floor bins reads as silent.
        private static let floorDBFS: Float = -120

        /// Fade `bins` one tick toward the floor at `fadePerTick` dB/tick (the
        /// configured dB/s × the poll interval, so it tracks the engine's active
        /// decay); returns `[]` once every bin has reached the floor so the canvas
        /// stops drawing the trace.
        static func decayed(_ bins: [Float], _ fadePerTick: Float) -> [Float] {
            guard !bins.isEmpty else { return [] }
            let next = bins.map { max(floorDBFS, $0 - fadePerTick) }
            return next.allSatisfy { $0 <= floorDBFS } ? [] : next
        }
    }
#endif
