// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The eighteen voice records of a P25 superframe.
//!
//! P25 Phase 1 voice travels in Logical Data Units (LDUs) of nine IMBE
//! frames, 180 ms of audio. LDU1 and LDU2 alternate, so a superframe is two
//! LDUs, 360 ms, eighteen frames. On the reflector wire each frame is its
//! own UDP datagram, a *record*, tagged `0x62`–`0x6A` for LDU1 and
//! `0x6B`–`0x73` for LDU2.
//!
//! Each record carries one IMBE frame as 11 bytes: the 88 raw voice bits,
//! most significant bit first, with the radio's FEC, interleaving and
//! whitening already removed. Around it, some records carry three bytes of
//! call information. The table is read out of `MMDVMHost/P25Network.cpp`
//! (`CP25Network::writeLDU1`, `writeLDU2` and the `REC62`…`REC73` arrays):
//!
//! | Tags | Length | IMBE at | Bytes 1–3 |
//! |---|---|---|---|
//! | `0x62`, `0x6B` | 22 | 10 | bytes 1–9 are `02 02 0C 0B 12 64 00 00 80` |
//! | `0x63`, `0x6C` | 14 | 1 | none |
//! | `0x64`–`0x69`, `0x6D`–`0x72` | 17 | 5 | the record's fields |
//! | `0x6A`, `0x73` | 16 | 4 | LSD, LSD, `02` |
//!
//! The fields worth reading are the talkgroup (`0x65`), the source radio ID
//! (`0x66`), both 24-bit big-endian, and the algorithm ID (`0x70` byte 1,
//! `0x80` for a clear call). This crate reads the ALGID so a layer up can
//! mute an encrypted call. It never decrypts anything, and depends on no
//! cryptography at all.

/// Records in a superframe: two LDUs of nine.
pub const RECORDS: usize = 18;
/// The tag of the first record, LDU1 frame 0.
pub const FIRST_TAG: u8 = 0x62;
/// The tag of the last record, LDU2 frame 8.
pub const LAST_TAG: u8 = 0x73;
/// The algorithm ID of a clear (unencrypted) call (TIA-102.AAAD; also
/// `MMDVMHost/P25Defines.h`, `P25_ALGO_UNENCRYPT`).
pub const ALGID_CLEAR: u8 = 0x80;

/// Bytes in one IMBE frame on the wire.
const IMBE_LEN: usize = 11;

/// Bytes 1–9 of the two header records, `0x62` and `0x6B`
/// (`MMDVMHost/P25Network.cpp`, `REC62` and `REC6B`).
const HEADER: [u8; 9] = [0x02, 0x02, 0x0C, 0x0B, 0x12, 0x64, 0x00, 0x00, 0x80];

/// The last byte of every 14- and 17-byte record (`MMDVMHost/P25Network.cpp`,
/// `REC63`…`REC72`).
const TRAILER: u8 = 0x02;

/// Bytes 1–3 of the five records whose templates carry fixed parity
/// (`MMDVMHost/P25Network.cpp`, `REC67`, `REC68`, `REC69`, `REC71` and
/// `REC72`), by record index. `writeLDU1` and `writeLDU2` never overwrite
/// them.
const TEMPLATE_PARITY: [(u8, [u8; 3]); 5] = [
    (5, [0xF0, 0x9D, 0x6A]),
    (6, [0x19, 0xD4, 0x26]),
    (7, [0xE0, 0xEB, 0x7B]),
    (15, [0xAC, 0xB8, 0xA4]),
    (16, [0x9B, 0xDC, 0x75]),
];

/// Record indices (tag minus `0x62`) of interest.
const INDEX_LCF: u8 = 2;
const INDEX_TALKGROUP: u8 = 3;
const INDEX_SOURCE: u8 = 4;
const INDEX_LDU1_LSD: u8 = 8;
const INDEX_ALGID: u8 = 14;
const INDEX_LDU2_LSD: u8 = 17;

/// One voice record: which of the eighteen it is, its IMBE frame, and its
/// three field bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceRecord {
    /// Position in the superframe, `tag - 0x62`, 0 to 17.
    pub index: u8,
    /// The 88 IMBE voice bits, MSB first.
    pub imbe: [u8; 11],
    /// Bytes 1–3 of the record. Zeros for the four records that carry none
    /// (`0x62`, `0x63`, `0x6B`, `0x6C`).
    pub fields: [u8; 3],
}

/// The length of record `index` on the wire, or 0 for an index outside
/// 0..18.
#[must_use]
pub const fn record_len(index: u8) -> usize {
    match index {
        0 | 9 => 22,
        1 | 10 => 14,
        8 | 17 => 16,
        2..=7 | 11..=16 => 17,
        _ => 0,
    }
}

