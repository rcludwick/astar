// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// `CallSession` on EchoLink: the directory login, the call, the chat and the
/// password's path from the Keychain to the engine.
///
/// Every call lands on `FakeStation`. Nothing here resolves a host or opens a
/// socket — no test in this file may ever reach an EchoLink server or node.
/// The session's EchoLink worker is replaced with one that runs inline, so a
/// login or logout finishes before the call that started it returns.
final class CallSessionEchoLinkTests: XCTestCase {

    private let password = "s3cret-echolink-pw"

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.echolink.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private let stationsJSON = #"""
        [{"call":"*ECHOTEST*","description":"Test server","node":9999,"kind":"conference","status":"online"},
         {"call":"W1AW","description":"Newington","node":1234,"kind":"user","status":"online"}]
        """#

    private func state(_ json: String) -> EchoLinkState {
        guard let state = EchoLinkState(json: json) else {
            XCTFail("fixture did not decode: \(json)")
            return EchoLinkState(login: .loggedOut, callsign: "")
        }
        return state
    }

    private func snapshot(
        available: Bool = true, loggedIn: Bool = false, active: Bool = false,
        status: IaxStatus = .idle
    ) -> CallSnapshot {
        CallSnapshot(
            status: status, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            echolinkAvailable: available, echolinkActive: active, echolinkLoggedIn: loggedIn)
    }

    private func session(
        loggedIn: Bool = false, defaults: UserDefaults? = nil,
        passwords: EchoLinkCredentialStore = InMemoryEchoLinkCredentialStore()
    ) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = snapshot(loggedIn: loggedIn)
        if loggedIn {
            fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"KX7ABC"}"#)
            fake.echolinkStationsValue =
                EchoLinkStationEntry.decodeStations(json: stationsJSON) ?? []
        }
        let session = CallSession(
            station: fake, echolinkPasswords: passwords,
            userDefaults: defaults ?? scratchDefaults())
        session.echolinkWorker = { $0() }
        session.poll()
        return (session, fake)
    }

    /// Let the engine report the login up, and poll once.
    private func loggedIn(_ session: CallSession, _ fake: FakeStation) {
        fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"KX7ABC"}"#)
        fake.snapshotToReturn = snapshot(loggedIn: true)
        session.poll()
    }

    /// Put a connected call in hand: dial it, then let the engine report it.
    private func connected(chat: String = "[]", receiving: Bool = false) throws -> (
        CallSession, FakeStation
    ) {
        let (session, fake) = session(loggedIn: true)
        try session.connect(node: "9999", network: .echolink)
        fake.snapshotToReturn = snapshot(loggedIn: true, active: true, status: .answered)
        fake.echolinkStateValue = state(
            #"{"login":"logged_in","callsign":"KX7ABC","link":"connected","peer":"*ECHOTEST*","node":9999,"ptt":false,"receiving":\#(receiving),"info":"Welcome","chat":\#(chat)}"#
        )
        session.poll()
        return (session, fake)
    }

    // MARK: - Snapshot and login state

    func testSnapshotFlagsMirror() {
        XCTAssertFalse(CallSnapshot.idle.echolinkAvailable)
        XCTAssertFalse(CallSnapshot.idle.echolinkActive)
        XCTAssertFalse(CallSnapshot.idle.echolinkLoggedIn)

        let (session, fake) = session(loggedIn: true)
        XCTAssertTrue(session.echolinkAvailable)
        XCTAssertEqual(session.echolinkLogin, .loggedIn)
        XCTAssertEqual(session.echolinkStatusLine, "Logged in as KX7ABC")
        XCTAssertEqual(session.echolinkStations.map(\.call), ["*ECHOTEST*", "W1AW"])

        fake.snapshotToReturn = snapshot(available: false)
        fake.echolinkStateValue = nil
        session.poll()
        XCTAssertFalse(session.echolinkAvailable)
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
        XCTAssertEqual(session.echolinkStations, [], "no login, no list")
    }

    /// `failed` covers a final refusal and "no server answered, still
    /// retrying". Only the first is one where "Log in again" helps.
    func testAFinalRefusalIsToldApartFromRetrying() {
        let (session, fake) = session()
        session.echolinkLogIn(
            callsign: "KX7ABC", password: password, autoLogin: false)
        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":false,"callsign":"KX7ABC","error":"EchoLink said the password is wrong"}"#
        )
        session.poll()
        XCTAssertEqual(session.echolinkLogin, .failed)
        XCTAssertEqual(session.echolinkStatusLine, "EchoLink said the password is wrong")
        XCTAssertTrue(session.echolinkLoginRefused)

        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":true,"callsign":"KX7ABC","error":"Couldn't reach any EchoLink server"}"#
        )
        session.poll()
        XCTAssertEqual(session.echolinkLogin, .failed)
        XCTAssertFalse(session.echolinkLoginRefused, "unreachable is still retrying")
        XCTAssertTrue(session.echolinkLoginRetrying)

        // The flag, not the wording, decides: a reworded sentence changes
        // nothing.
        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":true,"callsign":"KX7ABC","error":"No directory answered"}"#
        )
        session.poll()
        XCTAssertFalse(session.echolinkLoginRefused)
    }

    // MARK: - Logging in

    func testAutoLoginIsOffUntilTicked() throws {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, fake) = session(defaults: defaults, passwords: passwords)

        XCTAssertFalse(session.echolinkAutoLogin, "off until ticked")
        session.startEchoLinkIfRemembered()
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty, "a saved password alone logs nothing in")
    }

    func testAutoLoginLogsInWhenTickedAndSaved() throws {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        defaults.set(true, forKey: "echolink.autoLogin")

        // Ticked, but nothing saved: still nothing. Its own defaults, since
        // that session takes the tick off.
        let emptyDefaults = scratchDefaults()
        emptyDefaults.set("KX7ABC", forKey: "echolink.callsign")
        emptyDefaults.set(true, forKey: "echolink.autoLogin")
        let (empty, emptyFake) = session(defaults: emptyDefaults)
        empty.startEchoLinkIfRemembered()
        XCTAssertTrue(emptyFake.echolinkLoginCalls.isEmpty)

        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, fake) = session(defaults: defaults, passwords: passwords)
        session.startEchoLinkIfRemembered()
        XCTAssertEqual(fake.echolinkLoginCalls.count, 1)
        let call = try XCTUnwrap(fake.echolinkLoginCalls.first)
        XCTAssertEqual(call.callsign, "KX7ABC")
        XCTAssertEqual(call.password, password, "read from the store and handed straight over")
        XCTAssertEqual(call.servers, EchoLinkDefaults.servers)
    }

    /// The hand login saves the callsign at once and the password only once
    /// the directory has accepted it, so a mistyped password never lands in
    /// the Keychain.
    func testASuccessfulLoginSavesThePassword() {
        let defaults = scratchDefaults()
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(defaults: defaults, passwords: passwords)
        session.echolinkLocation = "Denver, CO"

        session.echolinkLogIn(callsign: " kx7abc ", password: password, autoLogin: false)
        XCTAssertEqual(fake.echolinkLoginCalls.first?.callsign, "KX7ABC")
        XCTAssertEqual(
            fake.echolinkLocations.last, "Denver, CO", "the location goes before the login")
        XCTAssertEqual(session.echolinkLogin, .loggingIn)
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"), "not before the result is known")
        XCTAssertNil(
            defaults.string(forKey: "echolink.callsign"), "the callsign switches on success")

        fake.echolinkStateValue = state(#"{"login":"logging_in","callsign":"KX7ABC"}"#)
        session.poll()
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"), "still not known")

        loggedIn(session, fake)
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password, "saved on success")
        XCTAssertEqual(defaults.string(forKey: "echolink.callsign"), "KX7ABC")
        XCTAssertTrue(session.echolinkHasSavedPassword(callsign: "kx7abc"))
        XCTAssertNil(session.echolinkKeychainMessage)
        XCTAssertFalse(session.echolinkAutoLogin, "not asked for")
    }

    func testARefusedLoginSavesNothing() {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: true)
        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":false,"callsign":"KX7ABC","error":"EchoLink said the password is wrong"}"#
        )
        session.poll()
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
        XCTAssertFalse(session.echolinkAutoLogin, "no saved password, no automatic login")

        // A later success on its own does not resurrect the refused attempt.
        loggedIn(session, fake)
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
    }

    /// No server answering is not a refusal: the engine keeps trying, and
    /// the password is saved if it gets in.
    func testALoginThatRetriesStillSavesOnSuccess() {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: false)
        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":true,"callsign":"KX7ABC","error":"Couldn't reach"}"#)
        session.poll()
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
        loggedIn(session, fake)
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password)
    }

    func testALogoutBeforeTheResultSavesNothing() {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: true)
        session.echolinkLogOut()
        loggedIn(session, fake)
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
        XCTAssertFalse(session.echolinkAutoLogin)
    }

    func testAKeychainFailureIsSaidInTheForm() {
        let (session, fake) = session(passwords: FailingEchoLinkCredentialStore())
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: true)
        loggedIn(session, fake)
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t save to the Keychain.")
        XCTAssertFalse(session.echolinkAutoLogin, "nothing saved, so nothing to log in with")
        XCTAssertEqual(session.echolinkLogin, .loggedIn, "the login itself still stands")
        XCTAssertFalse(session.echolinkKeychainMessage?.contains(password) ?? false)
    }

    /// "Log in automatically" takes only with a password saved, and a saved
    /// password can be used again without retyping it.
    func testAutoLoginNeedsASavedPassword() throws {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)

        session.setEchoLinkAutoLogin(true)
        XCTAssertFalse(session.echolinkAutoLogin, "nothing saved yet")

        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: true)
        XCTAssertFalse(session.echolinkAutoLogin, "not before the save")
        loggedIn(session, fake)
        XCTAssertTrue(session.echolinkAutoLogin, "set once the password is saved")

        session.setEchoLinkAutoLogin(false)
        XCTAssertFalse(session.echolinkAutoLogin)
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin)

        // Logging in again with the saved password: nothing typed.
        session.echolinkLogOut()
        session.echolinkLogIn(callsign: "kx7abc", password: "", autoLogin: true)
        XCTAssertEqual(fake.echolinkLoginCalls.count, 2)
        XCTAssertEqual(fake.echolinkLoginCalls.last?.password, password, "the saved one")
    }

    func testForgettingThePasswordTurnsOffAutoLogin() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, fake) = session(defaults: defaults, passwords: passwords)
        session.setEchoLinkAutoLogin(true)
        XCTAssertFalse(session.echolinkAutoLogin, "the saved callsigns are not read yet")
        session.refreshEchoLinkSavedCallsigns()
        XCTAssertEqual(session.echolinkSavedCallsigns, ["KX7ABC"])
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin)

        session.echolinkForgetPassword(callsign: " kx7abc ")
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
        XCTAssertFalse(session.echolinkHasSavedPassword(callsign: "KX7ABC"))
        XCTAssertFalse(session.echolinkAutoLogin)

        session.echolinkLogIn(callsign: "KX7ABC", password: "", autoLogin: false)
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty, "nothing saved, nothing typed")

        let (failing, _) = self.session(passwords: FailingEchoLinkCredentialStore())
        failing.echolinkForgetPassword(callsign: "KX7ABC")
        XCTAssertEqual(failing.echolinkKeychainMessage, "Couldn’t remove it from the Keychain.")
    }

    /// Asking whether a callsign has a password saved, or flipping
    /// "Log in automatically", never touches the Keychain: the saved
    /// callsigns are read once, keys only, and kept.
    func testSavedPasswordChecksReadTheCacheNotTheKeychain() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        let passwords = CountingEchoLinkCredentialStore(["KX7ABC": password])
        let (session, _) = session(defaults: defaults, passwords: passwords)
        session.refreshEchoLinkSavedCallsigns()
        XCTAssertEqual(passwords.listReads, 1)
        for typed in ["K", "KX", "KX7", "KX7A", "KX7AB", "kx7abc"] {
            _ = session.echolinkHasSavedPassword(callsign: typed)
        }
        XCTAssertTrue(session.echolinkHasSavedPassword(callsign: " kx7abc "))
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin)
        XCTAssertEqual(passwords.passwordReads, 0, "no password read for a check")
        XCTAssertEqual(passwords.listReads, 1)
    }

    /// The directory answering for some other callsign than the pending
    /// login's (a stale state, a second login) does not save its password.
    func testAPendingLoginCompletesOnlyForItsOwnCallsign() {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: true)
        fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"W1AW"}"#)
        fake.snapshotToReturn = snapshot(loggedIn: true)
        session.poll()
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"))
        XCTAssertNil(try passwords.password(callsign: "W1AW"))
        XCTAssertFalse(session.echolinkAutoLogin)

        loggedIn(session, fake)
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password, "its own answer")
    }

    /// A first login's failure arriving late does not drop a second login
    /// that replaced it.
    func testALateFailureClearsOnlyItsOwnPendingLogin() {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        var queued: [() -> Void] = []
        session.echolinkWorker = { queued.append($0) }

        fake.echolinkLoginError = StationError(code: -24, text: "echolink error")
        session.echolinkLogIn(callsign: "KX7ABC", password: "typo", autoLogin: false)
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: false)
        // The first runs and fails; then the engine takes the second.
        queued.removeFirst()()
        fake.echolinkLoginError = nil
        for work in queued { work() }
        queued.removeAll()

        loggedIn(session, fake)
        XCTAssertNil(try passwords.password(callsign: "KX7ABC"), "the save is the worker's job")
        for work in queued { work() }
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password)
    }

    /// The saved callsign changes only on a login that worked, so "Log in
    /// automatically" never stands for a callsign with nothing saved.
    func testARefusedLoginLeavesTheCallsignAndAutoLogin() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        defaults.set(true, forKey: "echolink.autoLogin")
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, fake) = session(defaults: defaults, passwords: passwords)

        session.echolinkLogIn(callsign: "W1AW", password: "wrong", autoLogin: false)
        fake.echolinkStateValue = state(
            #"{"login":"failed","retrying":false,"callsign":"W1AW","error":"no"}"#)
        session.poll()
        XCTAssertEqual(session.echolinkCallsign, "KX7ABC")
        XCTAssertTrue(session.echolinkAutoLogin, "still true for KX7ABC, which has a password")
    }

    /// Ticked at launch with the password gone from the Keychain: nothing
    /// logs in, and the tick, which can no longer be honoured, comes off.
    func testAutoLoginWithNothingSavedClearsItself() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        defaults.set(true, forKey: "echolink.autoLogin")
        let (session, fake) = session(defaults: defaults)
        session.startEchoLinkIfRemembered()
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty)
        XCTAssertFalse(session.echolinkAutoLogin)
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
        XCTAssertEqual(session.echolinkStatusLine, "")
    }

    /// After a login at launch the pane opens on the station list, never the
    /// form, so the launch itself has to read which callsigns are saved —
    /// or the "Log in automatically" box would hide and refuse to turn on.
    func testALaunchLoginReadsTheSavedCallsigns() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        defaults.set(true, forKey: "echolink.autoLogin")
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, fake) = session(defaults: defaults, passwords: passwords)

        session.startEchoLinkIfRemembered()
        loggedIn(session, fake)
        XCTAssertEqual(session.echolinkSavedCallsigns, ["KX7ABC"])
        XCTAssertTrue(session.echolinkHasSavedPassword(callsign: "KX7ABC"))
        session.setEchoLinkAutoLogin(false)
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin, "the box still turns back on")
    }

    /// A Keychain that will not answer at launch is not a missing password:
    /// nothing logs in, the tick stays for next time, and the form says why.
    func testAnUnreadableKeychainAtLaunchKeepsAutoLogin() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        defaults.set(true, forKey: "echolink.autoLogin")
        let passwords = UnreadableEchoLinkCredentialStore(["KX7ABC": password], readsFail: true)
        let (session, fake) = session(defaults: defaults, passwords: passwords)

        session.startEchoLinkIfRemembered()
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty)
        XCTAssertTrue(session.echolinkAutoLogin, "kept: the password may well be there")
        XCTAssertTrue(defaults.bool(forKey: "echolink.autoLogin"))
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t read the Keychain.")
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
    }

    /// Logging in with the saved password when the Keychain will not answer
    /// says so, rather than asking for a password that is saved.
    func testAnUnreadableKeychainOnAnEmptyFieldLoginSaysSo() {
        let passwords = UnreadableEchoLinkCredentialStore(["KX7ABC": password], readsFail: false)
        let (session, fake) = session(passwords: passwords)
        session.refreshEchoLinkSavedCallsigns()
        XCTAssertEqual(session.echolinkSavedCallsigns, ["KX7ABC"])

        passwords.readsFail = true
        session.echolinkLogIn(callsign: "KX7ABC", password: "", autoLogin: false)
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty)
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t read the Keychain.")
        XCTAssertNotEqual(session.echolinkStatusLine, "Enter your callsign and EchoLink password.")
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
    }

    func testAFailedSavedCallsignsReadIsSaid() {
        let passwords = UnreadableEchoLinkCredentialStore(["KX7ABC": password], readsFail: true)
        let (session, _) = session(passwords: passwords)
        session.refreshEchoLinkSavedCallsigns()
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t read the Keychain.")
        XCTAssertEqual(session.echolinkSavedCallsigns, [])

        passwords.readsFail = false
        session.refreshEchoLinkSavedCallsigns()
        XCTAssertNil(session.echolinkKeychainMessage, "a read that works clears it")
        XCTAssertEqual(session.echolinkSavedCallsigns, ["KX7ABC"])
    }

    func testAnEmptyCallsignOrPasswordStartsNothing() {
        let (session, fake) = session()
        session.echolinkLogIn(callsign: "  ", password: password, autoLogin: false)
        session.echolinkLogIn(callsign: "KX7ABC", password: "", autoLogin: false)
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty)
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
        XCTAssertFalse(session.echolinkStatusLine.isEmpty, "the status line says what is missing")
    }

    func testLoggingOutRunsOnTheWorkerAndEndsTheCall() throws {
        let (session, fake) = try connected()
        var queued: [() -> Void] = []
        session.echolinkWorker = { queued.append($0) }

        session.echolinkLogOut()
        XCTAssertEqual(fake.echolinkLogouts, 0, "the blocking logoff is not run in place")
        XCTAssertNil(session.activeCallNetwork, "the call is gone from the UI at once")
        XCTAssertEqual(session.echolinkStatusLine, "Logging out…")

        // A poll while the logoff is still running does not flip the login back.
        session.poll()
        XCTAssertEqual(session.echolinkStatusLine, "Logging out…")

        fake.echolinkStateValue = nil
        fake.snapshotToReturn = snapshot()
        for work in queued { work() }
        XCTAssertEqual(fake.echolinkLogouts, 1)
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
        XCTAssertEqual(session.echolinkStations, [])
    }

    func testTwoQueuedLogoutsStayLoggingOutUntilTheLast() throws {
        let (session, fake) = session(loggedIn: true)
        var queued: [() -> Void] = []
        session.echolinkWorker = { queued.append($0) }
        session.echolinkLogOut()
        session.echolinkLogOut()
        XCTAssertEqual(queued.count, 2)

        queued.removeFirst()()
        XCTAssertEqual(fake.echolinkLogouts, 1)
        XCTAssertTrue(session.echolinkLoggingOut, "the second logout is still to run")
        XCTAssertEqual(session.echolinkStatusLine, "Logging out…")
        session.poll()
        XCTAssertEqual(session.echolinkStatusLine, "Logging out…")

        fake.echolinkStateValue = nil
        fake.snapshotToReturn = snapshot()
        queued.removeFirst()()
        XCTAssertFalse(session.echolinkLoggingOut)
        XCTAssertEqual(session.echolinkLogin, .loggedOut)
        XCTAssertEqual(session.echolinkStatusLine, "")
    }

    // MARK: - Recording an EchoLink call

    /// A logged-in session whose recorder work runs inline, with AllStar
    /// credentials so a test can dial AllStar first.
    private func recordingSession(alwaysRecord: Bool = false) -> (
        CallSession, FakeStation, MemoryRecordingSettingsStore
    ) {
        let fake = FakeStation()
        fake.snapshotToReturn = snapshot(loggedIn: true)
        fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"KX7ABC"}"#)
        fake.echolinkStationsValue = EchoLinkStationEntry.decodeStations(json: stationsJSON) ?? []
        let store = MemoryRecordingSettingsStore()
        store.settings.alwaysRecord = alwaysRecord
        let session = CallSession(
            station: fake, hasCredentials: true, audioStore: MemoryAudioStore(),
            directoryStore: MemoryNodeDirectoryStore(),
            echolinkPasswords: InMemoryEchoLinkCredentialStore(),
            userDefaults: scratchDefaults(), recordingStore: store)
        session.echolinkWorker = { $0() }
        session.runRecordingWork = { $0() }
        session.poll()
        return (session, fake, store)
    }

    /// Let the poll see `status`, with the login still up.
    private func poll(_ session: CallSession, _ fake: FakeStation, _ status: IaxStatus) {
        fake.snapshotToReturn = snapshot(
            loggedIn: true, active: status == .dialing || status == .answered, status: status)
        session.poll()
    }

    /// Dial AllStar 2000, let it answer, and start recording it.
    private func recordingAllStar2000(
        _ session: CallSession, _ fake: FakeStation
    ) throws {
        try session.connect(node: "2000", network: .allstar)
        poll(session, fake, .dialing)
        fake.recordStateValue = try RecordState.make(recording: true)
        poll(session, fake, .answered)
        session.toggleRecording()
        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000"])
    }

    func testAlwaysRecordNamesAnEchoLinkFileAfterTheTarget() throws {
        let (session, fake, _) = recordingSession(alwaysRecord: true)
        try session.connect(node: "9999", network: .echolink)
        poll(session, fake, .dialing)
        fake.recordStateValue = try RecordState.make(recording: true)
        poll(session, fake, .answered)

        XCTAssertTrue(session.recording)
        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["EchoLink 9999"])
    }

    func testAnAllStarToEchoLinkDialWhileRecordingResumesOnTheAnswer() throws {
        let (session, fake, _) = recordingSession()
        try recordingAllStar2000(session, fake)

        try session.connect(node: "9999", network: .echolink)
        poll(session, fake, .hangup)
        XCTAssertEqual(fake.recordStopCount, 1, "the AllStar file is finished")
        XCTAssertFalse(session.recording)

        poll(session, fake, .answered)
        XCTAssertEqual(fake.recordStartCalls.map(\.label), ["AllStar 2000", "EchoLink 9999"])
        XCTAssertTrue(session.recording)
    }

    /// A logout landing while the EchoLink call is being set up supersedes
    /// the dial: no resume survives it, so a later answer starts nothing.
    func testALogoutDuringTheDialLeavesNoResumeToFire() throws {
        let (session, fake, _) = recordingSession()
        try recordingAllStar2000(session, fake)
        session.testPostEngineCallHook = { [weak session] in session?.echolinkLogOut() }

        try session.connect(node: "9999", network: .echolink)
        XCTAssertNil(session.dialedNode, "the superseded dial is not published")
        XCTAssertFalse(session.recording)
        poll(session, fake, .hangup)
        poll(session, fake, .answered)

        XCTAssertEqual(fake.recordStartCalls.count, 1, "only the AllStar recording ever started")
        XCTAssertFalse(session.recording)
    }

    /// Logging out mid-call ends the call, so it finishes the file at once
    /// and nothing resumes after it.
    func testLoggingOutMidCallStopsTheRecording() throws {
        let (session, fake, _) = recordingSession()
        try session.connect(node: "9999", network: .echolink)
        poll(session, fake, .dialing)
        fake.recordStateValue = try RecordState.make(recording: true)
        poll(session, fake, .answered)
        session.toggleRecording()
        XCTAssertTrue(session.recording)

        session.echolinkLogOut()
        XCTAssertFalse(session.recording)
        XCTAssertEqual(fake.recordStopCount, 1)
        XCTAssertEqual(fake.recordStartCalls.count, 1)
    }

    // MARK: - Calling

    /// A logout landing while the call is being set up wins: the dial does
    /// not publish `.echolink` over a session with no login and no call.
    func testALogoutDuringTheDialLeavesNoActiveNetwork() throws {
        let (session, fake) = session(loggedIn: true)
        var queued: [() -> Void] = []
        session.echolinkWorker = { queued.append($0) }
        session.testPostEngineCallHook = { [weak session] in session?.echolinkLogOut() }

        try session.connect(node: "9999", network: .echolink)
        XCTAssertEqual(fake.echolinkConnectCalls, ["9999"])
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertNil(session.dialedNode)
        XCTAssertGreaterThan(fake.echolinkDisconnects, 0, "the call just placed is hung up")

        // And with the logout already finished by the time the dial returns.
        let (inline, inlineFake) = self.session(loggedIn: true)
        inline.testPostEngineCallHook = { [weak inline] in inline?.echolinkLogOut() }
        try inline.connect(node: "9999", network: .echolink)
        XCTAssertEqual(inlineFake.echolinkLogouts, 1)
        XCTAssertNil(inline.activeCallNetwork)
        XCTAssertNil(inline.dialedNode)
        _ = queued
    }

    func testConnectNeedsALogin() {
        let (session, fake) = session()
        XCTAssertFalse(session.canDial("9999", network: .echolink))
        XCTAssertThrowsError(try session.connect(node: "9999", network: .echolink)) {
            XCTAssertEqual($0 as? CallSession.ConnectError, .echolinkNotLoggedIn)
        }
        XCTAssertTrue(fake.echolinkConnectCalls.isEmpty)
        XCTAssertNil(session.dialedNode, "a refused dial touches nothing")
    }

    func testConnectArmDialsTheTypedTarget() throws {
        let (session, fake) = session(loggedIn: true)
        XCTAssertTrue(session.canDial("9999", network: .echolink))
        XCTAssertTrue(session.canDial("*echotest*", network: .echolink))
        XCTAssertFalse(
            session.canDial("4321", network: .echolink),
            "well shaped but not listed: Connect stays off and the dial row says why")
        XCTAssertFalse(session.canDial("", network: .echolink))
        XCTAssertFalse(session.canDial("W1 AW!", network: .echolink), "not a station's shape")

        try session.connect(node: "9999", network: .echolink)
        XCTAssertEqual(fake.echolinkConnectCalls, ["9999"])
        XCTAssertEqual(session.activeCallNetwork, .echolink)
        XCTAssertEqual(session.dialedNode, "9999")
        XCTAssertEqual(
            Array(fake.callLog.suffix(2)), ["disconnect", "echolinkConnect"],
            "a stale call is cleared before the dial")

        XCTAssertThrowsError(try session.connect(node: "4321", network: .echolink)) {
            XCTAssertEqual($0 as? CallSession.ConnectError, .echolinkNotListedYet("4321"))
        }
    }

    func testAStationNotInTheListAsksForAFreshListBeforeRefusing() {
        let (session, fake) = session(loggedIn: true)
        XCTAssertThrowsError(try session.connect(node: "4321", network: .echolink)) {
            XCTAssertEqual(
                ($0 as? LocalizedError)?.errorDescription,
                "Not in the station list yet — refreshing…")
        }
        XCTAssertEqual(fake.echolinkRefreshes, 1, "a fresh list was asked for")
        XCTAssertEqual(
            Array(fake.callLog.suffix(1)), ["echolinkRefreshList"],
            "nothing was dialled")
        XCTAssertTrue(fake.echolinkConnectCalls.isEmpty)
        XCTAssertNil(session.dialedNode)
        XCTAssertEqual(session.echolinkRefresh, .idle, "a dial does not drive the control")

        // The next poll reads the list again rather than waiting 10 s, and the
        // new list makes the station callable.
        let reads = fake.echolinkStationsReads
        fake.echolinkStationsValue.append(
            EchoLinkStationEntry(
                call: "N0NEW", description: "", node: 4321, kind: .user, status: .online))
        session.poll()
        XCTAssertEqual(fake.echolinkStationsReads, reads + 1)
        XCTAssertNoThrow(try session.connect(node: "4321", network: .echolink))
        XCTAssertEqual(fake.echolinkConnectCalls, ["4321"])
    }

    func testTheRefreshControlSaysRefreshedOrTryAgain() {
        let (session, fake) = session(loggedIn: true)
        XCTAssertEqual(session.echolinkRefresh, .idle)
        session.echolinkRefreshStations()
        XCTAssertEqual(fake.echolinkRefreshes, 1)
        XCTAssertEqual(session.echolinkRefresh, .refreshed)

        fake.echolinkRefreshResult = false
        session.echolinkRefreshStations()
        XCTAssertEqual(session.echolinkRefresh, .tooSoon, "inside the engine's 30 s floor")

        fake.echolinkRefreshError = StationError(code: -24, text: "echolink error")
        session.echolinkRefreshStations()
        XCTAssertEqual(session.echolinkRefresh, .idle, "not logged in: nothing to show")

        // The "refreshed" state goes back to idle by itself.
        fake.echolinkRefreshError = nil
        fake.echolinkRefreshResult = true
        session.echolinkRefreshStations()
        XCTAssertEqual(session.echolinkRefresh, .refreshed)
        let back = expectation(description: "back to idle")
        DispatchQueue.main.asyncAfter(
            deadline: .now() + CallSession.echolinkRefreshStateSeconds + 0.3
        ) {
            XCTAssertEqual(session.echolinkRefresh, .idle)
            back.fulfill()
        }
        wait(for: [back], timeout: CallSession.echolinkRefreshStateSeconds + 2)
    }

    // MARK: - Quitting

    func testQuittingHangsUpAndLogsOff() throws {
        let (session, fake) = try connected()
        XCTAssertTrue(session.echolinkShutdownForQuit(timeout: 2))
        XCTAssertEqual(fake.echolinkLogouts, 1)
        XCTAssertGreaterThan(fake.echolinkDisconnects, 0)
        XCTAssertEqual(
            Array(fake.callLog.suffix(2)), ["echolinkDisconnect", "echolinkLogout"],
            "the call is hung up before the directory hears the logoff")
    }

    func testQuittingWaitsNoLongerThanTheDeadline() {
        let (session, fake) = session(loggedIn: true)
        fake.echolinkLogoutDelay = 1.5
        let started = Date()
        XCTAssertFalse(session.echolinkShutdownForQuit(timeout: 0.2))
        XCTAssertLessThan(Date().timeIntervalSince(started), 1.0, "the quit is not held")
    }

    func testQuittingWithNothingOnEchoLinkDoesNothing() {
        let (session, fake) = session()
        XCTAssertTrue(session.echolinkShutdownForQuit(timeout: 2))
        XCTAssertEqual(fake.echolinkLogouts, 0)
    }

    func testTheTransmitCapNoticeShowsAndClearsWithTheCall() throws {
        let (session, fake) = try connected()
        XCTAssertNil(session.echolinkNotice)
        fake.echolinkStateValue = state(
            #"{"login":"logged_in","callsign":"KX7ABC","link":"connected","peer":"*ECHOTEST*","node":9999,"notice":"EchoLink stopped transmitting after 10 minutes of continuous talk. Release PTT to talk again."}"#
        )
        session.poll()
        XCTAssertEqual(
            session.echolinkNotice,
            "EchoLink stopped transmitting after 10 minutes of continuous talk. Release PTT to talk again."
        )
        try session.disconnect()
        XCTAssertNil(session.echolinkNotice, "the call's sentence goes with the call")
    }

    func testAFailedConnectLeavesNoCallBehind() {
        let (session, fake) = session(loggedIn: true)
        fake.echolinkConnectError = StationError(code: -24, text: "echolink error")
        XCTAssertThrowsError(try session.connect(node: "9999", network: .echolink))
        XCTAssertNil(session.dialedNode)
        XCTAssertNil(session.activeCallNetwork)
    }

    func testChatHistoryClearsOnDisconnect() throws {
        let (session, fake) = try connected(
            chat:
                #"[{"from":"*ECHOTEST*","text":"hello","mine":false},{"from":"KX7ABC","text":"hi","mine":true}]"#
        )
        XCTAssertEqual(session.echolinkPeer, "*ECHOTEST*")
        XCTAssertEqual(session.echolinkInfo, "Welcome")
        XCTAssertEqual(session.echolinkChat.map(\.text), ["hello", "hi"])

        session.echolinkSendChat("  how copy?  ")
        session.echolinkSendChat("   ")
        XCTAssertEqual(
            fake.echolinkChatSent, ["how copy?"], "trimmed, and a blank line is not sent")

        try session.disconnect()
        XCTAssertEqual(fake.echolinkDisconnects, 1)
        XCTAssertEqual(session.echolinkChat, [])
        XCTAssertNil(session.echolinkPeer)
        XCTAssertNil(session.echolinkInfo)
        XCTAssertEqual(session.echolinkLogin, .loggedIn, "hanging up keeps the login")
    }

    func testTheFarEndHangingUpClearsTheCall() throws {
        let (session, fake) = try connected(
            chat: #"[{"from":"*ECHOTEST*","text":"hello","mine":false}]"#)
        fake.snapshotToReturn = snapshot(loggedIn: true, status: .hangup)
        fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"KX7ABC"}"#)
        session.poll()
        XCTAssertNil(session.activeCallNetwork)
        XCTAssertEqual(session.echolinkChat, [])
        XCTAssertNil(session.echolinkPeer)
    }

    func testLastHeardIsTheTalkingPeer() throws {
        let (session, fake) = try connected(receiving: false)
        XCTAssertNil(session.lastHeard, "nobody has talked yet")

        fake.echolinkStateValue = state(
            #"{"login":"logged_in","callsign":"KX7ABC","link":"connected","peer":"*ECHOTEST*","node":9999,"receiving":true}"#
        )
        session.poll()
        XCTAssertEqual(session.lastHeard, "*ECHOTEST*")

        fake.echolinkStateValue = state(
            #"{"login":"logged_in","callsign":"KX7ABC","link":"connected","peer":"*ECHOTEST*","node":9999,"receiving":false}"#
        )
        session.poll()
        XCTAssertEqual(session.lastHeard, "*ECHOTEST*", "last heard, not talking now")
    }

    // MARK: - The password

    func testPasswordIsNeverPublished() throws {
        let defaults = scratchDefaults()
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(defaults: defaults, passwords: passwords)
        session.echolinkLogIn(
            callsign: "KX7ABC", password: password, autoLogin: true)
        // While the result is pending too: the password waits inside the
        // save closure, never in a property a dump can print.
        assertNoPropertyHoldsThePassword(session)
        fake.echolinkStateValue = state(#"{"login":"logged_in","callsign":"KX7ABC"}"#)
        fake.snapshotToReturn = snapshot(loggedIn: true)
        session.poll()
        XCTAssertEqual(fake.echolinkLoginCalls.first?.password, password, "it did reach the engine")

        // Every stored property of the session, rendered in full. The station
        // and the password store are skipped: the first is the engine seam
        // (a fake here, which records the login by design) and the second IS
        // the Keychain.
        assertNoPropertyHoldsThePassword(session)
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password, "the Keychain has it")
        XCTAssertFalse(session.echolinkStatusLine.contains(password))

        // Nothing in the defaults domain, and nothing an export would carry.
        let domain = defaults.dictionaryRepresentation()
        for (key, value) in domain {
            XCTAssertFalse("\(value)".contains(password), "\(key) holds the password")
        }
        XCTAssertTrue(
            ConfigArchive.settingsSlice(from: domain).keys.allSatisfy {
                !$0.hasPrefix("echolink.")
            },
            "no EchoLink preference travels in an .astarconfig")
    }

    private func assertNoPropertyHoldsThePassword(
        _ session: CallSession, file: StaticString = #filePath, line: UInt = #line
    ) {
        for child in Mirror(reflecting: session).children {
            guard let label = child.label, label != "station", label != "echolinkPasswords"
            else { continue }
            var rendered = ""
            dump(child.value, to: &rendered)
            XCTAssertFalse(
                rendered.contains(password), "\(label) holds the password", file: file, line: line)
        }
    }

    // MARK: - Failure wording

    func testConnectFailureMessageForEchoLink() {
        let withDetail = StationError(
            code: -24, text: "echolink error",
            detail: "echolink error: No station 9999 is logged in to EchoLink")
        XCTAssertEqual(
            connectFailureMessage(for: withDetail, node: "9999"),
            "No station 9999 is logged in to EchoLink.")

        let bare = StationError(code: -24, text: "echolink error")
        let message = connectFailureMessage(for: bare, node: "9999")
        XCTAssertTrue(message.contains("9999"), message)
        XCTAssertTrue(message.contains("EchoLink"), message)
        XCTAssertFalse(message.contains("-24"), "an error code is not a diagnosis")

        XCTAssertEqual(
            connectFailureMessage(for: CallSession.ConnectError.echolinkNotLoggedIn, node: "9999"),
            "Log in to EchoLink to call a station.")
        XCTAssertEqual(
            CallSession.ConnectError.echolinkUnknownStation("4321").errorDescription,
            "4321 isn’t in the EchoLink station list.")
    }
}

/// A Keychain that refuses every write, for the failure wording.
private final class FailingEchoLinkCredentialStore: EchoLinkCredentialStore {
    struct Refused: Error {}
    func password(callsign: String) -> String? { nil }
    func savedCallsigns() throws -> Set<String> { throw Refused() }
    func save(_ password: String, callsign: String) throws { throw Refused() }
    func remove(callsign: String) throws { throw Refused() }
}

/// An in-memory store that counts its reads, to show the checks the form
/// makes per keystroke do not reach the Keychain.
private final class CountingEchoLinkCredentialStore: EchoLinkCredentialStore {
    private let inner: InMemoryEchoLinkCredentialStore
    private(set) var passwordReads = 0
    private(set) var listReads = 0
    init(_ initial: [String: String]) { inner = InMemoryEchoLinkCredentialStore(initial) }
    func password(callsign: String) throws -> String? {
        passwordReads += 1
        return try inner.password(callsign: callsign)
    }
    func savedCallsigns() throws -> Set<String> {
        listReads += 1
        return try inner.savedCallsigns()
    }
    func save(_ password: String, callsign: String) throws {
        try inner.save(password, callsign: callsign)
    }
    func remove(callsign: String) throws { try inner.remove(callsign: callsign) }
}

/// An in-memory store whose reads can be made to fail: a Keychain that is
/// there but will not answer (locked, or the prompt refused).
private final class UnreadableEchoLinkCredentialStore: EchoLinkCredentialStore {
    struct Unreadable: Error {}
    private let inner: InMemoryEchoLinkCredentialStore
    var readsFail: Bool
    init(_ initial: [String: String], readsFail: Bool) {
        inner = InMemoryEchoLinkCredentialStore(initial)
        self.readsFail = readsFail
    }
    func password(callsign: String) throws -> String? {
        if readsFail { throw Unreadable() }
        return try inner.password(callsign: callsign)
    }
    func savedCallsigns() throws -> Set<String> {
        if readsFail { throw Unreadable() }
        return try inner.savedCallsigns()
    }
    func save(_ password: String, callsign: String) throws {
        try inner.save(password, callsign: callsign)
    }
    func remove(callsign: String) throws { try inner.remove(callsign: callsign) }
}
