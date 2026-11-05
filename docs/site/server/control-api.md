---
icon: lucide/terminal
---

# Control API

`astar-server serve` exposes a small HTTP control channel plus a Server-Sent
Events stream, on the address in `[control] bind`.

!!! danger "Bind it to loopback, and never put a reverse proxy in front of it"

    This channel can **key a transmitter**. `127.0.0.1:8730` is the
    documented bind address and every example below uses it. A request from
    the node's own loopback address is trusted as the built-in **console**
    admin with no sign-in, which is what makes these `curl` examples work.
    That trust is per peer address, so a reverse proxy on the node would
    hand it to the whole internet: requests carrying `Forwarded` or
    `X-Forwarded-For` are never trusted, but do not rely on that. To use the
    admin pages from elsewhere, sign in on the public page instead
    ([Signing in](admin.md)), or use an SSH tunnel. With sign-in built in
    (`auth`), any peer that is not loopback must sign in, and
    `[control] loopback_login = true` makes loopback sign in too.

## Conventions

* Commands are `POST`. Bodies are JSON; commands with no parameters take an
  empty body.
* A command that returns state replies with the **snapshot** JSON described
  below. A command with nothing to report replies `{"ok":true}`.
* Errors are `{"error":"<message>"}` with a `400` (bad request body) or `500`
  (the command failed) status. Error messages are secret-free by construction.
* **Every request must carry a `Host` that names this node** — `localhost`, a
  loopback literal, or the address in `[control] bind`. Anything else is
  `403 {"error":"cross-origin requests are not accepted on this port"}`, on
  every route including `/events`. A request with no `Host` at all is
  `400 Host required`. `curl` sends the right `Host` by
  construction; what this refuses is a browser page that has been DNS-rebound
  at the port.
* **Bodies are capped at 64 KiB.** A larger body is
  `413 {"error":"request body exceeds 65536 bytes"}` before anything parses it.
* **Every `POST` answers to the Origin rule.** An `Origin` header, when
  present, must match `Host` (`Origin: null` never does), or the answer is
  the `403` above. A request a browser sent (one carrying `Origin` or
  `Sec-Fetch-Site`) must be `Content-Type: application/json`, or it is
  `415 {"error":"<METHOD> <path> requires Content-Type: application/json"}`.
  That is what stops a page in your browser from keying the transmitter with
  a cross-site `<form>`. `curl` sends neither header, so every `curl`
  example on this page works exactly as before, with or without `-H
  'Content-Type: application/json'`.
* The settings routes, `POST /discord/admin`, `POST /m17` and
  `POST /echolink` additionally require `Content-Type: application/json` of
  every caller, `curl` included.

## Connections

