// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#if os(macOS)
    import AstarCore
    import AstarStation
    import SwiftUI

    /// Settings → EchoLink: the callsign, the password (which can be shown),
    /// "Log in automatically", the location other stations see, and the
    /// login itself.
    ///
    /// **One copy of everything.** The callsign is `session.echolinkCallsign`,
    /// the password is the session's `echolinkPasswords` Keychain store and
    /// the tick is `session.echolinkAutoLogin`. This is the one place they
    /// are edited; the dial card and the station browser only offer Log in
    /// with what is saved here, and point back here when nothing is.
    ///
    /// **Saving.** Like the AllStarLink account above, edits save by
    /// themselves a moment after you stop typing: Settings is where you
    /// manage what is stored, so what you type here is stored.
    ///
    /// **The password** sits in this view's `@State` only while the section
    /// is on screen, and is cleared when it goes. It is never pre-filled: a
    /// saved one is read from the Keychain only when the operator presses
    /// "Show password", and never logged.
    struct EchoLinkSettingsView: View {
        @EnvironmentObject private var session: CallSession

        @State private var callsign = ""
        @State private var password = ""
        @State private var location = ""
        /// Whether the password field is plain text rather than dots.
        @State private var revealed = false
        /// A reveal waiting on the Keychain.
        @State private var revealing = false
        /// What a reveal put in the field, so leaving it untouched does not
        /// write the same password back.
        @State private var revealedPassword: String?
        @State private var saveTask: Task<Void, Never>?
        @State private var confirmingClear = false
        @State private var message: String?
        @FocusState private var passwordFocused: Bool

        private var trimmedCallsign: String {
            callsign.trimmingCharacters(in: .whitespacesAndNewlines)
        }

        /// Whether the Keychain has a password for the callsign in the field,
        /// from the session's cached set of saved callsigns (keys only).
        private var saved: Bool {
            !trimmedCallsign.isEmpty
                && session.echolinkHasSavedPassword(callsign: trimmedCallsign)
        }

        /// A password typed (or changed after a reveal) and not yet saved.
        private var passwordEdited: Bool {
            !password.isEmpty && password != revealedPassword
        }

        private var loggedOut: Bool { session.echolinkCallsignEditable }

        private var canLogIn: Bool {
            session.echolinkAvailable && loggedOut && !trimmedCallsign.isEmpty
                && (saved || passwordEdited)
        }

        var body: some View {
            Section("EchoLink") {
                VStack(alignment: .leading, spacing: 8) {
                    statusRow
                    callsignField
                    passwordField
                    autoLoginToggle
                    locationField
                    keychainNote
                    buttons
                }
                .font(.callout)
                .listRowSeparator(.hidden)
                // The scroll target for the dial card's "Open EchoLink
                // Settings…". On the row, not the Section: a List scrolls to
                // rows.
                .id(SettingsAnchor.echolink)
            }
            .onAppear {
                callsign = session.echolinkCallsign
                location = session.echolinkLocation
                // Keys only, on the worker; every "saved?" check after this
                // reads the session's copy.
                session.refreshEchoLinkSavedCallsigns()
            }
            // A login from the pane sets the callsign; follow it here unless
            // the operator is mid-edit.
            .onChange(of: session.echolinkCallsign) { committed in
                if saveTask == nil { callsign = committed }
            }
            .onChange(of: callsign) { _ in
                // The password field belongs to one callsign. A change of
                // callsign leaves it empty rather than quietly filing a
                // revealed password under a different registration.
                if revealedPassword != nil {
                    password = ""
                    revealedPassword = nil
                    revealed = false
                }
                scheduleSave()
            }
            .onChange(of: password) { _ in scheduleSave() }
            .onChange(of: location) { _ in scheduleSave() }
            // A Keychain failure arrives after "Saved" was said; the note
            // replaces it.
            .onChange(of: session.echolinkKeychainMessage) { note in
                if note != nil { message = nil }
            }
            // The password never outlives the section; an edit waiting on the
            // debounce is saved, not dropped.
            .onDisappear {
                if saveTask != nil { persist() }
                password = ""
                revealedPassword = nil
                revealed = false
            }
            .confirmationDialog(
                "Remove the saved EchoLink password?",
                isPresented: $confirmingClear, titleVisibility: .visible
            ) {
                Button("Clear password", role: .destructive, action: clear)
                Button("Cancel", role: .cancel) {}
            } message: {
                Text(
                    "This deletes the password for \(trimmedCallsign.uppercased()) from the Keychain "
                        + "and turns off Log in automatically. Your callsign and location stay."
                )
            }
        }

        // MARK: - Rows

        /// Logged in as X / not logged in, with the same dot the pane uses.
        private var statusRow: some View {
            HStack(spacing: 6) {
                switch session.echolinkLogin {
                case .loggedIn:
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                        .accessibilityHidden(true)
                case .loggingIn:
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityHidden(true)
                case .failed:
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                        .accessibilityHidden(true)
                case .loggedOut:
                    Image(systemName: "circle")
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                }
                Text(verbatim: statusText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .accessibilityElement(children: .combine)
        }

        private var statusText: String {
            if !session.echolinkAvailable { return "EchoLink isn’t in this build of astar." }
            let line = session.echolinkStatusLine
            if session.echolinkLogin == .loggedOut || line.isEmpty { return "Not logged in" }
            return line
        }

        private var callsignField: some View {
            VStack(alignment: .leading, spacing: 4) {
                SettingsField("Callsign") {
                    TextField("", text: $callsign)
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.username)
                        .disableAutocorrection(true)
                        .disabled(!loggedOut)
                        .onSubmit { persist() }
                        .accessibilityLabel("EchoLink callsign")
                }
                if !loggedOut {
                    Text("Log out to change the callsign.")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .settingsCaptionIndent()
                }
            }
        }

        private var passwordField: some View {
            VStack(alignment: .leading, spacing: 4) {
                SettingsField("Password") {
                    HStack(spacing: 4) {
                        Group {
                            if revealed {
                                TextField(passwordPlaceholder, text: $password)
                            } else {
                                SecureField(passwordPlaceholder, text: $password)
                            }
                        }
                        .textFieldStyle(.roundedBorder)
                        .textContentType(.password)
                        .disableAutocorrection(true)
                        .privacySensitive()
                        .focused($passwordFocused)
                        .onSubmit { persist() }
                        .accessibilityLabel("EchoLink password")
                        .accessibilityValue(saved && password.isEmpty ? "saved" : "")

                        Button(action: toggleReveal) {
                            if revealing {
                                ProgressView()
                                    .controlSize(.small)
                                    .frame(width: 16, height: 16)
                            } else {
                                Image(systemName: revealed ? "eye.slash" : "eye")
                                    .frame(width: 16, height: 16)
                                    .contentShape(Rectangle())
                            }
                        }
                        .buttonStyle(.borderless)
                        .disabled(revealing || (!saved && password.isEmpty))
                        .accessibilityLabel(revealed ? "Hide password" : "Show password")
                        .help(revealed ? "Hide password" : "Show password")
                    }
                }
                Label {
                    Text(
                        "EchoLink sends this password unencrypted. Use one you use nowhere else."
                    )
                } icon: {
                    Image(systemName: "lock.open")
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .settingsCaptionIndent()
            }
        }

        private var passwordPlaceholder: String {
            saved ? "Saved — re-enter to change" : ""
        }

        @ViewBuilder
        private var autoLoginToggle: some View {
            // Bound to the session's own setting, for the callsign it is for.
            let liveForThisCallsign =
                saved
                && session.echolinkHasSavedPassword(callsign: session.echolinkCallsign)
                && Self.same(trimmedCallsign, session.echolinkCallsign)
            VStack(alignment: .leading, spacing: 2) {
                Toggle(
                    "Log in automatically",
                    isOn: Binding(
                        get: { liveForThisCallsign && session.echolinkAutoLogin },
                        set: { session.setEchoLinkAutoLogin($0) })
                )
                .toggleStyle(.checkbox)
                .disabled(!liveForThisCallsign)
                .accessibilityHint(autoLoginCaption(live: liveForThisCallsign))
                Text(autoLoginCaption(live: liveForThisCallsign))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 20)  // under the checkbox's title
                    .accessibilityHidden(true)  // the hint above says it
            }
            .settingsCaptionIndent()
        }

        private func autoLoginCaption(live: Bool) -> String {
            live
                ? "Logs in with the saved password when astar starts. It never calls a station."
                : "Available once a password is saved for this callsign."
        }

        private var locationField: some View {
            VStack(alignment: .leading, spacing: 4) {
                SettingsField("Location") {
                    TextField("", text: $location)
                        .textFieldStyle(.roundedBorder)
                        .onSubmit { persist() }
                        .accessibilityLabel("EchoLink location")
                }
                Text("Shown beside your callsign in EchoLink’s station list, such as your town.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .fixedSize(horizontal: false, vertical: true)
                    .settingsCaptionIndent()
            }
        }

        /// The Keychain's failure sentence, shared with the pane.
        @ViewBuilder
        private var keychainNote: some View {
            if let note = session.echolinkKeychainMessage {
                Label(note, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .onAppear { announce(note) }
            }
        }

        private var buttons: some View {
            HStack(spacing: 10) {
                switch session.echolinkLogin {
                case .loggedIn:
                    Button("Log out", action: session.echolinkLogOut)
                        .buttonStyle(.bordered)
                        .accessibilityHint("Logs off the EchoLink directory")
                case .loggingIn:
                    Button("Cancel", action: session.echolinkLogOut)
                        .buttonStyle(.bordered)
                        .accessibilityHint("Stops logging in to EchoLink")
                case .failed where session.echolinkLoginRetrying:
                    Button("Cancel", action: session.echolinkLogOut)
                        .buttonStyle(.bordered)
                        .accessibilityHint("Stops logging in to EchoLink")
                case .failed, .loggedOut:
                    Button("Log in", action: logIn)
                        .buttonStyle(.bordered)
                        .disabled(!canLogIn)
                        .accessibilityHint("Logs in to the EchoLink directory")
                }
                if saved {
                    Button("Clear", role: .destructive) { confirmingClear = true }
                        .buttonStyle(.bordered)
                        .help("Removes this callsign’s EchoLink password from your Keychain")
                }
                if let message {
                    Text(message).font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
            }
        }

        // MARK: - Actions

        private func toggleReveal() {
            if revealed {
                revealed = false
                return
            }
            // Something typed is shown as it is; only an empty field reads the
            // saved one out of the Keychain.
            guard password.isEmpty, saved else {
                revealed = true
                return
            }
            revealing = true
            let forCallsign = trimmedCallsign
            session.echolinkRevealPassword(callsign: forCallsign) { found in
                revealing = false
                // The callsign changed while the Keychain answered.
                guard Self.same(forCallsign, trimmedCallsign), let found else { return }
                revealedPassword = found
                password = found
                revealed = true
            }
        }

        private static func same(_ a: String, _ b: String) -> Bool {
            a.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
                == b.trimmingCharacters(in: .whitespacesAndNewlines).uppercased()
        }

        /// Autosave ~0.7 s after the last edit, as the AllStarLink account does.
        private func scheduleSave() {
            message = nil
            saveTask?.cancel()
            saveTask = Task { @MainActor in
                try? await Task.sleep(nanoseconds: 700_000_000)
                guard !Task.isCancelled else { return }
                persist()
            }
        }

        private func persist() {
            saveTask?.cancel()
            saveTask = nil
            var changed = false
            if location != session.echolinkLocation {
                session.echolinkLocation = location
                changed = true
            }
            if loggedOut, !trimmedCallsign.isEmpty,
                !Self.same(trimmedCallsign, session.echolinkCallsign)
            {
                session.setEchoLinkCallsign(trimmedCallsign)
                changed = true
            }
            if passwordEdited, !trimmedCallsign.isEmpty {
                session.echolinkSavePassword(password, callsign: trimmedCallsign)
                // What is in the field is now what is saved.
                revealedPassword = password
                changed = true
            }
            if changed, session.echolinkKeychainMessage == nil {
                message = "Saved ✓"
                announce("Saved")
            }
        }

        private func logIn() {
            guard canLogIn else { return }
            let typed = passwordEdited ? password : ""
            persist()
            // Empty means the saved password. A typed one was just saved, and
            // the login saves it again only once the directory accepts it.
            session.echolinkLogIn(
                callsign: trimmedCallsign, password: typed,
                autoLogin: session.echolinkAutoLogin)
        }

        private func clear() {
            session.echolinkForgetPassword(callsign: trimmedCallsign)
            password = ""
            revealedPassword = nil
            revealed = false
            message = nil
        }

        private func announce(_ text: String) {
            AccessibilityAnnouncer.post(text, priority: .medium)
        }
    }

    #Preview {
        List {
            EchoLinkSettingsView()
        }
        .environmentObject(CallSession(station: NullStation()))
        .frame(width: 320, height: 420)
    }
#endif
