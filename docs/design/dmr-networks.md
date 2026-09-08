# DMR — design

**Status:** receive shipped; transmit gated (Tasks 12–13 of
`docs/superpowers/plans/2026-09-07-dmr-network.md`). `dmr-wire.md` is the wire
itself, read out of the references. astar can log in to a DMR master, join a
talkgroup on a timeslot and decode the voice on it, on every client; it has no
DMR transmit path and offers no PTT for one — see "Where this stands" at the
end. **Read first:** `docs/design/adding-a-network.md`, then `ysf-network.md`
— DMR inherits the AMBE+2 dongle plumbing and almost nothing else, and NOT the
vocoder configuration (see "The vocoder is not YSF's" below).

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

## The vocoder is not YSF's — a correction

**This section said "AMBE+2 half-rate on the AMBE-3000, the same as YSF DN and
NXDN", and that was wrong.** It is the same chip, not the same configuration.
DMR runs the AMBE-3000 at **2450 + 1150 — nine bytes, 72 bits per frame, with
the chip's own FEC in the second number** — where YSF DN and NXDN run
2450 + 0000, seven bytes, no FEC. The RATEP control packet differs, the
channel packet length differs, and a frame from one mode handed to the other
is not silence: it is the wrong vocoder's bits, transmitted.
`dmr-wire.md` §9 has the constants and the reference lines they come from.

That is why `VocoderMode` gained a `Dmr` case rather than reusing `YsfDn`, and
why `astar_codec`'s DMR path is not YSF's with a different frame count. What
YSF's work did pay for is everything around the vocoder — the dongle scan, the
init cookbook, the worker thread, the stream accounting — which is most of the
cost and none of the risk.

**One thing here is still a stand-in.** `null_frame(VocoderMode::Dmr)` in
`crates/astar-codec/src/ambe.rs` substitutes D-Star's null codeword for an
encode request the device never answered — cross-mode reuse of exactly the
kind the paragraph above warns about. It is tolerable only because **nothing
transmits DMR**: the receive path never calls it. The transmit task owes a
real silence frame read out of the reference (`MMDVMHost/DMRDefines.h`'s
silence pattern), with the hardware session as the arbiter if the two
disagree.

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

**Checked, 2026-09-07** (`dmr-brandmeister-position.md`): BrandMeister's wiki —
the most likely place for them to state a position on the Homebrew/MMDVM
login versus the Open DMR Terminal Protocol — answered every fetch with an
Anubis anti-bot interstitial, not the page. Every other page of theirs that
did render (`help.brandmeister.network` in full, their news site, their
homepage) states no position either way. That is outcome (b): no published
BrandMeister position astar could confirm, so the gate stands exactly as
designed below, off by default. It is not a clean bill of health — several
independent third-party accounts (forum threads, softclient documentation)
describe BrandMeister restricting the Homebrew login to hardware repeaters and
hotspots and blocking at least one softclient over it, consistently enough
that the real answer is probably a **permanent closure**, per the IMBE
precedent below. Nobody has read BrandMeister's own wording of it yet; a human
with a browser can, in about a minute, where an automated fetch could not.

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

### What the gate is, on this branch — narrower than designed

The gate above is the design. What shipped is **stricter**, and deliberately:

| | |
|---|---|
| Engine | `Station::dmr_connect` refuses BrandMeister **unconditionally**. `BRANDMEISTER_CONSENTED` in `crates/astar-station/src/station.rs` is a `const false`, checked through `astar_dmr::dialable` before a socket or a dongle is touched, and there is no preference, config field or C ABI in-arg that can set it. |
| App | No consent control is shown. The persisted flag (`dmr.brandmeisterConsent`) exists and defaults off, nothing in the UI sets it, and while it is off the directory hides BrandMeister's masters. A checkbox shipped briefly and was removed the same day: **this build cannot reach BrandMeister**, and a control that recorded an intent it could not honour was a promise the next connect broke. |

That is the honest state of a finding that is outcome (b) rather than a clean
answer: the gate is built and closed, and opening it is its own piece of work
with its own in-arg, its own ABI and its own UI. **What is owed first is a
human read of BrandMeister's own three wiki pages** — a browser gets in where
an automated fetch met an anti-bot interstitial. If that read confirms (b),
thread a consent in-arg through `dmr_connect` → C ABI → Swift →
`CallSession`. If it turns out they ask third-party clients not to connect,
the answer is the one below: leave it listed and refused, and write down why.

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

