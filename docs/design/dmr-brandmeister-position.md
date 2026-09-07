# BrandMeister's position on third-party clients — checked 2026-09-07

**What this answers.** `dmr-networks.md`'s open question: *"What does
BrandMeister's policy actually say about third-party clients today? Check
before building, not after."* This is that check, done before Task 10's picker
and before any BrandMeister-specific code exists. Nothing here changes the
gate's default — it is off in every outcome — and no BrandMeister-specific
behaviour was implemented to produce it.

**Read first:** `dmr-networks.md`'s "BrandMeister: a consent gate, and why"
and "What the gate is not", and `p25-network.md`'s "If the AMBE-3000 cannot do
IMBE" — the "leave it listed and refused, and write down why" precedent this
file follows if the ruling calls for it.

## The fetch log

Every URL below was fetched on **2026-09-07** with the WebFetch tool. Per the
task brief, `wiki.brandmeister.network` was expected to answer with an Anubis
anti-bot interstitial rather than the page — that is a recorded result, not a
gap to work around. No bot check was bypassed to get past it, by tooling or by
guessing alternate paths.

| URL | Result |
|---|---|
| `https://wiki.brandmeister.network/index.php/Homebrew_repeater_protocol` | **Anubis interstitial.** "Access Denied: error code 9e4edb5b6b850c41", served by Anubis v1.26.2 (Techaro). No article content returned. |
| `https://wiki.brandmeister.network/index.php/Open_DMR_Terminal_Protocol` | **Anubis interstitial.** Same access-denied page, same mechanism. No article content returned. |
| `https://wiki.brandmeister.network/index.php/Boxchip` | **Anubis interstitial.** Same access-denied page. No article content returned. |
| `https://help.brandmeister.network/` | **Rendered.** Full section index returned (Operating, Repeaters, Hotspots, SelfCare Dashboard, Last Heard Dash, Talk Groups, Bridges — see below for what was read). |
| `https://news.brandmeister.network/` | **Rendered.** Five recent articles (RadioID.net verification, repeater-description markup, US Radio ID renumbering, Master 2001 going HAMNET-only, TetraPack at HamRadio 2024). None address client software policy. |
| `https://brandmeister.network/` | **Rendered, but empty of content.** The response carried only the page title "BrandMeister" — no body text and no extractable links. This reads as a JavaScript single-page app that WebFetch's HTML-to-markdown conversion could not execute; it is not an access-denial page like the wiki, just a shell with nothing in it to quote. No terms-of-use or acceptable-use link could be found this way. |

Within `help.brandmeister.network`, the following sections were opened in
addition to the index, on the theory that a hotspot- or repeater-connection
page was the most likely place to find a restriction on who may use the
Homebrew/MMDVM protocol:

| Page | Result |
|---|---|
| Connecting Hotspots | Rendered. Documents the setup flow (Radio ID, Dashboard account, Hotspot Security Password since March 2021) for Pi-Star/MMDVM, openSPOT, and BlueDV/DVMega. All three are hardware hotspot platforms; nothing here states a restriction against software-only clients. |
| Connecting Repeaters | Rendered. States BrandMeister accepts "commercial repeaters, currently Motorola and Hytera, and MMDVM-based repeaters," with Radio ID and alias requirements. No explicit statement about RF-module requirements or software-only clients. |
| Operating Etiquette | Rendered. On-air conduct only (courtesy, language, talk-group use). No mention of client software. |
| Repeater Connection Issues | **404 Not Found.** Page does not exist at the guessed path; not followed further since it was not one of the brief's listed URLs. |

**No page BrandMeister itself served, of the ones that rendered, contains a
direct statement about software-only clients, "Network Radio," Radio-over-IP,
or the Open DMR Terminal Protocol.** The three pages most likely to carry that
statement by name — the wiki articles for the Homebrew protocol, the Open DMR
Terminal Protocol, and Boxchip — are exactly the three the Anubis wall blocked.

A human with a browser can very likely read all three wiki pages in under a
minute; Anubis's proof-of-work interstitial is designed to pass a real browser
transparently and only blocks scripted fetches. Task 2 did not use a browser
automation tool to get past it, because doing so is exactly the "work around
it" the brief rules out for an agent. If this file is revisited, reading those
three pages directly is the highest-value next step and could change the
ruling below from (b) to (a).

## What BrandMeister's own rendered material says

Nothing, on the specific question. The help site's hotspot- and
repeater-connection pages describe *how* to connect MMDVM/Pi-Star hotspots and
Motorola/Hytera/MMDVM repeaters — implicitly hardware-oriented audiences — but
none of the pages that actually rendered contains a sentence ruling
software-only clients in or out, naming the Open DMR Terminal Protocol as the
required path for them, or otherwise taking the position the brief's outcome
(a) or (c) would need quoted. The news site and homepage are silent on the
question for the same reason: they simply don't discuss it, not because they
rule it out.

## Community reports — not BrandMeister speaking

Everything in this section is third-party commentary, forum recollection, or
unofficial software documentation. None of it was published by BrandMeister on
a page this task could render, so none of it is treated as BrandMeister's
position for the ruling below. It is recorded because it bears directly on the
question and because pretending it doesn't exist would be its own kind of
dishonesty.

