---
icon: lucide/settings-2
---

# Configuration

`astar-server` reads one TOML file, conventionally `node.toml`. Start from the
template in the repository:

```bash
cp deploy/node.toml.example node.toml
```

!!! danger "Keep credentials out of this file unless it is private"

    `[secrets]` declares *where* the credentials come from. By default they
    live in `credentials.toml` beside this file (mode `0600`, written by the
    [settings page](#the-settings-page)), in environment variables, or are
    POSTed to the loopback control port at runtime. The one exception is
    `source = "config"`, for a one-file deploy: the credentials sit under
    `[secrets]` in this file, which must then be mode `0600` — the node warns
    at start-up when it is not. See [Secrets](#secrets) below.

Two sections are required — `[listener]` and `[control]` — plus `[secrets]`,
which says where the node's credentials live. Everything else is optional.

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
source = "file"
```

1.  Every inbound caller is MD5-challenged against the secret you provisioned
    for that node id — a bare `"<NODE>"` key in `credentials.toml`, or
    `ALLSTAR_PEER_<NODE>` under `source = "env"`; see [Secrets](#secrets).
    An unknown username is rejected.
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

A wildcard `bind` (`0.0.0.0` or `[::]`) is a start-up error: it would put the
keying port on every interface. A container whose published port arrives on
its own network interface rather than its loopback — the nodebench rig, the
smoke-test image — sets `allow_wildcard_bind = true` alongside it. That key is
for sealed containers only; the settings page never offers it, and never
accepts a wildcard bind either.

`loopback_login` (default `false`): whether a peer at a loopback address must
sign in too. By default it is the built-in console admin; see
[Signing in](admin.md#settings).

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

A top-level key, not a section, and **`"prefer_slin16"` is the default** — omit
it entirely and the node negotiates wideband:

```toml
codec_policy = "prefer_slin16"
```

| Value | Accepts on a call it answers, best first | Offers on a call it places |
|---|---|---|
| `"ulaw_only"` | µ-law, A-law, GSM — the pre-slin wire behaviour | µ-law, A-law |
| `"allow_slin"` | µ-law, A-law, slin (slin only if the peer asks for it), GSM | µ-law, A-law, slin |
| `"prefer_slin"` | slin, µ-law, A-law, GSM | slin, µ-law, A-law |
| `"prefer_slin16"` | slin16, slin, µ-law, A-law, GSM — **the default** | slin16, slin, µ-law, A-law |

`prefer_slin` negotiates 8 kHz 16-bit linear audio (~128 kbps) with peers that
permit it (ASL3: `allow = slin`) and falls back to µ-law. `prefer_slin16` adds
16 kHz wideband linear (~256 kbps) above that, and switches the station's own
audio pipeline — capture, playback and mixing — to 16 kHz.

**A µ-law-only node is still answered in µ-law.** Negotiation honours whatever
FORMAT a caller asks for whenever the node can carry it, so a ClearNode or an
ASL3 peer that only speaks µ-law gets µ-law on a `prefer_slin16` node, and it
can share a conference with a wideband client — each leg resamples at its own
codec edge. A caller that asks for µ-law gets µ-law even if it lists slin16 in
its CAPABILITY — the peer's stated FORMAT wins when it is in the common set,
because listing a codec says the peer *can* transcode it, not that it wants it
on this link. What the default changes is the caller that actually *asks* for
slin16 (the astar clients always do): it is answered in slin16 instead of being
quietly downgraded.

Set `codec_policy = "ulaw_only"` to pin the old behaviour — worth doing on a
bandwidth-constrained link, since each wideband leg costs roughly twice a slin
leg and eight times a µ-law one.

**GSM is accepted on calls the node answers, and never offered on calls it
places.** GSM 06.10 is the narrowband codec some older AllStar nodes are set
to use alone (`disallow=all / allow=gsm`). When such a node dials in, every
policy accepts GSM after everything else, so it is the node's own last
choice, and the caller is answered in GSM rather than turned away; the
FORMAT rule above still applies, so a caller that asks for GSM gets GSM.
When this node dials out, no policy offers GSM, not even `"ulaw_only"`, so
a node it calls can never pick GSM over a better codec both ends have. The
catch is that a GSM-only node cannot be dialled from here; it has to dial
in. The node
expects one 20 ms GSM frame (33 bytes) in each voice packet; a peer that packs
two frames into one packet is not understood yet, and those packets are
dropped with a warning.

A peer with *no* codec in common with the policy is rejected at call setup with
CAUSE `Unable to negotiate codec` and CAUSECODE 65, the same as Asterisk. astar
implements µ-law, A-law, slin, slin16 and GSM and no others, so a peer that
offers only, say, G.729 lands here. The same applies in the other
direction: if a node this server dials answers with a format the policy never
offered, the link is hung up with that cause rather than transmitting outside
the operator's cap.

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

`[announce.tts] binary` is a program the node runs, so the settings page
treats it like `[m17] codec_dirs`: it accepts only the absolute path of a file
that exists (a bare name such as `"piper"`, found through the daemon's `PATH`,
can be written only in the file itself), and setting it to a new program asks
first. Clearing it on the page goes back to the default, `"piper"`.

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

Under `[secrets] source = "file"` the password is the `portal` key in
`credentials.toml` and `credential_env` is not read.

## `[wireguard]` — link transport

Optional. Absent means plain UDP. When present, the link runs over a userspace
WireGuard tunnel.

## `[m17]` — receive-only M17 reflector monitor

Optional, and **off unless present**. With it, `/dial m17` from
[Discord](discord.md) and [`POST /m17`](control-api.md) can link the node to
one module of an M17 reflector as a **receive-only monitor**. The reflector's
audio is mixed into the conference, and the only packets the node ever sends
the reflector are the ones that hold a link open: connect, the keepalive reply
and disconnect.

```toml
[m17]                    # OPTIONAL. Absent = M17 off. Receive-only: no key here can transmit.
enabled   = true         # default true when the section exists
directory = "bundled"    # "bundled" | an absolute path to a JSON file | omit (table only)
codec_dirs = []          # extra libcodec2 directories, default empty — restart-tier, code-loading
[m17.reflectors]         # default empty. "NAME" = "host[:port]", port default 17000
"M17-XYZ" = "m17.example.net:17000"
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | `false` keeps the section and refuses every dial. |
| `directory` | absent | Where reflector names are looked up after the table. `"bundled"` is the snapshot compiled into this build; an absolute path is a JSON file in the same format. Absent means only the names in `[m17.reflectors]` can be dialled. |
| `codec_dirs` | `[]` | Extra directories searched for `libcodec2` before the defaults. It decides where the node loads code from, so it is read once at start-up. |
| `[m17.reflectors]` | empty | Your own names: `"NAME" = "host[:port]"`, with port `17000` when omitted. Checked first, so an entry here overrides the directory. A name is 3 to 8 letters, digits and `-`, and two names that differ only in case are the same name. IPv6 literals are refused. Entries here carry no module list, so any module letter is allowed. |

A bad entry in `[m17.reflectors]` is a start-up error. So is a `directory` that
is neither `"bundled"` nor an absolute path, and a `codec_dirs` entry that is
not an absolute path. A directory **file** that is missing or does not parse is
not: the node logs one line and carries on with the table alone.

**On the settings page.** The **M17** card (`/settings#m17`) edits this
section. The switch and `[m17.reflectors]` apply at the next dial, with no
restart; a monitor that is already up stays up until it is hung up.
`directory` and `codec_dirs` wait for a restart, and adding a `codec_dirs`
entry asks first, because it decides which library the node loads and runs.
The section is the opt-in: a save that would create it has to say whether
M17 is on, so adding a reflector on a node without one leaves M17 off until
the switch is turned on.

!!! danger "No key here can transmit"

    The section has no `callsign`, `transmit`, `ptt`, `contributes`, `arm`,
    `secret`, `token` or `password` key, and `deny_unknown_fields` turns an
    invented one into a start-up error. The callsign is not a setting: a
    Discord dial presents the dialler's registered callsign, and `POST /m17`
    must carry one in its body.

**Codec 2.** Decoding M17 audio needs the LGPL `libcodec2` library, which
`astar-server` loads at run time rather than linking. The node's container
image installs Debian's package; anywhere else, install it or point
`codec_dirs` at it. Without it every dial is refused up front, and the status
page shows an **M17: no codec** badge.

**The bundled directory** is a dated snapshot of the M17 rows of the
[hamcall-db](../about/reflector-data.md) reflector list, compiled into the
binary. The node never fetches the list itself. `GET /status` reports the
snapshot's date as `m17.directory_generated` while a monitor is up, and
`ci/fetch-m17-reflectors.sh` regenerates it. Each row is reduced to its name,
host, port and module list. The data is licensed CC BY 4.0:

> Reflector data provided by DVRef — <https://dvref.com/>

**What it does not do yet.** There is no jitter or reorder buffer, so audio
over a poor path sounds choppy. There is no automatic reconnect: a failed link
is torn down and reported once, with the reason. A restart of the node ends the
monitor. One monitor runs at a time.

## `[vocoder]` — trusted AMBE vocoder sticks

Optional. Lists the AMBE vocoder sticks (DVstick, ThumbDV, DV3000) this node
may open, by **USB serial number**. Absent or empty means none: the node opens
no vocoder port at all, and never scans for one.

```toml
[vocoder]                          # OPTIONAL. Absent = no vocoder is ever opened.
trusted_serials = ["TESTSER1"]     # USB serial numbers, exactly as `vocoder list` shows them
```

| Key | Default | Meaning |
|---|---|---|
| `trusted_serials` | `[]` | The serials of the sticks the node may open. Each is 1 to 64 printable ASCII characters with no spaces or commas, written exactly. Repeats are ignored. Read at start-up. |

A malformed serial is a start-up error, and so is any other key in the
section: there is deliberately no `port`, `scan` or `any` key, because nothing
may widen the list beyond the serials written down.

!!! danger "Why the node will not just find your stick"

    A DVstick and a radio's USB CAT/PTT cable often use the same FTDI chip,
    and they look identical on USB (`0403:6015`). Opening a serial port
    raises its DTR and RTS lines, and on a cable wired to a radio that keys
    the transmitter. So the node opens only the serials you have listed
    here. `IAX_THUMBDV_PORT` can narrow the choice to one of them, but it
    cannot add a port that is not listed.

**Adding a stick.** Run these on the node, with the stick plugged in:

```bash
astar-server vocoder list --config node.toml
astar-server vocoder add <serial> --config node.toml
```

`vocoder list` shows each vocoder-class USB device with its serial, its port
and whether it is trusted, and any trusted serial that is not plugged in. It
opens nothing.

`vocoder add` first asks you to confirm that the device is not a radio's
CAT/PTT cable. Type `yes`, or pass `--yes` in a script. It then opens that one
port with DTR and RTS held low, sends the AMBE reset and product-ID requests
with a short timeout, and closes it. The serial is written to `node.toml`
only when an AMBE-3000 answers:

| Reply | Result |
|---|---|
| `AMBE3000…` (DVstick 30, ThumbDV-class) | Added to `trusted_serials`. Restart the node to use it. |
| `AMBE3003…` (DVstick 33) | Recognised but not supported yet, so it is **not** added. |
| Anything else, or no reply | Refused, with the reason. It may be a cable. |

A serial that is already trusted is reported as such, and nothing is opened.
The write goes through the same path as the settings page, so comments and
every other key in `node.toml` are kept.

`vocoder list` and `vocoder add` need the vocoder driver, which is the
`vocoder` cargo feature (`cargo build -p astar-server --features vocoder`).
It is off by default because on Linux it needs `libudev`, and the published
container image is built without it. Without it, `list` finds nothing and
`add` answers "This build has no hardware vocoder support".

**On the settings page.** The **Vocoders** card (`/settings#vocoder`) shows
the list, and you can remove a stick from it. You can also type a serial in,
but the page cannot probe it, so it asks you to confirm first. Use
`vocoder add` where you can. Changes wait for a restart.

**What uses it today.** `astar-server` has no D-Star leg of its own yet, so
today the list only decides what the node *may* open. The station reads it at
start-up, so any vocoder use that comes later is limited to these sticks.

## `[discord]` — a Discord voice channel as a conference member

Optional, and behind the `discord` cargo feature. Mirrors this node's
conference into a Discord voice channel, **listen-only**, and — once
`approvers` is filled in — adds slash commands, an approvals system, a shared
mic and an admin page.

```toml
[discord]
guild_id   = "123456789012345678"
channel_id = "123456789012345678"
approvers  = ["123456789012345678"]
state_file = "/var/lib/astar/discord-approvals.json"
```

The bot token is **not** a key here: set it on the
[settings page](#the-settings-page) (Discord card), or under the other
sources as `DISCORD_BOT_TOKEN` in the environment or `POST /secrets` under
the username `discord`.

See **[Discord bridge](discord.md)** for the permissions to invite it with, the
four tiers, every command, the mic and its timers, and the limits of a
guild-wide server mute.

## `[echolink]` — EchoLink stations in the conference

Optional, and **off unless present**. With it, the node logs in to EchoLink's
directory at start-up, and an admin can call EchoLink stations into the
conference from the node page or with [`POST /echolink`](control-api.md#post-echolink).
Calling out only: the node never accepts an incoming EchoLink call.

```toml
[echolink]
callsign = "KX7ABC-L"    # required; must end in -L (a link) or -R (a repeater)
servers  = ["servers.echolink.org", "naeast.echolink.org",
            "nasouth.echolink.org", "backup.echolink.org"]   # the default
location = "Anytown, ST" # shown beside the callsign in the directory
```

The password is **not** a key here: it is the secret `echolink:<CALL>`, set
with `ASTAR_ECHOLINK_PASSWORD`, a line in `credentials.toml`, or
`POST /secrets`, and it never appears on the settings page.

**It refuses to load beside `[discord]`, `[web] public_bind` or
`[stream] enabled = true`**, at start-up and on every settings-page save,
because EchoLink's access policy forbids linking it to systems people reach
directly from a computer.

See **[EchoLink](echolink.md)** for each key, the password, the ports the
node needs, and the full reasoning behind that refusal.

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
source = "file"     # "file" | "env" | "control" | "config"
```

=== "`file` (recommended)"

    Every credential — the registration password, the portal password, the
    Discord bot token, per-node link secrets, per-peer inbound secrets — lives
    in `credentials.toml` beside `node.toml`, or at the path in
    `[secrets] file` (relative paths resolve against `node.toml`'s directory,
    never the working directory). The [settings page](#the-settings-page)
    writes it; you never have to.

    The settings page can move the file, but only to a file directly in
    `node.toml`'s directory or in the state directory (`$STATE_DIRECTORY`, or
    `state/` beside `node.toml`). It refuses a path with a `..` in it, a path
    whose directory does not exist, and any spelling of `node.toml` itself,
    symlinks included. A path you write into `node.toml` by hand is not
    restricted.

    ```toml
    # astar-server credentials — written by the settings page. Mode 0600.
    node = "…"            # registration password; mapped to [register].node_id at load
    portal = "…"          # allstarlink.org account password ([portal].user)
    discord = "…"         # bot token
    "link:2000" = "…"     # outbound dial secret for node 2000
    allstar-public = "…"  # bare username = inbound peer secret
    ```

    `node` is a named key, not your node number, so renumbering the node in
    the settings page never orphans its password. A bare username is the
    inbound peer secret for that caller — the home for names like
    `allstar-public`, which cannot be spelled as an environment variable.
    Under this source the `[portal]` password is the `portal` key;
    `credential_env` is not consulted.

    The file is created with mode `0600`, and **a file that group or others
    can read is not loaded.** The node still boots — a permissions slip must
    not become a crash loop under `--restart=always` — and logs one line:

    ```
    astar-server: credentials file /etc/iaxnode/credentials.toml is readable by group or others (mode 0o644); the node did not load it — run: chmod 600 /etc/iaxnode/credentials.toml
    ```

    Until the mode is fixed every credential on the settings page reads
    `not set`, which is the truth.

=== "`env`"

    The daemon reads the credentials from the environment at startup:

    ```bash
    export ALLSTAR_NODE=<your node number>    # must match [register].node_id
    export ALLSTAR_SECRET=<your node password>
    export ALLSTAR_PEER_55553=<that peer's inbound secret>   # one per peer node
    export ALLSTAR_LINK_55553=<the secret we present dialing 55553>
    export DISCORD_BOT_TOKEN=<bot token>
    astar-server serve --config node.toml
    ```

    The portal password is the variable named by `[portal] credential_env`.
    A credential saved on the settings page under this source applies until
    the next restart (except the portal password, which a node on this
    source never picks up from the page: the running node read it at
    startup, and the next start reads the environment variable again — move
    to `credentials.toml` first), and the page offers **Move credentials to
    credentials.toml**, which copies everything the running node can see —
    the portal password included — into the file and switches
    `source = "file"`.

=== "`control`"

    Nothing is read at startup; you POST the credential to the loopback control
    port after the daemon is up:

    ```bash
    curl -X POST http://127.0.0.1:8730/secrets \
         -d '{"username":"<node>","secret":"<password>"}'
    ```

    The reply is a bare `ok` — the secret has no path back out. The settings
    page's credential rows do the same thing with `POST /settings/credential`,
    and offer the same move to `credentials.toml`.

=== "`config`"

    An inline `secret` key, for a deployment where the operator mounts a
    private `node.toml` (for example `/etc/iaxnode/node.toml`) with restricted
    permissions. It covers the registration password and nothing else.
    **Repo-tracked templates ship this empty**, and it should stay that way.

    A node still on this source can be moved off it from the settings page:
    the first credential you save there offers **Move credentials to
    credentials.toml**, which imports the inline secret into the file, flips
    `source = "file"` and blanks `secret = ""` — in that order, so a crash
    between steps leaves a node that still authenticates.

!!! danger "Never commit, paste or screenshot a node secret"

    The daemon goes to some length to keep credentials out of `Debug` output,
    logs, snapshots, events and error messages. That effort is wasted the moment
    a password lands in a config file you push, or in a bug report.

## The settings page

`http://127.0.0.1:8730/settings` edits every section on this page
except `[wireguard]`, which it shows read-only, and holds every
credential — from a browser. Reach it the
way you reach the rest of the control port: on the node itself, or over an
SSH tunnel:

```bash
ssh -N -L 8730:127.0.0.1:8730 <user>@<node-host>
```

then open `http://localhost:8730/settings`.

**What it edits.** `node.toml`, in place: comments, ordering and any key the
page does not know about are preserved, so a hand-maintained file stays
yours. One card per section — Identity, Listener, Audio & bridge,
Announcements, Discord, Linking, M17, Vocoders, Advanced — each with its own **Save**, so a
half-edited Discord card is never swept up by saving Identity. Credentials
are write-only password fields beside a `set` / `not set` chip; nothing the
page ever receives is echoed back, and `GET /settings/state` reports
presence only.

**Live and restart.** Every field carries a tag. `live` — bridge mode,
mix-minus, local radio, parrot tuning, the audio devices, M17 on or off and
this node's own M17 reflectors, and every credential except the portal
password — takes effect when you save. The portal password is read at
startup; under `source = "file"` a saved one is used from the next restart,
and **Test the running credentials** then checks it. Under the other
sources, save it after **Move credentials to credentials.toml**. `restart` —
everything else: node number, registration, the listener, announcements,
DTMF, links, portal and Discord ids, the control bind, the codec policy, the
M17 directory and Codec 2 directories, the trusted vocoders — is written to the file and waits. The header counts what is waiting (`2 changes waiting for a
restart`) and a field whose running value differs shows that value in grey
underneath. A `live` field that differs on disk because someone edited the
file by hand shows a blue `apply` tag with an Apply button, not a false
`restart`.

**Restart node.** The button in the restart bar restarts the daemon in
place: the process re-executes itself with the same arguments and the same
PID, so `podman`, `systemd` and `just node` all see one continuous process.
It refuses while the node is on a net — calls up, links up, or the Discord
bridge joined — until you confirm that they may drop. If teardown wedges, a
30 s watchdog performs the restart anyway. The page then polls `/status` and
reports `Back`, or after a minute that the node did not come back: a config
that fails at startup exits non-zero, so read the log.

**The `file` badge.** The header's `file` badge turns red, with the path,
when the daemon cannot write to `node.toml`'s directory (an atomic write
needs to rename into it). The fix is to give the user the daemon runs as
write access to that directory. In a container that is the uid inside it —
1000 in the published image — so the host directory mounted as the config
directory must be writable by whichever host uid that maps to. The page
cannot do it for you.

**A templated `node.toml`.** If a provisioning tool (ansible, a Helm chart, a
config-management run) renders `node.toml` from a template, its next run
overwrites whatever the page saved, and a move to `credentials.toml` with it.
Make the changes you want to keep in the template, or stop templating the
file and let the page own it.

**Conflicts.** The page remembers the file it loaded. If `node.toml` changes
underneath it — `sudo -e`, a deploy — the next save answers
`node.toml changed since the page loaded — reload to see the current file.`
instead of overwriting the edit.

!!! danger "Two of the confirmations are worth reading before you click through"

    Binding `[control]` to a non-loopback address and turning `auth = "off"`
    with no `allowed_nodes` each stop the page with a confirm that quotes the
    consequence. A wildcard control bind (`0.0.0.0`, `::`) is refused
    outright: it would expose an unauthenticated keying port on every
    interface while the port's own `Host` check refuses the LAN name anyway.

## `[web]`, `[web.dns]` and `[stream]` — the public page

The public read-only listener page, its HTTPS (built-in Let's Encrypt), Porkbun
dynamic DNS and the live audio streams have a page of their own:
[The public page](listening.md). Sign-in on it (`anonymous_status`,
`admin_on_public`, `rp_id`, `session_idle_days`, `session_max_days`,
`step_up_minutes`) is described in [Signing in](admin.md#settings).

## `[recording]` — recording every transmission

The node can save every transmission its conference carries to a folder,
Discord audio included. It is off until you turn it on. The section and its
defaults:

```toml
[recording]
enabled = false                      # true starts recording at the next start
dir = "/var/lib/astar/recordings"    # where the recordings go
max_bytes = "2 GB"                   # the most the folder may hold, at least 100 MB
max_age_days = 30                    # how long a recording is kept, 1–3650
frames = true                        # also keep the codec frames, for unamblify
```

When the folder passes `max_bytes`, or a recording is older than
`max_age_days`, the oldest recordings are deleted first. `max_bytes` is a
whole number with an optional unit: `B`, `KB`, `MB` and `GB` count in
thousands (`"2 GB"` is 2,000,000,000 bytes), `KiB`, `MiB` and `GiB` count in
1024s, and a bare number is bytes. The settings page shows these under
**Recordings**, and every one of them takes effect at the next restart.
What a recording contains and how to download one is on
[Recording](recording.md).
