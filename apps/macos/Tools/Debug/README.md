# Tools/Debug

Tools that **manufacture a condition** astar has to cope with, so the handling
can be exercised without owning the hardware — or the bad luck — that normally
produces it.

Nothing here is part of a build. They are run by hand, from the repository
root, with `swift <path>`; none of them takes a dependency on astar.

| Tool | Manufactures |
|---|---|
| `dup-audio-devices.swift` | Two capture devices reporting the same name — an ICOM IC-7300 and an AllScan UCI150 both enumerate as "USB Audio Device" (astar-9d41). |
| `hogaudio.swift` | A device another process cannot open, via CoreAudio hog mode. Drives the audio-failure path that surfaces as `IAX_ERR_AUDIO` (-7). |

## Why hog mode, for `hogaudio.swift`

CoreAudio shares a device between clients quite happily, so "just open it twice"
does not fail and cannot be used to test a failure. Hog mode is the one
documented way to make another process's open genuinely fail.

macOS drops hog mode when the owning process exits, so Ctrl-C, a kill and a
crash all release the device. It cannot leave one seized.

Aggregate and virtual devices commonly refuse hog mode; `--list` reports that
per device rather than failing silently.
