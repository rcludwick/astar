# Vendored: `mp3lame-sys`

FFI bindings to LAME, with the **LAME 3.100 C source bundled**. It is what
astar-server's `/stream.mp3` encodes with, behind the `stream-mp3` feature,
which is never on by default.

| | |
|---|---|
| Upstream crate | [`mp3lame-sys`](https://crates.io/crates/mp3lame-sys) 0.1.11 |
| Repository | <https://github.com/DoumanAsh/mp3lame-sys> |
| Crate checksum | sha256 `54e3b1772db47828840702e5a2e05694527f731abadf9b931355d54035f019d8` (the `.crate` from crates.io) |
| Bundled C library | LAME 3.100, `lame-3.100/`, unmodified (<https://lame.sourceforge.io/>) |
| Licence | crate: LGPL-3.0; LAME: LGPL-2.0-or-later (`lame-3.100/COPYING`, `LICENSE`) |
| Vendored | 2026-10-12 |

## Why it is vendored

astar is AGPL-3.0-only; this is a second licence island beside
`vendor/ambe-thumbdv`. Its `Cargo.toml` hard-codes `license`, `version`,
`edition` and `repository`: **never** convert them to `.workspace = true`,
which would relabel LGPL code as AGPL.

Vendoring also lets the build drop autotools (below), so the server's static
musl release builds on any machine with a C compiler.

LGPL obligations for the binaries that link it statically — the notice, the
source (this directory), and the right to relink — are in
`LICENSE-EXCEPTIONS.md`. Nothing links it unless `stream-mp3` is turned on,
and `ci/guard-codec2-licensing.sh` fails a default build that does.

## Patches

The first commit (`vendor: mp3lame-sys 0.1.11 and mp3lame-encoder 0.2.5,
unmodified`) is the crate as published; every change since is one of these.

1. **`build.rs` compiles LAME with `cc` on every target.** Upstream runs
   autotools `configure` and `make` on unix (and `cc` only on Windows), which
   needs autoconf, make and a shell, and does not cross-compile to musl. The
   new `build.rs` compiles the same `lame-3.100/libmp3lame/*.c` list with the
   `cc` crate. `TAKEHIRO_IEEE754_HACK` is defined on x86 only, and
   `vector/xmm_quantize_sub.c` is added only on x86 with SSE.
2. **`astar/config.h`** stands in for the `config.h` that `configure` writes:
   standard C99 headers present, `ieee754_float32_t` as `float`.
3. **`Cargo.toml`:** the `autotools` (unix) and `cc` (Windows) build
   dependencies become one `cc` build dependency; `astar/**` is added to
   `include`; the crate's own `Cargo.lock` is removed (the workspace lock
   governs).

4. **`rustfmt.toml`** (new) sets `disable_all_formatting`, so the
   workspace's `cargo fmt --all` leaves upstream code as published.

`src/lib.rs` (the bindings) and `lame-3.100/` are untouched.

The `decoder` feature (LAME's `mpglib`) is never enabled by astar; upstream's
README describes that code as GPL.

## Updating

Copy the new crate from `~/.cargo/registry/src/*/mp3lame-sys-<v>` over this
directory in a commit of its own, then re-apply the patches above and update
the table. Run `cargo test -p mp3lame-encoder --test astar_smoke`.
