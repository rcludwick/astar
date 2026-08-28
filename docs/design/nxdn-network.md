# NXDN — design

**Status:** designed, not built. Engine item `iax-b9c2`, client item `astar-b8e4`.
**Read first:** `docs/design/adding-a-network.md`, then `ysf-network.md` — NXDN
is the second AMBE+2 network and inherits most of YSF's answers.

## Why it comes after YSF

Not because it is harder. Because **it is the same vocoder work**, and doing YSF
first means NXDN pays for framing and a session module rather than for the
AMBE+2 path as well.

If YSF lands first, NXDN's realistic cost is: a protocol crate, a session module,
and the §2.4 wiring. Everything vocoder-shaped is already solved and proven on
air.

## What the data already tells us

From the published directory, 2026-08-28:

| | |
|---|---|
| Rows | 296 |
| `dial` fields | `host`, `port`, `kind` — no callsign, no modules |
| Ports | 41400 (228), 41401 (21), 41402 (13), 41040 (5), tail |
| Ids | numeric (`100`) — these are **talkgroups**, not callsigns |

Same conclusions as YSF: a bare endpoint, `addressesModule` false, no module
picker, and `host:port` as the identity because of the `+1/+2` instances.

**The ids being talkgroup numbers is the thing to notice.** NXDN "100" and P25
"100" both exist as directory rows, which is exactly why `DirectoryEntry.key` is
`network:id` and why `ReflectorModuleMemory` keys on it too. That collision is
already handled; do not re-introduce a bare-id lookup anywhere.

## The vocoder

NXDN uses **AMBE+2 half-rate** — the same family as YSF DN, on the same
AMBE-3000. Once YSF has a RATEP word for half-rate AMBE+2, NXDN's vocoder work is
frame packing and little else.

Confirm the exact rate parameters against the reference implementation rather
than assuming YSF's word transfers unchanged; "same family" is not "same
configuration".

## The protocol

NXDNReflector, UDP, poll-based, structurally similar to YSFReflector. The
reference is G4KLX's `NXDNClients`.

As with YSF: **read the reference, do not recall the wire format.** Build
`crates/astar-nxdn` with framing, a link FSM and a loopback reflector.

## What is genuinely new

1. `crates/astar-nxdn` — framing, link FSM, loopback reflector.
2. NXDN frame packing against the existing AMBE+2 path.
3. `crates/astar-console/src/nxdn.rs` — session, `SharedState`, `apply_audio`,
   `connect_with_stream`.
4. `ConsoleSession` §2.4 wiring, **audio fan-out included**.
5. Station facade, features, C ABI, `just cbindgen`, Swift binding,
   `Network.nxdn`.

## Open questions

* **Is a per-user identity required?** D-Star and M17 both transmit the
  operator's callsign, and astar already has one field for it
  (`CallSession.operatorCallsign`). NXDN's radio identity is numeric. If NXDN
  needs a radio ID rather than a callsign, that is a **new** credential with its
  own registration story, and it should be designed deliberately — not bolted
  onto the callsign field, which means something else.

  Resolve this before writing the session module. It is the one place NXDN might
  diverge from the "one callsign, every network" model, and getting it wrong
  means either a wrong ID on the air or a second field nobody needed.

* Talker display: reuse D-Star's last-heard treatment.
