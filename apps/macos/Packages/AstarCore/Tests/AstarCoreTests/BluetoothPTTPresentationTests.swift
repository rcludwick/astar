// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import AstarStation
import XCTest

@testable import AstarCore

/// Every Bluetooth PTT string Rob reads, compared with `==`. A rewording is a
/// deliberate change to this file, never a side effect.
final class BluetoothPTTPresentationTests: XCTestCase {
    private typealias P = BluetoothPTTPresentation
    private let allStatuses: [BLELinkStatus] = [
        .disabled, .bluetoothOff, .unauthorized, .unsupported, .searching, .connecting,
        .notSupportedDevice, .needsConfirm, .ready, .releaseToArm, .reconnecting,
    ]

    // MARK: - Chip

    func testChipTextForEveryStatus() {
        let cases: [(BLELinkStatus, BLESignalBand?, String?)] = [
            (.disabled, nil, nil),
            (.ready, .strong, "B02PTT · strong"),
            (.ready, .fair, "B02PTT · fair"),
            (.ready, .weak, "B02PTT · weak"),
            (.ready, nil, "B02PTT · connected"),
            (.searching, nil, "B02PTT · searching…"),
            (.connecting, nil, "B02PTT · connecting…"),
            (.needsConfirm, nil, "B02PTT · not confirmed"),
            (.reconnecting, nil, "B02PTT · reconnecting…"),
            (.bluetoothOff, nil, "B02PTT · Bluetooth off"),
            (.unauthorized, nil, "B02PTT · no Bluetooth access"),
            (.unsupported, nil, "B02PTT · Bluetooth unavailable"),
            (.notSupportedDevice, nil, "B02PTT · not supported"),
            (.releaseToArm, nil, "B02PTT · release to arm"),
        ]
        for (status, signal, expected) in cases {
            XCTAssertEqual(
                P.chipText(
                    status: status, signal: signal, shortName: "B02PTT", transmitBlocked: false),
                expected, "\(status) \(String(describing: signal))")
        }
    }

    func testTransmitBlockedOverridesEveryChipButDisabled() {
        for status in allStatuses {
            let expected: String? = status == .disabled ? nil : "B02PTT · won't transmit"
            XCTAssertEqual(
                P.chipText(
                    status: status, signal: .strong, shortName: "B02PTT", transmitBlocked: true),
                expected, "\(status)")
            XCTAssertEqual(
                P.chipTone(status: status, transmitBlocked: true), .secondary, "\(status)")
        }
    }

    func testChipTone() {
        let secondary: [BLELinkStatus] = [
            .disabled, .ready, .searching, .connecting, .needsConfirm,
        ]
        let warning: [BLELinkStatus] = [
            .reconnecting, .bluetoothOff, .unauthorized, .unsupported, .notSupportedDevice,
            .releaseToArm,
        ]
        XCTAssertEqual(secondary.count + warning.count, allStatuses.count)
        for status in secondary {
            XCTAssertEqual(
                P.chipTone(status: status, transmitBlocked: false), .secondary, "\(status)")
        }
        for status in warning {
            XCTAssertEqual(
                P.chipTone(status: status, transmitBlocked: false), .warning, "\(status)")
        }
    }

    func testChipAccessibility() {
        XCTAssertEqual(P.chipAccessibilityLabel, "Bluetooth PTT button")
        let name = "B02PTT-6522"
        let cases: [(BLELinkStatus, BLESignalBand?, String)] = [
            (.ready, .strong, "B02PTT-6522, connected, signal strong"),
            (.ready, .fair, "B02PTT-6522, connected, signal fair"),
            (.ready, .weak, "B02PTT-6522, connected, signal weak"),
            (.ready, nil, "B02PTT-6522, connected"),
            (.searching, nil, "B02PTT-6522, searching"),
            (.connecting, nil, "B02PTT-6522, connecting"),
            (.needsConfirm, nil, "B02PTT-6522, not confirmed on this Mac"),
            (.reconnecting, nil, "B02PTT-6522, reconnecting"),
            (.bluetoothOff, nil, "B02PTT-6522, Bluetooth is off"),
            (.unauthorized, nil, "B02PTT-6522, Bluetooth access is off for astar"),
            (.unsupported, nil, "B02PTT-6522, Bluetooth unavailable"),
            (.notSupportedDevice, nil, "B02PTT-6522, not a supported PTT button"),
            (.releaseToArm, nil, "B02PTT-6522, release to arm"),
        ]
        for (status, signal, expected) in cases {
            XCTAssertEqual(
                P.chipAccessibilityValue(
                    name: name, status: status, signal: signal, transmitBlocked: false),
                expected)
            XCTAssertEqual(
                P.chipAccessibilityValue(
                    name: name, status: status, signal: signal, transmitBlocked: true),
                expected + ", won't transmit")
        }
    }

