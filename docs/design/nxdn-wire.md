# NXDN — wire study

**Status:** verified against the reference implementations, 2026-09-07.
**Read first:** `docs/design/nxdn-network.md` for the network's place in astar;
this note is the citable source every later NXDN task reads its constants
from.

References fetched and read for this note:

- `g4klx/NXDNClients` — `NXDNGateway/NXDNNetwork.cpp` (`CNXDNNetwork::writePoll`,
  `writeUnlink`, `writeData`, `readData`), `NXDNGateway/NXDNGateway.cpp` (poll
  cadence), `NXDNGateway/Reflectors.h` (`CNXDNReflector::m_id`),
  `NXDNParrot/NXDNNetwork.cpp` (`CNXDNNetwork::read`).
- `s1lviu/NXDNReflector` (a verbatim copy of G4KLX's `NXDNReflector`, which is
  **not** in `NXDNClients` any more) — `NXDNReflector.cpp`
  (`CNXDNReflector::run`), `NXDNReflector.h` (`CNXDNRepeater`),
  `NXDNNetwork.cpp`.
- `g4klx/MMDVMHost` — `NXDNDefines.h`, `NXDNControl.cpp`, `NXDNAudio.cpp`,
  `NXDNLICH.cpp`, `NXDNSACCH.cpp`, `NXDNFACCH1.cpp`, `NXDNLayer3.cpp`,
  `NXDNCRC.cpp`, `NXDNIcomNetwork.cpp`.
- `nostar/DroidStar` — `serialambe.cpp` (`SerialAMBE::config_ambe`), `nxdn.cpp`
  (`NXDN::process_udp`, `send_ping`, `get_frame`, `encode_header`,
  `encode_data`, `interleave`).

## The datagrams

| | |
|---|---|
| `NXDNP` poll | **17 bytes**: `'N','X','D','N','P'` + 10-byte space-padded callsign + talkgroup as **big-endian u16**. `NXDNGateway/NXDNNetwork.cpp: CNXDNNetwork::writePoll` (`data[15] = (m_id >> 8) & 0xFF; data[16] = (m_id >> 0) & 0xFF;`), callsign padded by `m_callsign.resize(10U, ' ')` in the constructor. |
| `NXDNU` unlink | **17 bytes**, byte-identical layout, tag `'U'`. `CNXDNNetwork::writeUnlink`. |
| `NXDND` data | **43 bytes**: tag(5) + srcId BE u16 @5 + dstId BE u16 @7 + flags @9 + **33-byte NXDN frame** @10. `CNXDNNetwork::writeData` (`::memcpy(buffer + 10U, data, 33U)`, `m_socket->write(buffer, 43U, ...)`). |
| flags byte @9 | `0x01` group, `0x02` data (not voice), `0x04` start-of-transmission, `0x08` **end**-of-transmission. `CNXDNNetwork::writeData` sets them; `NXDNReflector.cpp` reads `(buffer[9U] & 0x08U) == 0x08U` → "Received end of transmission"; `DroidStar nxdn.cpp: NXDN::process_udp` reads the same bit. |
| accepted lengths | Only 17 and 43, and only with a `"NXDN"` prefix. `NXDNReflector/NXDNNetwork.cpp: CNXDNNetwork::read`; `NXDNGateway/NXDNNetwork.cpp: CNXDNNetwork::readData` is stricter still (`"NXDNP"` && 17, or `"NXDND"` && 43). |
| ids are 16-bit | `writeData(..., unsigned short srcId, unsigned short dstId, ...)`; `Reflectors.h: CNXDNReflector::m_id` is `unsigned short`. |

## The link

