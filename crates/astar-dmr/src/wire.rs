// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The MMDVM/homebrew datagrams: what goes into a UDP datagram to a master
//! and what comes back out.
//!
//! Specified byte by byte in `docs/design/dmr-wire.md` §1, §2 and §4, which
//! is where every constant here was read out of — each one citing the file
//! and function it came from.
//!
//! | tag | bytes | direction | meaning |
//! |---|---|---|---|
//! | `RPTL` | 8 | to master | login: "this is my radio id" |
//! | `RPTACK` | 10 | from master | to `RPTL` it carries a salt; to `RPTK` and `RPTC` it echoes the id |
//! | `RPTK` | 40 | to master | the login digest |
//! | `RPTC` | 302 | to master | the config, §2's fixed-width ASCII table |
//! | `RPTPING` | 11 | to master | keepalive — the CLIENT pings |
//! | `MSTPONG` | 11 | from master | the answer to a ping |
//! | `MSTNAK` | 10 | from master | rejection, at whichever stage it arrives |
//! | `MSTCL` | 9 | from master | the master is going away |
//! | `RPTCL` | 9 | to master | we are going away |
//! | `DMRD` | 55 (53 accepted) | both | one 33-byte DMR burst with its routing |
//!
//! # Lengths, and the one place this is deliberately lenient
//!
//! astar **sends** a `DMRD` at 55 bytes, which is what every deployed client
//! and gateway sends, and **accepts** 53 or 55: `hblink3` slices a `DMRD` by
//! offset and never checks its length at all, so a master built on it will
//! relay whatever it was given, including a frame that lost G4KLX's two
//! trailing bytes somewhere upstream. Refusing that would silence a whole
//! class of master over a BER/RSSI pair astar does not act on.
//!
//! Everything else is exact. In particular the 71-byte trunking `DMRD` and
//! the `DMRT`/`DTC*` command family that current `DMRGateway` also speaks are
//! **not** parsed — they are YO8RZZ's trunking work, not the homebrew
//! protocol a TGIF-class master speaks, and half an implementation of them
//! would be worse than none (`docs/design/dmr-wire.md` §10).
//!
//! # The burst is carried, not decoded
//!
//! [`DataPacket::burst`] is 33 bytes handed over intact. What is inside it —
//! the sync patterns, the embedded LC, the three AMBE+2 frames — is
//! `docs/design/dmr-wire.md` §5–§8's business and a later crate's.

use sha2::{Digest, Sha256};

/// Bytes in an `RPTL` login: tag and id.
pub const LOGIN_LEN: usize = 8;
/// Bytes in an `RPTACK`: tag, then a salt or an echoed id.
pub const ACK_LEN: usize = 10;
/// Bytes in an `RPTK` authorisation: tag, id, and a 32-byte digest.
pub const AUTH_LEN: usize = 40;
/// Bytes in an `RPTC` config datagram.
pub const CONFIG_LEN: usize = 302;
/// Bytes in an `RPTPING`.
pub const PING_LEN: usize = 11;
/// Bytes in an `MSTPONG`.
pub const PONG_LEN: usize = 11;
/// Bytes in an `MSTNAK`.
pub const NAK_LEN: usize = 10;
/// Bytes in an `MSTCL` or an `RPTCL`.
pub const CLOSE_LEN: usize = 9;
/// Bytes in the `DMRD` astar sends.
///
/// `DMRGateway/DMRNetwork.cpp`: `HOMEBREW_DATA_PACKET_LENGTH = 55U`.
pub const DATA_LEN: usize = 55;
/// The shortest `DMRD` astar will read. HBlink3-derived masters slice by
/// offset and never check the length, so a relayed frame can arrive without
/// G4KLX's two trailing bytes.
pub const DATA_LEN_MIN: usize = 53;
/// Bytes in one DMR burst — 264 bits.
///
/// `MMDVMHost/DMRDefines.h`: `DMR_FRAME_LENGTH_BITS = 264U`;
/// `hblink3/playback.py` slices it as `_data[20:53]`. Defined here once: the
/// frame layer reads `crate::wire::BURST_LEN` rather than declaring its own.
pub const BURST_LEN: usize = 33;
/// Bytes of `RPTC` after its tag and id — the fixed-width ASCII table.
pub const CONFIG_BODY_LEN: usize = CONFIG_LEN - 8;

const TAG_LOGIN: &[u8] = b"RPTL";
const TAG_AUTH: &[u8] = b"RPTK";
const TAG_CONFIG: &[u8] = b"RPTC";
const TAG_PING: &[u8] = b"RPTPING";
const TAG_CLIENT_CLOSE: &[u8] = b"RPTCL";
const TAG_ACK: &[u8] = b"RPTACK";
const TAG_NAK: &[u8] = b"MSTNAK";
const TAG_PONG: &[u8] = b"MSTPONG";
const TAG_MASTER_CLOSE: &[u8] = b"MSTCL";
const TAG_DATA: &[u8] = b"DMRD";

/// `DMRD` bits byte, offset 15. DMRGateway/DMRNetwork.cpp:
/// `CDMRNetwork::write` sets them and `read` takes them back out;
/// hblink.py's master branch reads the same masks.
const BIT_SLOT2: u8 = 0x80;
const BIT_PRIVATE: u8 = 0x40;
const BIT_FRAME_TYPE: u8 = 0x30;
const BIT_VOICE_SYNC: u8 = 0x10;
const BIT_DATA_SYNC: u8 = 0x20;
const BIT_LOW_NIBBLE: u8 = 0x0F;

