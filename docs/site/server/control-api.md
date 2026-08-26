---
icon: lucide/terminal
---

# Control API

`astar-server serve` exposes a small HTTP control channel plus a Server-Sent
Events stream, on the address in `[control] bind`.

!!! danger "Bind it to loopback"

    There is no authentication on this channel, and it can **key a
    transmitter**. `127.0.0.1:8730` is the documented bind address and every
    example below uses it. If you need it from elsewhere, put it behind
    something that authenticates — a VPN, an SSH tunnel, a reverse proxy — and
    treat remote keying as an operator-supervised action.

## Conventions

* Commands are `POST`. Bodies are JSON; commands with no parameters take an
  empty body.
* A command that returns state replies with the **snapshot** JSON described
  below. A command with nothing to report replies `{"ok":true}`.
* Errors are `{"error":"<message>"}` with a `400` (bad request body) or `500`
  (the command failed) status. Error messages are secret-free by construction.

## Status

### `GET /status`

Returns a point-in-time snapshot:

```json
{
  "node_id": "77777",
  "listening": true,
  "registered": true,
  "calls": [],
  "links": []
}
```

| Field | Meaning |
|---|---|
| `node_id` | This node's ID — the register username. `null` when registration is unconfigured. |
| `listening` | Whether the inbound listener is accepting calls. |
| `registered` | Whether registration with the upstream registrar is current. |
| `calls` | Active calls. |
| `links` | Live node-to-node links — the `app_rpt` `RPT_LINKS` analogue. |

The snapshot is secret-free. It never contains a password or a token.

### `GET /` — status page

A small read-only status page is served from the same port (`/`, plus
`/app.js` and `/style.css`). It subscribes to the event stream and shows what
the node is doing. Read-only: it issues no commands.

### `GET /events` — Server-Sent Events

A live stream of state changes. Each event is JSON with an `event` discriminator:

| `event` | Payload | When |
|---|---|---|
| `snapshot` | the full status snapshot | state changed |
| `incoming_call` | `from` | an inbound call arrived |
| `registered` | — | registration succeeded |
| `register_failed` | `reason` | registration failed |
| `hangup` | `reason` | a call ended |
| `link` | `kind` (`connected` / `disconnected` / `keyed`), `node`, `call`, optional `reason`, optional `keyed` | a link lifecycle edge |
| `dtmf` | `call`, `digit`, optional `command` | a DTMF digit was received; `command` is set when the digit completed a `*` sequence |
| `announcement_started` / `announcement_finished` | `kind` (`id` / `event` / `command`) | an announcement began or ended |

Every event is secret-free.

```bash
curl -N http://127.0.0.1:8730/events
```

## Calls

| Endpoint | Body | Effect |
|---|---|---|
| `POST /dial` | `{"node":"55553"}` | Place an outbound call. |
| `POST /hangup` | — | End the current call. |
| `POST /answer` | — | Answer a ringing inbound call (`answer = "manual"`). |
| `POST /reject` | — | Reject a ringing inbound call. |

```bash
curl -X POST http://127.0.0.1:8730/dial -d '{"node":"55553"}'
curl -X POST http://127.0.0.1:8730/hangup
```

## PTT

| Endpoint | Body | Effect |
|---|---|---|
| `POST /key` | — | Key the transmitter. |
| `POST /unkey` | — | Unkey. |

!!! danger "Remote keying, and one hard refusal"

    `POST /key` transmits. It is a deliberate operator action; never wire it to
    a timer, a watchdog, or an automated test.

    **The daemon refuses to key while a D-Star session is active.** The request
    fails with:

    ```
    refusing to key: a D-Star session is active and
    D-Star transmit is not remotely keyable
    ```

    D-Star is the one network this daemon must never key remotely. Everything
    else reachable from here — IAX2, M17 — is remotely keyable by design. This
    guard is policy, not a bug: do not route around it.

## Inbound listener

| Endpoint | Effect |
|---|---|
| `POST /enable_inbound` | Start accepting inbound calls. |
| `POST /disable_inbound` | Stop accepting inbound calls. |

## Registration

| Endpoint | Effect |
|---|---|
| `POST /register` | Register with the configured registrar. |
| `POST /deregister` | Deregister. |

## Node-to-node links

### `POST /link`

```json
{ "action": "connect", "node": "55553" }
```

| `action` | `app_rpt` equivalent | Meaning |
|---|---|---|
| `connect` | `*3<node>` | Full two-way link (transceive). |
| `monitor` | `*2<node>` | Receive-only link. |
| `disconnect` | `*1<node>` | Tear the link down. |

`addr` may be supplied to dial an explicit address instead of resolving the node
number. An unknown action is a `400`.

## Conference bridge

### `POST /bridge`

Re-wire the bridge live. Every field is optional; an omitted field keeps its
current value.

```bash
curl -X POST http://127.0.0.1:8730/bridge -d '{"mode":"handset"}'
curl -X POST http://127.0.0.1:8730/bridge -d '{"mix_minus":false}'
```

| Field | Values |
|---|---|
| `mode` | `"handset"`, `"bridge"`, `"conference"` |
| `mix_minus` | `true` / `false` |
| `include_local_radio` | `true` / `false` |

An unrecognised mode is rejected.

## Audio devices

### `POST /devices`

```json
{ "input": "USB", "output": "USB" }
```

Substring matches against device names. Either field may be omitted.

## Secrets

### `POST /secrets`

Used when `[secrets] source = "control"`.

```bash
curl -X POST http://127.0.0.1:8730/secrets \
     -d '{"username":"<node>","secret":"<password>"}'
```

!!! info "The reply never echoes the credential"

    On success the handler returns a bare `{"ok":true}` rather than rendering a
    reply object, so the secret has no path into the response even if the reply
    type changes later. Nothing about this request is logged.

## Lifecycle

### `POST /shutdown`

Stops the daemon cleanly. `SIGINT` and `SIGTERM` do the same thing.

## Everything else

Any other path is a `404` with `{"error":"not found"}`.
