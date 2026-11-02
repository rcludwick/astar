// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import Foundation

/// EchoLink on the session: the directory login, the station list, the chat,
/// and the password's one path from the Keychain to the engine.
///
/// EchoLink differs from every other network in having two things to track.
/// The **login** is to the EchoLink directory and lasts as long as the
/// operator wants to be listed; the **call** is to one station and runs
/// through the ordinary `connect(node:network:)` path like any other dial.
/// A call needs a login, and hanging up keeps the login.
///
/// **The password.** It arrives as an argument to `echolinkLogIn` (typed by
/// the operator) or is read from `echolinkPasswords` (the Keychain), and is
/// handed straight to the engine in the same breath. A typed one also waits,
/// inside `EchoLinkPendingLogin`'s save closure and nowhere else, until the
/// directory accepts or refuses it; then it is written to the Keychain or
/// dropped. No property of the session holds it.
extension CallSession {
    static let echolinkCallsignKey = "echolink.callsign"
    static let echolinkLocationKey = "echolink.location"
    static let echolinkAutoLoginKey = "echolink.autoLogin"

    /// How often the station list is re-read while logged in. The engine
    /// fetches it from the directory every 600 s; this is only how quickly a
    /// fresh one reaches the UI, and the read costs a JSON parse of a few
    /// hundred rows.
    static let echolinkStationsRefreshInterval: TimeInterval = 10

    /// `true` when the login failed for good — the directory said no, or the
    /// engine refused to start it — so the UI can offer "Log in again".
    ///
    /// `false` while no server answers: the engine reports that as `failed`
    /// too, but keeps retrying with a back-off and may still get in, so a
    /// "Log in again" button there would only restart a wait already running.
    /// The engine says which in the state document's `retrying` field
    /// (``echolinkLoginRetrying``).
    public var echolinkLoginRefused: Bool {
        echolinkLogin == .failed && !echolinkLoginRetrying
    }

    // MARK: - Logging in and out

    /// Log in to the EchoLink directory by hand.
    ///
    /// `password` is what the operator typed. Empty means "use the saved
    /// one", which needs `callsign` in ``echolinkSavedCallsigns``; without
    /// one nothing starts and the status line says what is missing.
    ///
    /// **A typed password is saved only once the directory accepts it**, the
    /// way the AllStarLink and DMR passwords are saved only once known good,
    /// so a mistyped one never lands in the Keychain. A refusal, a logout or
    /// an engine that will not start the login drops it. The callsign
    /// (trimmed, upper-cased) becomes ``echolinkCallsign`` at that same
    /// moment, and `autoLogin` takes effect then too, after the save and only
    /// if it worked.
    ///
    /// Returns at once. The Keychain and the engine are both reached on the
    /// EchoLink worker; the login shows up in `echolinkLogin` /
    /// `echolinkStatusLine` as the engine reports it.
    public func echolinkLogIn(callsign: String, password typed: String, autoLogin: Bool) {
        let callsign = Self.normaliseEchoLinkCallsign(callsign)
        guard !callsign.isEmpty, !typed.isEmpty || echolinkHasSavedPassword(callsign: callsign)
        else {
            echolinkStatusLine = Self.echolinkMissingPasswordLine
            return
        }
        echolinkKeychainMessage = nil
        let token = nextEchoLinkLoginToken()
        let store = echolinkPasswords
        let save: (() throws -> Void)? =
            typed.isEmpty
            ? nil : store.map { store in { try store.save(typed, callsign: callsign) } }
        echolinkPendingLogin = EchoLinkPendingLogin(
            token: token, callsign: callsign, autoLogin: autoLogin, save: save)
        if typed.isEmpty {
            beginEchoLinkLogin(callsign: callsign, token: token, quietWithoutPassword: false) {
                try store?.password(callsign: callsign)
            }
        } else {
            beginEchoLinkLogin(callsign: callsign, token: token, quietWithoutPassword: false) {
                typed
            }
        }
    }

    /// Whether `callsign` (any case, trimmed) has a password saved, from
    /// ``echolinkSavedCallsigns``. Never reads the Keychain.
    public func echolinkHasSavedPassword(callsign: String) -> Bool {
        echolinkSavedCallsigns.contains(Self.normaliseEchoLinkCallsign(callsign))
    }

