# Python binding: `astarstation`

A pure-[`ctypes`](https://docs.python.org/3/library/ctypes.html) consumer of the
`astar-sys` poll+snapshot C-ABI. No build step, no native extension, no
third-party packages — just a stdlib module that loads the cdylib and mirrors
`crates/astar-sys/include/astar.h` exactly.

* **Poll + snapshot.** Call state is driven by polling `snapshot()` and
  `next_event()`. The one callback into Python is the optional credential
  resolver (`set_credential_resolver`), used solely to fetch a secret on demand.
* **Secret-free.** A `secret` is only ever a `connect()` argument or the return
  value of the credential resolver; it is never stored on the `Station`, never
  returned from a snapshot/event, and never in any `repr` / exception / log.
* **Vendor-neutral.** `connect()` is the generic IAX2 path; `connect_wt()` is
  the AllStar Web-Transceiver convenience.

## Prerequisites

Build the cdylib (the module finds `target/debug` or `target/release`):

```sh
cargo build -p astar-sys            # debug
cargo build --release -p astar-sys  # release
```

The module loads `libastar_sys.dylib` (macOS) / `.so` (Linux) /
`astar_sys.dll` (Windows). Library search order:

1. `$ASTAR_LIB` — a full path to the dylib, or a directory containing it.
2. `target/debug/` then `target/release/` relative to the repo root.

If none is found, `load_library()` raises `FileNotFoundError` listing the paths
it tried.

## Quick start

```python
from astarstation import Station, Status

with Station() as st:                       # context-manager: frees on exit
    print(st.snapshot().status)             # Status.IDLE
    print(st.list_inputs(), st.list_outputs())

    # Generic IAX2 connect (vendor-neutral). secret is a call-time arg.
    st.connect("55553", "55553", secret="allstar", name="py")

    for _ in range(80):
        snap = st.snapshot()
        print(snap.status, snap.tx_db, snap.rx_db)
        while (ev := st.next_event()) is not None:
            print("event:", ev.kind)
        time.sleep(0.1)

    st.disconnect()
```

AllStar WT path (pass portal creds at construction):

```python
st = Station(portal_user="me", portal_pass="...", portal_node="1234")
st.connect_wt("55553")
```

## API

`Station(*, input=None, output=None, portal_user=None, portal_pass=None,
portal_node=None, secret=None, lib=None)`

| Method | C-ABI |
| --- | --- |
| `connect(dest, calling, *, secret=None, name=...)` | `iax_station_connect` |
| `connect_wt(dest_node)` | `iax_station_connect_wt` |
| `disconnect()` | `iax_station_disconnect` |
| `set_ptt(on)` | `iax_station_set_ptt` |
| `set_input_gain(g)` / `set_output_gain(g)` | `iax_station_set_*_gain` |
| `set_compression(on)` / `set_noise_reduction(on)` | `iax_station_set_compression` / `iax_station_set_noise_reduction` |
| `set_compression_level(level)` (0.0–1.0, default 0.90) | `iax_station_set_compression_level` |
| `snapshot() -> Snapshot` | `iax_station_snapshot` |
| `next_event() -> Event \| None` | `iax_station_next_event` |
| `list_inputs()` / `list_outputs()` | `iax_station_list_*` |
| `set_devices(input, output)` | `iax_station_set_devices` |
| `close()` / `__enter__`/`__exit__` | `iax_station_free` |

Negative C return codes raise `StationError(code, text)` where `text` comes from
`iax_error_text` (generic, secret-free).

### Dataclasses

```python
@dataclass(frozen=True)
class Snapshot:
    status: Status            # IDLE | DIALING | ANSWERED | HANGUP
    ptt: bool
    remote_ptt: bool
    tx_db: float              # dBFS
    rx_db: float              # dBFS
    rtt_ms: int | None        # None when unknown (C -1)
    mode: Mode                # current operating mode (WT | NODE)

@dataclass(frozen=True)
class Event:
    kind: EventKind           # NONE | ANSWERED | REMOTE_PTT | HANGUP
    remote_ptt: bool
```

## Operating modes (WT + Node)

Two top-level modes (mirrors the Rust `Station`; see
`crates/astar-station/README.md`):

- **WebTransceiver (WT) client** — dial out with `connect()` / `connect_wt()`.
- **Node** — accept inbound calls and bridge them to a local handset. Configure
  with `set_node_config(...)`, switch with `set_mode(Mode.NODE)`, then poll for
  `EventKind.INCOMING` and call `answer()` / `reject()` (Manual answer) or let it
  auto-answer; `incoming_from()` gives the caller id. Optionally register **as**
  a node (so callers reach you by number) by passing `registrar=` /
  `register_user=` to `set_node_config`.

```python
from astarstation import Station, Mode, AnswerPolicy, AuthPolicy

st = Station(input="USB Audio", output="Speakers")
# The registrar password is supplied ONLY through the resolver — never config.
st.set_credential_resolver(lambda user: lookup_secret(user))
st.set_node_config(
    bind="0.0.0.0:4569",
    answer=AnswerPolicy.AUTO,
    auth=AuthPolicy.OFF,
    registrar="register.allstarlink.org:4569",  # omit to only listen
    register_user="77777",
)
st.set_mode(Mode.NODE)   # binds the listener + registers; blocking
```

See `examples/node.py` for a runnable version (offline `--dry-run` or live).

## Examples & tests

```sh
python3 bindings/python/astarstation.py    # offline self-check (no network)
python3 bindings/python/test_smoke.py    # offline smoke asserts (no network)

# The parrot example. Offline dry-run (no network):
python3 bindings/python/examples/parrot.py --dry-run
# Live: dials the AllStar parrot 55553 (audio loopback):
python3 bindings/python/examples/parrot.py
```

## astarserial (serial PTT)

`astarserial.py` is a pure-`ctypes` wrapper over `astar-serial-sys` — the
cross-platform serial radio-interface PTT library. It mirrors the C-ABI
(`iax_serial_open`, `iax_serial_ptt_tick`, `iax_serial_close`) and exposes a
Pythonic `SerialClient` / `SerialConfig` surface. No native extension, no
third-party packages.

### Prerequisites

Build the cdylib the module loads:

```sh
cargo build -p astar-serial-sys            # debug (default search path)
cargo build --release -p astar-serial-sys  # release (override path via env)
```

Library search order (unlike `astarstation`, there is **no** automatic
`target/release/` fallback):

1. `$ASTAR_SERIAL_LIB` — a full path to the cdylib, or a directory containing it.
2. `target/debug/libastar_serial_sys.dylib` (macOS) / `.so` (Linux) /
   `astar_serial_sys.dll` (Windows), relative to the repo root.

### API

`SerialConfig` — mirrors `IaxSerialConfig`:

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `port_path` | `str \| None` | `None` | Serial device path; `None` = autodetect |
| `key_line` | `KeyLine` | `KeyLine.CTS` | Operator-key input line |
| `key_active_high` | `bool` | `True` | Input asserted == keyed |
| `radio_line` | `RadioLine` | `RadioLine.RTS` | Radio-key output line |
| `radio_active_high` | `bool` | `True` | Assert output == key the radio |
| `debounce_ms` | `int` | `30` | Key de-glitch window in ms |
| `rx_mode` | `RxKeyMode` | `RxKeyMode.REMOTE_PTT` | What drives the radio key while receiving |
| `rx_floor_db` | `float` | `-45.0` | RxActivity: level threshold (dBFS) |
| `rx_hang_ms` | `int` | `250` | RxActivity: post-audio key hang (ms) |

`SerialClient(cfg: SerialConfig)` — wraps `IaxSerial*`:

| Method | Description |
| --- | --- |
| `SerialClient.autodetect() -> str \| None` | Detect the first UCI150 device path |
| `ptt_tick(remote_keyed, rx_db) -> tuple[bool, bool]` | One tick → `(changed, set_ptt)` |
| `close()` / `__enter__`/`__exit__` | Drop the radio line (fail-safe), then free |

### 20 ms poll loop

```python
import time
from astarserial import SerialClient, SerialConfig
from astarstation import Station

cfg = SerialConfig(port_path=SerialClient.autodetect())
with SerialClient(cfg) as serial, Station() as st:
    st.connect("55553", "55553", secret="allstar")
    while True:
        snap = st.snapshot()
        changed, set_ptt = serial.ptt_tick(snap.remote_ptt, snap.rx_db)
        if changed:
            st.set_ptt(set_ptt)
        time.sleep(0.02)
```

PTT source lives in `SerialClient`, not in `Station`: the library only sets the
call's keyed/unkeyed state via `set_ptt`. Secret-free, poll model, no callbacks.
