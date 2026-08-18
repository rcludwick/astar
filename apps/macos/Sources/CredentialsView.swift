// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarCore
import SwiftUI

/// AllStar account credentials for the WebTransceiver (WT) path (au-dee9).
///
/// The WT token mint logs into the **AllStarLink web portal** (`login.php`) with
/// your callsign + **account password**, then fetches a transceiver token for a
/// node your account owns. Per astar `PortalCredentials`, the password is
/// the *portal ACCOUNT password — NOT the node's IAX secret*. The three fields
/// map to `Credentials` as: Callsign → portalUser, Account password → portalPass,
/// Node number → portalNode. The password lives only in the Keychain, is consumed
/// into `StationConfig` at station build, and is never pre-filled or logged.
struct CredentialsView: View {
    @EnvironmentObject private var session: CallSession
    private let store = KeychainCredentialStore()

    @State private var callsign = ""
    @State private var node = ""
    @State private var accountPassword = ""
    @State private var saved = false
    @State private var message: String?
    /// Debounces autosave so we persist (and rebuild the station) once you stop
    /// typing, not on every keystroke.
    @State private var saveTask: Task<Void, Never>?
    /// True while a token-mint test is in flight (shows a spinner).
    @State private var testing = false
    /// Outcome of the last token-mint test — tints the Test button green/red.
    @State private var testResult: TokenTestResult = .untested
    private enum TokenTestResult { case untested, success, failure }
    /// Gates the destructive "Clear" action behind a confirmation.
    @State private var confirmingClear = false

    /// iax-6b58 landed `Station.testMintToken()` (mint + discard, no call) and
    /// it's vendored, so the Test button is live.
    private static let tokenTestAvailable = true

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("AllStarLink account").font(.subheadline.weight(.semibold))

