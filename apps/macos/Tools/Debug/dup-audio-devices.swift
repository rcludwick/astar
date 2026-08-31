// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// Dev tool for astar-9d41 / astar-uid: manufacture the duplicate-device-name
// condition without owning the hardware that causes it.
//
// An ICOM IC-7300 and an AllScan UCI150 both enumerate as "USB Audio Device".
// astar addresses audio devices BY NAME (`DeviceId` is "in:<name>"), so two
// same-named devices collapse to one identity and only the first is reachable.
// This creates two aggregate devices sharing one name so that path can be
// exercised on any Mac.
//
//   swiftc -O -o /tmp/dupdev apps/macos/Tools/dup-audio-devices.swift
//   /tmp/dupdev create     # two input devices both named ASTAR DUP TEST
//   /tmp/dupdev all        # every device, with input channel counts + UIDs
//   /tmp/dupdev destroy    # remove exactly what create made
//
// The name is deliberately NOT "USB Audio Device": colliding with real
// hardware would make that hardware unaddressable while the tool is active.
// `destroy` is keyed on the two fixed UIDs below, so it can never remove a
// device it did not create. Aggregate devices are visible in Audio MIDI Setup
// if a run is interrupted before cleanup.
//
// Verified against `cargo run -p astar-audio --example list_devices`, which
// prints the engine's own view — with these active it emits TWO entries with
// the identical id `in:ASTAR DUP TEST`.
import CoreAudio
import Foundation

let SHARED_NAME = "ASTAR DUP TEST"
let UIDS = ["com.astar.dup-repro.a", "com.astar.dup-repro.b"]

func allDeviceIDs() -> [AudioObjectID] {
    var addr = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size)
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &ids)
    return ids
}

func stringProp(_ id: AudioObjectID, _ sel: AudioObjectPropertySelector) -> String? {
    var addr = AudioObjectPropertyAddress(
        mSelector: sel, mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    var size = UInt32(MemoryLayout<CFString?>.size)
    var out: CFString? = nil
    let st = withUnsafeMutablePointer(to: &out) {
        AudioObjectGetPropertyData(id, &addr, 0, nil, &size, $0)
    }
    return st == noErr ? out as String? : nil
}

func inputChannels(_ id: AudioObjectID) -> Int {
    var addr = AudioObjectPropertyAddress(
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(id, &addr, 0, nil, &size) == noErr, size > 0 else { return 0 }
    let buf = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
    defer { buf.deallocate() }
    guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, buf) == noErr else { return 0 }
    let abl = buf.assumingMemoryBound(to: AudioBufferList.self)
    return UnsafeMutableAudioBufferListPointer(abl).reduce(0) { $0 + Int($1.mNumberChannels) }
}

func findByUID(_ uid: String) -> AudioObjectID? {
    allDeviceIDs().first { stringProp($0, kAudioDevicePropertyDeviceUID) == uid }
}

// A sub-device to wrap. BlackHole is present on this Mac and is input-capable.
func subDeviceUID() -> String? {
    for id in allDeviceIDs() where inputChannels(id) > 0 {
        if let n = stringProp(id, kAudioObjectPropertyName), n.contains("BlackHole") {
            return stringProp(id, kAudioDevicePropertyDeviceUID)
        }
    }
    return nil
}

func create() {
    guard let sub = subDeviceUID() else { print("no BlackHole sub-device found"); exit(1) }
    for uid in UIDS {
        if findByUID(uid) != nil { print("exists already: \(uid)"); continue }
        let desc: [String: Any] = [
            kAudioAggregateDeviceNameKey as String: SHARED_NAME,
            kAudioAggregateDeviceUIDKey as String: uid,
            kAudioAggregateDeviceSubDeviceListKey as String: [
                [kAudioSubDeviceUIDKey as String: sub]
            ],
            kAudioAggregateDeviceIsPrivateKey as String: 0,
        ]
        var newID = AudioObjectID(0)
        let st = AudioHardwareCreateAggregateDevice(desc as CFDictionary, &newID)
        print(st == noErr ? "created \(uid) -> id \(newID)" : "FAILED \(uid): OSStatus \(st)")
    }
}

func destroy() {
    for uid in UIDS {
        guard let id = findByUID(uid) else { print("absent: \(uid)"); continue }
        let st = AudioHardwareDestroyAggregateDevice(id)
        print(st == noErr ? "destroyed \(uid)" : "FAILED destroy \(uid): OSStatus \(st)")
    }
}

func list() {
    print("--- input-capable devices ---")
    for id in allDeviceIDs() where inputChannels(id) > 0 {
        let n = stringProp(id, kAudioObjectPropertyName) ?? "?"
        let u = stringProp(id, kAudioDevicePropertyDeviceUID) ?? "?"
        print("  \(n)   [uid \(u)]")
    }
}

switch CommandLine.arguments.dropFirst().first ?? "list" {
case "create": create(); list()
case "all":
    print("--- ALL devices ---")
    for id in allDeviceIDs() {
        let n = stringProp(id, kAudioObjectPropertyName) ?? "?"
        let u = stringProp(id, kAudioDevicePropertyDeviceUID) ?? "?"
        print("  \(n)  in=\(inputChannels(id))  [uid \(u)]")
    }
case "destroy": destroy(); list()
default: list()
}