The port answers one request per connection and closes it
(`Connection: close`). A request body needs a `Content-Length`: a chunked
body is refused with
`411 {"error":"chunked request bodies are not accepted; send Content-Length"}`
before any of it is read (`curl -d` and `--data-binary` send a length).
`GET /events` is sent until either side closes the connection, without
chunked framing. Every local tool (the macOS app, the TUI, nodebench, your
scripts) connects from the same loopback address, and together they may hold
up to 128 connections open, 64 of them event streams. Every answer carries
`X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
`Referrer-Policy: no-referrer` and the page's Content Security Policy.

## The `/admin/` prefix

Every route below is also served under `/admin/`: `/admin/status`,
`/admin/key`, `/admin/settings`, and so on, on this port and, for a
signed-in admin, on the public page. The admin pages use relative URLs, so
the same pages work at both. `/admin` itself redirects to `/admin/`. The
unprefixed paths stay, for `curl` and scripts; on the public page they are
404.

## Sign-in routes

With sign-in built in, `/auth/*` carries the sign-in API (sessions, passkey
and security-key ceremonies, authenticator codes, invites, users and the
activity log), used by the pages described in [Signing in](admin.md). Every
`POST` there needs `Content-Type: application/json` and an `Origin`, when
present, that matches `Host`. Its refusals are
`{"error":"<code>","message":"<sentence>"}`: `signin_required` (401),
`reauth_required` (401: confirm it's you, then retry), `admins_only` (403),
and so on. `POST /auth/cli/invite` and `POST /auth/cli/revoke-sessions`
answer only a loopback peer on this port; they are how
`astar-server admin` reaches a running node, and they are 404 anywhere
else.

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
| `m17` | The node's M17 monitor, present only while one exists — see `POST /m17` below. It is never listed in `links`. |
| `m17_enabled` | `true` when `node.toml` has an `[m17]` section with `enabled = true`. |
| `m17_relay` | Present only while `m17_enabled`: `relays_to` (`allstar`, `echolink`), who would hear a monitored reflector right now, and `warning`, the sentence the admin and settings pages show. Counted the same way as the `POST /m17` reply. |
| `m17_available` | `true` when a Codec 2 decoder was found. The status page shows an **M17: no codec** badge when M17 is enabled and this is `false`. |
| `echolink` | The node's EchoLink leg, present only when `node.toml` has an `[echolink]` section — see `POST /echolink` below. |
| `echolink_enabled` | `true` when `node.toml` has an `[echolink]` section. |

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
| `m17` | `kind` (`linking` / `connected` / `disconnected`), `reflector`, `module`, optional `reason` | an M17 monitor lifecycle edge; `reason` comes with `disconnected` |
| `dtmf` | `call`, `digit`, optional `command` | a DTMF digit was received; `command` is set when the digit completed a `*` sequence |
| `announcement_started` / `announcement_finished` | `kind` (`id` / `event` / `join` / `link_connect` / `link_disconnect` / `m17_connect` / `m17_disconnect`) | an announcement began or ended |

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

    D-Star is the one network this daemon must never key remotely. IAX2 and
    the console's own M17 client session are remotely keyable; the node's M17
    *monitor* has no transmit path, and `POST /key` cannot reach it. With only
    a monitor up, `POST /key` fails with `no active call`. This guard is
    policy, not a bug: do not route around it.

    **It also refuses while an EchoLink station is connected** to the node
    (see [`POST /echolink`](#post-echolink)):

    ```
    refusing to key: an EchoLink session is active and
    EchoLink transmit is not remotely keyable
    ```

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
number, but only a loopback `ip:port` literal (`127.0.0.0/8` or `[::1]`) — it
exists for test rigs on the same host. A dial answers the far end's challenge
with the stored link secret, so pointing one at another host would hand that
host something to crack offline. Any other `addr`, including a host name, is
`400 {"error":"addr: only a loopback ip:port literal (127.0.0.0/8 or [::1]) may be dialled directly; leave addr out to dial through AllStar"}`
and nothing is dialled. An unknown action is a `400`.

## M17 reflector monitor

### `POST /m17`

Links the node to one module of an M17 reflector as a **receive-only
monitor**, or tears that monitor down. The reflector's audio is mixed into the
conference. The only packets the node sends the reflector are connect, the
keepalive reply and disconnect. It is off unless `node.toml` has an `[m17]`
section — see [Configuration](configuration.md).

!!! warning "The reflector's audio is relayed, and linked radios transmit it"

    Receive-only means the node never sends voice to the reflector. What it
    hears there is mixed into the conference and relayed to every AllStar node
    linked to this node and every EchoLink station on it, and a linked node
    with a radio puts it on the air. Link only reflectors whose traffic you
    would rebroadcast, and mind your licence's rules on retransmission. The
    reply to a `monitor` says so, with counts (see below).

```json
{ "action": "monitor", "reflector": "M17-USA", "module": "C", "callsign": "AJ7HR" }
```

| `action` | Meaning |
|---|---|
| `monitor` | Link as a receive-only monitor. `reflector`, `module` and `callsign` are all required. |
| `disconnect` | Send the reflector a disconnect and tear the monitor down. Other fields are ignored. |

* **There is no address field.** `reflector` is a name, resolved by the node
  from `[m17.reflectors]` and then from the `[m17]` `directory`. A body
  carrying `addr`, or any other unknown field, is a `400`.
* **`callsign` is required, and `node.toml` has no default for it,** so a
  one-line POST cannot put the operator's callsign on a third-party reflector
  by accident. A callsign is 1 to 9 letters, digits, `-`, `/` or `.`. It is
  uppercased, and so are `reflector` and `module`.
* **One monitor at a time.** It can run beside AllStar links. It has its own
  `m17` field in `GET /status` and its own `m17` event.
* A `monitor` that is accepted answers `{"ok":true}` plus two fields:
  `relays_to`, how many members are connected at that moment and will hear
  the reflector, and `warning`, the sentence that says so. `allstar` counts
  the IAX2 legs that dialled in to this node and the links it dialled in
  transceive; a monitor-only link is never sent the mix, so it is not
  counted. `echolink` counts the EchoLink stations on the node's leg. A
  client that reads only `ok` sees what it always did.
* `{"ok":true}` means the monitor has **started** linking, not that the
  reflector answered. Watch for the `connected` event, or for `m17.state`
  becoming `linked`.
* There is no automatic reconnect, and a restart of the node ends the monitor.
  Received audio has no jitter or reorder buffer yet.

```bash
curl -X POST http://127.0.0.1:8730/m17 \
     -H 'Content-Type: application/json' \
     -d '{"action":"monitor","reflector":"M17-USA","module":"C","callsign":"AJ7HR"}'
curl -X POST http://127.0.0.1:8730/m17 \
     -H 'Content-Type: application/json' \
     -d '{"action":"disconnect"}'
```

| Case | Status | Body |
|---|---|---|
| No `Content-Type: application/json` | `415` | `{"error":"POST /m17 requires Content-Type: application/json"}` |
| An `Origin` that does not match `Host` | `403` | `{"error":"cross-origin requests are not accepted on this port"}` |
| Not JSON, an unknown `action`, or an unknown field such as `addr` | `400` | `{"error":"bad request: …"}` |
| `monitor` without `reflector`, `module` or `callsign` | `400` | `{"error":"bad request: monitor needs reflector, module and callsign"}` |
| `module` is not exactly one character | `400` | `{"error":"bad request: module is one letter, A to Z"}` |
| `disconnect` with no monitor up | `500` | `{"error":"No M17 link is up."}` |
| The node refuses the dial | `500` | `{"error":"<one sentence>"}`, from the table below |
| `monitor` accepted | `200` | `{"ok":true,"relays_to":{"allstar":3,"echolink":1},"warning":"The node never sends voice to an M17 reflector, … Right now that is 3 AllStar connections and 1 EchoLink station, and any of them with a radio will transmit what this reflector carries."}` |
| `disconnect` accepted | `200` | `{"ok":true}` |

The node's refusals, in the order it checks them:

| Why | `error` |
|---|---|
| No `[m17]` section, or `enabled = false` | ``M17 isn't enabled on this node — its operator can add an `[m17]` section.`` |
| A bad reflector name | ``A reflector name is letters, digits and '-' only.`` or ``A reflector name is 3 to 8 characters.`` |
| A bad module | ``A module is a single letter, A to Z.`` |
| A bad callsign | ``"AJ7 HR" isn't an M17 callsign — use 1 to 9 letters, digits, '-', '/' or '.'.`` |
| An unknown reflector | ``I don't know a reflector called M17-XYZ. The node's operator can add it under `[m17.reflectors]`.`` |
| A module the directory does not list | ``M17-ABC has modules A–D.`` (``has module A.`` for a single one) |
| No Codec 2 decoder | ``this node has no Codec 2 decoder — its operator needs to install libcodec2 (or build astar-server with codec2-static).`` |
| A monitor is already up | ``Already listening to M17 M17-USA C. `/hangup` first.`` |
| The bridge is in handset or parrot mode | ``m17 error: no conference: bridge topology is handset/parrot`` |

An unknown reflector is refused before any socket opens.

### Status and events

`GET /status` carries the monitor as `m17` (other fields elided):

```json
{
  "m17": {"reflector":"M17-USA","module":"C","target":"M17-USA C","callsign":"AJ7HR","state":"linked","receiving":false,"talker":"KC1ABC","talker_secs_ago":3,"up_secs":125,"dropped_packets":0,"directory_generated":"2026-09-15"},
  "m17_enabled": true,
  "m17_available": true,
  "m17_relay": {"relays_to":{"allstar":3,"echolink":1},"warning":"The node never sends voice to an M17 reflector, …"}
}
```

| Field | Meaning |
|---|---|
| `reflector`, `module`, `target` | What is linked; `target` is the two together. |
| `callsign` | The identity presented to the reflector. |
| `state` | `linking`, `linked` or `failed`. |
| `receiving` | A voice packet arrived in the last 400 ms. |
| `talker`, `talker_secs_ago` | The last station heard. The text comes from the reflector and is not verified; show it as text. |
| `up_secs` | Seconds since the reflector accepted the link; `null` while linking. |
| `dropped_packets` | Voice packets dropped because a second stream overlapped, or a stream ran faster than real time. |
| `directory_generated` | The date of the reflector directory in use; `null` when there is none. |

On `GET /events`, each edge is followed by a `snapshot`:

```json
{"event":"m17","kind":"linking","reflector":"M17-USA","module":"C"}
{"event":"m17","kind":"connected","reflector":"M17-USA","module":"C"}
{"event":"m17","kind":"disconnected","reflector":"M17-USA","module":"C","reason":"reflector refused the link"}
```

`reason` is one of `reflector refused the link`, `reflector sent DISC`,
`no reply for 30 s`, `could not resolve <host>: <error>`, `no Codec 2 decoder`,
`conference gone`, `monitor stopped`, `hung up` (a `disconnect`) or
`node shutting down`.

## EchoLink stations

### `POST /echolink`

Calls an EchoLink station into the conference, or hangs up on one. The
station joins as a full member: it hears the conference minus itself, and the
conference hears it. It is off unless `node.toml` has an `[echolink]`
section, and the node has to be logged in to EchoLink's directory first — see
[EchoLink](echolink.md).

```json
{ "action": "dial", "target": "9999" }
{ "action": "disconnect", "target": "W1AW" }
```

| `action` | Meaning |
|---|---|
| `dial` | Call the station `target` names: a node number (`9999`) or a callsign (`*ECHOTEST*`, in any case). |
| `disconnect` | Hang up on the station whose callsign is `target`. With no `target` at all, hang up on every EchoLink station on the node. The login stays up either way. |

* **It is an admin route.** The console and signed-in admins may call it; a
  listener may not.
* **It goes through the same-origin JSON check** (`same_origin_json`): it
  needs `Content-Type: application/json` of every caller, `curl` included,
  and an `Origin`, when present, that matches `Host`.
* **There is no address field.** A station is named by node number or
  callsign, and only a station the directory lists can be dialled; its
  address comes from the directory. A body carrying `addr`, `host`, or any
  other unknown field is a `400`.
* `target` is trimmed, and must be 1 to 32 characters with no spaces.
* `{"ok":true}` means the call has **started**, not that the station
  answered. Watch `echolink.peers` in `GET /status`, or the `snapshot`
  events, for its `state` to become `connected`.
* Several stations can be connected at once. While any is, `POST /key` is
  refused (see [PTT](#ptt)).

```bash
curl -X POST http://127.0.0.1:8730/echolink \
     -H 'Content-Type: application/json' \
     -d '{"action":"dial","target":"9999"}'
curl -X POST http://127.0.0.1:8730/echolink \
     -H 'Content-Type: application/json' \
     -d '{"action":"disconnect","target":"*ECHOTEST*"}'
```

| Case | Status | Body |
|---|---|---|
| No `Content-Type: application/json` | `415` | `{"error":"POST /echolink requires Content-Type: application/json"}` |
| An `Origin` that does not match `Host` | `403` | `{"error":"cross-origin requests are not accepted on this port"}` |
| Not JSON, an unknown `action`, or an unknown field such as `addr` | `400` | `{"error":"bad request: …"}` |
| The node refuses | `500` | `{"error":"<one sentence>"}`, from the table below |
| Success | `200` | `{"ok":true}` |

The node's refusals, in the order a dial checks them:

| Why | `error` |
|---|---|
| No `[echolink]` section | ``EchoLink isn't enabled on this node — its operator can add an [echolink] section.`` |
| A missing or badly shaped `target` | ``A dial needs a node number or callsign.`` |
| The node is not logged in | ``EchoLink isn't logged in yet: <reason>``, where the reason is, for example, ``no password: set the echolink:KX7ABC-L secret``, ``still logging in`` or the directory's refusal |
| The station list in hand does not have the station (the node asks the directory for a fresh list as it refuses, so a station that has just logged in is there on the next try) | ``Not in the station list yet — refreshing…`` |
| The station is already on the node | ``EchoLink station 9999 is already connected to this node.`` |
| The bridge is in handset or parrot mode | ``echolink error: no conference: bridge topology is handset/parrot`` |

And for a disconnect:

| Why | `error` |
|---|---|
| No `[echolink]` section | ``EchoLink isn't enabled on this node — its operator can add an [echolink] section.`` |
| A `target` that is present but blank or badly shaped | ``A disconnect needs a callsign; leave the target out to hang up on everyone.`` |
| No such station on the node | ``No EchoLink station W1AW is connected to this node.`` |
| No `target`, and nobody connected | ``No EchoLink station is connected to this node.`` |

A blank `target` is refused rather than read as "everyone", so an empty form
field can never hang up the whole leg.

### EchoLink in `GET /status`

`GET /status` carries the leg as `echolink` (other fields elided):

```json
{
  "echolink": {"login":"logged_in","callsign":"KX7ABC-L","error":null,"peers":[
    {"call":"*ECHOTEST*","node":9999,"state":"connected","talking":false,"up_secs":42,"notice":null}
  ]},
  "echolink_enabled": true
}
```

| Field | Meaning |
|---|---|
| `login` | `logged_out`, `logging_in`, `logged_in` or `failed`. |
| `callsign` | The node's EchoLink callsign. |
| `error` | Why the node is not logged in, for a person to read; `null` when nothing is wrong. |
| `peers` | Every station on the node, in the order they were dialled. |
| `call`, `node` | The station's callsign and node number, as the directory lists them. The callsign comes from the directory and is not verified; show it as text. |
| `state` | `connecting`, `connected`, or briefly `closed` before the row goes away. |
| `talking` | The station is sending audio. |
| `up_secs` | Seconds since the station answered; `null` while connecting. |
| `notice` | Why the node stopped sending the net to this station, for a person to read: *Stopped sending the net to \*ECHOTEST\* after 10 minutes of continuous talk; it resumes after the next pause.* `null` otherwise. It carries the directory's callsign, so show it as text. |

The password is never in it. There is no separate EchoLink event: each
change arrives as a `snapshot` on `GET /events`.

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
    type changes later. A body that is not that shape — a secret sent as a
    JSON number, say — gets one fixed sentence,
    `400 {"error":"bad request: expected {\"username\": \"…\", \"secret\": \"…\"}"}`,
    which quotes nothing from the body. Nothing about this request is logged.

## Settings

`GET /settings` serves the settings page (with `/settings.js`); the routes
under it are what the page calls. Every `POST` here needs
`Content-Type: application/json` and a loopback or bound-address `Host` (see
[Conventions](#conventions)); no route sends an `Access-Control-*` header. On
a controller with no settings store — a test harness or an embedder that
never installed one — `GET /settings/state` and the four `POST` routes each answer
`503 {"error":"settings are not available on this node"}`; the page and its
script are still served.

### `GET /settings/state`

```json
{
  "disk":     { "listener.bind": "0.0.0.0:4569", "bridge.mode": "bridge", "links.\"2000\".shape": "wt-guest", "credentials.source": "file", "wireguard.key_ref": "WIREGUARD_PRIVATE_KEY" },
  "running":  { "listener.bind": "0.0.0.0:4570", "bridge.mode": "conference", "links.\"2000\".shape": "wt-guest", "credentials.source": "file", "wireguard.key_ref": "WIREGUARD_PRIVATE_KEY" },
  "pending":  { "restart": ["listener.bind"], "apply": ["bridge.mode"] },
  "live":     ["bridge.", "parrot.", "audio.input", "audio.output", "m17.enabled", "m17.reflectors"],
  "credentials": {
    "node": "set", "portal": "unset", "discord": "set",
    "links": { "2000": "set" },
    "peers": { "allstar-public": "set" }
  },
  "credential_source": "file",
  "credential_file": "/etc/iaxnode/credentials.toml",
  "etag": "9f3a1c0b7e2d4455",
  "file": { "path": "/etc/iaxnode/node.toml", "writable": true },
  "restart": { "busy": false, "why": null },
  "node_id": "12345",
  "registered": true,
  "feature": { "discord": true }
}
```

| Field | Meaning |
|---|---|
| `disk` / `running` | The file as it is now, and the configuration the daemon is running, both as flat maps of dotted path → value. `[secrets]` appears as `credentials.source` / `credentials.file`; `[wireguard] secret_ref` appears as `wireguard.key_ref`. Absent keys are absent. |
| `pending` | Every path that differs between the two, split by whether saving it applies live (`apply`) or waits for a restart (`restart`). In the example the listener was rebound on disk and the bridge mode was changed by hand. |
| `live` | The path prefixes that apply live. Everything else is restart-tier. |
| `credentials` | Presence only — every value is the string `"set"` or `"unset"`, never the material. |
| `etag` | Send it back with `POST /settings`; a mismatch means the file changed underneath the page. |
| `file.writable` | Whether the daemon can write to `node.toml`'s directory. |
| `restart.busy` / `restart.why` | Whether a restart would drop something, and what (`"2 calls, 1 link"`). |

No key or value on this route contains `secret`, `password` or `token`; the
same test that pins `GET /status` secret-free pins this one.

### `POST /settings`

```json
{"etag":"9f3a1c0b7e2d4455","patch":{"bridge.mode":"conference","listener.bind":"0.0.0.0:4571"},"acknowledge":[]}
```

`etag` is the value from the last `GET /settings/state` or the last save.
`patch` maps dotted paths to new values; `null` removes an optional key, or
a whole optional table (`"register": null`). Paths outside the page's
vocabulary are refused (`unknown setting "<path>"`); `wireguard.*` is
read-only. `acknowledge` lists the confirmation keys an earlier reply asked
for.

```json
{"ok":true,"applied":["bridge.mode"],"on_restart":["listener.bind"],"etag":"1c0b7e2d44559f3a"}
```

`applied` was changed in the running daemon; `on_restart` was written and
waits. When a live apply fails after the file was written, the reply adds
`"apply_error":"<message>"` and those paths stay in `pending.apply`. A patch
whose value already matches the file but whose path is in `pending.apply`
(the page's Apply on a hand-edited live field) is applied without rewriting
the file; the etag comes back unchanged.

| Status | Body | When |
|---|---|---|
| `400` | `{"error":"bad request: <detail>"}` | the body is not a settings patch |
| `400` | `{"error":"<sentence>","field":"<path>"}` | a per-field rule failed; `"acknowledge":"<key>"` is added when a confirmation would let it through |
| `400` | `{"error":"<message>"}` | the patched file would not load; nothing was written |
| `409` | `{"error":"node.toml changed since the page loaded","conflict":true}` | the file changed since `etag` was read; nothing was written |
| `500` | `{"error":"cannot write <path>: <io error>"}` | the directory is not writable |

The confirmation keys are `control.bind.public`, `listener.auth.off`,
`dtmf.enabled`, `bridge.include_local_radio`, `announce.cw_keys_when_idle`,
`discord.mic_timeout.off`, `discord.approvers.remove`, `m17.codec_dirs` and
`announce.tts.binary` (which, on this route, must also be the absolute path of
an existing file). Two
things no acknowledgement unlocks: a wildcard `control.bind` (`control.bind: a wildcard
bind (0.0.0.0 or ::) is refused — it exposes an unauthenticated keying port on
every interface`), and changing `control.bind` at all from a request that did
not come from the node itself (`control.bind: may only be changed by a request
from this host (a loopback peer)`). That is decided by the connection's peer
address, not by its `Host` header, which the caller chooses.

### `POST /settings/credential`

```json
{"name":"node","value":"<password>"}
```

`name` is `node`, `portal`, `discord`, `link:<node>` or `peer:<username>`
(`peer:node`, `peer:portal` and `peer:discord` are refused, as is a peer
named after this node's own number). `{"name":"discord","forget":true}`
removes one. `"migrate":true` on a node whose source is not `file` first
moves every credential the running node holds into `credentials.toml` and
switches the source. The file is written before the running daemon is told,
so a crash between the two leaves a credential that survives the restart.

!!! info "The reply is always the bare `{"ok":true}`"

    Errors name the credential and never the value:
    `{"error":"unknown credential \"x\""}`,
    `{"error":"credential \"node\" needs a value"}` (both `400`), or
    `{"error":"credential \"node\" could not be saved: cannot write <path>: <io error>"}`
    and `{"error":"credential \"node\" could not be removed: <detail>"}`
    (`500`). Nothing about this request is logged.

### `POST /settings/portal/test`

Body `{}`. Mints and discards one web-transceiver token with the portal
credentials the daemon is **running** with — no call, no UDP. `{"ok":true}`,
or `409 {"error":"portal/token error: …"}`. A portal password saved on the
page takes effect at the next restart, so the flow is *set → restart → test*.

### `POST /settings/restart`

Body `{}` or `{"disconnect":true}`. Restarts the daemon in place by
re-executing the binary with its original arguments; the PID is preserved.
Refused with `409 {"error":"restart refused: this node is on a net (2 calls, 1 link)","busy":true}`
while calls or links are up or the Discord bridge is joined, unless
`disconnect` is `true`. `{"ok":true}` is sent before the port closes; poll
`/status` to see it come back.

## Lifecycle

### `POST /shutdown`

Stops the daemon cleanly. `SIGINT` and `SIGTERM` do the same thing.

## Everything else

Any other path is a `404` with `{"error":"not found"}`.

## Who may call what

On the console (a loopback peer, by default) everything is allowed. A
signed-in admin may call every route; these also need a sign-in method used
within `[web] step_up_minutes`, or they answer
`401 {"error":"reauth_required"}`:

| Route | Why |
|---|---|
| `POST /secrets` | sets a credential |
| `POST /settings`, `POST /settings/credential` | rewrites the configuration or a credential |
| `POST /settings/restart`, `POST /shutdown` | stops the node |

A listener (viewer) may call none of them. `GET /style.css`,
`GET /auth.css`, `GET /auth.js` and `GET /favicon.ico` need nothing.