| | |
|---|---|
| Linking | Send **three** polls. `NXDNGateway.cpp` calls `m_remoteNetwork->writePoll(*reflector)` three times in a row at every link point. |
| Poll cadence | **Every 5 s.** `NXDNGateway.cpp: CTimer pollTimer(1000U, 5U);` re-polling the current TG on expiry. |
| Acknowledgement | The reflector **echoes the poll back verbatim**. `NXDNReflector.cpp`: `// Return the poll` → `nxdnNetwork.write(buffer, len, address, addressLen);`. `NXDNParrot/NXDNNetwork.cpp: CNXDNNetwork::read` does the same. There is no distinct ack packet. |
| Registration | Only when the poll's talkgroup equals the reflector's: `unsigned short id = (buffer[15U] << 8) | buffer[16U]; if (id == tg)`. A poll for the wrong TG is silently dropped. `NXDNReflector.cpp`. |
| Unlink | Three `NXDNU`, same triple. Removes the repeater. `NXDNGateway.cpp`, `NXDNReflector.cpp`. |
| Client timeout | **120 s** of silence and the reflector forgets a client. `NXDNReflector.h: CNXDNRepeater::m_timer(1000U, 120U)`. |
| Transmission watchdog | **1.5 s** with no frame ends the current transmission reflector-side. `NXDNReflector.cpp: CTimer watchdogTimer(1000U, 0U, 1500U);`. |
| Relay rule | A frame is relayed to every *other* client only when `grp && dstId == tg`. `NXDNReflector.cpp`. |

## The 33-byte network frame

This is **not** the 384-bit over-the-air RTCH frame. MMDVMHost strips the FSW,
the FEC and the interleave before it goes on the network, so what a client
receives is already de-coded.

| offset | bytes | what | citation |
|---|---|---|---|
| 0 | 1 | LICH raw | `MMDVMHost/NXDNControl.cpp`: `netData[0U] = lich.getRaw();` |
| 1 | 4 | SACCH raw (26 bits + CRC-6) | `NXDNControl.cpp`: `sacch.getRaw(netData + 1U);`; `NXDNSACCH.cpp: getRaw` copies 4 bytes then `CNXDNCRC::encodeCRC6(data, 26U)` |
| 5 | 14 | block 0 — **two 49-bit AMBE+2 frames**, or one FACCH1 raw | `NXDNControl.cpp`: `audio.decode(..., netData + 5U + 0U);` / `facch.getRaw(netData + 5U + 0U);` |
| 19 | 14 | block 1 — same | `NXDNControl.cpp`: `audio.decode(..., netData + 5U + 14U);` |

**LICH byte** (`MMDVMHost/NXDNLICH.cpp`): bits 7–6 `RFCT`, 5–4 `FCT`/USC, 3–2
`Option` (steal), bit 1 direction, bit 0 parity. Parity is `true` exactly when
`m_lich[0] & 0xF0` is `0x80` or `0xB0` (`CNXDNLICH::getParity`). Constants from
`MMDVMHost/NXDNDefines.h`: `RFCT_RCCH 0`, `RFCT_RTCH 1`, `RFCT_RDCH 2`,
`RFCT_RTCH_C 3`; `USC_SACCH_NS 0`, `USC_UDCH 1`, `USC_SACCH_SS 2`,
`USC_SACCH_IDLE 3`; `STEAL_FACCH 0`, `STEAL_FACCH1_1 1`, `STEAL_FACCH1_2 2`,
`STEAL_NONE 3`.

**Voice header and trailer** are LICH `0x81` (inbound) / `0x83` (outbound) —
`RFCT_RDCH`, `USC_SACCH_NS`, `STEAL_FACCH`, parity 1 — with **both** 14-byte
blocks carrying a FACCH1 raw whose byte 0 is the Layer-3 message type: `0x01`
`VCALL` (header) or `0x08` `TX_REL` (trailer). `NXDNGateway/NXDNNetwork.cpp:
writeData` (`if (data[0U] == 0x81U || data[0U] == 0x83U) { buffer[9U] |=
data[5U] == 0x01U ? 0x04U : 0x00U; buffer[9U] |= data[5U] == 0x08U ? 0x08U :
0x00U; }`), `MMDVMHost/NXDNDefines.h`, `DroidStar nxdn.cpp:
NXDN::encode_header`.

## The four voice frames

Each 14-byte block holds **two 49-bit frames at bit offsets 0 and 49** — 98 of
the block's 112 bits used, the last 14 zero-padded:

- `MMDVMHost/NXDNAudio.cpp: CNXDNAudio::decode(const unsigned char* in,
  unsigned char* out)` is exactly `decode(in + 0U, out, 0U); decode(in + 9U,
  out, 49U);`.
