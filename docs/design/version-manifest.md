# The version manifest — design

**Status:** the published half is **built**. `ci/version_manifest.py` generates
`api/v1/releases.json`, `ci/build-docs.sh` publishes it, and `just ci` fails if
the version is spelled two different ways in the tree. The **client** half —
the footer indicator and the Settings toggle — is designed here and specified
below, but not built: backlog item `astar-vercheck`.

**Read first:** nothing. This is self-contained, and deliberately small.

## The problem, which is not "we need an update mechanism"

astar has no update mechanism and this document does not propose one. There is
no Sparkle feed, no signed appcast, no in-app download and no self-replacing
binary. The `.dmg` is arm64-only, notarized by hand, and a real update channel
implies a signing story astar has not yet earned.

What it proposes is much smaller: **a running astar should be able to find out
that a newer release exists**, and say so once, quietly, in the place a user
already looks for a version number. Everything after that is the user opening
the releases page themselves.

The reason it needs designing at all is that the version number today lives in
three files that a human keeps in step by hand on release day:

| Where | What it is | Read by |
|---|---|---|
| `apps/macos/project.yml` | `MARKETING_VERSION: "0.1.9beta"` | XcodeGen → `CFBundleShortVersionString` → the popover footer |
| `zensical.toml` | the `v0.1.9beta` chip in `copyright` | the site's footer |
| `CHANGELOG.md` | the newest `## 0.1.9beta — 2026-08-29` heading | the published changelog page |

Three homes, one number, no enforcement. The 0.1.9beta release bumped the first
two by hand. Adding a fourth hand-maintained home for a JSON file would make
that worse, so the manifest is **generated from those three and refuses to build
when they disagree**. The fragility becomes a gate.

## Where the release list comes from

`CHANGELOG.md`, and specifically its `## <version> — <YYYY-MM-DD>` headings.

The alternatives lose on the same axis, which is what the build is allowed to
do. `docs-pages.yml` runs on `ubuntu-latest` with `actions/checkout@v4` and
`persist-credentials: false`:

| Source | Cost |
|---|---|
| **`CHANGELOG.md` headings** | Free. Already in the checkout, already maintained on every release, already what the site publishes. |
| Git tags | Needs `fetch-depth: 0`, because the default checkout is a shallow single commit with no tags. Fixable, but it buys nothing the changelog does not already say. |
| GitHub Releases API | Network **and** auth inside a docs build that has neither by design. It would also make the published manifest depend on GitHub being reachable at build time. |

There is a second, better reason than cost. The changelog is what the manifest's
`notes_url` links *to*. Deriving the list from the same file guarantees the
manifest and the release notes can never describe different sets of releases —
every entry the manifest names has notes, and the anchor is proven to resolve
(below). A tag-derived list would happily publish a version with no notes.

The consequence, stated plainly: **the manifest describes the changelog, not
the GitHub releases page.** If a GitHub release is deleted and the changelog
heading stays, the manifest keeps naming that version. Withdrawing a release
means deleting its changelog heading — there is one place to do it.

## The disagreement rule

`ci/version_manifest.py --check` reads all three sources and fails if they are
not the same string. It also fails if `CHANGELOG.md` is out of descending
version order, names a version twice, or carries a heading it cannot parse.

It is wired into **`just ci`**, not only the docs build. That placement is the
point: `just ci` is the gate that must be green on every change, so a release-day
drift is caught by the everyday command minutes after it happens, rather than by
a Pages deploy that only runs on the public repo after a merge. `ci/build-docs.sh`
runs it again before generating, so the docs build cannot publish a manifest
built from a tree that disagrees with itself.

What each failure looks like, since these are the messages someone will actually
meet on a release day:

| Situation | Result |
|---|---|
| `project.yml` bumped, `zensical.toml` chip not | Fails naming all three values and their files |
| Both bumped, no changelog heading written yet | Same failure — the changelog is one of the three |
| A heading filed in the wrong place | "CHANGELOG.md is not in descending version order", with found vs expected |
| A heading typed as `## 0.1.9beta (2026-08-29)` | Fails quoting the line and the shape it wanted |

`project.yml` is YAML and the generator does **not** parse YAML. It matches one
line with a regex: `MARKETING_VERSION` appears exactly once as a setting, it is
a quoted scalar at a known key, and adding a YAML dependency to a docs build to
read one string would cost more than it is worth. If the key ever moves or
gains a sibling, the check fails loudly rather than reading the wrong one.