**RadioReference forum, "Droid-star for dmr"**
(`https://forums.radioreference.com/threads/droid-star-for-dmr.415480/`,
active since October 2020):

- One forum user, paraphrasing: *"BrandMeister has requested that pure 'Radio
  over IP' or 'Network Radio' devices connect to the server using
  BrandMeister's 'Open Terminal Protocol' but DROID-Star doesn't do this, and
  mimics the connection request from a Pi-Star hotspot."*
- Another forum user, presented as a direct quotation of BrandMeister's stated
  policy (source not independently verified by this task — it reads as a
  copy from the wiki or a BrandMeister admin post this task could not reach):
  *"Connecting to Brandmeister with the protocols Homebrew and MMDVM Hosts is
  reserved for repeaters and hotspots with an onboard radio (RF) module."*
- A separate user: *"Brandmeister has blocked access from Droidstar/Dudestar.
  If anyone mentions either one of those, in their Telegram support forum,
  just grab a soft drink and some popcorn and enjoy the show."*
- Other users report DroidStar/DUDE-Star connection failures specifically
  against BrandMeister servers while the same client worked against TGIF and
  FreeDMR, and recommend MMDVM hotspots, DVSwitch, or Open Terminal
  Protocol-based tools as the working alternative. One thread participant
  called BrandMeister's blocking "rightfully so."
- No BrandMeister staff post was found or quoted in this thread; every
  statement above is a forum member's account.

**`abo4/pyspot_rx`** (`https://github.com/abo4/pyspot_rx`) — a third-party
Python client for BrandMeister's Open Terminal Protocol. Its documented
requirements are a DMR ID, a BrandMeister SelfCare account with a hotspot
password, and a **DVMEGA DVstick 30** (an AMBE hardware dongle) — i.e. this
Open-Terminal-Protocol tool still expects a hardware vocoder, not a purely
software client. It states no BrandMeister policy of its own.

**Other third-party write-ups** (`redfast00`'s blog post on building a
BrandMeister DMR bridge, and the `redfast00/brandmeister-dmr-opendmr` bridge
README) describe the Open DMR Terminal Protocol as the intended path for
non-hotspot integrations (in that case, a talkgroup-to-Mumble bridge) but
quote no BrandMeister policy text and cite no BrandMeister source for the
distinction. They read as the authors' own understanding of the ecosystem, not
as BrandMeister's own words.

**Reading the community evidence honestly:** it is consistent and points one
way — several independent sources describe BrandMeister restricting the
Homebrew/MMDVM login to hardware repeaters and hotspots, directing
software-only ("Network Radio") clients to the Open DMR Terminal Protocol
instead, and blocking at least one softclient (DroidStar/DUDE-Star) over
exactly this. That is a real pattern. But every instance of it in this task's
research is secondhand — a forum member's recollection, a paraphrase, or a
quotation this task could not check against the page it is supposedly copied
from, because that page is the one behind the Anubis wall. The brief's
evidentiary bar for the ruling is BrandMeister's own rendered material, and on
that bar this evidence does not qualify.

## Ruling

**Outcome (b): BrandMeister publishes no position either way, on the material
this task could actually read.** The wiki pages most likely to hold
BrandMeister's own statement — Homebrew_repeater_protocol,
Open_DMR_Terminal_Protocol, Boxchip — were unreachable behind Anubis on
2026-09-07. Every other page in the brief's list that did render
(`help.brandmeister.network` in full, `news.brandmeister.network`,
`brandmeister.network`) was read and contains no statement on software-only
clients, Network Radio, Radio-over-IP, or the Open DMR Terminal Protocol in
either direction.

The wiki was unreadable, and nothing else published by BrandMeister speaks to
the question — so this is outcome (b), with the fetch log above as the
evidence. The consent gate stands as designed in `dmr-networks.md` — off by
default, one explicit opt-in, plain wording, no dark patterns — and this file
is the evidence that the question was asked rather than assumed away.

**This is not the same as a clean bill of health.** The community-reports
section above is consistent enough, across independent sources spanning
several years, that this task's honest assessment is: the real answer is
probably outcome (a), and the wiki page this task could not read is very
likely where BrandMeister says so in their own words. A human reading
`wiki.brandmeister.network/index.php/Open_DMR_Terminal_Protocol` and
`.../Homebrew_repeater_protocol` directly in a browser — which defeats
Anubis's proof-of-work check the way it's meant to be defeated, by being an
actual browser — is the fastest way to either confirm outcome (a) and close
the gate permanently, or refute it. Until that happens, the gate's copy says
plainly that astar looked and could not read BrandMeister's wiki, so nobody
downstream mistakes silence for permission.

## What changes because of this

Nothing behavioural. `DmrNetwork::BrandMeister` still exists, `dialable()`
still gates it exactly as before, and the default is still off. The only
changes are documentation: the gate's copy in `dmr-networks.md` now says
astar looked and what it found, and the "Open questions" bullet points here
instead of standing open.

**Follow-up worth a backlog line:** have a human open the three wiki pages
above in an ordinary browser and report back what they say. If they confirm
the community reports' account, this file's ruling flips to (a) and
`dmr-networks.md`'s gate becomes a permanent closure for the Homebrew login,
per the `p25-network.md` "leave it listed and refused" precedent — with a link
to BrandMeister's own wording instead of this task's secondhand account of it.