    // MARK: - Settings and Quick settings status

    func testSettingsStatus() {
        let cases: [(BLELinkStatus, BLESignalBand?, String, BluetoothPTTTone)] = [
            (.ready, .strong, "Ready · signal strong", .ready),
            (.ready, .fair, "Ready · signal fair", .ready),
            (.ready, .weak, "Ready · signal weak", .ready),
            (.ready, nil, "Ready", .ready),
            (.searching, nil, "Searching…", .secondary),
            (.connecting, nil, "Connecting…", .secondary),
            (.needsConfirm, nil, "Press the button once to confirm it's yours.", .secondary),
            (.disabled, nil, "Off", .secondary),
            (.reconnecting, nil, "Reconnecting…", .warning),
            (.bluetoothOff, nil, "Bluetooth is off", .warning),
            (.unauthorized, nil, "Bluetooth access is off for astar", .warning),
            (.unsupported, nil, "This Mac has no Bluetooth LE", .warning),
            (.notSupportedDevice, nil, "Not a supported PTT button", .warning),
            (.releaseToArm, nil, "Press ignored: release and press again", .warning),
        ]
        XCTAssertEqual(Set(cases.map { $0.0.rawValue }).count, allStatuses.count)
        for (status, signal, text, tone) in cases {
            let got = P.settingsStatus(status: status, signal: signal)
            XCTAssertEqual(got.text, text, "\(status)")
            XCTAssertEqual(got.tone, tone, "\(status)")
        }
    }

    func testQuickStatus() {
        let ready = P.quickStatus(status: .ready, signal: .strong, shortName: "B02PTT")
        XCTAssertEqual(ready.text, "B02PTT · strong")
        XCTAssertEqual(ready.tone, .secondary)
        let readyNoRSSI = P.quickStatus(status: .ready, signal: nil, shortName: "B02PTT")
        XCTAssertEqual(readyNoRSSI.text, "B02PTT · connected")
        XCTAssertEqual(
            P.quickStatus(status: .searching, signal: nil, shortName: "B02PTT").text,
            "Searching…")
        XCTAssertEqual(
            P.quickStatus(status: .connecting, signal: nil, shortName: "B02PTT").text,
            "Connecting…")
        let confirm = P.quickStatus(status: .needsConfirm, signal: nil, shortName: "B02PTT")
        XCTAssertEqual(confirm.text, "Press the button once to confirm it's yours.")
        XCTAssertEqual(confirm.tone, .secondary)
        for status in [
            BLELinkStatus.reconnecting, .bluetoothOff, .unauthorized, .unsupported,
            .notSupportedDevice, .releaseToArm, .disabled,
        ] {
            let quick = P.quickStatus(status: status, signal: nil, shortName: "B02PTT")
            let settings = P.settingsStatus(status: status, signal: nil)
            XCTAssertEqual(quick.text, settings.text, "\(status)")
            XCTAssertEqual(quick.tone, settings.tone, "\(status)")
        }
    }

    // MARK: - Banners

    func testReleaseBanners() {
        XCTAssertNil(P.releaseBanner(.button, shortName: "B02PTT"))
        XCTAssertNil(P.releaseBanner(.disabled, shortName: "B02PTT"))
        XCTAssertEqual(
            P.releaseBanner(.linkLost, shortName: "B02PTT"),
            "Transmit stopped: B02PTT disconnected. Release the button, then press again.")
        XCTAssertEqual(
            P.releaseBanner(.adapterOff, shortName: "B02PTT"),
            "Transmit stopped: Bluetooth turned off. Release the button, then press again.")
        XCTAssertEqual(
            P.releaseBanner(.malformed, shortName: "B02PTT"),
            "Transmit stopped: B02PTT sent a value astar doesn't recognise. "
                + "Release the button, then press again.")
        XCTAssertEqual(
            P.releaseBanner(.sleep, shortName: "B02PTT"),
            "Transmit stopped: the Mac went to sleep. Release the button, then press again.")
    }

    private func engineStop(_ reason: StationPTTForcedReason, seconds: Int, at: Date)
        -> PTTForcedStop
    {
        PTTForcedStop(
            seq: 1, sources: [.bluetooth, .vox], reason: reason, txTimeoutSeconds: seconds, at: at)
    }

