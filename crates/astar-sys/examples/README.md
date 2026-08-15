# C example: `parrot.c`

A minimal C consumer of the `astar-sys` poll+snapshot C-ABI. It exercises
the whole lifecycle with **no callbacks**: `iax_station_new` → `iax_station_connect`
(the generic, vendor-neutral path) → poll `iax_station_snapshot` /
`iax_station_next_event` in a loop → `iax_station_disconnect` → `iax_station_free`.

The guest secret is passed as a `connect()` **in-param** (`"allstar"`) and is
never read back out of any struct — the ABI is secret-free.

## Build

First build the library (produces `target/release/libastar_sys.a` and
`.dylib`):

```sh
cargo build --release -p astar-sys
```

Then compile + link the example with the helper script (resolves the staticlib
path and the per-platform link flags):

```sh
./examples/build.sh            # builds ./examples/parrot (dials parrot 55553)
IAX_PARROT_DRYRUN=1 ./examples/build.sh   # builds ./examples/parrot-dry (offline smoke)
```

### Link flags (manual)

The static library pulls in cpal's CoreAudio backend. On **arm64 macOS** the
confirmed-working link line is:

```sh
cc examples/parrot.c -I include \
   ../../target/release/libastar_sys.a \
   -framework CoreFoundation -framework CoreAudio -framework AudioUnit \
   -framework AudioToolbox \
   -o parrot
```

- `CoreFoundation`, `CoreAudio`, `AudioUnit`, `AudioToolbox` — cpal CoreAudio backend.

Note: `IOKit` is **no longer needed** — the earlier link line included it for
`serialport` (UCI150 hardware-PTT), but `astar-sys` depends only on
`astar-station` and carries no serial code. `cpal` does not link `IOKit`
on its own.

Linking against the **dylib** instead needs no `-framework` flags (the dylib
carries its own framework dependencies):

```sh
cc examples/parrot.c -I include -L ../../target/release -lastar_sys -o parrot
# run with: DYLD_LIBRARY_PATH=../../target/release ./parrot
```

On **Linux**, `build.sh` links the staticlib with `-lasound -lpthread -ldl -lm`.

## Run

```sh
./examples/parrot          # dials the AllStar parrot 55553 (audio loopback)
```

`IAX_PARROT_DRYRUN=1` builds an offline smoke (`parrot-dry`) that does
new → idle snapshot → `set_ptt` returns `NOT_CONNECTED` → free, with no network.
CI compiles + links the example (real mode) but does not run it.
