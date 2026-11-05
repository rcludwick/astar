# Vendored: `ham-digital-modes-ambe`

This directory holds the AMBE vocoder tree of Bruce Perens' `ham_digital_modes`
crate: a pure-Rust implementation of D-Star's AMBE 3600×2400 vocoder (2400 bit/s
of voice plus 1200 bit/s of forward error correction, 72-bit frames, 20 ms
each), encoder and decoder. astar links it when the `dstar-soft-ambe` feature
of `astar-codec` is on. Every shipped build turns that on (Rob, 2026-11-01): the
macOS app, statically, through `bindings/swift/build-xcframework.sh`, and
astar-server's `release` set. A plain `cargo build` does not. It gives D-Star a
software vocoder beside the ThumbDV / DVstick dongle; which one D-Star uses is
the operator's choice in config, and the dongle stays the default choice.

| | |
|---|---|
| Upstream repository | <https://github.com/BrucePerens/hams_open> |
| Upstream path | `daemons/ham_digital_modes/` (crate `ham_digital_modes` 0.1.0) |
| Upstream commit | `f3c12933ff17bcd2a710adc11c64ca407f8f0e12` (2026-11-02 00:38 UTC) |
| Last upstream change to `src/ambe/` at that commit | `ddaa6fb7996a53244b72aa8d561291bb7d89fee5` (2026-10-29) |
| Vendored | 2026-11-01 |
| Licence | `LGPL-3.0-or-later` (upstream's `Cargo.toml`, `LICENSE`, every file's SPDX header, and the repository's `LICENSING.md`) |
| Author | Bruce Perens |

## Why it is here, and why only this part

Rob decided on 2026-11-01 (backlog item `astar-dstar-perens`) to offer D-Star a
software vocoder beside the dongle. astar's rule (CLAUDE.md, "D-Star / AMBE")
allows a software vocoder only where the patent term has expired and the licence
is AGPL-compatible, which today means D-Star's AMBE 3600×2400 and nothing else.

Upstream's crate also implements AMBE+2 half rate (DMR, System Fusion, NXDN),
which still reads on US 8,359,197 until 2028-05-20. Upstream gates it behind an
off-by-default `ambe_plus_2` feature. **astar does not vendor it at all**: the
two `ambe_plus_2` directories are deleted from this copy, and the manifest does
not declare the feature, so `--features ambe_plus_2` is a build error.
`ci/guard-dstar-soft-ambe.sh` fails if either ever comes back.

The rest of upstream's crate (FT8, WSPR, PSK31, RTTY, Codec 2, the vendored C
`ft8_lib` and its build script, the examples, the test fixtures) is not needed
and is not here either.

## What is here, and what changed

Every difference from upstream, exhaustively:

* **`src/ambe/`** is upstream's `daemons/ham_digital_modes/src/ambe/` at the
  commit above, byte for byte, **minus** `src/ambe/float/ambe_plus_2/` and
  `src/ambe/fixed/ambe_plus_2/` (23 files) — the AMBE+2 code described above.
  No remaining file is edited. The two `#[cfg(feature = "ambe_plus_2")] pub mod
  ambe_plus_2;` lines in `float/mod.rs` and `fixed/mod.rs` are upstream's and
  stay; with the feature undeclared they are always false. A handful of doc
  comments still name `ambe_plus_2` items; they are comments only.
* **`src/lib.rs`** is new (astar's, under this crate's LGPL-3.0-or-later):
  `pub mod ambe;`. Upstream's own `lib.rs` declares the modes not vendored
  here, so it could not be kept; the module paths inside `src/ambe/`
  (`crate::ambe::…`) resolve exactly as they did upstream.
* **`Cargo.toml`** is astar's. It hard-codes upstream's licence and version,
  declares no dependencies (the AMBE tree uses `std` only) and no features,
  caps rustc warnings the way Cargo does for a crates.io dependency, tells
  rustc the `ambe_plus_2` cfg name is known, and ends with an empty
  `[workspace]` table so Cargo never adopts it into an enclosing workspace.
* **`LICENSE`** is upstream's crate `LICENSE`, unchanged.
  **`LICENSE-LGPL-3.0.txt`** and **`LICENSE-GPL-3.0.txt`** are upstream's
  `licenses/LGPL-3.0.txt` and `licenses/GPL-3.0.txt`, unchanged (LGPL-3.0 is
  an addendum to GPL-3.0, so both apply).
* **`rustfmt.toml`** (`disable_all_formatting = true`), **`.gitignore`** and
  **`VENDORED.md`** (this file) were added by astar.

Because files were removed and `lib.rs` replaced, this is a modified version of
upstream's crate in the LGPL's sense. LGPL-3.0 section 4 and GPL-3.0 section
5(a) ask for the modifications to be marked: this file is that notice, dated
2026-11-01, and the only modifications are the removals and the new `lib.rs`
and `Cargo.toml` listed above.

