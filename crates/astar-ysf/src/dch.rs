// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The DATA channel (DCH) that rides in a YSF payload beside the voice — the
//! part a Yaesu radio reads to put a callsign on its screen, and the part
//! every gateway between a reflector and the air needs before it will
//! attribute a transmission to anybody.
//!
//! Designed in `docs/design/ysf-dch.md` (`iax-ysfdch`), which carries the
//! citations; this module is the algorithms.
//!
//! # Two shapes
//!
//! A payload is five 18-byte blocks, and what a block holds depends on the
//! FICH's frame info:
//!
//! * **Communications**, V/D mode 2 — each block is `[5 bytes DCH][13 bytes
//!   VCH]`. The five DCH slots make one 25-byte field carrying **ten** bytes
//!   of plaintext, and *which* ten depends on the frame number: see
//!   [`vd2_dch`].
//! * **Header and Terminator** — no voice at all. The payload is two
//!   independent 45-byte fields, the first taking bytes 0..9 of each block
//!   and the second bytes 9..18, each carrying **twenty** bytes of
//!   plaintext: CSD1 (destination + source) and CSD2 (downlink + uplink).
//!
//! # One chain, two widths
//!
//! ```text
//! plaintext → XOR whitening → append CRC-16 → zero tail byte
//!           → rate-1/2 convolutional → dibit interleave → scatter into blocks
//! ```
//!
//! The order of the first two is the easy thing to get backwards: the
//! whitening goes on **first**, and the CRC is computed over the *whitened*
//! bytes. Decoding runs it backwards and the CRC is what decides whether the
//! result is believable — a receiver that cannot check it leaves the channel
//! alone rather than acting on noise, which is exactly what the reference
//! implementations do and why astar's all-zero DCH was silently discarded
//! rather than complained about.
//!
//! The convolutional code and the interleave are the FICH's, at a different
//! width; [`crate::conv`] owns both, so there is one coder in this crate and
//! not three.

use crate::conv;
use crate::crc;
use crate::frame::PAYLOAD_LEN;
use crate::wire::{CALLSIGN_LEN, Callsign};

/// Plaintext bytes in one V/D mode 2 frame's data channel.
pub const DCH_LEN: usize = 10;

/// Plaintext bytes in one header/terminator callsign block (CSD1 or CSD2).
///
/// Two ten-byte callsign fields: CSD1 is destination then source, CSD2 is
/// downlink then uplink.
pub const CSD_LEN: usize = 2 * CALLSIGN_LEN;

/// The frame total (FT) a client transmits, and so the highest frame number.
///
/// FT is a *total*, not a count: the last frame of a superframe is the one
/// where FN equals it, so this means frames 0..=6 — seven of them. A client
/// that leaves FN and FT at zero never sends the FN 0 and FN 1 frames a
/// receiver takes the destination and source from.
pub const FRAME_TOTAL: u8 = 6;

/// Frames in one superframe: [`FRAME_TOTAL`] plus one.
pub const SUPERFRAME: u8 = FRAME_TOTAL + 1;

/// The wire's "unaddressed", used as the destination by a client talking to
/// a reflector rather than to a station.
///
/// Reflectors treat it as absent and fall back to the `YSFD` header's own
/// destination field.
pub const UNADDRESSED: [u8; CALLSIGN_LEN] = *b"**********";

/// A blank ten-byte field, which is what a client with nothing to say puts
/// in the remarks and DT slots.
pub const BLANK: [u8; CALLSIGN_LEN] = *b"          ";

/// Blocks in a payload.
const BLOCKS: usize = 5;
/// Bytes in one payload block.
const BLOCK_LEN: usize = 18;

/// Bytes of the V/D mode 2 DCH in each block, and so five per payload.
const VD2_SLOT: usize = 5;
/// Dibits in the V/D mode 2 DCH field: 25 bytes, 200 bits, 100 pairs.
const VD2_DIBITS: usize = 100;
/// Interleave columns for the V/D mode 2 DCH — `INTERLEAVE_TABLE_5_20`.
const VD2_COLS: usize = 5;
/// Bytes into the coder: ten of plaintext, two of CRC, one of flush tail.
const VD2_CODED: usize = DCH_LEN + 2 + 1;
/// Bytes of the assembled V/D mode 2 DCH field: five per block, five blocks.
const VD2_FIELD: usize = BLOCKS * VD2_SLOT;

