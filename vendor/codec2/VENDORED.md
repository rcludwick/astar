# Vendored: `codec2`

This directory is a **verbatim vendored copy** of the `codec2` crate, a
pure-Rust port of David Rowe's Codec 2 — the M17 voice codec. astar links it
statically only when the opt-in `codec2-static` feature of `astar-codec` is on,
which is how the macOS app, and astar-server's static release (the `release`
feature), get M17 voice without a system `libcodec2`.

| | |
|---|---|
| Upstream crate | <https://crates.io/crates/codec2> |
| Upstream repository | <https://github.com/scriptjunkie/codec2> |
| Version | `0.3.1` |
| Upstream commit | `24ec3b7c5b9892c268e180802ee3a5ce7737f214` (from the published crate's `.cargo_vcs_info.json`) |
| crates.io checksum | `3c3c8ba70382a99c0b5ebadf6ad50e7d98fb6eb9bcc4c0e30d422f6321798d99` (the `Cargo.lock` entry before vendoring) |
| Vendored | 2026-10-12 |
| Licence | `LGPL-2.1-only AND MIT` (upstream's declaration, in `Cargo.toml`) |
| Authors | David Rowe; Matt Weeks <scriptjunkie@scriptjunkie.us> |

## Why it is vendored

Rob decided on 2026-10-12 that the LGPL source the macOS app links statically
should live in this repository, not only behind a crates.io reference. A
recipient of a released binary then gets the exact library source in the same
tagged tree as astar's own, which is what LGPL-2.1's relink provision is about
(see `LICENSE-EXCEPTIONS.md` section 2).

The rest of the workspace is AGPL-3.0-only. This crate keeps upstream's terms,
and lives under `vendor/` rather than `crates/` so the licence boundary is
visible in the tree.

## What is here, and what changed

Copied from the published `codec2-0.3.1` crate. `src/` and `README.md` are
byte-for-byte identical to both the crates.io package and upstream git at the
commit above, and `.gitignore` is upstream's.

Every difference from upstream, exhaustively:

* **`Cargo.toml`** is upstream's original manifest (the crate's
  `Cargo.toml.orig`, identical to `Cargo.toml` in upstream git), not cargo's
  normalized publish copy. No upstream key or value changed. astar added a
  `# NOTE (astar):` comment at the top of `[package]`; a `[lints.rust]`
  table setting `warnings = "allow"`, which restores the `--cap-lints allow`
  Cargo applies to every crates.io dependency but not to a path dependency
  (otherwise upstream's dead-code warning lands in every astar build log); and
  an empty `[workspace]` table at the end (with a comment) so the crate is its own
  workspace root — without it, Cargo walks up from this directory looking for
  a workspace, and in a git worktree nested inside the main checkout it finds
  the main checkout's `Cargo.toml` and refuses to build.
* **`LICENSE-LGPL-2.1`** was added by astar: the unmodified text of the GNU
  Lesser General Public License v2.1, from
  <https://www.gnu.org/licenses/old-licenses/lgpl-2.1.txt>. Upstream ships no
  licence file at all, in the crate or in its git repository; the licence is
  declared only in `Cargo.toml` and the opening of `src/lib.rs`. No MIT text was
  added, because upstream names no MIT copyright holder and inventing one would
  misstate their notice.
* **`rustfmt.toml`** was added by astar: `disable_all_formatting = true`.
  `cargo fmt --all` formats local path dependencies as well as workspace
  members, and would otherwise rewrite upstream's source (and fail
  `just fmt-check`).
* **`VENDORED.md`** (this file) was added by astar.

No source file is modified, so no modified-source notice under LGPL-2.1
section 2 is owed.

## How it is wired in

The root `Cargo.toml` points the workspace dependency here:

```toml
codec2 = { path = "vendor/codec2", version = "0.3.1" }
```

and lists `vendor/codec2` in the workspace's `exclude`. That keeps it out of
the workspace's members, so `cargo clippy --workspace -D warnings` and the
workspace lints never judge it, and the local `rustfmt.toml` stops
`cargo fmt --all` reformatting it; it builds as an ordinary path dependency,
under its own edition (2018). Only workspace members go through clippy; this
crate is compiled by plain rustc with its warnings allowed, exactly as it was
when it came from crates.io.

Only `astar-codec`'s `codec2-static` feature reaches it. A default build links
none of it, and `ci/guard-codec2-licensing.sh` fails if that ever changes.

## Do not inherit workspace metadata

`Cargo.toml` hard-codes `license`, `version`, `edition`, `repository` and
`authors` with upstream's values. **Never** convert them to
`.workspace = true`: `license` would resolve to `AGPL-3.0-only` and silently
relicense someone else's LGPL code. Do not add astar's AGPL SPDX header to any
file here either (`ci/guard-spdx-headers.sh` exempts `vendor/`).

## Updating

1. Fetch the new release, e.g. `cargo vendor` into a scratch directory, or
   from `~/.cargo/registry/src/*/codec2-<version>/` after a build that uses it.
2. Replace `src/`, `README.md` and `.gitignore` verbatim.
3. Copy the release's `Cargo.toml.orig` over `Cargo.toml`, then put back the
   `# NOTE (astar):` comment, the `[lints.rust]` table and the trailing empty
   `[workspace]` table. Keep
   `rustfmt.toml` and `LICENSE-LGPL-2.1`. Check the licence is still
   `LGPL-2.1-only AND MIT`; if it changed, `LICENSE-EXCEPTIONS.md`, `README.md`
   and `docs/site/about/license.md` change with it.
4. Bump `version` in the root `Cargo.toml`'s `codec2` entry, and every row of
   the table above (version, commit, checksum, date).
5. Run `just ci`, `./ci/guard-codec2-licensing.sh`, and
   `cargo test -p astar-codec --features codec2-static`.

Local source edits are discouraged — fix upstream and re-vendor, so the copies
do not drift. If an edit ever has to ship, LGPL-2.1 section 2 applies: list it
in "What is here, and what changed", mark the changed files with a dated
notice, and say so in `LICENSE-EXCEPTIONS.md`'s Modifications part.
