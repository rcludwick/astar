# How the WT (Web-Transceiver) path works

**Audience:** anyone integrating or debugging the AllStar Web-Transceiver dial.
**Scope:** `Station::connect_wt` and everything it triggers, end to end.

## TL;DR

"WT" is the AllStar **Web-Transceiver** convenience path: a one-argument dial
(`connect_wt(node)`) that lets an authenticated AllStar account transmit through
its own node/repeater **on air**. It is a thin wrapper over the generic IAX2
`connect`, plus two AllStar-specific steps:

1. **Mint a portal token** over HTTPS from the AllStar web portal (using the
   account's *portal* credentials), and
2. carry that token in the IAX2 `CALLING_NAME` IE so the AllStar node recognizes
   the caller as the node owner and routes audio to the radio.

The IAX2 authentication itself still uses the ordinary guest secret
(`"allstar"`) — the portal password is **never** sent over IAX2 and never leaves
the token-minting HTTP step. This two-credential split is the single most
important thing to understand about WT.

## Two credentials, two jobs

| Credential | Where it's used | What it proves | Secret-free handling |
|---|---|---|---|
| **Portal account** (`user`, `password`, `node`) | HTTPS to `allstarlink.org/portal` to mint a token | "I own this account/node" | Resolved on demand; the password lives only inside `mint_wt_token` and is never stored in config/snapshot/event/log nor transmitted over IAX2 |
| **IAX2 guest secret** (`"allstar"`) | MD5 challenge/response in the IAX2 handshake | "I can complete the IAX2 auth" | Passed in-arg to the call; the same secret guest dials use |

The **on-air vs off-air** distinction is enforced entirely on the AllStar
server: a valid portal token in `CALLING_NAME` → the node treats you as the owner
and keys the repeater; a missing/invalid token → generic guest treatment
(typically an off-air monitor). The IAX2 secret is identical either way.

This is why WT and guest dials both use secret `"allstar"`; what makes a call
"WT / on-air" is the minted token, not the IAX2 secret.

## WT vs generic `connect`

Both paths funnel through the same `Station::connect(dest, calling, secret,
name)`. WT just pre-computes the arguments:

```
connect_wt("55553"):
  token  = mint_wt_token(portal_creds)        // HTTPS, blocking
  secret = config.secret                        // "allstar"
  connect(dest="55553", calling="55553", secret="allstar", name=token)
                                  └ calling_number      └ guest secret  └ CALLING_NAME = token
```

The path is then carried as `CallMode::WebTransceiver { node, name }`, which lowers
to a `CallProfile` that:
- sets `CALLING_NUMBER` = the node number (`"55553"`),
- sets `CALLING_NAME` = the minted token,
- **omits** the `CAPABILITY` IE (`send_capability = false`), and
- forces `CALLED_NUMBER = "s"` (the AllStar WT dialplan extension).

A generic `connect` instead sends the real caller-id as `USERNAME`, includes the
`CAPABILITY` codec mask, and dials the literal destination.

## The flow, stage by stage

### Stage 1 — Mint the portal token (`astar-asl3::mint_wt_token`)

A three-step HTTPS sequence against the AllStar portal:

1. `POST /portal/login.php` with `user=<account>&pass=<password>`. Redirects are
   disabled so the `302`'s `Set-Cookie` (the `PHPSESSID` session cookie, and the
   `allstar_token` JWT) can be read directly. Both cookies are kept (any
   non-expired `Set-Cookie`) and forwarded — drop either and the next page
   renders unauthenticated.
2. `GET /portal/webtransceiver.php?node=<owned-node>` with the cookie jar.
3. Extract the token from the returned HTML — the `callingName` param
   (e.g. `value="84906e5c0000"`).

The result is a short token string destined for the IAX2 `CALLING_NAME` IE. The
AllStar node's server-side validator maps that token back to the account's
callsign.

`PortalCredentials { user, password, node }` — note `user` is the **portal
account callsign**, `password` is the **portal account password** (not any IAX2
secret), and `node` is a node the account **owns** (needed as the query param).

### Stage 2 — Resolve the node to an address (`astar-asl3::resolve_node`)

The destination node number is resolved to `host:port`:
- **Primary:** a DNS **TXT** lookup of `<node>.nodes.allstarlink.org`, returning
  character-strings like `"NN=55553" "IP=104.232.32.242" "PT=4569"`; the `IP=`
  and `PT=` fields give the address (port defaults to 4569). The TXT query/parse
  is a hand-rolled RFC 1035 codec (`dns.rs`), trying the system resolver first,
  then `8.8.8.8` / `1.1.1.1`.
- **Fallback:** an `A`-record lookup of the same name on port 4569.

### Stage 3 — The IAX2 handshake

Driven by the pure outbound FSM in `astar-iax-core`. The WT-specific frame is the
**NEW**; the rest is standard IAX2 auth.

```
client                                   AllStar node
  │  NEW (CALLED="s", CALLING_NUMBER="55553",        │
  │       CALLING_NAME=<token>, USERNAME=            │
  │       "allstar-public", FORMAT=G.711µ,           │
  │       no CAPABILITY, CALLTOKEN=empty, dest=0)    │
  │ ───────────────────────────────────────────────▶│
  │                                                  │
  │  CALLTOKEN (anti-spoof token)                    │   (typical on ASL3)
  │ ◀───────────────────────────────────────────────│
  │  reset reliability seqno; NEW resent             │
  │  (same IEs + CALLTOKEN=<token bytes>, dest=0)    │
  │ ───────────────────────────────────────────────▶│
  │                                                  │
  │  AUTHREQ (MD5 challenge, real peer scallno)      │
  │ ◀───────────────────────────────────────────────│
  │  SetPeerCall(peer); AUTHREP(md5(challenge ||     │
  │  "allstar"))                                     │
  │ ───────────────────────────────────────────────▶│
  │                                                  │
  │  ACCEPT  ─────────────────────────────────────▶  │
  │ ◀───────────────────────────────────────────────│
  │  → Connected → CallStatus::Answered, audio opens │
```