- `DroidStar nxdn.cpp: NXDN::process_udp` reads them at datagram offsets **15**
  (7 bytes, aligned), **21** (7 bytes, shifted left one bit — bit 49 of the
  block is bit 1 of byte 6), **29** (aligned) and **35** (shifted left one
  bit).

**Inside a 49-bit frame** the field order is `a` (12 bits at offset+0), `b` (12
bits at offset+12), `c` (25 bits at offset+24) — `MMDVMHost/NXDNAudio.cpp:
CNXDNAudio::decode(in, out, offset)` writes exactly that. **That is
byte-for-byte astar's existing logical order**: `astar_codec::ysf::DnFrame`'s
doc says "the twelve bits the Golay (24, 12) word protects, then the twelve
the (23, 12) word protects, then the twenty-five that nothing protects."

## CRCs

Needed only by Task 9, recorded here so the note is complete.
`MMDVMHost/NXDNCRC.cpp`:

- `createCRC6`: init `0x3F`, poly `0x27`, MSB-first over `length` bits, result
  masked `0x3F`, written into bits `length..length+6`.
- `createCRC12`: init `0x0FFF`, poly `0x080F`, MSB-first over `length` bits,
  result masked `0x0FFF`, written into bits `length..length+12`. FACCH1 uses
  `length = 80` (`NXDNFACCH1.cpp: getRaw`).

## The `VocoderMode` ruling

**Reuse `VocoderMode::YsfDn`. Do not add `VocoderMode::NxdnDn`.**

Citation, decisive and direct — `nostar/DroidStar`, `serialambe.cpp`,
`SerialAMBE::config_ambe()`:

```cpp
if(m_protocol == "DMR"){
    a.append(AMBE3000_2450_1150);  packet_size = 9;
}
else if( (m_protocol == "YSF") || (m_protocol == "NXDN") ){
    a.append(AMBE3000_2450_0000);  packet_size = 7;
}
```

and, same file:

```cpp
const uint8_t AMBE3000_2450_0000[17] = {0x61, 0x00, 0x0d, 0x00, 0x0a,
    0x04U,0x31U, 0x07U,0x54U, 0x00U,0x00U, 0x00U,0x00U, 0x00U,0x00U, 0x70U,0x31U};
```

which is byte-identical to `astar_codec::ysf::ratep_dn()`'s output — the test
`crates/astar-codec/src/ysf.rs::ratep_dn` already asserts `hex("61 00 0D 00 0A
04 31 07 54 00 00 00 00 00 00 70 31")`. Same RATEP word, same `packet_size =
7`, same 49-bit channel packet, same `dvsi_interleave` table (`DroidStar
nxdn.cpp` declares the identical 49-entry array `ysf.cpp` does, and applies it
only when a hardware dongle is in play).

Corroborated independently: `MMDVMHost/NXDNControl.cpp` regenerates NXDN's
on-air AMBE with `CAMBEFEC::regenerateYSFDN(...)` — the **YSF DN** function —
at offsets `+0, +9, +18, +27`. MMDVMHost treats NXDN's 49-bit AMBE+2 frame and
YSF DN's as the same object.

## Not verified

- **Layer-3 group bit.** `MMDVMHost/NXDNLayer3.cpp: getIsGroup` reads
  `(m_data[2U] & 0x80U) != 0x80U`; `NXDNReflector.cpp`'s Icom branch reads
  `(buffer[7U] & 0x20U) == 0x20U` over the same byte. The two disagree.
  **Not load-bearing for astar**: the group flag astar reads and writes is
  the `NXDND` header's `flags & 0x01`, which both references agree on. Do not
  build on `m_data[2]` without re-deriving it.
- **DroidStar's voice header omits the FACCH1 CRC-12.** `NXDN::encode_header`
  does `memcpy(&m_nxdnframe[15U], m_layer3, 14U)` — 14 raw Layer-3 bytes, no
  CRC — where `MMDVMHost/NXDNFACCH1.cpp: getRaw` writes 10 bytes plus
  `encodeCRC12(data, 80U)` into 12. Neither the reflector nor the parrot
  validates it (both check only tag and length), so RX is unaffected.
  **Task 9 follows MMDVMHost**, which is the self-consistent one.
