# astar-server

AllStarLink node daemon built on the `astar-station` always-on Station
(iax-a1fb).  Runs headless with an HTTP+SSE control channel (`serve`) or in
an interactive stdin menu (`tui`).

## What it is

`astar-server` is a long-running process that holds one IAX2 Station open
and exposes call control, PTT, inbound listener management, and registration
over a local HTTP+SSE channel (serve mode) or a simple stdin menu (tui mode).

It is AllStar-first: the env-var names (`ALLSTAR_*`) and the sample config
reflect AllStarLink conventions, but the underlying library is plain-Asterisk
IAX2 — any IAX2 peer works.

It can also monitor one module of an M17 reflector, **receive-only**, when
`node.toml` has an `[m17]` section. Reflector audio is mixed into the
conference, and the node sends the reflector nothing but the packets that hold
the link open.

`node.toml` is secret-free. Credentials live in `credentials.toml` beside it
(mode 0600, written by the settings page at `/settings`), or are injected at
runtime via env vars or the `/secrets` control endpoint — `[secrets] source`
picks one.

## Run modes

```
astar-server serve --config node.toml
astar-server tui   --config node.toml
astar-server vocoder list --config node.toml
astar-server vocoder add <serial> --config node.toml [--yes]
```

Both modes load the TOML config, enable the inbound listener, and optionally
register with the upstream peer before entering the main loop.

- **serve** — binds the HTTP+SSE control server and blocks.  Signal with
  `SIGINT` or `SIGTERM` (or `POST /shutdown`) to stop gracefully.
  Background/daemon-friendly; pipe logs to a file or journal.

- **tui** — runs an interactive stdin menu.  Press Enter after each key.
  Exits on `q` (Shutdown) or EOF.

- **vocoder list / add** — manage `[vocoder] trusted_serials`, the only AMBE
  sticks the node may open (see below). Neither starts the node, and neither
  creates a config: `--config` must name an existing file.

## Trusted vocoders (`[vocoder]`)

The engine opens an AMBE vocoder port only when its USB serial number is in
`[vocoder] trusted_serials`. Absent or empty means no vocoder is ever opened.
A DVstick and a radio's FT-X CAT/PTT cable both enumerate as FTDI
`0403:6015`, and opening a cable's tty raises DTR/RTS, which keys the radio —
so nothing scans.

- `vocoder list` enumerates the vocoder-class USB devices and marks which are
  trusted. **It opens nothing.**
- `vocoder add <serial>` asks you to confirm the device is not a radio's
  CAT/PTT cable (or takes `--yes`), then opens that one port with DTR/RTS held
  low and sends RESET + PRODID with a bounded timeout. Only an `AMBE3000*`
  reply is written into `node.toml`, through the settings page's own
  `toml_edit` write path. `AMBE3003*` (DVstick 33) is recognised as not
  supported yet and is not added; anything else is refused.

Both need the `vocoder` cargo feature (`--features vocoder`), which pulls the
vendored ThumbDV serial driver. It is off by default because on Linux the
driver needs `libudev`; without it `list` is empty and `add` reports "no
hardware vocoder support". The settings page's **Vocoders** card edits the
same list and asks before trusting a serial typed in by hand. Full reference:
`docs/site/server/configuration.md`.

## Sample `node.toml`

