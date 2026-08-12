---
icon: lucide/scale
---

# Licence

## The repository: AGPL-3.0-only

astar's own code is licensed under the **GNU Affero General Public License,
version 3.0 only** — the engine crates, `astar-server`, the macOS app, the Iced
client, the bindings, and these docs. Three vendored components keep their own
terms; they are listed under [Third-party components](#third-party-components)
below.

Copyright © 2026 Rob Ludwick.

The full text is in `LICENSE` at the root of the repository. Every first-party
`.rs`, `.swift`, `.sh` and `.py` file carries an SPDX header, and
`ci/guard-spdx-headers.sh` fails the build if one goes missing:

```
SPDX-License-Identifier: AGPL-3.0-only
```

Prose — these docs, the READMEs — is covered by the root `LICENSE` rather than
by per-file headers.

!!! info "What AGPL-3.0 means in practice"

    The AGPL is the GPL plus a network clause. If you modify astar and let
    other people use it **over a network** — a hosted node, a web front-end, a
    service — those users are entitled to the corresponding source of your
    modified version, not just the people you hand a binary to.

    Running an unmodified astar for yourself, or on your own node, carries no
    obligation at all.

    This is a summary, not legal advice. Read the licence.

## Third-party components

Three paths in the repository are **not** AGPL-3.0. This list matches the table
in the repository's `README.md`; if the two ever disagree, the licence files
shipped alongside the code win.

| Path | Component | Licence |
|---|---|---|
| `vendor/ambe-thumbdv` | ThumbDV / DV3000 AMBE driver | MIT **OR** Apache-2.0 |
| `apps/gui/assets/fonts` | The Inter typeface, bundled with the Iced client | SIL Open Font License 1.1 (`LICENSE-Inter.txt`) |
| `harness/asterisk_parity/c_iaxclient/vendored/libiax2` | The historical C libiax2, used only as a parity reference by the test harness — never linked into a shipped binary | GPL v2 (`COPYING`) / LGPL v2 (`COPYING.LIB`) |

### `vendor/ambe-thumbdv`

`vendor/ambe-thumbdv` is the ThumbDV / DV3000 AMBE vocoder driver — the D-Star
hardware path. It is **dual-licensed MIT OR Apache-2.0** and keeps those terms.

It lives under `vendor/` rather than `crates/` precisely so the licence boundary
is visible in the directory tree, and its own `Cargo.toml` states its licence
explicitly rather than inheriting the workspace's. Its notices ship with it:

* `vendor/ambe-thumbdv/LICENSE-MIT`
* `vendor/ambe-thumbdv/LICENSE-APACHE`
* `vendor/ambe-thumbdv/VENDORED.md` — provenance: upstream repository, the exact
  revision vendored, and why.

Copyright for that crate is also Rob Ludwick's; it is permissively licensed on
purpose, so it can be reused outside an AGPL project.

### `apps/gui/assets/fonts`

The Iced client embeds the **Inter** typeface, which is licensed under the SIL
Open Font License 1.1. The licence text ships with the fonts, in
`apps/gui/assets/fonts/LICENSE-Inter.txt`.

### `harness/asterisk_parity/c_iaxclient/vendored/libiax2`

A snapshot of the historical C `libiax2`, kept so the test harness can compare
astar's framing against the original implementation. It is **GPL v2 / LGPL v2**
(`COPYING` and `COPYING.LIB` in that directory) and is never linked into any
shipped astar binary — it exists for parity testing only.

## Third-party dependencies

astar builds on a large tree of Rust crates under their own (mostly MIT/Apache)
terms. `cargo metadata` is the authoritative list for any given checkout.

## No warranty

As stated in the licence: this software comes with **absolutely no warranty**.
It keys radio transmitters. You are the licensed operator, and the
responsibility for what your station emits is yours.
