# DMR — the MMDVM/homebrew wire

**Status:** every value below was read out of the reference implementations on
2026-09-07 by fetching the cited file at its `master` branch HEAD and reading
the cited function. Nothing here is recalled. Rows that a reference
contradicted are written as the reference has them, and the disagreement is
recorded in **§10, Not verified and disagreements**.
**Read first:** `docs/design/dmr-networks.md` for DMR's place in astar — the
taxonomy, the identity question and the BrandMeister gate. This note is the
citable source every later DMR task reads its constants from.

Everything below was read out of GPL-2.0 and GPL-3.0 sources as a
**specification of the wire** and re-derived here. No code was copied; astar is
AGPL-3.0-only and the licences do not mix. Constants and layouts only, each row
naming the file and function it came from — which is also what makes it
checkable later. Where an algorithm is needed it is given by its mathematical
definition (generator polynomial, parity equations, field), not by transcribing
someone's table.

## References fetched and read

Every file was fetched over HTTPS and read in full or, for the two very large
ones, read in full across the cited functions.

| Project | File | URL fetched |
|---|---|---|
| `g4klx/DMRGateway` | `DMRNetwork.cpp` | `https://raw.githubusercontent.com/g4klx/DMRGateway/master/DMRNetwork.cpp` |
| `g4klx/DMRGateway` | `DMRNetwork.h` | `.../g4klx/DMRGateway/master/DMRNetwork.h` |
| `g4klx/MMDVMHost` | `DMRDefines.h` | `.../g4klx/MMDVMHost/master/DMRDefines.h` |
| `g4klx/MMDVMHost` | `DMRSlot.cpp` | `.../g4klx/MMDVMHost/master/DMRSlot.cpp` |
| `g4klx/MMDVMHost` | `DMRFullLC.cpp` | `.../g4klx/MMDVMHost/master/DMRFullLC.cpp` |
| `g4klx/MMDVMHost` | `DMRLC.cpp` | `.../g4klx/MMDVMHost/master/DMRLC.cpp` |
| `g4klx/MMDVMHost` | `DMREMB.cpp` | `.../g4klx/MMDVMHost/master/DMREMB.cpp` |
| `g4klx/MMDVMHost` | `DMREmbeddedData.cpp` | `.../g4klx/MMDVMHost/master/DMREmbeddedData.cpp` |
| `g4klx/MMDVMHost` | `Sync.cpp` — **not** `DMRSync.cpp`, which does not exist | `.../g4klx/MMDVMHost/master/Sync.cpp` |
| `g4klx/MMDVMHost` | `DMRSlotType.cpp` | `.../g4klx/MMDVMHost/master/DMRSlotType.cpp` |
| `g4klx/MMDVMHost` | `BPTC19696.cpp` | `.../g4klx/MMDVMHost/master/BPTC19696.cpp` |
| `g4klx/MMDVMHost` | `Golay2087.cpp` | `.../g4klx/MMDVMHost/master/Golay2087.cpp` |
| `g4klx/MMDVMHost` | `Hamming.cpp` | `.../g4klx/MMDVMHost/master/Hamming.cpp` |
| `g4klx/MMDVMHost` | `QR1676.cpp` | `.../g4klx/MMDVMHost/master/QR1676.cpp` |
| `g4klx/MMDVMHost` | `RS129.cpp` | `.../g4klx/MMDVMHost/master/RS129.cpp` |
| `g4klx/MMDVMHost` | `AMBEFEC.cpp` | `.../g4klx/MMDVMHost/master/AMBEFEC.cpp` |
| `g4klx/MMDVMHost` | `CRC.cpp` | `.../g4klx/MMDVMHost/master/CRC.cpp` |
| `nostar/DroidStar` | `serialambe.cpp` | `.../nostar/DroidStar/master/serialambe.cpp` |
| `nostar/DroidStar` | `dmr.cpp` | `.../nostar/DroidStar/master/dmr.cpp` |
| `nostar/DroidStar` | `dmr.h` | `.../nostar/DroidStar/master/dmr.h` |
| `nostar/DroidStar` | `CRCenc.h` — read to check the embedded-LC checksum citation in §6 | `.../nostar/DroidStar/master/CRCenc.h` |
| `nostar/DroidStar` | `CRCenc.cpp` — same | `.../nostar/DroidStar/master/CRCenc.cpp` |
| `nostar/DroidStar` | `mode.h` — read to settle the colour-code question in §10 | `.../nostar/DroidStar/master/mode.h` |
| `HBLink-org/hblink3` | `hblink.py` | `.../HBLink-org/hblink3/master/hblink.py` |
| `HBLink-org/hblink3` | `playback.py` | `.../HBLink-org/hblink3/master/playback.py` |
| `HBLink-org/hblink3` | `const.py` — read to name the frame-type values | `.../HBLink-org/hblink3/master/const.py` |

`MMDVMHost` has no `DMRSync.cpp`; the DMR sync writers live in the shared
`Sync.cpp` (`CSync::addDMRDataSync`, `CSync::addDMRAudioSync`) and the sync
*constants* in `DMRDefines.h`. The repository listing was taken from the GitHub
contents API to confirm that.

---

## 1. The handshake

All packets UDP, one master, one socket, one datagram per message.
`DMRGateway/DMRNetwork.cpp` unless stated. `hblink.py` citations are the
`HBSYSTEM.master_datagramReceived` / `peer_datagramReceived` branches.

