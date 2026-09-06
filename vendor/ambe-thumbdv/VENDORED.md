# Vendored: `ambe-thumbdv`

This directory is a **verbatim vendored copy** of the `ambe-thumbdv` crate — the
DVSI AMBE-3000 packet driver that talks to a ThumbDV / DV3000 USB dongle. It is
the hardware vocoder path for D-Star; nothing else in this repository can encode
or decode AMBE.

| | |
|---|---|
| Upstream | <https://github.com/rcludwick/ambe> |
| Path upstream | `crates/ambe-thumbdv` |
| Revision | `41c35d817af39016707f58b1bbbc6b1bfaeedabe` (2026-09-06) |
| Licence | MIT OR Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`) |
| Copyright | Rob Ludwick |

## Why it is vendored

The rest of this workspace is AGPL-3.0-only. `ambe-thumbdv` is dual MIT/Apache
and stays that way: it lives under `vendor/` rather than `crates/` so the
licence boundary is visible in the tree, and its `Cargo.toml` hard-codes
`license`, `version`, `edition` and `repository` instead of inheriting them from
the workspace — inheriting would resolve `license` to `AGPL-3.0-only` and
silently relicense someone else's terms.

Vendoring also removes the last `git = "https://github.com/rcludwick/ambe"`
dependency from the build, so a clean checkout builds fully offline.

`astar-codec` consumes it as a path dependency behind the `ambe-hw` feature:

```toml
ambe-thumbdv = { path = "../../vendor/ambe-thumbdv", optional = true }
```

## What is *not* vendored

The `ambe-core` / `ambe-dstar` **software** vocoder crates from the same
upstream repo were deliberately dropped. D-Star in this repository is
hardware-only (ThumbDV); the `ambe-soft` feature and the `SoftAmbe` backend that
used them no longer exist.

## Updating

Copy `src/` from upstream verbatim, keep this file's revision row current, and
do **not** let the four hard-coded metadata keys revert to `.workspace = true`.
Local edits are discouraged — fix upstream and re-vendor, so the two copies do
not drift.
