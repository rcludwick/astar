---
icon: lucide/settings-2
---

# Configuration

`astar-server` reads one TOML file, conventionally `node.toml`. Start from the
template in the repository:

```bash
cp deploy/node.toml.example node.toml
```

!!! danger "This file is secret-free by design"

    No passwords, no tokens, ever. `[secrets]` declares *where* the credential
    comes from; the credential itself lives in an environment variable or is
    POSTed to the loopback control port at runtime. See
    [Secrets](#secrets) below.

Two sections are required — `[listener]` and `[control]` — plus `[secrets]` if
you want the node to authenticate anywhere. Everything else is optional.

## A minimal config

`bind = "0.0.0.0:4569"` is an **internet-reachable** socket once UDP 4569 is
forwarded, on a daemon that can bridge a local radio and key a transmitter.
Start closed:

```toml
[listener]
bind          = "0.0.0.0:4569"
answer        = "auto"
max_calls     = 2
auth          = "required"                  # (1)!
allowed_nodes = ["<node id allowed to call you>"]   # (2)!

[control]
bind = "127.0.0.1:8730"

[secrets]
source = "env"
```

1.  Every inbound caller is MD5-challenged against the secret you provisioned
    for that node id — see [Secrets](#secrets) (`ALLSTAR_PEER_<NODE>`). An
    unknown username is rejected.
2.  A second, independent gate: a caller whose node id is not on the list is
    rejected at call setup, before answer. Fill it in — a placeholder left in
    place admits nobody, which is the safe way to fail.

!!! danger "`auth = "off"` with no allowlist is an open node"

    That combination — which is what an omitted `allowed_nodes` plus
    `auth = "off"` gives you — admits **every caller on the internet**, subject
    only to `max_calls`. It is a deliberate opt-in for a closed lab network or a
    loopback bind, never a starting point:

    ```toml
    [listener]
    bind = "127.0.0.1:4569"   # loopback only
    auth = "off"
    ```

    If you want an open node on a public bind, that is your call.

## `[listener]` — the inbound socket

| Key | Values | Meaning |
|---|---|---|
| `bind` | `"0.0.0.0:4569"` | Where the IAX2 socket listens. UDP 4569 must be forwarded from the internet for inbound calls to land. |
| `answer` | `"auto"` \| `"manual"` | Answer inbound calls automatically, or wait for `POST /answer`. |
| `max_calls` | integer | Cap on simultaneous inbound calls. |
| `auth` | `"required"` \| `"optional"` \| `"off"` | Authentication policy for **inbound** callers. |
| `allowed_nodes` | list of node ids | Optional allowlist. **Omitted or empty means every caller is admitted**, subject to `auth` and `max_calls`. A caller not on a non-empty list is rejected at call setup, before answer. Set it unless you mean to run an open node. |

## `[register]` — register with a registrar

Optional. Omit the whole section if the node should not register.

```toml
[register]
peer    = "<registrar-ip>:4569"
node_id = "<your node number>"
```

`peer` is the registrar's address; take the host from your existing `rpt.conf`
`register => <node>:<pw>@<HOST>` line. `node_id` is your node number and is also
the register username. **The password is not here** — see [Secrets](#secrets).

## `[control]` — the control channel

```toml
[control]
bind = "127.0.0.1:8730"
```

Keep it on loopback. This is where commands and secrets are POSTed, and it can
key the transmitter. See [Control API](control-api.md).

## `[bridge]` — audio topology

Absent means `mode = "bridge"`.

| Key | Default | Meaning |
|---|---|---|
| `mode` | `"bridge"` | `"handset"` (1:1 with the local radio), `"bridge"` (pure conference, local radio off), or `"conference"` (the same mix-minus engine). |
| `mix_minus` | `true` | Each member hears everyone but itself. `false` gives a full mix, so members hear themselves — useful for loopback testing. |
| `include_local_radio` | `false` | Add the local microphone as a conference source and feed the local speaker the sum of all members. |

The topology can be re-wired live with `POST /bridge`.

## `[audio]` — device selection

Optional; omit for system defaults.

```toml
[audio]
input   = "USB"     # substring match on the device name
output  = "USB"
backend = "cpal"    # or "none" for a headless host with no audio devices
```

`backend = "none"` selects a hardware-free null backend, which is what a
container or a headless VPS wants.

## `codec_policy` — codec negotiation

A top-level key, not a section:

```toml
codec_policy = "prefer_slin"
```

`"ulaw_only"` (the default), `"allow_slin"`, or `"prefer_slin"`. `prefer_slin`
negotiates 16-bit linear audio with peers that permit it (ASL3: `allow = slin`)
and falls back to µ-law.

## `[announce]` — voice and CW announcements

Optional. Covers a periodic station ID and per-event announcements.

```toml
[announce]
enabled          = true
id_mode          = "cw"      # "cw" (Morse), "tts", or "off"
id_interval_secs = 600       # 0 or omitted disables the periodic ID
cw_wpm           = 20
cw_tone_hz       = 800.0
cw_keys_when_idle = true

[announce.tts]
enabled    = true
binary     = "piper"
voice      = "/path/to/voice.onnx"
timeout_ms = 4000
gain_db    = -6.0            # negative attenuates a voice that renders hot

[announce.events.incoming_call]
enabled     = true
destination = "to_air"       # "to_air" or "to_monitor"
```

Event names are `incoming_call`, `hangup`, `registered`, `register_failed` and
`answered`.

!!! warning "`cw_keys_when_idle` transmits"

    A periodic station ID that keys the transmitter is still a transmission.
    Configure it as the licensed operator responsible for the station, and know
    what your node is connected to.

## `[dtmf]` — DTMF command execution

Off by default, deliberately.

```toml
[dtmf]
enabled                = true
inter_digit_timeout_ms = 3000
```

!!! warning "Enabling this lets any connected member command your links"

    DTMF `*` sequences map to link commands (`*1`/`*2`/`*3`). Turn it on only if
    you are comfortable with everyone on the conference being able to connect
    and disconnect links.

## `[links]` — per-target dial profiles

```toml
[links."55553"]
shape = "standard"    # or "wt-guest"
```

`"wt-guest"` is the AllStarLink web-transceiver guest shape, needed for app
nodes whose guest context only exposes the WT extension. It requires a
`[portal]` section.

## `[portal]` — AllStarLink portal account

Only needed for `wt-guest` link dials, which require a freshly minted
web-transceiver token.

```toml
[portal]
user           = "<your portal callsign>"
node           = "<a node the account owns>"
credential_env = "ALLSTAR_PORTAL_PASS"
```

`credential_env` is the **name of an environment variable**, not the password.
The value is resolved once at startup and never logged.

## `[wireguard]` — link transport

Optional. Absent means plain UDP. When present, the link runs over a userspace
WireGuard tunnel.

## `[parrot]` — parrot tuning

Only consulted when the bridge is in parrot mode; harmless otherwise.

```toml
[parrot]
playback_delay_ms = 3000
silence_gap_ms    = 800
vox_threshold_db  = -40.0
```

## Secrets

The `[secrets]` section declares the **source**, never the material.

```toml
[secrets]
source = "env"      # "env" | "control" | "config"
```

=== "`env` (recommended)"

    The daemon reads two environment variables at startup:

    ```bash
    export ALLSTAR_NODE=<your node number>    # must match [register].node_id
    export ALLSTAR_SECRET=<your node password>
    astar-server serve --config node.toml
    ```

=== "`control`"

    Nothing is read at startup; you POST the credential to the loopback control
    port after the daemon is up:

    ```bash
    curl -X POST http://127.0.0.1:8730/secrets \
         -d '{"username":"<node>","secret":"<password>"}'
    ```

    The reply is a bare `ok` — the secret has no path back out.

=== "`config`"

    An inline `secret` key, for a deployment where the operator mounts a
    private `node.toml` (for example `/etc/iaxnode/node.toml`) with restricted
    permissions. **Repo-tracked templates ship this empty**, and it should stay
    that way. If you can avoid it, avoid it.

!!! danger "Never commit, paste or screenshot a node secret"

    The daemon goes to some length to keep credentials out of `Debug` output,
    logs, snapshots, events and error messages. That effort is wasted the moment
    a password lands in a config file you push, or in a bug report.
