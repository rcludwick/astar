// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// EchoLinkTests.swift — offline tests for the EchoLink calls and decoders, and
// for the guard that checks this binding's C struct mirrors against the
// library's own sizes.
//
// Nothing here logs in or connects: every call that would reach the network is
// one the engine refuses before it opens a socket (an empty password, no login
// to connect from), so no test ever contacts EchoLink.

import CAstarStation
import XCTest

@testable import AstarStation

final class EchoLinkTests: XCTestCase {

    // MARK: Struct layout guard

    func testTheStateMirrorIsTheSizeTheLibraryWrites() {
        XCTAssertEqual(MemoryLayout<IaxState>.size, Int(iax_state_size()))
        XCTAssertEqual(MemoryLayout<IaxEvent>.size, Int(iax_event_size()))
    }

    func testMatchingSizesRaiseNoLayoutProblem() {
        XCTAssertNil(Station.layoutProblem(type: "IaxState", mirror: 144, library: 144))
    }

    func testAMismatchNamesTheTypeAndBothSizes() throws {
        let problem = try XCTUnwrap(
            Station.layoutProblem(type: "IaxState", mirror: 136, library: 144))
        XCTAssertTrue(problem.contains("IaxState"), problem)
        XCTAssertTrue(problem.contains("136"), problem)
        XCTAssertTrue(problem.contains("144"), problem)
    }

    // MARK: State decoding

    func testAConnectedCallDecodesEveryField() throws {
        let json = """
            {"login":"logged_in","retrying":false,"callsign":"KX7ABC","error":null,\
            "link":"connected","peer":"W1AW","node":1234,"ptt":false,\
            "receiving":true,"info":"Newington",\
            "chat":[{"from":"W1AW","text":"hi","mine":false},\
            {"from":"KX7ABC","text":"hello","mine":true}]}
            """
        let state = try XCTUnwrap(EchoLinkState(json: json))
        XCTAssertEqual(
            state,
            EchoLinkState(
                login: .loggedIn, callsign: "KX7ABC", error: nil, link: .connected,
                peer: "W1AW", node: 1234, ptt: false, receiving: true, info: "Newington",
                chat: [
                    EchoLinkChatLine(from: "W1AW", text: "hi", mine: false),
                    EchoLinkChatLine(from: "KX7ABC", text: "hello", mine: true),
                ]))
    }

