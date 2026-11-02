// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// What Settings → EchoLink asks of the session: change the callsign, save a
/// password without logging in, and read a saved one back to show it.
///
/// Every call lands on `FakeStation` and an in-memory password store. Nothing
/// here reaches the real Keychain or any network.
final class CallSessionEchoLinkSettingsTests: XCTestCase {

    private let password = "s3cret-echolink-pw"

    private func scratchDefaults() -> UserDefaults {
        let suite = "astar.tests.echolink-settings.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        return defaults
    }

    private func session(
        defaults: UserDefaults? = nil,
        passwords: EchoLinkCredentialStore = InMemoryEchoLinkCredentialStore()
    ) -> (CallSession, FakeStation) {
        let fake = FakeStation()
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            echolinkAvailable: true, echolinkActive: false, echolinkLoggedIn: false)
        let session = CallSession(
            station: fake, echolinkPasswords: passwords,
            userDefaults: defaults ?? scratchDefaults())
        session.echolinkWorker = { $0() }
        session.poll()
        return (session, fake)
    }

    /// The EchoLink item over a fake backend, so no test reaches the login
    /// Keychain.
    private func keychainItem(_ backend: FakeKeychainBackend) -> KeychainItem {
        KeychainItem(
            service: KeychainService.current,
            account: KeychainEchoLinkCredentialStore.defaultAccount,
            legacyService: KeychainService.legacy, backend: backend)
    }

    // MARK: - The callsign

    func testSettingTheCallsignNormalisesAndPersists() {
        let defaults = scratchDefaults()
        let (session, fake) = session(defaults: defaults)
        XCTAssertTrue(session.setEchoLinkCallsign("  kx7abc "))
        XCTAssertEqual(session.echolinkCallsign, "KX7ABC")
        XCTAssertEqual(defaults.string(forKey: "echolink.callsign"), "KX7ABC")
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty, "changing it never logs in")
    }

    /// "Log in automatically" belongs to a callsign with a saved password;
    /// moving to one without takes it off, moving between saved ones keeps it.
    func testMovingToACallsignWithNoPasswordTurnsOffAutoLogin() {
        let defaults = scratchDefaults()
        defaults.set("KX7ABC", forKey: "echolink.callsign")
        let passwords = InMemoryEchoLinkCredentialStore([
            "KX7ABC": password, "KX7ABC-L": "other-pw",
        ])
        let (session, _) = session(defaults: defaults, passwords: passwords)
        session.refreshEchoLinkSavedCallsigns()
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin)

        session.setEchoLinkCallsign("kx7abc-l")
        XCTAssertTrue(session.echolinkAutoLogin, "KX7ABC-L has a password too")

        session.setEchoLinkCallsign("W1AW")
        XCTAssertFalse(session.echolinkAutoLogin, "nothing saved for W1AW")
        XCTAssertFalse(defaults.bool(forKey: "echolink.autoLogin"))
    }

    func testTheCallsignIsLockedWhileLoggedIn() {
        let (session, fake) = session()
        session.setEchoLinkCallsign("KX7ABC")
        session.echolinkLogIn(callsign: "KX7ABC", password: password, autoLogin: false)
        XCTAssertFalse(session.echolinkCallsignEditable, "logging in")
        fake.echolinkStateValue = EchoLinkState(
            json: #"{"login":"logged_in","callsign":"KX7ABC"}"#)
        fake.snapshotToReturn = CallSnapshot(
            status: .idle, ptt: false, remotePTT: false, txDB: -60, rxDB: -60, rttMS: nil,
            echolinkAvailable: true, echolinkActive: false, echolinkLoggedIn: true)
        session.poll()
        XCTAssertEqual(session.echolinkLogin, .loggedIn)
        XCTAssertFalse(session.setEchoLinkCallsign("W1AW"))
        XCTAssertEqual(session.echolinkCallsign, "KX7ABC")
    }

    // MARK: - Saving a password

    func testSavingAPasswordWritesTheSameStoreTheLoginReads() throws {
        let passwords = InMemoryEchoLinkCredentialStore()
        let (session, fake) = session(passwords: passwords)
        session.echolinkSavePassword(password, callsign: " kx7abc ")
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password)
        XCTAssertTrue(session.echolinkHasSavedPassword(callsign: "KX7ABC"))
        XCTAssertNil(session.echolinkKeychainMessage)
        XCTAssertTrue(fake.echolinkLoginCalls.isEmpty, "saving never logs in")

        // With one saved, "Log in automatically" now takes.
        session.setEchoLinkCallsign("KX7ABC")
        session.setEchoLinkAutoLogin(true)
        XCTAssertTrue(session.echolinkAutoLogin)

        // The pane's empty-field login uses what Settings saved.
        session.echolinkLogIn(callsign: "KX7ABC", password: "", autoLogin: true)
        XCTAssertEqual(fake.echolinkLoginCalls.last?.password, password)
    }

    func testABlankPasswordOrCallsignSavesNothing() throws {
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, _) = session(passwords: passwords)
        session.echolinkSavePassword("   ", callsign: "KX7ABC")
        session.echolinkSavePassword(password, callsign: "  ")
        XCTAssertEqual(try passwords.password(callsign: "KX7ABC"), password, "left alone")
        XCTAssertEqual(try passwords.savedCallsigns(), ["KX7ABC"])
    }

    /// The `astar-dmr-keychain-wipe` pattern: a Keychain that will not be
    /// read must not be written over with a map holding only the new one.
    func testASaveOverAnUnreadableKeychainWipesNothing() throws {
        let backend = FakeKeychainBackend()
        let passwords = KeychainEchoLinkCredentialStore(item: keychainItem(backend))
        try passwords.save("kept-pw", callsign: "W1AW")
        let before = backend.items
        let writesBefore = backend.writes.count
        backend.readFails = true
        let (session, _) = session(passwords: passwords)
        session.echolinkSavePassword(password, callsign: "KX7ABC")
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t save to the Keychain.")
        XCTAssertFalse(session.echolinkHasSavedPassword(callsign: "KX7ABC"))
        XCTAssertEqual(backend.writes.count, writesBefore, "nothing written over it")
        XCTAssertEqual(backend.items, before, "W1AW's password is not lost")
        backend.readFails = false
        XCTAssertEqual(try passwords.password(callsign: "W1AW"), "kept-pw")
        XCTAssertFalse(session.echolinkKeychainMessage?.contains(password) ?? false)
    }

    // MARK: - Showing a saved password

    func testRevealHandsBackTheSavedPassword() {
        let passwords = InMemoryEchoLinkCredentialStore(["KX7ABC": password])
        let (session, _) = session(passwords: passwords)
        var shown: String?
        session.echolinkRevealPassword(callsign: "kx7abc") { shown = $0 }
        XCTAssertEqual(shown, password)

        var none: String? = "unset"
        session.echolinkRevealPassword(callsign: "W1AW") { none = $0 }
        XCTAssertNil(none, "nothing saved for W1AW")
        XCTAssertNil(session.echolinkKeychainMessage)
    }

    func testAnUnreadableKeychainRevealsNothingAndSaysSo() {
        let backend = FakeKeychainBackend()
        backend.readFails = true
        let (session, _) = session(
            passwords: KeychainEchoLinkCredentialStore(item: keychainItem(backend)))
        var shown: String? = "unset"
        session.echolinkRevealPassword(callsign: "KX7ABC") { shown = $0 }
        XCTAssertNil(shown)
        XCTAssertEqual(session.echolinkKeychainMessage, "Couldn’t read the Keychain.")
    }

    /// Neither a save nor a reveal leaves the password in any property of
    /// the session, or anywhere in the defaults domain.
    func testSettingsPathsNeverPublishThePassword() {
        let defaults = scratchDefaults()
        let (session, _) = session(defaults: defaults)
        session.setEchoLinkCallsign("KX7ABC")
        session.echolinkSavePassword(password, callsign: "KX7ABC")
        session.echolinkRevealPassword(callsign: "KX7ABC") { _ in }
        for child in Mirror(reflecting: session).children {
            guard let label = child.label, label != "station", label != "echolinkPasswords"
            else { continue }
            var rendered = ""
            dump(child.value, to: &rendered)
            XCTAssertFalse(rendered.contains(password), "\(label) holds the password")
        }
        for (key, value) in defaults.dictionaryRepresentation() {
            XCTAssertFalse("\(value)".contains(password), "\(key) holds the password")
        }
        XCTAssertTrue(
            ConfigArchive.settingsSlice(from: defaults.dictionaryRepresentation()).keys
                .allSatisfy { !$0.hasPrefix("echolink.") },
            "no EchoLink preference travels in an .astarconfig")
    }
}
