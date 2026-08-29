# Reflector directory in the clients — design

**Goal.** Typing `XLX836` connects you. Finding a reflector you don't already know
is possible. Neither requires an API token, an account, or a network round-trip
at dial time.

**Shape.** A cached directory fetched from hamcall-db's static JSON API, refreshed
at most weekly, with a manual sync button. One data model serves D-Star, M17, YSF,
DMR and whatever comes next.

---

## 1. What the directory is

hamcall-db publishes `/api/v1/reflectors.json` — every reflector across every
covered network, CC BY 4.0, no token. The contract is `docs/REFLECTOR-API.md` in
that repo. The parts that matter here:

* An entry is an **envelope** (`network`, `id`, `name`, `aliases`, `description`,
  `country`, `sponsor`, `dashboard`) plus a **discriminated `dial` object** whose
  `kind` says how to connect.
* **`dial` absent, or a `kind` we don't implement, means "listed but not
  dialable."** That is the extension mechanism, and the client must honour it:
  show the entry, refuse to connect to it.
* `client_refresh_days` tells the client how often to re-check. It is data, not a
  constant — if the cadence changes upstream, clients follow without a release.

## 2. Where it lives in the app

### `ReflectorDirectory` (AstarCore)

One type, no UI, testable without a network:

```swift
public struct DirectoryEntry {
    public let network: Network
    public let id: String            // "XLX836"
    public let name: String
    public let aliases: [String]     // ["XRF836"]
    public let description: String?
    public let country: String?
    public let dial: DialTarget?     // nil => listed, not dialable
}

public enum DialTarget {
    case dextra(host: String, port: UInt16, callsign: String, modules: [String])
    case m17(host: String, port: UInt16, callsign: String, modules: [String])
    case ysf(host: String, port: UInt16)
    case unsupported(kind: String)   // known to exist, not implemented here
}
```

`unsupported` is deliberate. Decoding an unknown `kind` into a case rather than
dropping the row means a YSF-only build still *lists* DMR reflectors and can say
"not supported in this build" instead of pretending they don't exist.

The directory exposes three operations and nothing else:

```swift
func search(_ query: String, network: Network?) -> [DirectoryEntry]
func resolve(_ name: String, network: Network) -> DirectoryEntry?
func sync() async throws -> SyncOutcome
```

### Storage

| Layer | Path | Why |
|---|---|---|
| Bundled snapshot | `astar.app/Contents/Resources/reflectors.json` | First launch works offline, before any sync, and before the user has a network. |
| Cache | `~/Library/Application Support/astar/reflectors.json` | What sync writes. Preferred when present and parseable. |

The snapshot is committed at `apps/macos/Resources/reflectors.json` and copied
into the bundle by the `Resources` path in `apps/macos/project.yml`. **Refresh
it with `just reflectors` at release time**, in the same pass as the version
bump, and commit the result — nothing else ever refreshes it, and a snapshot
that silently rots is worse than one that is obviously old. The fetch validates
the payload before it overwrites anything, for the same reason sync parses
before it writes: a 200 carrying an error page must not replace a directory
that works, and here the damage would not surface until someone launched the
app offline.

It ships whole — all 3,185 rows, every network, ~1.2 MB — rather than trimmed
to what astar can dial today. Trimming would make the bundled state and the
synced state different shapes, which is a bug source for the sake of a
rounding error in a DMG, and it would break the "listed but not dialable"
contract on a first launch specifically.

A corrupt or truncated cache falls back to the bundled copy rather than leaving
the picker empty — an empty reflector list looks identical to "this feature is
broken," and only one of those is recoverable by the user.

## 3. Dialling by name

The dial field stays **one smart field**. Today it already distinguishes a node
number from a host. Directory resolution slots in ahead of the address parser:

```
"XLX836"              -> reflector resolves; module still needed, Connect disabled
"XLX836 A"            -> complete: 45.56.69.219:30001, RPT "XRF836", module A
"XLX836/B"            -> complete, same thing with a slash
"45.56.69.219:30001/A" -> no hit, falls through to the existing address parser
```

**Resolution order is directory-then-address**, not the reverse, so a name never
gets mistaken for a hostname. Matching is case-insensitive and checks `aliases`,
so `XRF836` finds the XLX entry it genuinely aliases.

### How it is built (astar-refl-ship)