To check the copy, fetch upstream at the commit above and run
`diff -r daemons/ham_digital_modes/src/ambe vendor/ham-digital-modes-ambe/src/ambe`:
the only output is the two `Only in …: ambe_plus_2` lines.

## Where upstream's numbers came from

Upstream records its sources in the code's own comments, and they are worth
repeating here because two of them carry notices:

* The D-Star quantiser tables (`float/dstar/tables*.rs`, `T5TAB`–`T7TAB` in
  `float/dstar/decode.rs`) and the frame's bit mapping are transcribed from
  **mbelib** (<https://github.com/szechyjs/mbelib>, `ambe3600x2400_const.h`,
  `ambe3600x2400.c`), and the over-the-air interleave tables
  (`float/dstar/interleave.rs`) were checked against **DSD**
  (<https://github.com/szechyjs/dsd>, `include/dstar_const.h`). Both are under
  the ISC licence; their notices are below. (DSD's `dstar_header`,
  `descramble.h` and `fcs.h` are GPL-2.0, but those are D-Star header and
  scrambler code, not voice, and nothing here comes from them.)
* Where the chip and mbelib disagree, upstream measured the real DVSI
  AMBE-3000R and followed the chip (see `float/dstar/mod.rs`).
* The shared analysis and synthesis code follows the TIA-102.BABA IMBE
  standard, transcribed from the standard and cross-checked against
  `kchmck/imbe.rs` (MIT) and OP25's `imbe_vocoder`.

None of it refers to or derives from `rcludwick/ambe`, which astar never
references or vendors.

mbelib's notice (its `COPYRIGHT` file):

```
Copyright (C) 2010 mbelib Author
GPG Key ID: 0xEA5EFE2C (9E7A 5527 9CDC EBF7 BF1B  D772 4F98 E863 EA5E FE2C)

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND ISC DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS.  IN NO EVENT SHALL ISC BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

DSD's notice for the code outside the three GPL-2.0 files (its `COPYRIGHT`
file):

```
Copyright (C) 2010 DSD Author
GPG Key ID: 0x3F1D7FD0 (74EF 430D F7F2 0A48 FCE6  F630 FAA2 635D 3F1D 7FD0)

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND ISC DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS.  IN NO EVENT SHALL ISC BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE
OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

## How it is wired in

The root `Cargo.toml` names it as a workspace dependency and lists
`vendor/ham-digital-modes-ambe` in the workspace's `exclude`, so
`cargo clippy --workspace -D warnings` and the workspace lints never judge it,
and the local `rustfmt.toml` stops `cargo fmt --all` reformatting it. It builds
as an ordinary path dependency under its own edition (2021).

Only `astar-codec`'s `dstar-soft-ambe` feature reaches it (re-exported by
`astar-console`, `astar-station`, `astar-sys` and `astar-server` under the same
name). `crates/astar-codec/src/ambe_soft.rs` wraps its float D-Star encoder and
decoder in astar's `AmbeStream` trait, and refuses every mode but D-Star.
`ci/guard-dstar-soft-ambe.sh` fails if a plain `cargo build` of any of those
crates pulls this one in, or if the shipped recipes stop turning it on.

## Do not inherit workspace metadata

`Cargo.toml` hard-codes `license`, `version`, `edition` and `repository`.
**Never** convert them to `.workspace = true`: `license` would resolve to
`AGPL-3.0-only` and silently relicense someone else's LGPL code. Do not add
astar's AGPL SPDX header to any file here either (`ci/guard-spdx-headers.sh`
exempts `vendor/`).

## Updating

1. Fetch upstream at the new commit (a sparse checkout of
   `daemons/ham_digital_modes` and `licenses` is enough).
2. Replace `src/ambe/` with upstream's, then delete
   `src/ambe/float/ambe_plus_2/` and `src/ambe/fixed/ambe_plus_2/` again.
   Check nothing outside those directories started to need them
   (`cargo build` here fails if so) and that the crate still has no
   dependencies.
3. Check the licence is still `LGPL-3.0-or-later` in upstream's `Cargo.toml`,
   `LICENSE` and the SPDX headers, and re-read `LICENSING.md`. If it changed,
   `LICENSE-EXCEPTIONS.md` section 9 changes with it.
4. Re-read the source comments for any new third-party origin, and add its
   notice above.
5. Update the table above (commit, dates).
6. Run `just ci`, `./ci/guard-dstar-soft-ambe.sh` and `just soft-ambe-check`.

Local source edits are discouraged — fix upstream and re-vendor. If one ever
has to ship, list it under "What is here, and what changed" with its date.
