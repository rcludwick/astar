// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import CoreGraphics
import Foundation

/// What the engine files a picture under: the network, the node or
/// reflector, and the talker when the network names one.
struct SstvContext: Equatable {
    var network: String
    var target: String
    var talker: String?

    /// No call: pictures heard now belong to nothing.
    static let none = SstvContext(network: "", target: "", talker: nil)
}

/// SSTV pictures in heard audio: turning the decoder on and off, reading its
/// state at the poll cadence, and turning that state into the call view's
/// picture panel.
///
/// The decoder hands over a picture only once it has ended, so while one
/// airs the panel has a mode and a clock to show (`SstvLive.expectedSecs`,
/// `elapsedSecs`) and no pixels; the pixels are fetched once, when the
/// picture's end is first seen.
extension CallSession {
    /// The panel shows: a picture is airing or has just ended, and the
    /// operator has not closed the panel on it.
    public var sstvPanelVisible: Bool {
        guard let live = sstvLive else { return false }
        return live.seq != sstvDismissedSeq
    }

    /// Close the panel on the picture it shows. The next picture opens it
    /// again.
    public func dismissSstvPanel() {
        guard let seq = sstvLive?.seq, sstvDismissedSeq != seq else { return }
        sstvDismissedSeq = seq
    }

    /// `sstvSettings`' `didSet`: save, tell the engine, and drop the panel
    /// when decoding goes off.
    func sstvSettingsChanged(from old: SstvSettings) {
        guard sstvSettings != old else { return }
        sstvStore.save(sstvSettings)
        if sstvSettings.enabled != old.enabled {
            // Whatever ended while decoding was off is old news when it
            // comes back on; the first read sets a fresh baseline.
            sstvBaselineSeq = nil
            sstvFetchedSeq = nil
            lastSstvStateRead = nil
            if !sstvSettings.enabled, sstvLive != nil { sstvLive = nil }
            updateSstvClock(callUp: status == .answered)
        }
        configureSstv()
    }

    /// `reconfigure`'s SSTV step: the new engine's decoder starts off and
    /// numbers pictures from zero, so the old engine's numbers, context and
    /// picture mean nothing to it. Forget them and configure it.
    func resetSstvForNewEngine() {
        sstvBaselineSeq = nil
        sstvFetchedSeq = nil
        sstvFrameFailures = 0
        lastSstvStateRead = nil
        sstvPushedContext = .none
        if sstvLive != nil { sstvLive = nil }
        if sstvDismissedSeq != nil { sstvDismissedSeq = nil }
        configureSstv()
        updateSstvClock(callUp: status == .answered)
    }

    /// Hand the settings to the engine on the SSTV queue: this starts or
    /// stops the decoder thread, and stopping can wait for a picture to
    /// finish. Only the latest configure's answer reaches `sstvError`.
    func configureSstv() {
        sstvConfigureToken &+= 1
        let token = sstvConfigureToken
        let settings = sstvSettings
        let offset = Int32(clamping: TimeZone.current.secondsFromGMT())
        let station = self.station
        runSstvWork { [weak self] in
            var failure: String?
            do {
                try station.sstvConfigure(
                    enabled: settings.enabled, directory: settings.folder.path,
                    utcOffsetSeconds: offset)
            } catch let error as StationError {
                failure = error.message
            } catch {
                failure = String(describing: error)
            }
            sstvOnMain {
                guard let self, token == self.sstvConfigureToken else { return }
                self.sstvConfigureFailure = failure
                if self.sstvError != failure { self.sstvError = failure }
            }
        }
    }

    /// The SSTV step, from the UI poll and from the SSTV clock alike: tell
    /// the engine about a changed connection, read its state, and start or
    /// stop the clock to match.
    func advanceSstv(status: IaxStatus) {
        let callUp = status == .answered
        pushSstvContext(
            callUp
                ? SstvContext(
                    network: (activeCallNetwork ?? .allstar).displayName,
                    target: dialedNode ?? "", talker: lastHeard)
                : .none)
        pollSstvState(callUp: callUp)
        updateSstvClock(callUp: callUp)
    }

    /// One tick of the SSTV clock. While the UI poll runs it keeps `status`
    /// current; while it is stopped (the popover on another pane), the
    /// clock asks the engine whether the call is still up.
    func sstvClockTick() {
        var current = status
        if !pollRunning, let snap = try? station.readSnapshot() { current = snap.status }
        advanceSstv(status: current)
    }

    /// Run the SSTV clock while decoding is on and a call is up or a picture
    /// is still airing; stop it otherwise. The engine releases an overdue
    /// jitter-buffer hold only when its state is read, and the UI poll stops
    /// whenever the popover leaves the call card, so the reads cannot ride
    /// on that poll alone.
    func updateSstvClock(callUp: Bool) {
        let wanted = sstvSettings.enabled && (callUp || sstvAiring)
        if wanted, sstvClockCancel == nil {
            sstvClockCancel = scheduleSstvClock(0.5) { [weak self] in self?.sstvClockTick() }
        } else if !wanted, let cancel = sstvClockCancel {
            cancel()
            sstvClockCancel = nil
        }
    }

    /// A picture is airing and has not outlived its deadline.
    var sstvAiring: Bool {
        guard let live = sstvLive, !live.finished else { return false }
        return !sstvOverdue(live)
    }

