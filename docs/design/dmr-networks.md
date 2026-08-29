# DMR — design

**Status:** started. `crates/astar-dmr` holds the network taxonomy and the
consent gate; there is no protocol, no vocoder and no dial grammar yet. Still
last of the AMBE family by Rob's call — see "Where this stands" at the end.
**Read first:** `docs/design/adding-a-network.md`, then `ysf-network.md` — DMR
inherits the AMBE+2 vocoder work and almost nothing else.

## DMR is not one network

Every other network astar speaks is a thing you connect to. DMR is a **family of
independent networks** that happen to share a protocol, each with its own
operator, its own registration, its own talkgroup numbering and its own rules
about who may connect and with what.

| Network | Notes |
|---|---|
| **TGIF** | Small, permissive, straightforward registration. The right first target. |
| FreeDMR | Community-run, open. |
| DMR+ / IPSC2 | Reflector-and-talkgroup hybrid. |
| SystemX | |
| AmComm, VKDMR, FreeSTAR, ADN | Regional / smaller. |
| **BrandMeister** | The largest. Its own section below. |

Treating "DMR" as one `Network` case would be wrong in the same way treating
"D-Star" as one would have been if REF, XRF and DCS were separate operators
rather than protocols. **The network is part of the target**, not a detail of it.

## What makes DMR different from everything astar has done

Four things, and each one costs more than the protocol does.

1. **Identity is a registered numeric ID.** A DMR ID comes from radioid.net,
   is tied to a verified licence, and is not a callsign. A DMR ID is a
   **different credential**, with its own registration story and its own
   failure mode when absent or wrong. It must not be bolted onto the callsign
   field.

   **Settled (2026-08-29, astar-c9d2).** astar now carries both, side by side,
   in an "Operator" section at the top of Settings: `operatorCallsign` (stored
   under `m17.callsign`, the legacy key kept deliberately) and `dmrRadioID`
   (stored under `dmr.radioId`, digits only, validated by `RadioID`). Neither
   stands in for the other. This is the answer `nxdn-network.md` and
   `p25-network.md` asked for as well — one callsign plus one numeric ID, for
   all three networks, decided once.

2. **Per-network credentials.** Each DMR network wants its own account and a
   hotspot password/security key. That is a *set* of secrets, per network, and
   astar's rule is absolute: secrets are connect-time in-args only — never on a
   Station, never in a snapshot, event, error or log.

3. **Talkgroups, not reflectors.** A DMR target is a talkgroup number on a
   timeslot on a network. `ReflectorDial` has no shape for that, and the
   directory has no rows for it: hamcall-db publishes D-Star, M17, YSF, NXDN,
   P25 and URF, and **no DMR at all**. This is the only network of the four that
   needs directory work as well as engine work.

4. **Timeslots.** TS1/TS2 is a concept no other astar network has. It is not a
   module — it is closer to a channel — and the dial grammar has nowhere to put
   it today.

## The vocoder is the easy part

AMBE+2 half-rate on the AMBE-3000, the same as YSF DN and NXDN. If YSF ships
first, DMR's vocoder work is frame packing.

## The protocol

The MMDVM / homebrew repeater protocol (`DMRGateway`, `MMDVMHost`), UDP,
login-and-keepalive with a per-network password. Reference implementations are
G4KLX's. **Read them; do not recall the wire format.**

## BrandMeister: a consent gate, and why

**BrandMeister goes last, behind an explicit opt-in the operator has to tick.**

The reason is not that BrandMeister is bad. It is that BrandMeister is a private
network with its own acceptable-use policy, its own enforcement, and a documented
willingness to block accounts **permanently**. Operators have lost access over
conduct and over technical infractions. astar cannot promise that connecting
through a third-party softclient is within their terms, because that is their
call to make and not ours.

That is a genuine, specific risk to the *user's own network access*, created by
using our software. Presenting it plainly is the only honest option.

### What the gate looks like

* A checkbox in Settings, **off by default**, that must be ticked before
  BrandMeister appears as a dialable target at all.