/// Bytes of each CSD field in each block, and so nine per payload.
const CSD_SLOT: usize = 9;
/// Dibits in a CSD field: 45 bytes, 360 bits, 180 pairs.
const CSD_DIBITS: usize = 180;
/// Interleave columns for a CSD field — `INTERLEAVE_TABLE_9_20`.
const CSD_COLS: usize = 9;
/// Bytes into the coder: twenty of plaintext, two of CRC, one of flush tail.
const CSD_CODED: usize = CSD_LEN + 2 + 1;
/// Bytes of one assembled CSD field: nine per block, five blocks.
const CSD_FIELD: usize = BLOCKS * CSD_SLOT;

// ── The scrambler ───────────────────────────────────────────────────────

/// Bytes of scrambling sequence the references publish.
const WHITENING_LEN: usize = 20;

/// The YSF scrambler: 160 bits of the PN9 sequence `x^9 + x^5 + 1`, run from
/// state `0b1_0010_0111`, most significant stage out first.
///
/// The reference implementations print this as twenty magic bytes
/// (`WHITENING_DATA`). It is not magic — a nine-stage Fibonacci LFSR with
/// taps on stages 9 and 5 reproduces all 160 of those bits, which the
/// `the_scrambler_is_the_published_sequence` test asserts. The data channel
/// uses the first ten bytes of it or all twenty, depending on the shape.
const fn whitening() -> [u8; WHITENING_LEN] {
    let mut out = [0u8; WHITENING_LEN];
    let mut state: u16 = 0b1_0010_0111;
    let mut i = 0;
    while i < WHITENING_LEN * 8 {
        out[i / 8] |= ((state >> 8) as u8 & 1) << (7 - (i % 8));
        let feedback = ((state >> 8) ^ (state >> 4)) & 1;
        state = ((state << 1) | feedback) & 0x1FF;
        i += 1;
    }
    out
}

/// The scrambling sequence, generated once at compile time.
///
/// The same sequence whitens the voice channel; `astar-codec`'s `ysf` module
/// takes it from here so the two halves of a payload cannot disagree.
pub const WHITENING: [u8; WHITENING_LEN] = whitening();

// ── The coding chain, both widths ───────────────────────────────────────

/// Runs the encode chain over `plain`, returning the interleaved field.
///
/// `PLAIN` is the plaintext width, `CODED` is `PLAIN + 3` (CRC and tail) and
/// `FIELD` is `CODED * 2` less the tail's slack — expressed as `DIBITS`,
/// which is what the interleave is indexed by.
fn encode_field<const PLAIN: usize, const CODED: usize, const FIELD: usize>(
    plain: &[u8; PLAIN],
    dibits: usize,
    cols: usize,
) -> [u8; FIELD] {
    let mut coded = [0u8; CODED];
    for (slot, (&byte, &mask)) in coded.iter_mut().zip(plain.iter().zip(WHITENING.iter())) {
        *slot = byte ^ mask;
    }
    // The CRC covers the whitened bytes, not the plaintext, and lands in the
    // two bytes after them. The last byte stays zero: it is the four-bit
    // flush tail the Viterbi decoder finishes on, with four bits to spare.
    crc::append(&mut coded[..PLAIN + 2]);

    let mut convolved = [0u8; FIELD];
    conv::encode(&coded, &mut convolved, dibits);

    let mut field = [0u8; FIELD];
    for i in 0..dibits {
        let n = conv::dibit_interleave(i, cols);
        conv::set_bit(&mut field, n, conv::bit(&convolved, i * 2));
        conv::set_bit(&mut field, n + 1, conv::bit(&convolved, i * 2 + 1));
    }
    field
}