    /// The engine's hold deadline: a picture still "airing" half again its
    /// own length plus 5 s after it started has a wedged decoder behind it,
    /// not a picture. One with no length reported never goes overdue.
    private func sstvOverdue(_ live: SstvLive) -> Bool {
        guard let expected = live.expectedSecs else { return false }
        let limit = min(max(expected, 0) * 1.5 + 5, 86_400)
        return now() > live.startedAt.addingTimeInterval(limit)
    }

    /// An airing picture past its deadline, shown as lost.
    private func expiredIfOverdue(_ live: SstvLive) -> SstvLive {
        guard !live.finished, sstvOverdue(live) else { return live }
        var lost = live
        lost.finished = true
        lost.lost = true
        lost.expectedSecs = nil
        lost.elapsedSecs = nil
        return lost
    }

    /// Tell the engine what a picture starting now belongs to, when that
    /// changed. Cheap, so it runs on the main thread. Recorded only once the
    /// engine took it, so a failed push is tried again on the next step.
    func pushSstvContext(_ context: SstvContext) {
        guard context != sstvPushedContext else { return }
        do {
            try station.sstvSetContext(
                network: context.network, target: context.target, talker: context.talker)
            sstvPushedContext = context
        } catch {}
    }

    /// Read the decoder at most every 0.5 s while decoding is on and a call
    /// is up, or a picture is still airing after it. Not only while the
    /// panel shows: each read also lets the engine release a jitter-buffer
    /// hold whose picture never said it ended.
    private func pollSstvState(callUp: Bool) {
        guard sstvSettings.enabled else { return }
        if let live = sstvLive {
            let expired = expiredIfOverdue(live)
            if expired != live { sstvLive = expired }
        }
        guard callUp || sstvAiring else { return }
        let t = now()
        if let last = lastSstvStateRead, t.timeIntervalSince(last) < 0.5 { return }
        lastSstvStateRead = t
        guard let state = try? station.sstvState() else { return }
        applySstvState(state)
    }

    private func applySstvState(_ state: SstvState) {
        let error = state.error ?? sstvConfigureFailure
        if sstvError != error { sstvError = error }

        let baseline: UInt64
        if let known = sstvBaselineSeq {
            baseline = known
        } else {
            // The first read: an airing picture is news, anything that
            // already ended is not.
            baseline = state.receiving ? state.seq &- 1 : state.seq
            sstvBaselineSeq = baseline
        }

        if state.receiving, state.seq > baseline {
            let previous = sstvLive?.seq == state.seq ? sstvLive : nil
            let live = SstvLive(
                seq: state.seq, mode: state.mode ?? "SSTV", talker: state.talker,
                startedAt: state.startedMS.map(Self.date(unixMS:)) ?? previous?.startedAt
                    ?? now(),
                lines: state.lines, height: state.height, width: state.width,
                expectedSecs: state.expectedSecs, elapsedSecs: state.elapsedSecs)
            // A wedged decoder keeps reporting the picture; past its
            // deadline it shows as lost rather than stuck mid-way.
            let shown = expiredIfOverdue(live)
            if sstvLive != shown { sstvLive = shown }
        } else if let last = state.last, last.seq > baseline,
            sstvLive?.seq != last.seq || sstvLive?.finished != true
                || (sstvLive?.lost == true && !last.lost)
                || (!last.lost && sstvFetchedSeq != last.seq)
        {
            sstvLive = finishedSstvLive(last)
        }
    }

    /// The panel's picture for one that ended: its file, and its pixels when
    /// it was kept. The pixels are read once per picture.
    private func finishedSstvLive(_ row: SstvPictureRow) -> SstvLive {
        let previous = sstvLive?.seq == row.seq ? sstvLive : nil
        var lines = previous?.lines ?? 0
        var height = previous?.height ?? 0
        var width = previous?.width ?? 0
        var image = previous?.image
        if !row.lost, sstvFetchedSeq != row.seq {
            do {
                let frame = try station.sstvFrame()
                // Marked fetched whatever comes back: a frame for another
                // picture means a newer one ended, and the next read
                // reports it.
                sstvFetchedSeq = row.seq
                sstvFrameFailures = 0
                if let frame, frame.seq == row.seq {
                    image = SstvLive.image(from: frame)
                    lines = frame.lines
                    height = frame.height
                    width = frame.width
                }
            } catch {
                // A failed read is tried again on the next tick; a few in a
                // row and the picture shows without its pixels (its PNG is
                // on disk either way).
                sstvFrameFailures += 1
                if sstvFrameFailures >= Self.sstvFrameAttempts {
                    sstvFetchedSeq = row.seq
                    sstvFrameFailures = 0
                }
            }
        }
        return SstvLive(
            seq: row.seq, mode: row.mode, talker: row.talker,
            startedAt: Self.date(unixMS: row.timeMS), lines: lines, height: height,
            width: width, finished: true, lost: row.lost, partial: row.partial,
            file: row.file.map { URL(fileURLWithPath: $0, isDirectory: false) }, image: image)
    }

    /// How many times a finished picture's pixels are asked for before
    /// giving up on them.
    static let sstvFrameAttempts = 3

    private static func date(unixMS: UInt64) -> Date {
        Date(timeIntervalSince1970: Double(unixMS) / 1000)
    }
}

/// Run `body` on the main thread: now if already there, else soon.
private func sstvOnMain(_ body: @escaping () -> Void) {
    if Thread.isMainThread { body() } else { DispatchQueue.main.async(execute: body) }
}
