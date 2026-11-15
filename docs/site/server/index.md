---
icon: lucide/server
---

# astar-server

`astar-server` is the **node daemon**: a long-running, headless process that
holds one IAX2 station open, answers inbound calls, optionally registers with
the AllStarLink registrar, and bridges everyone connected to it.

It is AllStarLink-first — it speaks the `app_rpt` link-layer dialect and
node-to-node `ilink` semantics — but plain IAX2 underneath. It is the same
engine the [macOS app](../macos/index.md) uses, with no UI attached.

!!! warning "Work in progress — read this before deploying one"

    The daemon runs and has been used against live nodes, but it is **not
    finished** and is not a drop-in `app_rpt` replacement. Known gaps, each with
    a backlog item:

    | Gap | Item |
    |---|---|
    | Inbound authentication is not wired to config. `IncomingCallPolicy` supports MD5 challenge/validate, but `node.toml` has no credentials map, so the practical default is an **open listener**. | `iax-12fb` |
    | The D-Star keying guard is a runtime check in the controller, not a build-level one. Cargo feature unification compiles D-Star into the daemon even though its manifest does not ask for it. | `iax-d9f4` |
    | Node-to-node link control (`*3` / `*2` / `*1`) is not at parity with AllStar. | `iax-d829` |
    | In-band DTMF from RX/mic is not decoded into the command mapper — DTMF commands arrive over the control channel only. | `iax-72df` |
    | `/events` is not flushed per frame, so the status page lags roughly 3 s. | `iax-5562` |

    These pages document **what exists today**, so that anyone — human or agent —
    picking up one of those items can see the current shape without reading
    6,400 lines of Rust first. Where behaviour is unfinished it says so.

## What it does

* **Inbound IAX2 listener** on UDP 4569 (bind address, answer policy, call cap,
  authentication policy, and an optional caller allowlist are all configurable).
* **Registration** with an upstream registrar, so your node is reachable by
  number.
