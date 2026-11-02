// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// The EchoLink station list's filter and search, and the binding's decoders
/// fed real engine documents. Every list here is decoded from JSON in the
/// shape `iax_station_echolink_stations` writes, so the filter is tested
/// against what the engine hands over rather than values a test built.
final class EchoLinkTests: XCTestCase {

    /// One of each kind, out of callsign order, plus a kind this binding
    /// does not know.
    private let listJSON = #"""
        [{"call":"W1AW","description":"Newington [ON 12:00]","node":1234,"kind":"user","status":"online"},
         {"call":"*ECHOTEST*","description":"Test server","node":9999,"kind":"conference","status":"online"},
         {"call":"KX7ABC-L","description":"Denver link","node":99123,"kind":"link","status":"busy"},
         {"call":"AJ7HR-R","description":"Repeater","node":45678,"kind":"repeater","status":"unknown"},
         {"call":"AB1CD","description":"","node":5555,"kind":"user","status":"busy"},
         {"call":"ZZ9ZZ","description":"","node":7777,"kind":"bridge","status":"away"}]
        """#

    private var stations: [EchoLinkStationEntry] {
        EchoLinkStationEntry.decodeStations(json: listJSON) ?? []
    }

    private func calls(_ filter: EchoLinkFilter, _ query: String = "") -> [String] {
        filterEchoLinkStations(stations, filter: filter, query: query).map(\.call)
    }

    func testFilterPeopleLinksRepeatersConferences() {
        XCTAssertEqual(stations.count, 6, "the fixture decodes whole")
        XCTAssertEqual(calls(.people), ["AB1CD", "W1AW"])
        XCTAssertEqual(calls(.links), ["KX7ABC-L"])
        XCTAssertEqual(calls(.repeaters), ["AJ7HR-R"])
        XCTAssertEqual(calls(.conferences), ["*ECHOTEST*"])
        XCTAssertEqual(
            calls(.all), ["*ECHOTEST*", "AB1CD", "AJ7HR-R", "KX7ABC-L", "W1AW", "ZZ9ZZ"],
            "All is every row, the unknown kind included, sorted by callsign")
        XCTAssertEqual(EchoLinkFilter.allCases.count, 6)
    }

    func testSearchByCallsignAndNodePrefix() {
        XCTAssertEqual(calls(.all, "echo"), ["*ECHOTEST*"], "callsign, any case, any position")
        XCTAssertEqual(calls(.all, "99"), ["*ECHOTEST*", "KX7ABC-L"], "node numbers by prefix")
        XCTAssertEqual(calls(.all, "23"), [], "a node number matches from its start only")
        XCTAssertEqual(calls(.all, "  w1aw "), ["W1AW"], "surrounding blanks are ignored")
        XCTAssertEqual(calls(.links, "99"), ["KX7ABC-L"], "the filter and the search combine")
        XCTAssertEqual(calls(.people, "").count, 2, "an empty query keeps the whole filter")
    }

    func testUnknownLinkStringDecodesToClosed() {
        let state = EchoLinkState(
            json: #"{"login":"logged_in","callsign":"KX7ABC","link":"ringing"}"#)
        XCTAssertEqual(state?.link, .closed)
        XCTAssertEqual(state?.login, .loggedIn)
    }

    func testSnakeCaseLoginStatesDecode() {
        func login(_ json: String) -> EchoLinkState? { EchoLinkState(json: json) }
        XCTAssertEqual(login(#"{"login":"logged_in","callsign":"KX7ABC"}"#)?.login, .loggedIn)
        XCTAssertEqual(login(#"{"login":"logging_in","callsign":"KX7ABC"}"#)?.login, .loggingIn)
        XCTAssertEqual(login(#"{"login":"logged_out","callsign":"KX7ABC"}"#)?.login, .loggedOut)
        let failed = login(
            #"{"login":"failed","callsign":"KX7ABC","error":"EchoLink said the password is wrong"}"#
        )
        XCTAssertEqual(failed?.login, .failed)
        XCTAssertEqual(failed?.error, "EchoLink said the password is wrong")
        XCTAssertNil(login("{}"), "logged out with nothing to say")
    }

    func testUnknownStationKindAndStatusDecodeToUnknown() {
        let rows = EchoLinkStationEntry.decodeStations(
            json:
                #"[{"call":"ZZ9ZZ","description":"","node":7777,"kind":"bridge","status":"away"}]"#)
        XCTAssertEqual(rows?.first?.kind, .unknown)
        XCTAssertEqual(rows?.first?.status, .unknown)
    }

    /// The default server list lives here and only here on the app side; the
    /// engine names no EchoLink host at all.
    func testDefaultServersAreTheFourPublicOnesInOrder() {
        XCTAssertEqual(
            EchoLinkDefaults.servers,
            [
                "servers.echolink.org", "naeast.echolink.org", "nasouth.echolink.org",
                "backup.echolink.org",
            ])
    }

    /// The pane's three states, plus the wait between them. A call in hand
    /// wins over everything; a login that is retrying by itself is still a
    /// wait, not a form to fill in again.
    func testPaneModeFollowsTheLoginAndTheCall() {
        XCTAssertEqual(EchoLinkPaneMode(login: .loggedOut, retrying: false, link: nil), .signIn)
        XCTAssertEqual(EchoLinkPaneMode(login: .failed, retrying: false, link: nil), .signIn)
        XCTAssertEqual(EchoLinkPaneMode(login: .loggingIn, retrying: false, link: nil), .signingIn)
        XCTAssertEqual(EchoLinkPaneMode(login: .loggingIn, retrying: true, link: nil), .signingIn)
        XCTAssertEqual(EchoLinkPaneMode(login: .failed, retrying: true, link: nil), .signingIn)
        XCTAssertEqual(EchoLinkPaneMode(login: .loggedIn, retrying: false, link: nil), .stations)
        XCTAssertEqual(
            EchoLinkPaneMode(login: .loggedIn, retrying: false, link: .closed), .stations,
            "a closed link is no call")
        XCTAssertEqual(
            EchoLinkPaneMode(login: .loggedIn, retrying: false, link: .connecting), .call)
        XCTAssertEqual(EchoLinkPaneMode(login: .loggedIn, retrying: false, link: .connected), .call)
    }

    /// What VoiceOver says for the dot, so colour is never the only signal.
    func testStatusDotHasASpokenName() {
        XCTAssertEqual(EchoLinkStationStatus.online.spokenName, "Free")
        XCTAssertEqual(EchoLinkStationStatus.busy.spokenName, "Busy")
        XCTAssertEqual(EchoLinkStationStatus.unknown.spokenName, "Status unknown")
    }

    // MARK: - Chat ids

    private func line(_ text: String, mine: Bool = false) -> EchoLinkChatLine {
        EchoLinkChatLine(from: mine ? "KX7ABC" : "*ECHOTEST*", text: text, mine: mine)
    }

    /// Lines keep their ids as the chat grows, and as the engine's cap drops
    /// the oldest, so a list keyed on them neither redraws nor loses its
    /// place.
    func testChatIdsSurviveAppendsAndTheCap() {
        var nextID = 0
        let first = echolinkChatEntries(
            previous: [], next: [line("a"), line("b")], nextID: &nextID)
        XCTAssertEqual(first.map(\.id), [0, 1])

        let grown = echolinkChatEntries(
            previous: first, next: [line("a"), line("b"), line("c")], nextID: &nextID)
        XCTAssertEqual(grown.map(\.id), [0, 1, 2])

        // At the cap: the oldest goes as the newest arrives, and the count
        // stays the same.
        let capped = echolinkChatEntries(
            previous: grown, next: [line("b"), line("c"), line("d")], nextID: &nextID)
        XCTAssertEqual(capped.map(\.id), [1, 2, 3])
        XCTAssertEqual(capped.map(\.line.text), ["b", "c", "d"])

        // Repeated text still gets one id per line.
        let repeated = echolinkChatEntries(
            previous: capped, next: [line("c"), line("d"), line("d")], nextID: &nextID)
        XCTAssertEqual(repeated.map(\.id), [2, 3, 4])

        // A new call shares nothing with the old one.
        let fresh = echolinkChatEntries(
            previous: repeated, next: [line("hello", mine: true)], nextID: &nextID)
        XCTAssertEqual(fresh.map(\.id), [5])
        XCTAssertEqual(echolinkChatEntries(previous: fresh, next: [], nextID: &nextID), [])
    }
}
