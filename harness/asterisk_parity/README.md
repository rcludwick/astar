# Asterisk-in-Podman parity harness (iax-6813)

Local fixture generator. Brings up a single-container Asterisk node, runs
five IAX2 scenarios from the host via `astar-conformance::driver`, captures
the wire traffic with a `tshark` sidecar, and commits sanitized pcaps
under `crates/astar-conformance/fixtures/asterisk/`. The committed fixtures
are consumed by `replay.rs` so day-to-day `cargo test` does not need
podman.

## Prerequisites

- `podman` and `podman-compose` (5.x or newer)
- Rust toolchain at `/Users/rob/.rustup/toolchains/stable-aarch64-apple-darwin/bin`

## Usage

```
./run.sh up                       # start Asterisk and wait until ready
./run.sh down                     # tear down
./run.sh refresh                  # rebuild all 5 fixtures
./run.sh refresh call_ulaw        # rebuild a single fixture
```

## Scenarios

| Fixture | Peer | Flow |
|---|---|---|
| `register.pcap`     | astartest_notok | REGREQ → REGAUTH → REGREQ+MD5 → REGACK → REGREL → REGACK |
| `call_notoken.pcap` | astartest_notok | NEW → AUTHREQ → AUTHREP → ACCEPT → HANGUP (local) |
| `call_token.pcap`   | astartest       | NEW(empty CALLTOKEN IE) → CALLTOKEN → NEW(token) → AUTHREQ → AUTHREP → ACCEPT → HANGUP (local) |
| `call_ulaw.pcap`    | astartest       | call_token flow + ~2s ulaw mini-frames + HANGUP (local) |
| `peer_hangup.pcap`  | astartest       | call_token flow with Asterisk-initiated HANGUP via `astar-bye` |

## Architecture

- **Asterisk container** publishes 4569/udp; configuration in `asterisk/`.
- **tshark sidecar** joins Asterisk's network namespace
  (`network_mode: service:asterisk`) and captures all UDP/4569 traffic
  to a mounted `captures/` volume.
- **Host process** (`cargo run --example harness`) connects to
  `127.0.0.1:4569` and drives the scenario through the throwaway
  `Session` driver in `astar-conformance::driver`.
- **`run.sh`** orchestrates per-scenario start/stop and moves the
  captured pcap into the committed fixtures directory.

## Limitations

- Happy-path only. Unhappy paths tracked as **iax-8b0a**.
- Does not validate ASL3-specific quirks (see **iax-7022**).
- No byte-level parity against a patched C iaxclient (deferred).
- The driver in `astar-conformance::driver` is throwaway and will be
  replaced when **iax-612e** lands a real high-level API.
- `peer_hangup` scenario reaches Hangup{Peer} but not Closed — the
  driver doesn't yet pump FSM timers. Pcap still shows the full
  setup + peer-initiated tear-down.
- `register.pcap` currently captures 0 IAX2 datagrams because
  tshark's startup race with the ~50ms scenario; investigate as a
  iax-6813 follow-up.

## FSM/parser changes landed via this harness

Live integration against Asterisk 20 surfaced bugs that no unit test
caught:

- FSM didn't preserve dialled extension across CALLTOKEN handshake.
- Reliability oseqno wasn't reset on resent NEW (RFC 5456 §8.6).
- Parser rejected unknown subclasses + IE 56 (FORMAT2 9 bytes).
- FSM only handled `Control(Hangup)` in Active; Asterisk emits
  `Iax(Hangup)`.
- Driver wasn't routing inbound ACK to FSM in Hangup state.

All fixed; regression tests added in `session_loopback.rs`. Remaining
gaps documented above and tracked as iax-c333 follow-ups.