    /// Read which callsigns have a password saved (keys only) into
    /// ``echolinkSavedCallsigns``, on the EchoLink worker. A failed read
    /// leaves the set as it was and says "Couldn’t read the Keychain." in
    /// `echolinkKeychainMessage`; the next read that works clears it.
    public func refreshEchoLinkSavedCallsigns() {
        guard let store = echolinkPasswords else { return }
        echolinkWorker { [weak self] in
            let saved: Set<String>?
            do {
                saved = try store.savedCallsigns()
            } catch {
                NSLog("[astar] echolink: keychain read failed: %@", "\(error)")
                saved = nil
            }
            Self.onMain {
                guard let self else { return }
                guard let saved else {
                    self.echolinkKeychainMessage = Self.echolinkKeychainUnreadable
                    return
                }
                if self.echolinkKeychainMessage == Self.echolinkKeychainUnreadable {
                    self.echolinkKeychainMessage = nil
                }
                if self.echolinkSavedCallsigns != saved { self.echolinkSavedCallsigns = saved }
            }
        }
    }

    /// Turn "Log in automatically" on or off for ``echolinkCallsign``. On
    /// takes only while that callsign has a password saved, since there
    /// would be nothing to log in with.
    public func setEchoLinkAutoLogin(_ on: Bool) {
        echolinkAutoLogin = on && echolinkHasSavedPassword(callsign: echolinkCallsign)
    }

    /// Remove the saved password for `callsign` from the Keychain, on the
    /// EchoLink worker. Forgetting ``echolinkCallsign``'s also turns "Log in
    /// automatically" off. A failed removal leaves everything as it was and
    /// says so in `echolinkKeychainMessage`.
    public func echolinkForgetPassword(callsign: String) {
        let callsign = Self.normaliseEchoLinkCallsign(callsign)
        guard let store = echolinkPasswords, !callsign.isEmpty else { return }
        echolinkWorker { [weak self] in
            var failed = false
            do {
                try store.remove(callsign: callsign)
            } catch {
                // The Keychain's own status, never the password.
                NSLog("[astar] echolink: keychain remove failed: %@", "\(error)")
                failed = true
            }
            Self.onMain {
                guard let self else { return }
                if failed {
                    self.echolinkKeychainMessage = "Couldn’t remove it from the Keychain."
                    return
                }
                self.echolinkKeychainMessage = nil
                self.echolinkSavedCallsigns.remove(callsign)
                if callsign == Self.normaliseEchoLinkCallsign(self.echolinkCallsign) {
                    self.echolinkAutoLogin = false
                }
            }
        }
    }

    /// The login by hand has its answer and it is yes: the callsign becomes
    /// the EchoLink callsign, then the typed password (if there was one) is
    /// saved on the worker, and only after that, on the main thread, does
    /// "Log in automatically" take what was asked — or come off, if the save
    /// failed.
    private func finishEchoLinkLogin(_ pending: EchoLinkPendingLogin) {
        echolinkCallsign = pending.callsign
        guard let save = pending.save else {
            setEchoLinkAutoLogin(pending.autoLogin)
            return
        }
        echolinkWorker { [weak self] in
            var failed = false
            do {
                try save()
            } catch {
                NSLog("[astar] echolink: keychain update failed: %@", "\(error)")
                failed = true
            }
            Self.onMain {
                guard let self else { return }
                if failed {
                    self.echolinkKeychainMessage = "Couldn’t save to the Keychain."
                    self.echolinkAutoLogin = false
                } else {
                    self.echolinkKeychainMessage = nil
                    self.echolinkSavedCallsigns.insert(pending.callsign)
                    self.setEchoLinkAutoLogin(pending.autoLogin)
                }
            }
        }
    }

    /// Log in at launch, but only when the operator has logged in by hand
    /// before and ticked "Log in automatically", and a password is saved for
    /// the callsign. Otherwise no login starts and nothing is said; a tick
    /// whose password has gone from the Keychain comes off, since it can no
    /// longer be honoured.
    ///
    /// Reads a fresh snapshot to check EchoLink is in this build, so it can
    /// be called before the poll loop has run. The Keychain is read on the
    /// EchoLink worker.
    public func startEchoLinkIfRemembered() {
        guard echolinkAutoLogin else { return }
        let callsign = Self.normaliseEchoLinkCallsign(echolinkCallsign)
        guard !callsign.isEmpty,
            (try? station.readSnapshot())?.echolinkAvailable == true,
            let store = echolinkPasswords
        else { return }
        // The pane opens on the station list after this login, never the
        // form, so this is where the saved callsigns get read.
        refreshEchoLinkSavedCallsigns()
        let token = nextEchoLinkLoginToken()
        beginEchoLinkLogin(callsign: callsign, token: token, quietWithoutPassword: true) {
            try store.password(callsign: callsign)
        }
    }

