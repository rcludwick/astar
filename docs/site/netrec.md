---
icon: lucide/radio
---

# Net recorder

A **net** is a scheduled on-air meeting: every Tuesday at 7 pm, say, a group
of operators gets on a reflector and takes turns checking in. If you cannot
be there, or you want a record of who said what, you need something that
listens at that time every week and keeps what it hears.

`astar-netrec` is that something. It is a small program that runs on a
Linux machine or a Mac and does one job. Shortly before each net you have
listed, it joins the net's reflector, saves the audio until the net is over,
and leaves. What it keeps is a folder per net and per night: one long audio
file of the whole evening, one small file for every **over** (an over is one
person's turn at the microphone, from the moment they key up to the moment
they let go), and a log of who talked when.

## It never transmits

`astar-netrec` is **receive-only**. It has no microphone, no sound device
and no push-to-talk line.

It is built on the same engine as the rest of astar, and that engine knows how
to key a radio, so the keying code is linked into the program. Three layers
keep the recorder away from it:

* **A wrapper with no keying call.** The recorder only ever holds a listener
  that can do three things: join a net, say how the link is doing, and leave.
  None of those keys a radio, and no other part of the recorder may hold the
  engine itself.
* **A check in CI.** `ci/guard-netrec-receive-only.sh` fails the build if the
  recorder's code names a keying call or reaches around the wrapper.
* **An audio backend with no microphone input.** Sound only comes out of the
  engine into the recorder. There is no input to send from, and D-Star's keying
  path refuses to run without one.

In practice that means:

* it never speaks on the reflector, not even to say who it is;
* it never opens a serial port for push-to-talk;
* a radio, a sound card and a USB radio interface can all be missing from
  the machine, and it works the same.

It does log in, because a reflector will not send audio to someone who has
not joined. It joins as your callsign plus a suffix letter, `<callsign> R` by
default, so the reflector's list of connected stations shows it as a separate
entry from you: `N0CALL R` next to `N0CALL`.

It also records other people's voices. Nets are open to the public, but it is
still worth knowing that the recordings sit on the disk named by `root` and
nowhere else, and that nothing leaves the machine.

## Networks it can record

This release records two networks:

| `network =` | Reflector kind |
|---|---|
| `"m17"` | M17 reflectors |
| `"dstar"` | D-Star reflectors that speak DExtra (XLX and the like) |

You name a reflector by its address: `host`, `port` and `module`. Looking
a reflector up by its name (`reflector = "M17-XYZ"`) comes later; for now
that key is refused on an M17 net, and on a D-Star net it is only a label.

