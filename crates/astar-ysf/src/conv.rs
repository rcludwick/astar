// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The rate-1/2, constraint-length-5 convolutional code wrapped around a
//! YSF FICH, and a hard-decision Viterbi decoder for it.
//!
//! Two generator polynomials, in the usual notation with the newest bit
//! first: `1 + D^3 + D^4` (`0b11001`) and `1 + D + D^2 + D^4` (`0b10111`).
//! One input bit produces two output bits; sixteen encoder states.
//!
//! Bits are numbered MSB-first within each byte, which is how they are
//! numbered everywhere on this wire.

/// Number of memory bits, i.e. constraint length minus one.
const MEMORY: usize = 4;
/// Encoder states — one per possible contents of the memory.
const STATES: usize = 1 << MEMORY;
/// `1 + D^3 + D^4`, newest input bit in the low position.
const G1: usize = 0b1_1001;
/// `1 + D + D^2 + D^4`.
const G2: usize = 0b1_0111;

/// Reads bit `index` of `buf`, MSB-first within each byte.
#[must_use]
pub fn bit(buf: &[u8], index: usize) -> bool {
    buf[index / 8] >> (7 - index % 8) & 1 == 1
}

/// Writes bit `index` of `buf`, MSB-first within each byte.
pub fn set_bit(buf: &mut [u8], index: usize, value: bool) {
    let mask = 1u8 << (7 - index % 8);
    if value {
        buf[index / 8] |= mask;
    } else {
        buf[index / 8] &= !mask;
    }
}

/// The two output bits for input bit `input` from encoder state `state`.
///
/// `state` holds the four previous input bits, newest in bit 0.
fn branch(state: usize, input: usize) -> (bool, bool) {
    let register = (state << 1) | input;
    (
        (register & G1).count_ones() % 2 == 1,
        (register & G2).count_ones() % 2 == 1,
    )
}

/// Encodes `bits` input bits from `input` into `2 * bits` output bits in
/// `output`.
///
/// The encoder starts from the all-zero state. Flushing it is the caller's
/// job: append `MEMORY` zero bits to the input and they come back out as
/// the tail that lets the decoder finish in a known state.
///
/// # Panics
/// If either buffer is too short for the bit count.
pub fn encode(input: &[u8], output: &mut [u8], bits: usize) {
    assert!(input.len() * 8 >= bits, "input too short");
    assert!(output.len() * 8 >= bits * 2, "output too short");
    let mut state = 0usize;
    for i in 0..bits {
        let d = usize::from(bit(input, i));
        let (g1, g2) = branch(state, d);
        set_bit(output, i * 2, g1);
        set_bit(output, i * 2 + 1, g2);
        state = ((state << 1) | d) & (STATES - 1);
    }
}

/// Hard-decision Viterbi decoder for [`encode`].
///
/// Consumes `symbols` pairs of received bits and returns the `symbols`
/// decoded input bits, which include the caller's tail. Traceback starts
/// from the all-zero state, which is where a properly flushed encoder ends.
///
/// # Panics
/// If `input` is too short for `symbols` symbol pairs, or `symbols` is zero.
#[must_use]
pub fn decode(input: &[u8], symbols: usize) -> Vec<bool> {
    assert!(symbols > 0, "nothing to decode");
    assert!(input.len() * 8 >= symbols * 2, "input too short");

    // Path metric per state, and one predecessor-bit per state per step so
    // the winning path can be walked back.
    let mut metric = [u32::MAX; STATES];
    metric[0] = 0;
    let mut history: Vec<[usize; STATES]> = Vec::with_capacity(symbols);

    for i in 0..symbols {
        let r0 = bit(input, i * 2);
        let r1 = bit(input, i * 2 + 1);
        let mut next = [u32::MAX; STATES];
        let mut step = [0usize; STATES];
        for (state, &here) in metric.iter().enumerate() {
            if here == u32::MAX {
                continue;
            }
            for d in 0..2usize {
                let (g1, g2) = branch(state, d);
                let cost = u32::from(g1 != r0) + u32::from(g2 != r1);
                let successor = ((state << 1) | d) & (STATES - 1);
                let candidate = here + cost;
                if candidate < next[successor] {
                    next[successor] = candidate;
                    // Remember where we came from: the bit that fell off the
                    // end of the register is the predecessor's oldest.
                    step[successor] = state >> (MEMORY - 1);
                }
            }
        }
        metric = next;
        history.push(step);
    }

    // Walk back from the all-zero state. Each step yields the input bit that
    // drove the transition — the low bit of the state we are standing in.
    let mut state = 0usize;
    let mut out = vec![false; symbols];
    for i in (0..symbols).rev() {
        let dropped = history[i][state];
        out[i] = state & 1 == 1;
        state = (dropped << (MEMORY - 1)) | (state >> 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `bits` data bits plus a zero tail, then decode and compare.
    fn round_trip(data: &[u8], bits: usize) -> Vec<bool> {
        let total = bits + MEMORY;
        let mut padded = data.to_vec();
        padded.resize(total.div_ceil(8) + 1, 0);
        // Force the tail to zeros whatever the caller handed us.
        for i in bits..total {
            set_bit(&mut padded, i, false);
        }
        let mut encoded = vec![0u8; (total * 2).div_ceil(8)];
        encode(&padded, &mut encoded, total);
        decode(&encoded, total)
    }

    #[test]
    fn bits_are_numbered_msb_first() {
        let buf = [0b1000_0000u8, 0b0000_0001];
        assert!(bit(&buf, 0));
        assert!(!bit(&buf, 1));
        assert!(bit(&buf, 15));

        let mut out = [0u8; 2];
        set_bit(&mut out, 0, true);
        set_bit(&mut out, 15, true);
        assert_eq!(out, [0b1000_0000, 0b0000_0001]);
        set_bit(&mut out, 0, false);
        assert_eq!(out, [0b0000_0000, 0b0000_0001]);
    }

    #[test]
    fn the_first_input_bit_appears_in_both_outputs() {
        // From the all-zero state a 1 sets both generators' newest tap.
        let mut out = [0u8; 1];
        encode(&[0b1000_0000], &mut out, 1);
        assert_eq!(out[0] >> 6, 0b11);
    }

    #[test]
    fn a_clean_round_trip_returns_the_input() {
        let data = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
        let bits = 40;
        let decoded = round_trip(&data, bits);
        for (i, value) in decoded.iter().take(bits).enumerate() {
            assert_eq!(*value, bit(&data, i), "bit {i}");
        }
    }

    #[test]
    fn the_flushed_tail_decodes_as_zeros() {
        let data = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
        let bits = 40;
        let decoded = round_trip(&data, bits);
        for (i, value) in decoded.iter().enumerate().skip(bits) {
            assert!(!value, "tail bit {i}");
        }
    }

    #[test]
    fn a_single_channel_error_is_corrected() {
        let data = [0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
        let bits = 40;
        let total = bits + MEMORY;
        let mut padded = data.to_vec();
        padded.resize(8, 0);
        for i in bits..total {
            set_bit(&mut padded, i, false);
        }
        let mut encoded = vec![0u8; (total * 2).div_ceil(8)];
        encode(&padded, &mut encoded, total);

        for flip in 0..total * 2 {
            let mut corrupt = encoded.clone();
            let flipped = !bit(&corrupt, flip);
            set_bit(&mut corrupt, flip, flipped);
            let decoded = decode(&corrupt, total);
            for (i, value) in decoded.iter().take(bits).enumerate() {
                assert_eq!(*value, bit(&data, i), "flip {flip}, bit {i}");
            }
        }
    }
}