    static func normaliseEchoLinkCallsign(_ callsign: String) -> String {
        callsign.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
    }

    static let echolinkMissingPasswordLine = "Enter your callsign and EchoLink password."
    static let echolinkKeychainUnreadable = "Couldn’t read the Keychain."

    private func nextEchoLinkLoginToken() -> Int {
        echolinkLoginToken += 1
        return echolinkLoginToken
    }

    /// Hang up any EchoLink call and log off the directory.
    ///
    /// The call disappears from the UI at once; the logoff itself runs on the
    /// EchoLink worker, because the engine's can block for over a minute
    /// waiting on a directory exchange already in flight. The saved password
    /// and the "Log in automatically" tick are left as they are.
    public func echolinkLogOut() {
        if activeCallNetwork == .echolink {
            // Logging out hangs the call up, so it ends the recording the way
            // `disconnect()` does: the file is finished and nothing resumes.
            endRecordingForHangup()
            setDialedNode(nil)
            setActiveCallNetwork(nil)
        }
        clearEchoLinkCall()
        echolinkPendingLogin = nil
        echolinkPendingRequests += 1
        echolinkLogoutsPending += 1
        echolinkStatusLine = "Logging out…"
        if !echolinkStations.isEmpty { echolinkStations = [] }
        echolinkStationsReadAt = nil
        let station = self.station
        echolinkWorker { [weak self] in
            try? station.echolinkLogout()
            Self.onMain {
                guard let self else { return }
                self.echolinkPendingRequests -= 1
                self.echolinkLogoutsPending -= 1
                // Another request still queued (a second logout, or a login
                // after this one) owns the login fields until it finishes.
                guard self.echolinkPendingRequests == 0 else { return }
                self.echolinkLogin = .loggedOut
                self.echolinkLoginError = nil
                self.echolinkLoginRetrying = false
                self.echolinkStatusLine = ""
            }
        }
    }

    /// Hang up any EchoLink call and log off the directory as the app quits,
    /// waiting no longer than `timeout` (2 s) for it.
    ///
    /// The hang-up and the logoff run on a queue of their own, not the main
    /// thread and not the EchoLink worker (which may be busy with a login),
    /// and the caller waits on them with a deadline. The far station hears a
    /// BYE and the directory a logoff in the common case; a logoff stuck
    /// behind a directory exchange already in flight is left behind rather
    /// than holding the quit, and the directory drops the listing itself
    /// once its status refreshes stop.
    ///
    /// Returns `true` when both finished in time, and at once when there was
    /// no login or call to end.
    @discardableResult
    public func echolinkShutdownForQuit(timeout: TimeInterval = 2) -> Bool {
        guard
            echolinkLogin != .loggedOut || echolinkLink != nil || echolinkPendingRequests > 0
                || activeCallNetwork == .echolink
        else { return true }
        let station = self.station
        let done = DispatchSemaphore(value: 0)
        DispatchQueue(label: "astar.echolink.quit", qos: .userInitiated).async {
            try? station.echolinkDisconnect()
            try? station.echolinkLogout()
            done.signal()
        }
        let finished = done.wait(timeout: .now() + timeout) == .success
        if !finished {
            NSLog("[astar] echolink: the logoff did not finish within %.0f s of quitting", timeout)
        }
        return finished
    }

    /// Send one line of chat to the station on the call. Trimmed; a blank line
    /// is not sent. Runs on the EchoLink worker; the line appears in
    /// `echolinkChat` when the engine reports it.
    public func echolinkSendChat(_ text: String) {
        let line = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !line.isEmpty else { return }
        let station = self.station
        echolinkWorker {
            do {
                try station.echolinkSendChat(line)
            } catch {
                NSLog("[astar] echolink: chat not sent: %@", "\(error)")
            }
        }
    }

