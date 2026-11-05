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

Five paths in the repository, and two upstream dependencies (plus a system
`libcodec2`, if the opt-in `codec2-runtime` feature loads one), are **not**
AGPL-3.0. This list matches the table in the repository's `README.md`; if the
two ever disagree, the licence files shipped alongside the code win.

| Path or crate | Component | Licence |
|---|---|---|
| `vendor/ambe-thumbdv` | ThumbDV / DV3000 AMBE driver | MIT **OR** Apache-2.0 |
| `vendor/codec2` / system `libcodec2` | Codec 2 — the M17 voice codec (the `codec2` crate, vendored unmodified). Opt-in features only | `LGPL-2.1-only AND MIT` / LGPL-2.1 |
| `vendor/ham-digital-modes-ambe` | Bruce Perens' software D-Star vocoder (AMBE 3600×2400), without its AMBE+2 code. Linked into the macOS app and astar-server's static release | `LGPL-3.0-or-later` |
| `apps/gui/assets/fonts` | The Inter typeface, bundled with the Iced client | SIL Open Font License 1.1 (`LICENSE-Inter.txt`) |
| `harness/asterisk_parity/c_iaxclient/vendored/libiax2` | The historical C libiax2, used only as a parity reference by the test harness — never linked into a shipped binary | GPL v2 (`COPYING`) / LGPL v2 (`COPYING.LIB`) |
| `nnnoiseless` crate | RNNoise — mic noise reduction, compiled into every build | BSD-3-Clause |
| `oxideav-gsm` crate | GSM 06.10 — the EchoLink voice codec, also used for GSM-only AllStar nodes | MIT |

### `nnnoiseless` (RNNoise)

Mic noise reduction is `nnnoiseless`, a pure-Rust port of Xiph's RNNoise. It is
an ordinary crates.io dependency rather than vendored source, and the ~87 KB
model is a const array inside the crate — there is no runtime file to locate,
sign or bundle on any platform.

BSD-3-Clause is GPL-compatible, so a shipped astar binary is distributable under
the AGPL. The licence asks for attribution in return, and **five** copyright
lines ship together: the Rust port, the Mozilla-era RNNoise, Jean-Marc Valin's
original work, the Xiph.Org Foundation — whom the no-endorsement clause names
specifically — and Mark Borgerding's KISS FFT. All five, and the full licence
text, are reproduced in `LICENSE-EXCEPTIONS.md` §3.

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

### `vendor/codec2`

`vendor/codec2` is Codec 2, the M17 voice codec, as the pure-Rust `codec2`
crate (version 0.3.1). It is licensed **LGPL-2.1-only AND MIT** and keeps those
terms. Its source files are exactly as upstream published them; what astar
added, and why, is listed in its `VENDORED.md`.

It is only built when the opt-in `codec2-static` feature asks for it — the
macOS app and astar-server's static release (`just server-release`) do, a
plain `cargo build` does not. Because both link it statically, the library's
source ships in this repository next to astar's, so anyone can change it and
rebuild. Its notices:

* `vendor/codec2/LICENSE-LGPL-2.1`
* `vendor/codec2/VENDORED.md` — provenance: upstream crate and repository, the
  exact version, commit and checksum vendored, and how to update it.
* `LICENSE-EXCEPTIONS.md` §2 — the LGPL notices and the relink offer.

### `vendor/ham-digital-modes-ambe`

`vendor/ham-digital-modes-ambe` is Bruce Perens' software vocoder for D-Star's
AMBE 3600×2400: the AMBE part of his `ham_digital_modes` crate, from
`BrucePerens/hams_open`, at a pinned commit. It is licensed
**LGPL-3.0-or-later** and keeps those terms. astar removed upstream's AMBE+2
code (DMR, NXDN, System Fusion), which is still patented, and replaced its
`lib.rs` and `Cargo.toml`; everything else is as upstream published it.

It is built by the `dstar-soft-ambe` feature, which the macOS app and
astar-server's static release both turn on, so both link it statically and its
source ships in this repository next to astar's; anyone can change it and
rebuild. A plain `cargo build` leaves it out. D-Star uses it only when the
operator chooses it; the dongle is the default. Its notices:

* `vendor/ham-digital-modes-ambe/LICENSE`, `LICENSE-LGPL-3.0.txt`,
  `LICENSE-GPL-3.0.txt`
* `vendor/ham-digital-modes-ambe/VENDORED.md` — provenance, the exact commit,
  every change, and the ISC notices of mbelib and DSD, whose tables it uses.
* `LICENSE-EXCEPTIONS.md` §9 — the LGPL notices, the relinking terms, and the
  patent reasoning.

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
