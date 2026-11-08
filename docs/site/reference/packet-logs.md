---
icon: lucide/bug
---

# Packet logs

Picture an EchoLink call on a busy net. Behind the speaker's voice you hear a
faint copy of it, a little later. Where did the copy come from? Either the far
station sent it, or astar made it while playing the call back. A recording
can't settle that, because a recording only holds what astar played. It also
can't say when each piece of sound reached your computer, so it can't show
delay.

A **packet log** can. When you press the capture button, astar writes down the
voice packets of the call exactly as they arrived, before astar does anything
to them, and stamps each with the moment it came in. If the copy is already in
the packets, the far side sent it. If the packets are clean and the recording
isn't, astar made it.

This page covers what a log holds, what to be careful about, and how to read
one. The button is on the [macOS app](../macos/packet-log.md) and
[astar-server](../server/packet-log.md) pages. A packet log is a debugging
tool: it captures nothing until you press the button, it works on every
network, and it runs whether or not Record is on.

## What is in a log

Each call writes two files that sit side by side and share a name:

```
2026-11-05 19-29-06 EchoLink WA7ABU-R.pcap
2026-11-05 19-29-06 EchoLink WA7ABU-R.jitter.jsonl
```

* The **`.pcap` file** holds the voice packets that arrived. A pcap is the
  standard capture format that Wireshark, tshark and tcpdump read. Each packet
  is stored as a UDP datagram with its real addresses and ports, and its
  arrival time to the nanosecond. There is no Ethernet layer, because
  there was none to record; Wireshark shows it as "Raw IP".