            // What it costs you to leave this blank (astar-4e8a). The red field
            // below says the password is missing; this says what that means.
            if CredentialsValidation.showsAllStarUnavailable(hasCredentials: saved) {
                HStack(alignment: .top, spacing: 6) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(CredentialsValidation.allStarUnavailable)
                            .font(.caption.weight(.medium))
                        Text(CredentialsValidation.allStarUnavailableDetail)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 0)
                }
                .padding(8)
                .background(
                    Color.orange.opacity(0.12),
                    in: RoundedRectangle(cornerRadius: 6, style: .continuous)
                )
                // One announcement, not three fragments.
                .accessibilityElement(children: .combine)
            }
            Text(
                saved
                    ? "Saved. Toggle “Use my account” when you dial for an authenticated WebTransceiver connection."
                    : "Sign in with your allstarlink.org account to dial via the WebTransceiver. Changes save automatically."
            )
            .font(.caption2)
            .foregroundStyle(.secondary)

            TextField("Callsign", text: $callsign)
                .textFieldStyle(.roundedBorder)
            TextField("Node number", text: $node)
                .textFieldStyle(.roundedBorder)
            SecureField(
                saved ? "Account password (re-enter to change)" : "Account password",
                text: $accountPassword
            )
            .textFieldStyle(.roundedBorder)
            // Red outline when the password is missing or the portal rejected it
            // (astar-4e8a). Drawn as an overlay rather than a background so the
            // native rounded-border field keeps its focus ring.
            .overlay(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .stroke(Color.red, lineWidth: passwordStatus.isInvalid ? 1.5 : 0)
            )
            .animation(.easeInOut(duration: 0.15), value: passwordStatus)
            .accessibilityValue(passwordStatus.message ?? "")
            // The reason replaces the standing hint while something is wrong —
            // two captions stacked under one field is noise, and the red one is
            // the one that matters.
            if let problem = passwordStatus.message {
                Label(problem, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption2)
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .transition(.opacity)
            } else {
                Text("Your allstarlink.org account password — not the node’s IAX secret.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            HStack(spacing: 10) {
                if saved {
                    Button("Test", action: testTokenMint)
                        .buttonStyle(.bordered)
                        .tint(testTint)
                        .disabled(!Self.tokenTestAvailable || testing)
                        .help("Validate WebTransceiver token minting without placing a call")
                    Button("Clear", role: .destructive) { confirmingClear = true }
                        .buttonStyle(.bordered)
                }
                if testing { ProgressView().controlSize(.small) }
                if let message {
                    Text(message).font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
            }

            // The mint-only engine entry point (iax-6b58) isn't vendored yet, so
            // the Test button is disabled until then (the only mint path today is
            // connectWT, which also places a call — we won't briefly key on air
            // just to test). Flip `tokenTestAvailable` once it lands.
            if saved && !Self.tokenTestAvailable {
                Text("Token test needs an engine update (iax-6b58, in progress).")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            // M17 callsign (astar-c2e5 Task 9): a separate network from the
            // AllStarLink account above — M17 transmits this callsign
            // verbatim in every frame, so it's collected here as well as in
            // the dial card's own prompt (whichever the user reaches first).
            // Bound straight to `session.m17Callsign`, which persists itself
            // (`CallSession`'s `didSet`) — no separate save step needed.
            // Gated on `session.m17Available` — latent (no new UI at all)
            // until the engine build actually supports M17, matching the
            // network picker's own gate.
            if session.m17Available {
                Divider()
                Text("M17").font(.subheadline.weight(.semibold))
                TextField("Callsign (M17)", text: $session.m17Callsign)
                    .textFieldStyle(.roundedBorder)
                    .onChange(of: session.m17Callsign) { value in
                        let upper = value.uppercased()
                        if upper != value { session.m17Callsign = upper }
                    }
                Text("Your callsign — M17 transmits it with every packet you send.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
        .onAppear(perform: loadExisting)
        .onChange(of: callsign) { _ in scheduleSave() }
        .onChange(of: node) { _ in scheduleSave() }
        .onChange(of: accountPassword) { _ in scheduleSave() }
        // Don't keep the secret in memory once you leave the panel; it's already
        // in the Keychain. Re-entry is required to change it again (we never
        // pre-fill the password).
        .onDisappear {
            accountPassword = ""
            saveTask?.cancel()
        }
        .confirmationDialog(
            "Remove your AllStarLink account?",
            isPresented: $confirmingClear, titleVisibility: .visible
        ) {
            Button("Clear account", role: .destructive, action: clear)
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "This deletes your saved callsign, node, and password from the Keychain. You can re-enter them anytime."
            )
        }
    }

    /// How to draw the password field (astar-4e8a). `saved` is what keeps a
    /// working account from being flagged: astar never pre-fills the password, so
    /// a blank box there means "unchanged", not "missing".
    private var passwordStatus: CredentialFieldStatus {
        CredentialsValidation.password(
            text: accountPassword,
            hasSavedCredentials: saved,
            portalRejected: testResult == .failure)
    }

    private var canSave: Bool {
        !callsign.trimmingCharacters(in: .whitespaces).isEmpty
            && !node.trimmingCharacters(in: .whitespaces).isEmpty
            && !accountPassword.isEmpty
    }

    private func loadExisting() {
        if let c = store.load() {
            callsign = c.portalUser
            node = c.portalNode
            saved = true
        }
    }

    /// Autosave: persist ~0.7 s after the last edit, once all fields are present.
    private func scheduleSave() {
        message = nil
        testResult = .untested  // creds changed → any prior test result is stale
        saveTask?.cancel()
        guard canSave else { return }
        saveTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 700_000_000)
            guard !Task.isCancelled else { return }
            persist()
        }
    }

    private func persist() {
        let creds = Credentials(
            portalUser: callsign.trimmingCharacters(in: .whitespaces),
            portalPass: accountPassword,
            portalNode: node.trimmingCharacters(in: .whitespaces)
        )
        do {
            try store.save(creds)
            // Rebuilding the station re-applies construction-time knobs — read
            // the persisted audio settings so devices + gains survive a
            // credential save (the codec policy is always prefer_slin16).
            let (station, hasCredentials) = CallSession.makeStation(
                credentials: creds, audio: UserDefaultsAudioSettingsStore().load())
            session.reconfigure(station: station, hasCredentials: hasCredentials)
            saved = true
            message = "Saved ✓"
            announce("Saved")
        } catch {
            message = "Couldn’t save to the Keychain."
            announce("Couldn’t save to the Keychain.")
        }
    }

    /// Validate WT token minting (login + mint + discard) without placing a call
    /// (astar-2fde). The mint is a blocking network round-trip, so run it off the
    /// main thread and hop back to publish the result.
    private func testTokenMint() {
        testing = true
        message = nil
        testResult = .untested
        let session = self.session
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try session.testTokenMint()
                DispatchQueue.main.async {
                    testing = false
                    testResult = .success
                    message = "Token minted ✓ — credentials valid."
                    announce("Token minted — credentials valid.")
                }
            } catch {
                DispatchQueue.main.async {
                    testing = false
                    testResult = .failure
                    message = "Token test failed — check callsign, password, and node."
                    announce("Token test failed — check callsign, password, and node.")
                }
            }
        }
    }

    /// Green after a successful mint, red after a failure, default otherwise.
    private var testTint: Color {
        switch testResult {
        case .success: return .green
        case .failure: return .red
        case .untested: return .accentColor
        }
    }

    private func clear() {
        try? store.clear()
        let (station, hasCredentials) = CallSession.makeStation(
            credentials: nil, audio: UserDefaultsAudioSettingsStore().load())
        session.reconfigure(station: station, hasCredentials: hasCredentials)
        callsign = ""
        node = ""
        accountPassword = ""
        saved = false
        message = "Cleared."
        announce("Cleared.")
    }

    /// Posts `text` as a VoiceOver announcement (astar-b167, audit F21): the
    /// save/test outcome messages above are view-local state that never
    /// reaches `CallSession`, so they go through the announcer's static
    /// one-off helper rather than a per-session planner. macOS-only (AppKit);
    /// a no-op on iOS until that platform gets its own announcer.
    private func announce(_ text: String) {
        #if os(macOS)
            AccessibilityAnnouncer.post(text, priority: .medium)
        #endif
    }
}

#Preview {
    CredentialsView()
        .environmentObject(CallSession(station: NullStation()))
        .frame(width: 288)
        .padding()
}