/// The largest value a `DMRD` source or destination id can hold: 24 bits.
///
/// Public because it bounds more than a radio ID: a talkgroup is a `DMRD`
/// *destination* id and has the same 24 bits behind it, so `Station` and
/// `astar-cli` check theirs against this rather than restating the literal.
pub const RADIO_ID_MAX: u32 = 0x00FF_FFFF;

/// A radio ID as it goes on the wire: the low 24 bits address a station in a
/// `DMRD` and all 32 identify the peer in the handshake.
///
/// DMR does not put a callsign on the air. This number, registered at
/// radioid.net against a verified licence, is the whole of a station's
/// identity — see [`crate::network`] and `docs/design/dmr-networks.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioId(u32);

/// Why a number could not be used as a radio ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioIdError {
    /// Zero. What an unset field looks like, not a registration.
    Zero,
    /// Past 24 bits, so it cannot be a `DMRD` source or destination id.
    TooLarge,
}

impl std::fmt::Display for RadioIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero => write!(f, "a radio ID of 0 is an unset field, not a registration"),
            Self::TooLarge => write!(f, "a radio ID is at most {RADIO_ID_MAX} — 24 bits"),
        }
    }
}

impl std::error::Error for RadioIdError {}

impl RadioId {
    /// Refuses 0 and anything past 24 bits. Zero is what an unset field looks
    /// like; a value past 16,777,215 cannot be a `DMRD` source id.
    ///
    /// # Errors
    /// [`RadioIdError::Zero`] or [`RadioIdError::TooLarge`].
    pub const fn new(id: u32) -> Result<RadioId, RadioIdError> {
        if id == 0 {
            return Err(RadioIdError::Zero);
        }
        if id > RADIO_ID_MAX {
            return Err(RadioIdError::TooLarge);
        }
        Ok(RadioId(id))
    }

    /// The id as a number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The four bytes the handshake carries: `RPTL`, `RPTK`, `RPTC`,
    /// `RPTPING`, `RPTCL`, and `DMRD`'s peer field.
    #[must_use]
    pub const fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    /// The three bytes a `DMRD` source or destination field carries.
    #[must_use]
    pub const fn to_be_u24(self) -> [u8; 3] {
        let [_, b1, b2, b3] = self.0.to_be_bytes();
        [b1, b2, b3]
    }
}

/// Which of DMR's two TDMA timeslots a frame is on.
///
/// TS2 is the hotspot convention and astar's default, but it is a
/// *convention* — `docs/design/dmr-wire.md` §10 — so it is carried as a field
/// and never baked into the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeslot {
    /// Timeslot 1. Bit 7 of the bits byte clear.
    Ts1,
    /// Timeslot 2. Bit 7 set.
    Ts2,
}

impl Timeslot {
    /// A stable lowercase name — an ABI string, not a debug convenience.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Timeslot::Ts1 => "ts1",
            Timeslot::Ts2 => "ts2",
        }
    }

    /// This slot as its bit in the `DMRD` bits byte.
    ///
    /// `buffer[15U] = slotNo == 1U ? 0x00U : 0x80U`
    /// (`DMRGateway/DMRNetwork.cpp: CDMRNetwork::write`).
    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Timeslot::Ts1 => 0x00,
            Timeslot::Ts2 => BIT_SLOT2,
        }
    }

    /// The slot a bits byte names.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Timeslot {
        if bits & BIT_SLOT2 == 0 {
            Timeslot::Ts1
        } else {
            Timeslot::Ts2
        }
    }
}

/// Group call or unit-to-unit.
///
/// Bit 6 clear is a **group** call and set is a **private** one — the
/// polarity `docs/design/dmr-wire.md` §4 settles against two independent
/// implementations, because getting it backwards turns every group call into
/// a silently misrouted private one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallType {
    /// A talkgroup call. `dst_id` is the talkgroup.
    Group,
    /// A unit-to-unit call. `dst_id` is the called station's radio id.
    Private,
}

/// What kind of burst a `DMRD` carries, from bits 5–4 and 3–0 of the bits
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Bits 5-4 == 00. `n` is the voice sequence, burst A..F as 0..5.
    Voice {
        /// Burst A..F as 0..5.
        n: u8,
    },
    /// Bits 5-4 == 01. Burst A, carrying the audio sync pattern.
    VoiceSync,
    /// Bits 5-4 == 10. `data_type` is a `DT_*` code point.
    DataSync {
        /// One of `MMDVMHost/DMRDefines.h`'s `DT_*` values, masked to 4 bits.
        data_type: u8,
    },
    /// Bits 5-4 == 11. Reserved; astar carries it rather than guessing.
    Reserved {
        /// Bits 3-0, whatever they turn out to mean.
        low: u8,
    },
}