* The **`.jitter.jsonl` file**, the **companion**, holds the jitter buffer's
  state about once a second. The jitter buffer is the short queue that
  smooths out packets arriving unevenly before they are played. The file is
  JSON Lines: one small JSON object per line. [Reading the companion
  file](#reading-the-companion-file) lists every field.

Only packets that carry someone's voice are written. Each network has an
allowlist, a short test of the first bytes and the length, and a packet that
isn't recognised stays out:

| Network | Written | Left out |
|---|---|---|
| AllStar (IAX2) | Voice frames, full and mini | Every IAX command (NEW, AUTHREQ, AUTHREP, REGREQ, REGAUTH and the rest), control frames, DTMF, text |
| EchoLink | The 144-byte audio datagrams on the audio port | Info and chat on the audio port, and the whole control port |
| M17 | The 54-byte stream packets | Connect, acknowledge, ping, pong, disconnect |
| D-Star | The `DSVT` header and voice packets | Connect, acknowledge, keepalive, unlink |
| Fusion (YSF) | `YSFD` data packets | Poll, unlink, status and info packets |
| NXDN | `NXDND` data packets | Poll and unlink packets |
| DMR | `DMRD` data packets | The login exchange, rejections, pings and closes |

Inbound AllStar calls are logged from the first frame, before the call is
answered.

## What is never in a log

Logins and other secrets never reach a log. The IAX2 login (the node secret
and its challenge), EchoLink's password, the portal account and DMR's
password hash all travel on paths the log does not watch, and anything astar
transmits is not logged at all. Only packets *received* during a call are
written.

## Privacy

A log is not secret-free just because it has no passwords. It holds:

* **Other people's voices**, the same as a recording.
* **IP addresses**: the far station's, and this machine's.
* **Callsigns**, which sit inside D-Star, M17, Fusion and DMR packets.
* **Short text or GPS positions** that someone put on the air. D-Star slow
  data, DMR data bursts and Fusion data frames travel in the same kinds of
  packet as voice, so they are in the log too.

Treat a log like a recording, and think before you share one.

## Opening a log in Wireshark

Open the `.pcap` in [Wireshark](https://www.wireshark.org/) (or read it with
`tshark -r file.pcap`). What you see depends on the network:

* **AllStar** is dissected as IAX2, because the traffic is on port 4569.
  Wireshark shows each frame's type, timestamp and call numbers.
* **The other six networks** appear as plain UDP. Wireshark has no decoder
  for them, but arrival times, sizes and the payload bytes are all there,
  which is enough to measure delay.
* **EchoLink** is the awkward one. Its packets start with the bytes `C0 03`,
  which look like an RTP header of version 3, so *Decode As → RTP* does not
  help; Wireshark gives up with "Version: Unknown (3)". Use the recipes below
  instead.

A call over WireGuard is logged as the inner datagram, with the tunnel's
addresses.

## Encryption

Packets are logged decrypted. The tap sits above WireGuard, so a call carried
through a tunnel is logged as the inner datagram, after the tunnel has been
removed. None of these networks' media is encrypted by astar or on its way to
astar, so there is nothing else to remove.

There is one case to know about. An M17 sender can encrypt its own voice, end
to end, with a key that astar does not hold. astar can't decrypt that voice,
so the log holds it exactly as received, still encrypted.

## Measuring delay

Every EchoLink audio packet carries a **sequence number**, a count that goes up
by one per packet. The sender makes a packet every 80 ms, so
packet number *n* should arrive about *n* × 80 ms after packet 0. The
sequence number is in payload bytes 2 and 3 (counting from 0, big-endian).

So take each packet's arrival time and subtract *n* × 80 ms. If the network
were perfect, the answer would be the same for every packet:

```sh
tshark -r call.pcap -T fields -e frame.time_epoch -e udp.payload |
python3 -c '
import sys
t0 = None
for line in sys.stdin:
    t, hexbytes = line.split()
    seq = int.from_bytes(bytes.fromhex(hexbytes)[2:4], "big")
    delay = float(t) - seq * 0.08
    t0 = delay if t0 is None else t0
    print(seq, round((delay - t0) * 1000, 1))
'
```

Each output line is a sequence number and how many milliseconds later than
packet 0 that packet arrived, relative to where it should have been.

* A **flat line** near 0 means the packets are arriving on schedule.
* A **rising line** means delay is growing: the sender's clock is running
  slower than yours, or packets are being held up and released in bursts.
* **Spread around the line** is jitter. A few milliseconds is normal.

The sequence number is 16 bits, so it wraps after 65,536 packets, about 87
minutes. Start a fresh measurement after a wrap, and again wherever the count
starts over.

The other way to see delay is the companion file's `depth_ms`, described next.

## Reading the companion file

The first line identifies the file:

```json
{"astar_packet_log":1,"network":"echolink","codec":null,"label":"EchoLink WA7ABU-R","local":"192.0.2.1:5198","peer":"203.0.113.7:5198","started_at":"2026-11-06T02:29:06Z","version":"0.1.20-beta","part":1}
```

An AllStar call's first line looks the same, with `"network":"allstar","codec":"ulaw"`.

`network` and `label` say what the call was. `codec` names what an AllStar
call agreed on. It is there because a capture that starts mid-call may hold no
packet that says which codec is in use. On the other networks it is `null`,
because the network or the packets themselves say. An AllStar log with
`"codec":null` means the codec is unknown. `local` and `peer` are the two
ends, `started_at` is in UTC, `version` is the astar that wrote it, and `part`
is 1 unless the call was split (see [Limits](#limits)).

Then comes about one row a second:

```json
{"ts_ns":1793000000123456789,"packets":412,"dropped":0,"depth_ms":60,"jitter_ms":14,"lost":3,"late":0,"ooo":0,"underruns":0}
```

| Field | Meaning |
|---|---|
| `ts_ns` | When the row was written, in nanoseconds since 1970. It runs on the same clock as the pcap's packet times, so rows and packets line up. |
| `packets` | Packets written to this file so far. |
| `dropped` | Packets that never reached the file, for any reason. A dropped packet looks exactly like network loss in the pcap, so check this before blaming the network. |
| `depth_ms` | The jitter buffer's cushion: how much received audio it is holding back to ride out the network. |
| `jitter_ms` | The buffer's estimate of how unevenly packets arrive. |
| `lost` | Frames the buffer counts as lost. This includes frames it skipped on purpose to shrink an over-deep cushion. |
| `late` | Frames that arrived after their play time and were thrown away. |
| `ooo` | Frames that arrived out of order and were put back in place. |
| `underruns` | Times the speaker ran out of audio to play. |

`depth_ms` is the cushion, not the whole delay. The mixer, the output bus and
the sound device each add delay after the jitter buffer, and `depth_ms` does
not include them.

A field is `null` when it has no meaning for that call, which is different from
`0`:

* **AllStar and EchoLink calls you place** have a jitter buffer, so these
  fields have values.
* **Digital networks** (M17, D-Star, Fusion, NXDN, DMR) have no jitter buffer, so
  `depth_ms`, `jitter_ms`, `lost`, `late` and `ooo` are `null`. `underruns` is
  set when the call has a voice route.
* **AllStar conference members and EchoLink members of a server** hand the
  conference bare frames, so all of their fields are `null`. `packets` and
  `dropped` are always there.

The last line says why the file ended:

```json
{"end":"hangup","ts_ns":1793000330000000000,"packets":4120,"dropped":0}
```

| `end` | Why |
|---|---|
| `hangup` | The call ended. |
| `stopped` | The capture was stopped: the button, Debug mode going off, a settings save, or astar quitting with the call still up. |
| `continued` | The call goes on in the next part. |
| `limit` | The folder reached its size limit with only open logs in it. |
| `disk_low` | Free disk space fell below 500 MB. |
| `write_error` | The disk refused a write. |

A file with no last line means astar or the machine stopped suddenly.

## Hearing the audio

An EchoLink packet is 144 bytes: a 12-byte header, then 132 bytes of audio,
which is four standard GSM 06.10 frames of 33 bytes each. Cut off the header,
join the audio bytes of every packet in order, and you have a `.gsm` file that
[sox](https://sox.sourceforge.net/) can convert:

```sh
tshark -r call.pcap -T fields -e udp.payload |
  cut -c25- | tr -d '\n' | xxd -r -p > call.gsm
sox call.gsm call.wav
```

The `cut -c25-` drops the first 24 hex characters of each line, which are the
12 header bytes.

This joins packets as they arrived, so a lost packet is not filled with
silence, and the audio shifts. It is meant for answering "is the copy already
in there?" rather than for polished playback. Play `call.wav` and listen for
the echo. If it is there, it came over the network.

## Limits

* **A capture starts at the press.** It can begin in the middle of an over, so
  a D-Star capture can lack that over's header, and AllStar mini frames carry
  only 16 bits of timestamp until the next full frame.
* **A quiet capture leaves no file.** The file opens at the first voice
  packet, so if nobody talks after the press, nothing is written.
* **Times are taken in user space.** Each packet is stamped just after astar
  reads it, not when the network card received it. A receive thread that was
  busy for a moment, for example waiting on a slow vocoder, shows up as a
  small burst of packets with nearly the same time.
* **Drops are counted.** The log never slows a call down. If its writer falls
  far behind, if the buffer pool is empty, or if a datagram is over 2,048
  bytes, the packet is dropped and counted in `dropped`.
* **Long calls split into parts.** A file that reaches one sixteenth of the
  size limit is closed (with `end` set to `continued`) and the call goes on in
  a new pair of files named `… part 2.pcap`, `… part 3.pcap` and so on, each
  with its own companion. A short call is a single pair.
* **The size limit is enforced while a capture runs.** When the folder passes its size limit,
  closed logs are deleted oldest first. Logs that are still open are never
  deleted, and files in the folder that astar did not write are left alone.
* **An open log can be downloaded.** It downloads at the size it has when you
  open it, so it can end partway through a record. Wireshark reads what is
  there and complains about the last packet.
