# YSF — design

**Status:** the protocol crate is built; the vocoder and the session are not.
Engine item `iax-e8a4`, client item `astar-e7b3`. See "Where this stands"
at the end.
**Read first:** `docs/design/adding-a-network.md` — the fourteen layers. This
document covers only what is YSF-specific.

## Why this one first

Of the AMBE family it is the cheapest by a distance, and everything that makes
it cheap is already paid for:

* **1,439 reflectors are already in the directory**, published with
  `dial.kind: "ysf"`, and already listed in astar's search pane today. They are
  visible and undialable — the exact state the directory was designed to make
  honest, and the exact state this closes.
* **The vocoder is a RATEP word away.** YSF DN mode is AMBE+2 on the AMBE-3000
  in the ThumbDV astar already drives (`vendor/ambe-thumbdv`). Not a new driver,
  not new hardware, not a new licence.
* **The client needs nothing.** `dial.kind` is already `ysf`, `addressesModule`
  is already false, the picker segment appears from `Network.available`, and the
  search pane, dial-by-name, sync and attribution all work unchanged.

So this is layers 1–11 of the recipe and almost none of 12–14. It is also the
proving run for the AMBE+2 path that NXDN and DMR then inherit.

## What the data already tells us

From the published directory, 2026-08-28:

| | |
|---|---|
| Rows | 1,439 |
| `dial` fields | `host`, `port`, `kind` — **no callsign, no modules** |
| Ports | 42000 (837), 42002 (141), 42001 (87), 42003 (44), then a long tail |
| Ids | numeric strings (`00006`) |

Two things follow. **YSF dials a bare endpoint** — there is no wire callsign to
address and no module to choose, so `addressesModule` stays false and the module
picker correctly never appears. And the `+1/+2/+3` port pattern is several
reflector instances on one host, so the host alone is not an identity: key on
`host:port`.

## The vocoder

YSF carries two voice modes:

* **DN** (digital narrow) — AMBE+2 half-rate plus FEC. The common case.
* **VW** (voice wide) — full-rate. Rarer.

Both are AMBE-3000 modes. The work is a RATEP word per mode alongside
`packet::ratep_dstar()`, and the frame packing to get the vocoder bytes out of a
YSF frame.

**Decide early whether VW is in scope.** Supporting DN only is a legitimate first
milestone — it is what the overwhelming majority of traffic uses — but the client
must then *say* so when a VW stream arrives rather than emitting noise. Silence
with a reason beats garbage.

## The protocol

YSFReflector, UDP. Poll-based: the client polls to register and keeps polling to
stay registered, then exchanges data frames.

**Do not write this from memory.** The reference implementation is G4KLX's
`YSFClients` / `MMDVMHost`, and the wire detail — the `YSFP` / `YSFU` / `YSFD` /
`YSFS` packet tags, the poll cadence, the FICH layout inside a data frame — comes
from reading that, not from recall. Everything in this section is a claim to
verify before it is a claim to build on.

Build it as `crates/astar-ysf` with the same shape as `crates/astar-dstar`: FICH
and frame parsing, a link FSM, and **a loopback reflector for tests**. That last
one is not optional — §5.1 forbids tests that touch anything but `127.0.0.1`, so
without it the session layer cannot be tested at all.

## The YCS wrinkle

Some rows describe themselves as YCS (`"(YCS202)"` in the directory today). YCS
is a newer reflector implementation that adds **DG-ID rooms** — module-like
subdivisions of one reflector.

This matters because it breaks the "YSF has no modules" assumption above, for
some reflectors. It does not need solving in the first milestone: joining a YCS
reflector without specifying a DG-ID lands somewhere sane. But it should be
*known* before the dial grammar is fixed, because retrofitting a module onto a
network whose `addressesModule` is false means changing a published schema field,
and that is a much worse day than deciding now.

Recommendation: ship DN + plain YSF first, treat DG-ID as a follow-up, and do not
paint the dial grammar into a corner in the meantime.

## What is genuinely new

1. `crates/astar-ysf` — framing, FICH, link FSM, loopback reflector. **Done.**
2. AMBE+2 RATEP word(s) + YSF frame packing in `astar-codec`.
3. `crates/astar-console/src/ysf.rs` — session, run loop, `SharedState`
   (link/talker/`ptt` only — no meters, no audio preferences), and
   `connect_with_audio`/`connect_with_stream` taking a `CallAudio` from the
   station's voice route (adding-a-network.md §2.3, since the
   one-audio-lane refactor — `YsfLink` owns no `AudioRouter`).
4. `ConsoleSession` wiring — the §2.4 checklist.
5. `Station::ysf_connect/_disconnect/_available/_state`, feature chain, C ABI,
   `just cbindgen`, Swift binding, `Network.ysf`.

## Where this stands

`crates/astar-ysf` exists and is tested: 62 tests, no dependencies beyond
`std`, one I/O module.

**The wire, confirmed rather than recalled.** Every number below was read out
of the deployed reference implementations and then verified locally before it
was written down as a test:

| | |
|---|---|
| Radio frame | 120 bytes: 5 sync (`D4 71 C9 63 4D`) + 25 FICH + 90 payload |
| `YSFD` | 155 bytes: tag, gateway/source/destination callsigns (10 each), one byte of counter and end-flag, then the frame |
| `YSFP` / `YSFU` | 14 bytes: tag plus a ten-byte, space-padded callsign |
| `YSFO` | 50 bytes; `YSFS` and `YSFI` are accepted and ignored |
| Linking | send three polls, poll every 5 s; the reflector's own poll coming back is the whole acknowledgement |
| FICH coding | four Golay (24, 12) blocks → rate-1/2 K=5 convolutional code → interleave across the 25-byte field |
| FICH CRC | poly `0x1021`, init `0x0000`, unreflected, final XOR `0xFFFF` — **not** D-Star's CRC |

Two of those cost real work and were worth writing out longhand rather than
copying: the Golay code is eight lines of long division that reproduce all
4,096 entries of the published table, and the interleave is
`(i / 5) * 2 + (i % 5) * 40`, which is the hundred-entry table every
implementation prints. A table says what; those say why.

**What the crate deliberately does not do** is decode the ninety payload
bytes. `Frame::payload` hands them over intact. Without a vocoder there is
nothing to check a payload parser against, and a decoder nothing calls is
worse than an honest gap — so `DataType::is_half_rate_voice` is in place as
the gate the vocoder work will hang off, and nothing pretends to hear
anything yet.

**Open questions this did not settle.** DN-only versus DN+VW is still open,
and so is DG-ID: the FICH carries it and `YsfFsm::set_options` can ask a YCS
reflector for a room, but nothing above decides which. Answering those needs
the session layer, not more protocol.

## Open questions

* DN only, or DN + VW?
* Does `ysf_available()` mean "a ThumbDV is attached" (same as D-Star) — yes,
  and it should reuse the same probe rather than growing a second one.
* Talker display: YSF frames carry a source callsign. Reuse the D-Star
  `talker` treatment (last-heard, cleared on teardown) rather than inventing a
  second presentation.
