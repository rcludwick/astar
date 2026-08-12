// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 base-40 callsign addressing.
//!
//! Callsigns are packed into a 48-bit address using a base-40 alphabet, then
//! stored big-endian in 6 bytes. See the M17 spec section on addressing.

/// The base-40 alphabet. Index 0 is space; `A`-`Z` are 1..=26; `0`-`9` are
/// 27..=36; `-` is 37; `/` is 38; `.` is 39.
const ALPHABET: &[u8; 40] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-/.";

/// Reserved 6-byte address meaning "broadcast to all stations".
pub const BROADCAST: [u8; 6] = [0xFF; 6];

/// Encodes a callsign (or reflector name, e.g. `"M17-KCW A"`) into its 6-byte
/// base-40 address.
///
/// Input is uppercased before encoding. Returns `None` if `cs` is empty, has
/// more than 9 characters, or contains a character outside [`ALPHABET`].
#[must_use]
pub fn encode_callsign(cs: &str) -> Option<[u8; 6]> {
    if cs.is_empty() || cs.chars().count() > 9 {
        return None;
    }
    let upper = cs.to_uppercase();
    let mut addr: u64 = 0;
    for c in upper.chars().rev() {
        let val = alphabet_index(c)?;
        addr = addr * 40 + u64::from(val);
    }
    let be = addr.to_be_bytes();
    Some([be[2], be[3], be[4], be[5], be[6], be[7]])
}

/// Decodes a 6-byte base-40 address back into its callsign string.
///
/// Trailing padding (encoded as space, value 0) is dropped naturally: decoding
/// stops once the remaining address value reaches zero.
#[must_use]
pub fn decode_callsign(addr: &[u8; 6]) -> String {
    let mut val: u64 = 0;
    for &b in addr {
        val = (val << 8) | u64::from(b);
    }
    let mut out = String::new();
    while val > 0 {
        let idx = usize::try_from(val % 40).unwrap_or(0);
        out.push(char::from(ALPHABET[idx]));
        val /= 40;
    }
    out
}

fn alphabet_index(c: char) -> Option<u8> {
    let b = u8::try_from(c).ok()?;
    let i = ALPHABET.iter().position(|&a| a == b)?;
    u8::try_from(i).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ab1cd_matches_the_spec_worked_example() {
        assert_eq!(
            encode_callsign("AB1CD"),
            Some([0x00, 0x00, 0x00, 0x9F, 0xDD, 0x51])
        );
        assert_eq!(
            decode_callsign(&[0x00, 0x00, 0x00, 0x9F, 0xDD, 0x51]),
            "AB1CD"
        );
    }

    #[test]
    fn round_trips_lowercase_and_rejects_invalids() {
        let enc = encode_callsign("n0call").unwrap();
        assert_eq!(decode_callsign(&enc), "N0CALL");
        assert!(encode_callsign("").is_none());
        assert!(encode_callsign("TENCHARSXX").is_none()); // 10 chars
        assert!(encode_callsign("BAD!").is_none());
    }

    #[test]
    fn broadcast_is_all_ff() {
        assert_eq!(BROADCAST, [0xFF; 6]);
    }

    #[test]
    fn reflector_name_with_module_space_round_trips() {
        let enc = encode_callsign("M17-KCW A").unwrap();
        assert_eq!(decode_callsign(&enc), "M17-KCW A");
    }
}