`ReflectorIndex.resolveDial` is the whole of it, and the ordering is a property
of its return type rather than a convention: `ReflectorDialResolution` has four
cases — `ready`, `needsModule`, `notDialable`, `notInDirectory` — and
`notInDirectory` is the *only* one that lets a caller reach an address parser.
There is no code path from dial text to `M17Dial.parse` that has not already
asked the directory.

The index is a frozen, `Sendable` value the directory republishes on every load
and sync, not the `@MainActor` directory itself. Dialling runs off the main
thread by contract (the AllStar path mints a portal token over HTTP first), so
the dial path cannot hold an actor-isolated object; a value also means a
resolution test needs no storage, no clock and no network. A session that was
never handed one holds `.empty`, every name answers `notInDirectory`, and
dialling is address-only — the behaviour that existed before the directory did.

`CallSession.canDial` is the single gate the Connect button and the dial itself
both consult, so the two cannot disagree about whether a field is complete.

One correction to the sketch above: the grammar (`NAME`, `NAME module`,
`NAME/module`) is shared by D-Star and M17 rather than being a D-Star path —
`ReflectorDialText` splits the text and knows nothing about either.

That prediction held when `Network.dstar` landed: the name grammar gained
nothing. What it *did* need was the address half. While D-Star was reachable
only through the directory there was no `DStarDial` type and no reason for one;
a picker segment means an operator can type a bare address into the field, and
refusing to parse one would be a worse answer than parsing it. So the address
grammar moved to `ReflectorAddressDial`, which both networks now share —
`M17Dial` and `DStarDial` are the same parser with different default ports
(17000 and 30001), because the networks differ in protocol, not in how someone
types a host.

### The module: no default, because there is nothing to base one on

D-Star needs a module and the XLX registry does not publish which modules are
active — `modules` is empty for every D-Star entry. So the client cannot offer a
populated picker; it can only accept a letter.

An earlier draft of this design defaulted the omitted module to **A**, on the
reasoning that a default you can see is not a silent guess. That was wrong, and
the reason is worth writing down: on D-Star the module *is* the room. Guessing it
does not fail visibly — it succeeds, and puts you in a conversation you did not
mean to join, keyed up under your own callsign. There is no data behind the
guess, and "usually A" is not knowledge.

So there is no default. Typing a bare `XLX836` resolves the reflector and leaves
the module unset; **Connect stays disabled until a letter is chosen.** This is
not an error state — nothing is wrong, the form is simply incomplete, and it
should read that way: the resolved target line fills in as soon as the module
does.

```
XLX836 ·  module —  · 45.56.69.219:30001      Connect disabled
XLX836 ·  module A  · 45.56.69.219:30001      Connect enabled
```

The last module used per reflector is remembered and pre-selected on return, so
the cost lands once per reflector rather than once per call. That is a
recollection, not an assumption — it is a thing you actually did. (Not built
yet; it needs the picker, which is a separate item.)

Two cases the draft did not cover, settled in astar-refl-ship:

* **A half-typed module** (`XLX836 AB`) resolves as `needsModule`, not as a
  miss. The reflector is still right and only the letter is unusable, so the
  resolved line must stay on screen while the operator fixes the letter rather
  than flickering out to "not a reflector" and back.
* **Networks with no module at all** — YSF, NXDN, P25 — are complete without
  one. Which dials address a module is a property of the `kind`
  (`ReflectorDial.addressesModule`), never of whether `modules` happens to be
  populated: that array is what the publisher *listed*, it is empty for every
  D-Star row, and reading emptiness as "no module needed" is precisely how a
  client ends up dialling into a room nobody chose.

## 4. Browsing and search

A magnifying-glass button beside the dial field opens a sheet over the popover —
not a window, and not an expansion of the popover, which is already tight (the
status row measures 243 pt).

The sheet is a searchable list: a search field, a network filter that defaults to
the currently selected network, and rows showing name, country and description.
Selecting a row fills the dial field and dismisses; it does not connect. Dialling
stays a deliberate second action, consistent with the rest of the app.

Search is local and substring, case-insensitive, over `id`, `name`, `aliases`,
`description`, `country` and `sponsor`. The whole set is a few hundred KB — there
is no reason to ask the network anything at dial time.

Entries whose `dial` is `nil` or `.unsupported` render disabled with a short
reason. They are still findable, because "astar can see it but can't dial it" is
information, and silently omitting them makes the app look wrong rather than
honest.

### How it is built (astar-refl-ui)

