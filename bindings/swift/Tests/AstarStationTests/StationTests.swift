// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// StationTests.swift — offline unit tests for the AstarStation Swift wrapper.
//
// No network, no audio hardware: every test exercises only the idle surface of
// a freshly-constructed station. The secret-free invariant is asserted directly.

import XCTest

@testable import AstarStation

final class StationTests: XCTestCase {

    func testNewStationSnapshotIsIdle() throws {
        let st = try Station()
        let snap = try st.snapshot()
        XCTAssertEqual(snap.status, .idle)
        XCTAssertNil(snap.rttMS, "fresh station should report unknown rtt")
        XCTAssertFalse(snap.ptt)
        XCTAssertFalse(snap.remotePTT)
        // TX health counters start at zero on a fresh station (iax-9e55).
        XCTAssertEqual(snap.txReanchors, 0)
        XCTAssertEqual(snap.txCaptureOverruns, 0)
        // No call → no negotiated codec (iax-3e53).
        XCTAssertNil(snap.negotiatedFormat)
    }

    // MARK: Codec policy (iax-3e53)

    func testCodecPolicyValidStringsConstruct() throws {
        // nil (the default), "", "default", and every documented policy string
        // all construct successfully.
        for policy: String? in
            [nil, "", "default", "ulaw_only", "allow_slin", "prefer_slin", "prefer_slin16"]
        {
            XCTAssertNoThrow(
                try Station(config: StationConfig(codecPolicy: policy)),
                "codecPolicy \(String(describing: policy)) must construct"
            )
        }
    }

    func testCodecPolicyInvalidStringThrows() throws {
        // An unknown policy string fails construction — never a silent default.
        XCTAssertThrowsError(try Station(config: StationConfig(codecPolicy: "opus"))) { error in
            XCTAssertTrue(error is StationError, "expected StationError, got \(error)")
        }
    }

    func testVoiceFormatFriendlyRepresentation() throws {
        // Raw values are the IAX2 format bits carried across the C-ABI.
        XCTAssertEqual(VoiceFormat(rawValue: 4), .g711u)
        XCTAssertEqual(VoiceFormat(rawValue: 8), .g711a)
        XCTAssertEqual(VoiceFormat(rawValue: 64), .slin)
        XCTAssertEqual(VoiceFormat(rawValue: 32768), .slin16)
        // 0 = none/unknown → no case (Snapshot surfaces it as nil).
        XCTAssertNil(VoiceFormat(rawValue: 0))
        // Friendly, display-ready descriptions; slin16 reads as wideband.
        XCTAssertTrue(VoiceFormat.slin16.description.contains("slin16"))
        XCTAssertTrue(VoiceFormat.slin16.description.contains("wideband"))
        XCTAssertTrue(VoiceFormat.g711u.description.contains("8 kHz"))
    }

