# Vendored: `mp3lame-encoder`

The safe Rust wrapper over `mp3lame-sys` (LAME). astar-server's MP3 stream
encoder calls it, behind the `stream-mp3` feature, which is never on by
default.

| | |
|---|---|
| Upstream crate | [`mp3lame-encoder`](https://crates.io/crates/mp3lame-encoder) 0.2.5 |
| Repository | <https://github.com/DoumanAsh/mp3lame-encoder> |
| Crate checksum | sha256 `60cb9bdd89806317373e36ff745f264b7ed7ffc5bc5aab02dc7d1b837c16a8d4` (the `.crate` from crates.io) |
| Licence | LGPL-3.0 (`LICENSE`) |
| Vendored | 2026-10-12 |

Its `Cargo.toml` hard-codes `license`, `version`, `edition` and
`repository`; never convert them to `.workspace = true`. See
`../mp3lame-sys/VENDORED.md` for why LAME is vendored at all and
`LICENSE-EXCEPTIONS.md` for the obligations.

## Patches

1. **`Cargo.toml`:** `mp3lame-sys` points at `../mp3lame-sys` (version
   0.1.11); the `symphonia` dev-dependency (upstream's decode tests) is
   removed so the workspace lock does not carry it; the crate's own
   `Cargo.lock` is removed; a `[[test]]` entry names the test below.
2. **`tests/astar_smoke.rs`** (new): one second of a 1 kHz tone at 8 and
   16 kHz encodes to valid MPEG-2 / 2.5 Layer III frames.

3. **`rustfmt.toml`** (new) sets `disable_all_formatting`, so the
   workspace's `cargo fmt --all` leaves upstream code as published.

`src/` is untouched. The API astar uses: `Builder`, `MonoPcm`, `FlushNoGap`,
`Bitrate`, `Quality`, `max_required_buffer_size`.

## Updating

Copy the new crate over this directory in a commit of its own, re-apply the
patches, update the table, and run the smoke test.
