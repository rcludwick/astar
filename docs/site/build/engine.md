---
icon: lucide/cpu
---

# The engine

**astar-lib** is the collection of Rust crates under `crates/`: IAX2 framing
and session state, codecs, audio I/O, PTT backends, the multi-network station
facade, and the C ABI every front-end reaches it through. No UI, no platform
assumptions beyond what `cpal` and `serialport` need.

It is the easiest thing in the repository to build. A Rust toolchain, and on
Linux two dev packages, is the whole list.

```bash
cargo build --workspace
cargo test  --workspace
```

## The workspace

One cargo workspace, `resolver = "2"`, everything sharing a version
(`0.1.3-beta`) and a licence (`AGPL-3.0-only`) through `[workspace.package]`.

| Crate | What it is |
|---|---|
| `astar-iax-core` | IAX2 frames, the session state machine, RFC 5456 conformance |
| `astar-iax` | The link layer and call manager on top of it |
| `astar-codec` | Audio codecs and the jitter buffer. G.711 always on; GSM, Speex and iLBC behind feature flags; Codec 2 and AMBE loaders |
| `astar-audio` | Device enumeration, capture/playback, resampling, metering |
| `astar-ptt` | PTT backends — raw USB, tty, CM108 HID |
| `astar-serial-sys` | The serial C ABI |
| `astar-station` | The multi-network station facade the clients drive |
| `astar-console` | The operating runtime beneath the station |
| `astar-sys` | The main C ABI (`astar.h`) |
| `astar-server` | The node daemon — see [astar-server](server.md) |
| `astar-cli` | A terminal IAX2 client — register, dial, parrot |
| `astar-m17` / `astar-dstar` | The M17 and D-Star protocol implementations |
| `astar-asl3` | AllStarLink portal and registrar helpers |
| `astar-wireguard` | The WireGuard link transport |
| `astar-inspect` | A diagnostic tool with a small web view |
| `astar-conformance` | Replay tests against captured wire traffic |

`vendor/ambe-thumbdv` is in the workspace too, but it is **not** astar code: it
is the ThumbDV vocoder driver, vendored under its own MIT/Apache-2.0 terms. Its
`Cargo.toml` hard-codes its licence and version on purpose — converting those
to `.workspace = true` would relicense someone else's code as AGPL.

## Scoping a build

The `build`, `test` and `clippy` recipes pass their arguments straight through,
so anything cargo understands works:

```bash
just build -p astar-audio          # one crate
just test  -p astar-server         # one crate's tests
just clippy                        # whole workspace, warnings as errors
cargo build --workspace --release  # or: just release
```

The release profile uses thin LTO and a single codegen unit, so it is
noticeably slower to build and meaningfully faster to run.

## Feature flags

Most features are on by default and you can ignore this section. The three that
matter are off-by-default or licence-driven.

| Feature | Crate | Default | What it does |
|---|---|:--:|---|
| `m17` | `astar-station`, `astar-console` | **on** | M17 client. Pulls `astar-m17` (pure protocol, no extra deps) and enables `codec2-runtime`. |
| `codec2-runtime` | `astar-codec` | **off**¹ | `dlopen`s a *system* `libcodec2` at run time via `libloading`. No LGPL code enters the build. |
| `codec2-static` | `astar-codec` | **off** | Links the `codec2` crate directly. Kept off, and out of every default set, because Codec 2 is LGPL-2.1 and MIT. |
| `ambe-hw` | `astar-codec` | **off** | The ThumbDV USB vocoder backend. |
| `dstar` | `astar-cli`, `astar-station`, `astar-console` | **off** | D-Star. Pulls in `ambe-hw`; there is no software fallback. |
| `dstar` | `astar-sys` | **on** | D-Star over the C ABI, because the Swift binding is generated from this crate. |

¹ Off in `astar-codec` itself, but `astar-station`'s and `astar-console`'s
default `m17` feature turns it on — so a normal build has it.

!!! warning "`astar-sys` turns D-Star on for the whole workspace build"

    Cargo unifies features across a workspace build. Because `astar-sys`
    enables `dstar` by default, a plain `cargo build --workspace` compiles
    D-Star into every crate that can carry it — `astar-server` included. It
    costs a user with no dongle nothing but a serial-port enumeration every
    500 ms: `dstar_available` reports `false` and every D-Star call fails
    cleanly. It is worth knowing when you are reading a dependency tree and
    wondering why `ambe-thumbdv` is in it.

Verify what you actually got:

```bash
cargo tree -p astar-cli                    # no ambe-* crates
cargo tree -p astar-cli --features dstar   # ambe-thumbdv present
```

## Testing

```bash
just test                      # the whole workspace
just dstar-test                # the hardware-free half of the D-Star suites
```

Hardware-touching tests **skip by default**, printing why on stderr, so the
suites run green on a machine with nothing plugged in. They run only when
`IAX_THUMBDV_TESTS=1` is set explicitly.

!!! danger "Do not arm the hardware suites casually"

    `IAX_THUMBDV_TESTS=1` and `just dstar-test-hw` open serial ports. On a
    machine with a radio interface attached that is not a hypothetical:
    opening a USB radio interface's tty asserts RTS, and RTS keys a
    transmitter. Only one process may hold a ThumbDV at a time, so nothing else
    — another test run, `dstar-listen` — may be running alongside it.

    No test may reach anything outside `127.0.0.1`. The `just guards` recipe
    fails the build if any hardware or live-network opt-in is armed.

## The C ABI and the bindings

`astar-sys` and `astar-serial-sys` expose the engine as a C library, and the
Swift and Python bindings are generated from it. The headers
(`crates/astar-sys/include/astar.h`,
`crates/astar-serial-sys/include/astarserial.h`) are **committed**, and must
match what the current source generates:

```bash
just cbindgen        # regenerate + fail on drift; part of just ci
```

After changing any `extern "C"` function or `#[repr(C)]` type — including their
doc comments, which cbindgen copies into the header — run it. If it reports
drift it prints the exact command to fix it.

Exercise the ABI end to end:

```bash
just ffi-example     # build the cdylib/staticlib, compile and link the C example
just python          # offline Python ctypes smoke test
```

The Swift bindings are their own build step and are covered in
[The macOS app](macos-app.md).

## Next steps

* [The macOS app](macos-app.md) · [The Windows / Linux client](clients.md) ·
  [astar-server](server.md)
* [Verifying a build](verifying.md) — the gates.
