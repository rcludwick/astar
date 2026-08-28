# P25 — design

**Status:** designed, not built. Client item follows `astar-b8e4`'s shape; no
engine item exists yet.
**Read first:** `docs/design/adding-a-network.md`.

## The one question that decides this

**P25 Phase 1 is IMBE, not AMBE+2 — and it is not yet established that astar's
dongle can do it.**

Everything else here is routine. This is not, and it is the reason P25 comes
after YSF and NXDN rather than alongside them: those two are the same vocoder as
each other and a RATEP word away from D-Star's. P25 may be a different vocoder
entirely, and if it is, the honest options are narrow.

`vendor/ambe-thumbdv` is a **DVSI AMBE-3000** packet driver (`VENDORED.md`). The
AMBE-3000 is an AMBE+2 part; it is configured for D-Star's AMBE 2020 by
`packet::ratep_dstar()`, which is what proves RATEP can select a legacy mode.
Whether it will also select **IMBE** is exactly the unknown.

### The gating experiment

Cheap, and it must happen before any P25 code is written:

1. With the ThumbDV free (astar not holding it), query the chip:
   `packet::prodid_query()` and `packet::verstring_query()` — both already exist
   in the driver.
2. Take the reported product and firmware to the DVSI AMBE-3000 documentation and
   read the RATEP table for an IMBE entry.
3. If there is one: P25 is a third RATEP word and this becomes as cheap as NXDN.
4. If there is not: see below.

Do this on a dongle nobody is using. The probe opens the serial device, and
opening it mid-QSO would interrupt a live contact.

### If the AMBE-3000 cannot do IMBE

Three options, in order of preference:

* **List but do not dial.** P25 rows stay in the directory, render disabled with
  the reason "astar can't dial p25 yet", and nothing is claimed that is not true.
  This is already how the client behaves for unsupported kinds and it costs
  nothing — it is where P25 sits *today*.
* **Different hardware.** A DVSI part that does IMBE, gated on its own
  capability flag. A second dongle class is a real support burden; worth it only
  if P25 demand justifies it.
* **A software IMBE decoder.** The obvious candidate is `mbelib`, and it is
  **legally grey** — the algorithms are patented and its distribution status is
  contested. astar's position on this is already settled for D-Star: hardware
  only, no software AMBE, no `ambe-soft`, and the old feature was *removed*
  rather than left off. Applying a different standard to IMBE would undo that
  decision quietly. **Do not take this option without an explicit, recorded
  decision from Rob.**

## What the data already tells us

From the published directory, 2026-08-28:

| | |
|---|---|
| Rows | 314 |
| `dial` fields | `host`, `port`, `kind` — no callsign, no modules |
| Ports | 41000 (240), 41001 (22), 41002 (13), 41003 (11), tail |
| Ids | numeric talkgroups (`100`) |

Same shape as YSF and NXDN: bare endpoint, no module, `host:port` identity,
talkgroup-numbered ids that collide across networks.

## The protocol

P25Reflector, UDP, poll-based. Reference: G4KLX's `P25Clients`. Read it; do not
recall it.

Structurally this is the least interesting part of P25 — it is the third
poll-based reflector protocol in a row, and if YSF and NXDN are done first, the
session module is largely a transcription.

## Identity

P25 addresses radios by numeric **radio ID**, not callsign. The same question
NXDN raises, and probably the same answer — resolve it once, for both, rather
than twice differently.

## Order of work

1. **The gating experiment above.** Nothing else starts until it answers.
2. If IMBE is reachable: protocol crate, RATEP word, session module, §2.4
   wiring, facade, ABI, binding, `Network.p25`.
3. If it is not: write the answer down in this file and in the backlog, leave the
   rows listed-and-refused, and stop. That is a legitimate outcome and a much
   better one than a half-working network or a licence problem.
