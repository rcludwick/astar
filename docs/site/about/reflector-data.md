---
icon: lucide/database
---

# Reflector data

astar's reflector list comes from **hamcall-db**, a static JSON directory of
digital-voice reflectors that needs no API token and no account. This page
records where that data comes from, the attribution it carries, and how astar
is designed to consume it.

!!! note "Where this stands"

    Everything under [Where the data comes from](#where-the-data-comes-from) is
    live — the files are built nightly and served now, and anyone can fetch
    them.

    The macOS app reads the directory: it ships with a bundled snapshot, syncs
    on its own cadence (with a sync button in Settings), keeps a cached copy on
    disk, and resolves a reflector typed by name before any address parser
    runs. D-Star and System Fusion — the networks the directory was built for
    first — are both in the app, on a DVMEGA DVstick 30 or a ThumbDV / DV3000
    dongle. Networks the app cannot dial yet (DMR) are listed and counted, not
    hidden.

## Where the data comes from

hamcall-db aggregates the upstream reflector registries once, on a schedule, and
publishes the result as files on a CDN:

| Endpoint | What it is |
|---|---|
| `https://rcludwick.github.io/hamcall-db/api/v1/reflectors.json` | Every reflector on every covered network. The primary endpoint. |
| `https://rcludwick.github.io/hamcall-db/api/v1/reflectors/{network}.json` | One network on its own, so a client that only speaks D-Star need not re-download the rest to learn nothing changed. |
| `https://rcludwick.github.io/hamcall-db/api/v1/index.json` | A manifest: which networks exist, how many rows each holds, how fresh they are. |

Coverage today is **D-Star, M17, YSF, NXDN, P25 and URF**. astar implements two
of those; the rest are listed because the directory is not astar-specific, and
because a network astar cannot dial is still information worth showing rather
than hiding.

The point of the arrangement is that the credential stays upstream. The
registries these files are built from want an API token and a registered
account; a client that spoke to them directly would have to ship one, which is
exactly what their terms forbid. Fetching a static file instead means **astar
needs no token, no account, and no per-user registration** — and that a fetch
cannot be rate-limited, and works from a cached copy when the network does not.

## Attribution

The directory is licensed **CC BY 4.0**, which makes attribution a condition of
use rather than a courtesy. Both lines below are required wherever astar
presents the data, and are surfaced in the app rather than buried in a
repository file:

> Reflector data provided by DVRef — <https://dvref.com/>

> XLX reflector data from the XLX registry maintained by Luc Engelmann, LX1IQ

DVRef covers M17, YSF, NXDN, P25, URF and XRF; the XLX registry is the source of
record for D-Star, where it lists an order of magnitude more reflectors than any
other public directory.

## How astar will use it

!!! info "Planned — see `docs/app/design/reflector-directory.md` in the repository for the full design"

    None of this is implemented. It is written down here so the data's
    obligations and refresh policy are on record before the code exists rather
    than after.

**Caching and refresh.** Every file carries a `client_refresh_days` field —
currently **7**. astar is designed to cache the directory for that long and to
treat the number as data rather than a compiled-in constant: if the publisher
changes the cadence, clients follow without a release. A manual **Sync now**
button will exist for the impatient case, which is precisely why the automatic
schedule never needs loosening — **astar will not poll more often than
`client_refresh_days` says**. Reflector addresses move on a scale of weeks;
nightly polling from every install would cost a volunteer-run service bandwidth
and buy nothing.

Refreshes will be conditional requests, so an unchanged directory costs a `304`
rather than a download, and a failed sync will keep the last good copy instead
of emptying the list.

**Dialling and browsing.** The directory is what will let a reflector be reached
by name — resolving it to a host, port and callsign locally, with no network
round-trip at dial time — and what will back a searchable reflector list in the
client.

**What it does not change.** The directory only resolves names to addresses.
Connecting and keying remain deliberate manual actions, as described in
[On-air safety](safety.md); nothing here dials anything on its own.
