// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AppKit
    import AstarCore
    import Combine

    /// Posts VoiceOver announcements for astar's async call-session events
    /// (astar-b167, accessibility-audit finding F6). Owned for the app's full
    /// lifetime — a sibling of `StatusItemController` in `AppDelegate` — so
    /// announcements fire whether or not the popover window is open, the same
    /// reasoning `session.start()`'s baseline poll already uses.
    ///
    /// Thin by design: ALL wording and edge-detection logic lives in
    /// `AccessibilityAnnouncementPlanner` (AstarCore, fully unit-tested). This
    /// class only (1) turns each `CallSession` published property's Combine
    /// sink into a planner call, (2) runs the 1 s-while-keyed talk-timer tick,
    /// and (3) posts the resulting `Announcement`s via `NSAccessibility`.
    @MainActor
    final class AccessibilityAnnouncer {
        private let planner = AccessibilityAnnouncementPlanner()
        private var cancellables = Set<AnyCancellable>()
        /// Ticks the talk-timer phase once a second — only while keyed
        /// (`session.$keyedSince != nil`), so this never burns CPU at rest.
        private var talkTimerTimer: Timer?

        init(session: CallSession) {
            observe(session)
        }

        deinit {
            talkTimerTimer?.invalidate()
        }

        private func observe(_ session: CallSession) {
            // `[weak self, weak session]` throughout: this must never keep the
            // session alive past its own lifetime, and self must never leak via
            // the sink's retained closure (astar-b167 guardrail).
            session.$status
                .receive(on: RunLoop.main)
                .sink { [weak self, weak session] newStatus in
                    guard let self, let session else { return }
                    let target = session.dialedNode.map(session.connectedTargetLabel(for:))
                    self.post(self.planner.statusChanged(to: newStatus, connectedTarget: target))
                }
                .store(in: &cancellables)

            session.$ptt
                .receive(on: RunLoop.main)
                .sink { [weak self] newValue in
                    self?.post(self?.planner.pttChanged(to: newValue, now: Date()) ?? [])
                }
                .store(in: &cancellables)

            session.$dtmfTotal
                .receive(on: RunLoop.main)
                .sink { [weak self] newValue in
                    self?.post(self?.planner.dtmfTotalChanged(to: newValue) ?? [])
                }
                .store(in: &cancellables)

            session.$txDisabled
                .receive(on: RunLoop.main)
                .sink { [weak self] newValue in
                    self?.post(self?.planner.txDisabledChanged(to: newValue) ?? [])
                }
                .store(in: &cancellables)

            session.$lastDialFailure
                .receive(on: RunLoop.main)
                .sink { [weak self] newValue in
                    self?.post(self?.planner.dialFailureChanged(to: newValue) ?? [])
                }
                .store(in: &cancellables)

            // Talk timer: the 1 s tick runs ONLY while keyed. `keyedSince`
            // flips nil<->non-nil on every key/unkey edge (CallSession.poll()),
            // so this publisher is exactly the "start/stop the timer" signal.
            session.$keyedSince
                .receive(on: RunLoop.main)
                .sink { [weak self, weak session] keyedSince in
                    guard let self, let session else { return }
                    self.talkTimerTimer?.invalidate()
                    self.talkTimerTimer = nil
                    guard keyedSince != nil else {
                        self.post(self.planner.talkTimerPhaseChanged(to: nil))
                        return
                    }
                    self.tickTalkTimer(session: session)
                    self.talkTimerTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) {
                        [weak self, weak session] _ in
                        guard let self, let session else { return }
                        self.tickTalkTimer(session: session)
                    }
                }
                .store(in: &cancellables)
        }

        /// Reads the same app-global talk-timer default (`@AppStorage`-backed,
        /// `Sources/FavoritesSettingsView.swift`) MenuPopover's dot uses, so the
        /// announcer and the visible dot can never disagree about the phase.
        private func tickTalkTimer(session: CallSession) {
            let defaults = UserDefaults.standard
            let enabled =
                defaults.object(forKey: TalkTimerDefaults.enabledKey) != nil
                ? defaults.bool(forKey: TalkTimerDefaults.enabledKey) : TalkTimer.defaultEnabled
            let minutes =
                defaults.object(forKey: TalkTimerDefaults.minutesKey) != nil
                ? defaults.integer(forKey: TalkTimerDefaults.minutesKey)
                : TalkTimer.defaultLimitMinutes
            let phase = session.talkTimerPhase(
                defaultEnabled: enabled, defaultLimitSeconds: minutes * 60)
            post(planner.talkTimerPhaseChanged(to: phase))
        }

        private func post(_ announcements: [Announcement]) {
            for announcement in announcements {
                Self.post(announcement.text, priority: announcement.priority)
            }
        }

        /// One-off announcement helper (astar-b167, audit F21/F22): for
        /// view-local state that never reaches `CallSession` — credential
        /// save/test outcomes (`CredentialsView`), the serial PTT self-test
        /// detection (`SetupsView`) — so those views don't need their own
        /// planner instance. Posts on the key window's content view when one
        /// exists, else `NSApp` itself (VoiceOver ignores this when off, so
        /// this is safe to call unconditionally).
        static func post(_ text: String, priority: Announcement.Priority) {
            let element: Any = NSApp.keyWindow?.contentView ?? NSApp as Any
            let level: NSAccessibilityPriorityLevel = priority == .high ? .high : .medium
            NSAccessibility.post(
                element: element,
                notification: .announcementRequested,
                userInfo: [
                    .announcement: text,
                    .priority: level.rawValue,
                ])
        }
    }
#endif