    /// Hand the location text to the engine, off the main thread.
    func pushEchoLinkLocation() {
        let station = self.station
        let location = echolinkLocation
        echolinkWorker { try? station.echolinkSetLocation(location) }
    }

    /// Start a login. `password` runs on the EchoLink worker — it may read
    /// the Keychain — and what it returns is held for the length of one
    /// engine call and is gone when it returns.
    ///
    /// `token` is this login's: a late answer from the worker changes the
    /// login fields, and drops the pending login, only while no newer login
    /// has started. With no password to hand, the login stops:
    ///
    /// - **None saved** (a confirmed absence): quietly at launch, taking the
    ///   "Log in automatically" tick off with it, since it can no longer be
    ///   honoured; by hand, with the status line saying what is missing.
    /// - **The Keychain would not answer**: the tick stays (the password may
    ///   well be there next time) and `echolinkKeychainMessage` says so.
    private func beginEchoLinkLogin(
        callsign: String, token: Int, quietWithoutPassword: Bool,
        password: @escaping () throws -> String?
    ) {
        echolinkPendingRequests += 1
        echolinkLogin = .loggingIn
        echolinkLoginError = nil
        echolinkLoginRetrying = false
        echolinkStatusLine = Self.echolinkStatusLine(
            login: .loggingIn, callsign: callsign, error: nil)
        let station = self.station
        let location = echolinkLocation
        echolinkWorker { [weak self] in
            var failure: String?
            var noPassword = false
            var unreadable = false
            let found: String?
            do {
                found = try password()
            } catch {
                NSLog("[astar] echolink: keychain read failed: %@", "\(error)")
                found = nil
                unreadable = true
            }
            if let password = found {
                do {
                    try station.echolinkSetLocation(location)
                    try station.echolinkLogin(
                        callsign: callsign, password: password, servers: EchoLinkDefaults.servers)
                } catch {
                    failure = Self.echolinkLoginFailureMessage(error)
                }
            } else if !unreadable {
                noPassword = true
            }
            Self.onMain {
                guard let self else { return }
                self.echolinkPendingRequests -= 1
                guard failure != nil || noPassword || unreadable else { return }
                if self.echolinkPendingLogin?.token == token { self.echolinkPendingLogin = nil }
                // A newer login owns the fields now.
                guard token == self.echolinkLoginToken else { return }
                self.echolinkLoginError = nil
                self.echolinkLoginRetrying = false
                if let failure {
                    self.echolinkLogin = .failed
                    self.echolinkLoginError = failure
                    self.echolinkStatusLine = failure
                } else if unreadable {
                    self.echolinkLogin = .loggedOut
                    self.echolinkStatusLine = ""
                    self.echolinkKeychainMessage = Self.echolinkKeychainUnreadable
                } else if quietWithoutPassword {
                    self.echolinkLogin = .loggedOut
                    self.echolinkStatusLine = ""
                    self.echolinkAutoLogin = false
                } else {
                    self.echolinkLogin = .loggedOut
                    self.echolinkStatusLine = Self.echolinkMissingPasswordLine
                }
            }
        }
    }

    /// The sentence for a login the engine refused to start.
    private static func echolinkLoginFailureMessage(_ error: Error) -> String {
        if let stationError = error as? StationError {
            return stationError.message
        }
        return error.localizedDescription
    }

    /// The status line for a login state.
    static func echolinkStatusLine(
        login: EchoLinkLoginState, callsign: String, error: String?
    ) -> String {
        switch login {
        case .loggedIn: return "Logged in as \(callsign)"
        case .loggingIn: return "Logging in as \(callsign)…"
        case .failed: return error ?? "The EchoLink login failed."
        case .loggedOut: return error ?? ""
        }
    }

    /// Run `body` on the main thread: inline when already there, queued
    /// otherwise.
    private static func onMain(_ body: @escaping () -> Void) {
        if Thread.isMainThread {
            body()
        } else {
            DispatchQueue.main.async(execute: body)
        }
    }

    // MARK: - Calling