/// The inverse of [`encode_field`]. `None` when the CRC says the recovered
/// bytes are not what was sent.
fn decode_field<const PLAIN: usize, const CODED: usize, const FIELD: usize>(
    field: &[u8; FIELD],
    dibits: usize,
    cols: usize,
) -> Option<[u8; PLAIN]> {
    let mut convolved = [0u8; FIELD];
    for i in 0..dibits {
        let n = conv::dibit_interleave(i, cols);
        conv::set_bit(&mut convolved, i * 2, conv::bit(field, n));
        conv::set_bit(&mut convolved, i * 2 + 1, conv::bit(field, n + 1));
    }

    let bits = conv::decode(&convolved, dibits);
    let mut coded = [0u8; CODED];
    for (i, value) in bits.iter().take((PLAIN + 2) * 8).enumerate() {
        conv::set_bit(&mut coded, i, *value);
    }
    if !crc::check(&coded[..PLAIN + 2]) {
        return None;
    }

    let mut plain = [0u8; PLAIN];
    for (slot, (&byte, &mask)) in plain.iter_mut().zip(coded.iter().zip(WHITENING.iter())) {
        *slot = byte ^ mask;
    }
    Some(plain)
}

/// Gathers `slot` bytes from each block, starting `offset` into it.
fn gather<const N: usize>(payload: &[u8; PAYLOAD_LEN], offset: usize, slot: usize) -> [u8; N] {
    let mut out = [0u8; N];
    for block in 0..BLOCKS {
        let at = block * BLOCK_LEN + offset;
        out[block * slot..(block + 1) * slot].copy_from_slice(&payload[at..at + slot]);
    }
    out
}

/// The inverse of [`gather`]: scatters a field back across the blocks,
/// touching nothing else in the payload.
fn scatter<const N: usize>(
    payload: &mut [u8; PAYLOAD_LEN],
    offset: usize,
    slot: usize,
    field: &[u8; N],
) {
    for block in 0..BLOCKS {
        let at = block * BLOCK_LEN + offset;
        payload[at..at + slot].copy_from_slice(&field[block * slot..(block + 1) * slot]);
    }
}

// ── V/D mode 2: one ten-byte data channel per frame ─────────────────────

/// Writes one V/D mode 2 frame's data channel into `payload`.
///
/// Touches only the five DCH slots, leaving the voice channel exactly as it
/// was — so a caller may pack voice and data in either order.
pub fn write_vd2(payload: &mut [u8; PAYLOAD_LEN], dch: &[u8; DCH_LEN]) {
    let field = encode_field::<DCH_LEN, VD2_CODED, VD2_FIELD>(dch, VD2_DIBITS, VD2_COLS);
    scatter(payload, 0, VD2_SLOT, &field);
}

/// Reads one V/D mode 2 frame's data channel back out.
///
/// `None` when the CRC fails — which is what an all-zero or damaged data
/// channel looks like, and the reason a receiver must not act on one.
#[must_use]
pub fn read_vd2(payload: &[u8; PAYLOAD_LEN]) -> Option<[u8; DCH_LEN]> {
    let field: [u8; VD2_FIELD] = gather(payload, 0, VD2_SLOT);
    decode_field::<DCH_LEN, VD2_CODED, VD2_FIELD>(&field, VD2_DIBITS, VD2_COLS)
}

/// The ten bytes a client puts in frame `frame_number`'s data channel.
///
/// Frame numbers outside a superframe fold back into it, so a caller that
/// counts past [`FRAME_TOTAL`] gets the right slot rather than a blank.
///
/// | FN | contents |
/// |---|---|
/// | 0 | destination — [`UNADDRESSED`], which is what a client sends |
/// | 1 | source — this station |
/// | 2 | downlink — the gateway putting the frame on the network |
/// | 3 | uplink — the same |
/// | 4, 5 | remarks 1..4 — blank |
/// | 6 | DT1 — blank |
///
/// DT1 is where a Yaesu handheld puts its own radio-ID and GPS block. astar
/// sends a blank one: copying a specific handset's identifying constant onto
/// the air would be a false statement about what transmitted, and astar has
/// no GPS to fill it honestly. A blank block still passes its CRC.
#[must_use]
pub fn vd2_dch(frame_number: u8, source: &Callsign, gateway: &Callsign) -> [u8; DCH_LEN] {
    match frame_number % SUPERFRAME {
        0 => UNADDRESSED,
        1 => *source.as_bytes(),
        2 | 3 => *gateway.as_bytes(),
        _ => BLANK,
    }
}

// ── Header and terminator: CSD1 and CSD2 ────────────────────────────────