`ReflectorSearchPane` is a **pane of the main window**, not a sheet
(astar-5a41): the magnifying glass beside the dial field swaps the window over
to it, and a Back chevron — the same one Settings uses, on the same ⌘[ — swaps
back. It began life as a sheet on the reasoning that 3,000 rows should not grow
a 330 pt popover, but the window is resizable, Settings had already set what a
full-window pane looks like here, and browsing a directory is not a decision the
app is blocked on, which is the only thing a sheet is for.

The pane has **two steps, not one**: pick a reflector, then pick a room. The
second step only appears for the networks that address one
(`ReflectorModuleOptions.options(for:)` returns empty for YSF, NXDN and P25, and
those fill the field and leave straight away).

The offer is the published `modules` list when there is one — M17 and URF rows
carry them — and A–Z when there is not, which is every D-Star row. That is not
a guess at which rooms are live: it is the protocol's range, with the operator
supplying the knowledge the registry does not publish. The picker says so, in
those words, rather than leaving a bare grid to imply otherwise.

The pane hands back **dial text, not a target**. `onSelect` writes a string
into the same field the keyboard types into, so there is exactly one place
holding what Connect will dial — two places each holding half of it is how a UI
comes to display `XLX836 A` and dial `XLX836`.

Descriptions are stripped of markup before display (`DirectoryEntry
.plainDescription`). Several upstream rows carry HTML verbatim because the
registries behind them feed web dashboards, and `Text` renders that as literal
angle brackets — one reflector's sponsor reading as source code in the picker.

The same module picker is offered inline under the dial field whenever typed
text resolves to `needsModule`, so the typed path does not dead-end at a
correct-but-unfinished line with nowhere to finish it. The last module used per
reflector is remembered (`ReflectorModuleMemory`, keyed on `network:id` because
bare ids collide across networks) and shown *marked* in the menu — never
pre-applied. A recollection the operator can see and repeat is a different
thing from a default, and only the first one is compatible with §3.

The magnifying glass appears only for a network that has a directory
(`Network.reflectorNetwork`). AllStar nodes are not reflectors and never appear
in this data, so offering to search it there would be an empty promise.

## 5. Sync

### The button

Settings gains a **Reflector directory** section:

```
Reflector directory
953 D-Star · 104 M17 · 1,432 YSF
Last synced 3 days ago                        [ Sync Now ]
Data from DVRef and the XLX registry (CC BY 4.0)
```

The attribution line is not decoration — CC BY requires it, and a credit that
lives only in a repo file is one refactor from being lost.

### The policy

| Trigger | Behaviour |
|---|---|
| App launch | If the cache is older than `client_refresh_days` (currently **7**), sync in the background. Never blocks the UI. |
| Manual button | Always permitted, but debounced to once per hour so a frustrated user cannot hammer a volunteer-run CDN. |
| Failure | Keep the last good copy. Surface quietly in Settings; never a modal. |

**Automatic sync never runs more often than `client_refresh_days`.** Reflector
addresses move on a scale of weeks; nightly polling from every install would cost
bandwidth and buy nothing. The server publishes the number, the client obeys it,
and the manual button exists precisely so the throttle never has to be loosened
for the impatient case.

Sync is a conditional GET (`If-None-Match` / `If-Modified-Since`). An unchanged
directory costs a 304, which matters because the upstream build is deliberately
byte-stable — most weeks there is genuinely nothing to send.

The `User-Agent` identifies astar and its version. hamcall-db's upstreams ask for
that so a misbehaving client can be contacted rather than blocked, and astar
should extend the same courtesy to hamcall-db.

### How it is built (astar-refl-ui)

`ReflectorSettingsView` is the section, and the launch-time sync is one line in
`AppDelegate.applicationDidFinishLaunching` — started and forgotten, so it can
never delay launch:

```swift
Task { @MainActor in try? await reflectors.sync(trigger: .automatic) }
```

Fire-and-forget is why the section had to exist first. That call has nobody to
throw at, so `ReflectorDirectory.lastSyncError` records the failure and the
section is the one place it surfaces — quietly, beside a directory that is
still loaded and still dialable. A background fetch with nowhere to report is a
fetch nobody can debug.

It is cheap in the common case: `.automatic` returns `.skipped(.notDue)`
*without opening a socket* until `client_refresh_days` has passed since the last
definitive answer. The section shows that number back as a sentence — "Checks
again in 6 days" — computed from `nextAutomaticSync`, which reads the cadence
off the loaded feed every time it is asked. It is the only place an operator can
tell that a directory sitting still for a week is policy rather than breakage.

The freshness line has a case of its own for an install that has never reached
the network: "Bundled with astar — never synced". "Never synced" on its own
reads as a failure, and it is not one — there is a complete directory loaded,
and naming which one is the difference between reassurance and alarm.

## 5b. D-Star, the first network the directory actually unlocked

The directory shipped listing 944 D-Star reflectors that astar could not dial,
because `Network` had no D-Star case — browsable and honest, and useless. That
is now closed, and almost none of the work was in the directory.

**The engine was already there.** `Station.dstar_connect` / `dstar_disconnect` /
`dstar_available` and the whole DExtra path have existed since iax-a9d4 and
iax-2f6b, and the C-ABI and Swift binding expose all of it. What was missing
was the four layers above: `CallSnapshot` did not carry the flags,
`StationDriving` had no methods, `Network` had no case, and `CallSession` had
no arm. Adding them is what this was.

**Availability is hardware, not a build flag.** D-Star voice is AMBE and astar
ships no software vocoder, so `dstarAvailable` is true only while a ThumbDV is
attached — the segment appears when the dongle is plugged in and not otherwise.
That is the honest gate: a picker entry that always failed to connect would be
worse than no entry. The engine memoizes its probe process-wide, so a dongle
plugged in *after* launch is not noticed until the next one.

**The reflector callsign is the one genuinely new argument.** D-Star transmits
the destination in the RF header's `RPT1`/`RPT2`, and the engine derives it
from the hostname when told nothing (`xlx836.…` → `XLX836` → `XRF836` on the
DExtra wire). That derivation is right for a reflector reached by its published
hostname and *impossible* for one reached by a bare IP — and the feed publishes
plenty of those; XLX836 itself is `45.56.69.219`. So a directory dial always
passes the callsign the feed gave it, and only the typed-address path leaves it
to the engine, with `nil` meaning "derive it" rather than a guess going out on
the air.

**One callsign, not one per network.** M17 sends it in every frame and D-Star
puts it in every header, and it is the same string, so it is one field:
`CallSession.operatorCallsign`. It is backed by the existing `m17.callsign`
defaults key, deliberately — renaming the key would be a `ConfigVersion` bump
plus a translation in both directions, bought for nothing, because the value is
already exactly this and no reader would misread it. The name is an
implementation detail; the meaning never changed.

**Last heard, not talking now.** A D-Star call publishes `dstarTalker` and
`dstarSlowText`, shown under the connected-node line. Both persist past
end-of-transmission by design — the status dot and the RX meter say who is
keyed *now* — and both clear on every path a session can end by, because a
talker callsign left on screen after the link drops is a claim about the
present that is no longer true. The slow-data text is typed by whoever is
transmitting on the reflector: render it, never interpret it.

**The failure message does not come from the engine.** `iax_error_text(-19)` is
the static string `"dstar error"`, so the engine's precise classification
("ThumbDV at /dev/cu.usbserial-… is busy — another process has it open") never
crosses the C-ABI. `connectFailureMessage` names the three real causes instead,
which is more use than an error code and more honest than picking one it cannot
distinguish. An engine-side last-error accessor would beat it; until then, this.

**Still open:** a per-network audio profile for D-Star, the equivalent of M17's
TX overrides (`M17AudioOverrides`). D-Star currently runs on the shared audio
chain.

## 6. What this gives the other networks

Adding YSF or DMR later needs **no new UI**:

* YSF entries already carry `dial.kind = "ysf"`; implement the case and the
  picker, the search pane and the sync button all work unchanged.
* DMR entries carry `requires: ["dmr_id", "password"]`. The client reads that and
  prompts, rather than the schema pretending a public file can hold a per-user
  credential. Until the DMR protocol is implemented the entries list as
  `.unsupported`, which is accurate rather than absent.

The extension point is the `dial.kind` switch and nothing else.

## 7. Out of scope here

* The D-Star protocol work itself — `reflector_callsign` plumbing is separate.
* Writing to the directory. This is read-only; reflector registration belongs to
  DVRef and the XLX registry, not to astar.
* Per-entry live status (users connected, last heard). The upstreams publish some
  of it, but it is exactly the data that goes stale in a weekly cache, and
  showing a stale user count is worse than showing none.