    func testTheTransmitCapNoticeDecodesAndDefaultsToNil() throws {
        let capped = try XCTUnwrap(
            EchoLinkState(
                json: #"{"login":"logged_in","callsign":"KX7ABC","link":"connected","notice":"EchoLink stopped."}"#
            ))
        XCTAssertEqual(capped.notice, "EchoLink stopped.")
        let plain = try XCTUnwrap(
            EchoLinkState(json: #"{"login":"logged_in","callsign":"KX7ABC","notice":null}"#))
        XCTAssertNil(plain.notice)
    }

    func testRetryingDecodesAndDefaultsToFalse() throws {
        let unreachable = try XCTUnwrap(
            EchoLinkState(
                json: #"{"login":"failed","retrying":true,"callsign":"KX7ABC","error":"Couldn't reach any EchoLink server"}"#
            ))
        XCTAssertEqual(unreachable.login, .failed)
        XCTAssertTrue(unreachable.retrying)
        let refused = try XCTUnwrap(
            EchoLinkState(
                json: #"{"login":"failed","retrying":false,"callsign":"KX7ABC","error":"EchoLink said the password is wrong"}"#
            ))
        XCTAssertFalse(refused.retrying)
        let old = try XCTUnwrap(EchoLinkState(json: #"{"login":"failed","callsign":"KX7ABC"}"#))
        XCTAssertFalse(old.retrying, "absent reads as false")
    }

    func testAFailedLoginCarriesItsSentenceAndNoCall() throws {
        let json = """
            {"login":"failed","callsign":"KX7ABC",\
            "error":"EchoLink said the password is wrong"}
            """
        let state = try XCTUnwrap(EchoLinkState(json: json))
        XCTAssertEqual(state.login, .failed)
        XCTAssertEqual(state.callsign, "KX7ABC")
        XCTAssertEqual(state.error, "EchoLink said the password is wrong")
        XCTAssertNil(state.link)
        XCTAssertNil(state.peer)
        XCTAssertNil(state.node)
        XCTAssertFalse(state.ptt)
        XCTAssertFalse(state.receiving)
        XCTAssertNil(state.info)
        XCTAssertEqual(state.chat, [])
    }

    func testTheEmptyDocumentIsNoState() {
        XCTAssertNil(EchoLinkState(json: "{}"))
        XCTAssertNil(EchoLinkState(json: "not json"))
    }

    func testEveryLoginRawValueMatchesTheEngine() {
        XCTAssertEqual(EchoLinkLoginState.loggedOut.rawValue, "logged_out")
        XCTAssertEqual(EchoLinkLoginState.loggingIn.rawValue, "logging_in")
        XCTAssertEqual(EchoLinkLoginState.loggedIn.rawValue, "logged_in")
        XCTAssertEqual(EchoLinkLoginState.failed.rawValue, "failed")
        XCTAssertEqual(EchoLinkLink.connecting.rawValue, "connecting")
        XCTAssertEqual(EchoLinkLink.connected.rawValue, "connected")
        XCTAssertEqual(EchoLinkLink.closed.rawValue, "closed")
    }

    func testUnknownLoginAndLinkStringsLandOnTheSafeCases() throws {
        let state = try XCTUnwrap(
            EchoLinkState(json: #"{"login":"dancing","callsign":"K","link":"warping"}"#))
        XCTAssertEqual(state.login, .loggedOut)
        XCTAssertEqual(state.link, .closed, "an unknown link offers no PTT")
    }

    // MARK: Station list decoding

    func testStationsDecodeWithUnknownKindAndStatusFallbacks() throws {
        let json = """
            [{"call":"W1AW","description":"Newington","node":1234,\
            "kind":"user","status":"online"},\
            {"call":"*ECHOTEST*","description":"Test","node":9999,\
            "kind":"conference","status":"busy"},\
            {"call":"N0CALL-R","description":"","node":42,\
            "kind":"satellite","status":"asleep"}]
            """
        let stations = try XCTUnwrap(EchoLinkStationEntry.decodeStations(json: json))
        XCTAssertEqual(
            stations,
            [
                EchoLinkStationEntry(
                    call: "W1AW", description: "Newington", node: 1234, kind: .user,
                    status: .online),
                EchoLinkStationEntry(
                    call: "*ECHOTEST*", description: "Test", node: 9999, kind: .conference,
                    status: .busy),
                EchoLinkStationEntry(
                    call: "N0CALL-R", description: "", node: 42, kind: .unknown,
                    status: .unknown),
            ])
        XCTAssertEqual(stations[0].id, 1234)
        XCTAssertEqual(EchoLinkStationKind.link.rawValue, "link")
        XCTAssertEqual(EchoLinkStationKind.repeater.rawValue, "repeater")
    }

    func testAnEmptyListIsEmptyAndGarbageIsNil() {
        XCTAssertEqual(EchoLinkStationEntry.decodeStations(json: "[]"), [])
        XCTAssertNil(EchoLinkStationEntry.decodeStations(json: "{}"))
    }

    // MARK: A fresh station

    func testAFreshStationHasEchoLinkAvailableAndIdle() throws {
        let station = try Station()
        let snap = try station.snapshot()
        XCTAssertTrue(snap.echolinkAvailable, "the xcframework is built with echolink")
        XCTAssertFalse(snap.echolinkActive)
        XCTAssertFalse(snap.echolinkLoggedIn)
        XCTAssertNil(try station.echolinkState())
        XCTAssertEqual(try station.echolinkStations(), [])
    }

    func testDisconnectAndLogoutAreNoOpsWhileIdle() throws {
        let station = try Station()
        XCTAssertNoThrow(try station.echolinkDisconnect())
        XCTAssertNoThrow(try station.echolinkDisconnect())
        XCTAssertNoThrow(try station.echolinkLogout())
        XCTAssertNoThrow(try station.echolinkSetLocation("Denver"))
    }

    func testAnEmptyPasswordIsRefusedBeforeAnythingStarts() throws {
        let station = try Station()
        XCTAssertThrowsError(
            try station.echolinkLogin(callsign: "KX7ABC", password: "", servers: ["127.0.0.1"])
        ) { error in
            XCTAssertEqual((error as? StationError)?.code, IAX_ERR_ECHOLINK)
        }
        XCTAssertNil(try station.echolinkState(), "nothing started")
    }

    func testConnectingWithoutALoginIsRefused() throws {
        let station = try Station()
        XCTAssertThrowsError(try station.echolinkConnect("9999")) { error in
            XCTAssertEqual((error as? StationError)?.code, IAX_ERR_ECHOLINK)
        }
        XCTAssertFalse(try station.snapshot().echolinkActive)
    }

    func testRefreshingTheListNeedsALogin() throws {
        let station = try Station()
        XCTAssertThrowsError(try station.echolinkRefreshList()) { error in
            XCTAssertEqual((error as? StationError)?.code, IAX_ERR_ECHOLINK)
        }
    }

    func testAPasswordTheDirectoryCannotTakeIsRefused() throws {
        let station = try Station()
        for bad in ["pass\rword", "pass\nword", "pass\u{AC}word"] {
            XCTAssertThrowsError(
                try station.echolinkLogin(callsign: "KX7ABC", password: bad, servers: ["127.0.0.1"])
            ) { error in
                XCTAssertEqual((error as? StationError)?.code, IAX_ERR_ECHOLINK)
                XCTAssertFalse((error as? StationError)?.message.contains(bad) ?? true)
            }
        }
        XCTAssertNil(try station.echolinkState(), "nothing started")
    }

    func testChatWithNoCallIsNotConnected() throws {
        let station = try Station()
        XCTAssertThrowsError(try station.echolinkSendChat("hi")) { error in
            XCTAssertEqual((error as? StationError)?.code, IAX_ERR_NOT_CONNECTED)
        }
    }
}
