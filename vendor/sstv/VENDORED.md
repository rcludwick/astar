# Vendored: `sstv`

A fork of the `sstv` crate, a no_std-capable SSTV (slow-scan television)
encoder and decoder. astar uses its encoder to make test signals and to send
pictures (astar-station's `sstv-tx`, which no shipped build turns on), and its
de-jitter to repair a received picture's timing before slowrx decodes it.

| | |
|---|---|
| Upstream | <https://github.com/unexcellent/sstv> |
| Version | 0.3.1 (tag `v0.3.1`) |
| Revision | `9761c4c61a6353d7ae2173e1579e434e6c1b1ce3` |
| Licence | MIT (see `LICENSE`), Copyright (c) 2026 Tobias Klockau |
| Vendored | 2026-11-19 |

The tag's `src/` is byte-identical to the crates.io 0.3.1 source.

## Why it is vendored

astar is AGPL-3.0-only; this is a licence island, like `vendor/ambe-thumbdv`.
Its `Cargo.toml` hard-codes `license`, `version`, `edition` and `repository`:
**never** convert them to `.workspace = true`, which would relabel MIT code as
AGPL. It is in the workspace `exclude` and has its own empty `[workspace]`, so
Cargo never adopts it into the enclosing workspace.

It is a fork, not a copy, because astar needs the scan timing the crate keeps
private, a way to build without the encoder, and a home for the de-jitter, so
one copy serves both slowrx (in astar) and the crate's own decoder. The
patches are kept small and separate so they can be offered upstream. Nothing is sent upstream from this
repository.

## What is vendored, and only that

`src/`, `build.rs`, `examples/`, `LICENSE`, `README.md`, `clippy.toml` and
`Cargo.toml`. Left out: upstream's `tests/` (its integration tests fetch
recordings from the internet and need a dozen dev-dependencies),
`rust-toolchain.toml` (it pins upstream's own toolchain), `Cargo.lock`, and CI
and repository files. The unit tests and doc tests in `src/` are all here and
run with `cargo test --manifest-path vendor/sstv/Cargo.toml`.

## Patches

The first commit (`vendor: sstv 0.3.1 (upstream 9761c4c), unmodified`) is the
crate as published; every change since is one of these.

1. **Public scan timing.** New `src/timing.rs`: `ScanTiming`, `SyncPosition`,
   `Mode::scan_timing()` and `Mode::duration()`, computed from the crate's
   private timing tables, so a receiver can find each line's sync pulse and
   map lines to picture rows. Files: `src/timing.rs` (new), `src/lib.rs`
   (`pub mod timing;`).
2. **`encode` feature.** `mod encode` and its re-exports (`Encoder`,
   `Synthesizer`) go behind `#[cfg(feature = "encode")]`, and `encode` joins
   `default`, so a decode-only build leaves the encoder out. Files: `src/lib.rs`,
   `Cargo.toml`, and `src/modes/mod.rs` (`value_frequency`, which only the
   encoder and the tests call, allows dead code without `encode`).
3. **`dejitter` feature.** `dejitter = ["std"]` turns on patch 5. File:
   `Cargo.toml`.
4. **Workspace isolation.** `Cargo.toml` gains an empty `[workspace]` table and
   a header comment; `rustfmt.toml` (new) sets `disable_all_formatting`, so
   `cargo fmt --all` leaves upstream's files as published.
5. **De-jitter.** New `src/dejitter/mod.rs` (`pub mod dejitter`, behind
   `dejitter`). `Plan` re-times a picture's audio from the end of its header
   so every line's sync pulse sits on the mode's grid: it cuts stuffed runs
   of zeros, follows audio lost or added on the way, and finds the lines
   again after a dropout. It marks the lines whose audio was lost
   (`damaged`, `patched`) and `render`s the audio on the grid; `fill_rows`
   rebuilds marked rows from their neighbours. It reads only `ScanTiming`
   (patch 1) and needs no new dependency. New `src/decode/dejittered.rs`:
   `Decoder::decode_dejittered`, the crate's own decoder with the de-jitter
   in front. To reach the samples and the image's start it adds
   crate-private hooks: `Demodulator::into_samples`
   (`src/decode/demodulator.rs`), `Decoder::into_parts` and `find_image`
   made `pub(super)` (`src/decode/decoder.rs`), and
   `ImageTones::sequence_start` (`src/decode/assembler/mod.rs`);
   `src/decode/mod.rs` and `src/lib.rs` declare the modules. New
   `tests/dejitter.rs` (a `[[test]]` in `Cargo.toml` that needs `encode` and
   `dejitter`) holds `dejittered_decode_survives_a_dropped_frame`, the
   reproducer for upstream: Scottie 2 at 8 kHz with one 20 ms frame lost
   decodes with a mean error of 78 plain and under 6 de-jittered. Beside it,
   the same comparison for stuffed zeros, PD120 with lost frames and a
   sender clock off by 300 and ±2000 ppm, and a check that a noisy but
   undamaged picture has no line marked and decodes no worse. The module
   was written for astar (as `astar-sstv`'s re-timer) by Rob Ludwick, who
   offers it under this crate's MIT licence.

With `--no-default-features --features std` the library builds with no encoder
(`cargo build`). Its tests do not: upstream's own unit and doc tests, in the
decoder and every mode file, build their signals with the encoder, so run the
tests with the default features, and add `dejitter` for the de-jitter's
(`cargo test --manifest-path vendor/sstv/Cargo.toml --features dejitter`).
The fork's own tests in `src/timing.rs` that need the encoder are gated on
`encode`; the others do not need it.

## For upstream: where this crate's timing and slowrx's differ

astar runs the same de-jitter in front of two decoders, slowrx (in
`astar-sstv`) and this crate's own, and each gets its own decoder's timing
table: `astar-sstv` turns slowrx's `ModeSpec` into a `ScanTiming`
(`scan_timing` in `crates/astar-sstv/src/mode.rs`), and
`decode_dejittered` uses `Mode::scan_timing()`. A test there,
`scan_timing_agrees_with_slowrx`, compares the two tables for the eleven
modes both know. They agree to 1 µs in every field but these, each pinned
to its exact values:

| Mode, field | slowrx | this crate | Read by the de-jitter? |
|---|---|---|---|
| Scottie 1, line | 428.38 ms (slowrx's `modespec.c`) | 428.22 ms (the sum of the published sync, separator and scan times) | yes: it is the grid spacing |
| Robot 36 / 72, separator (`porch_seconds`) | 6 ms / 4.7 ms (separator and the porch after it) | 4.5 ms (the separator tone) | no: a Robot line starts with its sync pulse |
| Robot 72, pixel | 0.2875 ms (three channels of one length) | 0.43125 ms (luminance scanned 1.5 times slower) | no, for the same reason |

Why neither difference can tear a picture through either decoder:

- The de-jitter finds each line's sync pulse in the audio itself, searching
  45 ms either side of where the last pulse predicts it, so a table off by
  0.16 ms a line never loses a pulse. It then resamples the audio so the
  pulses sit exactly one table line apart, and the decoder reads it with
  the same table. The spacing the de-jitter writes is the spacing the
  decoder expects, so the lines cannot drift against the decoder.
- Mixing them up would cost a slant, not a tear: slowrx given this crate's
  Scottie 1 grid would read every line 0.16 ms out, 41 ms over the
  picture.
- The separator and Robot 72 pixel times only place the sync pulse inside a
  line, and only for the Scottie modes, whose pulse sits mid-line. The
  Robot modes start each line with the pulse, so the de-jitter never reads
  them.

Which Scottie 1 line is right was not settled here. 428.22 ms is the sum of
the published parts; slowrx's 428.38 ms may make up for something its
decoder does.

## Updating

Re-fetch the new upstream tag, diff it against the first commit of this
directory, and re-apply the patches above. Keep this file's revision row
current.
