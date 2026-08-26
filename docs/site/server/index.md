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

## Credentials never touch disk

!!! danger "The config file is secret-free by design"

    `node.toml` carries **no passwords and no tokens**. The registration secret
    arrives at runtime, either from the environment (`ALLSTAR_SECRET`) or by
    being POSTed to the loopback control port. The `[portal]` section names an
    *environment variable* rather than holding a password.

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
| `template.rs` | 85 | The commented config written when the file is missing. |

Tests are `crates/astar-server/tests/config_bootstrap.rs` and `serve_wiring.rs`,
plus unit tests inside the source files:

```bash
cargo test -p astar-server
```

The controller is driven by a pump rather than threads-per-connection, so a
behaviour change is usually a new `NodeCommand` arm plus a route in `http.rs`
plus a menu entry in `tui.rs` — the three surfaces are deliberately parallel.

Two rules that are load-bearing rather than stylistic, and which a change here
must not break:

* **Secrets are in-args only.** They never reach a `Station`, a snapshot, an
  event, an error, or a log line. `secrets.rs` exists to keep it that way.
* **The `key_refusal` guard stays.** `POST /key` must keep refusing while
  `dstar_active`. `iax-d9f4` tracks making that structural instead of a runtime
  check; until then the runtime check is the only thing holding the line.

## Next steps

* [Configuration](configuration.md) — every section of `node.toml`.
* [Control API](control-api.md) — the HTTP and SSE surface.
* [Building astar-server](../build/server.md) — the build itself.