```toml
# Codec negotiation: prefer_slin16 (DEFAULT) | prefer_slin | allow_slin | ulaw_only
# Omitting this key gives you prefer_slin16: 16 kHz wideband linear audio
# (~256 kbps) with peers that allow it, falling back to slin, then ulaw, then
# alaw. It switches the station's own audio pipeline (capture/playback/mixing)
# to 16 kHz (iax-4348); a peer that only offers slin/ulaw still gets a correct
# call — the codec edge resamples, and a ulaw-only ClearNode is answered in
# ulaw, not rejected.
# prefer_slin stops at 8 kHz 16-bit linear (~128 kbps) with peers that allow it
# (ASL3: `allow = slin`), falling back to ulaw. allow_slin offers slin but only
# uses it if the peer asks. ulaw_only is the pre-slin wire behaviour — set it
# explicitly on a bandwidth-constrained link.
#codec_policy = "ulaw_only"

[listener]
bind      = "0.0.0.0:4569"   # IAX2 UDP socket
answer    = "auto"            # "auto" | "manual"
max_calls = 20
auth      = "required"        # "required" | "optional" | "off"
# Optional inbound node allowlist. When present AND non-empty, only callers
# whose node id is on the list are admitted; everyone else is rejected with
# "not authorized" at call setup, before answer. Omit (or leave empty) to
# admit all callers — the default, backward-compatible behavior.
allowed_nodes = ["55553", "77777"]

# Optional — omit to skip registration
[register]
peer    = "104.232.32.242:4569"   # upstream registrar (host:port)
node_id = "77777"                  # node number / register username

# Optional — omit to use system defaults
[audio]
input  = "USB"        # capture device substring match; null = system default
output = "Built-in"   # playback device substring match; null = system default

# Optional — conference bridge topology (iax-647d).
# IMPORTANT: when this section is ABSENT the daemon default is mode = "bridge"
# (a pure mix-minus bridge among remote callers, local radio off) — so every
# connected user hears everyone else. This is the daemon default; the
# `astar-station` *library* default stays "handset" (1:1) for embedders.
[bridge]
mode = "bridge"               # "handset" (1:1) | "bridge" (default) | "conference"
mix_minus = true              # each member hears everyone but itself; false = full mix
include_local_radio = false   # add local mic as a source + local speaker as a sink

# Optional — voice/CW announcements (iax-da05, iax-c4ea).
[announce]
enabled = true
# Periodic station ID (omit id_interval_secs to disable).
id_interval_secs = 600        # seconds between automatic IDs
id_mode = "voice"             # "voice" (TTS) | "cw" | "off"
# On-join greeting (iax-c4ea): spoken to EACH joining user on their OWN leg,
# announcing the node they reached. The literal token {server-node-number} is
# replaced with this node's id expanded into space-separated digits so TTS reads
# it digit-by-digit (77777 -> "7 7 7 7 7"). Omit to use the default below.
join_template = "Connected to node {server-node-number}"

# TTS engine (piper) — required for any "voice"/Phrase::Text announcement,
# including the on-join greeting. See "Voice (TTS / piper) setup" below.
[announce.tts]
enabled    = true
binary     = "piper"                              # path or name of the piper binary
voice      = "/etc/iaxnode/voices/en_US.onnx"     # piper voice model (.onnx)
timeout_ms = 4000

# Per-event announcements. The "answered" event IS the on-join greeting: enable
# it to speak join_template to each joining conference member.
[announce.events]
answered = { enabled = true, destination = "to_air" }

# [wireguard] — optional userspace WireGuard link transport (iax-580b) for
# guaranteed inbound behind CGNAT. Presence of the section routes the WHOLE
# engine (outgoing calls, registrar, inbound listener) over ONE shared
# userspace stack — no TUN device, no root. Private key is NOT here; export
# the env var named by secret_ref (default WIREGUARD_PRIVATE_KEY) before launch.
# [wireguard]
# address         = "10.99.0.2/32"        # node tunnel address (IPv4 CIDR)
# peer_public_key = "<VPS wg public key, base64>"
# endpoint        = "vps.example.org:51820"
# allowed_ips     = ["10.99.0.0/24"]
# keepalive_secs  = 25
# secret_ref      = "WIREGUARD_PRIVATE_KEY"  # env var holding the private key
# also_bind_udp   = "0.0.0.0:4569"        # optional extra plain-UDP listener
#                                         # for direct/LAN peers

# Optional — receive-only M17 reflector monitor. ABSENT = M17 off. No key here
# can transmit, and there is no callsign key: a Discord dial presents the
# dialler's registered callsign, and POST /m17 must carry one in its body.
# [m17]
# enabled    = true
# directory  = "bundled"            # "bundled" | absolute path to a JSON file | omit
# codec_dirs = []                   # extra libcodec2 directories (start-up only)
# [m17.reflectors]
# "M17-XYZ" = "m17.example.net:17000"   # port defaults to 17000

# Optional — the AMBE vocoder sticks this node may open, by USB serial number.
# ABSENT or empty = none is ever opened, and nothing scans for one (a stick and
# a radio's CAT/PTT cable look the same on USB). Add one with
# `astar-server vocoder add <serial>`, which probes it first.
# [vocoder]
# trusted_serials = ["TESTSER1"]

# Optional — record every transmission the node carries, one FLAC per over.
# ABSENT or enabled = false = off. See docs/site/server/recording.md.
# [recording]
# enabled      = true
# dir          = "/var/lib/astar/recordings"   # absolute path
# max_bytes    = "2 GB"                        # at least 100 MB
# max_age_days = 30                            # 1–3650
# frames       = true                          # keep M17 codec frames for unamblify

[control]
bind = "127.0.0.1:8730"   # HTTP+SSE control channel (serve mode)

[secrets]
source = "file"   # "file" | "env" | "control" | "config" — see Environment variables below
```

