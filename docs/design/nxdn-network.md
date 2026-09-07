# NXDN — design

**Status:** receive shipped; transmit gated (Tasks 9–10 of
docs/superpowers/plans/2026-09-07-nxdn-network.md). Engine item `iax-b9c2`,
client item `astar-b8e4`.
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

**Ruled (2026-09-07, see `docs/design/nxdn-wire.md`): reuse
`VocoderMode::YsfDn`. Do not add `VocoderMode::NxdnDn`.** `DroidStar`'s
`serialambe.cpp: SerialAMBE::config_ambe` sends NXDN the same
`AMBE3000_2450_0000` RATEP word and the same `packet_size = 7` it sends YSF —
byte-identical to `astar_codec::ysf::ratep_dn()`'s output — and MMDVMHost
regenerates NXDN's on-air AMBE with `CAMBEFEC::regenerateYSFDN(...)`. Same
family *is* same configuration here; see the wire note for the full citation
and the frame-packing detail this ruling still leaves to do.

## The protocol

NXDNReflector, UDP, poll-based, structurally similar to YSFReflector. The
reference is G4KLX's `NXDNClients`.

As with YSF: **read the reference, do not recall the wire format.** Build
`crates/astar-nxdn` with framing, a link FSM and a loopback reflector.

## What is genuinely new

1. `crates/astar-nxdn` — framing, link FSM, loopback reflector.
2. NXDN frame packing against the existing AMBE+2 path.
3. `crates/astar-console/src/nxdn.rs` — session, `SharedState` (link/talker/
   `ptt` only), `connect_with_audio`/`connect_with_stream` taking a
   `CallAudio` from the station's voice route (adding-a-network.md §2.3) —
   the session owns no `AudioRouter`.
4. `ConsoleSession` §2.4 wiring.
5. Station facade, features, C ABI, `just cbindgen`, Swift binding,
   `Network.nxdn`.

## Open questions

* ~~**Is a per-user identity required?**~~ **Answered (2026-08-29,
  astar-c9d2).** A numeric radio ID is a separate credential from a callsign,
  and astar now stores both: `CallSession.operatorCallsign` and
  `CallSession.dmrRadioID`. NXDN reads the numeric one. Nothing is bolted onto
  the callsign field, and there is no second callsign.

  The field is named for DMR because DMR is what drove it and DMR is what
  registers it — radioid.net issues the ID against a licence. If NXDN turns out
  to need a *differently* registered number rather than the same one, that is a
  third field and a fresh decision, not a re-argument of this one.

  **That exit was taken (2026-09-07).** NXDN source and destination ids are
  16-bit: `NXDNGateway/NXDNNetwork.cpp` packs them as
  `writeData(..., unsigned short srcId, unsigned short dstId, ...)`, and
  `NXDNReflector`'s `Reflectors.h` holds `CNXDNReflector::m_id` the same way. A
  registered DMR ID is six or seven digits and does not fit in sixteen bits, so
  it **cannot** be used verbatim, and truncating one would put somebody else's
  number on the air. So astar carries a third field: `CallSession.nxdnRadioID`,
  stored under `nxdn.radioId`, digits only, range `1...65519` (`NxdnID`) — the
  `0` address and the reserved block above `65519` are both refused.

  Adding it does not move `ConfigVersion`: it is an addition, and both
  directions already cope (`CLAUDE.md`, "Config version"). A dial with no NXDN
  id is refused before the dongle is touched
  (`ConnectError.missingRadioID` / `.radioIDOutOfRange`) rather than derived
  from anything else. The field's placement and label are still Rob's to
  change; the *separateness* is what the wire settles.

* Talker display: reuse D-Star's last-heard treatment.

## The identity question is not closed

The "does NXDN need its own field" question above is answered — it does, and
`CallSession.nxdnRadioID` exists. What is not settled is **where the range
check on that field lives.**

The 16-bit finding is structural and holds everywhere: NXDN source/destination
ids are `unsigned short` on the wire (`NXDNGateway/NXDNNetwork.cpp`,
`NXDNReflector/Reflectors.h`), so nothing above 65535 can ever be sent, and
`Station::nxdn_connect` (`crates/astar-station/src/station.rs`) refuses a
`radio_id` of `0` — "0 is not a registration" — before the dongle is touched,
the same way it refuses an empty callsign and a zero talkgroup. That refusal
is engine-level and applies to every caller: the app, `nxdn-listen`, and
anything built on `Station` hereafter.

The app's `NxdnID` goes narrower: `1...65519`, excluding a reserved block
above `65519` that the engine does not know about and does not enforce.
`nxdn-listen --radio-id` today accepts anything `Station::nxdn_connect`
accepts — `1..=65535` — which means a value in that reserved band is refused
by the Swift picker but not by the CLI or by the engine directly. Whether that
gap should close (push the reserved-block refusal down into
`astar-console::nxdn` so every caller gets it, not just the one built UI) or
stay exactly where it is (a policy choice that belongs to a client, not a
protocol constraint the engine should own) is Rob's decision, not an agent's —
recorded here so it is not silently assumed either way.