| packet | bytes | layout | citation |
|---|---|---|---|
| `RPTL` login | **8** | `"RPTL"`(4) + repeater id BE u32(4) | `CDMRNetwork::writeLogin`; `DroidStar dmr.cpp: hostname_lookup` builds the same 8; `hblink.py: peer_maintenance_loop` sends `RPTL + RADIO_ID` |
| `RPTACK` + salt | **10** | `"RPTACK"`(6) + 4-byte salt | `CDMRNetwork::clock`, `STATUS::WAITING_LOGIN`: salt taken from `m_buffer + 6U`, `sizeof(uint32_t)`; `hblink.py` master `RPTL` branch sends `RPTACK` joined with `bytes_4(randint(0, 0xFFFFFFFF))`; `hblink.py` peer `RPTA` branch reads it at `_data[6:10]` |
| `RPTK` auth | **40** | `"RPTK"`(4) + id(4) + SHA-256 digest(32) | `CDMRNetwork::writeAuthorisation`; `DroidStar dmr.cpp: process_udp`, `CONNECTING` branch, builds a 40-byte `out`; `hblink.py` peer sends `RPTK + RADIO_ID + digest` |
| the digest | | `SHA256(salt_bytes ‖ password_bytes)` — the salt as the **4 raw bytes received**, never re-encoded, then the password's bytes. Digest sent **raw**, not hex | `writeAuthorisation` hashes `m_salt`(4) ‖ `m_password`; `hblink.py` master computes `bhex(sha256(_salt_str + PASSPHRASE).hexdigest())` and compares it against `_data[8:]`; `DroidStar` appends `buf[6]..buf[9]` then the password |
| `RPTACK` + id | **10** | `"RPTACK"`(6) + id(4) — the reply to `RPTK`, to `RPTC` and to `RPTO` | `hblink.py` master `RPTK`, `RPTC` and `RPTO` branches all send `RPTACK` joined with `_peer_id` |
| `RPTC` config | **302** | `"RPTC"`(4) + id(4) + 294 config bytes — §2 | `CDMRNetwork::writeConfig` (`m_configLen + 8U`); `DroidStar dmr.cpp: process_udp` `DMR_AUTH` branch, one `::sprintf` into `buffer + 8U` then `out.append(buffer, 302)`; `hblink.py` master slices to offset 302 |
| `RPTO` options | 8 + n | `"RPTO"`(4) + id(4) + ASCII options, no terminator | `CDMRNetwork::writeOptions` writes `m_options.length() + 8U`. **astar sends none** — §3 |
| `RPTPING` | **11** | `"RPTPING"`(7) + id(4) | `CDMRNetwork::writePing`; `DroidStar dmr.cpp: send_ping`; `hblink.py` master reads the id at `_data[7:11]` |
| `MSTPONG` | **11** | `"MSTPONG"`(7) + id(4) | `CDMRNetwork::clock` matches 7 bytes; `hblink.py` master sends `MSTPONG + _peer_id`, peer reads `_data[7:11]`; `DroidStar dmr.cpp: process_udp` gates on `buf.size() == 11` |
| `MSTNAK` | **10** | `"MSTNAK"`(6) + id(4) — rejection at any stage | `CDMRNetwork::clock` matches 6 bytes; `hblink.py` master writes `MSTNAK + _peer_id` at every rejection, peer reads `_data[6:10]` |
| `MSTCL` | **9** | `"MSTCL"`(5) + id(4) — master closing | `CDMRNetwork::clock` matches 5 bytes; `hblink.py: master_dereg` sends `MSTCL + _peer`, peer reads `_data[5:9]` |
| `RPTCL` | **9** | `"RPTCL"`(5) + id(4) — client closing | `CDMRNetwork::close(true)` writes 9; `DroidStar dmr.cpp: send_disconnect`; `hblink.py: peer_dereg` sends `RPTCL + RADIO_ID` |
| `RPTSBKN` | 7 + n | `"RPTSBKN"`(7) + id — master asks for a site beacon | `CDMRNetwork::clock` sets `m_beacon`; `hblink.py` peer `RPTS` branch. astar ignores it |

**A rejection is never fatal on its own.** `hblink.py`'s master answers a
client's own `RPTCL` with `MSTNAK` (the `RPTC`/`RPTCL` branch deletes the peer
and NAKs it). astar must not read the `MSTNAK` that follows its own goodbye as
an error.

**Commands are matched on four bytes.** `hblink.py` sets `_command = _data[:4]`,
so `RPTC` and `RPTCL` share a branch and are separated by re-testing
`_data[:5]`, and `RPTP`/`MSTP`/`MSTN`/`MSTC`/`RPTA`/`RPTS` are four-byte
prefixes of the longer tags. `DMRGateway` instead `memcmp`s the full tag.
Sending the full tag satisfies both.

**Which stage a `MSTNAK` arrives at is the diagnosis, and it is worth keeping.**
`DMRGateway/DMRNetwork.cpp: writeJSONLinkFailed` maps them: NAK to `RPTL` =
id not permitted; NAK to `RPTK` = **wrong password**, the common case; NAK to
`RPTC`/`RPTO` = config rejected, e.g. this id is already connected elsewhere;
NAK while running = session dropped; `MSTCL` = master closed; no reply at all =
unreachable. astar's error text should make the same distinction.

### The keepalive direction is settled: the CLIENT pings

`RPTPING` → `MSTPONG`, client to master. `CDMRNetwork::writePing` is called
from the client's own `clock()` on the `RUNNING` arm of the retry timer;
`hblink.py`'s **master** branch answers `RPTP` with `MSTPONG + _peer_id`, and
its **peer** branch sends `RPTPING + RADIO_ID` from `peer_maintenance_loop`.
`DroidStar dmr.cpp: send_ping` is on the client's own timer. Three independent
implementations, all one direction. The `MSTPING`/`RPTPONG` naming in the
OK-DMR transcription (`research-dmr.md` §1) is the reversed reading and every
live implementation contradicts it.

### Cadences

| | value | citation |
|---|---|---|
| `PING_INTERVAL` | **5 s** | `DroidStar dmr.cpp: setup_connection` — `m_ping_timer->start(5000)`. The BrandMeister wiki's recommended 5–15 s, 5 preferred, agrees |
| `LOGIN_RETRY` | **10 s** | `DMRGateway/DMRNetwork.cpp`, constructor: `m_retryTimer(1000U, 10U)` |
| `LINK_TIMEOUT` | **60 s** | same constructor: `m_timeoutTimer(1000U, 60U)` |
| burst cadence | **60 ms** | `MMDVMHost/DMRDefines.h`: `DMR_SLOT_TIME = 60U`; `hblink3/playback.py` re-sends a recorded stream with `sleep(0.06)` between datagrams |

`DMRGateway` has no separate ping timer: `clock()` reuses `m_retryTimer` for
the ping when the state is `RUNNING`, so G4KLX's gateway pings at **10 s** and
DroidStar at 5 s. Both sit inside the recommended window; astar takes 5 s,
which is the softclient's number and the more conservative one.

