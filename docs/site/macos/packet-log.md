---
icon: lucide/bug
---

# Packet log

Packet capture is a debugging tool you press during a call. When you do, astar
writes the voice packets of that call to a file exactly as they arrive, so you
can open them in Wireshark and see what the far side really sent and when. It is
for chasing problems such as an echo or a growing delay, not for everyday use.
Nothing is captured until you press the button.

What the files contain, what to be careful with, and how to read them is on
[Packet logs](../reference/packet-logs.md). This page covers the switches.

The packet log is in the macOS app. The Windows/Linux client is paused and does
not have it.

## Turning it on

Open Settings → **Recording**, open **Advanced**, and turn on **Debug mode**.
A line under the switch says what it does: it adds a capture button beside
Record, and the files it writes hold other people's voices. Turning the switch
on does not capture anything. It only puts the button in reach.

## Capturing a call

Once you are in a call, a small ladybug button sits beside Record. Press it, or
press ⇧⌘R, and astar starts capturing that call. While it runs the ladybug turns
orange and the word **Capturing** appears beside it, so you can tell at a glance
that it is on.

The capture stops when you press the button again, when the call ends, or if the
disk or the size limit runs out. If it stops by itself, or a press is refused,
the reason appears in red under the call controls and in Settings. It clears at
your next press, at the next call, or when you turn Debug mode off.

The button is greyed out while a new call is being dialled, because that call is
about to replace the one you would capture.

A file appears at the first voice packet, so a capture during which nobody talks
leaves no file. Each press makes a new pair of files: the capture itself and the
jitter-buffer history beside it.

## The settings

Once Debug mode is on, two more rows appear.

* **Packet logs folder** shows where the files go, by its Finder name, with the
  full path in its tooltip. It starts as `~/Music/astar Recordings/Packet Logs`.
  **Choose…** picks another folder; it applies at the next press, and files
  already written stay where they were. **Show in Finder** opens the folder,
  creating it first if need be. If it can't be created, the reason appears in
  red under the row.
* **Keep at most** is the most the files may take up in total: 250 MB, 500 MB,
  1 GB (the default), 2 GB or 5 GB. The limit is enforced while a capture runs,
  and the oldest files are deleted first. Once you have captured something, a
  line below shows how much the files use now.

For scale, an EchoLink call writes about 8.5 MB an hour, and the busiest
case, AllStar at full quality, about 126 MB an hour. A very long call is split
into parts so that its newest audio is never the part that is lost; see
[Limits](../reference/packet-logs.md#limits).

Like recording, a capture leaves at least 500 MB free on the disk.

## What stays on this Mac

Neither the captures nor the setting travel in an exported `.astarconfig`. A folder
path means nothing on another computer, and a config file you import should
never be able to switch on something that saves other people's voices.
Importing a file that contains the setting ignores it.

## Using a log

Press the button before the bad moment comes round again, note the time when it
happens, then open the newest `.pcap` from the folder.
[Packet logs](../reference/packet-logs.md) shows how to measure delay and how to
listen to the audio inside.