/// Writes a header or terminator payload: CSD1 into the first nine bytes of
/// every block, CSD2 into the last nine.
///
/// This overwrites the whole payload. A header frame carries no voice.
pub fn write_csd(payload: &mut [u8; PAYLOAD_LEN], csd1: &[u8; CSD_LEN], csd2: &[u8; CSD_LEN]) {
    let first = encode_field::<CSD_LEN, CSD_CODED, CSD_FIELD>(csd1, CSD_DIBITS, CSD_COLS);
    scatter(payload, 0, CSD_SLOT, &first);
    let second = encode_field::<CSD_LEN, CSD_CODED, CSD_FIELD>(csd2, CSD_DIBITS, CSD_COLS);
    scatter(payload, CSD_SLOT, CSD_SLOT, &second);
}

/// Reads a header or terminator payload back. `None` unless **both** blocks
/// pass their CRC — half a header is not a header.
#[must_use]
pub fn read_csd(payload: &[u8; PAYLOAD_LEN]) -> Option<([u8; CSD_LEN], [u8; CSD_LEN])> {
    let first: [u8; CSD_FIELD] = gather(payload, 0, CSD_SLOT);
    let second: [u8; CSD_FIELD] = gather(payload, CSD_SLOT, CSD_SLOT);
    Some((
        decode_field::<CSD_LEN, CSD_CODED, CSD_FIELD>(&first, CSD_DIBITS, CSD_COLS)?,
        decode_field::<CSD_LEN, CSD_CODED, CSD_FIELD>(&second, CSD_DIBITS, CSD_COLS)?,
    ))
}

