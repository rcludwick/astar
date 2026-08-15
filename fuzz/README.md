# astar fuzz targets

cargo-fuzz harnesses for `astar_iax_core::parse` (the IAX2 wire-format frame
parser). Lives outside the cargo workspace (see the root `Cargo.toml`
`exclude` list) because cargo-fuzz needs nightly + libFuzzer and would
otherwise break the stable `cargo build`.

## Targets

- **`parse_no_panic`** — feeds arbitrary bytes to `parse()` and asserts the
  parser never panics. Any `Err` is acceptable; an unwinding panic or
  process abort is a real bug.
- **`parse_encode_parse`** — when `parse()` succeeds, re-encodes the parsed
  frame and re-parses it. Asserts the second parse also succeeds and the
  two parsed frames compare equal. Catches encode/parse asymmetry.

## Running

cargo-fuzz needs nightly. On this machine the rustup shims under
`/opt/homebrew/bin` lose `argv[0]`, so prepend the nightly toolchain bindir
to `PATH` before invoking cargo:

```sh
export PATH="/Users/rob/.rustup/toolchains/nightly-aarch64-apple-darwin/bin:$PATH"
cargo fuzz run parse_no_panic -- -max_total_time=300
cargo fuzz run parse_encode_parse -- -max_total_time=300
```

(`cargo +nightly fuzz run …` also works when the toolchain shims are
healthy.) Drop `-max_total_time` for an open-ended session.

## Storage layout

- `fuzz/corpus/<target>/` — accumulated interesting inputs (gitignored).
- `fuzz/artifacts/<target>/crash-*` — crashing inputs (gitignored). Copy
  the bytes into `crates/astar-iax-core/tests/fuzz_regression/<hash>.bin`
  before deleting.
- `fuzz/target/` — build artefacts (gitignored).

## Triaging a crash

A panic caught by `parse_no_panic` is by definition a bug in
`astar-iax-core`: the parser is supposed to return `Err` on every malformed
input, never panic. When libFuzzer reports a crash:

1. The offending bytes are written to `fuzz/artifacts/<target>/crash-<hash>`.
2. Copy that file to
   `crates/astar-iax-core/tests/fuzz_regression/<hash>.bin`.
3. Add a regression test that reads the bytes and calls
   `astar_iax_core::parse` (the test only needs to *not* panic; a
   `let _ = parse(&bytes);` body suffices).
4. File an `au` ticket against `astar-iax-core` describing the failure
   mode. Don't try to patch the parser inside this crate.

A crash from `parse_encode_parse` (re-parse failure or unequal frames)
indicates encode/parse asymmetry — same procedure, but the bug may be on
either the encode or parse side.

## macOS notes

cargo-fuzz on Apple Silicon links against libFuzzer shipped with the
nightly compiler. If linker errors appear (`undefined symbols
_LLVMFuzzer*`), make sure the nightly toolchain has the `rust-src`
component and that `xcrun --show-sdk-path` resolves; reinstalling nightly
usually fixes it.
