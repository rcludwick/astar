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
recollection, not an assumption — it is a thing you actually did.

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

## 6. What this gives the other networks

Adding YSF or DMR later needs **no new UI**:

* YSF entries already carry `dial.kind = "ysf"`; implement the case and the
  picker, the search sheet and the sync button all work unchanged.
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
