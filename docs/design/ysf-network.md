# YSF — design

**Status:** designed, not built. Engine item `iax-e8a4`, client item `astar-e7b3`.
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

1. `crates/astar-ysf` — framing, FICH, link FSM, loopback reflector.
2. AMBE+2 RATEP word(s) + YSF frame packing in `astar-codec`.
3. `crates/astar-console/src/ysf.rs` — session, run loop, `SharedState`,
   `apply_audio`, and a `connect_with_stream` test seam.
4. `ConsoleSession` wiring — the §2.4 checklist, **including the audio fan-out**.
5. `Station::ysf_connect/_disconnect/_available/_state`, feature chain, C ABI,
   `just cbindgen`, Swift binding, `Network.ysf`.

## Open questions

* DN only, or DN + VW?
* Does `ysf_available()` mean "a ThumbDV is attached" (same as D-Star) — yes,
  and it should reuse the same probe rather than growing a second one.
* Talker display: YSF frames carry a source callsign. Reuse the D-Star
  `talker` treatment (last-heard, cleared on teardown) rather than inventing a
  second presentation.