### The fourth version, which is deliberately not in the lockstep

The root `Cargo.toml` declares `workspace.package.version = "0.1.3-beta"`. The
app is on `0.1.9beta`. **They have already drifted**, six releases' worth, even
though the comment in `project.yml` says "Bump both together."

That is a real finding and it is not fixed here. Folding `Cargo.toml` into
`--check` today would fail `just ci` on a pre-existing condition rather than on
anything a change did, and correcting it means bumping a version, which this
work is explicitly not allowed to do. It is recorded in `astar-semver` in the
backlog, together with the question of whether a Cargo crate version and a
user-facing marketing version should be the same number at all — they answer
different questions, and SemVer's rules about what a major bump means apply to
one of them and not the other.

## The schema

The document is a pure function of the working tree. Two builds of the same
commit publish byte-identical bytes, which is why there is **no `generated`
timestamp**: freshness is `current.date`, a fact about the release, rather than
a fact about the build machine's clock. (This is where it departs from the
in-house precedent, `hamcall-db`'s `api/v1/reflectors.json`, which does carry
`generated` — correctly, because its data comes from upstream feeds that change
without a commit. This one cannot.)

```json
{
  "schema": 1,
  "product": "astar",
  "current": {
    "version": "0.1.9beta",
    "semver": "0.1.9-beta",
    "ordinal": 10,
    "date": "2026-08-29",
    "notes_url": "https://rcludwick.github.io/astar/changelog/#019beta-2026-08-29",
    "release_url": "https://github.com/rcludwick/astar/releases/tag/v0.1.9beta"
  },
  "releases": [
    { "version": "0.1.9beta", "semver": "0.1.9-beta", "ordinal": 10, "date": "2026-08-29", "…": "…" },
    { "version": "0.1.8beta", "semver": "0.1.8-beta", "ordinal": 9,  "date": "2026-08-28", "…": "…" }
  ]
}
```

| Field | Why it is there |
|---|---|
| `schema` | An integer, following `hamcall-db`. Bumped only if an existing field changes meaning — adding a field does not, exactly as `CLAUDE.md` rules for `ConfigVersion`. |
| `product` | So a file fetched from the wrong URL is recognisably the wrong file. |
| `current` | A full copy of `releases[0]`, so the dumbest possible client is one field read away from an answer. |
| `version` | The exact string the app reports as `CFBundleShortVersionString`. Display, and the key a client matches itself against. |
| `semver` | The same release as strict SemVer. See below. |
| `ordinal` | 1 for the first release ever, ascending. The comparison primitive. |
| `date` | The release date, `YYYY-MM-DD`. |
| `notes_url` | The changelog anchor on this same site. |
| `release_url` | The GitHub release page for the tag. |

`releases` is newest-first, matching the changelog it came from.

### How a client compares versions, given `0.1.9beta` is not SemVer

Two ways, and a client may use either.

**The easy way — ordinals.** Find your own `CFBundleShortVersionString` in
`releases[]`, compare its `ordinal` with `current.ordinal`, and if `current` is
larger there is something newer. No version-parsing code on the client at all,
in any language. This is the recommended path and it is why `ordinal` exists.

If your own version is **not** in the list — a build from source between
releases, a withdrawn version, a local hack — the client does nothing and says
nothing. That is the correct answer: astar does not know what you are running
relative to what is published, and inventing a guess would nag someone who is
ahead of the published world, not behind it.

**The comparing way — `semver`.** Normalize your own string with the same rule
the generator uses (`0.1.9beta` → `0.1.9-beta`) and compare per SemVer §11. The
rule is five lines and lives in `normalize()` / `sort_key()`.

The trap this all exists for: **`0.1.10beta` is newer than `0.1.9beta`, and
string comparison says the opposite.** `sort_key()` compares the numeric fields
as numbers, and `ci/test_version_manifest.py` pins that case first.

### Leaving beta costs no code and no schema bump

Today's strings glue the pre-release to the patch number with no separator,
which is not SemVer. SemVer's own pre-release syntax (`0.1.9-beta`, or
`0.2.0-beta.1`) would have solved the ordering problem from the start, and
`astar-semver` in the backlog is the item to adopt it.

The comparator is **already written for both shapes**, which was the cheaper
call by a distance:

* `0.1.9beta` → `0.1.9-beta` (legacy, translated)
* `0.2.0-beta.1` → unchanged (already SemVer)
* `0.2.0` → unchanged, and sorts *after* every `0.2.0-*` pre-release, per §11