    /// Turn the dial field's text into the target the engine is handed — the
    /// question both `canDial` and `connect` ask.
    ///
    /// Needs a login. The text must be a node number or callsign (the
    /// characters `Network.echolink` admits), and once the station list has
    /// arrived it must name a listed station, by node number or by callsign
    /// in any case — otherwise it throws `echolinkNotListedYet`, which
    /// `connect` answers by asking for a fresh list. Before the list arrives
    /// the engine, which matches against the same list, has the last word.
    ///
    /// `connect` runs off the main thread while `poll()` writes the login and
    /// the list on it, so both are read once, on the main thread, before
    /// anything is checked.
    func echolinkTarget(_ raw: String) throws -> String {
        let (login, stations) = onMainSync { (echolinkLogin, echolinkStations) }
        guard login == .loggedIn else {
            throw ConnectError.echolinkNotLoggedIn
        }
        let target = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty, target.allSatisfy(Network.echolink.admitsDialCharacter) else {
            throw ConnectError.echolinkUnknownStation(target)
        }
        let listed = stations.contains {
            String($0.node) == target || $0.call.caseInsensitiveCompare(target) == .orderedSame
        }
        guard listed || stations.isEmpty else {
            throw ConnectError.echolinkNotListedYet(target)
        }
        return target
    }

    /// Why the EchoLink dial field's text cannot be dialled yet, as a
    /// sentence for under the field, or `nil` when there is nothing to say:
    /// the text dials, the field is empty, or there is no login (the dial row
    /// says that itself, with a way to log in).
    public func echolinkDialProblem(_ raw: String) -> String? {
        let target = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty, echolinkLogin == .loggedIn else { return nil }
        do {
            _ = try echolinkTarget(target)
            return nil
        } catch ConnectError.echolinkNotListedYet {
            return "\(target) isn’t in the station list. Check the spelling, or refresh the list."
        } catch {
            return "Enter a callsign or node number from the station list."
        }
    }

    // MARK: - Refreshing the station list

    /// How long "Refreshed" or "Try again in a moment" stays on the Refresh
    /// control before it goes back to idle.
    static let echolinkRefreshStateSeconds: TimeInterval = 3

    /// Ask the directory for a fresh station list now, from the pane's
    /// Refresh control. Runs on the EchoLink worker; ``echolinkRefresh``
    /// says how it went: `.refreshed` when the directory was asked, and
    /// `.tooSoon` when the last list came less than 30 s ago (the engine's
    /// floor), each for a few seconds.
    public func echolinkRefreshStations() {
        requestEchoLinkListRefresh(fromControl: true)
    }

    /// The one path to the engine's refresh. `fromControl` is the Refresh
    /// control, whose state it drives; a dial that missed only asks.
    func requestEchoLinkListRefresh(fromControl: Bool) {
        let station = self.station
        let ask = { [weak self] in
            var asked: Bool?
            do {
                asked = try station.echolinkRefreshList()
            } catch {
                NSLog("[astar] echolink: station list refresh refused: %@", "\(error)")
                asked = nil
            }
            Self.onMain {
                guard let self else { return }
                if asked == true {
                    self.echolinkRefreshAskedAt = Date()
                    // Read again on the next poll, then every second.
                    self.echolinkStationsReadAt = nil
                }
                guard fromControl else { return }
                self.echolinkRefreshToken += 1
                let token = self.echolinkRefreshToken
                switch asked {
                case true?: self.echolinkRefresh = .refreshed
                case false?: self.echolinkRefresh = .tooSoon
                case nil: self.echolinkRefresh = .idle
                }
                guard self.echolinkRefresh != .idle else { return }
                DispatchQueue.main.asyncAfter(
                    deadline: .now() + Self.echolinkRefreshStateSeconds
                ) { [weak self] in
                    guard let self, self.echolinkRefreshToken == token else { return }
                    self.echolinkRefresh = .idle
                }
            }
        }
        if fromControl {
            echolinkRefresh = .refreshing
            echolinkWorker(ask)
        } else if Thread.isMainThread {
            echolinkWorker(ask)
        } else {
            // A dial runs off the main thread already, and the refresh is one
            // short request; doing it here keeps the order "ask, then refuse".
            ask()
        }
    }

    // MARK: - Polling

