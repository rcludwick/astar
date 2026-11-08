---
icon: lucide/circle-dot
---

# Recording

astar can record a connection to an ordinary audio file on your Mac: what you
heard, what you sent, or both. You can press a button when a net gets
interesting, or have every connection recorded by itself. The files are
[FLAC](https://xiph.org/flac/), a lossless format that Music, QuickTime
Player, VLC and every audio editor open directly.

Recording is in the macOS app. The Windows/Linux client is paused and does
not record yet.

## Recording a net

Connect to a node or a reflector as usual. Once the connection is
**answered** (the far end has picked up and the call card shows it), the
**Record** button beside the talk control comes alive.
Before that it is greyed out, because there is nothing to record while
astar is still dialling.

Click **Record**, or press **⌘R**. Three things change:

* The grey circle on the button fills red, and a clock appears beside it
  counting how long the recording has run: `00:42`, and `1:02:03` once it
  passes an hour.
* The asterisk in the menu bar gets a small red dot in its bottom-right
  corner. It stays there while the window is closed, so you can tell from
  any app that a recording is still running.
* VoiceOver reads the button as "Recording, 42 seconds" instead of
  "Record".

Click the button or press **⌘R** again to stop. Hanging up stops it too,
and so does the far end dropping the call. Either way the file is finished
properly, with its length written into it.

The file lands in the recordings folder, `~/Music/astar Recordings` unless
you choose another (see [The folder](#the-folder)). Its name says when it
started, on which network, and where you were connected:

```
2026-10-26 19-00-04 AllStar 2000.flac
2026-10-26 21-15-30 D-Star XLX458 B.flac
```

The time is your Mac's local time, with dashes instead of colons, because
a colon can't go in a file name. A slash or colon in a reflector's name
becomes a dash for the same reason. If a file of that name already exists,
the new one gets a number on the end (`… AllStar 2000 2.flac`) rather than
replacing it.

Quitting astar while recording finishes the file before the app goes.

## Recording every connection

If you would rather never miss a net, turn on **Always record** in
Settings → **Recording**. It is off until you turn it on.

With it on, a recording starts by itself each time a connection is
answered, and stops when the connection ends. That gives you one file per
connection, named as above. You can still press **Record** (or **⌘R**) to
stop one you don't want, and press it again to start a new file.

One connection's recording never runs on into the next. If you dial
somewhere on a **different network** while recording, say from AllStar to
M17, the AllStar file is finished at the switch and a new file starts once
the M17 connection is answered. Dialling another node on the same network
ends the recording, and nothing starts again unless **Always record** is
on.

## Choosing a layout

A connection has two directions: what you **receive** (the far end, the
net) and what you **transmit** (your own voice as astar sends it). The
**layout**, in Settings → **Recording**, says how those two end up on disk.

| Layout | What you get | Good for |
|---|---|---|
| **Stereo — Them left, you right** (the default) | One stereo file. The far end is on the left channel, your transmit audio on the right. | Checking your own transmit audio: you can hear, or see in an editor, exactly what you sent beside what came back. |
| **Two mono files** | Two files started and stopped together, `… rx.flac` (receive) and `… tx.flac` (transmit). | Editing each side separately, for example to clean up the net and leave your own audio alone. |
| **Mixed** | One mono file with both directions added together. | Sharing a net as one ordinary file that sounds the way it did on the air. |

Each file is 16-bit audio at the rate the connection runs at. While nobody
is talking in a direction, that direction is recorded as silence, so the
two sides always stay lined up in time.

## Saving codec frames

Digital networks don't send audio as sound. They send **codec frames**:
small packets of compressed speech, typically one every 20 milliseconds,
which a **vocoder** (the codec's decoder, hardware like a ThumbDV or
software) turns back into sound at your end. astar can keep those frames
exactly as they arrived, before they are decoded, and the ones it sends,
after your voice is encoded.

That is for [unamblify](https://github.com/rcludwick/unamblify), a separate project that studies and re-decodes these
codecs, not for listening back, so the
switch sits one click in: Settings → **Recording** → **Advanced** → **Save
codec frames**. It is off until you turn it on.

With it on, each recording on a digital network also writes a `captured/`
folder inside the recordings folder, laid out the way unamblify reads its
own captures:

```
captured/<mode>/manifest.jsonl
captured/<mode>/astar-air/<key>.ambe
captured/<mode>/astar-air/<key>.flac
```

* `<mode>` names the codec family.
* Each `.ambe` file holds one transmission's raw frames, back to back, with
  no header.
* Its `.flac` partner is the same transmission decoded, 8 kHz mono.
* `manifest.jsonl` has one line per transmission: who was talking, which
  direction, when, how many frames, and where any were lost.
* `<key>` is the time, network, talker and direction, such as
  `20261026-190004-dstar-W1ABC-rx`.

| Network | Folder under `captured/` | Read by unamblify |
|---|---|---|
| D-Star | `dstar` | yes |
| M17 | `codec2-3200` | yes |
| Fusion (YSF) | `ysf-dmr` | yes |
| NXDN | `nxdn` | no, kept but skipped |
| DMR | `dmr72` | no, kept but skipped |

AllStar and EchoLink have no codec frames to keep: their audio is recorded
as plain sound samples, so an AllStar or EchoLink recording writes no
`captured/` folder.

These are recordings taken off the air. There is no clean original of what
the talker said to compare them with, so they are useful for listening
tests and for re-decoding through unamblify, but they are not training
pairs.

## The folder

Settings → **Recording** shows the recordings folder by its Finder name,
with the full path in its tooltip. It starts as `~/Music/astar Recordings`,
which astar creates the first time it records.

* **Choose…** picks another folder. The next recording goes there; files
  already recorded stay where they are.
* **Show in Finder** opens the folder in a Finder window, creating it first
  if nothing has been recorded yet. If the folder can't be created, the
  reason appears in red under the row.

These settings belong to this Mac. They are not part of an exported
`.astarconfig`, because a folder path means nothing on another computer.

## When recording stops by itself

Two things stop a recording without you asking. In both, the button goes
back to grey, the menu-bar dot disappears, and a line in red under the call
controls says why.

**The disk is nearly full.** astar keeps at least **500 MB** free on the
disk that holds the recordings folder. With less than that free, pressing
**Record** refuses to start and says how much is left, for example
*"disk has only 120 MB free; recording needs 500 MB"*. A recording that is
already running checks every ten seconds and stops cleanly when it crosses
the line. The file it was writing is finished and kept.

**The folder disappeared.** If the recordings folder is deleted, or the
external drive it lives on is unplugged, astar notices within a fraction of
a second and stops, saying the folder is gone. Choose another folder, or
plug the drive back in, and record again.

The message clears when the connection ends or the next recording starts.

To capture a call at the packet level instead, see [Packet log](packet-log.md).