So on the day the suffix is dropped, the generator needs no change, the schema
needs no bump, and every client keeps working. That last part is not optional:
**a manifest already fetched from the wild will contain `0.1.9beta`-shaped
strings forever**, so anything that parses them must keep parsing them. Emitting
`semver` alongside the display string from day one is what makes that free — a
client written today against `semver` never has to learn the legacy shape at
all, and a client written against `version` still has `ordinal`.

That is the call, made now rather than deferred: **no schema bump for the
SemVer transition.**

### The `.dmg` URL is deliberately absent

The obvious next field would be the asset path GitHub derives from the tag —
`releases/download/<tag>/astar.dmg` under the repository. It is not there, for
three reasons in increasing order of weight:

1. It is fully derivable from the tag, so it adds no information.
2. The build cannot verify it. The docs build has no network and no auth, so
   the field would be an assertion of convention, not of fact.
3. **It is the wrong shape for the file.** This manifest is read by the macOS
   app, and will be read by the Iced client on Windows and Linux and by
   `astar-server`. A macOS-arm64-only `.dmg` URL in a cross-platform document
   invites exactly the wrong client behaviour — a Linux user offered a disk
   image — and `CLAUDE.md` is explicit that install instructions must not imply
   channels that do not exist.

`release_url` is in for a different reason: it is where a human goes to read
about a release and, if they are on a Mac, download it. It carries the same
"asserted, not verified" caveat, so a client must treat a failure to open it as
cosmetic. If a release is deleted or re-tagged, that link 404s until someone
edits `CHANGELOG.md` — the manifest keys on the version string, not on a
commit, so a re-tag (same version, different commit) is correctly invisible.

`notes_url`, by contrast, **cannot dangle**, because it is served by the same
Pages deploy that publishes the manifest and the build proves it — see below.

## Where the file lands, and how

`docs/site/api/v1/releases.json`, generated before Zensical runs, copied into
the built site verbatim, and published at:

```
https://rcludwick.github.io/astar/api/v1/releases.json
```

The path shape is borrowed from `hamcall-db`'s `api/v1/reflectors.json`, which
astar already consumes — same in-house convention for "a small static file that
is an API in every way that matters".

There were two placements. `zensical build --clean` wipes `site_dir`, so the
choice is really *when*, not *where*:

| | Pre-build into `docs/site/` (chosen) | Post-build into `docs/.site/` |
|---|---|---|
| Survives `--clean` | Yes — written before the wipe, copied through by the build | Yes — written after |
| `zensical serve` (`just docs`) shows it | Yes | No |
| Depends on Zensical copying non-Markdown assets | Yes — **verified**, and `ci/build-docs.sh` hard-fails if it stops being true | No |
| Leaves a generated file in the docs source tree | Yes — gitignored (`docs/.gitignore`: `/site/api/`) | No |

Pre-build won because the local preview and the published site then show the
same thing, and because the dependency it takes on is checked rather than
assumed: the build asserts `docs/.site/api/v1/releases.json` exists afterwards
and tells you to switch to post-build if that ever fails.

It is **gitignored, not committed**. A committed generated file would have to be
regenerated by hand on every changelog edit — a fourth thing to keep in step,
which is the exact problem being removed. `just docs` generates it first so a
fresh clone's live preview is not missing it.

### The changelog anchor is proven, not assumed

`notes_url` ends in a fragment (`#019beta-2026-08-29`) computed from the heading
text using Python-Markdown's `toc` slugify rule, which is what Zensical renders.
That rule is a dependency of a dependency, so after the site is built,
`ci/build-docs.sh` runs `--verify-anchors` and fails if any emitted fragment is
not an `id=` on the rendered changelog page.

If a toolchain upgrade ever changes the slug rule, the build breaks with a
message naming the function to fix — instead of quietly shipping ten dead links.

### A path-filter bug found on the way

`docs-pages.yml` filtered on `docs/site/**`, but `docs/site/changelog.md` is a
**symlink** to the root `CHANGELOG.md`, and a `paths:` filter matches the path
that actually changed. A release that only edited the changelog therefore never
republished the site — the published changelog page was already stale by
construction, before this work existed. `CHANGELOG.md`, `apps/macos/project.yml`
and `ci/version_manifest.py` are now in the filter, which fixes that and keeps
the manifest's three inputs as its triggers.

## Freshness, and what a client may assume