/// Where record `index`'s IMBE frame starts, or 0 for an index outside
/// 0..18.
#[must_use]
pub const fn imbe_offset(index: u8) -> usize {
    match index {
        0 | 9 => 10,
        1 | 10 => 1,
        8 | 17 => 4,
        2..=7 | 11..=16 => 5,
        _ => 0,
    }
}

/// True for the records whose bytes 1–3 are fields rather than the constant
/// header or the start of the IMBE frame.
const fn carries_fields(index: u8) -> bool {
    matches!(index, 2..=8 | 11..=17)
}

fn u24(bytes: [u8; 3]) -> u32 {
    u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
}

fn u24_bytes(value: u32) -> [u8; 3] {
    let b = value.to_be_bytes();
    [b[1], b[2], b[3]]
}

impl VoiceRecord {
    /// The record's tag, `0x62 + index`.
    #[must_use]
    pub const fn tag(&self) -> u8 {
        FIRST_TAG.wrapping_add(self.index)
    }

    /// The talkgroup, from record `0x65`; `None` on every other record.
    #[must_use]
    pub fn talkgroup(&self) -> Option<u32> {
        (self.index == INDEX_TALKGROUP).then(|| u24(self.fields))
    }

    /// The source radio ID, from record `0x66`; `None` on every other record.
    #[must_use]
    pub fn source_id(&self) -> Option<u32> {
        (self.index == INDEX_SOURCE).then(|| u24(self.fields))
    }

    /// The algorithm ID, from record `0x70` byte 1; `None` on every other
    /// record. [`ALGID_CLEAR`] means the call is not encrypted.
    #[must_use]
    pub fn algid(&self) -> Option<u8> {
        (self.index == INDEX_ALGID).then_some(self.fields[0])
    }

    /// The record as a datagram: the constant header, the fields, the
    /// trailing `02` and the IMBE frame, every other byte zero.
    /// Empty for an `index` outside 0..18.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let len = record_len(self.index);
        if len == 0 {
            return Vec::new();
        }
        let mut out = vec![0u8; len];
        out[0] = self.tag();
        if matches!(self.index, 0 | 9) {
            out[1..=HEADER.len()].copy_from_slice(&HEADER);
        }
        if carries_fields(self.index) {
            out[1..4].copy_from_slice(&self.fields);
        }
        // MMDVMHost's templates end every 14- and 17-byte record with `02`,
        // after the IMBE frame. No reader looks at it; it is written so a
        // record built here matches real traffic byte for byte.
        if matches!(len, 14 | 17) {
            out[len - 1] = TRAILER;
        }
        let off = imbe_offset(self.index);
        out[off..off + IMBE_LEN].copy_from_slice(&self.imbe);
        out
    }

    /// Reads a voice record out of a datagram: its tag must be one of the
    /// eighteen and its length exactly that record's. `None` otherwise.
    pub(crate) fn decode(datagram: &[u8]) -> Option<VoiceRecord> {
        let index = datagram.first()?.checked_sub(FIRST_TAG)?;
        let len = record_len(index);
        if len == 0 || datagram.len() != len {
            return None;
        }
        let off = imbe_offset(index);
        let imbe: [u8; IMBE_LEN] = datagram.get(off..off + IMBE_LEN)?.try_into().ok()?;
        let fields = if carries_fields(index) {
            datagram.get(1..4)?.try_into().ok()?
        } else {
            [0; 3]
        };
        Some(VoiceRecord {
            index,
            imbe,
            fields,
        })
    }
}

/// The call information a superframe carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superframe {
    /// Source radio ID. Only the low 24 bits go on the wire.
    pub source_id: u32,
    /// Talkgroup. Only the low 24 bits go on the wire.
    pub talkgroup: u32,
    /// Algorithm ID; [`ALGID_CLEAR`] for a clear call.
    pub algid: u8,
    /// Key ID.
    pub kid: u16,
}