Two subtleties that were hard-won bugs (don't regress them):

- **CALLTOKEN resent NEW keeps `dest_call = 0`** (ticket iax-ff7b). The
  CALLTOKEN frame's `source_call` is a throwaway anti-spoof scallno; Asterisk
  picks a *different* real scallno for the AUTHREQ. Putting the CALLTOKEN's
  scallno into the resent NEW's `dest_call` causes ASL3 to silently drop the
  call. The real peer scallno is learned later, from the AUTHREQ.
- **Reliability sequence reset on resend.** When the FSM moves NEW→resent, the
  reliability layer's `oseqno` is reset to 0 so the resent NEW starts a clean
  sequence (the WT shape work, iax-3fca).

The MD5 response is `md5(challenge || secret)` as lowercase hex, matching
Asterisk's `iax2_md5_hash`, with `secret = "allstar"` (the guest IAX2 secret —
again, **not** the portal password).

`Connected` fires on **ACCEPT** (IAX2 subclass 5); an ANSWER (Control subclass 4)
may or may not follow depending on the node's dialplan, and the FSM handles both.

### Stage 4 — Audio

A WT call rides `Manager::dial` + `Manager::route` and owns its own audio
channels (it shares no audio code with the inbound `Manager::adopt` path):

- **Output first:** `open_monitor_call` opens the playback device and joins it to
  a mixer bus before the call spawns, so RX audio has somewhere to go.
- **Mic after route:** `bind_mic` opens the capture device; the cpal callback
  feeds PCM into the `AudioRouter`, which resamples to 8 kHz and µ-law encodes
  160-byte frames, sending them to the call run-loop and waking it.
- **TX:** the run-loop drains mic frames → `SendVoice` → the first frame is a
  full Voice frame (G.711µ), subsequent ones are mini frames.
- **RX:** inbound Voice frames → `VoiceReceived` → decoded µ-law → the output bus
  mixer → cpal playback.

Ownership: `Station` → `Arc<Mutex<ConsoleSession>>` → `Manager` → `AudioRouter`
(owns the cpal streams). Each call has its own `CallAudio` channel pair.

## What the consumer sees

The whole thing is poll + snapshot, no callbacks:
- `connect_wt(node)` kicks off the handshake and returns. (It does block briefly
  on the portal HTTPS mint and the DNS resolve, which are synchronous.)
- The connection then progresses on the call's own thread; the consumer observes
  it via `snapshot().status` (`idle → dialing → answered`) and/or
  `next_event()` (`Answered`, `Hangup`, …).
- Once `answered`, `set_ptt(true/false)` keys/unkeys transmit; `tx_db`/`rx_db`/
  `remote_ptt` in the snapshot track levels.

## Configuration

Set the portal credentials on the station config; the guest secret defaults to
`"allstar"`:

- Rust: `StationConfig { portal: Some(PortalCredentials { user, password, node }), secret, .. }`
- C-ABI: `IaxConfig { portal_user, portal_pass, portal_node, secret }` — all
  three portal fields must be non-NULL to enable the WT path.
- Swift: `StationConfig.portalUser / .portalPass / .portalNode / .secret`
- Python: `Station(portal_user=…, portal_pass=…, portal_node=…, secret=…)`

The portal password is consumed into the config and used only at mint time; it is
never echoed into a snapshot, event, log, or any IAX2 frame.

## Code map

| Concern | Location |
|---|---|
| `Station::connect_wt` (entry) | `crates/astar-station/src/station.rs` (`connect_wt`) |
| `StationConfig` + `PortalCredentials` | `crates/astar-station/src/config.rs` |
| Portal token mint (HTTPS) | `crates/astar-asl3/src/mint.rs` (`mint_wt_token`) |
| Node resolution (DNS TXT) | `crates/astar-asl3/src/resolve.rs` (`resolve_node`), `dns.rs` |
| WT call mode → CallProfile | `crates/astar-iax/src/call_mode.rs` (`CallMode::WebTransceiver`) |
| NEW frame IEs | `crates/astar-iax-core/src/session/builders.rs` (`build_new`) |
| CALLTOKEN / AUTHREQ / ACCEPT FSM | `crates/astar-iax-core/src/session/handlers_outbound.rs` |
| MD5 challenge response | `crates/astar-iax-core/src/session/auth.rs` (`md5_response`) |
| Dial + audio wiring | `crates/astar-iax/src/manager.rs`, `crates/astar-iax/src/runtime.rs` |
| C-ABI entry | `crates/astar-sys/src/ffi.rs` (`iax_station_connect_wt`) |
| Swift / Python wrappers | `bindings/swift/Sources/AstarStation/Station.swift` (`connectWT`), `bindings/python/astarstation.py` (`connect_wt`) |
| Integration test (NEW shape + md5) | `crates/astar-iax/tests/wt_loopback.rs` |

## Related

- AllStar is a *policy layer*; the IAX2 core is vendor-neutral and also works
  against plain Asterisk (see the `astar-asl3` crate boundary).
- Live-tested against ASL3 parrot 55553 after iax-3fca (WT shape) + iax-ff7b
  (CALLTOKEN `dest_call=0` fix).
