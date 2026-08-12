# jitter_parity

Cross-check the Rust `JitterBuf` in `crates/astar-codec/src/jitter.rs`
against the reference C `jitterbuf` from
`/Users/rob/dev/astar/vendor/iaxclient/lib/libiax2/src/jitterbuf.c`.

## What it validates

A small text-based trace format describes inputs (`put`, `get`, `next`,
`reset`, `config`). Two harnesses consume the same trace:

- `c_harness/harness_c` drives the C `jitterbuf` and writes a result line
  per op.
- `crates/astar-codec/tests/jitter_parity.rs` drives the Rust port and
  formats results identically.

The C harness's output is committed as `*.out` golden files in `traces/`.
The Rust test diffs its output against those goldens and fails on any
divergence.

## Trace format

ASCII, one op per line. Comments start with `#`. Whitespace-separated:

```
config <max_jitterbuf> <resync_threshold> <max_contig_interp> <target_extra>
put    <now> <ts> <ms> <voice|control|silence|video> <hex-payload>
get    <now> <interpl>
next   <now>            # arg ignored; kept for symmetry
reset
```

Result lines:

```
config ok
put    ok | put sched | put drop
get    ok ts=<ts> ms=<ms> ftype=<...> payload=<hex>
get    drop ts=<ts> ms=<ms> ftype=<...> payload=<hex>
get    interpolate | get empty | get noframe | get scheduled
next   none | next at=<ts>
reset  ok
```

## How to (re)generate goldens

After a deliberate change to either the Rust port or the C compile flags
that changes formatted output, rebuild the C harness and regenerate every
golden:

```
cd harness/jitter_parity/c_harness
make
cd ..
for t in traces/*.in; do
  c_harness/harness_c < "$t" > "${t%.in}.out"
done
```

Then re-run `cargo test -p astar-codec --test jitter_parity` and
inspect any new diffs before committing the regenerated `.out` files.

## Known deltas vs C

None at present. The port had one documented simplification (the
shrink-with-frame-present branch in voice `get`, folded into the no-frame
shrink path; see ticket `iax-9c62`). None of the bundled traces exercises
exactly that branch, so the harness currently passes against every golden.
If a future trace hits that path and produces a divergence, add a per-trace
allowlist entry in `allowed_diff_lines` inside `jitter_parity.rs`, with a
comment pointing to `iax-9c62`.

The harness did surface one separate divergence on initial implementation:
the Rust port was treating `silence_begin_ts == -1` (the post-reset
sentinel) as voice mode, whereas the C condition `if (!silence_begin_ts)`
treats any non-zero value as silent. The Rust `JitterBuf::get` and `next`
were corrected to match the C semantics; see the corresponding edits in
`crates/astar-codec/src/jitter.rs`.

## Adding a new trace

1. Drop a `traces/foo.in` file. Keep it minimal: one thing per trace.
2. From `harness/jitter_parity/`:
   `c_harness/harness_c < traces/foo.in > traces/foo.out`
3. Open `traces/foo.out` and confirm the C behaviour is what you expect
   before committing.
4. Run `cargo test -p astar-codec --test jitter_parity` to confirm
   the Rust port agrees.