impl FrameType {
    /// This frame type as bits 5–0 of the bits byte.
    ///
    /// `DMRGateway/DMRNetwork.cpp: CDMRNetwork::write`:
    /// `if (dataType == DT_VOICE_SYNC) buffer[15U] |= 0x10U; else if (dataType
    /// == DT_VOICE) buffer[15U] |= data.getN(); else buffer[15U] |= (0x20U |
    /// dataType);`
    const fn bits(self) -> u8 {
        match self {
            FrameType::Voice { n } => n & BIT_LOW_NIBBLE,
            FrameType::VoiceSync => BIT_VOICE_SYNC,
            FrameType::DataSync { data_type } => BIT_DATA_SYNC | (data_type & BIT_LOW_NIBBLE),
            FrameType::Reserved { low } => BIT_FRAME_TYPE | (low & BIT_LOW_NIBBLE),
        }
    }

    /// The frame type a bits byte names.
    ///
    /// `hblink.py`: `_frame_type = (_bits & 0x30) >> 4`, `_dtype_vseq =
    /// (_bits & 0xF)`, with `const.py` naming `HBPF_VOICE = 0x0`,
    /// `HBPF_VOICE_SYNC = 0x1`, `HBPF_DATA_SYNC = 0x2`.
    const fn from_bits(bits: u8) -> FrameType {
        let low = bits & BIT_LOW_NIBBLE;
        match bits & BIT_FRAME_TYPE {
            0x00 => FrameType::Voice { n: low },
            BIT_VOICE_SYNC => FrameType::VoiceSync,
            BIT_DATA_SYNC => FrameType::DataSync { data_type: low },
            _ => FrameType::Reserved { low },
        }
    }
}

/// A `DMRD`: one DMR burst with everything the network needs to route it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPacket {
    /// Sequence number, wrapping. `hblink.py`: `_seq = _data[4]`.
    pub seq: u8,
    /// Who is talking, 24 bits.
    pub src_id: u32,
    /// Talkgroup for a group call, called station for a private one.
    pub dst_id: u32,
    /// The repeater/peer this came through, 32 bits.
    pub peer_id: u32,
    /// Which timeslot.
    pub slot: Timeslot,
    /// Group or private.
    pub call_type: CallType,
    /// What the burst is.
    pub frame_type: FrameType,
    /// Four opaque bytes. Equality is the only operation this has.
    pub stream_id: [u8; 4],
    /// The 33-byte burst, carried and not decoded.
    pub burst: [u8; BURST_LEN],
    /// Bit error rate, as the sender measured it. Zero from a softclient.
    pub ber: u8,
    /// Received signal strength, as the sender measured it. Zero from a
    /// softclient.
    pub rssi: u8,
}

/// What `RPTC` declares. Everything is ASCII and space-padded on the wire;
/// this holds the meanings, and [`config`] renders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFields {
    /// The operator's callsign. Eight bytes on the wire.
    pub callsign: String,
    /// Receive frequency in Hz. Nine digits.
    pub rx_freq_hz: u32,
    /// Transmit frequency in Hz. Nine digits.
    pub tx_freq_hz: u32,
    /// Transmit power in dBm. Two digits.
    pub tx_power_dbm: u8,
    /// DMR colour code. Two digits.
    pub colour_code: u8,
    /// Latitude, already formatted. Eight bytes.
    pub latitude: String,
    /// Longitude, already formatted. Nine bytes.
    pub longitude: String,
    /// Height above ground in metres. Three digits.
    pub height_m: u16,
    /// Free text location. Twenty bytes.
    pub location: String,
    /// Free text description. Nineteen bytes — the 19 + 1 split
    /// `docs/design/dmr-wire.md` §2 settles two implementations to one.
    pub description: String,
    /// The slots byte, one character, sent as-is.
    pub slots: u8,
    /// A URL. 124 bytes.
    pub url: String,
    /// Software identification. Forty bytes.
    pub software_id: String,
    /// Package identification. Forty bytes.
    pub package_id: String,
}

impl ConfigFields {
    /// The values a softclient with no radio and no site tells the truth
    /// with. See `docs/design/dmr-wire.md` §3.
    ///
    /// | field | value | reason |
    /// |---|---|---|
    /// | callsign | the operator's | it is the only true thing here |
    /// | rx / tx freq | `000000000` | there is no radio; zero is honest, a plausible-looking number is not |
    /// | tx power | `00` | ditto |
    /// | colour code | `01` | `DroidStar dmr.cpp`'s `m_txcc(1)` default; a network-only connection has no RF colour code to clash with |
    /// | latitude / longitude | `0.000000` / `00.000000` | `DMRGateway/DMRNetwork.cpp: writeConfig`'s own no-location placeholder, used verbatim — and CLAUDE.md keeps personal addresses out of astar |
    /// | height | `000` | there is no site |
    /// | location | empty | ditto |
    /// | description | `astar softclient` | honest self-identification |
    /// | slots | `'4'` | `DroidStar dmr.cpp`'s literal. Semantics unverified; nothing is built on it |
    /// | URL | empty | |
    /// | software id | the caller's | astar and its version; never a claim to be approved firmware |
    /// | package id | `astar` | |
    #[must_use]
    pub fn softclient(callsign: &str, software_id: &str) -> ConfigFields {
        ConfigFields {
            callsign: callsign.to_string(),
            rx_freq_hz: 0,
            tx_freq_hz: 0,
            tx_power_dbm: 0,
            colour_code: 1,
            latitude: "0.000000".to_string(),
            longitude: "00.000000".to_string(),
            height_m: 0,
            location: String::new(),
            description: "astar softclient".to_string(),
            slots: b'4',
            url: String::new(),
            software_id: software_id.to_string(),
            package_id: "astar".to_string(),
        }
    }
}