* Beside it, plainly worded and not buried in a tooltip:

  > **BrandMeister enforces its own access rules.**
  > It is a private network. Its operators set the terms, decide what counts as
  > a violation, and have permanently blocked accounts — for conduct and for
  > technical reasons. astar is a third-party client and cannot tell you whether
  > connecting this way is within their rules. **If your access is revoked, that
  > is between you and BrandMeister.** Read their policy before you tick this.

  With a link to BrandMeister's own policy, so the operator reads the terms from
  the people who enforce them rather than our summary of them.
* No dark patterns in either direction: do not pre-tick it, and do not make it
  hard to find for someone who has read the terms and accepted them.

### What the gate is not

It is **not** a disclaimer that lets astar behave carelessly. astar should
identify itself honestly to any network it connects to, respect rate limits, and
never disguise itself as approved firmware. If BrandMeister asks third-party
clients not to connect, the right response is to not connect — not to hide the
request behind a checkbox the user clicked.

If that turns out to be their position, this design's answer is the same one
`p25-network.md` gives for IMBE: **leave it listed and refused, and write down
why.** A network astar declines to reach for a stated reason is a better product
than one it reaches dishonestly.

## Order of work

1. **Settle the identity question** (DMR ID vs callsign) — shared with NXDN and
   P25, and blocking all three.
2. **TGIF first.** Smallest, most permissive, simplest registration. It proves
   the protocol, the credential handling and the talkgroup dial grammar against
   a network that will not punish an operator for a bug in our client.
3. Directory work in hamcall-db: DMR talkgroups, per network, with a `dial.kind`
   that carries network + talkgroup + timeslot. Sources need finding — each
   network publishes its own list, and W0CHP's compiled lists are explicitly
   **not** reusable (see hamcall-db's NOTICE and the rejected-sources note).
4. FreeDMR, DMR+, SystemX as the shape settles.
5. **BrandMeister last**, behind the gate above, and only after checking their
   current position on third-party clients.

## Open questions

* ~~Does the dial grammar grow a network selector, or does each DMR network get
  its own `Network` case?~~ **Settled (2026-08-29): the selector.** One DMR
  `Network` case, with the network carried alongside the talkgroup as part of
  the address. A case per operator would be honest and unusable — eight
  segments in a picker for one protocol. `astar_dmr::DmrNetwork` is that
  selector, and `NetworkClass` groups the independently run networks together
  and holds BrandMeister apart, which is the only split that changes
  behaviour.
* Where does the timeslot live in `ReflectorDial`? A new kind, almost certainly,
  rather than stretching an existing one.
* What does BrandMeister's policy actually say about third-party clients today?
  **Check before building, not after.**

## Where this stands

`crates/astar-dmr` exists. It contains `network` and nothing else: no I/O, no
dependencies beyond `std`, and no ability to connect to anything.

| | |
|---|---|
| `DmrNetwork` | TGIF, FreeDMR, DMR+, SystemX, AmComm, VKDMR, FreeSTAR, ADN, BrandMeister — each with a UI `label` and a stable `slug` for dial grammar and saved configuration |
| `NetworkClass` | `Independent` (label: "Independent networks") and `BrandMeister`. The one distinction with teeth |
| `dialable(consented)` | The gate, written once so no call site can forget it. BrandMeister is absent unless the operator has opted in |
| Identity | Settled: callsign and radio ID are two fields, neither standing in for the other |

**What it deliberately does not hold: master hostnames, ports or passwords.** A
password is a per-network secret and astar's rule is absolute — connect-time
in-arg only. Endpoints are directory data with their own sourcing problem (see
"Order of work" item 3) and they move; a hostname compiled into the engine is a
hostname that goes stale in a shipped binary.

**Why the taxonomy came before the wire.** DMR is the one network where the
address is the hard part. A talkgroup number names nothing on its own — TG 91
exists on several of these networks and is a different room on each — so what a
target *is* had to be settled before any wire code could be written against it.

**Next, in order:** the MMDVM/homebrew login-and-keepalive against TGIF, read
out of the reference implementations and verified rather than recalled; then
AMBE+2 frame packing, which YSF's vocoder work pays for; then the talkgroup
dial grammar and the timeslot, which still has nowhere to live in
`ReflectorDial`. The BrandMeister policy check in "Open questions" is still
outstanding and still blocks that network specifically — the gate is built, the
question of whether the gate should ever open is not answered.
