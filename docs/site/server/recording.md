---
icon: lucide/archive
---

# Recording

`astar-server` can keep a recording of every transmission that passes
through the node: each time someone keys up, on any link, the node writes
what they said to its own audio file. Later you can play those files in a
browser, download them one at a time or all together, or feed them to
[unamblify](https://github.com/rcludwick/unamblify).

Recording is off until you turn it on.

## Turning it on

Add a `[recording]` section to `node.toml`. Here it is with every key at
its default, so `enabled = true` is the only line you have to change:

```toml
[recording]
enabled = true                       # the default is false
dir = "/var/lib/astar/recordings"    # where the recordings go
max_bytes = "2 GB"                   # the most the folder may hold
max_age_days = 30                    # how long a recording is kept
frames = true                        # also keep the codec frames, for unamblify
```

| Key | Default | What it means |
|---|---|---|
| `enabled` | `false` | Record every transmission. |
| `dir` | `"/var/lib/astar/recordings"` | The recordings folder. It must be an absolute path, and the node creates it if it is missing. |
| `max_bytes` | `"2 GB"` | The most the recordings may take up. At least `"100 MB"`. `KB`, `MB` and `GB` count in thousands (`"2 GB"` is 2,000,000,000 bytes); `KiB`, `MiB` and `GiB` count in 1024s; a bare number is bytes. |
| `max_age_days` | `30` | How many days a recording is kept, from 1 to 3650. |
| `frames` | `true` | Also keep the codec frames of M17 transmissions, laid out for unamblify (see [below](#feeding-a-download-to-unamblify)). |

The node checks these as it loads `node.toml` and refuses to start with a
message naming the bad key, for example a relative `dir` or a `max_bytes`
under 100 MB.

The [settings page](configuration.md) has the same rows on its
**Recordings** card: **Record every transmission**, **Folder**, **Keep at
most**, **Keep for days**, and, under the card's advanced rows, **Save codec
frames for unamblify**. Like every key in this section, a change there takes
effect at the next restart.

## What gets recorded

The node records **one file per transmission**. A transmission, which hams
call an **over**, starts when audio begins arriving and ends when it stops.
Two small rules decide where one over ends and the next begins:

* **A gap shorter than half a second is part of the over.** Someone pausing
  for breath, or a link dropping a few packets, does not split a sentence
  into two files. The gap is filled with silence.
* **A gap of half a second or more ends it.** The file is then closed with
  one second of silence on the end, so it doesn't stop dead on the last
  syllable.

Each file is mono FLAC, a lossless audio format that browsers and audio
editors play directly, at the conference's own sample rate. Files go into
one folder per day, named after the time the over started (in UTC), the
network and who was talking:

```
2026-10-27/20261027-010004-allstar-2001.flac
2026-10-27/20261027-010231-m17-W1ABC.flac
2026-10-27/20261027-011500-node-unknown.flac
```

**Who was talking** comes from the node's own events. For AllStar it is the
node number of the link that keyed up within the last two seconds. For M17
it is the callsign the M17 monitor heard. When the node can't tell, the
network reads `node` and the talker `unknown`. When two stations talk over
each other, the file is named for the first one, and the list on the
Recordings page shows both.

A node in **parrot mode** records nothing: there is no conference to listen
to.

## What recordings contain

The recorder listens to the conference the way one more listener would. It
is a member of the conference that can only hear, so it can never put
anything on the air. What it hears is the whole mix: every AllStar link,
the M17 monitor, and a Discord bridge if one is running. Whatever someone
says in the Discord voice channel ends up in the recordings too.

Keeping those recordings, and who you share them with, is your decision as
the node's operator. If the people on your net, or in your Discord channel,
would expect to be told, tell them.

## Keeping the disk in check

The node keeps the folder within the two limits you set:

* Any recording older than `max_age_days` is deleted.
* Then it adds up the recordings and deletes the **oldest first** until the
  total is under `max_bytes`.

It checks both limits when the node starts, after each over is written,
and once an hour, so a node that stays quiet for weeks still lets old
recordings age out. A check never runs in the middle of an over: one that
falls due then waits for the over to end.

It only ever counts and deletes files it wrote itself. It keeps a list of
them, `index.jsonl`, in the folder, and the codec-frame files are listed in
the `manifest.jsonl` files under `captured/`. Anything else you put in the
folder is never counted and never deleted.

Separately, the node keeps at least **500 MB** free on the disk under the
folder, whatever `max_bytes` says. With less than that free, it doesn't
start a new file, and a long over that is already being written is cut
short (what was written is kept). The Recordings page shows a red banner
while this lasts: *"Under 500 MB free on the recordings disk — nothing new
is recorded until there is room again."* Recording picks up again by itself
once there is room.

## The Recordings page

Open **Recordings →** at the top of the node page, or go straight to
`/admin/recordings`. It is admin-only, like the rest of `/admin`: a
listener who is signed in is refused, and on the control port the node's
own loopback is the console admin as usual (see
[Signing in](admin.md#where-you-sign-in)). The `/admin` prefix exists only on
the public listener; the control port serves the same pages without it, so
there the page is at `/recordings`.

The page lists every over, newest first, with its time, length, talker,
network and size. Each row has three buttons:

* **Play** plays the recording in the browser. You can drag to any point
  in it, because the node sends just the part the player asks for. The
  button turns into **Pause** while it plays.
* **Download** saves the `.flac` file.
* **Delete** asks *"Delete the recording from … ? This cannot be undone."*
  and removes the file once you confirm.

The badge at the top says what the recorder is doing: recording an over,
idle, waiting for a conference, or stopped for lack of disk. If recording
is off in `node.toml`, the page says so and links to the **Recordings**
card in Settings.

## Downloading everything

**Download all**, at the top of the page, downloads every recording in one
file: a `.tar` archive from `/admin/recordings.tar`. The node writes the
archive as it sends it, one file at a time, so a big folder needs no extra
disk space and recording and pruning carry on meanwhile. A recording
deleted while the download is running is left out of the archive.

Downloads and **Play** use the same per-client stream slots as the live
audio and event streams: on the public listener one address may hold 4 at
once, and everyone behind one proxy or NAT address shares those 4.

Add `?since=` and an RFC 3339 time to fetch only what started at or after
it, for example everything from 26 October on:

```
/admin/recordings.tar?since=2026-10-26T00:00:00Z
```

From the node itself, `curl` against the loopback control port is the
easiest way, since the loopback is the console admin and needs no sign-in:

```bash
curl -o recordings.tar 'http://127.0.0.1:8730/recordings.tar?since=2026-10-26T00:00:00Z'
```

From your own computer, open an SSH tunnel first
(`ssh -N -L 8730:127.0.0.1:8730 <user>@<node-host>`) and run the same
command there. A `since` the node can't read is refused with *"since must be
an RFC 3339 time"*.

Inside, the archive looks like this:

```
recordings/index.jsonl
recordings/2026-10-27/20261027-010004-allstar-2001.flac
captured/codec2-3200/manifest.jsonl
captured/codec2-3200/astar-air/20261027-010231-m17-W1ABC-rx.ambe
captured/codec2-3200/astar-air/20261027-010231-m17-W1ABC-rx.flac
```

`recordings/` holds the audio and its list. `captured/` holds the codec
frames, and appears only with `frames = true` and some M17 traffic.

## Feeding a download to unamblify

Digital networks send speech as **codec frames**, small packets of
compressed speech that a vocoder decodes back into sound. M17 uses Codec 2
at 3200 bit/s. With `frames = true`, the node keeps each M17 over's frames
exactly as they arrived, before decoding, beside the decoded audio, in the
layout unamblify reads its own captures from:

* `captured/<mode>/astar-air/<key>.ambe` is the raw frames, back to back,
  with no header.
* `captured/<mode>/astar-air/<key>.flac` is the same over decoded, 8 kHz
  mono.
* `captured/<mode>/manifest.jsonl` has one line per over.

To use a download with unamblify:

1. Unpack it into an empty folder: `mkdir air && tar -xf recordings.tar -C air`.
2. Point unamblify's data root at the folder that holds `captured/`, either
   with `--data-root air` or by setting `UNAMBLIFY_DATA`.
3. Check the files: `unamblify verify --data-root air --mode codec2-3200`.

unamblify reads the `dstar`, `ysf-dmr` and `codec2-3200` folders. The
`nxdn` and `dmr72` folders that the macOS app can write are kept, but
unamblify skips them.

For a debugging log of the raw packets rather than the audio, see
[Packet log](packet-log.md).

These are off-air recordings, so there is no clean original of what was
said to pair them with. They are for listening tests and for re-decoding,
not for training pairs.