    /// Advance the EchoLink fields from one poll.
    ///
    /// The state crosses the ABI with a buffer and a JSON parse, so it is read
    /// only while there is something to watch — a login in any state but
    /// logged out, a call, or a request in flight — and never on an idle
    /// build that has not logged in. The station list is read on the edge
    /// into logged-in and then every `echolinkStationsRefreshInterval`.
    ///
    /// While a login or logout request is queued, the login fields are left
    /// alone: the engine has not seen the request yet, and its stale answer
    /// would flip the UI back.
    func refreshEchoLink(snapshot snap: CallSnapshot) {
        if echolinkAvailable != snap.echolinkAvailable {
            echolinkAvailable = snap.echolinkAvailable
        }
        let requestInFlight = echolinkPendingRequests > 0
        let watching =
            snap.echolinkLoggedIn || snap.echolinkActive || echolinkLogin != .loggedOut
            || requestInFlight || echolinkLink != nil
        let state = watching ? ((try? station.echolinkState()) ?? nil) : nil

        if !requestInFlight {
            let login = state?.login ?? .loggedOut
            let error = state?.error
            if echolinkLogin != login { echolinkLogin = login }
            if echolinkLoginError != error { echolinkLoginError = error }
            let retrying = state?.retrying ?? false
            if echolinkLoginRetrying != retrying { echolinkLoginRetrying = retrying }
            let line = Self.echolinkStatusLine(
                login: login, callsign: state?.callsign ?? echolinkCallsign, error: error)
            if echolinkStatusLine != line { echolinkStatusLine = line }

            // A login by hand learns its answer here: in, refused, or gone.
            // Retrying is no answer yet, so its password keeps waiting.
            // Only an answer about this login's callsign counts: a stale
            // state for another one says nothing about this password.
            if let pending = echolinkPendingLogin {
                let ours =
                    state.map { Self.normaliseEchoLinkCallsign($0.callsign) == pending.callsign }
                    ?? false
                switch login {
                case .loggedIn where ours:
                    echolinkPendingLogin = nil
                    finishEchoLinkLogin(pending)
                case .failed where ours && !retrying:
                    echolinkPendingLogin = nil
                case .loggedOut:
                    echolinkPendingLogin = nil
                case .loggedIn, .failed, .loggingIn:
                    break
                }
            }
        }

        // While logging out the call is already gone from the UI; the
        // engine hangs it up first thing, and its last report of it is not
        // brought back.
        if echolinkLoggingOut {
            // nothing: `echolinkLogOut` cleared the call.
        } else if let state, let link = state.link, link != .closed {
            if echolinkLink != link { echolinkLink = link }
            if echolinkPeer != state.peer { echolinkPeer = state.peer }
            if echolinkInfo != state.info { echolinkInfo = state.info }
            if echolinkChat != state.chat { echolinkChat = state.chat }
            if echolinkNotice != state.notice { echolinkNotice = state.notice }
            if state.receiving, let peer = state.peer, echolinkTalker != peer {
                echolinkTalker = peer
                refreshLastHeard()
            }
        } else if echolinkLink != nil || echolinkPeer != nil || echolinkInfo != nil
            || !echolinkChat.isEmpty || echolinkTalker != nil
        {
            clearEchoLinkCall()
        }

        let listWanted = !echolinkLoggingOut && state?.login == .loggedIn
        if listWanted {
            let now = Date()
            // Just after a refresh was asked for, read every second so the
            // new list shows as soon as it lands.
            let justAsked = echolinkRefreshAskedAt.map { now.timeIntervalSince($0) < 15 } ?? false
            let interval = justAsked ? 1 : Self.echolinkStationsRefreshInterval
            if let readAt = echolinkStationsReadAt, now.timeIntervalSince(readAt) < interval {
                return
            }
            echolinkStationsReadAt = now
            let stations = (try? station.echolinkStations()) ?? []
            if echolinkStations != stations { echolinkStations = stations }
        } else {
            echolinkStationsReadAt = nil
            if !echolinkStations.isEmpty { echolinkStations = [] }
        }
    }
}

/// A login by hand waiting on the directory's answer.
///
/// The typed password is captured by `save` and by nothing else, so a dump
/// of the session prints a function here, never the password. It is written
/// to the Keychain only when the login comes up, and dropped with this value
/// on a refusal, a logout or a login the engine would not start.
struct EchoLinkPendingLogin {
    /// The login's token (see `CallSession.echolinkLoginToken`).
    let token: Int
    /// Trimmed and upper-cased.
    let callsign: String
    /// What the operator asked "Log in automatically" to be.
    let autoLogin: Bool
    /// Saves the typed password, or `nil` when the login used the saved one
    /// (or there is no Keychain to save to).
    let save: (() throws -> Void)?
}
