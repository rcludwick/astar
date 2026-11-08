---
icon: lucide/bug
---

# Packet log

The packet log is a debugging tool. When you press a button on the Recordings
page, `astar-server` writes the voice packets of every live call to a file
exactly as they arrive, so you can open them in Wireshark and see what the far
side really sent and when. The capture runs until you press the button again,
and it includes calls that start while it is running.

Nothing is captured until you press that button, and the button only appears
when Debug mode is on.

What the files contain, what to be careful with, and how to read them is on
[Packet logs](../reference/packet-logs.md). This page covers the switches.

## Turning on Debug mode

Add a `[packet_log]` table to `node.toml`. Here it is with each key at its
default, so `debug_mode = true` is the only line you have to change:

```toml
[packet_log]
debug_mode = true                    # the default is false
dir = "/var/lib/astar/packet-logs"   # where the logs go
max_bytes = "1 GB"                   # the most the folder may hold
```

| Key | Default | What it means |
|---|---|---|
| `debug_mode` | `false` | Show a **Capture packets** button on the Recordings page. It starts nothing by itself. |
| `dir` | `"/var/lib/astar/packet-logs"` | The folder for the logs. It must be an absolute path, and the node creates it if it is missing. It may be a symlink to another folder, for example on a bigger disk. |
| `max_bytes` | `"1 GB"` | The most the folder may hold, at least `"100 MB"`. The units are the same as for [`[recording]`](configuration.md#recording-recording-every-transmission). When the folder passes it, the oldest closed logs are deleted first. |

The table refuses keys it doesn't know, and the node refuses to start with a
message naming a bad key, such as a relative `dir` or a `max_bytes` under 100
MB.

The [settings page](configuration.md) has a **Packet log** card with the same
rows (**Debug mode**, **Folder**, **Keep at most**), placed between
**Recordings** and **Advanced**. A change takes effect when you press Save, with
no restart. Saving the card also stops a capture that is in progress, so press
the button again afterwards. A restart always leaves capture off, even with
Debug mode on.

With no `[packet_log]` table there is no button, and nothing else about the
node changes.

## Capturing packets

Open the admin **Recordings** page. With Debug mode on, a **Capture packets**
button sits in the header, beside **Download all**. Press it and the button
changes to **Stop capture**, and an amber **Capturing packets** badge appears
next to it. Screen readers hear "Capture started." and "Capture stopped."

While the capture runs, a file appears at each call's first voice packet. A call
that began before you pressed the button is picked up too, from its next packet
on.

If the size limit or the disk runs out, or a write fails, the capture stops by
itself. The badge goes away, the button goes back to **Capture packets**, and an
error banner on the card says what happened. Free some space and press **Capture
packets** again, which also clears the banner. Turning Debug mode off clears it
too. The node notices while the Recordings page is open, or the next time you
open it.

The press itself is refused if the folder can't be written, so a read-only
folder shows up straight away and not at the first call.

## Getting the logs

Under the list of overs sits a **Packet logs** card. Each row is one log file,
with its size and date, a link for the `.pcap` and a link for the companion
`.jitter.jsonl`, and a delete button. A call can have several rows, because a
long call splits into parts. While a call is still writing to a log, that row's
delete button is greyed out, and the log can't be deleted.

An open log downloads at the size it has when you click, so the file can end
partway through a record. Wireshark reads what is there. A long call is split
into several rows named `... part 2`, `... part 3` and so on.

The card lists, downloads and deletes logs even with Debug mode off, as long as
the `[packet_log]` table is present. The logs of an earlier run hold the same
voices, and an admin still has to be able to reach them. With no table at all,
these pages answer `packet log is off`.

Only admins can reach them. The four routes, `GET /packet-logs/state`,
`GET /packet-logs/file`, `POST /packet-logs/delete` and
`POST /packet-logs/capture`, are admin-class, so a read-only viewer or an
anonymous visitor never gets one. A file is always named by the name in the
listing, never by a path. The capture route takes `{"capturing": true}` or
`{"capturing": false}`, the state you want rather than a toggle, and answers 409
when Debug mode is off.

## Using a log

Press **Capture packets** before the bad moment, note the time when it happens,
then press **Stop capture** and download the `.pcap` that covers it.
[Packet logs](../reference/packet-logs.md) shows how to measure delay and how
to listen to the audio inside.
