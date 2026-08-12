# IAX2 replay fixtures

Drop `.pcap` or `.pcapng` captures of real IAX2 traffic into this directory.
The integration test at `crates/astar-conformance/tests/replay.rs` walks every
`*.pcap` and `*.pcapng` file here, extracts UDP/4569 datagrams, and asserts
that `astar_iax_core::parse` + `encode` round-trips each frame byte-for-byte.

## File format

- Container: classic libpcap (`.pcap`) or pcapng (`.pcapng`). Both work.
- Link-layer: Ethernet (`DLT_EN10MB`), raw IP (`DLT_RAW`, `DLT_IPV4`,
  `DLT_IPV6`), or BSD/OpenBSD loopback (`DLT_NULL`, `DLT_LOOP`). Other
  link types are skipped with a warning.
- Network layer: IPv4 or IPv6. IPv6 extension headers are not followed —
  if you need to test traffic that uses them, capture from somewhere that
  doesn't insert them.
- Transport: UDP. Non-UDP packets are silently dropped.
- Port filter: any datagram with `src.port == 4569` or `dst.port == 4569`
  is treated as an IAX2 frame.
- IPv4 fragmentation: skipped (with a warning). IAX2 frames are small
  enough that this should not occur on a normal MTU.

A single capture may contain multiple flows; the replay test does not
care, it just round-trips every UDP/4569 payload it finds.

## Naming

Use descriptive stems so failures point at the scenario:

```
fixtures/
  register.pcap                 ← REGREQ / REGAUTH / REGACK
  new_calltoken.pcap            ← NEW with CALLTOKEN, then CHALLENGE/RESPONSE
  call_ulaw.pcap                ← in-call mini-frame stream
  hangup.pcap                   ← teardown
```

A companion `<stem>.md` file (e.g. `register.md`) is optional. If
present, the replay harness loads its path so future versions can pin
typed assertion patterns against it. For now it is purely documentation:
write prose describing what the capture should exercise, the expected IE
set on key frames, anything unusual about the dialect.

## How to capture

`tshark` on the active interface:

```sh
tshark -i en0 -f "udp port 4569" -w fixtures/<name>.pcap
```

`tcpdump` works just as well:

```sh
sudo tcpdump -i any -w fixtures/<name>.pcap "udp port 4569"
```

For ASL3 hubs you usually want to capture on the box that's running
`asterisk` (or its bridge interface) so you see both sides of the flow.

## Sanitization

Captures may contain authentication material — the `CHALLENGE` /
`MD5_RESULT` pair leaks the SHA-1 of your IAX secret given a known
challenge string. Two ways to keep the fixture safe to commit:

1. Use a throwaway hub + account whose secret you don't care about.
2. Hand-scrub `AUTHREP` / `MD5_RESULT` payloads with a hex editor or
   `editcap`/`tshark` before committing. (The replay test still passes
   because the byte equality is between the file and the parser; the
   capture file itself becomes the source of truth.)

Do not commit captures from production accounts.

## Auto-discovery

The replay test runs on every `.pcap` and `.pcapng` in this directory.
No code change is needed when you add a fixture; just drop the file in
and run:

```sh
cargo test -p astar-conformance --test replay
```

## Synthetic seed fixture

`synthetic.pcap` is a small hand-crafted capture used to prove the
replay path works before real captures land. It contains seven packets:
NEW, AUTHREQ, AUTHREP, ACCEPT, RINGING, and two ULAW mini-frames. Regen
with:

```sh
cargo run -p astar-conformance --example gen_synthetic_pcap
```

The generator lives at `crates/astar-conformance/examples/gen_synthetic_pcap.rs`.
It uses `astar_iax_core::encode` for the IAX2 payload and hand-rolls the
Ethernet/IPv4/UDP encapsulation + libpcap file format (no external pcap
writer dependency).
