// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The CRC-16 that guards a YSF FICH.
//!
//! Parameters, because the catalogue name is not worth arguing about:
//! polynomial `0x1021`, initial value `0x0000`, neither input nor output
//! reflected, final XOR `0xFFFF`. That is XMODEM's CRC with the result
//! inverted, and it is **not** the "CCITT" that D-Star's RF header uses
//! (`astar_dstar::crc_ccitt`), which is reflected and seeded to all-ones.
//! Two protocols, two CRCs that share a polynomial and nothing else — hence
//! a local one rather than a shared helper that would have to carry a mode
//! flag and get it wrong once.
//!
//! Stored big-endian: the high byte first. See [`append`].

/// The CRC-16 of `data`.
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0x0000;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    !crc
}

/// Writes the CRC of `buf[..buf.len() - 2]` into the final two bytes,
/// high byte first.
///
/// # Panics
/// If `buf` is shorter than three bytes — there would be nothing to protect.
pub fn append(buf: &mut [u8]) {
    assert!(buf.len() > 2, "CRC needs at least one byte to protect");
    let n = buf.len();
    let crc = crc16(&buf[..n - 2]);
    buf[n - 2] = (crc >> 8) as u8;
    buf[n - 1] = (crc & 0xFF) as u8;
}

/// Whether the final two bytes of `buf` are the CRC of everything before them.
#[must_use]
pub fn check(buf: &[u8]) -> bool {
    if buf.len() <= 2 {
        return false;
    }
    let n = buf.len();
    let crc = crc16(&buf[..n - 2]);
    buf[n - 2] == (crc >> 8) as u8 && buf[n - 1] == (crc & 0xFF) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors taken by running the reference implementation's table-driven
    // form (YSFClients' `CCRC::addCCITT16`) over these inputs — the point of
    // writing the CRC out bitwise here is that it has to agree with the
    // wire, and these pin that it does.
    #[test]
    fn matches_the_reference_vectors() {
        assert_eq!(crc16(&[0x12, 0x34, 0x56, 0x78]), 0x4BD3);
        assert_eq!(crc16(b"123456789"), 0xCE3C);
        assert_eq!(crc16(&[0x00, 0x00, 0x00, 0x00]), 0xFFFF);
    }

    #[test]
    fn an_empty_input_is_the_inverted_seed() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }

    #[test]
    fn append_then_check_round_trips() {
        let mut buf = [0x11, 0x22, 0x33, 0x44, 0x00, 0x00];
        append(&mut buf);
        assert!(check(&buf));
        assert_eq!(u16::from_be_bytes([buf[4], buf[5]]), crc16(&buf[..4]));
    }

    #[test]
    fn a_flipped_bit_fails_the_check() {
        let mut buf = [0x11, 0x22, 0x33, 0x44, 0x00, 0x00];
        append(&mut buf);
        buf[2] ^= 0x01;
        assert!(!check(&buf));
    }

    #[test]
    fn too_short_to_carry_a_crc_never_checks_out() {
        assert!(!check(&[]));
        assert!(!check(&[0xAB, 0xCD]));
    }
}
