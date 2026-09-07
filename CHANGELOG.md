# Changelog

Notable changes to astar. Newest first.

Since `0.1.1beta` there is a signed, notarized macOS `astar.dmg` on the
[releases page](https://github.com/rcludwick/astar/releases/latest); everything
else is still built from source. Versions stay on `beta` until the client has
had a real sit-down-and-use-it pass on all three platforms.

From `0.1.10-beta` onward the version is real SemVer — `MAJOR.MINOR.PATCH-beta`,
with a hyphen. Everything up to and including `0.1.9beta` glued the
pre-release to the patch number, which sorts wrongly: `0.1.10beta` is newer
than `0.1.9beta` and a string comparison says the opposite. Shipped versions
are left as they were spelled.

## 0.1.11-beta — 2026-09-07

System Fusion is the third digital network astar speaks: link a reflector,
hear it, and key back into it with a ThumbDV. The node daemon now runs
wideband by default, refuses a peer it cannot decode instead of failing
silently, and finally writes a log. And every digital network shares one
audio lane, so the meters, the spectrum and your microphone settings mean
the same thing whichever network is live.

### Added

- **Last heard: the popover names whoever keyed up on M17 and System Fusion,
  as it already did on D-Star.**

- **System Fusion (YSF): link a reflector, hear it, and transmit**, with the
  ThumbDV in DN mode. YSF sits beside AllStarLink, M17 and D-Star in the
  network switcher and gets everything the others get — its own audio
  profile, level meters, spectrum, last-heard, a timeline that records your
  own overs, and a live link that reports as connected rather than looking
  like a failed one.

  One dongle means one direction at a time. While you are keyed, received
  voice is not decoded — there is a single AMBE-3000 behind the ThumbDV, and
  D-Star has always made the same trade. Callsigns cost no vocoder, so
  last-heard stays truthful while you talk; a busy reflector simply goes
  quiet for the length of your over.

  **Receive is verified on live reflectors; transmit so far only against a
  parrot on the bench.** D-Star and M17 transmit are the ones verified on
  live reflectors.

- **`ysf-listen` and `ysf-parrot`, for the bench.** `just ysf-listen
  <host> <callsign>` links a reflector and plays it — or captures it with
  `--wav` — which is the hardware checkpoint in one command. `just
  ysf-parrot <port>` runs a System Fusion parrot on your own machine, the
  twin of `just m17-parrot`: key into it from astar and hear yourself, with
  nothing on the air. Frames are relayed verbatim, so it hides no framing
  bug.

- **DMR in the bundled reflector directory.** The 2026-09-07 snapshot
  carries 3,415 reflectors across seven networks, now including hamcall-db's
  DMR masters — 185 servers, named and counted. astar has no DMR client yet,
  so they are listed rather than dialable.

- **The node daemon has a voice.** `astar-server` depended on `tracing` and
  never installed a subscriber, so 35 log sites in the engine dispatched to
  nothing and a hub could run for days emitting one line. It now installs
  one — `info` by default, `RUST_LOG` overrides, timestamps and no ANSI
  because this lands in journald — and codec negotiation prints a line per
  inbound call naming the caller, what it can carry, what it asked for and
  what was accepted.

- **DMR: log in to a talkgroup and hear it.** Point astar at a DMR master —
  TGIF, FreeDMR, DMR+, SystemX and the rest — and it speaks the MMDVM/homebrew
  repeater protocol, joins a talkgroup on a timeslot, and decodes the AMBE+2 on
  it through the same ThumbDV D-Star, Fusion and NXDN use. The network switcher,
  the directory's 185 DMR rows, and last-heard. Your radio ID and each
  network's own password are separate credentials and stay that way; the
  password is used to log in and never stored anywhere else. BrandMeister is
  listed and not dialable: it is a private network whose operators set the
  terms, and astar has not confirmed where they stand on third-party clients.
  Receive only for now: astar has no DMR transmit path yet and says so instead
  of offering a PTT button that would do nothing.

- **NXDN: link a talkgroup and hear it.** Point astar at an NXDNReflector
  talkgroup and it decodes the AMBE+2 on it through the same ThumbDV D-Star
  and Fusion use — the network switcher, the directory's 297 NXDN rows,
  and last-heard. Receive only for now: astar has no
  NXDN transmit path yet and says so instead of offering a PTT button that
  would do nothing.

### Changed

- **astar-server prefers signed 16-bit linear by default** and rejects a peer
  with no usable codec instead of accepting one it cannot decode; µ-law-only
  nodes are still accepted, and the reject carries Asterisk's own CAUSE and
  CAUSECODE. In the other direction, a peer that answers with a format the
  station never offered is hung up on rather than transmitted to: a `ulaw_only`
  node no longer ends up sending wideband because the far end asked for it.

- **One audio lane**: M17, D-Star and System Fusion now share the station's
  single audio router; level meters and spectrum are computed once at the
  microphone and the speaker for every network.

- **The docs say a licence is required before first contact.** The page
  someone follows from download to first QSO listed a Mac, an account and a
  headset and stopped there, while every step on it ends with a real
  transmitter keying on a real band. The DVstick 30 is also documented as
  known-working rather than buy-at-your-own-risk: it enumerates as the same
  FT230X the dongle scan looks for, and it is what astar is developed
  against day to day.

### Fixed

- **The beat on System Fusion receive.** YSF delivers five 20 ms frames at
  once every 100 ms, and handing each burst straight to the output bus left
  it starved for the remaining 60 — roughly two audible gaps a second, worse
  the longer somebody talked. Decoded audio is now released one frame per
  20 ms behind a small cushion, the same arrangement D-Star uses, at the same
  cost of about 60 ms of latency.

- **Garbled System Fusion transmit.** The encoder dropped every microphone
  frame past the fourth in a pass, which is dropped speech, and the last
  frames of an over were lost or arrived during the *next* one as somebody
  else's audio. Captured frames are now queued the way received ones already
  were, and the encoder is drained before the over closes.

- **A rejected inbound call — bad CALLTOKEN, failed auth, or no common codec —
  no longer leaves a call number and a `max_calls` slot held forever.**

- **The app dials as a slin16 station again whichever network it used first.**
  Digital voice now rides a 16 kHz station through an exact 8↔16 kHz bridge
  instead of pinning the engine to 8 kHz.

- **A node that answers calls keeps its own codec policy.** Turning the
  inbound listener on pinned the station to µ-law at 8 kHz whatever it had
  been configured for, and on an engine already running at 16 kHz the
  mismatch meant the node answered nothing at all.

- **A stated FORMAT is honoured as a request, not treated as a hint.** A node
  that lists 16-bit linear for completeness but asks for µ-law was answered
  with slin16 and dropped the call silently — the worst shape a negotiation
  failure can take. Nor is a wideband codec asserted over a peer that sent no
  CAPABILITY at all: its stated format is the only thing it actually told us.

- **Registration fails over between a registrar's addresses.** A registrar
  hostname is commonly several hosts and they do not all answer a given
  source address, so pinning one is a bet that gets re-lost whenever the far
  end changes — silently, with REGREQs going out forever and nothing coming
  back. The retry ladder now advances one candidate per attempt and stays on
  whichever address answered.

- **VOX pre-roll and speech onset are no longer lost on D-Star/YSF key-down**,
  and YSF key-ups now appear in the timeline.

- **A live D-Star or System Fusion session draws its spectrum.** The graphs
  dispatched to AllStarLink and M17 only, so the two dongle networks showed
  empty bars.

- The self-hosted M17 parrot (`just m17-parrot`) replays at exactly 40 ms per
  packet; it drifted 4 % slow and was audible as a beat after a few seconds.

## 0.1.10-beta — 2026-08-30

Noise reduction that works while you are talking — new, and off until you turn
it on. Plus failures that say what went wrong, and every dependency brought
current.

### Added

- **Noise reduction that can clean up under your voice.** astar's noise
  reduction has always been a gate: it quietens the gaps between words, and by
  its nature can do nothing about a fan, traffic or a noisy room while you are
  actually speaking. There is now a second option that can — a small neural
  network (RNNoise, via the pure-Rust `nnnoiseless`) running on the microphone
  at the device's own rate, before anything else touches it.

  **It is off by default, and it has not been judged on real air yet.** Tick
  "Noise reduction" in Settings to try it. Whether it helps on your microphone
  — and especially whether it survives a low-bitrate vocoder like Codec 2 or
  AMBE, which allocate bits by spectral structure and may not thank you for it
  — is an open question this release does not answer. That is why the default
  is off, and why it stays off until somebody has sat and listened.

  It needs a capture device running at 48 kHz, which most USB radio interfaces
  are. A device that cannot offer 48 kHz keeps the old hum filter and gate
  instead of being fed audio the network was never trained on, and a line
  underneath the toggle tells you which of the two is running — or which
  *would* run, when you are not in a call and there is nothing to measure.

  A **Strength** slider appears alongside it, from full down to bypass. Turning
  it down leaves more of your original audio in the mix, which is the dial to
  reach for if it sounds over-processed.

  The hum filter stays in the chain either way: a measured notch removes a
  cheap microphone's whine deterministically, and a general-purpose network has
  no particular reason to treat a steady in-band tone as noise. The gate stands
  down only while the network is running, and comes back the moment it is not.

  The cost is about 35.7 µs of CPU per 10 ms of audio — 0.357% of one core,
  measured on Apple silicon and not yet anywhere else — and up to roughly 20 ms
  of extra delay on transmit, which is under one voice frame and does not delay
  keying. The design, including what has and has not been measured, is written
  up in `docs/design/noise-suppression.md`.

  There is also an `ASTAR_MIC_DENOISE` environment variable (`neural`,
  `legacy`, or `off`) for anyone who wants to A/B the two chains directly. It
  chooses which chain the checkbox turns on; it does not turn anything on by
  itself.

- **A published release list**, at
  [`/api/v1/releases.json`](https://rcludwick.github.io/astar/api/v1/releases.json),
  built with the docs from the app's own `MARKETING_VERSION` so it cannot
  describe a version that was never built. Every release carries both the
  display `version` and a strict-SemVer `semver`, so a client written against
  either one keeps working across the spelling change below.

- **A Discord server**, linked from the site header, the front page and the
  README.

### Changed

- **Versions are real SemVer from here on**, this one included, and the app and
  the Rust workspace now spell a release identically — they had been carrying
  two different strings for the same version because Cargo demands the hyphen.
  `just ci` fails if any of the five places that record a version disagree.
  Released versions are not retrofitted.

- **astar asks capture devices for 48 kHz.** It used to take whatever a device
  called its default. 48 kHz is what the new noise reduction requires, and it
  is the better rate regardless: 48 → 8 kHz is a clean 6:1 decimation where
  44.1 → 8 kHz is 5.5125:1. A device that does not offer it keeps its own
  default and simply does not get the neural chain.

- **Every dependency a major version behind was brought forward**: cpal
  0.15 → 0.18, rubato 0.16 → 5.0, rand 0.8 → 0.10, base64 0.22 → 0.23,
  libloading 0.8 → 0.9, md-5 0.10 → 0.11, and toml 0.8 → 1.1 in the Iced
  client, alongside 52 packages moved to their latest compatible versions.
  Device names are unchanged across the cpal upgrade, so saved input and
  output selections still resolve; cpal 0.18 also finds output devices 0.15
  did not enumerate.

### Fixed

- **Failures say what actually went wrong.** A dial that failed used to read
  something like "astarstation error -7: audio error" — a number and a
  category, neither of which tells you what to do about it. The engine knew
  more all along and had nowhere to put it; now it does. D-Star failures name
  the real reason instead of listing the three likeliest causes, and everything
  else shows the engine's own sentence rather than an error code.

  A microphone or speaker that will not open is the deliberate exception: it
  says simply "Couldn't open audio device, is it busy or unplugged?", because
  every cause behind that code has the same two remedies and naming which one
  it was would give you nothing more to act on.

  Failures are also **orange** now rather than red, matching every other
  warning in the app. A dial that did not go through is something for you to
  fix, not a fault in astar.

- **Switching networks clears the dial field.** A node number typed for
  AllStar is not an M17 or D-Star target and never was, but it used to stay
  in the box after the switch, looking like one. Picking a favorite, a recent
  or a reflector still fills the field as before — those set the target and
  the network together.

- **Two security advisories** in the WireGuard transport's dependencies,
  both inherited from boringtun 0.6: a timing-variability finding in
  curve25519-dalek (RUSTSEC-2024-0344) and a panic in ring's AES with
  overflow checks on (RUSTSEC-2025-0009). boringtun 0.7 clears both, and
  the dependency audit is now clean.

- **The docs site republishes on a changelog-only edit.** Its path filter
  never matched `CHANGELOG.md` — the file the site renders is a symlink to
  it — so a release that touched nothing else would not have rebuilt the
  page announcing it.

## 0.1.9beta — 2026-08-29

Who you are, at the top of Settings — and the first bricks of DMR.

### Added

- **The start of DMR: `crates/astar-dmr`.** DMR is not one network — it is a
  family of independently run ones that share a protocol, and a talkgroup
  number names nothing on its own (TG 91 exists on several of them and is a
  different room on each). So the first thing built is the address, not the
  wire: TGIF, FreeDMR, DMR+, SystemX, AmComm, VKDMR, FreeSTAR and ADN are
  grouped together as "Independent networks", and BrandMeister stands apart
  behind a consent gate that is shut until the operator opens it. Nothing
  dials DMR yet.

- **A DMR radio ID field**, in its own section below the AllStarLink account.
  DMR does not put a callsign on the air — it addresses radios by a number
  registered at radioid.net against a verified licence. That is a different
  credential from a callsign, so it gets its own field rather than being
  bolted onto one that means something else. Exported configs carry it under
  the same "Callsign and radio ID" checkbox, so a file you hand to someone
  else still leaves your identity behind. Nothing dials DMR yet, and the
  field says so.

### Changed

- **Settings fields have standing labels.** Callsign, DMR Radio ID, Node number
  and Password each carry a label in their own column instead of relying on
  placeholder text. A placeholder is not a label: it disappears exactly when
  the field has content, which is the moment you most want to know what you are
  looking at. The Operator and Account sections share one label width, so the
  pane reads as one form rather than two stacked by accident. The password
  keeps a placeholder for the one thing a label cannot say — "Re-enter to
  change", because an empty box there means unchanged, not blank.

- **Your callsign is the first thing in Settings.** It used to sit at the
  bottom of the AllStarLink account panel under an "M17" heading, which said
  two wrong things at once: that it belonged to M17, and that it was part of an
  AllStarLink account. It is neither. M17 sends it in every frame, D-Star puts
  it in every header, YSF carries it in every data packet — and AllStarLink is
  the one network that never transmits it, because there you dial as a node
  number. It is now an "Operator" section above everything else, and the
  AllStarLink account panel below it **no longer has a callsign box at all.** Your allstarlink.org login *is* your callsign;
  a second field for the same fact was only ever a way to get the two out of
  step. The account panel says which callsign it signs in as, and changing it
  above updates the saved account without asking anyone to retype a password.
  Clearing the account leaves your callsign alone — that is who you are, not
  an account detail.

- **The reflector directory is a pane of the window, not a sheet.** The
  magnifying glass beside the dial field now swaps the window over to the
  reflector list the same way the gear swaps it to Settings, and the same Back
  chevron — and the same ⌘[ — brings you home. Picking a module is still the
  second step, with its own Back to the list. Nothing about a directory you are
  browsing is modal, and a sheet floating over a popover was one layer of chrome
  more than the job needed.

## 0.1.8beta — 2026-08-28

The big one: astar can find a reflector it doesn't already know, and it can talk
to D-Star.

### Added

- **A reflector directory.** 3,188 reflectors across D-Star, M17, YSF, NXDN, P25
  and URF, searchable inside the app — no account, no API token, no network
  round-trip when you dial. A magnifying glass beside the dial field opens a
  search sheet; pick a reflector, pick a module, and the dial field fills in. It
  never connects for you: dialling stays a deliberate second action.

  The list ships inside the app, so a first launch with no network still has a
  complete directory, and refreshes itself **at most once a week** — the cadence
  is published by the data, not hard-coded, so it can change without an astar
  release. Settings gains a **Reflector directory** section with the counts, when
  it last synced, when it next will, and a **Sync Now** button.

- **Dial a reflector by name.** Type `XLX836 A` or `M17-002 A` instead of an
  address. The directory gets first refusal on the text and the address parsers
  only see what it doesn't recognise — so a reflector name can never be mistaken
  for a hostname and quietly resolved to something else.

- **D-Star.** A D-Star entry in the network switcher, reflector-and-module
  dialling against those 943 D-Star reflectors, and the callsign of whoever is
  talking shown under the connected-node line along with any slow-data message.

  D-Star voice is AMBE and astar ships no software vocoder, so the entry appears
  only while a **ThumbDV** dongle is attached. That is the honest gate: an entry
  that always failed to connect would be worse than no entry. (The probe runs
  once at launch, so a dongle plugged in afterwards needs a restart.)

- **Module pickers that ask rather than guess.** On D-Star the module *is* the
  room, and the registries publish no list of active ones — so astar offers the
  letters and says plainly that it cannot tell which are live. It never fills one
  in for you. Where a reflector does publish its modules (M17, URF), those are
  what you are offered. The module you last used on a reflector is remembered and
  shown marked on your return — a recollection you can see and change, not a
  default applied behind your back.

### Fixed

- **`XRF002` dialled the wrong reflector.** Some XLX reflectors carry an
  `XRF`-form alias that is also the real name of a *different*, standalone
  reflector — 44 names collided this way, and typing one reached whichever entry
  the directory happened to index first. `XRF002` reached a reflector in China
  rather than the one in the US that actually bears the name. Names that mean two
  reflectors are no longer published as aliases; the reflector keeps its own
  name, and the wire callsign it needs is unaffected.

- **The volume control had no effect on D-Star.** D-Star was never wired into
  the audio-preference fan-out, so it played at full scale while every other
  network sat at whatever you had set — which is why it came out louder than
  the rest. Output gain and RX levelling now reach a D-Star session, both when
  you connect and when you change them mid-QSO.

- **D-Star transmissions addressed the destination reflector.** The `RPT1`/`RPT2`
  header fields now carry the reflector being called, derived from its published
  hostname or taken from the directory when it is only reachable by IP — rather
  than going out blank.

### Changed

- **One callsign, not one per network.** M17 sends it in every frame and D-Star
  puts it in every header, and it is the same callsign — so it is one field, and
  the prompt names whichever network you are on. Nothing to re-enter; what you
  had is what it uses.

- Reflectors are searchable under **every name they answer to**. One XLX box is
  `XLX836`, `XRF836`, `REF836` and `DCS836` at once, and any of those now finds
  it.

- The documentation site was rewritten to be technical rather than promotional,
  `astar-server` got its own page saying plainly that it is a work in progress,
  and the changelog you are reading is now published there too.

## 0.1.7beta — 2026-08-22

One fix, in the status row at the top of the popover.

### Fixed

- **The round-trip time no longer shows a wrong number when the window is
  narrow.** Compacting the window horizontally squeezed the `ms` readout until
  it broke one character per line into a vertical strip, and — after a first
  attempt at protecting it — until it clipped to a single digit. A latency
  figure clipped to its first digit is not a cosmetic defect: `9`, `93` and
  `935` ms describe three very different calls, and nothing on screen showed it
  was truncated.

  The readout is now drawn at its full width or not at all, and the space it
  would have taken goes back to the connected-node name, which regains
  characters at narrow widths. It reappears by itself when the window is
  widened. On a narrow window with a long favorite name it stays hidden — a
  supplementary readout that is absent is honest; one that is truncated is not.

  Also fixed on the way: the connected-node name wrapped to a second line
  instead of eliding, which grew the card and pushed the level graphs down, and
  `Connected` itself could pick up an ellipsis while the row visibly still had
  room.

## 0.1.6beta — 2026-08-22

A small one, entirely about the two things a new user meets first: the app menu
and the About panel.

### Fixed

- **astar → Settings… no longer opens an empty window.** It opened SwiftUI's
  placeholder settings scene rather than astar's own settings. `0.1.1beta`
  shipped a fix for this that never took effect: it replaced the main menu at
  launch, and SwiftUI reinstalls its own menu afterwards and wins. The item is
  now removed at the source instead. Settings is where it has always actually
  been — the gear in the popover footer.

### Changed

- **The About panel says who wrote astar and where to find it.** It now carries
  `Copyright © 2026 Rob Ludwick. AGPL-3.0-only.` and links to the
  documentation, **the source on GitHub**, and AJ7HR on QRZ. AGPL-3.0-only asks
  that the source be reachable from the program, not merely published
  somewhere, and until now nothing in the app pointed at it.

  Both come from the app bundle rather than from code, so the panel reads the
  same however it was opened.

## 0.1.5beta — 2026-08-22

Your setup stops being trapped on one Mac. Configs, favorites and settings can
be exported to a file and imported back — in whole or in part — so a second Mac,
a rebuild, or handing a working rig to another operator is no longer a matter of
re-entering everything by hand.

### Added

- **Export and import your configuration** (Settings → Backup). Export writes a
  plain-text `.astarconfig` file; both ends are section-by-section, so one
  format serves a personal backup and something you share. The sections are
  saved configs (with their mic profiles), the node directory, audio and serial
  settings, callsign, and window state.

  **Callsign is its own tick box**, because it is the field that decides whether
  a file identifies you. Leave it off and an export is a rig anyone can use.

  Import **adds and updates; it never deletes**, and re-importing the same file
  is a no-op rather than a way to end up with everything twice. Node entries
  match on node number rather than id, so the same node is never stored twice
  and your own label and ★ survive an import. This is what makes it possible to
  take somebody's node list without adopting their microphone and serial wiring.

  **Your AllStarLink account is never exported.** It stays in the Keychain,
  there is no option to include it, and an import leaves existing credentials
  untouched. A test encodes a full archive and greps it for the password to
  keep that true.

- **A config version**, stamped into both an exported file and the preferences
  domain, so a future release can translate an old config rather than reject it.
  It marks translation, not change: adding fields or sections leaves it alone,
  and a version 1 file will keep importing. A real `0.1.4beta` export is kept in
  the test suite to hold that promise to something.

- **A favorite's node number is editable in place** (Settings → Favorites).
  Repointing a favorite used to mean deleting it and adding it again, which
  threw away its label, its ★ and its per-node talk-timer override.

### Fixed

- **Two audio devices with the same name no longer both appear selected.** An
  ICOM IC-7300 and an AllScan UCI150 both enumerate as "USB Audio Device", and
  astar identifies devices by name — so of two same-named devices exactly one is
  reachable, and offering both was offering a choice that does not exist. The
  picker now lists one entry per distinct name, marks an ambiguous selection with
  an orange name, a red outline and a warning triangle, and explains what to do
  about it: renaming a device in Audio MIDI Setup is stored against that device
  and fixes it for good.

  The underlying repair — identifying devices by their stable CoreAudio UID
  instead of their name — is tracked as `astar-uid`. Until then, a saved device
  name still binds to whichever same-named device the system enumerates first.

### Documentation

- **[Saved configs, backup and transfer](https://rcludwick.github.io/astar/macos/configs/)** —
  what a config contains, where it is stored, what export and import do and do
  not carry, and the config-version rule.

## 0.1.4beta — 2026-08-18

M17 now works on a Mac that has never seen Homebrew. That was the whole point
of this release: `0.1.3beta` offered an M17 network picker that could not
actually open a codec, because the Codec 2 library it needed was something you
had to install yourself.

### Added

- **Codec 2 is linked into the shipped app**, so M17 works out of the box.
  astar still prefers a system `libcodec2` if you have one and only falls back
  to the linked copy, so nothing changes for anyone who installed it via
  Homebrew. Verified as a controlled experiment on a Mac with Homebrew's
  `codec2` uninstalled: the runtime-only build reports M17 unavailable, the
  shipped build reports it available.

  This is the only LGPL code in astar, it is unmodified, and it is deliberately
  never part of a default build — see `LICENSE-EXCEPTIONS.md` for the Codec 2
  notices and the written offer, and `ci/guard-codec2-licensing.sh`, which
  fails the build if it ever leaks into a default feature set.

- **An App Store distribution exception** under AGPL-3.0 §7, scoped to Rob
  Ludwick's own copyright and removable exactly as §7 allows. astar stays
  AGPL-3.0-only; the exception exists so the same source can eventually ship
  through the App Store, whose terms conflict with the bare AGPL. It grants no
  rights over Codec 2, which is not Rob's to relicense — and nothing here stops
  you modifying Codec 2 and rebuilding astar, which the notices say in as many
  words.

- **A first-run walkthrough** in the docs — install, account, audio levels, the
  four ways to key up, first contact. Written from the questions an outside
  tester actually asked over a week rather than from what seemed obvious from
  the inside.

### Fixed

- The CodeQL workflow could hang for 25 minutes on its own `apt-get` step
  against an unreachable Ubuntu mirror. The step is now bounded and retried,
  and reports what happened instead of stalling the run.

## 0.1.3beta — 2026-08-17

Everything here is about the first ten minutes with astar. `0.1.2beta` fixed
Settings being unreachable; this fixes Settings being *findable but silent* —
you could open it and still not learn that an AllStarLink account is the thing
standing between you and a call.

### Added

- **Settings opens by default when no AllStarLink account is configured.**
  Without one the dial field is disabled, so the call UI is a form that cannot
  be typed into. astar now lands on Settings instead, from every route into the
  window — the menu-bar asterisk, the Dock icon, and `Cmd-,`. It stops the
  moment an account is saved.

- **A first launch with no account raises the window on Settings** — once.
  astar is a menu-bar app, so a new user otherwise sees an asterisk and nothing
  else. It happens a single time: M17 needs no portal login, so running astar
  without an AllStarLink account is perfectly legitimate and is not nagged at.

- **The account password is outlined in red** when it is empty with nothing
  saved, or when the portal rejected the last token test, with the reason
  underneath.

  An empty box on an account that *is* saved stays unmarked. astar never
  pre-fills the password — it lives in the Keychain and the field reads
  "re-enter to change" — so flagging that would tell you your working
  credentials are broken.

- **Settings now says what a missing account costs you:** AllStarLink is
  unavailable without one, because dialling a node signs in to your
  allstarlink.org account and there is no guest access. M17 is unaffected.

### Fixed

- CodeQL had never completed a single run. cpal's Linux backend builds against
  ALSA and the hosted runner ships no ALSA headers, so the build died before
  analysis started. Invisible from a Mac, where cpal uses CoreAudio.

## 0.1.2beta — 2026-08-17

Both fixes here came out of the first outside report against the `0.1.1beta`
DMG, from a tester who had never built astar from source — so he met the app
exactly as a new user does, and hit two walls in a row.

### Fixed

- **Settings opened an empty window.** astar spent its life as an `LSUIElement`
  accessory, which has no application menu. The Dock icon added in `0.1.1beta`
  promotes the app to a regular one, and that handed it a menu bar whose
  `Settings…` item was still wired to the placeholder empty scene the app used
  to satisfy SwiftUI's "an App must have a Scene" requirement. Choosing it
  opened a window with nothing in it. astar now builds its menu explicitly, and
  `Cmd-,` opens the real settings pane — the same one the popover's own settings
  button shows, not a second copy.

- **A fresh install invented a config for hardware you may not own.** With no
  saved configs, astar seeded one named after the AllScan UCI150 and put it on a
  serial hardware profile, whether or not that interface had ever been plugged
  in. Meanwhile the entry that described plain system audio was filtered out of
  the settings list and never appeared at all.

### Added

- **A built-in `System Default` config**: your Mac's current input and output,
  no serial PTT. It is always present, can't be deleted, sits at the top of
  Saved configs, and is what a fresh install starts on and stars as its launch
  default.

  Existing setups are left alone. If you already have saved configs but never
  set a launch default, astar still applies nothing at startup — making System
  Default win there would reset your devices and switch off your serial PTT on
  every launch.

- **A real menu bar.** `About astar` with links to the documentation and to
  AJ7HR on QRZ; `Settings…` on `Cmd-,`; a standard Edit menu, so `Cmd-C` /
  `Cmd-V` / `Cmd-A` work in the account, node, and config fields; Window; and a
  Help menu linking the documentation site, the issue tracker, and QRZ.

- **`Report an Issue…` in the Help menu**, pointing at the public repository's
  issue tracker. Nothing in the app used to say where a bug should go.

## 0.1.1beta — 2026-08-14

### Added

- **A Dock icon for the macOS app**, on by default, with a `Show in Dock` toggle
  in the status item's right-click menu. astar has had a real main window for a
  while; it now has the Dock presence and Cmd-Tab entry a windowed app is
  expected to have. Turning it off returns it to menu-bar-only, and that choice
  sticks across launches.

  The app still launches as an `LSUIElement` accessory and promotes itself
  afterwards, so anyone who turns the icon off never sees it flash on at startup.

- **Clicking the Dock icon opens the astar window.** It shows the window, it
  never hides it — a second click on a Dock icon is not a close button.

- **M17 in every default client build.** The macOS and Iced clients ship the M17
  network without a feature flag.

### Fixed

- The `Show in Dock` toggle applied only after a restart. Under
  `@NSApplicationDelegateAdaptor`, `NSApp.delegate` is SwiftUI's own wrapper
  delegate rather than the app's, so the status item's `as? AppDelegate` cast
  silently produced nil and the apply step never ran. The activation-policy
  change now lives on a stateless type both callers own outright.

### Changed

- CI moved to a private development repository with a self-hosted runner; the
  public repository is the release target and carries no runner. Both workflow
  files exist in both repositories and are guarded on `github.repository`, so
  each is inert in the wrong one.

- Documentation is published to GitHub Pages on merge to `main` in the public
  repository.

### Documentation

- Stopped claiming the macOS app has "no Dock icon and no main window". The main
  window has existed for some time; the Dock icon now exists too. Corrected in
  the README, the site front page, the macOS page, and the build guide.

- Brought the site in line with the README on D-Star, and stopped
  `.github/README.md` shadowing the front page.

## 0.1.0beta — 2026-08-11

First tagged version, and the point at which astar became one repository: the
client and the `iaxclient-rs` engine were merged into a single AGPL-3.0-only
cargo workspace with one git history.

What that version contains:

- **astar-lib** — the engine. IAX2, M17, and D-Star protocol work, codecs,
  audio, PTT, the station facade, and a C ABI.
- **The macOS client** — a SwiftUI menu-bar app over that engine.
- **The Iced client** — the Windows and Linux front-end, sharing the same core.
- **astar-server** — the headless node daemon.

No binaries were published, then or since. `just dmg` produces a local, unsigned
disk image; building from source is the only install path.
