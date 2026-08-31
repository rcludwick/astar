// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// hogaudio — take exclusive (hog-mode) ownership of a CoreAudio device so
// another app's attempt to open it fails, for testing astar's audio-error UI.
//
//   swift apps/macos/Tools/Debug/hogaudio.swift --list
//   swift apps/macos/Tools/Debug/hogaudio.swift "Mac mini Speakers"
//   swift apps/macos/Tools/Debug/hogaudio.swift --input "KT USB Audio"
//
// Ctrl-C releases. macOS also releases hog mode when this process exits, so a
// crash cannot leave a device permanently seized.
//
// Why hog mode and not just "open it twice": CoreAudio happily shares a device
// between clients, so a second opener normally succeeds. Hog mode is the one
// documented way to make it genuinely fail.

import CoreAudio
import Foundation

func addr(_ selector: AudioObjectPropertySelector,
          _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal)
    -> AudioObjectPropertyAddress
{
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope,
                               mElement: kAudioObjectPropertyElementMain)
}

func allDevices() -> [AudioDeviceID] {
    var a = addr(kAudioHardwarePropertyDevices)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject),
                                         &a, 0, nil, &size) == noErr else { return [] }
    var ids = [AudioDeviceID](repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
    guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject),
                                     &a, 0, nil, &size, &ids) == noErr else { return [] }
    return ids
}

func deviceName(_ id: AudioDeviceID) -> String {
    var a = addr(kAudioObjectPropertyName)
    var size = UInt32(MemoryLayout<CFString?>.size)
    var cf: CFString? = nil
    guard AudioObjectGetPropertyData(id, &a, 0, nil, &size, &cf) == noErr,
          let s = cf as String? else { return "<unknown>" }
    return s
}

/// Does the device have streams in this direction? That is what makes it an
/// input or an output; many devices are one, a few are both.
func hasStreams(_ id: AudioDeviceID, input: Bool) -> Bool {
    var a = addr(kAudioDevicePropertyStreams,
                 input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(id, &a, 0, nil, &size) == noErr else { return false }
    return size > 0
}

func hogOwner(_ id: AudioDeviceID, input: Bool) -> pid_t {
    var a = addr(kAudioDevicePropertyHogMode,
                 input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput)
    var owner: pid_t = -1
    var size = UInt32(MemoryLayout<pid_t>.size)
    guard AudioObjectGetPropertyData(id, &a, 0, nil, &size, &owner) == noErr else { return -2 }
    return owner
}

@discardableResult
func setHog(_ id: AudioDeviceID, input: Bool, to pid: pid_t) -> OSStatus {
    var a = addr(kAudioDevicePropertyHogMode,
                 input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput)
    var value = pid
    return AudioObjectSetPropertyData(id, &a, 0, nil, UInt32(MemoryLayout<pid_t>.size), &value)
}

// --- args ------------------------------------------------------------------

var args = Array(CommandLine.arguments.dropFirst())
var wantInput = false
if let i = args.firstIndex(of: "--input") { wantInput = true; args.remove(at: i) }
if let i = args.firstIndex(of: "--output") { wantInput = false; args.remove(at: i) }
let listOnly = args.contains("--list")
args.removeAll { $0 == "--list" }
let query = args.first

if listOnly || query == nil {
    print("Audio devices (hog owner: -1 = free, otherwise the owning pid)\n")
    for id in allDevices() {
        let n = deviceName(id)
        for input in [true, false] where hasStreams(id, input: input) {
            let dir = input ? "in " : "out"
            let owner = hogOwner(id, input: input)
            let state = owner == -1 ? "free" : (owner == -2 ? "no hog support" : "held by pid \(owner)")
            print("  \(dir) " + n.padding(toLength: max(24, n.count), withPad: " ", startingAt: 0) + "  \(state)")
        }
    }
    if query == nil && !listOnly {
        print("\nUsage: swift hogaudio.swift [--input|--output] <name substring>")
    }
    exit(0)
}

let needle = query!.lowercased()
guard let dev = allDevices().first(where: {
    deviceName($0).lowercased().contains(needle) && hasStreams($0, input: wantInput)
}) else {
    print("no \(wantInput ? "input" : "output") device matching \"\(query!)\" — try --list")
    exit(1)
}

let name = deviceName(dev)
let existing = hogOwner(dev, input: wantInput)
if existing != -1 {
    print("\(name) is already hogged by pid \(existing)")
    exit(1)
}

let rc = setHog(dev, input: wantInput, to: getpid())
guard rc == noErr, hogOwner(dev, input: wantInput) == getpid() else {
    print("could not take hog mode on \(name) (OSStatus \(rc)) — some virtual devices refuse it")
    exit(1)
}

print("HOGGING \(wantInput ? "input" : "output") \"\(name)\" as pid \(getpid())")
print("Other apps opening it should now FAIL. Ctrl-C to release.")

// A `defer` after `RunLoop.run()` would never fire, and a signal handler that
// calls `exit` skips it too — so do not pretend to release explicitly here.
// macOS drops hog mode when the owning process dies, which covers Ctrl-C, a
// kill, and a crash alike. That is the real guarantee; an explicit release
// would only be belt-and-braces on top of it.
signal(SIGINT) { _ in
    print("\nreleased (hog mode is dropped when this process exits)")
    exit(0)
}
RunLoop.current.run()