**GitHub Pages sets its own cache headers and astar cannot change them.** Pages
serves with a short `max-age` and an ETag, and the CDN revalidates, but the
honest statement is that a client can be handed a stale copy for an unspecified
interval after a release and has no way to demand otherwise.

That is fine, because nothing here is time-critical. A user learning about a
release an hour or a day late loses nothing. The rules that follow from it:

* **Poll at most once every 24 hours**, on a schedule anchored to the last
  successful check, not to launch. A user who opens and closes the app twenty
  times in an afternoon must generate at most one request.
* **Never poll on a timer while the app is idle in the menu bar.** Check on
  launch if 24 hours have passed, and otherwise not at all.
* **Never bust the cache.** No cache-busting query parameter — a `?t=` would
  both defeat the CDN and turn every request into something uniquely shaped.
  Send a conditional request (`If-None-Match`) and be happy with a 304.
* **A failed check is not an event.** No retry storm, no backoff state machine.
  If it fails, wait for the next 24-hour window.

## Privacy

An update check is an outbound request from a user's machine that they did not
ask for individually, so the constraints are absolute and they are constraints
on *us*, not on the user:

* **No identifiers.** No install ID, no machine ID, no callsign, no node number,
  no serial, no license, no locale, nothing.
* **No telemetry.** The request does not report the running version. Comparison
  happens entirely on the client — that is the whole reason the file carries the
  full release list rather than exposing a `?version=` endpoint that answers
  "is X current". An endpoint like that would be a log of who runs what.
* **No query parameters at all**, for the same reason and for the cache reason
  above. A bare GET of a static path.
* **No custom User-Agent.** `fetch-reflectors.sh` sets a contactable one because
  it is a build-time fetcher hitting someone else's data and that is a courtesy
  between maintainers. A per-user request is the opposite case: a distinctive
  User-Agent is a fingerprint, and there is nobody to email.
* **No server-side logging is designed, requested or enabled.** GitHub Pages
  keeps whatever GitHub keeps; astar adds nothing and reads nothing. There is no
  dashboard, and there will not be one.
* **It must be possible to turn off**, and off must mean no request at all — not
  a check whose result is hidden.

The file itself is inert: it has no timestamps, no counters and no fields that
would encourage anyone to build a tracking design on top of it later. A test
asserts that (`test_manifest_carries_no_identifiers_or_timestamps`).

## The client half — specified, not built

Backlog item `astar-vercheck`. Building it is separate work; what follows is
settled requirement, not open question.

### Where the notification goes

**The popover's bottom bar** — `MenuPopover.swift`, `private var footer`. It
holds, in order: the Settings gear (leading), `Text(Self.appVersion)` showing
the *running* build's `CFBundleShortVersionString`, a `Spacer`, and "Quit astar"
(trailing).

The indicator belongs **beside that version string**, because the footer already
answers "what am I running" and "and there is a newer one" is the same
question's other half, in the place a user already looks for it.

The layout constraints there are load-bearing and recorded in the existing
comment. The window is resizable from `minWidth: 310` with a text size the user
controls; the version `Text` uses `fixedSize()` + `lineLimit(1)` with the
`Spacer` absorbing the slack, precisely so nothing truncates or drifts
off-centre. **Anything added there has to survive the same squeeze** — so it
should be a small, fixed-width element (an SF Symbol, or a very short label)
that is also `fixedSize()`, not a sentence, and it must not steal the `Spacer`'s
job. It needs an accessibility label naming the new version, and it should be
actionable — opening `release_url`, or `notes_url` — because an indicator that
says "there is something newer" and gives you nowhere to go is a nag.

### The Settings toggle

A checkbox in the Settings pane's `List` (`devicesPane` in `MenuPopover.swift`),
in its own small section near the bottom — beside `ConfigTransferView`, with the
application-level housekeeping, not among the audio and radio equipment.

**Default: on.** Arguing it rather than asserting it: the check is a single
unauthenticated GET of a static file every 24 hours, carrying nothing about the
user — the privacy cost of the default is as close to zero as a network request
gets. Against that, astar has no update mechanism at all, so a user who does not
know a release happened simply never gets it; defaulting off means the feature
does nothing for almost everyone who would benefit. The honest test for an
on-by-default network call is "would a reasonable user be surprised or harmed to
learn it happens?", and for a parameterless fetch of a public JSON file with no
identifier and no logging, the answer is no. Ham operators also run astar on
metered and offline links, which is a real cost — but a few hundred bytes a day
that fails silently when there is no route is not one worth defaulting away.