> **Supervise the daemon when using `[wireguard]`.** The userspace stack runs
> on a background thread; if it fails the node keeps running but tunnel peers
> become unreachable. Run `astar-server` under a process supervisor (e.g. a
> systemd unit with `Restart=always`) so it recovers. Stack health (handshake
> age, tx/rx/drop counters) is logged periodically via `tracing`.

Field notes:

| Section | Field | Values |
|---|---|---|
| top-level | `codec_policy` | `"prefer_slin16"` (default) \| `"prefer_slin"` \| `"allow_slin"` \| `"ulaw_only"` (iax-31f7, iax-4348). Applies to both the inbound listener and (via the Station) outbound links; `prefer_slin16` also switches the station's audio pipeline to 16 kHz. A ulaw-only peer is still answered in ulaw; a peer with no codec in common is rejected with CAUSE `Unable to negotiate codec` |
| `[listener]` | `answer` | `"auto"` (answer immediately) or `"manual"` (wait for `/answer`) |
| `[listener]` | `auth` | `"required"`, `"optional"`, or `"off"` (case-insensitive) |
| `[listener]` | `allowed_nodes` | Inbound node allowlist. Absent/empty = admit all; non-empty = reject callers not on the list ("not authorized") before answer |
| `[bridge]` | `mode` | `"handset"` (1:1), `"bridge"` (mix-minus, the **daemon default** even when the section is absent), or `"conference"` (alias for the same mix-minus engine) |
| `[bridge]` | `mix_minus` | `true` (default) — each member hears everyone but itself; `false` = full mix (members hear themselves), for parrot/loopback |
| `[bridge]` | `include_local_radio` | `false` (default) — pure bridge among remote callers; `true` adds the local mic as a conference source and feeds the local speaker the sum of all members |
| `[announce]` | `enabled` | Master switch for announcements |
| `[announce]` | `id_mode` | `"off"`, `"cw"` (Morse) or `"tts"` (spoken) for the periodic station ID. Absent reads as `"off"`. |
| `[announce]` | `id_interval_secs` | Seconds between automatic IDs; omit to disable |
| `[announce]` | `join_template` | On-join greeting text (iax-c4ea). The token `{server-node-number}` is replaced with this node's id as space-separated digits (`77777` → `7 7 7 7 7`). Default when unset: `"Connected to node {server-node-number}"`. Spoken to each joining user's own leg when `[announce.events].answered` is enabled |
| `[announce.tts]` | `enabled` | Enable the piper TTS subprocess (required for any voice/`Phrase::Text` announcement, including the join greeting) |
| `[announce.tts]` | `binary` | piper executable (path or name on `PATH`); default `"piper"` |
| `[announce.tts]` | `voice` | piper voice model file (`.onnx`) |
| `[announce.tts]` | `timeout_ms` | Synthesis timeout in ms; default `4000` |
| `[announce.events].answered` | `enabled` | Fire the on-join node-id greeting (`join_template`) to each joining conference member |
| `[wireguard]` | `enabled` | Defaults to `true` when the section is present (presence selects the WG transport for the whole engine); set `false` to keep plain UDP without deleting the section |
| `[wireguard]` | `address` | Tunnel IP in CIDR form (IPv4), e.g. `"10.99.0.2/32"` |
| `[wireguard]` | `peer_public_key` | VPS peer public key (base64 x25519) |
| `[wireguard]` | `endpoint` | VPS WireGuard endpoint `host:port` (public underlay address) |
| `[wireguard]` | `allowed_ips` | Networks reachable through the tunnel, e.g. `["10.99.0.0/24"]` |
| `[wireguard]` | `keepalive_secs` | Persistent keepalive interval in seconds (default `25`) |
| `[wireguard]` | `secret_ref` | Env var holding the base64 private key (default `"WIREGUARD_PRIVATE_KEY"`); the key itself never lands in the file |
| `[wireguard]` | `also_bind_udp` | Optional plain (non-tunnel) UDP listener address bound alongside the tunnel listener for direct/LAN peers, e.g. `"0.0.0.0:4569"` |
| `[m17]` | `enabled` | Defaults to `true` when the section is present. An absent section means M17 is off |
| `[m17]` | `directory` | `"bundled"` (the dated hamcall-db M17 snapshot compiled in; CC BY 4.0, "Reflector data provided by DVRef — https://dvref.com/"), an absolute path to a JSON file in the same format, or omitted (table only). A missing or unparseable file is logged once and ignored |
| `[m17]` | `codec_dirs` | Extra `libcodec2` search directories, read once at start-up |
| `[m17.reflectors]` | `"NAME"` | `"host[:port]"`, port default `17000`. Checked before `directory` |
| `[announce]` | `link_connect_template` / `link_disconnect_template` | Spoken link announcements (iax-9e02). `{node}` is replaced with the target node number as spaced digits. Connect fires BEFORE the dial; disconnect fires AFTER the link is torn down, so it never goes out over the link. Heard only by web-transceiver members — never sent to linked nodes. Defaults: `"Connecting to node {node}"` / `"Disconnected from node {node}"`. |
| `[announce]` | `m17_connect_template` / `m17_disconnect_template` | Spoken M17 monitor announcements. `{node}` becomes the reflector's characters spaced out, then " module ", then the letter. Heard only by conference members that are not links. Defaults: `"Monitoring M17 reflector {node}"` / `"Stopped monitoring M17 reflector {node}"`. |
| `[portal]` | `user` / `node` / `credential_env` | `AllStarLink` portal account for WT-token minting (iax-b7f2). `wt-guest` link dials mint a fresh token per dial and send it as `CALLING_NAME` — required by ASL3 WT contexts (e.g. parrot 55553), which validate it server-side and clear tokenless calls ~1 s after answer. The password is the `portal` key in `credentials.toml` under `source = "file"`, else the env var named by `credential_env` (default `ALLSTAR_PORTAL_PASS`); it is never in this file. |
| `[links."<node>"]` | `shape` | Per-target link dial shape (iax-5029): `"standard"` (default) or `"wt-guest"`. ASL3 app nodes (e.g. parrot 55553) only accept the web-transceiver guest shape — declare them `wt-guest` and `/link` (and DTMF `*3`) dial them correctly. |
| `[dtmf]` | `enabled` | DTMF `*` command execution (iax-d254): `*3<node>` connect, `*2<node>` monitor, `*1<node>` disconnect, finalized by a `#` or the inter-digit timeout. Default **false** — enabling lets any connected member command links. |
| `[dtmf]` | `inter_digit_timeout_ms` | Gap that finalizes a pending `*` command (default 3000). |
| `[secrets]` | `source` / `file` | `"file"` reads `credentials.toml` beside `node.toml` (or the path in `file`, relative to `node.toml`'s directory; mode 0600, not loaded when group/other-readable); `"env"` loads from env at startup; `"control"` waits for `POST /secrets`; `"config"` reads credentials inline from `node.toml` (keep it 0600; the node warns otherwise) |
| `[secrets]` | `secret` / `portal` / `discord` / `porkbun_pk` / `porkbun_sk` | Inline credentials, read only under `source = "config"`: the node password, the portal password, the Discord bot token, and Porkbun's two API keys. Never in `Debug`, logs, snapshots or the settings page |
| `[web]` | `public_bind` | The public, read-only listener page, e.g. `"0.0.0.0:443"`. Absent: no public page. A separate router: no admin route is reachable on it. See `docs/site/server/listening.md` |
| `[web]` | `public_hostnames` | Names the page answers to and its certificate covers; `{node}` is `[register] node_id`. Defaults to `["{node}.<domain>"]` with `[web.dns]` |
| `[web]` | `acme_email` / `acme_staging` | Let's Encrypt contact (optional) and its staging CA |
| `[web]` | `acme_challenge` / `acme_http_bind` | `"http-01"` (port 80, default `0.0.0.0:80`) or `"dns-01"` (default with `[web.dns]`; no port 80 unless `acme_http_bind` is set) |
| `[web.dns]` | `provider` / `domain` / `host` | `"porkbun"`; the zone at Porkbun; the default hostname's subdomain (`"{node}"`) |
| `[web.dns]` | `ddns` / `ddns_interval_secs` / `ttl` | Keep an A record per hostname at this box's public IPv4 (default off, every 300 s, TTL 600). Keys: `PORKBUN_API_KEY` / `PORKBUN_SECRET_API_KEY`, or `porkbun_pk` / `porkbun_sk` |
| `[stream]` | `enabled` | Live link audio on the public page (`/stream.ws` Opus, `/stream.mp3`). Default `false`; never in parrot mode; announcements are never in it |
| `[stream]` | `max_listeners` / `mp3_kbps` / `opus_kbps` | Listener cap across both streams (1–200, default 25); MP3 bitrate (8/16/24/32/40/48/64, default 32); Opus bitrate (6–128, default 24) |

## Voice (TTS / piper) setup

Voice announcements — the periodic ID (`id_mode = "voice"`) and the **on-join
node-id greeting** (iax-c4ea) — are rendered by [piper](https://github.com/rhasspy/piper),
an offline neural TTS engine, invoked as a subprocess. piper is a **runtime
prerequisite**; the daemon and its tests build and pass without it, but no voice
audio is produced until it is installed and `[announce.tts].enabled = true`.

1. **Install the piper binary** (so `[announce.tts].binary` resolves):

   ```sh
   # There is no Homebrew formula for piper. Download a prebuilt binary for
   # your platform from the project's own releases page and put it on PATH:
   #   https://github.com/rhasspy/piper/releases
   piper --version                   # confirm it is on PATH
   ```

   Set `[announce.tts].binary` to the executable's path if it is not on `PATH`.

2. **Download a voice model** (a `.onnx` file plus its `.onnx.json` sidecar) and
   point `[announce.tts].voice` at the `.onnx`:

   ```sh
   mkdir -p /etc/iaxnode/voices
   # Download a voice + its config from the piper voices catalog, e.g. en_US:
   #   https://huggingface.co/rhasspy/piper-voices
   #   en_US-lessac-medium.onnx  AND  en_US-lessac-medium.onnx.json
   cp en_US-lessac-medium.onnx      /etc/iaxnode/voices/en_US.onnx
   cp en_US-lessac-medium.onnx.json /etc/iaxnode/voices/en_US.onnx.json
   ```

   Keep the `.onnx.json` sidecar next to the `.onnx`; piper needs both.

3. **Enable it** in `node.toml` (`[announce.tts].enabled = true`) and verify
   end-to-end by joining the node — the joining user should hear "Connected to
   node &lt;your digits&gt;".

If piper is missing or the model path is wrong, voice announcements are skipped
(the resolve fails and the announcement is dropped); CW and WAV-sample
announcements are unaffected.

## Environment variables

Active when `[secrets].source = "env"`. Under `source = "file"` the same
credentials come from `credentials.toml` instead — `node`, `portal`,
`discord`, `"link:<node>"`, and a bare username per inbound peer — and only
`[wireguard] secret_ref` is still read from the environment.

| Variable | Purpose |
|---|---|
| `ALLSTAR_NODE` | This node's ID — the register username |
| `ALLSTAR_SECRET` | Registration password for `ALLSTAR_NODE` |
| `ALLSTAR_PEER_<NODE>` | Inbound peer secret; repeat for each peer node |
| `ALLSTAR_LINK_<NODE>` | Password WE present when dialing `<NODE>` as a standard-shape link (iax-5029); also settable at runtime via `POST /secrets` with username `link:<node>` |
| `DISCORD_BOT_TOKEN` | The Discord bot token (`discord` feature) |
| the name in `[portal] credential_env` | The portal password; `ALLSTAR_PORTAL_PASS` by default |

Example:

```sh
export ALLSTAR_NODE=77777
export ALLSTAR_SECRET=MyRegistrationPassword
export ALLSTAR_PEER_55553=PeerSecret          # secret for inbound calls from node 55553
astar-server serve --config node.toml
```

`ALLSTAR_PEER_<NODE>` authenticates **inbound** calls: with the listener's
`auth = "required"` (or `"optional"`), a caller offering username `<NODE>` is
MD5-challenged against this secret; an unknown username is rejected under
`required`. The same store feeds outbound registration, so one variable covers
both directions for a given peer.

> Hyphenated usernames (e.g. `allstar-public`) cannot be expressed as
> `ALLSTAR_PEER_*` env var names. They go in `credentials.toml` as a bare
> key (`allstar-public = "…"`), or at runtime via `POST /secrets` /
> `POST /settings/credential` with `peer:allstar-public`.

When `source = "control"`, omit the env vars and push credentials after startup
via `POST /secrets`.

## HTTP control API

The control channel binds to `127.0.0.1:8730` by default.

**Security note:** The default bind is localhost-only.  The channel can dial
nodes, register with a peer, key the transmitter, and carries credentials on
`POST /secrets` and `POST /settings/credential`.  Every route refuses a
`Host` that is not `localhost`, a loopback literal or the bound address, and
bodies over 64 KiB.  Every `POST` refuses an `Origin` that does not match
`Host`, and a browser's request (`Origin` or `Sec-Fetch-Site` present) that
is not `application/json`, so a page in the operator's browser cannot post
a `<form>` to `/key` (iax-ctlcsrf); `curl` sends neither header and is
unaffected.

**Sign-in (feature `auth`, in the release build).**  A loopback peer is the
built-in `console` admin (unless `[control] loopback_login = true`, and
never with `Forwarded`/`X-Forwarded-For`); anyone else signs in with a
passkey, a security key (with or without a PIN) or an authenticator app,
and every admin route goes through one classification table
(`src/access.rs`): admin, or admin with a credential used in the last
`step_up_minutes` for secrets, settings saves, restart and shutdown.  The
public page serves the same admin pages under `/admin/` to a signed-in
admin, and nothing else from this router.  The first admin comes from the
console:

```bash
astar-server admin invite --role admin --name W1ABC   # prints a one-time link
astar-server admin users
astar-server admin revoke-sessions --user W1ABC
```

Users, credentials, hashed sessions and invites, and a 500-entry audit live
in `state/auth.json` (0600).  The code is `src/auth/`; the operator's guide
is `docs/site/server/admin.md`.

### Routes

#### `GET /status`

Return a snapshot of current daemon state.

```sh
curl http://127.0.0.1:8730/status
```

Response (200):

```json
{
  "reply": "snapshot",
  "listening": true,
  "registered": true,
  "calls": [],
  "links": []
}
```

#### `POST /link` — node-to-node link control (iax-d829.1)

Connect, monitor, or disconnect a link to another node — the AllStar `ilink`
surface (`*3`/`*2`/`*1`). `action` is `connect` (transceive, full two-way),
`monitor` (RX-only, mic withheld), or `disconnect`. `addr` is **optional**: when
present the node dials that explicit `host:port` (bypassing AllStar DNS — handy
for a local Asterisk/ASL3 harness or a NAT hairpin); when omitted the node
resolves `node` via `<node>.nodes.allstarlink.org`. A `connect`/`monitor` for a
node that is ALREADY linked switches the existing link's mode in place (no
re-dial, no DNS) — the `*2` → `*3` upgrade path.

```sh
# *3 — connect (two-way) to node 55553 via AllStar DNS
curl -X POST http://127.0.0.1:8730/link \
     -H 'Content-Type: application/json' \
     -d '{"action":"connect","node":"55553"}'

# *2 — monitor (receive-only) a node at an explicit address (harness/localhost)
curl -X POST http://127.0.0.1:8730/link \
     -H 'Content-Type: application/json' \
     -d '{"action":"monitor","node":"55553","addr":"127.0.0.1:4569"}'

# *1 — disconnect the link to node 55553
curl -X POST http://127.0.0.1:8730/link \
     -H 'Content-Type: application/json' \
     -d '{"action":"disconnect","node":"55553"}'
```

Returns `{"ok":true}` on success; 400 for a malformed body or unknown `action`;
500 (e.g. `{"error":"no live link to node ..."}`) when disconnecting a node with
no live link. Live links appear in `GET /status` under `links[]` (per link:
`node`, `mode`, `state` = `connecting`/`up`, `keyed`), and link lifecycle edges
(`connected`/`disconnected`/`keyed`) are pushed on `GET /events` as
`{"event":"link", ...}`.

> Auth/secret: this is the `auth=off` interop path (empty dial secret). Per-node
> link authentication (CALLTOKEN + per-node password) is wired by the
> conformance sibling (iax-5029). Permanent links and reconnect supervision
> (`Manager::tick`) are a follow-up — links here are one-shot.

#### `POST /m17` — receive-only M17 reflector monitor

Link to one module of an M17 reflector as a receive-only monitor, or tear it
down. `reflector` is a name the node resolves from `[m17.reflectors]` and then
the `[m17]` `directory`; there is no address field. `callsign` is required.

```sh
curl -X POST http://127.0.0.1:8730/m17 \
     -H 'Content-Type: application/json' \
     -d '{"action":"monitor","reflector":"M17-USA","module":"C","callsign":"AJ7HR"}'

curl -X POST http://127.0.0.1:8730/m17 \
     -H 'Content-Type: application/json' \
     -d '{"action":"disconnect"}'
```

The route requires `Content-Type: application/json` (`415` otherwise) and, when
a browser sends one, an `Origin` that matches `Host` (`403` otherwise).

Returns `{"ok":true}` once the monitor starts linking. A malformed body, an
unknown `action` or field (including `addr`), a missing `reflector`, `module` or
`callsign`, or a `module` that is not exactly one character gives a 400. A
refusal from the node gives a 500 with a one-sentence `error` — for example M17
not enabled, a bad reflector name, module or callsign, an unknown reflector, a
module the directory does not list, no Codec 2 decoder, a monitor already up, a
bridge in handset/parrot mode, or `disconnect` with no monitor up
(`No M17 link is up.`). See docs/site/server/control-api.md for the full table.
The monitor shows in `GET /status` as `m17` (never in `links[]`), alongside
`m17_enabled` and `m17_available`, and its lifecycle is pushed on
`GET /events` as `{"event":"m17", ...}`. The node only ever sends the
reflector connect, keepalive-reply and disconnect packets. There is no
automatic reconnect, and a restart ends the monitor.

#### `POST /dial`

Dial an outbound call to a node.

```sh
curl -X POST http://127.0.0.1:8730/dial \
     -H 'Content-Type: application/json' \
     -d '{"node":"55553"}'
```

#### `POST /enable_inbound`

Enable (or re-enable) the inbound call listener.

```sh
curl -X POST http://127.0.0.1:8730/enable_inbound
```

#### `POST /register`

Trigger registration with the upstream peer configured in `[register]`.

```sh
curl -X POST http://127.0.0.1:8730/register
```

Returns 500 if no `[register]` section is present in the config.

#### `POST /secrets`

Push a credential into the runtime secret store.  The secret is **never**
echoed in the response.

```sh
curl -X POST http://127.0.0.1:8730/secrets \
     -H 'Content-Type: application/json' \
     -d '{"username":"77777","secret":"MyRegistrationPassword"}'
```

Response (200): `{"ok":true}`

#### `GET /settings` and the settings routes

The settings page edits `node.toml` in place and holds the credentials. Its
API is documented in full in `docs/site/server/control-api.md`; the shapes:

| Method | Path | Body → reply |
|---|---|---|
| `GET` | `/settings`, `/settings.js` | the page and its script |
| `GET` | `/settings/state` | `{disk, running, pending:{restart,apply}, live, credentials, credential_source, credential_file, etag, file:{path,writable}, restart:{busy,why}, node_id, registered, feature}` — credentials are `"set"`/`"unset"`, never values |
| `POST` | `/settings` | `{"etag":"…","patch":{"<dotted path>":<value>},"acknowledge":[]}` → `{"ok":true,"applied":[…],"on_restart":[…],"etag":"…"}`; `409 {"error":"node.toml changed since the page loaded","conflict":true}` on a stale etag |
| `POST` | `/settings/credential` | `{"name":"node\|portal\|discord\|link:<n>\|peer:<user>","value":"…"}` or `{"name":"…","forget":true}` → bare `{"ok":true}` |
| `POST` | `/settings/portal/test` | `{}` → `{"ok":true}` or `409 {"error":"portal/token error: …"}` |
| `POST` | `/settings/restart` | `{}` or `{"disconnect":true}` → `{"ok":true}`; `409 {…,"busy":true}` while on a net |

Every `POST` here requires `Content-Type: application/json` (`415` otherwise,
naming the route). `/settings/state` and every `POST` here answer
`503 {"error":"settings are not available on this node"}` when no settings
store was installed (an embedder or a test controller); the page and its
script are still served.

#### `POST /bridge`

Re-wire the conference bridge live (iax-647d). All fields are optional — an
omitted field keeps its current value, so a partial body is a partial update.
`mode` is `"handset"`, `"bridge"`, or `"conference"` (case-insensitive).

```sh
curl -X POST http://127.0.0.1:8730/bridge \
     -H 'Content-Type: application/json' \
     -d '{"mode":"conference","mix_minus":true,"include_local_radio":false}'
```

Switching modes re-wires the calls already up: `handset → bridge/conference`
enrolls every live call as a mix-minus member; `bridge/conference → handset`
drains the conference and restores the per-call output bus. Returns
`{"ok":true}` on success, or 500 with `{"error":"bridge.mode: ..."}` for an
unrecognised `mode`.

#### `POST /shutdown`

Gracefully shut down the daemon.

```sh
curl -X POST http://127.0.0.1:8730/shutdown
```

#### `GET /events` (SSE)

Subscribe to asynchronous events as a Server-Sent Events stream.

```sh
curl -N http://127.0.0.1:8730/events
```

Events are newline-delimited JSON `data:` lines in the SSE format.  Event
types: `snapshot`, `incoming_call`, `registered`, `register_failed`, `hangup`,
`announcement_started`, `announcement_finished`, `link`, `m17`, `dtmf`.

`dtmf` (iax-2f5e) reports EVERY digit received from a connected member —
whether or not `[dtmf] enabled` maps it to a command — so an operator can see
whether a handset's tones are reaching the node at all:

```json
{"event":"dtmf","call":7,"digit":"5"}
{"event":"dtmf","call":7,"digit":"#","command":"connect 55553"}
```

`command` appears only on the digit that completes a `*`-sequence. A sequence
finalized by the inter-digit timeout (no `#`) dispatches without a `command`
annotation; watch for the resulting `link` event instead.

### Additional routes

| Method | Path | Description |
|---|---|---|
| `POST` | `/hangup` | Hang up the active call |
| `POST` | `/answer` | Answer an inbound call (when `answer = "manual"`) |
| `POST` | `/reject` | Reject an inbound call |
| `POST` | `/key` | Assert PTT (key the transmitter) |
| `POST` | `/unkey` | Release PTT |
| `POST` | `/disable_inbound` | Stop accepting new inbound calls |
| `POST` | `/deregister` | Withdraw registration from the upstream peer |
| `POST` | `/devices` | Select audio devices at runtime: `{"input":"USB","output":"Built-in"}` |
| `POST` | `/bridge` | Re-wire the conference bridge live: `{"mode":"conference","mix_minus":true,"include_local_radio":false}` (all fields optional) |
| `POST` | `/m17` | Receive-only M17 reflector monitor: `{"action":"monitor","reflector":"M17-USA","module":"C","callsign":"AJ7HR"}` or `{"action":"disconnect"}` |
| `GET` | `/settings/state` | The settings page's view of `node.toml` and the credentials' presence |
| `POST` | `/settings` | Patch `node.toml` in place |
| `POST` | `/settings/credential` | Set or forget one credential |
| `POST` | `/settings/portal/test` | Mint and drop one WT token with the running portal credentials |
| `POST` | `/settings/restart` | Restart the daemon in place (re-exec) |

Error responses are `{"error":"<message>"}` with an appropriate HTTP status.
A `Host` that does not name this node is `403` on every route; a body over
64 KiB is `413`.

## TUI menu keys

Press Enter after each key.

| Key | Action |
|---|---|
| `d <node>` | Dial node (e.g. `d 55553`) |
| `h` | Hangup |
| `k` | Key (PTT on) |
| `u` | Unkey (PTT off) |
| `a` | Answer inbound call |
| `r` | Reject inbound call |
| `i` | Enable inbound listener |
| `o` | Disable inbound listener |
| `g` | Register |
| `G` | Deregister (uppercase G) |
| `s` | Status (snapshot) |
| `q` | Shutdown and exit |

## Notes

**Secret-free config:** No passwords or tokens belong in `node.toml`.
`source = "file"` keeps them in `credentials.toml` beside it, mode 0600,
written by the settings page; `source = "env"` suits an automated deployment
that already has a secret store; `source = "control"` pushes credentials
after startup via `POST /secrets`.

**Conference bridge by default (iax-647d):** The daemon defaults to
`mode = "bridge"` — a mix-minus conference where every connected user hears
everyone else. With local radio off (the default), a *lone* connected user is
alone (hears nothing, talks to no one) until a second joins; set
`include_local_radio = true` to fold the local mic/speaker back in, or
`mode = "handset"` for the old 1:1 behavior. Switch live with `POST /bridge`.
Per-call-ID control of individual members is deferred.

**AllStar-first:** Env-var names, sample node numbers, and the `[register]`
defaults reflect AllStarLink conventions, but `astar-iax-core` is
vendor-neutral IAX2 and works with any Asterisk peer.

## Deferred / out of scope (SP-2)

The following are explicitly not implemented in this release:

- **Socket/UNIX adapter** — a socket-based command adapter is not included.
- **Remote TUI client** — the TUI is stdin-only; no remote terminal client.
- **Secrets in `node.toml`** — the config file stays secret-free. The one
  file that holds credentials is `credentials.toml` (`source = "file"`), a
  separate file with its own mode bit; the inline `[secrets] secret` under
  `source = "config"` exists for a privately mounted file and covers only
  the registration password.
- **Control-channel auth / TLS** — the HTTP channel has no authentication or
  encryption.  Bind to localhost and control access at the network level.
- **External reachability** — binding the control channel on a non-loopback
  address is tracked as iax-be48 (SP-2).
- **Self-daemonize** — process supervision is left to the caller (systemd,
  launchd, etc.).