/// The two callsign blocks a client puts in its header and terminator.
///
/// CSD1 is destination then source; CSD2 is downlink then uplink. A client
/// linked to a reflector is its own gateway, so the downlink and uplink are
/// the same callsign — which is also what a reflector overwrites when it
/// relays, so this is the honest value rather than a placeholder.
#[must_use]
pub fn header_csd(source: &Callsign, gateway: &Callsign) -> ([u8; CSD_LEN], [u8; CSD_LEN]) {
    let mut csd1 = [b' '; CSD_LEN];
    csd1[..CALLSIGN_LEN].copy_from_slice(&UNADDRESSED);
    csd1[CALLSIGN_LEN..].copy_from_slice(source.as_bytes());

    let mut csd2 = [b' '; CSD_LEN];
    csd2[..CALLSIGN_LEN].copy_from_slice(gateway.as_bytes());
    csd2[CALLSIGN_LEN..].copy_from_slice(gateway.as_bytes());

    (csd1, csd2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WHITENING_DATA`, as every reference implementation prints it.
    const WHITENING_REFERENCE: [u8; WHITENING_LEN] = [
        0x93, 0xD7, 0x51, 0x21, 0x9C, 0x2F, 0x6C, 0xD0, 0xEF, 0x0F, 0xF8, 0x3D, 0xF1, 0x73, 0x20,
        0x94, 0xED, 0x1E, 0x7C, 0xD8,
    ];

    fn call(s: &str) -> Callsign {
        Callsign::new(s).expect("a legal callsign")
    }

    #[test]
    fn the_scrambler_is_the_published_sequence() {
        assert_eq!(WHITENING, WHITENING_REFERENCE);
    }

    #[test]
    fn the_payload_arithmetic_adds_up() {
        assert_eq!(BLOCKS * BLOCK_LEN, PAYLOAD_LEN);
        assert_eq!(BLOCKS * VD2_SLOT * 8, VD2_DIBITS * 2);
        assert_eq!(BLOCKS * CSD_SLOT * 8, CSD_DIBITS * 2);
        // Plaintext plus CRC plus the flush tail, and the coder's input
        // width must cover exactly the dibits it is asked for.
        assert_eq!(VD2_CODED * 8, VD2_DIBITS + 4);
        assert_eq!(CSD_CODED * 8, CSD_DIBITS + 4);
        // The assembled fields hold exactly the coder's output.
        assert_eq!(VD2_FIELD, 25);
        assert_eq!(VD2_FIELD * 8, VD2_DIBITS * 2);
        assert_eq!(CSD_FIELD, 45);
        assert_eq!(CSD_FIELD * 8, CSD_DIBITS * 2);
    }

    #[test]
    fn a_vd2_data_channel_round_trips() {
        let mut payload = [0u8; PAYLOAD_LEN];
        let dch = *b"AJ7HR     ";
        write_vd2(&mut payload, &dch);
        assert_eq!(read_vd2(&payload), Some(dch));
    }

    #[test]
    fn writing_the_data_channel_leaves_the_voice_channel_alone() {
        // The DCH and the VCH share a block, and the whole point of the slot
        // arithmetic is that one may be written without disturbing the
        // other — `astar-codec`'s `pack_dn` relies on it in both directions.
        let mut payload = [0u8; PAYLOAD_LEN];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(0);
        }
        let before = payload;
        write_vd2(&mut payload, b"W1AW      ");
        for block in 0..BLOCKS {
            let vch = block * BLOCK_LEN + VD2_SLOT;
            assert_eq!(
                payload[vch..vch + 13],
                before[vch..vch + 13],
                "block {block}'s voice channel was disturbed"
            );
        }
    }

    #[test]
    fn an_all_zero_data_channel_is_refused_not_believed() {
        // This is the bug `iax-ysfdch` is about, as a test: a payload whose
        // data channel was never built does not decode to ten NUL bytes, it
        // fails its CRC — which is why every reference receiver dropped
        // astar's callsign on the floor without saying anything.
        let payload = [0u8; PAYLOAD_LEN];
        assert_eq!(read_vd2(&payload), None);
        assert_eq!(read_csd(&payload), None);
    }

    #[test]
    fn every_frame_of_a_superframe_round_trips_its_own_channel() {
        let me = call("AJ7HR");
        for fn_ in 0..SUPERFRAME {
            let dch = vd2_dch(fn_, &me, &me);
            let mut payload = [0u8; PAYLOAD_LEN];
            write_vd2(&mut payload, &dch);
            assert_eq!(read_vd2(&payload), Some(dch), "frame {fn_}");
        }
    }

    #[test]
    fn the_frame_numbers_carry_what_the_references_read() {
        let me = call("AJ7HR");
        let gw = call("W1AW");
        assert_eq!(vd2_dch(0, &me, &gw), *b"**********", "FN0 is destination");
        assert_eq!(vd2_dch(1, &me, &gw), *b"AJ7HR     ", "FN1 is source");
        assert_eq!(vd2_dch(2, &me, &gw), *b"W1AW      ", "FN2 is downlink");
        assert_eq!(vd2_dch(3, &me, &gw), *b"W1AW      ", "FN3 is uplink");
        assert_eq!(vd2_dch(4, &me, &gw), BLANK, "FN4 is remarks 1+2");
        assert_eq!(vd2_dch(5, &me, &gw), BLANK, "FN5 is remarks 3+4");
        assert_eq!(vd2_dch(6, &me, &gw), BLANK, "FN6 is DT1");
        // A caller counting past the superframe folds back rather than
        // falling off the end into a blank.
        assert_eq!(vd2_dch(SUPERFRAME + 1, &me, &gw), vd2_dch(1, &me, &gw));
    }

    #[test]
    fn a_header_payload_round_trips_both_blocks() {
        let me = call("AJ7HR");
        let (csd1, csd2) = header_csd(&me, &me);
        assert_eq!(&csd1[..CALLSIGN_LEN], b"**********");
        assert_eq!(&csd1[CALLSIGN_LEN..], b"AJ7HR     ");
        assert_eq!(&csd2[..CALLSIGN_LEN], b"AJ7HR     ");
        assert_eq!(&csd2[CALLSIGN_LEN..], b"AJ7HR     ");

        let mut payload = [0u8; PAYLOAD_LEN];
        write_csd(&mut payload, &csd1, &csd2);
        assert_eq!(read_csd(&payload), Some((csd1, csd2)));
    }

    #[test]
    fn the_two_header_blocks_do_not_overwrite_each_other() {
        // CSD1 takes bytes 0..9 of every block and CSD2 bytes 9..18. Get the
        // offsets wrong and one silently eats the other; both would still
        // "round trip" if they were written and read at the same place.
        let mut payload = [0u8; PAYLOAD_LEN];
        let csd1 = *b"**********AJ7HR     ";
        let csd2 = *b"W1AW      K0ABC     ";
        write_csd(&mut payload, &csd1, &csd2);
        let (a, b) = read_csd(&payload).expect("both blocks must decode");
        assert_eq!(a, csd1);
        assert_eq!(b, csd2);
        assert_ne!(a, b, "the two blocks must be independent");
    }

    /// Reads a hex string into a fixed field.
    fn hex<const N: usize>(text: &str) -> [u8; N] {
        let bytes = text.as_bytes();
        assert_eq!(bytes.len(), N * 2, "expected {N} bytes of hex");
        let mut out = [0u8; N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex");
        }
        out
    }

    /// The known-answer test, and it is only worth the name because these
    /// bytes did not come from this module.
    ///
    /// They were produced on 2026-09-07 by an independent re-implementation
    /// of the recipe in `docs/design/ysf-dch.md` — whitening, CRC-16 over the
    /// *whitened* bytes, zero tail byte, rate-1/2 convolutional code, dibit
    /// interleave, then the 5-byte (V/D mode 2) or 9+9-byte (header) scatter
    /// across the five payload blocks — written from the specification rather
    /// than from this code, and it agreed byte for byte.
    ///
    /// Composing the expectation out of this crate's own `crc`, `conv` and
    /// `dibit_interleave` would only prove `write_vd2` calls them in some
    /// order. These constants are what catches the whole chain being
    /// self-consistently wrong — a CRC computed before the whitening instead
    /// of after being the classic way for that to happen.
    #[test]
    fn a_known_answer_from_the_published_recipe() {
        let mut payload = [0u8; PAYLOAD_LEN];
        write_vd2(&mut payload, b"AJ7HR     ");
        let field: [u8; VD2_FIELD] = gather(&payload, 0, VD2_SLOT);
        assert_eq!(
            field,
            hex::<VD2_FIELD>("EBE8236E5494967A06F32545D5742D2C0F17E08B3BCAF1D964"),
            "V/D mode 2 data channel for \"AJ7HR     \""
        );

        let mut payload = [0u8; PAYLOAD_LEN];
        write_csd(
            &mut payload,
            b"**********AJ7HR     ",
            b"AJ7HR     AJ7HR     ",
        );
        let first: [u8; CSD_FIELD] = gather(&payload, 0, CSD_SLOT);
        let second: [u8; CSD_FIELD] = gather(&payload, CSD_SLOT, CSD_SLOT);
        assert_eq!(
            first,
            hex::<CSD_FIELD>(
                "F3F8F0C014531181A8B8A5E078E69026FF8EE5A32CD36CF1169C75431F043EE8A5C9938F25A83635570E2681BF"
            ),
            "CSD1 for \"**********AJ7HR     \""
        );
        assert_eq!(
            second,
            hex::<CSD_FIELD>(
                "F0A350C013AC7F31A8B21CDAB8E6921A948EE5A2353FACF110A146031F037FE525C99F8A1368363994D5A68183"
            ),
            "CSD2 for \"AJ7HR     AJ7HR     \""
        );
    }

    #[test]
    fn a_burst_of_channel_errors_is_repaired() {
        // What the three layers are for. The interleave spreads a burst
        // across the coder's memory, and the Viterbi decoder eats it.
        let dch = *b"AJ7HR     ";
        let mut payload = [0u8; PAYLOAD_LEN];
        write_vd2(&mut payload, &dch);
        for block in 0..BLOCKS {
            for start in 0..(VD2_SLOT * 8 - 3) {
                let mut damaged = payload;
                for b in start..start + 3 {
                    let at = block * BLOCK_LEN * 8 + b;
                    let flipped = !conv::bit(&damaged, at);
                    conv::set_bit(&mut damaged, at, flipped);
                }
                assert_eq!(
                    read_vd2(&damaged),
                    Some(dch),
                    "burst at block {block} bit {start}"
                );
            }
        }
    }

    #[test]
    fn a_field_of_noise_is_refused_rather_than_believed() {
        // As `fich`'s equivalent: not "always" — sixteen bits of CRC let one
        // through now and again — but a decoder that believed noise would
        // put a stranger's callsign on a radio's display.
        let mut accepted = 0;
        for seed in 0u32..64 {
            let mut payload = [0u8; PAYLOAD_LEN];
            let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
            for byte in &mut payload {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *byte = u8::try_from(x & 0xFF).unwrap_or(0);
            }
            if read_vd2(&payload).is_some() {
                accepted += 1;
            }
        }
        assert!(
            accepted <= 1,
            "{accepted} of 64 noise payloads were believed"
        );
    }
}