---

## 2. The 302-byte `RPTC` body

Offsets from the start of the datagram. Every field is **ASCII**, space-padded
on the right, except the id.

| offset | len | field | format | citation |
|---|---|---|---|---|
| 0 | 4 | `"RPTC"` | | `writeConfig`; `hblink.py` `RPTC` branch |
| 4 | 4 | repeater/radio id | BE u32, binary | `writeConfig` `::memcpy(buffer + 4U, m_id, 4U)`; `DroidStar` writes the four shifted bytes |
| 8 | 8 | callsign | `%-8.8s` | `DroidStar` sprintf; `hblink.py` `_data[8:16]` |
| 16 | 9 | rx frequency, Hz | `%09u` | `DroidStar` sprintf; `hblink.py` `_data[16:25]` |
| 25 | 9 | tx frequency, Hz | `%09u` | `DroidStar` sprintf; `hblink.py` `_data[25:34]` |
| 34 | 2 | tx power, dBm | `%02u` | `DroidStar` sprintf; `hblink.py` `_data[34:36]` |
| 36 | 2 | colour code | `%02u` | `DroidStar` sprintf; `hblink.py` `_data[36:38]` |
| 38 | 8 | latitude | `%8.8s` | `DroidStar` sprintf; `hblink.py` `_data[38:46]`; `writeConfig`'s placeholder patch |
| 46 | 9 | longitude | `%9.9s` | `DroidStar` sprintf; `hblink.py` `_data[46:55]`; `writeConfig`'s placeholder patch |
| 55 | 3 | height, m | `%03d` | `DroidStar` sprintf; `hblink.py` `_data[55:58]` |
| 58 | 20 | location | `%-20.20s` | `DroidStar` sprintf; `hblink.py` `_data[58:78]` |
| 78 | 19 | description | `%-19.19s` | `DroidStar` sprintf; `hblink.py` `_data[78:97]` |
| 97 | 1 | slots | one character | `DroidStar` sprintf `%c`; `hblink.py` `_data[97:98]` |
| 98 | 124 | URL | `%-124.124s` | `DroidStar` sprintf; `hblink.py` `_data[98:222]` |
| 222 | 40 | software id | `%-40.40s` | `DroidStar` sprintf; `hblink.py` `_data[222:262]` |
| 262 | 40 | package id | `%-40.40s` | `DroidStar` sprintf; `hblink.py` `_data[262:302]` |

The primary citation is `DroidStar dmr.cpp: process_udp`'s single
`::sprintf(buffer + 8U, "%-8.8s%09u%09u%02u%02u%8.8s%9.9s%03d%-20.20s%-19.19s%c%-124.124s%-40.40s%-40.40s", ...)`
followed by `out.append(buffer, 302)`.

**Three independent implementations agree, at every offset.**

* `hblink3`'s master parses the datagram field by field with explicit slices,
  which is a byte-for-byte second reading of the whole table — including the
  description/slots boundary at 97.
* `DMRGateway/DMRNetwork.cpp: writeConfig` corroborates offset 38 exactly: with
  no location configured it patches `::memcpy(buffer + 38U, "0.00000000.000000", 17U)`.
  Seventeen bytes at 38 is precisely latitude(8) + longitude(9).
* `DroidStar` reaches the same 17 bytes from the other direction: it renders
  latitude with `%08f` and longitude with `%09f` before the `%8.8s`/`%9.9s`
  fields, so an unset location gives `"0.000000"` and `"00.000000"` — the same
  seventeen characters G4KLX writes as a literal.

`research-dmr.md`'s table gives description 20 and no separate slots byte. The
sum is the same 302 and the two disagree by one character. **The 19 + 1 split
is correct**, and it is not a judgement call between one implementation and a
summary: DroidStar writes it and hblink3 parses it. See §10.

---

## 3. What astar sends in `RPTC`, and why

