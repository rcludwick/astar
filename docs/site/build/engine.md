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
(`0.1.13-beta`) and a licence (`AGPL-3.0-only`) through `[workspace.package]`.

| Crate | What it is |
|---|---|
| `astar-iax-core` | IAX2 frames, the session state machine, RFC 5456 conformance |
| `astar-iax` | The link layer and call manager on top of it |
| `astar-codec` | Audio codecs and the jitter buffer. G.711 µ-law/A-law and signed-linear (8/16 kHz) on the IAX2 path; Codec 2 for M17; AMBE via the ThumbDV dongle |
| `astar-audio` | Device enumeration, capture/playback, resampling, metering |
| `astar-ptt` | PTT backends — raw USB, tty, CM108 HID |
| `astar-ble-ptt` | The Bluetooth LE PTT button's device table and button state machine. Pure: no dependencies, no I/O, no clock |
| `astar-serial-sys` | The serial C ABI |
| `astar-station` | The multi-network station facade the clients drive, including the PTT arbiter |
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

`vendor/codec2` is the same kind of island: the `codec2` crate (Codec 2, for
M17), vendored unmodified under its LGPL-2.1-only AND MIT terms. It is listed in
the workspace's `exclude` rather than its members, so workspace-wide `fmt` and
`clippy` leave it exactly as upstream wrote it; Cargo still builds it as a path
dependency whenever `codec2-static` is on.

## Scoping a build

The `build`, `test` and `clippy` recipes pass their arguments straight through,
so anything cargo understands works:

```bash
just build -p astar-audio          # one crate
just test  -p astar-server         # one crate's tests
just clippy                        # whole workspace, warnings as errors
cargo build --workspace --release  # or: just build-release
```

The release profile uses thin LTO and a single codegen unit, so it is
noticeably slower to build and meaningfully faster to run.

## Feature flags

Most features are on by default and you can ignore this section. The ones that
matter are off-by-default or licence-driven.

| Feature | Crate | Default | What it does |
|---|---|:--:|---|
| `m17` | `astar-station`, `astar-console` | **on** | M17 client. Pulls `astar-m17` (pure protocol, no extra deps) and enables `codec2-runtime`. |
| `codec2-runtime` | `astar-codec` | **off**¹ | `dlopen`s a *system* `libcodec2` at run time via `libloading`. No LGPL code enters the build. |
| `codec2-static` | `astar-codec` | **off** | Links the `codec2` crate directly. Kept off, and out of every default set, because Codec 2 is LGPL-2.1 and MIT. |
| `ambe-hw` | `astar-codec` | **off** | The ThumbDV USB vocoder backend. |
| `dstar` | `astar-cli`, `astar-station`, `astar-console` | **off** | D-Star. Pulls in `ambe-hw`. The dongle is its vocoder unless the software one below is both built and chosen; nothing ever falls back from one to the other. |
| `dstar` | `astar-sys` | **on** | D-Star over the C ABI, because the Swift binding is generated from this crate. |
| `dstar-soft-ambe` | `astar-codec`, `astar-console`, `astar-station`, `astar-sys`, `astar-server` | **off**² | The software D-Star vocoder: Bruce Perens' AMBE 3600×2400, vendored at `vendor/ham-digital-modes-ambe` (LGPL-3.0-or-later). D-Star only: System Fusion, NXDN and DMR use AMBE+2 and stay on the dongle. Built in, it is a choice, not a switch-over: D-Star uses the dongle unless the operator picks Software. |

¹ Off in `astar-codec` itself, but `astar-station`'s and `astar-console`'s
default `m17` feature turns it on — so a normal build has it.

² Off in a plain `cargo build`, like `codec2-static`, so nothing links LGPL
code by accident. **Every shipped build turns it on**: `just xcframework` (the
macOS app; `ASTAR_DSTAR_SOFT_AMBE=0` opts out) and astar-server's `release`
feature set. `ci/guard-dstar-soft-ambe.sh` checks both halves.

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

## Push-to-talk in the engine

Every decision about whether astar transmits is made here, not in a client.

**The arbiter.** `astar-station`'s `PttArbiter` sits between every keying
source and the console session. The sources are `Manual` (the on-screen button
and Space), `Vox`, `Serial`, `Bluetooth`, and `Legacy` — `Station::set_ptt`,
used by astar-server, the Python binding, the Windows/Linux client and the
CLI's `dstar-listen`. (`astar-cli dial` and `parrot` key the IAX2 call
manager directly and do not go through the arbiter.)

* The radio is on air while any held source is not latched, and only an edge
  of that combined state reaches the console session.
* A press the session refuses (not connected, a receive-only network) is
  rolled back, and the source is latched until it releases. `Legacy` is the
  exception: a refused `set_ptt(true)` leaves nothing held or latched.
* A release clears that source's hold and latch, and is never refused.
* `Station::ptt_stop_all` releases every source and latches every held one.
  `set_ptt(false)` means exactly that, and also re-arms `Legacy`.
* A call ending latches every held source except `Legacy`, which the
  hang-up simply releases, so astar-server's next `/key` keys on the next
  call.
* `Bluetooth` is leased: `ptt_lease_press` takes a time to live (1.5 s from
  the Mac) and `ptt_lease_renew` extends it (every 250 ms). Renew never
  presses.
* The transmit time-out is one clock of continuous on-air time on the
  combined state. `Station::set_tx_timeout` accepts 60–600 s and keeps the old
  value otherwise; a new `Station` starts at 120 s. When it fires, every held
  source is released and latched, `Legacy` included: the time-out and
  `ptt_stop_all` are the only things that latch `Legacy`, and after it
  `set_ptt(true)` returns `ptt source is latched: release it before keying
  again` until `set_ptt(false)`.

**The watchdog.** The thread `astar-ptt-watch` starts on the first press,
checks the lease and the time-out every 50 ms, holds only weak references, and
ends when the `Station` drops. Locks are always taken arbiter first, then
session.

**`astar-ble-ptt`.** The device table (one row: the Inrico B02PTT's service
and characteristic), advertised-name matching, GATT acceptance, decoding, and
`ButtonMachine`, a state machine the caller passes time into. It latches the
button on a link loss only when the button was down at the loss, so an idle
drop reconnects to ready and the next press keys. The JSON traces in
`crates/astar-ble-ptt/scenarios/` are replayed by both the Rust and the Swift
tests. `ci/guard-ble-ptt.sh` (part of `just guards`) keeps the crate free of
normal dependencies and out of astar-server's dependency tree. The Bluetooth
transport itself is the client's: on macOS, CoreBluetooth in the app.

**Over the C ABI.** `iax_station_ptt_source`, `iax_station_ptt_stop_all`,
`iax_station_ptt_lease_press`, `iax_station_ptt_lease_renew` and
`iax_station_set_tx_timeout_ms` — which returns `IAX_ERR_INVALID` below 60 000
ms (0 included) and above 600 000 ms, keeping the old value, so no binding can
switch the time-out off. The snapshot's `IaxState` carries the `ptt_*` fields,
and `iax_bleptt_*` exposes the machine and the device table. The Python binding
has `Station.ptt_source`, `ptt_stop_all`, `ptt_lease_press`, `ptt_lease_renew`
and `set_tx_timeout_ms`.

`just mutants-ble-ptt` runs mutation testing over `astar-ble-ptt` and the
arbiter; it expects zero survivors and is too slow for `just ci`.

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
