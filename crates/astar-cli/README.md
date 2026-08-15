# astar-cli

A thin command-line front-end over the `astar-iax` engine crates. It places
no protocol or audio logic of its own: registration goes through
`astar_iax::Registrar`, calls through `astar_iax::Manager`, and audio through
`astar_audio::CpalBackend`.

## Commands

```
astar-cli register [OPTIONS] <host[:port]> <user> [password]
astar-cli dial     [OPTIONS] <host[:port]> <number>
astar-cli parrot   [OPTIONS] [host[:port]] [number]
```

The port defaults to `4569` (the IAX2 well-known port) when omitted.

Run `astar-cli --help` or `astar-cli <command> --help` for the full
option list.

### `register`

Registers to an IAX2 peer, prints each lifecycle transition
(`registering` -> `REGISTERED` -> ...), and reports the terminal result. On a
successful registration it deregisters cleanly before exiting, so it leaves no
dangling binding. `--timeout` bounds the wait for a terminal result.

```
astar-cli register asterisk.example.com myuser s3cr3t
astar-cli register 10.0.0.5:4569 node --password hunter2 --refresh 120
```

### `dial`

Places a call to `<number>`, routes the default (or chosen) microphone, drives
the call to `answered`, and prints call-progress events. PTT is read from
stdin (see below).

```
astar-cli dial pbx.example.com 1001
astar-cli dial --input "USB" --output "Monitor" pbx.example.com 1001
```

### `parrot`

The `dial` path with parrot-friendly defaults: host `127.0.0.1:4569` and the
ASL3 public parrot/echo extension `55553`. Key PTT, talk, then unkey; the
remote parrot echoes your audio back through the selected output device.

```
astar-cli parrot                       # 127.0.0.1:4569, ext 55553
astar-cli parrot 104.232.32.242:4569 55553
```

## Keyboard (stdin) PTT control protocol

PTT is driven from **stdin**, one command per line (press Enter). It is
deliberately line-based — no terminal raw-mode dependency — so it works over
pipes and in non-interactive harnesses.

| Input                | Action                                   |
| -------------------- | ---------------------------------------- |
| `k`, `key`, `1`      | engage PTT (mic audio flows to the peer) |
| `u`, `unkey`, `0`    | release PTT                              |
| `t`, `toggle`        | flip the current PTT state               |
| *(empty line)*       | toggle PTT (quick push-to-talk tap)      |
| `d <digit>`          | send a DTMF digit (e.g. `d 5`, `d #`)    |
| `q`, `quit`, `hangup`| hang up and exit                         |
| EOF (Ctrl-D)         | hang up and exit                         |

Anything else prints an "unknown command" hint and is ignored.

## Audio device selection

`dial` and `parrot` accept `--input <substr>` / `--output <substr>` to pick a
capture/playback device by a case-insensitive, unique name substring (matching
the resolution used by the `astar-iax` `parrot` example). With no flag, the
system default device is used. `--list-devices` enumerates devices and exits.

## Running

```
cargo run -p astar-cli -- --help
cargo run -p astar-cli -- parrot 127.0.0.1:4569 55553
```