astar is a softclient with no antenna, no site and no radio. It must identify
itself honestly to any network it connects to (`dmr-networks.md`: "astar should
identify itself honestly … never disguise itself as approved firmware"), and it
must not carry anyone's coordinates into the repository.

| field | value | reason |
|---|---|---|
| callsign | the operator's | it is the only true thing here |
| rx / tx freq | `000000000` | there is no radio; zero is honest, a plausible-looking number is not |
| tx power | `00` | ditto |
| colour code | `01` | `DroidStar dmr.cpp`'s `m_txcc(1)` constructor default; a network-only connection has no RF colour code to clash with |
| latitude / longitude | `"0.000000"` / `"00.000000"` | `DMRGateway/DMRNetwork.cpp: writeConfig`'s own no-location placeholder, used verbatim |
| height | `000` | |
| location | `""` | |
| description | `"astar softclient"` | |
| slots | `'4'` | `DroidStar dmr.cpp`'s literal `'4'` in the sprintf argument list. **Semantics not verified** — §10 |
| URL | `""` | |
| software id | `"astar <workspace version>"` | honest self-identification; the workspace version is `0.1.11-beta` at the time of writing and comes from `Cargo.toml`, not a literal |
| package id | `"astar"` | |

astar sends **no `RPTO`**. Options are a per-master static-talkgroup request
with a per-master grammar; `hblink.py`'s master logs the option string and acts
on nothing in it. astar activates a talkgroup the portable way, by transmitting
to it (`research-dmr.md` §5: dynamic TG activation), and until transmit exists
it activates nothing at all.

---

## 4. The `DMRD` data packet — 55 bytes

| offset | len | field | citation |
|---|---|---|---|
| 0 | 4 | `"DMRD"` | `CDMRNetwork::write(const CDMRData&)` |
| 4 | 1 | sequence number | `buffer[4U] = data.getSeqNo()`; read back as `m_buffer[4U]`; `hblink.py` `_seq = _data[4]` |
| 5 | 3 | source id, BE u24 | `buffer[5U] = srcId >> 16` and the two following; `hblink.py` `_data[5:8]` |
| 8 | 3 | destination id (talkgroup or radio id), BE u24 | `buffer[8U] = dstId >> 16` and the two following; `hblink.py` `_data[8:11]` |
| 11 | 4 | repeater/radio id, BE u32 | `::memcpy(buffer + 11U, m_id, 4U)`; `hblink.py` `_peer_id = _data[11:15]` |
| 15 | 1 | bits — table below | |
| 16 | 4 | stream id — **opaque, four bytes** | `::memcpy(buffer + 16U, &streamId, 4U)` and, on read, `::memcpy(&streamId, m_buffer + 16U, 4U)`: a raw host-order `memcpy` in both directions in `DMRGateway`, so it has **no defined endianness on the wire**. `hblink.py` keeps it as the raw slice `_data[16:20]` and only ever compares it for equality. astar carries it as `[u8; 4]` |
| 20 | 33 | the DMR burst | `data.getData(buffer + 20U)`; `hblink3/playback.py`: `dmrpkt = _data[20:53]` |
| 53 | 1 | BER | `buffer[53U] = data.getBER()`; read back as `m_buffer[53U]` |
| 54 | 1 | RSSI | `buffer[54U] = data.getRSSI()`; read back as `m_buffer[54U]` |

`HOMEBREW_DATA_PACKET_LENGTH = 55U` (`DMRGateway/DMRNetwork.cpp`, line 32).
`DroidStar dmr.cpp: process_udp` gates every `DMRD` branch on `buf.size() == 55`
and `build_frame`/`send_frame` write `txdata.append((char *)m_dmrFrame, 55)`.
`hblink3`'s XLX module-change packet in `hblink.py: send_xlxmaster` is likewise
`DMRD` + 51 bytes, of which the last two are zero — 55 total.

**This overrides `research-dmr.md`'s "budget for 53"**: the two trailing bytes
are not an extension, they are part of what every deployed client and gateway
sends and expects. astar **sends 55** and, on receive, **accepts 53 or 55** —
`hblink3` slices by offset and never checks a `DMRD`'s length at all, so a
master built on it will relay whatever it was given. Its repeat path
(`master_datagramReceived`) rebuilds the datagram as `_data[:11]` + the target
peer's id + `_data[15:]`, passing everything from offset 15 onward through
unchanged, whatever its length.

### The bits byte (offset 15)

| bits | meaning | citation |
|---|---|---|
| 7 (`0x80`) | timeslot: `0` = TS1, `1` = TS2 | `buffer[15U] = slotNo == 1U ? 0x00U : 0x80U`; read back as `(m_buffer[15U] & 0x80U) == 0x80U ? 2U : 1U`; `hblink.py` `_slot = 2 if (_bits & 0x80) else 1` |
| 6 (`0x40`) | call type: **`0` = group, `1` = unit-to-unit (private)** | `buffer[15U] \|= flco == FLCO::GROUP ? 0x00U : 0x40U`, and on read `FLCO flco = (m_buffer[15U] & 0x40U) == 0x40U ? FLCO::USER_USER : FLCO::GROUP`; `hblink.py`: `if _bits & 0x40: _call_type = 'unit'` |
| 5–4 (`0x30`) | frame type: `00` voice (N in bits 3–0), `01` voice sync, `10` data sync (data type in bits 3–0) | `if (dataType == DT_VOICE_SYNC) buffer[15U] \|= 0x10U; else if (dataType == DT_VOICE) buffer[15U] \|= data.getN(); else buffer[15U] \|= (0x20U \| dataType);`. `hblink.py`: `_frame_type = (_bits & 0x30) >> 4`, and `const.py` names the values `HBPF_VOICE = 0x0`, `HBPF_VOICE_SYNC = 0x1`, `HBPF_DATA_SYNC = 0x2` |
| 3–0 (`0x0F`) | voice sequence `0..5` = burst A..F, **or** the DMR data type | same; `hblink.py`: `_dtype_vseq = (_bits & 0xF)` with the comment "data, 1=voice header, 2=voice terminator; voice, 0=burst A … 5=burst F" |

**The call-type polarity is settled and `research-dmr.md` §1's flagged
disagreement is resolved: `0x40` set means PRIVATE.** Two independent
implementations — G4KLX's gateway, on both write and read, and HBlink3's master
and peer branches — agree. The go-dmr comment that says `1 = group` is wrong.
Getting this backwards turns every group call into a silently-misrouted private
call, which is why it is called out here rather than left in a table.

`hblink3`'s live code adds a third case between `unit` and `group`: a frame
with `(_bits & 0x23) == 0x23` — data sync, data type `DT_CSBK` — is classified
`vcsbk`. It does not touch the `0x40` reading. See §10.

### Data types

`MMDVMHost/DMRDefines.h`, masked with `DT_MASK = 0x0F`:

`DT_VOICE_PI_HEADER 0x00`, `DT_VOICE_LC_HEADER 0x01`,
`DT_TERMINATOR_WITH_LC 0x02`, `DT_CSBK 0x03`, `DT_MBC_HEADER 0x04`,
`DT_MBC_CONTINUATION 0x05`, `DT_DATA_HEADER 0x06`, `DT_RATE_12_DATA 0x07`,
`DT_RATE_34_DATA 0x08`, `DT_IDLE 0x09`, `DT_RATE_1_DATA 0x0A`.

The two internal markers `DT_VOICE_SYNC 0xF0` and `DT_VOICE 0xF1` are
MMDVMHost-local sentinels — `DMRDefines.h` labels them "Dummy values" — and
**never appear in the bits nibble**; `CDMRNetwork::write` turns them into the
`0x10` and `0x00` frame types instead.

**Stream boundaries.** A stream opens on a data-sync burst with
`DT_VOICE_LC_HEADER` and closes on one with `DT_TERMINATOR_WITH_LC`.
`DroidStar dmr.cpp: process_udp` reads exactly that: `buf[15] & 0x20` selects
the data-sync branch, then `& 0x02` is EOT and `& 0x01` is a new stream.
`hblink3/playback.py` closes a recording on
`_frame_type == HBPF_DATA_SYNC and _dtype_vseq == HBPF_SLT_VTERM`.

---

## 5. The 33-byte burst

264 bits. The ETSI shape is `[108 bits info][48 bits SYNC or EMB+embedded-LC][108 bits info]`,
and the split is nibble-aligned, not byte-aligned:

| burst bits | burst bytes | what |
|---|---|---|
| 0–107 | 0..12 whole, 13 high nibble | information half 1 |
| 108–155 | 13 low nibble, 14..18 whole, 19 high nibble | SYNC (bursts A and signalling) or EMB + embedded LC (bursts B–F) |
| 156–263 | 19 low nibble, 20..32 whole | information half 2 |

Citation: `MMDVMHost/DMRDefines.h`'s `SYNC_MASK[] = {0x0F, 0xFF, 0xFF, 0xFF,
0xFF, 0xFF, 0xF0}` applied at `data + 13U` for seven bytes
(`MMDVMHost/Sync.cpp: CSync::addDMRAudioSync`, and the identical private copies
in `DroidStar dmr.cpp: addDMRAudioSync` / `addDMRDataSync`), together with
`DMR_SYNC_LENGTH_BITS = 48U` and `DMR_FRAME_LENGTH_BITS = 264U`.
`DMR_SYNC_LENGTH_BYTES` is `6U` — 48 bits — even though the write touches seven
bytes, because the first and last are half-masked.

**On a signalling burst the two info halves are not all information.** The
slot type takes ten bits from the end of half 1 and ten from the start of
half 2, leaving 98 + 98 = 196 for the BPTC payload:

| burst bits | what |
|---|---|
| 0–97 | BPTC(196,96) payload, first 98 bits |
| 98–107 | slot type, first 10 bits |
| 108–155 | sync |
| 156–165 | slot type, second 10 bits |
| 166–263 | BPTC(196,96) payload, second 98 bits |

Citation: `MMDVMHost/BPTC19696.cpp: decodeExtractBinary` reads the burst's first
98 bits, then bits 6 and 7 of burst byte 20 — which are burst bits 166 and 167 —
then burst bytes 21..32; and `MMDVMHost/DMRSlotType.cpp: getData` writes only
the complementary bits, preserving `data[20U] & 0x03U`. On a **voice** burst
there is no slot type and all 216 bits outside the sync field are AMBE.

### Three 72-bit AMBE+2 frames per burst

Laid contiguously across the two halves, so that frame 2 **straddles** the sync
field:

```
ambe[0..12]      -> burst[0..12]
ambe[13] & 0xF0  -> burst[13] high nibble
ambe[13] & 0x0F  -> burst[19] low nibble
ambe[14..26]     -> burst[20..32]
```

Citation, transmit — `DroidStar dmr.cpp: send_frame` copies 13 bytes to
`m_dmrFrame + 20U`, then puts `m_ambe[13] & 0xF0` at `m_dmrFrame[33U]` and
`m_ambe[13] & 0x0F` at `m_dmrFrame[39U]`, then copies `m_ambe[14..26]` to
`m_dmrFrame + 40U`. Since `m_dmrFrame + 20` is the burst, `[33]` is burst byte
13 and `[39]` is burst byte 19.

Citation, receive — `DroidStar dmr.cpp: process_udp` is the exact inverse: it
copies 14 bytes from the burst, masks byte 13 to `0xF0`, ORs in
`dmrframe[19] & 0x0F`, then copies burst bytes 20..32 into `dmr3ambe[14..26]`.
`process_modem_data` does the same unpack a second time.

**Independently corroborated at the bit level** by
`MMDVMHost/AMBEFEC.cpp: CAMBEFEC::regenerateDMR(unsigned char*)`, which
addresses the three frames as `pos`, `pos + 72` with `+= 48` once the result
reaches 108, and `pos + 192`. That is precisely: frame 1 at burst bits 0–71,
frame 2 at 72–107 and 156–191, frame 3 at 192–263. Two projects, two
representations, the same layout.

So the 27 bytes are three 9-byte frames at 0..8, 9..17 and 18..26.
**This confirms the packing `research-dmr.md` §3 flagged as "commonly cited,
not independently re-verified"** — first half = frame 1 plus the first 36 bits
of frame 2, second half = the remaining 36 bits of frame 2 plus frame 3.

### Sync patterns

`MMDVMHost/DMRDefines.h`, seven bytes each, written into burst bytes 13..19
under `SYNC_MASK`:

| name | bytes |
|---|---|
| `MS_SOURCED_AUDIO_SYNC` | `07 F7 D5 DD 57 DF D0` |
| `MS_SOURCED_DATA_SYNC` | `0D 5D 7F 77 FD 75 70` |
| `BS_SOURCED_AUDIO_SYNC` | `07 55 FD 7D F7 5F 70` |
| `BS_SOURCED_DATA_SYNC` | `0D FF 57 D7 5D F5 D0` |
| `DIRECT_SLOT1_AUDIO_SYNC` | `05 D5 77 F7 75 7F F0` |
| `DIRECT_SLOT1_DATA_SYNC` | `0F 7F DD 5D DF D5 50` |
| `DIRECT_SLOT2_AUDIO_SYNC` | `07 DF FD 5F 55 D5 F0` |
| `DIRECT_SLOT2_DATA_SYNC` | `0D 75 57 F5 FF 7F 50` |
| `SYNC_MASK` | `0F FF FF FF FF FF F0` |

**astar is a mobile station and sends `MS_SOURCED_*`.** `MMDVMHost/Sync.cpp`
selects `BS_SOURCED_*` when `duplex` is true and `MS_SOURCED_*` when it is
false; `DroidStar dmr.cpp` always calls `addDMRAudioSync(m_dmrFrame + 20, 0)`
and `addDMRDataSync(m_dmrFrame + 20, 0)` — the `duplex == false`, MS branch.

This answers `research-dmr.md` §2's "exact 48-bit hex values were not found in
a citable open-web prose source": they are in `DMRDefines.h`, and they are
constants of the standard, not code.

---

## 6. EMB and embedded LC (bursts B–F)

The 48-bit middle field of a non-sync voice burst is
`EMB(8) ‖ embedded-LC fragment(32) ‖ EMB(8)`:

```
burst[13] low nibble  = EMB[0] high nibble          }
burst[14] high nibble = EMB[0] low nibble           } EMB first half,  bits 108-115
burst[14] low nibble  = fragment byte 0 low nibble  }
burst[15..17]         = fragment bytes 1..3         } 32-bit fragment, bits 116-147
burst[18] high nibble = fragment byte 4 high nibble }
burst[18] low nibble  = EMB[1] high nibble          }
burst[19] high nibble = EMB[1] low nibble           } EMB second half, bits 148-155
```

Citations: `MMDVMHost/DMREMB.cpp: CDMREMB::getData` / `putData` for the EMB
nibbles, and `MMDVMHost/DMREmbeddedData.cpp: CDMREmbeddedData::getData` /
`addData` for the fragment. `DroidStar dmr.cpp: get_emb_data` and
`get_embedded_data` are byte-for-byte the same two operations.

`EMB[0] = ((cc << 4) & 0xF0) | (PI ? 0x08 : 0) | ((lcss << 1) & 0x06)`,
`EMB[1] = 0`, then QR(16,7,6) over the pair. `MMDVMHost` carries the PI flag as
a field; `DroidStar` has that line commented out and always leaves `0x08`
clear. astar leaves it clear too — astar sends no PI header.

### LCSS

From `CDMREmbeddedData::getData(data, n)` (and `DroidStar`'s
`get_embedded_data`), for `n` in 1..4, which decrements before use:

| n | LCSS | meaning |
|---|---|---|
| 1 | `1` | first fragment |
| 2, 3 | `3` | continuation |
| 4 | `2` | last fragment |
| anything else | `0` | no fragment; the five fragment bytes are cleared |

`DroidStar dmr.cpp: encode_data` calls it with `n = (m_dmrcnt - 1) % 6`, which
runs 1..5 over bursts B..F, so burst F takes the `0` branch and carries no
fragment. `CDMREmbeddedData::addData` reassembles in the same order —
`lcss == 1` starts, two `lcss == 3` continue, `lcss == 2` finishes and triggers
the decode.

The 128-bit BPTC(128,77) matrix carrying the 72 LC bits plus the 5-bit checksum
is built once per superframe (`encodeEmbeddedData`) and read out in four 32-bit
fragments across bursts B–E.

### The embedded-LC "CRC-5" is not a CRC

`MMDVMHost/CRC.cpp: CCRC::encodeFiveBit` reads the 72 LC bits as nine bytes,
**sums them as unsigned integers**, and takes the total **mod 31**. Write it as
that, not as a polynomial.

`DroidStar` carries its own copy: `CRCenc.h` declares
`CCRC::encodeFiveBit(const bool*, uint32_t&)` and `CRCenc.cpp` defines it as
the identical loop — nine bytes read big-endian out of the 72 bits, summed,
`% 31` — called from `dmr.cpp: encode_embedded_data`. Two projects, the same
arithmetic; neither is a polynomial CRC.

---

## 7. Full LC (voice header and terminator bursts)

Nine bytes, then RS(12,9) parity, then BPTC(196,96) into both info halves:

```
lc[0]    = FLCO             (MMDVMHost/DMRDefines.h: FLCO::GROUP = 0, FLCO::USER_USER = 3)
lc[1]    = FID              (FID_ETSI = 0, FID_DMRA = 16)
lc[2]    = service options
lc[3..5] = destination id, BE u24
lc[6..8] = source id, BE u24
RS(12,9) parity over lc[0..8] -> parity[0..2]
lc[9]  = parity[2] ^ mask[0]
lc[10] = parity[1] ^ mask[1]
lc[11] = parity[0] ^ mask[2]
```

`VOICE_LC_HEADER_CRC_MASK = {0x96, 0x96, 0x96}`,
`TERMINATOR_WITH_LC_CRC_MASK = {0x99, 0x99, 0x99}` (`MMDVMHost/DMRDefines.h`).
Citations for the byte assembly: `MMDVMHost/DMRLC.cpp: CDMRLC::getData` and
`DroidStar dmr.cpp: lc_get_data`; for the **reversed** parity order:
`MMDVMHost/DMRFullLC.cpp: CDMRFullLC::encode` and `DroidStar dmr.cpp:
full_lc_encode`, which are identical. `CDMRFullLC::decode` un-XORs the same
mask and then `CRS129::check` re-encodes and compares, so the reversal is
symmetric.

`lc[0]` also carries two flags above the FLCO: bit 7 PF (protect) and bit 6 R
(reserved), with `FLCO` read as `lc[0] & 0x3F`
(`MMDVMHost/DMRLC.cpp`, `CDMRLC(const unsigned char*)`). astar sends both
clear. The remaining `FLCO` values are `TALKER_ALIAS_HEADER 4`,
`TALKER_ALIAS_BLOCK1..3 5,6,7`, `GPS_INFO 8` — embedded-LC payloads astar
decodes only if it later wants talker alias.

### Slot type

Written into the four bytes that flank the sync field on a signalling burst.
`SlotType[0] = (cc << 4) | dataType`, `SlotType[1] = SlotType[2] = 0`, then
Golay(20,8) over that, giving a 20-bit codeword held left-aligned in the three
bytes — codeword bits 19..12 in byte 0, 11..4 in byte 1, 3..0 in byte 2's high
nibble.

**The codeword then goes into the burst contiguously, most significant bit
first, in the ten bits either side of the sync field:**

| codeword bits | burst bits | burst bytes |
|---|---|---|
| 19..14 | 98–103 | byte 12, bits 5..0 |
| 13..10 | 104–107 | byte 13, bits 7..4 |
| 9..6 | 156–159 | byte 19, bits 3..0 |
| 5..0 | 160–165 | byte 20, bits 7..2 |

The masked bits each write leaves alone — byte 12's top two, byte 13's low
nibble, byte 19's high nibble, byte 20's low two — are the sync field and the
BPTC payload's bits 166 and 167, exactly as §5 says.

Citations: `MMDVMHost/DMRSlotType.cpp: CDMRSlotType::getData`, and
`DroidStar dmr.cpp: get_slot_data`, which perform the identical four masked
writes; `CDMRSlotType::putData` is the exact inverse and was checked against it.

---

## 8. The forward error correction, by definition

Later tasks implement these; none of the reference tables were copied. Each is
given here by its mathematical definition, and each definition was **checked
against the reference's own encoding table** by hand before being written down.

**BPTC(196,96)** — the full-LC and signalling payload.
`MMDVMHost/BPTC19696.cpp`. Interleave is arithmetic, not tabular:
`deinterleaved[a] = raw[(a * 181) mod 196]` for `a` in 0..195, and the encoder
is the same permutation applied backwards. The deinterleaved array is one
unused bit at index 0 followed by a 13 × 15 matrix at indices 1..195, row `r`
starting at `15r + 1`. Rows 0..8 are **Hamming(15,11,3)** codewords (11 data,
4 parity); all fifteen columns are **Hamming(13,9,3)** codewords (9 data rows,
4 parity rows). The 96 payload bits occupy row 0's positions 3..10 — its first
three data positions are unused, alongside index 0 — and positions 0..10 of
rows 1..8, giving 8 + 8 × 11 = 96.

**BPTC(128,77)** — the embedded LC. `MMDVMHost/DMREmbeddedData.cpp`. A
16-wide, 8-tall matrix. Rows 0..6 are **Hamming(16,11,4)** codewords; row 7 is
the column parity of rows 0..6. Row 0 and row 1 carry 11 LC bits each; rows
2..6 carry 10 LC bits plus one checksum bit at row position 10 — the five
checksum bits, from `encodeFiveBit`, land at matrix indices 42, 58, 74, 90 and
106, most significant first. The result is read out **down the columns**:
output index `a` takes matrix index `(16a) mod 127`, with index 127 mapping to
itself.

**Hamming parity equations**, from `MMDVMHost/Hamming.cpp`. These are the codes'
definitions, not an implementation:

* `(15,11,3)`: `d11 = d0^d1^d2^d3^d5^d7^d8`, `d12 = d1^d2^d3^d4^d6^d8^d9`,
  `d13 = d2^d3^d4^d5^d7^d9^d10`, `d14 = d0^d1^d2^d4^d6^d7^d10`.
* `(16,11,4)`: the same four, plus `d15 = d0^d2^d5^d6^d8^d9^d10`.
* `(13,9,3)`: `d9 = d0^d1^d3^d5^d6`, `d10 = d0^d1^d2^d4^d6^d7`,
  `d11 = d0^d1^d2^d3^d5^d7^d8`, `d12 = d0^d2^d4^d5^d8`.

**Golay(20,8)** — the slot type. This is the **(24,12) extended Golay code
shortened by four**: generator polynomial
`x¹¹ + x⁹ + x⁷ + x⁶ + x⁵ + x + 1` (0xC75), systematic, with a twelfth parity
bit that is the overall even parity of the nineteen bits before it. Verified by
hand against `MMDVMHost/Golay2087.cpp`'s `ENCODING_TABLE_2087` at indices 1, 2
and 48: reproducing `(m · x¹¹) mod g` plus the overall parity bit gives that
table's entries exactly, laid out as data byte, then the remainder's low byte,
then the remainder's high nibble.

**QR(16,7,6)** — the EMB. The cyclic `(15,7)` code with generator polynomial
`x⁸ + x⁵ + x⁴ + x³ + 1` (0x139), systematic, extended by an overall even-parity
bit to `(16,7)`. Verified by hand against `MMDVMHost/QR1676.cpp`'s
`ENCODING_TABLE_1676` at indices 1, 2 and 48. The seven data bits sit in
`EMB[0]` bits 7..1; the eight remainder bits occupy `EMB[0]` bit 0 and
`EMB[1]` bits 7..1; the overall parity bit is `EMB[1]` bit 0.
`DroidStar dmr.cpp: encode_qr1676` uses the identical table and layout.

**RS(12,9)** — the full LC parity. `MMDVMHost/RS129.cpp`. GF(2⁸) with primitive
polynomial `x⁸ + x⁴ + x³ + x² + 1` (0x11D, confirmed by the exponent table's
`0x80 → 0x1D` step), α = 2, three parity symbols, generator
`g(x) = (x + α)(x + α²)(x + α³) = x³ + 14x² + 56x + 64` — the file states those
four coefficients directly as its `POLY`. The parity symbols are appended to
the LC in **reverse** order, as §7 shows.

**AMBE+2 channel FEC.** `MMDVMHost/AMBEFEC.cpp: regenerateDMR` counts 141
protected bits per burst (three frames × 47) and rebuilds them with
Golay(24,12) on the `a` bits, a PRNG-whitened Golay(23,12) on the `b` bits, and
no protection on the 25 `c` bits. **astar does not need any of it**: the
AMBE-3000 in DMR mode emits and consumes the FEC-complete 72-bit frame — see
§9 — and `regenerateDMR` is a repeater's soft-decision cleanup, not a
requirement of the wire.

---

## 9. The vocoder is not YSF's

**DMR is AMBE+2 at 2450 × 1150, nine bytes per frame** — a different RATEP
configuration from YSF DN and NXDN, which astar already has.
`DroidStar serialambe.cpp: SerialAMBE::config_ambe`:

| protocol | RATEP constant | `packet_size` |
|---|---|---|
| DMR | `AMBE3000_2450_1150` | **9** |
| YSF, NXDN | `AMBE3000_2450_0000` | 7 |
| P25 | `AMBEP251_4400_2800` | — |

`AMBE3000_2450_1150` is the 17-byte control packet
`61 00 0D 00 0A 04 31 07 54 24 00 00 00 00 00 6F 48` — the same
`0x04 0x31 0x07 0x54` RATEP prefix astar's `astar_codec::ysf::ratep_dn()`
already emits, with `0x24` where DN has `0x00` and a different trailing
checksum. The channel packet that carries a frame to the dongle is
`61 00 0B 01 01 48` followed by the nine bytes (`SerialAMBE::decode_3000`,
which switches to the `09`/`0x31` header only when `packet_size == 7`), and
`SerialAMBE::get_ambe` reads back `6 + packet_size` bytes with
`AMBE3000_TYPE_CHANNEL` in byte 3.

Three 9-byte frames per 60 ms burst is 20 ms of audio each — the same 160-sample
PCM block `DMR::transmit` feeds the encoder.

---

## 10. Not verified, and disagreements

**The `RPTC` slots byte.** `DroidStar` sends the literal `'4'` — it is a
constant in the sprintf argument list, not a variable. No reference in this
pass says what the values mean; `hblink3` stores `SLOTS` as opaque text
(`_data[97:98]`) and never reads it again, so it is very likely inert for a
hotspot-class connection. **Send `'4'` because the only working softclient
does**; do not build behaviour on it.

**`RPTC`'s description/slots split — settled more firmly than the brief
claimed.** The brief ruled for DroidStar's 19 + 1 over `research-dmr.md`'s
20 + 0 on the grounds that DroidStar is "the only source that is a working
softclient against these masters". That reasoning is weaker than the evidence:
`hblink3`'s master parses description as `_data[78:97]` and slots as
`_data[97:98]`, which is a **second independent implementation** of the same
split, on the receiving side. It is 2–1, not a judgement call. Implement
19 + 1.

**The timeslot default.** `DroidStar dmr.cpp` constructs with `m_txslot(2)`,
and `process_modem_data` forces `m_txslot = m_modeinfo.slot = 2` on the modem
path. Hotspot-class connections conventionally carry network traffic on TS2.
That is **a convention, not a specification** — nothing in any reference read
here requires it. Default to TS2, and make it a user-visible field rather than
a constant.

**The stream id's byte order.** There is none. `DMRGateway` `memcpy`s a
host-order `uint32_t` in both directions. `hblink3` never interprets it at all
— it is a raw slice compared for equality. `DroidStar` `memcpy`s a host-order
`uint32_t` on transmit (`build_frame`) but reads it back big-endian for its
status display; that read is cosmetic and feeds nothing on the wire, so it does
not establish an ordering. **Note the brief's wording is slightly wrong here**
— it says "a raw host-order `memcpy` in both directions" of DroidStar too. The
conclusion is unchanged and is what matters: treat the stream id as four opaque
bytes, compare it only for equality, and never print it as a number and claim
the number means anything.

**BER and RSSI on receive.** Present at offsets 53 and 54 in everything G4KLX
writes, and zero in everything `DroidStar` sends (`build_frame` sets both to
`0`). astar reads them for a future signal display and **does not act on them**
in this plan — the receive path is BER-free by construction
(`dmr-networks.md`'s receive-first split).

**`DMRGateway` accepts a `DMRD` at 71 bytes as well as 55.** The brief says
`clock()` accepts a `DMRD` "only at that length". Current `master` accepts
either `HOMEBREW_DATA_PACKET_LENGTH` (55) or
`HOMEBREW_TRUNKING_DATA_PACKET_LENGTH` (71, documented in the file as "DMRD
with 16 byte UUID extension") when trunking is enabled, and the whole `DMRT` /
`DTC*` command family alongside it. That is YO8RZZ's 2025–2026 trunking work,
not the homebrew protocol TGIF speaks. **astar sends 55, accepts 53 or 55, and
ignores `DMRT`/`DTC*` entirely.** Nothing in the plan needs trunking, and
implementing half of it would be worse than none.

**`hblink3`'s call-type line is a three-way, and the brief quotes the version
that is commented out.** The brief cites
`_call_type = 'unit' if (_bits & 0x40) else 'group'`. That line exists in
`hblink.py` in both the master and peer branches, but it is commented out; the
live code is `unit` if `_bits & 0x40`, else `vcsbk` if `(_bits & 0x23) == 0x23`
(a data-sync burst whose data type is `DT_CSBK`), else `group`. The `0x40`
polarity is untouched and the §4 ruling stands; astar has no CSBK path and
needs no third case yet.

**`MMDVMHost` has no `DMRSync.cpp`.** The brief lists one under *Consumes*.
The DMR sync writers are `CSync::addDMRDataSync` and `CSync::addDMRAudioSync`
in the shared `Sync.cpp`; the constants are in `DMRDefines.h`. Both were
fetched and read, and the values in §5 are unaffected.

**Two ping cadences exist in the references.** `DroidStar` pings every 5 s;
`DMRGateway` has no ping timer at all and reuses its 10 s retry timer, so
G4KLX's gateway pings at 10 s. `hblink3` takes its `PING_TIME` from
configuration and drops a peer after `PING_TIME × MAX_MISSED` of silence, so a
master's tolerance is a deployment choice, not a protocol constant. 5 s is
inside every window seen here; it is what astar uses, and 60 s remains the
right link-timeout.

**`RPTACK` answers three requests, not two.** The brief's handshake table calls
the 10-byte `RPTACK` + id "the reply to `RPTK` and to `RPTC`". `hblink.py`'s
master also answers `RPTO` with `RPTACK + _peer_id`, and
`CDMRNetwork::clock`'s `WAITING_OPTIONS` state expects exactly that. Recorded
in §1's table. Not load-bearing for astar, which sends no `RPTO`.

**DroidStar's `RPTC` power and colour code are sprintf literals.** The brief
cites `m_txcc(1)` (`dmr.cpp:44`) for the colour-code default, which is real,
but the `RPTC` sprintf passes a literal `1` for tx power, a literal `1` for
colour code and a literal `0` for height — it never reads `m_txcc`. Both routes
give 1, so §3's value is unchanged; the citation is kept as `m_txcc(1)` because
that is the field a later task will actually wire up.

**Do not copy DroidStar's colour-code handling.** `DroidStar dmr.cpp` reads
`m_modeinfo.cc` when building both the EMB (`get_emb_data`) and the slot type
(`get_slot_data`), but `m_modeinfo.cc` is never assigned anywhere in the DMR
path — `MODEINFO` in `mode.h` is a plain struct with no initialiser, and the
user's colour code lands in `m_txcc` via `cc_changed`, which is what the
`RPTC` field uses. The colour code DroidStar stamps into the burst is therefore
not the one it declares at login. astar must use **one** colour code
everywhere: `RPTC` offset 36, the EMB, and the slot type.

**Not read, and therefore not settled here.** The DMR data path
(`DT_DATA_HEADER`, the rate 1/2, 3/4 and 1 data types, `DMRTrellis`,
`DMRDataHeader`), CSBK (`DMRCSBK`), talker alias assembly (`DMRTA`), short LC
(`DMRShortLC`), and the reverse channel MMDVMHost builds in
`CDMRSlot::createReverseChannel`. astar's plan is voice only; anything that
needs those must read them first rather than extrapolating from this note.

**The master's own timeouts are not a constant.** `hblink3`'s master removes a
peer at `LAST_PING + PING_TIME × MAX_MISSED`, both from its config file. TGIF's
actual values are not knowable from these sources; astar should not assume more
than "keep pinging".