* **Node-to-node links** — connect (`*3`), monitor (`*2`), disconnect (`*1`).
* **M17 reflector monitor** — link to one module of an M17 reflector,
  receive-only, from Discord (`/dial m17`) or `POST /m17`. Off until
  `node.toml` has an `[m17]` section. The node never sends voice to the
  reflector, but what it hears there is relayed to every AllStar node and
  EchoLink station linked to this node, and a linked radio transmits it —
  see the warning in [Configuration](configuration.md#m17-receive-only-m17-reflector-monitor).
* **Conference bridge** — mix-minus by default, so each member hears everyone
  but themselves; switchable live between handset, bridge and conference
  topologies.
* **Voice / CW announcements** — a station ID and per-event announcements
  (incoming call, hangup, registered, answered), optionally through a TTS
  binary.
* **DTMF command execution** — off by default, because enabling it lets *any*
  connected member command your links.
* **A control channel**: HTTP for commands, Server-Sent Events for a live
  stream, plus a small read-only status page.

## Running it

```bash
cp deploy/node.toml.example node.toml   # then edit it
just node                               # astar-server serve --config node.toml
```

Directly, without `just`:

```bash
astar-server serve --config node.toml   # HTTP + SSE control channel
astar-server tui   --config node.toml   # interactive stdin menu
```

Two subcommands, one config file:

| Subcommand | What it is |
|---|---|
| `serve` | The daemon. Runs the control channel and the node loop until `SIGINT`, `SIGTERM`, or `POST /shutdown`. |
| `tui` | An interactive terminal menu over the same controller — handy for bring-up on a machine you are sitting in front of. |

Argument parsing is hand-rolled rather than `clap`: the binary accepts a
subcommand, `--config <path>`, and `--help`, and nothing else.

If the config path does not exist, `serve` writes a commented template there and
carries on with safe defaults rather than exiting — a daemon under
`--restart=always` must not crash-loop over a missing file.

## Credentials never touch node.toml

!!! danger "The config file is secret-free by design"

    `node.toml` carries **no passwords and no tokens**. Credentials live in
    `credentials.toml` beside it — mode `0600`, written by the settings page —
    or arrive from the environment or the loopback control port, whichever
    `[secrets] source` names. The `[portal]` section names an account, never
    a password.

    Secrets are connect/init arguments only: they are never stored on a station,
    never present in snapshots, events or errors, and never logged. Do not put a
    password in an example config, a bug report, or a screenshot.

    See [Configuration](configuration.md#secrets).

## Keying is operator-supervised

!!! danger "`POST /key` is remote keying"

    The control channel can key the transmitter. Bind it to **loopback**
    (`127.0.0.1`) — every example here does — and treat a key command as
    something a licensed operator does deliberately, not something a script
    does on a timer.

    `POST /key` is **refused outright while a D-Star session is active**:
    D-Star is the one network this daemon must never key remotely. The guard is
    `key_refusal` in `controller.rs`, checked on every `NodeCommand::Key`. See
    [Control API](control-api.md#ptt).

    IAX2 and the console's own M17 client session are remotely keyable; the
    node's M17 *monitor* has no transmit path toward the reflector, and
    `POST /key` cannot reach it. What the monitor hears is still relayed to
    the node's AllStar and EchoLink members, and their radios transmit it.

## Reading the link log

Every AllStarLink link the node makes or answers writes a short story to its
log: one line when the call starts, one when the codec is agreed, one when the
link comes up, and one when it ends. The log is the server's standard output,
so it goes wherever your service manager puts that. In the usual container,
named `iaxnode`, read it with:

```bash
podman logs --since 1h iaxnode | grep astar_iax::link
```

If the container hands its output to journald (`--log-driver journald`), the
same lines are in the journal, and they survive a container restart:

```bash
journalctl CONTAINER_NAME=iaxnode --since today | grep astar_iax::link
```

Every line carries the same four fields first, so you can follow one link
through the log even while several are up at once:

| Field | Means |
|---|---|
| `direction` | `in` when the far node called this one, `out` when this node dialled it. |
| `peer` | The far end's IP address and UDP port. |
| `node` | The node number the caller claims to be (`in`), or the number this node dialled (`out`). `?` when a caller sent neither a number nor a username. The caller chooses this text, so the log keeps at most 64 characters of it and writes any control character or space in it as `?`. |
| `call` | This node's own call number for the link, which tells apart two links from one address. |

Here is one link from start to finish, a far node calling in and then hanging
up after about two and a half seconds (the timestamps are shortened):

```text
02:58:55.053Z  INFO astar_iax::link: link starting direction=in peer=203.0.113.7:4569 node=1999 call=1
02:58:55.053Z  INFO astar_iax::link: link codec settled direction=in peer=203.0.113.7:4569 node=1999 call=1 codec=ulaw
02:58:55.054Z  INFO astar_iax::link: link up direction=in peer=203.0.113.7:4569 node=1999 call=1 setup_ms=0
02:58:57.512Z  INFO astar_iax::link: link ended direction=in peer=203.0.113.7:4569 node=1999 call=1 ended_by=them cause="Normal Clearing" duration_s=2.5 was_up=true
02:58:57.512Z  WARN astar_iax::link: short link: it ended less than 10 s after coming up direction=in peer=203.0.113.7:4569 node=1999 call=1 ended_by=them duration_s=2.5
```

Taking the lines one at a time:

* **`link starting`** is written the moment this node dials out. For a call
  that arrives, it is written once the caller has proved its address is real.
  The node answers a caller's first request with a *call token*, a value the
  caller has to send back with its next request, and only a caller at the
  address it claims ever sees the token. The node counts the token's age in
  whole seconds and accepts it up to ten, so a token is good for between ten
  and eleven seconds after it was sent, and only from that address and that
  call. Until it comes back the node
  keeps nothing for the caller at all, so a scanner or a forged address
  costs it nothing and writes nothing in the log. A request carrying a token
  that is wrong or too old is refused at once. If the node is set not to ask
  for tokens, `link starting` is written as soon as the call arrives.
  A call refused for a bad request or an unknown user has its own line
  instead: `rejecting inbound call`, on the target `astar_iax::listener`,
  with the `cause` it was refused for.
* **`link codec settled`** names the voice codec both ends agreed on, in the
  spelling Asterisk uses: `ulaw`, `alaw`, `gsm`, `slin` or `slin16`.
* **`link up`** means the call was answered and audio can flow. `setup_ms` is
  how long that took after `link starting`; on a link this node dials, it
  includes the network round trip and any authentication.
* **`link ended`** says how the link finished:
    * `ended_by` is `us` when this node hung up (an operator, a DTMF or Discord
      command, a reject, or the server shutting down), `them` when the far end
      sent a hangup or a reject, and `timeout` when the far end stopped
      answering and this node gave up on it.
    * `cause` is the cause text from the hangup or reject, when there was one,
      such as `"Normal Clearing"`. Otherwise it describes what the node saw:
      `"peer timed out (in Active)"` for a far end that went silent while the
      link was up, `"hung up locally"` for our own hangup with no reason given.
      Like `node`, a cause sent by the far end is cut to 64 characters, with
      control characters written as `?`.
    * A hangup counts as ours from the moment this node starts it on the
      wire. When this node hangs up on an AllStar node, it first sends
      `!!DISCONNECT!!` and waits a quarter of a second before the hangup
      itself. The far node usually hangs up during that wait, in answer, and
      the line still says `us`, with our cause. Only a hangup that arrives
      while this node had sent nothing toward ending the link says `them`.
    * `duration_s` is how long the link was up, in seconds. For a call that
      never came up it counts from `link starting` instead, and `was_up` is
      `false`.
* **`short link`** is a warning written alongside `link ended` when a link
  that came up lasted less than ten seconds. A healthy link stays up for
  minutes or hours, so this is the line to search for when a far node reports
  that its link to yours keeps dropping:

    ```bash
    podman logs iaxnode | grep 'short link'
    ```

    The `link ended` line just above it says who ended the link and why.

The log never holds the node secret, a password or a call token. It writes
nothing per audio packet, so a link that stays up for a day costs four lines.

## Working on it

The crate is `crates/astar-server`, about 6,400 lines of Rust. Where things
live:

| File | Lines | What is in it |
|---|---:|---|
| `controller.rs` | 2278 | The node loop and every `NodeCommand`. Start here — this is where behaviour lives. |
| `http.rs` | 553 | The request router and JSON responses. One arm per route. |
| `main.rs` | 372 | Argument parsing, config load, station construction, mode dispatch. |
| `tui.rs` | 303 | The interactive menu, over the same controller as `serve`. |
| `dtmf_commands.rs` | 254 | DTMF sequence to `NodeCommand` mapping. |
| `secrets.rs` | 245 | Runtime secret injection. Nothing here is written to disk. |
| `server.rs` | 196 | The HTTP listener and the `/events` SSE path, ahead of the router. |
| `sse.rs` | 169 | Event serialization for the SSE stream. |
| `config.rs` | — | `node.toml` deserialization; one struct per section. |
| `m17_directory.rs` | — | `[m17]` reflector names to `host:port`: the operator's table, then the bundled or file directory. |
| `template.rs` | 85 | The commented config written when the file is missing. |

Tests are `crates/astar-server/tests/config_bootstrap.rs` and `serve_wiring.rs`,
plus unit tests inside the source files:

```bash
cargo test -p astar-server
```

The controller is driven by a pump rather than threads-per-connection, so a
behaviour change is usually a new `NodeCommand` arm plus a route in `http.rs`
plus a menu entry in `tui.rs` — the three surfaces are deliberately parallel.

Three rules that are load-bearing rather than stylistic, and which a change here
must not break:

* **Secrets are in-args only.** They never reach a `Station`, a snapshot, an
  event, an error, or a log line. `secrets.rs` exists to keep it that way.
* **The `key_refusal` guard stays.** `POST /key` must keep refusing while
  `dstar_active`. `iax-d9f4` tracks making that structural instead of a runtime
  check; until then the runtime check is the only thing holding the line.
* **The M17 monitor stays receive-only.** `POST /m17` and `/dial m17` carry a
  reflector *name*, never an address, and `M17Action` has only `monitor` and
  `disconnect`. The monitor itself lives in `astar-console`, whose
  `tests/m17_monitor.rs` and `tests/m17_monitor_vocabulary.rs` fail if it ever
  sends anything but control packets.

## Next steps

* [Configuration](configuration.md) — every section of `node.toml`.
* [Control API](control-api.md) — the HTTP and SSE surface.
* [Building astar-server](../build/server.md) — the build itself.
