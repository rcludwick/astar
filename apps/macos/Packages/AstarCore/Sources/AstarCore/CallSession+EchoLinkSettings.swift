// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// What Settings → EchoLink asks of the session, beyond what the pane's login
/// form already uses: change the callsign without logging in, save a password
/// straight to the Keychain, and read one back to show it.
///
/// Both views work on the same three things — ``echolinkCallsign``, the
/// `echolinkPasswords` store and ``echolinkAutoLogin`` — so a change made in
/// one shows in the other. There is no second copy of any of them.
///
/// **The password**, here as in `CallSession+EchoLink.swift`, never lands in a
/// property of the session. A save carries it inside the worker closure and
/// nowhere else; a reveal hands it to the caller's completion and forgets it.
extension CallSession {
    /// Whether the EchoLink callsign can change right now: only while no
    /// login is holding it (logged out, or a login the directory refused).
    public var echolinkCallsignEditable: Bool {
        echolinkLogin == .loggedOut || echolinkLoginRefused
    }

    /// Set ``echolinkCallsign`` to `callsign`, trimmed and upper-cased,
    /// without logging in. Refused (returns `false`) while a login holds the
    /// callsign — see ``echolinkCallsignEditable``.
    ///
    /// "Log in automatically" belongs to the callsign, so moving to one with
    /// no password saved takes it off: it could not be honoured at launch.
    @discardableResult
    public func setEchoLinkCallsign(_ callsign: String) -> Bool {
        guard echolinkCallsignEditable else { return false }
        let callsign = Self.normaliseEchoLinkCallsign(callsign)
        guard callsign != echolinkCallsign else { return true }
        echolinkCallsign = callsign
        if echolinkAutoLogin && !echolinkHasSavedPassword(callsign: callsign) {
            echolinkAutoLogin = false
        }
        return true
    }

    /// Save `password` for `callsign` in the Keychain now, on the EchoLink
    /// worker, without logging in. A blank password or callsign saves
    /// nothing (blank counts as none).
    ///
    /// The store refuses to write when it cannot read what is already there,
    /// so a Keychain that will not answer never costs the other callsigns
    /// their passwords; the failure is said in `echolinkKeychainMessage`.
    public func echolinkSavePassword(_ password: String, callsign: String) {
        let callsign = Self.normaliseEchoLinkCallsign(callsign)
        guard let store = echolinkPasswords, !callsign.isEmpty,
            !password.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return }
        echolinkWorker { [weak self] in
            var failed = false
            do {
                try store.save(password, callsign: callsign)
            } catch {
                // The Keychain's own status, never the password.
                NSLog("[astar] echolink: keychain save failed: %@", "\(error)")
                failed = true
            }
            Self.onMainForSettings {
                guard let self else { return }
                if failed {
                    self.echolinkKeychainMessage = "Couldn’t save to the Keychain."
                    return
                }
                self.echolinkKeychainMessage = nil
                self.echolinkSavedCallsigns.insert(callsign)
            }
        }
    }

    /// Read the saved password for `callsign` out of the Keychain, on the
    /// EchoLink worker, and hand it to `completion` on the main thread — for
    /// Settings' "Show password" and nothing else.
    ///
    /// `completion` gets `nil` when none is saved, and also when the Keychain
    /// will not answer; that case also says "Couldn’t read the Keychain." in
    /// `echolinkKeychainMessage`. The session keeps no copy.
    public func echolinkRevealPassword(
        callsign: String, _ completion: @escaping (String?) -> Void
    ) {
        let callsign = Self.normaliseEchoLinkCallsign(callsign)
        guard let store = echolinkPasswords, !callsign.isEmpty else {
            completion(nil)
            return
        }
        echolinkWorker { [weak self] in
            var found: String?
            var unreadable = false
            do {
                found = try store.password(callsign: callsign)
            } catch {
                NSLog("[astar] echolink: keychain read failed: %@", "\(error)")
                unreadable = true
            }
            Self.onMainForSettings {
                if let self {
                    if unreadable {
                        self.echolinkKeychainMessage = Self.echolinkKeychainUnreadable
                    } else if self.echolinkKeychainMessage == Self.echolinkKeychainUnreadable {
                        self.echolinkKeychainMessage = nil
                    }
                }
                completion(found)
            }
        }
    }

    /// Run `body` on the main thread: inline when already there, queued
    /// otherwise. The same as the login path's own helper, which is private
    /// to its file.
    private static func onMainForSettings(_ body: @escaping () -> Void) {
        if Thread.isMainThread {
            body()
        } else {
            DispatchQueue.main.async(execute: body)
        }
    }
}