/// One received datagram, as far as a client cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// `RPTACK`. The four bytes after the tag are reported as **both** a salt
    /// and an id, because on the wire they are the same four bytes and only
    /// the state machine knows which stage it is at.
    Ack {
        /// The four bytes read as a big-endian id.
        id: u32,
        /// The same four bytes read as a salt. `None` never comes out of
        /// [`parse`] — an `RPTACK` too short to carry them is not parsed as
        /// an ack at all — but the shape stays optional so a caller that
        /// synthesises one cannot pretend to a salt it does not have.
        salt: Option<[u8; 4]>,
    },
    /// `MSTNAK`. Which stage it arrived at is the whole diagnosis.
    Nak {
        /// The id the master echoed.
        id: u32,
    },
    /// `MSTPONG`, the answer to our ping.
    Pong {
        /// The id the master echoed.
        id: u32,
    },
    /// `MSTCL`: the master is going away.
    MasterClosing {
        /// The id the master echoed.
        id: u32,
    },
    /// A `DMRD`.
    Data(Box<DataPacket>),
    /// A datagram this build has no reading for — `RPTSBKN`, an `RPTO`
    /// echo, a trunking command. Kept whole so a caller can say what it saw
    /// rather than reporting that nothing arrived.
    Unknown(Vec<u8>),
}

/// Parses a received datagram.
///
/// Returns `None` only for something that is not a datagram this protocol
/// has: too short to carry a tag, or a known tag at a length that makes it
/// unreadable. Anything else comes back as [`Packet::Unknown`].
#[must_use]
pub fn parse(datagram: &[u8]) -> Option<Packet> {
    if datagram.len() < 4 {
        return None;
    }
    let len = datagram.len();
    if datagram.starts_with(TAG_DATA) {
        return parse_data(datagram);
    }
    if datagram.starts_with(TAG_ACK) && len >= ACK_LEN {
        // The four bytes are reported as both a salt and an id; only the
        // state machine knows which this one is. A shorter RPTACK carries
        // neither, so it is not an acknowledgement of anything and must not
        // be allowed to advance a handshake -- it falls through to
        // `Unknown` rather than parsing with an invented id.
        let salt = four(datagram, 6)?;
        return Some(Packet::Ack {
            id: u32::from_be_bytes(salt),
            salt: Some(salt),
        });
    }
    if datagram.starts_with(TAG_NAK) && len >= NAK_LEN {
        return Some(Packet::Nak {
            id: u32::from_be_bytes(four(datagram, 6)?),
        });
    }
    if datagram.starts_with(TAG_PONG) && len >= PONG_LEN {
        return Some(Packet::Pong {
            id: u32::from_be_bytes(four(datagram, 7)?),
        });
    }
    // Checked after RPTPING/MSTPONG so a longer tag is never read as this
    // one; `MSTCL` is a five-byte prefix of nothing else the master sends.
    if datagram.starts_with(TAG_MASTER_CLOSE) && len >= CLOSE_LEN {
        return Some(Packet::MasterClosing {
            id: u32::from_be_bytes(four(datagram, 5)?),
        });
    }
    Some(Packet::Unknown(datagram.to_vec()))
}

/// The four bytes at `at`, if the datagram is long enough to have them.
fn four(datagram: &[u8], at: usize) -> Option<[u8; 4]> {
    datagram.get(at..at + 4)?.try_into().ok()
}

fn parse_data(datagram: &[u8]) -> Option<Packet> {
    // 53 or 55 and nothing else. In particular not 71: the trunking form is
    // ignored, never guessed at.
    if datagram.len() != DATA_LEN && datagram.len() != DATA_LEN_MIN {
        return None;
    }
    let bits = datagram[15];
    let mut burst = [0u8; BURST_LEN];
    burst.copy_from_slice(&datagram[20..20 + BURST_LEN]);
    Some(Packet::Data(Box::new(DataPacket {
        seq: datagram[4],
        src_id: u24(&datagram[5..8]),
        dst_id: u24(&datagram[8..11]),
        peer_id: u32::from_be_bytes(four(datagram, 11)?),
        slot: Timeslot::from_bits(bits),
        call_type: if bits & BIT_PRIVATE == 0 {
            CallType::Group
        } else {
            CallType::Private
        },
        frame_type: FrameType::from_bits(bits),
        stream_id: four(datagram, 16)?,
        burst,
        // Absent on a 53-byte relay. Zero is what DroidStar sends anyway
        // (`build_frame` sets both to 0), so it is not a fabricated value.
        ber: datagram.get(53).copied().unwrap_or(0),
        rssi: datagram.get(54).copied().unwrap_or(0),
    })))
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
}

/// Builds an `RPTL` login.
///
/// `DMRGateway/DMRNetwork.cpp: CDMRNetwork::writeLogin`.
#[must_use]
pub fn login(id: RadioId) -> [u8; LOGIN_LEN] {
    let mut out = [0u8; LOGIN_LEN];
    out[..4].copy_from_slice(TAG_LOGIN);
    out[4..].copy_from_slice(&id.to_be_bytes());
    out
}

