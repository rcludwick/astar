// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
// main.swift — a minimal Swift consumer of the AstarStation poll+snapshot API.
//
// Offline by default: new -> snapshot(idle) -> setPTT(NOT_CONNECTED) ->
// nextEvent(nil) -> free (via deinit). No network, no callbacks. This is the
// phase gate's "Swift example that links + runs offline".
//
// Set IAX_PARROT_LIVE=1 to instead dial the AllStar parrot 55553 (audio
// loopback) and poll for ~8s. The live path needs the network + audio devices;
// CI / a headless box runs only the offline path.
//
// Secret-free: the guest secret is a connect() IN-arg ("allstar") and is never
// read back out of any Snapshot / Event / error.

import Foundation
import AstarStation

func statusName(_ s: IaxStatus) -> String {
    switch s {
    case .idle: return "idle"
    case .dialing: return "dialing"
    case .answered: return "answered"
    case .hangup: return "hangup"
    }
}

func drainEvents(_ st: Station) throws {
    while let ev = try st.nextEvent() {
        switch ev {
        case .answered: print("  event: answered")
        case .remotePTT(let on): print("  event: remotePTT=\(on)")
        case .hangup: print("  event: hangup")
        case .modeChanged: print("  event: modeChanged -> \((try? st.mode()) ?? .wt)")
        case .incoming: print("  event: incoming from \((try? st.incomingFrom()) ?? "")")
        case .registered: print("  event: registered")
        case .registerFailed: print("  event: registerFailed")
        }
    }
}

func runOffline(_ st: Station) throws -> Int32 {
    let snap = try st.snapshot()
    guard snap.status == .idle else {
        FileHandle.standardError.write(Data("expected idle snapshot, got \(snap.status)\n".utf8))
        return 1
    }
    guard snap.rttMS == nil else {
        FileHandle.standardError.write(Data("expected unknown rtt, got \(String(describing: snap.rttMS))\n".utf8))
        return 1
    }
    print("snapshot: \(snap)")

    // set_ptt while idle must report NOT_CONNECTED (-3).
    do {
        try st.setPTT(true)
        FileHandle.standardError.write(Data("setPTT(true) should have thrown while idle\n".utf8))
        return 1
    } catch let e as StationError {
        guard e.code == -3 else {
            FileHandle.standardError.write(Data("expected NOT_CONNECTED (-3), got \(e)\n".utf8))
            return 1
        }
        print("setPTT(true) while idle -> \(e)")
    }

    if try st.nextEvent() != nil {
        FileHandle.standardError.write(Data("expected no event\n".utf8))
        return 1
    }
    print("nextEvent() -> nil")
    print("dry run ok: idle snapshot + not-connected guard")
    return 0
}

/// Secret-free guard: nothing the app can read back mentions a secret. Runs in
/// the offline example so the invariant is checked even on a box without XCTest.
func runSecretFreeGuard() throws -> Int32 {
    let secret = "topsecret-please-do-not-leak"
    let cfg = StationConfig(portalPass: secret, secret: secret)
    let st = try Station(config: cfg)
    let snap = try st.snapshot()

    var haystacks: [String] = [st.description, "\(snap)"]
    if let ev = try st.nextEvent() { haystacks.append("\(ev)") }
    // The error text is generic 'static C — assert it too.
    do { try st.setPTT(true) } catch let e as StationError { haystacks.append(e.description) }

    for h in haystacks {
        if h.contains(secret) || h.lowercased().contains("secret")
            || h.lowercased().contains("password")
        {
            FileHandle.standardError.write(Data("secret leaked in: \(h)\n".utf8))
            return 1
        }
    }
    print("secret-free guard ok: repr/snapshot/event/error carry no secret")
    return 0
}

/// Node-mode usage snippet — offline-safe: it configures the listener and a
/// credential resolver but never switches to Node mode (which would start a real
/// listener / touch audio hardware). It also shows the secret-free contract: the
/// registrar password is supplied only through the resolver closure.
func runNodeConfigDemo(_ st: Station) throws -> Int32 {
    guard try st.mode() == .wt else {
        FileHandle.standardError.write(Data("expected default WT mode\n".utf8))
        return 1
    }

    // Listen-only node config (no registrar): does not switch mode.
    try st.setNodeConfig(NodeConfig(bind: "127.0.0.1:0", answer: .manual, auth: .off))

    // The ONLY secret channel: a closure the library calls when it needs a
    // password. The secret never appears in NodeConfig or any snapshot/event.
    try st.setCredentialResolver { user in
        user == "77777" ? "node-secret" : ""
    }

    // answer()/reject() return NOT_CONNECTED while in WT mode.
    do {
        try st.answer()
        FileHandle.standardError.write(Data("answer() should have thrown in WT mode\n".utf8))
        return 1
    } catch let e as StationError where e.code == -3 {
        print("answer() while WT -> \(e)")
    }

    print("incomingFrom() -> \"\(try st.incomingFrom())\"")
    print("node-config demo ok: listen-only config + resolver set, still WT mode")
    return 0
}

func runLive(_ st: Station) throws -> Int32 {
    // Generic IAX2 connect (vendor-neutral): dial the parrot. secret is an
    // in-param, consumed and never echoed.
    try st.connect(dest: "55553", calling: "55553", secret: "allstar", name: "astar")
    print("connected; polling ~8s...")
    for _ in 0..<80 {
        let s = try st.snapshot()
        print(String(
            format: "status=%-8@ tx=%.1fdB rx=%.1fdB ptt=%d rtt=%@",
            statusName(s.status) as NSString,
            s.txDB, s.rxDB, s.ptt ? 1 : 0,
            s.rttMS.map(String.init) ?? "?" as NSString
        ))
        try drainEvents(st)
        Thread.sleep(forTimeInterval: 0.1)
    }
    try st.disconnect()
    print("done")
    return 0
}

let live = ProcessInfo.processInfo.environment["IAX_PARROT_LIVE"] == "1"

do {
    let st = try Station()  // all-default config: default devices, no portal.
    if live {
        exit(try runLive(st))
    }
    let offlineRC = try runOffline(st)
    if offlineRC != 0 { exit(offlineRC) }
    let nodeRC = try runNodeConfigDemo(st)
    if nodeRC != 0 { exit(nodeRC) }
    exit(try runSecretFreeGuard())
} catch {
    FileHandle.standardError.write(Data("error: \(error)\n".utf8))
    exit(1)
}