Storage follows `AudioSettings`: a `UserDefaults`-backed preference in the
`com.aj7hr.astar` domain, sibling to the `audio.*` / `m17.*` / `serial.*` keys.
The key is **`updates.checkForNewVersions`** (Bool), and the new `updates.`
prefix is chosen on purpose:

* It does **not** join `ConfigArchive.settingsPrefixes` (`audio.`, `m17.`,
  `serial.`), so it does **not** travel in an exported `.astarconfig`. An
  exported config moves a *station setup* between Macs or hands it to someone
  else; whether that person's machine makes an outbound request is their
  decision about their machine, not a property of the radio setup. Importing a
  config must never silently switch someone's update checking on or off.
* It is not `ui.` either — that slice is window and panel state.

**Adding this key does NOT bump `ConfigVersion.current`.** `CLAUDE.md` is
explicit: adding a field or a whole section does not bump it, because an older
reader ignores what it does not recognise and a newer reader treats what is
absent as unset. An absent `updates.checkForNewVersions` reads as the default.
This paragraph exists so nobody bumps it later by reflex.

### Failure posture: silence, in every failure mode

**If the version cannot be found, nothing happens.** No alert, no error state,
no red badge, no banner, no log spam. The footer simply shows nothing extra, and
that is indistinguishable — by design — from "you are up to date".

Explicitly, all of these are the same non-event:

| What went wrong | What the user sees |
|---|---|
| No network / DNS failure / captive portal | Nothing |
| Timeout, TLS failure, 404, 500, redirect loop | Nothing |
| Body is not JSON, or is truncated mid-object | Nothing |
| JSON parses but `schema` is a number this client does not understand | Nothing |
| `current` or `releases` missing, or `releases` empty | Nothing |
| The running version is not in `releases[]` | Nothing |
| The running version **is** the newest | Nothing |

The last two rows are the point of the table: "I cannot tell" and "you are
current" must be the same UI, or the absence of a badge stops meaning anything.
A malformed or truncated body is discarded whole — no partial parse, no
salvaging a `current` out of a broken document — and the previous cached answer,
if any, is left alone rather than being cleared into an error. Nothing is
written to the log at any level above debug: a user on a train should not
accumulate a log full of failed update checks.

### What the client needs from this file, in one line

`current.ordinal` and `current.version`, its own `CFBundleShortVersionString`
matched against `releases[].version` for an `ordinal`, and `notes_url` /
`release_url` to open. Nothing else.

## What is deliberately NOT being built

* **No update mechanism.** No download, no install, no self-replacement, no
  Sparkle, no appcast, no signature verification, because nothing is being
  installed. This file tells you a version exists; you go and get it yourself.
* **No `.dmg` URL** — argued above.
* **No server component, no endpoint, no query interface.** A static file the
  client reasons about, not a service that reasons about clients.
* **No server-side logging or analytics of any kind.**
* **No "you are out of date" modal, nag screen, or anything that interrupts.**
  A user who ignores the footer indicator forever must be able to keep doing so.
* **No Iced or `astar-server` client work yet.** Both will read this same file
  when their turn comes, and the schema was chosen to be language-neutral
  precisely so their comparison logic can be `ordinal` arithmetic. Same file,
  same rules, same silence on failure.
* **No pre-release or nightly channel.** One list, one `current`, and it is
  whatever the changelog says.

## Open questions

* **Should `Cargo.toml`'s workspace version join the lockstep, or is it a
  different number?** It is already six releases adrift at `0.1.3-beta`. A crate
  version and a marketing version answer different questions and SemVer's major-
  bump rules apply cleanly to only one of them. Recorded as `astar-semver`.
* **Does the Iced client have anywhere as natural as the popover footer?** The
  cross-platform rule in `CLAUDE.md` says every feature ships on all three
  platforms with per-platform native UI; the footer indicator needs an Iced
  equivalent that is equally ignorable, and nobody has looked yet.
* **`astar-server` runs headless.** "Show an indicator" has no meaning there.
  Probably a field in the status snapshot and nothing else — certainly not a log
  line every 24 hours — but that is a decision for whoever wires it.
* **Should `notes_url` point at the site or at GitHub?** It points at the site
  today because that link is provably alive. If the site is ever unreachable for
  a user who can reach GitHub, they have `release_url` as well; whether the
  client should prefer one is untested.