/// Builds an `RPTK`: the tag, the id, and the digest raw — not hex.
///
/// `CDMRNetwork::writeAuthorisation`; `hblink.py`'s peer sends
/// `RPTK + RADIO_ID + digest`.
#[must_use]
pub fn auth(id: RadioId, salt: [u8; 4], password: &str) -> [u8; AUTH_LEN] {
    let mut out = [0u8; AUTH_LEN];
    out[..4].copy_from_slice(TAG_AUTH);
    out[4..8].copy_from_slice(&id.to_be_bytes());
    out[8..].copy_from_slice(&auth_digest(salt, password));
    out
}

/// The digest `RPTK` carries, exposed so the master fixture can recompute it.
///
/// `SHA256(salt_bytes ‖ password_bytes)`, the salt being the four bytes **as
/// received** — never round-tripped through an integer, which would work by
/// accident on a big-endian host and silently fail everywhere else.
#[must_use]
pub fn auth_digest(salt: [u8; 4], password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    hasher.finalize().into()
}

/// Builds an `RPTC`: 302 bytes, every field fixed-width ASCII.
///
/// Over-long values are **truncated**, never allowed to shift what follows:
/// a field that pushed the rest along would put the master's offset-based
/// parse out for the whole remainder of the packet.
#[must_use]
pub fn config(id: RadioId, fields: &ConfigFields) -> [u8; CONFIG_LEN] {
    let mut out = [b' '; CONFIG_LEN];
    out[..4].copy_from_slice(TAG_CONFIG);
    out[4..8].copy_from_slice(&id.to_be_bytes());
    text(&mut out[8..16], &fields.callsign);
    number(&mut out[16..25], u64::from(fields.rx_freq_hz));
    number(&mut out[25..34], u64::from(fields.tx_freq_hz));
    number(&mut out[34..36], u64::from(fields.tx_power_dbm));
    number(&mut out[36..38], u64::from(fields.colour_code));
    text(&mut out[38..46], &fields.latitude);
    text(&mut out[46..55], &fields.longitude);
    number(&mut out[55..58], u64::from(fields.height_m));
    text(&mut out[58..78], &fields.location);
    text(&mut out[78..97], &fields.description);
    // One character, sent as-is. A non-printable byte here would be a
    // corrupt config rather than a choice, so it falls back to the literal
    // DroidStar sends; nothing is built on what the value means
    // (`docs/design/dmr-wire.md` §10).
    out[97] = if fields.slots.is_ascii_graphic() {
        fields.slots
    } else {
        b'4'
    };
    text(&mut out[98..222], &fields.url);
    text(&mut out[222..262], &fields.software_id);
    text(&mut out[262..302], &fields.package_id);
    out
}

/// Writes `value` left-aligned into a fixed-width, space-padded ASCII field,
/// truncating rather than shifting, and replacing anything that is not
/// printable ASCII with `?`.
///
/// The replacement is not fussiness: this text is read by other people's
/// dashboards, and a control byte in it is either a bug or an attempt to make
/// a master's log say something it should not.
fn text(field: &mut [u8], value: &str) {
    field.fill(b' ');
    for (slot, b) in field.iter_mut().zip(value.bytes()) {
        *slot = if b == b' ' || b.is_ascii_graphic() {
            b
        } else {
            b'?'
        };
    }
}

/// Writes `value` zero-padded into a fixed-width numeric field.
///
/// A value too big for the field is clamped to that field's largest, for the
/// same reason [`text`] truncates: the alternative is a packet every master
/// misreads from that offset on. A frequency or a height that does not fit
/// nine or three digits is not a value astar can send honestly anyway.
fn number(field: &mut [u8], value: u64) {
    let rendered = format!("{value:0width$}", width = field.len());
    if rendered.len() > field.len() {
        field.fill(b'9');
        return;
    }
    field.copy_from_slice(rendered.as_bytes());
}

/// Builds an `RPTPING`. The **client** pings; `docs/design/dmr-wire.md` §1
/// settles the direction against three implementations.
#[must_use]
pub fn ping(id: RadioId) -> [u8; PING_LEN] {
    let mut out = [0u8; PING_LEN];
    out[..7].copy_from_slice(TAG_PING);
    out[7..].copy_from_slice(&id.to_be_bytes());
    out
}

/// Builds an `RPTCL`: we are going away.
///
/// `CDMRNetwork::close(true)`. Expect an `MSTNAK` back — `hblink.py`'s master
/// deletes the peer and NAKs its goodbye — and do not read that as an error.
#[must_use]
pub fn close(id: RadioId) -> [u8; CLOSE_LEN] {
    let mut out = [0u8; CLOSE_LEN];
    out[..5].copy_from_slice(TAG_CLIENT_CLOSE);
    out[5..].copy_from_slice(&id.to_be_bytes());
    out
}

/// Why a [`DataPacket`] could not be written to the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataError {
    /// `src_id` is past the 24 bits its field holds.
    SourceTooLarge,
    /// `dst_id` is past the 24 bits its field holds.
    DestinationTooLarge,
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceTooLarge => write!(f, "source id is past {RADIO_ID_MAX} — 24 bits"),
            Self::DestinationTooLarge => {
                write!(f, "destination id is past {RADIO_ID_MAX} — 24 bits")
            }
        }
    }
}

impl std::error::Error for DataError {}