1. ~~**Settle the identity question**~~ (DMR ID vs callsign) — **done**
   (astar-c9d2): two fields, neither standing in for the other.
2. ~~**TGIF first.**~~ **Done for receive.** Smallest, most permissive,
   simplest registration; it proved the protocol, the credential handling and
   the talkgroup dial grammar against a network that will not punish an
   operator for a bug in our client. It is still the **only** live target
   sanctioned for the first transmit test.
3. Directory work in hamcall-db: DMR talkgroups, per network, with a
   `dial.kind` that carries network + talkgroup + timeslot. **Half-done**: the
   feed now publishes 185 DMR *server* rows (see the correction below) and the
   app consumes them, including per-system talkgroup lists where a row has
   them. What is still missing is talkgroup coverage for the networks whose
   rows carry none, and TGIF, which publishes no server rows at all. Sources
   need finding — each network publishes its own list, and W0CHP's compiled
   lists are explicitly **not** reusable (see hamcall-db's NOTICE and the
   rejected-sources note).
4. ~~FreeDMR, DMR+, SystemX as the shape settles.~~ **Done**: the shape is one
   `Network.dmr` with the system carried in the address, so every independent
   network came at once.
5. **Transmit** — Tasks 12–13, fenced until Rob confirms a clean YSF parrot
   round trip, then proven against `just dmr-parrot` on 127.0.0.1 before TGIF
   and nothing else.
6. **BrandMeister last**, behind the gate above, and only after a human has
   read their current position on third-party clients.

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
* ~~What does BrandMeister's policy actually say about third-party clients
  today? Check before building, not after.~~ **Checked (2026-09-07):**
  BrandMeister's own material that could be read that day states no position
  either way; see `dmr-brandmeister-position.md` for the fetch log, why the
  wiki couldn't be read, and the community evidence that makes this an open
  question worth revisiting rather than a closed one.

## The directory's system slug is not the engine's family slug — a correction

`astar_dmr::network`'s doc says a slug is "the stable identifier used in dial
grammar, **directory rows** and saved configuration". For dial grammar and
saved configuration that is true. **For directory rows it is not, and cannot
be.**

Checked against the live feed on 2026-09-07
(`api/v1/reflectors/dmr.json`, 185 rows, DVRef via CC BY 4.0): the rows carry
**111 distinct `system` values** — `freedmr-network`, `dmrplus-ipsc2-uk`,
`ipsc2-poland`, `adn-systems-espana`, `hb_it_trani_conference`, `xlx696` — and
not one of them equals a `DmrNetwork` slug. The reason is structural, not a
data-quality problem: **DVRef enumerates servers**, one row per operator
instance (19 rows are `freedmr-network`), while **`DmrNetwork` enumerates
families**. Neither vocabulary can be derived from the other by renaming.

**The ruling: they are two different things and both are kept.**

