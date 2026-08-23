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
    D-Star is the one network this daemon must never key remotely. See
    [Control API](control-api.md#ptt).

## Next steps

* [Configuration](configuration.md) — every section of `node.toml`.
* [Control API](control-api.md) — the HTTP and SSE surface.