AllStarLink and EchoLink are on the way (see [What is coming](#what-is-coming)).
A net with `network = "allstar"` or `"echolink"` is refused with "not yet
supported".

## Installing it

Build it with the release set, which includes the M17 codec and the
software D-Star vocoder:

```bash
cargo build --release -p astar-netrec --features release
```

That makes `target/release/astar-netrec`. Copy it somewhere on your `PATH`,
such as `/usr/local/bin/`. A build without `--features release` still runs,
but M17 then needs a system `libcodec2` at run time, and `check` and `run`
refuse a D-Star net on `[vocoder] dstar = "software"`.

### The first config

Ask it for a config. If the file does not exist, `run` and `check` write a
commented template there and read it back:

```bash
astar-netrec --config ./netrec.toml check
```

Open the file, fill in your callsign and at least one `[[net]]`, and run
`check` again. `check` reads the config, tells you anything that is wrong or
suspicious, and lists the next ten sessions it would record, with the time
it will join and the time it will leave. It makes no network connection.

### Running it as a service on Linux

[`astar-netrec.service`](netrec/astar-netrec.service) is a systemd unit. It
runs as an `astar` user, reads `/etc/astar/netrec.toml`, may write only to
`/var/lib/astar/netrec`, and restarts on failure after 30 seconds. The
comments at its top list the same commands as these. Run them from the folder
that holds the binary, the unit and the `netrec.toml` you filled in above:

```bash
sudo install -m 0755 astar-netrec /usr/local/bin/
sudo useradd --system --home /var/lib/astar astar
sudo install -d -o astar -g astar /var/lib/astar/netrec
sudo install -d /etc/astar
sudo install -m 0644 netrec.toml /etc/astar/netrec.toml
sudo install -m 0644 astar-netrec.service /etc/systemd/system/
sudo systemctl enable --now astar-netrec
```

Skip the `useradd` line if the machine already has an `astar` user.

The config has to be in place before the service starts. The service sees
`/etc` as read-only, so it cannot write the template there the way `check` does
in your own folder. Without a config it fails, and systemd tries again every 30
seconds until there is one. The same goes for `root`: leave it at
`/var/lib/astar/netrec`, because that is the only folder the service may write.

The 30-second wait is deliberate. A start that is refused (see
[One recorder at a time](#one-recorder-at-a-time)) would otherwise be retried
fast enough to hit systemd's limit on restarts and be given up on.

### Running it as a service on a Mac

[`org.astar.netrec.plist`](netrec/org.astar.netrec.plist) is a launchd agent.
Copy it to `~/Library/LaunchAgents/`, replace `YOU` with your user name
(launchd does not expand `~`), and load it:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/org.astar.netrec.plist
```

It starts at login, restarts if it exits, and writes its log to
`~/Library/Logs/astar-netrec.log`. A Mac that is asleep records nothing; see
[Sleep](#when-the-machine-sleeps).

## The commands

Every command takes `--config <file>`, which defaults to `./netrec.toml`.

| Command | What it does |
|---|---|
| `run` | Runs the recorder: waits for each net on the schedule, records it, and starts the next wait. This is what the service runs. |
| `check` | Validates the config and lists the next ten sessions. Writes the template if there is no file. |
| `now <NET> [--for 30m]` | Records the net called `NET` (any capitalisation) right now, for the length given, ignoring the schedule. Use it to try a new net. `Ctrl-C` ends it early and the file is kept. |
| `status` | Prints one line about the recorder using this config's `root`: `recording "Tuesday M17 net" since 19:00, linked`, `waiting for "Tuesday M17 net", joining Tue 2026-11-10 18:55`, `idle, nothing scheduled`, or `not running`. |

`status` and `now` need the config to exist already; they never write a
template, because a template's `root` would not be your real archive.

Logs go to standard error and follow `RUST_LOG` (the default is `info`).

### One recorder at a time

Two recorders logging in with one identity would each knock the other off
the reflector, so only one may run per `root`. The first to start takes an
operating-system lock on the file `root/.netrec.lock`; a second one refuses
with "another recorder is running".

That also means `now` cannot run while the service runs. To try a net by
hand, stop the service, run `now`, and start the service again.

The lock file is never deleted, and that is normal: the lock is held by the
running program, not by the file's existence, and the system releases it the
moment the program exits, even by a crash. Keep `root` on a local disk. The
lock may not work on NFS, SMB or a container bind mount.

## The config file

`netrec.toml` has four kinds of section: `[recorder]`, `[web]`, `[vocoder]`
and one `[[net]]` for each net. An unknown key is an error rather than
being ignored, so a typo shows up at once. Here is the template with every
default written out:

```toml
[recorder]
root = "/var/lib/astar/netrec"
callsign = "N0CALL"
callsign_suffix = "R"
my_callsigns = ["N0CALL"]
max_bytes = "50 GB"
max_age_days = 365
pre_roll = "5m"
post_roll = "10m"

[web]
local = false
bind = "127.0.0.1:8050"

[vocoder]
dstar = "dongle"
trusted_serials = []

[[net]]
name = "Tuesday M17 net"
network = "m17"
host = "m17.example.org"
port = 17000
module = "C"
when = "Tue 19:00"
duration = "90m"
tz = "America/Denver"
visibility = "private"
my_side = false
```

### `[recorder]`

| Key | Default | What it means |
|---|---|---|
| `root` | required | The folder the archive is kept in. The recorder creates it if it is missing. |
| `callsign` | required | The callsign it logs in with. The placeholder `N0CALL` works but draws a warning. |
| `callsign_suffix` | `"R"` | One uppercase letter or digit appended to the callsign for the login, so the reflector lists `N0CALL R`. Anything else is refused: a space would make the login read as your own callsign. |
| `my_callsigns` | `[]` | Callsigns that count as "my side". Used by the page that comes in a later release; required if any net sets `my_side = true`. |
| `max_bytes` | `"50 GB"` | The most the archive may hold. `KB`, `MB` and `GB` count in thousands; `KiB`, `MiB` and `GiB` count in 1024s; a bare number is bytes. A value under 500 MB draws a warning, because an archive that small may delete sessions soon after recording them. This limit is separate from the free space on the disk; see [When the disk fills](#when-the-disk-fills). |
| `max_age_days` | `365` | How many days a finished session is kept, 1 to 3650. |
| `pre_roll` | `"5m"` | How long before a net's start the recorder joins. At most 24 hours. |
| `post_roll` | `"10m"` | How long after a net's end it stays. At most 24 hours. |

A duration is a run of number-and-unit pieces: `"90m"`, `"1h30m"`, `"45s"`.

The rolls exist because nets rarely start or finish on the minute. An early
start and a late finish cost a few megabytes and make sure the first
check-in and the last goodnight are on the file.

### `[[net]]`

| Key | Default | What it means |
|---|---|---|
| `name` | required | The net's name. It names the net's folder and appears in the log and in `status`. Names are matched in any capitalisation by `now`. It cannot be empty, `.` or `..`, or hold a slash or backslash. |
| `network` | required | `"m17"` or `"dstar"`. |
| `host` | required | The reflector's hostname or address. |
| `port` | 17000 (M17), 30001 (D-Star) | The reflector's UDP port. |
| `module` | required | The module letter, `A` to `Z`. |
| `reflector` | none | D-Star only: the reflector's name, such as `XLX458`, kept as a label in `session.json`. |
| `when` | required | The schedule; see [Saying when](#saying-when). |
| `duration` | required | How long the net runs, from more than zero up to 24 hours. |
| `tz` | required | The time zone `when` is read in, such as `America/Denver`. |
| `visibility` | `"private"` | `"private"` or `"public"`. Recorded in `session.json`; the page that uses it is a later release. |
| `my_side` | `false` | Whether the page should default to "my side". Later release. |
| `video` | `true` | Read now and used by a later release; it does nothing yet. |
| `youtube_live`, `youtube_upload` | `false` | Read now and used by later releases; they do nothing yet. Both need `visibility = "public"`. |

The config is also refused when two nets' windows overlap (counting the
rolls), because one recorder can be in one place at a time. Move one net or
shorten the rolls. The check looks a year ahead, and further if a one-off date
is further away, up to ten years.

Two nets cannot share a name, or two names that make the same folder, such as
`Tuesday net` and `tuesday-net`: their sessions would land in one place.

### `[web]`

`local` and `bind` are read and checked, but nothing serves a page yet. They
are for a later release.

### Saying when

`when` is one of four shapes, read in the net's own `tz`:

| Text | Meaning |
|---|---|
| `"Tue 19:00"` | Every Tuesday at 7 pm. |
| `"Mon Wed Fri 07:30"` | Those three days at 7:30 am. |
| `"daily 21:00"` | Every day at 9 pm. |
| `"2026-11-14 18:00"` | Once, on that date. |

Day names are the three-letter forms or the full names, in any case. Times
are 24-hour.

Because the time is read in the net's zone, a net at "19:00" stays at 19:00
on the wall clock when daylight saving time starts or ends, so its UTC time
moves by an hour. Two moments need a rule, since a local time can be missing
or doubled on the night the clocks change:

* **A time that does not exist.** In the spring the clocks skip from 02:00
  to 03:00, so `02:30` never happens. The net starts at 03:30 that day, the
  skipped hour added on.
* **A time that happens twice.** In the autumn 01:30 comes round twice. The
  net starts at the first one.

`check` shows each session's start in the net's zone with its zone
abbreviation, so you can see the result for any date.

## What a session looks like on disk

Inside `root` there is one folder per net, and inside it one folder per
session, named for the local date and time the recorder joined:

```text
/var/lib/astar/netrec/
  .netrec.lock
  .netrec-state.json
  tuesday-m17-net/
    2026-11-10 1855/
      session.flac
      overs/
        001 19-00-12 N0CALL.flac
        002 19-01-40 KD0XYZ.flac
      log.jsonl
      log.csv
      checkins.txt
      session.json
```

| File | What it holds |
|---|---|
| `session.flac` | The whole session, start to finish, as one mono FLAC file. It runs on the wall clock: silence stays in, so a position in the file is a time of day. |
| `overs/NNN HH-MM-SS CALL.flac` | One file per over: its number, the local time it started, and who the network said was talking (`unknown` when it did not say). Each ends with about a second of tail so it does not stop on the last syllable. |
| `log.jsonl` | One JSON line for each event: `joined`, `over`, `dropped`, `rejoined`, `left`, `error`. It is written as things happen, so it survives a crash. |
| `log.csv` | The overs as a spreadsheet: `start,end,duration_s,talker,file`. Written when the session closes, or rebuilt from `log.jsonl` after a crash. |
| `checkins.txt` | Each callsign heard, once, in the order it first spoke. Written and rebuilt like `log.csv`. |
| `session.json` | The session's facts: the version of this file's layout (`format`, 1 for now), net, network, target, window start and end, `visibility`, `my_side`, `sample_rate`, when it started and ended, **how** it ended (`window`, `stopped`, `crashed` or `error`), how many times it rejoined, how many frames were dropped, and how many overs there were. |

Two files live in `root` itself: `.netrec-state.json`, which `status` reads,
and `.netrec.lock`, described [above](#one-recorder-at-a-time).

Audio is saved at the rate the engine delivers it, 8000 Hz by default, and
`session.json` records the rate in `sample_rate`. An over's end is its start
plus its duration, and each over includes that tail of about a second.

If the same minute is used twice (a `now` run right after a scheduled one,
say), the second folder gets ` (2)` on the end rather than overwriting.

### When a link drops

If the reflector link drops mid-net, the recorder rejoins, waiting a second
after the first failure and twice as long after each one, up to 30 seconds. The
waits go back to one second only after a link has stayed up for a whole minute.
That way a reflector that accepts the login and drops it straight away is not
logged in to once a second all evening. The file stays on the wall clock: the
missing time is silence, the log gets a `dropped` row and a `rejoined` row, and
`session.json` counts the rejoins.

### When the machine sleeps

A sleeping computer cannot record. Two cases:

* **The machine sleeps through the end of a window.** When it wakes, the
  session closes at once. The file stops where the machine fell asleep, so it
  is short by the rest of the window. It never rejoins late.
* **The machine sleeps in the middle of a window.** The file is short by
  the time it slept, and the log gets an `error` row saying the clocks
  differ. Nothing can be done about that afterwards.

For a Mac that must record, turn off sleep while it is plugged in
(`caffeinate` or Energy settings). A Linux server does not sleep unless you
told it to.

### When the disk fills

The recorder refuses to start a session, and stops a running one cleanly,
with an `error` row in the log, once the volume holding `root` has less
than 500 MB free. What was recorded is kept.

## Retention

The recorder keeps the archive inside the limits you set, and it is the only
thing that ever deletes anything:

1. Any finished session whose end is older than `max_age_days` is deleted.
2. If the archive is still over `max_bytes`, the oldest finished sessions are
   deleted until it fits.

It runs at start-up, after every session closes, and once an hour while it
is idle. Four kinds of folder are never deleted: a session that is still
recording, a session that has not finished, a session whose `session.json`
cannot be read, and anything that is not a plain folder holding a
`session.json` directly below a plain net folder. (Symbolic links are never
followed, so a link planted in the archive cannot point deletion somewhere
else. `root` itself may be a link.)

A session that was open when the machine lost power is marked `crashed` at
the next start. Its audio needs no repair: a FLAC file with an unknown length
is valid. The same start rebuilds what the session never got to write:
`log.csv`, `checkins.txt`, and the counts of overs and rejoins in
`session.json`. All of it comes from `log.jsonl`, so the over that was still
going when the power went has its audio in `overs/` but no line in the
summaries. The count of dropped frames is lost, and reads 0.

## The D-Star vocoder

M17 audio is decoded by a codec that ships inside the program. D-Star voice is
compressed with AMBE, and decoding it takes either a hardware dongle
(a ThumbDV or DVstick) or the built-in software vocoder. You choose:

```toml
[vocoder]
dstar = "dongle"      # or "software"
trusted_serials = []
```

* **`"dongle"`** (the default) uses a stick. A serial port that is not a
  vocoder can be a radio interface, and opening one can key a transmitter, so
  the recorder opens **only** sticks whose USB serial number is listed in
  `trusted_serials`, and nothing else.
  A D-Star net on `"dongle"` with an empty list is refused by `check`. This
  list means the same as `trusted_serials` in `node.toml`.
* **`"software"`** uses Bruce Perens' D-Star vocoder inside the program. No
  stick is needed and no port is opened. It needs a build with the
  `dstar-soft-ambe` feature, which `--features release` includes, and `check`
  refuses a D-Star net on it in any other build. It is for
  D-Star only; the AMBE+2 codec of DMR, NXDN and Fusion is patented until
  2028 and the recorder does not record those.

The dongle is opened **only when a D-Star net actually starts**, never when the
program starts. A machine running only M17 nets, or waiting for the next net
on a Tuesday, never touches a USB port.

### Adding your stick to the list

The list holds USB serial numbers, and you type them in yourself. First find
your stick's serial without opening any serial port:

* On a Mac, run `system_profiler SPUSBDataType` and find the ThumbDV or DV3000
  entry. Its **Serial Number** line is the one you want.
* On Linux, run `lsusb -v` and look for the same device's `iSerial` line.
* Or, if you have `astar-server` built with `--features vocoder`, run
  `astar-server vocoder list --config node.toml`. It prints the vocoder-class
  USB devices it can see, and it opens nothing to do so.

Then copy that serial into `netrec.toml`. This one is made up:

```toml
[vocoder]
dstar = "dongle"
trusted_serials = ["AB12CD34"]
```

Only a ThumbDV or DV3000 stick belongs in this list. A listed port is opened
when a D-Star net starts, and opening a radio interface's serial port can key
a transmitter. The [server configuration page](server/configuration.md#vocoder-trusted-ambe-vocoder-sticks) explains the risk in more
detail.

## What is coming

Later releases add these. They are not in this one, and the config keys for
them either do nothing or are refused:

* **AllStarLink and EchoLink nets.** Each needs an identity of the
  recorder's own: an AllStar node registered for it, and a validated
  EchoLink login. Until then they are refused as "not yet supported".
* **Looking up a reflector by name**, so `reflector = "M17-XYZ"` replaces
  `host` and `port`.
* **A web page** to browse sessions and play overs, with the "my side"
  view (`[web]`, `visibility`, `my_side`, `my_callsigns`).
* **A compressed `session.m4a` and a `peaks.json`** waveform for the page.
* **Video**, and **YouTube**: live streaming and upload (`video`,
  `youtube_live`, `youtube_upload`).
