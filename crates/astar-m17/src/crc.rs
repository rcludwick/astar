// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! CRC-16/M17: poly `0x5935`, init `0xFFFF`, no input/output reflection, no
//! XOR-out. Computed bit-by-bit, MSB-first.

/// Computes the CRC-16/M17 checksum of `data`.
#[must_use]
pub fn crc16_m17(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            if crc & 0x8000 == 0 {
                crc <<= 1;
            } else {
                crc = (crc << 1) ^ 0x5935;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_test_vectors() {
        assert_eq!(crc16_m17(b""), 0xFFFF);
        assert_eq!(crc16_m17(b"A"), 0x206E);
        assert_eq!(crc16_m17(b"123456789"), 0x772B);
        let all: Vec<u8> = (0u8..=255).collect();
        assert_eq!(crc16_m17(&all), 0x1C31);
    }
}