/// Builds the eighteen datagrams of one superframe, `0x62` to `0x73` in
/// order, around eighteen IMBE frames.
///
/// Byte for byte what `MMDVMHost/P25Network.cpp` sends for a clear group
/// call: its templates (the constant header in `0x62`/`0x6B`, the fixed
/// parity bytes in `0x67`–`0x69` and `0x71`/`0x72`, the trailing `02`),
/// with LCF `0x00` and MFID `0x00` in `0x64`, the talkgroup in `0x65`, the
/// source in `0x66`, ALGID and KID in `0x70`, zero LSD bytes, and a zero
/// message indicator in `0x6D`–`0x6F`. It is for the loopback reflector and
/// tests; transmit is out of scope.
#[must_use]
pub fn superframe(info: &Superframe, frames: &[[u8; 11]; RECORDS]) -> Vec<Vec<u8>> {
    (0u8..)
        .zip(frames)
        .map(|(index, imbe)| {
            let fields = match index {
                INDEX_LCF => [0x00, 0x00, 0x00],
                INDEX_TALKGROUP => u24_bytes(info.talkgroup),
                INDEX_SOURCE => u24_bytes(info.source_id),
                INDEX_ALGID => {
                    let kid = info.kid.to_be_bytes();
                    [info.algid, kid[0], kid[1]]
                }
                INDEX_LDU1_LSD | INDEX_LDU2_LSD => [0x00, 0x00, 0x02],
                _ => TEMPLATE_PARITY
                    .iter()
                    .find(|(i, _)| *i == index)
                    .map_or([0; 3], |(_, p)| *p),
            };
            VoiceRecord {
                index,
                imbe: *imbe,
                fields,
            }
            .encode()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Packet, parse};

    /// Distinct bytes for every record that carries fields, zeros for the
    /// four that carry none.
    fn field_bytes_for(index: u8) -> [u8; 3] {
        match index {
            0 | 1 | 9 | 10 => [0; 3],
            i => [i, i.wrapping_mul(7), 0xA0 | i],
        }
    }

    fn voice(datagram: &[u8]) -> VoiceRecord {
        match parse(datagram) {
            Some(Packet::Voice(r)) => r,
            other => panic!("expected a voice record, got {other:?}"),
        }
    }

    #[test]
    fn every_record_type_round_trips() {
        for index in 0..18u8 {
            let rec = VoiceRecord {
                index,
                imbe: [index; 11],
                fields: field_bytes_for(index),
            };
            let bytes = rec.encode();
            assert_eq!(bytes.len(), record_len(index));
            assert_eq!(bytes[0], 0x62 + index);
            assert_eq!(rec.tag(), 0x62 + index);
            let off = imbe_offset(index);
            assert_eq!(&bytes[off..off + 11], &[index; 11]);
            assert_eq!(parse(&bytes), Some(Packet::Voice(rec)));
        }
    }

    #[test]
    fn the_record_table_matches_mmdvmhost() {
        let table: [(u8, usize, usize); 18] = [
            (0x62, 22, 10),
            (0x63, 14, 1),
            (0x64, 17, 5),
            (0x65, 17, 5),
            (0x66, 17, 5),
            (0x67, 17, 5),
            (0x68, 17, 5),
            (0x69, 17, 5),
            (0x6A, 16, 4),
            (0x6B, 22, 10),
            (0x6C, 14, 1),
            (0x6D, 17, 5),
            (0x6E, 17, 5),
            (0x6F, 17, 5),
            (0x70, 17, 5),
            (0x71, 17, 5),
            (0x72, 17, 5),
            (0x73, 16, 4),
        ];
        for (tag, len, off) in table {
            let index = tag - FIRST_TAG;
            assert_eq!(record_len(index), len, "{tag:#04x}");
            assert_eq!(imbe_offset(index), off, "{tag:#04x}");
        }
        assert_eq!(RECORDS, 18);
        assert_eq!(LAST_TAG - FIRST_TAG + 1, 18);
    }

    #[test]
    fn an_out_of_range_index_has_no_record() {
        assert_eq!(record_len(18), 0);
        assert_eq!(imbe_offset(200), 0);
        let rec = VoiceRecord {
            index: 200,
            imbe: [0; 11],
            fields: [0; 3],
        };
        assert_eq!(rec.encode(), Vec::<u8>::new());
        assert_eq!(rec.talkgroup(), None);
    }

    #[test]
    fn the_header_records_carry_the_constant_header() {
        for index in [0u8, 9] {
            let bytes = VoiceRecord {
                index,
                imbe: [0; 11],
                fields: [0; 3],
            }
            .encode();
            assert_eq!(
                &bytes[1..10],
                &[0x02, 0x02, 0x0C, 0x0B, 0x12, 0x64, 0x00, 0x00, 0x80]
            );
        }
    }

    #[test]
    fn talkgroup_and_source_are_24_bit_big_endian() {
        let tg = parse(&[
            0x65, 0x00, 0x27, 0xD8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
        let Packet::Voice(r) = tg else { panic!() };
        assert_eq!(r.talkgroup(), Some(10_200));
        assert_eq!(r.source_id(), None);
        let src = voice(&[
            0x66, 0x12, 0xD6, 0x87, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(src.source_id(), Some(1_234_567));
        assert_eq!(src.talkgroup(), None);
        let max = voice(&[
            0x66, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(max.source_id(), Some(0x00FF_FFFF));
    }

    #[test]
    fn the_algid_is_byte_one_of_0x70() {
        let clear = voice(&[0x70, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(clear.algid(), Some(ALGID_CLEAR));
        let aes = voice(&[
            0x70, 0x84, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(aes.algid(), Some(0x84));
        let other = voice(&[0x6F, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(other.algid(), None);
    }

    /// `MMDVMHost/P25Network.cpp` lines 36–82: the `REC62`…`REC73` templates
    /// `CP25Network::writeLDU1` and `writeLDU2` start every record from,
    /// written out as data. Bytes the writers overwrite are zero here and
    /// laid in by the test below.
    fn mmdvmhost_template(index: u8) -> Vec<u8> {
        let mut t = vec![0u8; record_len(index)];
        t[0] = FIRST_TAG + index;
        match index {
            0 | 9 => {
                t[1..10].copy_from_slice(&[0x02, 0x02, 0x0C, 0x0B, 0x12, 0x64, 0x00, 0x00, 0x80]);
            }
            1 | 10 => t[13] = 0x02,
            8 | 17 => t[3] = 0x02,
            _ => t[16] = 0x02,
        }
        let parity: Option<[u8; 3]> = match index {
            5 => Some([0xF0, 0x9D, 0x6A]),
            6 => Some([0x19, 0xD4, 0x26]),
            7 => Some([0xE0, 0xEB, 0x7B]),
            15 => Some([0xAC, 0xB8, 0xA4]),
            16 => Some([0x9B, 0xDC, 0x75]),
            _ => None,
        };
        if let Some(p) = parity {
            t[1..4].copy_from_slice(&p);
        }
        t
    }

    #[test]
    fn superframe_matches_mmdvmhost_byte_for_byte() {
        let frames: [[u8; 11]; RECORDS] = std::array::from_fn(|i| {
            let i = u8::try_from(i).unwrap();
            std::array::from_fn(|j| i.wrapping_mul(13) ^ u8::try_from(j).unwrap())
        });
        let recs = superframe(
            &Superframe {
                source_id: 1_234_567,
                talkgroup: 10_200,
                algid: 0x80,
                kid: 0x1234,
            },
            &frames,
        );
        for (index, rec) in (0u8..).zip(&recs) {
            // What the writers lay over the template: LCF and MFID (zero
            // for a group call) into 0x64, the talkgroup into 0x65, the
            // source into 0x66, ALGID and KID into 0x70, the two LSD bytes
            // (zero) into 0x6A/0x73, the MI (zero for a clear call) into
            // 0x6D–0x6F, and the IMBE frame at its offset.
            let mut want = mmdvmhost_template(index);
            match index {
                3 => want[1..4].copy_from_slice(&[0x00, 0x27, 0xD8]),
                4 => want[1..4].copy_from_slice(&[0x12, 0xD6, 0x87]),
                14 => want[1..4].copy_from_slice(&[0x80, 0x12, 0x34]),
                _ => {}
            }
            let off = imbe_offset(index);
            want[off..off + 11].copy_from_slice(&frames[usize::from(index)]);
            assert_eq!(rec, &want, "record {:#04x}", FIRST_TAG + index);
        }
    }

    #[test]
    fn superframe_builds_eighteen_records_in_order() {
        let recs = superframe(
            &Superframe {
                source_id: 1_234_567,
                talkgroup: 10_200,
                algid: 0x80,
                kid: 0x1234,
            },
            &[[7; 11]; 18],
        );
        assert_eq!(recs.len(), 18);
        assert!(
            recs.iter()
                .enumerate()
                .all(|(i, r)| usize::from(r[0]) == 0x62 + i)
        );
        assert!(
            recs.iter()
                .enumerate()
                .all(|(i, r)| r.len() == record_len(u8::try_from(i).unwrap()))
        );
        assert_eq!(voice(&recs[4]).source_id(), Some(1_234_567));
        assert_eq!(voice(&recs[3]).talkgroup(), Some(10_200));
        assert_eq!(voice(&recs[14]).algid(), Some(0x80));
        assert_eq!(voice(&recs[14]).fields, [0x80, 0x12, 0x34]);
        assert_eq!(voice(&recs[2]).fields, [0, 0, 0]);
        assert_eq!(voice(&recs[8]).fields, [0, 0, 0x02]);
        assert_eq!(voice(&recs[17]).fields, [0, 0, 0x02]);
        for r in &recs {
            assert_eq!(voice(r).imbe, [7; 11]);
        }
    }
}