| | |
|---|---|
| `ReflectorDial.mmdvm(system:host:port)` | carries the directory's `system` **verbatim** — it is what names the master, its login and its upstream talkgroup list, and it is the key the master password is saved under |
| `DmrFamily` (the app's mirror of `DmrNetwork`) | what the picker groups by and what the consent gate reads |
| `DmrDial.family(ofSystem:)` | the documented bridge, a prefix/alias table with the 2026-09-07 evidence in its doc comment |
| `DmrNetwork::from_system_slug` | **the same bridge on the Rust side**, over the same table in the same order — added 2026-09-07 after review found the app's rows could not connect |

**Both sides of the ABI need the bridge, and for a while only one had it.**
The app resolved a family for its picker and its consent gate, then handed
`connectDMR(system:)` the directory's `system` verbatim — which is right —
but `Station::dmr_connect` resolved that string with `DmrNetwork::from_slug`
alone and refused everything that was not one of the nine family slugs. Every
one of the 185 directory rows was therefore listed, grouped, dialable in the
UI and refused at the engine with "unknown DMR network"; only a hand-typed
`tgif:tgif.network:62031/31313/2` worked. The fix is the ruling above applied
in Rust: **`system` is a name, not an enumeration.** `dmr_connect` accepts any
non-empty string, resolves the family through `from_slug` then
`from_system_slug`, and dials with `family: None` when neither answers.
BrandMeister is the one refusal, and it is checked twice — the resolved family
AND the raw spelling, so `brandmeister3102` cannot slip past the separator
rule. `DmrConfig` carries both halves: `system: String` (what was dialed) and
`family: Option<DmrNetwork>` (what the engine made of it).

The two tables are twins and must stay in step. Adding a spelling to
`familyNames` in `ReflectorAddressDial.swift` without adding it to
`SYSTEM_NAMES` in `crates/astar-dmr/src/network.rs` makes the picker and the
gate disagree about which rows are BrandMeister; each table's doc comment says
so, and both are tested against the same rows. The engine carries one extra
belt the app does not: any system whose lowercased spelling *starts with*
`brandmeister` is treated as BrandMeister at the gate even when the separator
rule resolves no family, so a hand-typed `brandmeister3102` is refused by the
engine while the picker showed it as an unrecognised independent. Fail-safe by
design; do not "fix" the app to match.

`family(ofSystem:)` answers **`nil` for "independent, unrecognised"** rather
than guessing. Most rows answer `nil` — 111 systems against nine families —
and every one of them is still listed, still grouped (under "Independent
networks") and still dialable. The only thing a family decides is
`requiresConsent`, and a network astar does not recognise is not BrandMeister.

One more consequence worth stating: **30 of the 185 rows have no `dial` at
all.** They are listed and not dialable, which `ReflectorDial.unsupported`
already models — dropping them would make a directory that lists 185 networks
look like one that lists 155.

`astar_dmr::network`'s own doc comment used to claim a slug was "the stable
identifier used in dial grammar, **directory rows** and saved configuration".
Two of those three were true; it now says so, and points at
`from_system_slug` for the third.

### TGIF has no server rows

**TGIF is not in the directory at all.** No row's `system` contains `tgif` —
TGIF publishes talkgroups to DVRef but not servers. So astar's first and
recommended target is reachable only by typing an address, which is not a gap
to work around but the reason the manual form exists:

```
tgif:tgif.network:62031/31313/2
```

That address is **documentation, and a placeholder in the app's help text —
never a constant in the engine**. `crates/astar-dmr` holds no hostname and no
port for any network, for the reason "Where this stands" gives: endpoints are
directory data, they move, and a hostname compiled into a shipped binary goes
stale where nobody can fix it. `dmr-listen`'s usage text and this document are
where the string lives; `grep -r tgif.network crates/` finds it only as an
argument in `dmr_listen.rs`'s parser tests — a string typed at the CLI, not a
default anything falls back to.

## Where this stands

**astar hears DMR.** `crates/astar-dmr` is a full client of the MMDVM/homebrew
repeater protocol, and every layer above it is wired through: the codec, the
session, the facade, all three bindings, the macOS app and the CLI. The Iced
client has no DMR, for the same reason it has no D-Star or Fusion — it has no
digital voice at all yet (`astar-guidv` in `docs/app/BACKLOG.md`), and half a
picker entry would be worse than none.

| | |
|---|---|
| `network` | `DmrNetwork` — TGIF, FreeDMR, DMR+, SystemX, AmComm, VKDMR, FreeSTAR, ADN, BrandMeister — each with a UI `label` and a stable `slug`; `from_system_slug`, the bridge from a directory row's server name to its family; `NetworkClass` (`Independent` / `BrandMeister`), the one distinction with teeth; `dialable(consented)`, the gate written once so no call site can forget it |
| `wire` | `RPTL`/`RPTK`/`RPTC`/`RPTPING`/`RPTCL` and the 55-byte `DMRD`, built and parsed by definition from the references (`dmr-wire.md` §1–§4); `RadioId` (24 bits, 0 refused) and `Timeslot` |
| `fsm` | The login state machine — salt, `SHA256(salt ‖ password)`, config, ping/pong, timeouts and retries — with the password moved in and dropped after one digest |
| `fec` | BPTC(196,96) and BPTC(128,77), Hamming (16,11,4)/(13,9,3)/(15,11,3), Golay(20,8), QR(16,7,6), Reed–Solomon(12,9) and the 5-bit embedded-LC checksum — each written from its definition rather than transcribed (§8) |
| `frame` | The 33-byte burst: sync patterns, EMB, embedded LC across bursts B–F, full LC in the header and terminator, and the three 9-byte AMBE frames |
| `master` | A real master for the bench — `just dmr-parrot` — that binds, runs the whole handshake, and relays or replays verbatim. It never dials anything |
| `astar-codec` | `VocoderMode::Dmr`: 2450 + 1150, nine bytes, on the same `ThumbDV` — see the vocoder correction above |
| `astar-console` | `DmrLink` + `DmrSnapshot`, receive-only, on the one shared audio lane; `Failed` published on a timeout and on a send failure, with the step it failed at |
| `astar-station` | `dmr_connect(system, host, port, radio_id, callsign, talkgroup, timeslot, password)` — the password by value, any non-empty `system` (family slug or directory server name), the consent gate checked before a socket or a dongle is touched, mutual exclusion with every other network |
| C ABI / Swift / Python | `IAX_ERR_DMR = -22`, `iax_station_connect_dmr` / `iax_station_dmr_disconnect` / `iax_station_dmr_state`, and the snapshot's `dmr_available` / `dmr_active` — mirrored in all three bindings |
| macOS app | `Network.dmr` in the switcher, the `system:host[:port]/tg[/ts]` dial grammar, the directory's 185 rows grouped by family, the master password, and the consent checkbox |
| `astar-cli` | `dmr-listen` — the hardware checkpoint in one command, `--features dmr` |

**Receive only, and PTT is refused rather than stubbed.**
`Station::set_ptt` refuses a key-down while a DMR link is live, the snapshot's
`ptt` is always false, `canTransmit` is false in the app so no key appears,
and `dmr-listen` has no PTT reader at all. Transmit is Tasks 12–13 of the
plan, **fenced until Rob confirms a clean YSF parrot round trip on the fixed
build**: the AMBE encode path is shared, and the end-to-end proof that a
transmission is intelligible is his checkpoint, not an agent's. DMR raises one
bar higher than YSF or NXDN did — a master routes by the radio ID astar logged
in with, so a malformed burst is attributable to a licence. The first
transmission goes to `just dmr-parrot` on 127.0.0.1; the first live one goes
to TGIF and nothing else.

**Verified as bytes, not as sound.** The crate's own suites, the loopback
master and the session pipeline prove bytes in and bytes out. Whether the
result is speech needs a `ThumbDV` and a live master, and both "point this at
a real master" and "listen to what comes out" are Rob's:

```
just dmr-parrot 62031                                        # terminal 1
ASTAR_DMR_PASSWORD=… just dmr-listen 127.0.0.1:62031 tgif <CALL> <ID> 31313
```

The macOS DMR UI has not been seen on screen with a dongle attached either;
that is tracked in `docs/app/BACKLOG.md`.

**What it deliberately does not hold: master hostnames, ports or passwords.**
A password is a per-network secret and astar's rule is absolute — connect-time
in-arg only, and `dmr-listen` reads it from `ASTAR_DMR_PASSWORD` rather than
`argv` for the same reason. Endpoints are directory data with their own
sourcing problem (see "Order of work" item 3) and they move; a hostname
compiled into the engine is a hostname that goes stale in a shipped binary.

**Why the taxonomy came before the wire.** DMR is the one network where the
address is the hard part. A talkgroup number names nothing on its own — TG 91
exists on several of these networks and is a different room on each — so what a
target *is* had to be settled before any wire code could be written against it.
The correction above is the same lesson arriving from the other direction: the
directory names servers where the engine names families, and the app bridges
them rather than either side pretending to be the other.

**Still owed, in order:** the human read of BrandMeister's wiki, and the
consent in-arg behind it; transmit (Tasks 12–13, fenced); talkgroup lists for
the networks whose directory rows carry none, and for TGIF, which has no rows;
the real DMR silence frame; and the small corrections tracked in
`docs/BACKLOG.md` under `iax-d4f7`.