/// Builds a `DMRD` at the full 55 bytes.
///
/// # Errors
/// [`DataError`] if either id is past the 24 bits its field holds. Truncating
/// silently — which is what writing the low three bytes and saying nothing
/// would do — would send a frame **addressed to somebody else**: a
/// destination of `0x0100_0059` would go out as TG 89.
pub fn data(packet: &DataPacket) -> Result<[u8; DATA_LEN], DataError> {
    if packet.src_id > RADIO_ID_MAX {
        return Err(DataError::SourceTooLarge);
    }
    if packet.dst_id > RADIO_ID_MAX {
        return Err(DataError::DestinationTooLarge);
    }
    let mut out = [0u8; DATA_LEN];
    out[..4].copy_from_slice(TAG_DATA);
    out[4] = packet.seq;
    out[5..8].copy_from_slice(&packet.src_id.to_be_bytes()[1..]);
    out[8..11].copy_from_slice(&packet.dst_id.to_be_bytes()[1..]);
    out[11..15].copy_from_slice(&packet.peer_id.to_be_bytes());
    out[15] = packet.slot.bit()
        | match packet.call_type {
            CallType::Group => 0x00,
            CallType::Private => BIT_PRIVATE,
        }
        | packet.frame_type.bits();
    out[16..20].copy_from_slice(&packet.stream_id);
    out[20..53].copy_from_slice(&packet.burst);
    out[53] = packet.ber;
    out[54] = packet.rssi;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> RadioId {
        RadioId::new(3_153_591).expect("a plausible seven-digit id")
    }

    #[test]
    fn a_login_is_the_tag_and_the_id() {
        // DMRGateway/DMRNetwork.cpp: CDMRNetwork::writeLogin -- "RPTL" then
        // four big-endian id bytes, eight in total.
        let bytes = login(id());
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[..4], b"RPTL");
        assert_eq!(
            u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            3_153_591
        );
    }

    #[test]
    fn a_zero_or_over_wide_radio_id_is_refused() {
        // A DMRD source id is 24 bits (CDMRNetwork::write: `srcId >> 16`
        // into three bytes), and 0 is what an unset field looks like -- not
        // a registration. Refusing both here means no later layer has to.
        assert_eq!(RadioId::new(0), Err(RadioIdError::Zero));
        assert_eq!(RadioId::new(0x0100_0000), Err(RadioIdError::TooLarge));
        assert!(RadioId::new(0x00FF_FFFF).is_ok());
    }

    #[test]
    fn the_auth_digest_is_sha256_of_the_salt_then_the_password() {
        // DMRGateway/DMRNetwork.cpp: writeAuthorisation builds
        // `in = m_salt(4) || m_password` and hashes it; hblink.py computes
        // `sha256(_salt_str + PASSPHRASE)`. The salt is the four bytes AS
        // RECEIVED -- never re-encoded through an integer.
        let salt = [0x0A, 0x7E, 0xD4, 0x98];
        let digest = auth_digest(salt, "DL5DI");
        // Recomputed, not recalled:
        //   python3 -c 'import hashlib; print(hashlib.sha256(
        //       bytes([0x0A,0x7E,0xD4,0x98]) + b"DL5DI").hexdigest())'
        // and, independently,
        //   printf '\x0a\x7e\xd4\x98DL5DI' | shasum -a 256
        // both give a763d5c7...bf921119.
        assert_eq!(
            digest,
            [
                0xA7, 0x63, 0xD5, 0xC7, 0x3E, 0x65, 0xA2, 0xE3, 0x1B, 0x2F, 0xCA, 0x6F, 0xD4, 0x60,
                0x6C, 0xB6, 0x4F, 0x5D, 0xBC, 0xDD, 0x0A, 0xFA, 0x9F, 0x5E, 0x4D, 0xDB, 0xF5, 0x58,
                0xBF, 0x92, 0x11, 0x19,
            ]
        );
        assert_eq!(digest.len(), 32);

        let packet = auth(id(), salt, "DL5DI");
        assert_eq!(packet.len(), 40);
        assert_eq!(&packet[..4], b"RPTK");
        assert_eq!(&packet[4..8], &id().to_be_bytes());
        assert_eq!(&packet[8..], &digest);
    }

    #[test]
    fn the_config_packet_is_three_hundred_and_two_bytes_at_the_published_offsets() {
        // DroidStar dmr.cpp: the single ::sprintf plus out.append(buffer, 302),
        // corroborated at offset 38 by DMRGateway/DMRNetwork.cpp: writeConfig,
        // which patches 17 bytes there for a location-free config -- exactly
        // latitude(8) + longitude(9).
        let fields = ConfigFields::softclient("KC0ABC", "astar 0.1.11-beta");
        let bytes = config(id(), &fields);
        assert_eq!(bytes.len(), 302);
        assert_eq!(&bytes[..4], b"RPTC");
        assert_eq!(&bytes[4..8], &id().to_be_bytes());
        assert_eq!(&bytes[8..16], b"KC0ABC  ");
        assert_eq!(&bytes[16..25], b"000000000");
        assert_eq!(&bytes[25..34], b"000000000");
        assert_eq!(&bytes[34..36], b"00");
        assert_eq!(&bytes[36..38], b"01");
        assert_eq!(&bytes[38..55], b"0.00000000.000000");
        assert_eq!(&bytes[55..58], b"000");
        assert_eq!(&bytes[97..98], b"4");
        assert_eq!(&bytes[222..239], b"astar 0.1.11-beta");
        assert!(
            bytes[8..].iter().all(u8::is_ascii),
            "every config field is ASCII"
        );
    }

    #[test]
    fn the_config_packet_never_carries_a_position() {
        // Not a formatting detail: a coordinate in a config packet is a home
        // address on a public network, and CLAUDE.md keeps personal
        // addresses out of astar entirely.
        let bytes = config(id(), &ConfigFields::softclient("KC0ABC", "astar"));
        assert_eq!(&bytes[38..46], b"0.000000");
        assert_eq!(&bytes[46..55], b"00.000000");
    }

    #[test]
    fn an_over_long_field_is_truncated_not_allowed_to_shift_the_packet() {
        // Every field is fixed-width. A long callsign that pushed the rest
        // along would put the master's parse out by n bytes for the whole
        // remainder -- a silent, total corruption of the config.
        let mut fields = ConfigFields::softclient("KC0ABCDEFGH", "astar");
        fields.location = "x".repeat(64);
        let bytes = config(id(), &fields);
        assert_eq!(bytes.len(), 302);
        assert_eq!(&bytes[8..16], b"KC0ABCDE");
        assert_eq!(&bytes[58..78], &b"x".repeat(20)[..]);
    }

    #[test]
    fn a_ping_and_a_close_are_the_tag_and_the_id() {
        // CDMRNetwork::writePing -- "RPTPING" + 4 = 11; close(true) --
        // "RPTCL" + 4 = 9.
        assert_eq!(&ping(id())[..7], b"RPTPING");
        assert_eq!(ping(id()).len(), 11);
        assert_eq!(&close(id())[..5], b"RPTCL");
        assert_eq!(close(id()).len(), 9);
    }

    #[test]
    fn an_ack_with_ten_bytes_carries_a_salt_and_an_ack_to_a_later_stage_echoes_the_id() {
        // Both are ten bytes and both are "RPTACK" + four; only the state
        // machine knows which is which, so `parse` reports the four bytes as
        // BOTH a salt and an id and lets the FSM choose.
        let mut ack = [0u8; 10];
        ack[..6].copy_from_slice(b"RPTACK");
        ack[6..].copy_from_slice(&[0x0A, 0x7E, 0xD4, 0x98]);
        assert_eq!(
            parse(&ack),
            Some(Packet::Ack {
                id: 0x0A7E_D498,
                salt: Some([0x0A, 0x7E, 0xD4, 0x98])
            })
        );
    }

    #[test]
    fn a_truncated_ack_is_not_an_ack() {
        // Six to nine bytes of "RPTACK" carry neither a salt nor an id, so
        // they acknowledge nothing. Parsing them as an ack with an invented
        // id would let a stray datagram advance a handshake.
        for len in 6..ACK_LEN {
            let mut short = [0u8; ACK_LEN];
            short[..6].copy_from_slice(b"RPTACK");
            assert_eq!(
                parse(&short[..len]),
                Some(Packet::Unknown(short[..len].to_vec())),
                "an {len}-byte RPTACK must not parse as an ack"
            );
        }
    }

    #[test]
    fn a_nak_a_pong_and_a_master_close_are_recognised() {
        let mut nak = [0u8; 10];
        nak[..6].copy_from_slice(b"MSTNAK");
        nak[6..].copy_from_slice(&id().to_be_bytes());
        assert_eq!(parse(&nak), Some(Packet::Nak { id: 3_153_591 }));

        let mut pong = [0u8; 11];
        pong[..7].copy_from_slice(b"MSTPONG");
        pong[7..].copy_from_slice(&id().to_be_bytes());
        assert_eq!(parse(&pong), Some(Packet::Pong { id: 3_153_591 }));

        let mut cl = [0u8; 9];
        cl[..5].copy_from_slice(b"MSTCL");
        cl[5..].copy_from_slice(&id().to_be_bytes());
        assert_eq!(parse(&cl), Some(Packet::MasterClosing { id: 3_153_591 }));
    }

    fn sample_data() -> DataPacket {
        let mut burst = [0u8; BURST_LEN];
        for (i, slot) in burst.iter_mut().enumerate() {
            *slot = u8::try_from(i + 1).expect("small");
        }
        DataPacket {
            seq: 7,
            src_id: 3_153_591,
            dst_id: 31_313,
            peer_id: 3_153_591,
            slot: Timeslot::Ts2,
            call_type: CallType::Group,
            frame_type: FrameType::VoiceSync,
            stream_id: [0xDE, 0xAD, 0xBE, 0xEF],
            burst,
            ber: 0,
            rssi: 0,
        }
    }

    #[test]
    fn a_data_packet_is_fifty_five_bytes_and_round_trips() {
        // DMRGateway/DMRNetwork.cpp: HOMEBREW_DATA_PACKET_LENGTH = 55U, and
        // CDMRNetwork::write lays out tag(4) seq(1) src(3) dst(3) peer(4)
        // bits(1) stream(4) burst(33) ber(1) rssi(1).
        let packet = sample_data();
        let bytes = data(&packet).expect("ids in range");
        assert_eq!(bytes.len(), 55);
        assert_eq!(&bytes[..4], b"DMRD");
        assert_eq!(bytes[4], 7);
        assert_eq!(&bytes[5..8], &[0x30, 0x1E, 0xB7]);
        assert_eq!(&bytes[8..11], &[0x00, 0x7A, 0x51]);
        assert_eq!(&bytes[11..15], &[0x00, 0x30, 0x1E, 0xB7]);
        assert_eq!(&bytes[16..20], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&bytes[20..53], &packet.burst);
        assert_eq!(parse(&bytes), Some(Packet::Data(Box::new(packet))));
    }

    #[test]
    fn a_group_call_leaves_bit_six_clear_and_a_private_call_sets_it() {
        // THE bit this protocol is easiest to get backwards, and getting it
        // backwards turns every group call into a misrouted private one.
        // DMRGateway/DMRNetwork.cpp: `flco == FLCO::GROUP ? 0x00U : 0x40U`,
        // and hblink.py: `'unit' if (_bits & 0x40) else 'group'`.
        let mut p = sample_data();
        p.call_type = CallType::Group;
        assert_eq!(data(&p).expect("ids in range")[15] & 0x40, 0x00);
        p.call_type = CallType::Private;
        assert_eq!(data(&p).expect("ids in range")[15] & 0x40, 0x40);
    }

    #[test]
    fn the_timeslot_bit_is_set_for_ts2_only() {
        let mut p = sample_data();
        p.slot = Timeslot::Ts1;
        assert_eq!(data(&p).expect("ids in range")[15] & 0x80, 0x00);
        p.slot = Timeslot::Ts2;
        assert_eq!(data(&p).expect("ids in range")[15] & 0x80, 0x80);
    }

    #[test]
    fn the_frame_type_nibbles_are_the_ones_the_gateway_writes() {
        // DT_VOICE_SYNC -> 0x10; DT_VOICE -> the sequence number in bits 3-0
        // with 5-4 clear; anything else -> 0x20 | dataType.
        let mut p = sample_data();
        p.frame_type = FrameType::VoiceSync;
        assert_eq!(data(&p).expect("ids in range")[15] & 0x3F, 0x10);
        p.frame_type = FrameType::Voice { n: 3 };
        assert_eq!(data(&p).expect("ids in range")[15] & 0x3F, 0x03);
        p.frame_type = FrameType::DataSync { data_type: 0x02 };
        assert_eq!(data(&p).expect("ids in range")[15] & 0x3F, 0x22);
        for want in [
            FrameType::VoiceSync,
            FrameType::Voice { n: 5 },
            FrameType::DataSync { data_type: 0x01 },
            FrameType::Reserved { low: 0x0D },
        ] {
            p.frame_type = want;
            let Some(Packet::Data(back)) = parse(&data(&p).expect("ids in range")) else {
                panic!("data")
            };
            assert_eq!(back.frame_type, want);
        }
    }

    #[test]
    fn a_fifty_three_byte_data_packet_is_read_with_zero_ber_and_rssi() {
        // HBlink3-derived masters slice DMRD by offset and never check the
        // length, so a relay can arrive without G4KLX's two trailing bytes.
        // Refusing it would silence a whole class of master over a field
        // astar does not use.
        let bytes = data(&sample_data()).expect("ids in range");
        let Some(Packet::Data(short)) = parse(&bytes[..53]) else {
            panic!("data")
        };
        assert_eq!(short.ber, 0);
        assert_eq!(short.rssi, 0);
        assert_eq!(short.burst, sample_data().burst);
        assert_eq!(parse(&bytes[..52]), None);
    }

    #[test]
    fn a_seventy_one_byte_trunking_data_packet_is_ignored_rather_than_guessed_at() {
        // docs/design/dmr-wire.md sec.10: current DMRGateway also accepts
        // HOMEBREW_TRUNKING_DATA_PACKET_LENGTH (71, "DMRD with 16 byte UUID
        // extension"). That is YO8RZZ's trunking work, not the homebrew
        // protocol TGIF speaks; astar sends 55, accepts 53 or 55, and reads
        // nothing else as a DMRD.
        let mut long = data(&sample_data()).expect("ids in range").to_vec();
        long.extend_from_slice(&[0u8; 16]);
        assert_eq!(long.len(), 71);
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn an_id_past_twenty_four_bits_is_refused_rather_than_truncated() {
        // Writing the low three bytes and saying nothing would put a frame on
        // the air addressed to somebody else: 0x0100_0059 as a destination
        // would go out as TG 89.
        let mut p = sample_data();
        p.src_id = 0x0100_0000;
        assert_eq!(data(&p), Err(DataError::SourceTooLarge));

        let mut p = sample_data();
        p.dst_id = 0x0100_0059;
        assert_eq!(data(&p), Err(DataError::DestinationTooLarge));

        let mut p = sample_data();
        p.src_id = RADIO_ID_MAX;
        p.dst_id = RADIO_ID_MAX;
        let bytes = data(&p).expect("the widest legal pair");
        assert_eq!(&bytes[5..8], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&bytes[8..11], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn the_stream_id_is_four_opaque_bytes() {
        // Both DMRGateway and DroidStar memcpy a host-order uint32 in and
        // out, so there is no wire byte order to honour -- only equality.
        let p = sample_data();
        let Some(Packet::Data(back)) = parse(&data(&p).expect("ids in range")) else {
            panic!("data")
        };
        assert_eq!(back.stream_id, [0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn an_unknown_tag_is_kept_rather_than_thrown_away() {
        assert_eq!(
            parse(b"RPTSBKN\x00\x00\x00\x01"),
            Some(Packet::Unknown(b"RPTSBKN\x00\x00\x00\x01".to_vec()))
        );
        assert_eq!(parse(&[]), None);
    }
}