    func testForcedStopBannerFollowsTheSetting() {
        let at = Date(timeIntervalSince1970: 1_800_000_000)
        XCTAssertEqual(
            P.forcedStopBanner(engineStop(.txTimeout, seconds: 120, at: at)),
            "Transmit stopped after 2 minutes. Release to talk again.")
        XCTAssertEqual(
            P.forcedStopBanner(engineStop(.txTimeout, seconds: 60, at: at)),
            "Transmit stopped after 1 minute. Release to talk again.")
        XCTAssertEqual(
            P.forcedStopBanner(engineStop(.txTimeout, seconds: 600, at: at)),
            "Transmit stopped after 10 minutes. Release to talk again.")
        XCTAssertEqual(
            P.forcedStopBanner(engineStop(.leaseExpired, seconds: 120, at: at)),
            "Transmit stopped: astar stopped responding. Release the button, then press again.")
    }

    func testBannerShowsForEightSecondsThenHides() {
        let t0 = Date(timeIntervalSince1970: 1_800_000_000)
        let stop = engineStop(.txTimeout, seconds: 120, at: t0)
        XCTAssertEqual(
            P.bannerText(
                engine: stop, bluetooth: nil, shortName: "B02PTT",
                now: t0.addingTimeInterval(7.999), keyedSince: nil),
            "Transmit stopped after 2 minutes. Release to talk again.")
        XCTAssertNil(
            P.bannerText(
                engine: stop, bluetooth: nil, shortName: "B02PTT",
                now: t0.addingTimeInterval(8), keyedSince: nil))
    }

    func testBannerDismissedByALaterKeyUp() {
        let t0 = Date(timeIntervalSince1970: 1_800_000_000)
        let stop = engineStop(.txTimeout, seconds: 120, at: t0)
        XCTAssertNil(
            P.bannerText(
                engine: stop, bluetooth: nil, shortName: "B02PTT",
                now: t0.addingTimeInterval(2), keyedSince: t0.addingTimeInterval(1)))
        XCTAssertEqual(
            P.bannerText(
                engine: stop, bluetooth: nil, shortName: "B02PTT",
                now: t0.addingTimeInterval(2), keyedSince: t0.addingTimeInterval(-30)),
            "Transmit stopped after 2 minutes. Release to talk again.",
            "a key-up that began before the stop does not dismiss it")
    }

    func testBannerShowsTheNewerStop() {
        let t0 = Date(timeIntervalSince1970: 1_800_000_000)
        let engine = engineStop(.leaseExpired, seconds: 120, at: t0)
        let linkLost = BluetoothForcedRelease(
            seq: 1, reason: .linkLost, at: t0.addingTimeInterval(1))
        XCTAssertEqual(
            P.bannerText(
                engine: engine, bluetooth: linkLost, shortName: "B02PTT",
                now: t0.addingTimeInterval(2), keyedSince: nil),
            "Transmit stopped: B02PTT disconnected. Release the button, then press again.")
        let laterEngine = engineStop(.leaseExpired, seconds: 120, at: t0.addingTimeInterval(1.5))
        XCTAssertEqual(
            P.bannerText(
                engine: laterEngine, bluetooth: linkLost, shortName: "B02PTT",
                now: t0.addingTimeInterval(2), keyedSince: nil),
            "Transmit stopped: astar stopped responding. Release the button, then press again.")
        XCTAssertNil(
            P.bannerText(
                engine: nil, bluetooth: nil, shortName: "B02PTT", now: t0, keyedSince: nil))
        XCTAssertNil(
            P.bannerText(
                engine: nil,
                bluetooth: BluetoothForcedRelease(seq: 2, reason: .button, at: t0),
                shortName: "B02PTT", now: t0, keyedSince: nil),
            "an ordinary release has no banner")
    }

    // MARK: - Audio helper

    func testAudioMatchNeedsBothDirections() {
        let both = BluetoothAudioMatch.devices(
            advertisedName: "B02PTT-6522",
            inputs: ["MacBook Pro Microphone", "B02PTT-6522 Hands-Free", "B02PTT-6522 #2"],
            outputs: ["MacBook Pro Speakers", "B02PTT-6522 Hands-Free"])
        XCTAssertEqual(both?.input, "B02PTT-6522 Hands-Free")
        XCTAssertEqual(both?.output, "B02PTT-6522 Hands-Free")
        XCTAssertNil(
            BluetoothAudioMatch.devices(
                advertisedName: "B02PTT-6522", inputs: ["B02PTT-6522"],
                outputs: ["MacBook Pro Speakers"]))
        XCTAssertNil(
            BluetoothAudioMatch.devices(
                advertisedName: "B02PTT-6522", inputs: ["b02ptt-6522"], outputs: ["b02ptt-6522"]),
            "the match is case-sensitive, like the advertised-name match")
        XCTAssertNil(
            BluetoothAudioMatch.devices(
                advertisedName: "", inputs: ["Anything"], outputs: ["Anything"]),
            "an empty name would match every device")
    }
}