    func testSetPTTWhileIdleThrowsNotConnected() throws {
        let st = try Station()
        XCTAssertThrowsError(try st.setPTT(true)) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            // IAX_ERR_NOT_CONNECTED == -3
            XCTAssertEqual(e.code, -3)
        }
    }

    func testSendDTMFWhileIdleThrowsNotConnected() throws {
        let st = try Station()
        // Valid DTMF keys — the full 16-key set, A-D included (iax-47ae) — with
        // no active call → not connected (-3).
        for key: Character in ["5", "*", "#", "A", "D"] {
            XCTAssertThrowsError(try st.sendDTMF(key)) { error in
                guard let e = error as? StationError else {
                    return XCTFail("expected StationError, got \(error)")
                }
                XCTAssertEqual(e.code, -3, "\(key) is a valid DTMF key → not-connected")
            }
        }
    }

    func testSendDTMFRejectsNonDialerCharacter() throws {
        let st = try Station()
        // Not one of the 16 DTMF keys (0-9, *, #, A-D) → invalid digit (-15),
        // checked before the not-connected guard.
        for bad: Character in ["E", "x", " ", "+", "é"] {
            XCTAssertThrowsError(try st.sendDTMF(bad)) { error in
                guard let e = error as? StationError else {
                    return XCTFail("expected StationError, got \(error)")
                }
                // IAX_ERR_INVALID_DIGIT == -15
                XCTAssertEqual(e.code, -15, "\(bad) should be rejected as an invalid digit")
            }
        }
    }

    func testSetDTMFModeDoesNotThrow() throws {
        // iax-7fff: the emission-mode setter stores the mode on the station
        // (applies to future digits), so setting it while idle never throws.
        let st = try Station()
        XCTAssertNoThrow(try st.setDTMFMode(.protocolFrames))
        XCTAssertNoThrow(try st.setDTMFMode(.inBand))
    }

    func testSendDTMFSequenceWhileIdleThrowsNotConnected() throws {
        // iax-4b7a: a valid command with no active call → not connected (-3),
        // and nothing is queued.
        let st = try Station()
        XCTAssertThrowsError(try st.sendDTMF(sequence: "*3AD")) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            XCTAssertEqual(e.code, -3)
        }
    }

    func testSendDTMFSequenceRejectsInvalidCharacterWithoutSending() throws {
        // Validation is all-or-nothing and happens before the not-connected
        // check: one bad character (or an empty command) → invalid digit (-15).
        let st = try Station()
        for bad in ["*3x", "", "*3 5", "é5"] {
            XCTAssertThrowsError(try st.sendDTMF(sequence: bad)) { error in
                guard let e = error as? StationError else {
                    return XCTFail("expected StationError, got \(error)")
                }
                XCTAssertEqual(e.code, -15, "\(bad) must reject as invalid digit")
            }
        }
    }

    func testCancelDTMFIsIdleSafe() throws {
        // iax-4b7a: cancel with nothing playing is a no-op, never an error.
        let st = try Station()
        XCTAssertNoThrow(try st.cancelDTMF())
    }

    func testSnapshotReportsIdleDTMFProgress() throws {
        // No sequence on a fresh station: progress is 0/0.
        let st = try Station()
        let snap = try st.snapshot()
        XCTAssertEqual(snap.dtmfPlayed, 0)
        XCTAssertEqual(snap.dtmfTotal, 0)
    }

    func testNextEventWhileIdleIsNil() throws {
        let st = try Station()
        XCTAssertNil(try st.nextEvent())
    }

    func testGainsDoNotThrow() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.setInputGain(0.5))
        XCTAssertNoThrow(try st.setOutputGain(1.5))
    }

    func testMicDspTogglesDoNotThrow() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.setCompression(true))
        XCTAssertNoThrow(try st.setCompression(false))
        XCTAssertNoThrow(try st.setNoiseReduction(true))
        XCTAssertNoThrow(try st.setNoiseReduction(false))
        // Compression level: in-range and out-of-range (clamped) never throw.
        XCTAssertNoThrow(try st.setCompressionLevel(0.5))
        XCTAssertNoThrow(try st.setCompressionLevel(2.0))
        XCTAssertNoThrow(try st.setCompressionLevel(-1.0))
    }

    func testRxCompressionTogglesDoNotThrow() throws {
        // iax-a4e7 PHASE 1: RX/output compression, mirroring the mic-side
        // compression toggle test.
        let st = try Station()
        XCTAssertNoThrow(try st.setRxCompression(true))
        XCTAssertNoThrow(try st.setRxCompression(false))
        // Strength: in-range and out-of-range (clamped) never throw.
        XCTAssertNoThrow(try st.setRxCompressionLevel(0.5))
        XCTAssertNoThrow(try st.setRxCompressionLevel(2.0))
        XCTAssertNoThrow(try st.setRxCompressionLevel(-1.0))
    }

    func testTxTrimDoesNotThrow() throws {
        let st = try Station()
        // TX trim: in-range and out-of-range (clamped) never throw.
        XCTAssertNoThrow(try st.setTxTrim(0.5))
        XCTAssertNoThrow(try st.setTxTrim(2.5))
        XCTAssertNoThrow(try st.setTxTrim(-1.0))
    }

    func testDisconnectWhileIdleIsOK() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.disconnect())  // idempotent no-op
    }

    func testConnectWTWithoutPortalThrowsPortal() throws {
        let st = try Station()  // no portal config
        XCTAssertThrowsError(try st.connectWT(destNode: "55553")) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            // IAX_ERR_PORTAL == -5
            XCTAssertEqual(e.code, -5)
        }
    }

    // MARK: Mode + Node (offline)

    func testNewStationModeIsWT() throws {
        let st = try Station()
        XCTAssertEqual(try st.mode(), .wt)
        XCTAssertEqual(try st.snapshot().mode, .wt)
    }

    func testListenOnlyNodeConfigDoesNotThrowOrSwitchMode() throws {
        let st = try Station()
        XCTAssertNoThrow(
            try st.setNodeConfig(NodeConfig(bind: "127.0.0.1:0", answer: .manual, auth: .off))
        )
        // Listen-only config does not switch the operating mode.
        XCTAssertEqual(try st.mode(), .wt)
    }

    func testNodeConfigBadBindThrowsResolve() throws {
        let st = try Station()
        XCTAssertThrowsError(try st.setNodeConfig(NodeConfig(bind: "not-an-address"))) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            // IAX_ERR_RESOLVE == -6
            XCTAssertEqual(e.code, -6)
        }
    }

    func testNodeConfigRegistrarWithoutUserThrowsNull() throws {
        let st = try Station()
        let cfg = NodeConfig(bind: "0.0.0.0:4569", registrar: "127.0.0.1:4569", registerUser: nil)
        XCTAssertThrowsError(try st.setNodeConfig(cfg)) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            // IAX_ERR_NULL == -2
            XCTAssertEqual(e.code, -2)
        }
    }

    func testAnswerAndRejectInWTModeThrowNotConnected() throws {
        let st = try Station()
        for op in [st.answer, st.reject] {
            XCTAssertThrowsError(try op()) { error in
                guard let e = error as? StationError else {
                    return XCTFail("expected StationError, got \(error)")
                }
                // IAX_ERR_NOT_CONNECTED == -3
                XCTAssertEqual(e.code, -3)
            }
        }
    }

    func testIncomingFromIsEmptyInitially() throws {
        let st = try Station()
        XCTAssertEqual(try st.incomingFrom(), "")
    }

    func testSetCredentialResolverDoesNotThrowAndStillSnapshots() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.setCredentialResolver { _ in "be04-secret" })
        // Station still works after installing the resolver.
        XCTAssertEqual(try st.snapshot().status, .idle)
        XCTAssertEqual(try st.mode(), .wt)
        // Replacing the resolver releases the prior box and does not crash/leak.
        XCTAssertNoThrow(try st.setCredentialResolver { _ in "replacement" })
        XCTAssertEqual(try st.snapshot().status, .idle)
    }

    /// Secret-free guard: nothing the app can read back mentions a secret. The
    /// secret is only ever a connect/init in-arg.
    func testSecretFreeSurface() throws {
        let secret = "topsecret-please-do-not-leak"
        let cfg = StationConfig(portalPass: secret, secret: secret)
        let st = try Station(config: cfg)
        // Install a resolver that yields a secret: it must never surface in any
        // readable representation either (the closure is the only secret holder).
        try st.setCredentialResolver { _ in secret }
        // Set a node config (secret-free by construction) to exercise that path.
        try st.setNodeConfig(NodeConfig(bind: "127.0.0.1:0"))
        let snap = try st.snapshot()

        var haystacks: [String] = [st.description, "\(snap)", "\(try st.mode())"]
        haystacks.append(try st.incomingFrom())
        if let ev = try st.nextEvent() { haystacks.append("\(ev)") }

        for h in haystacks {
            XCTAssertFalse(h.contains(secret), "secret leaked in: \(h)")
            XCTAssertFalse(h.lowercased().contains("secret"), "'secret' leaked in: \(h)")
            XCTAssertFalse(h.lowercased().contains("password"), "'password' leaked in: \(h)")
        }

        // The StationError text is also secret-free (generic 'static C strings).
        do {
            try st.setPTT(true)
        } catch let e as StationError {
            XCTAssertFalse(e.description.lowercased().contains("secret"))
            XCTAssertFalse(e.description.lowercased().contains("password"))
            XCTAssertFalse(e.description.contains(secret))
        }
    }

    // MARK: M17 (iax-f2b8 Task 5)

    func testFreshStationSnapshotReportsNoM17Session() throws {
        let st = try Station()
        let snap = try st.snapshot()
        XCTAssertFalse(snap.m17Active, "a never-connected station must report no M17 session")
        // `m17Available` depends on this machine's codec2 install; only its
        // type is meaningful here (it is a plain `Bool`, never optional).
        _ = snap.m17Available
    }

    func testConnectM17NonAsciiModuleThrowsM17Error() throws {
        let st = try Station()
        XCTAssertThrowsError(
            try st.connectM17(host: "127.0.0.1", module: "é", callsign: "N0CALL")
        ) { error in
            guard let e = error as? StationError else {
                return XCTFail("expected StationError, got \(error)")
            }
            // IAX_ERR_M17 == -18. Rejected binding-side before crossing the
            // C-ABI (mirrors sendDTMF's non-ASCII guard).
            XCTAssertEqual(e.code, -18)
        }
    }

    func testConnectM17HermeticBranchOnAvailability() throws {
        // `m17Available` is a codec2-only probe; a real `Station` resolves its
        // configured mic/speaker BEFORE the engine ever checks for codec2 (see
        // `M17Session::connect`), so a headless/sandboxed test runner with no
        // accessible audio device can throw IAX_ERR_AUDIO (-7) regardless of
        // codec2 availability. Assert only what `m17Available` actually
        // guarantees: when true, codec2 was found, so the call must never
        // fail as an M17 error; when false, it may fail as either an M17 or
        // an Audio error (whichever the engine reaches first), but never
        // succeed.
        let st = try Station()
        let available = try st.snapshot().m17Available
        do {
            try st.connectM17(host: "127.0.0.1", port: 17_000, module: "A", callsign: "N0CALL")
            XCTAssertNoThrow(try st.m17Disconnect(), "clean up an accepted session")
        } catch let e as StationError {
            if available {
                XCTAssertNotEqual(
                    e.code, -18, "codec2 is available; must not fail as an M17 error"
                )
            } else {
                XCTAssertTrue(
                    e.code == -18 || e.code == -7,
                    "expected M17 (-18) or Audio (-7) when unavailable, got \(e.code)"
                )
            }
        }
    }

    func testM17DisconnectIdleNeverThrows() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.m17Disconnect())
    }

    func testSetCodecDirsNeverThrows() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.setCodecDirs([]))
        XCTAssertNoThrow(try st.setCodecDirs(["/opt/app/lib", "/usr/local/lib"]))
        XCTAssertNoThrow(try st.setCodecDirs([]))
    }
    // MARK: D-Star (iax-4c8e)

    func testFreshStationHasNoDStarSession() throws {
        let st = try Station()
        XCTAssertFalse(try st.snapshot().dstarActive)
        // dstarAvailable is deliberately not asserted: it reports whether a
        // ThumbDV is plugged into the machine running the test.
        XCTAssertNil(try st.dstarState(), "no session → no state")
    }

    func testDStarDisconnectIsIdempotentWhileIdle() throws {
        let st = try Station()
        XCTAssertNoThrow(try st.dstarDisconnect())
        XCTAssertNoThrow(try st.dstarDisconnect())
    }

    func testConnectDStarRejectsANonASCIIModule() throws {
        let st = try Station()
        // Rejected in Swift, before the call ever crosses the C-ABI — and so
        // before the engine would scan serial ports looking for a dongle.
        XCTAssertThrowsError(
            try st.connectDStar(host: "127.0.0.1", module: "\u{00e9}", callsign: "N0CALL")
        )
    }

    func testDStarStateDecodesTheEngineJSON() throws {
        let json = """
            {"link":"linked","talker":"AJ7HR","slow_text":"hi there",\
            "backend":"thumbdv","tx_capable":true,"ptt":false,\
            "tx_db":-60.0,"rx_db":-31.25,"input_db":-45.5}
            """
        let state = try XCTUnwrap(DStarState(json: json))
        XCTAssertEqual(state.link, .linked)
        XCTAssertEqual(state.talker, "AJ7HR")
        XCTAssertEqual(state.slowText, "hi there")
        XCTAssertEqual(state.backend, .thumbdv)
        XCTAssertTrue(state.txCapable)
        XCTAssertFalse(state.ptt)
        XCTAssertEqual(state.rxDB, -31.25)
        XCTAssertEqual(state.inputDB, -45.5)
    }

    func testDStarStateTreatsAMissingTalkerAsNil() throws {
        let json = """
            {"link":"linking","talker":null,"slow_text":null,"backend":"thumbdv",\
            "tx_capable":true,"ptt":false,"tx_db":-60.0,"rx_db":-60.0,"input_db":-60.0}
            """
        let state = try XCTUnwrap(DStarState(json: json))
        XCTAssertEqual(state.link, .linking)
        XCTAssertNil(state.talker, "JSON null must decode as nil, not the string \"null\"")
        XCTAssertNil(state.slowText)
    }

    func testDStarStateRejectsTheEmptyDocumentAndDegradesOnAnUnknownLink() throws {
        XCTAssertNil(DStarState(json: "{}"), "the no-session document decodes as nil")
        // A newer engine naming a link state this binding does not know must
        // degrade to .failed — the direction that will not offer PTT.
        let odd = """
            {"link":"reticulating","talker":null,"slow_text":null,"backend":"thumbdv",\
            "tx_capable":true,"ptt":false,"tx_db":-60.0,"rx_db":-60.0,"input_db":-60.0}
            """
        XCTAssertEqual(try XCTUnwrap(DStarState(json: odd)).link, .failed)
    }

}
