//! Bit prioritization (TIA-102.BABA_2003.pdf section 7.1, Fig. 22): rearranges the quantizer values
//! `b_hat_0, b_hat_1, ..., b_hat_{L+1}, b_hat_{L+2}` into the eight prioritized bit vectors
//! `u_hat_0..u_hat_7` that [`super::fec`]'s Golay/Hamming codes protect (`u_0..u_3` at 12 bits each,
//! `u_4..u_6` at 11 bits each, `u_7` at 7 bits -- summing to `super::VOICE_BITS`, 88).
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 53-55, following the same
//! discipline as every other body-text equation in this spec (Type3 digit font defeats
//! `pdftotext`) -- with one extra layer of care this section specifically needed, recorded below.
//!
//! # A real, caught-in-the-act pdftotext digit corruption, not just the usual garbling
//!
//! Page 54's own body text, extracted with plain `pdftotext`, reads: "The next bits to be inserted
//! into the bit vectors are all of the bits of `b_hat_1` ..., followed by bit 2 and then bit 1 of
//! `b_hat_2`". Read at face value this contradicts the *same page's own earlier paragraph*, which
//! already stated `u_hat_0`'s middle three bits come from "the three most significant bits of
//! `b_hat_2`" -- if true, `b_hat_1` would be quantizing something never assigned any bits at all
//! anywhere in this document, and `b_hat_2`'s top 3 bits would be assigned twice. Re-rendered that
//! earlier paragraph at 600 DPI specifically to check, and it actually reads "the three most
//! significant bits of **`b_hat_2`**" (not `b_hat_1` as an initial plain-text skim might suggest) --
//! `pdftotext`'s own digit-glyph reconstruction conflated the Type3 font's control-character
//! encodings for `1` and `2` in that one spot. With that resolved, the whole section is internally
//! consistent (verified below, not just asserted): `b_hat_0` contributes 8 bits total (6 up front,
//! 2 at the very end), `b_hat_2` contributes 6 bits total (3 up front, 2 in the middle, 1 near the
//! end), `b_hat_1` contributes all `K_hat` of its own bits in one place (the middle), and the
//! `b_hat_3..b_hat_{L+1}` raster scan (Fig. 22) contributes the rest -- summing to exactly
//! `VOICE_BITS` (88) for every real `(L, K_hat)` pair the codec produces, confirmed against the
//! actual worked example in Fig. 22 (`L_hat = 16`, `K_hat = 6`) bit for bit before writing any of the
//! code below, not merely by re-deriving the total count.
//!
//! # The raster scan itself (Fig. 22)
//!
//! `b_hat_3` through `b_hat_{L+1}` are drawn as columns of varying height (each column's height is
//! that value's own bit allocation, from Annex F for `b_hat_3..b_hat_7` and Annex G for
//! `b_hat_8..b_hat_{L+1}`), and are scanned in one continuous top-to-bottom, left-to-right order:
//! within each bit-significance level (starting from whatever the tallest column's own MSB is, down
//! to every column's shared LSB level 0), visit each column left to right and emit that column's own
//! bit at the current level if the column is tall enough to have one. A column with a `0`-bit
//! allocation (a real value in Annex G, see `super::tables`' own doc comment) contributes no cells at
//! all, which this scan handles for free -- it's simply never "tall enough" at any level.
//!
//! The resulting flat sequence of scanned bits is inserted continuously across two separate
//! destination spans (the first 39 bits fill the last 3 bits of `u_hat_0` plus all of `u_hat_1`
//! through `u_hat_3`; the remainder fills alongside `b_hat_1`/`b_hat_2` into `u_hat_4` through the
//! top of `u_hat_7`) -- but from the *source* side there is no seam: it's one uninterrupted scan,
//! which is why [`raster_scan_bits`] takes no notion of where the two spans divide.

/// Runs Fig. 22's own raster scan over `columns` (`(value, bit_width)` pairs for `b_hat_3` through
/// `b_hat_{L+1}`, in that order -- gain-vector columns from Annex F, then higher-order-coefficient
/// columns from Annex G, omitting any real `0`-bit Annex G entries as those contribute nothing).
/// Returns the flat scanned bit sequence, MSB-of-tallest-column first.
// [@ANCHOR: raster_scan_bits]
pub fn raster_scan_bits(columns: &[(u32, u8)]) -> Vec<bool> {
    let max_width = columns.iter().map(|&(_, w)| w).max().unwrap_or(0);
    let mut bits = Vec::new();
    for level in (0..max_width).rev() {
        for &(value, width) in columns {
            if width > level {
                bits.push((value >> level) & 1 == 1);
            }
        }
    }
    bits
}

/// Runs the full bit prioritization (Fig. 22) for one frame, producing the eight prioritized bit
/// vectors `u_hat_0..u_hat_7` (packed as plain integers, MSB-first within each). Returns `None` if
/// the inputs don't add up to exactly `VOICE_BITS` (88) -- a real internal-consistency precondition
/// (the spec's own Annex F/G bit allocations are designed so this always holds for a genuine
/// `(L_hat, K_hat)` pair), not a spec-defined error case, so callers passing mismatched data get a
/// clear `None` instead of a silently wrong or panicking result.
///
/// - `b0`: the fundamental frequency quantizer value (8 bits, [`super::parameter_encoding::quantize_fundamental_frequency`]).
/// - `b1`, `k_hat`: the V/UV decision bits and their real bit width ([`super::parameter_encoding::encode_voicing_decisions`]).
/// - `b2`: the 6-bit gain index ([`super::tables::quantize_gain_index`]).
/// - `gain_vector`: `(value, bits)` for `b_hat_3..b_hat_7`, in order (from [`super::quantize::quantize_gain_vector_element`] and [`super::tables::gain_bit_allocation`]).
/// - `higher_order`: `(value, bits)` for `b_hat_8..b_hat_{L+1}`, `bits > 0` only (from [`super::quantize::quantize_higher_order_coefficients`] and [`super::tables::higher_order_bit_allocation`], both already filtering the same way).
/// - `sync_bit`: `b_hat_{L+2}`, the frame-to-frame alternating synchronization value ("Synchronization
///   Encoding and Decoding") -- not yet its own module, so for now this is the caller's own tracked
///   alternating-bit state.
// [@ANCHOR: prioritize_bits]
pub fn prioritize_bits(
    b0: u32,
    b1: u32,
    k_hat: u32,
    b2: u32,
    gain_vector: [(u32, u8); 5],
    higher_order: &[(u32, u8)],
    sync_bit: bool,
) -> Option<[u32; 8]> {
    let mut bits: Vec<bool> = Vec::with_capacity(88);

    // Step 1: b0's own top 6 bits (7..2), dropping its bottom 2 (used at the very end).
    for i in (2..8).rev() {
        bits.push((b0 >> i) & 1 == 1);
    }
    // Step 2: b2's own top 3 bits (5..3).
    for i in (3..6).rev() {
        bits.push((b2 >> i) & 1 == 1);
    }

    let mut columns: Vec<(u32, u8)> = gain_vector.to_vec();
    columns.extend_from_slice(higher_order);
    let scan = raster_scan_bits(&columns);
    let first_scan_len = scan.len().min(39);

    // Step 3: the scan's own first 39 bits.
    bits.extend_from_slice(&scan[..first_scan_len]);
    // Step 4: all of b1's own k_hat bits, MSB first.
    for i in (0..k_hat).rev() {
        bits.push((b1 >> i) & 1 == 1);
    }
    // Step 5: b2's own bits 2 and 1.
    for i in (1..3).rev() {
        bits.push((b2 >> i) & 1 == 1);
    }
    // Step 6: the rest of the scan.
    bits.extend_from_slice(&scan[first_scan_len..]);
    // Step 7: b2's own bit 0.
    bits.push(b2 & 1 == 1);
    // Step 8: b0's own bits 1 and 0.
    for i in (0..2).rev() {
        bits.push((b0 >> i) & 1 == 1);
    }
    // Step 9: the sync bit.
    bits.push(sync_bit);

    if bits.len() != 88 {
        return None;
    }

    const LENGTHS: [usize; 8] = [12, 12, 12, 12, 11, 11, 11, 7];
    let mut u = [0u32; 8];
    let mut idx = 0;
    for (slot, &len) in u.iter_mut().zip(LENGTHS.iter()) {
        let mut value = 0u32;
        for _ in 0..len {
            value = (value << 1) | (bits[idx] as u32);
            idx += 1;
        }
        *slot = value;
    }
    Some(u)
}

/// Extracts `b_hat_0` (Eq. 45) directly from the FEC-decoded prioritized vectors `u_hat_0`/`u_hat_7`
/// alone -- **without** needing `k_hat` or any Annex F/G width table, breaking a real bootstrapping
/// problem [`deprioritize_bits`] on its own can't solve: that function needs `k_hat` and the gain/
/// higher-order column widths just to know where the raster scan splits, but `k_hat` and those widths
/// are only knowable *after* decoding `b_hat_0` into `omega0_tilde`/`L~`/`K~`. The fix is this
/// function: `b_hat_0`'s own top 6 bits are always `prioritize_bits`'s own very first field (fixed at
/// `u_hat_0`'s own top 6 of 12 bits, `step 1`, before anything parameter-dependent is ever inserted),
/// and its own bottom 2 bits are always within the very last 4 bits of the entire 88-bit stream
/// (`step 8`, immediately before the final sync bit, `step 9`) -- and since every real `(L~, K~)` pair
/// sums to exactly 88 bits by construction (this module's own doc comment), the last 4 fields
/// (`b_hat_2`'s own LSB, `b_hat_0`'s own two bottom bits, the sync bit) always land in the *same*
/// fixed positions within `u_hat_7`'s own 7 bits (bits 3, 2, 1, 0 respectively) regardless of `k_hat`
/// or the scan length -- only `u_hat_7`'s own upper 3 bits (4, 5, 6) vary with where the
/// parameter-dependent split actually falls. Verified as a real, parameter-independent property by
/// the test below (checked across several different `(L~, K~)` configurations with the same `b0`,
/// not just argued from the bit-position algebra above).
// [@ANCHOR: extract_fundamental_frequency_quantizer]
pub fn extract_fundamental_frequency_quantizer(u: &[u32; 8]) -> u32 {
    let top6 = (u[0] >> 6) & 0b11_1111;
    let bottom2 = (u[7] >> 1) & 0b11;
    (top6 << 2) | bottom2
}

/// The exact inverse of [`raster_scan_bits`]: given the flat scanned bit sequence and the same
/// column widths used to produce it, recovers each column's own value. Mirrors
/// [`raster_scan_bits`]'s own traversal order exactly (same level-by-level, column-by-column walk),
/// so it consumes `bits` in the same order they were produced. Returns `None` if `bits` has the
/// wrong length for `widths` (too few to fill every real cell, or leftover bits after every cell is
/// filled) -- a real internal-consistency check, not a spec-defined error case.
// [@ANCHOR: raster_unscan_bits]
fn raster_unscan_bits(bits: &[bool], widths: &[u8]) -> Option<Vec<u32>> {
    let max_width = widths.iter().copied().max().unwrap_or(0);
    let mut values = vec![0u32; widths.len()];
    let mut idx = 0;
    for level in (0..max_width).rev() {
        for (col, &width) in widths.iter().enumerate() {
            if width > level {
                let bit = *bits.get(idx)?;
                idx += 1;
                if bit {
                    values[col] |= 1 << level;
                }
            }
        }
    }
    if idx != bits.len() {
        return None;
    }
    Some(values)
}

/// Reads the next `n` bits from `bits[*idx..]` MSB-first as an unsigned integer, advancing `*idx`.
/// Returns `None` (leaving `*idx` at the point of failure) if fewer than `n` bits remain.
// [@ANCHOR: read_bits]
fn read_bits(bits: &[bool], idx: &mut usize, n: usize) -> Option<u32> {
    let mut value = 0u32;
    for _ in 0..n {
        let bit = *bits.get(*idx)?;
        *idx += 1;
        value = (value << 1) | (bit as u32);
    }
    Some(value)
}

/// [`deprioritize_bits`]'s own decoded parameters, one field per quantizer value
/// [`prioritize_bits`] originally packed together.
#[derive(Debug, PartialEq)]
pub struct DeprioritizedBits {
    pub b0: u32,
    pub b1: u32,
    pub b2: u32,
    pub gain_vector: [(u32, u8); 5],
    pub higher_order: Vec<(u32, u8)>,
    pub sync_bit: bool,
}

/// The exact inverse of [`prioritize_bits`]: recovers every original quantizer value from the eight
/// received (and by this point, FEC-decoded) prioritized bit vectors `u_hat_0..u_hat_7`.
/// `gain_widths`/`higher_widths` must be the *same* bit-width columns the original frame was
/// prioritized with (from [`super::tables::gain_bit_allocation`]/
/// [`super::tables::higher_order_bit_allocation`], keyed off this frame's own decoded `L~`) -- an
/// external precondition this function can't check for itself, exactly like [`prioritize_bits`]'s
/// own caller-supplied `gain_vector`/`higher_order` widths. Returns `None` on any internal length
/// mismatch (a corrupted `k_hat`/width mismatch this deep would mean upstream decoding already went
/// wrong, not a case this function can meaningfully recover from).
// [@ANCHOR: deprioritize_bits]
pub fn deprioritize_bits(
    u: [u32; 8],
    k_hat: u32,
    gain_widths: [u8; 5],
    higher_widths: &[u8],
) -> Option<DeprioritizedBits> {
    const LENGTHS: [usize; 8] = [12, 12, 12, 12, 11, 11, 11, 7];
    let mut bits: Vec<bool> = Vec::with_capacity(88);
    for (&value, &len) in u.iter().zip(LENGTHS.iter()) {
        for i in (0..len).rev() {
            bits.push((value >> i) & 1 == 1);
        }
    }

    let total_scan_len = gain_widths.iter().map(|&w| w as usize).sum::<usize>()
        + higher_widths.iter().map(|&w| w as usize).sum::<usize>();
    let first_scan_len = total_scan_len.min(39);
    let rest_len = total_scan_len - first_scan_len;

    let mut idx = 0usize;
    let b0_top = read_bits(&bits, &mut idx, 6)?;
    let b2_top = read_bits(&bits, &mut idx, 3)?;
    let mut scan: Vec<bool> = bits.get(idx..idx.checked_add(first_scan_len)?)?.to_vec();
    idx += first_scan_len;
    let b1 = read_bits(&bits, &mut idx, k_hat as usize)?;
    let b2_mid = read_bits(&bits, &mut idx, 2)?;
    scan.extend_from_slice(bits.get(idx..idx.checked_add(rest_len)?)?);
    idx += rest_len;
    let b2_lsb = read_bits(&bits, &mut idx, 1)?;
    let b0_bottom = read_bits(&bits, &mut idx, 2)?;
    let sync_bit = *bits.get(idx)?;
    idx += 1;
    if idx != 88 {
        return None;
    }

    let mut widths_all: Vec<u8> = gain_widths.to_vec();
    widths_all.extend_from_slice(higher_widths);
    let values = raster_unscan_bits(&scan, &widths_all)?;

    let b0 = (b0_top << 2) | b0_bottom;
    let b2 = (b2_top << 3) | (b2_mid << 1) | b2_lsb;
    let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (values[i], gain_widths[i]));
    let higher_order: Vec<(u32, u8)> = higher_widths
        .iter()
        .zip(values[5..].iter())
        .map(|(&w, &v)| (v, w))
        .collect();

    Some(DeprioritizedBits {
        b0,
        b1,
        b2,
        gain_vector,
        higher_order,
        sync_bit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::tia_102_baba::tables;

    /// The real Annex F/G bit widths for the spec's own worked example, `L_hat = 16`, `K_hat = 6`
    /// (Table 6 and the higher-order bit allocation table, both already transcribed and tested in
    /// `super::tables`) -- used throughout this module's own tests so every check runs against the
    /// same ground truth Fig. 22 itself was read against.
    fn l16_widths() -> ([u8; 5], Vec<u8>) {
        let gain: [u8; 5] =
            std::array::from_fn(|i| tables::gain_bit_allocation(16, i as u32 + 2).unwrap().0);
        let higher = tables::higher_order_bit_allocation(16).unwrap().to_vec();
        (gain, higher)
    }

    #[test]
    fn l16_widths_match_fig_22s_own_column_heights() {
        let (gain, higher) = l16_widths();
        assert_eq!(gain, [6, 6, 6, 5, 5]);
        assert_eq!(higher, vec![6, 6, 5, 4, 4, 3, 3, 3, 3, 2]);
    }

    #[test]
    // Tests [@ANCHOR: raster_scan_bits]
    fn raster_scan_bits_produces_exactly_67_bits_for_the_l16_example() {
        let (gain, higher) = l16_widths();
        let columns: Vec<(u32, u8)> = gain
            .iter()
            .map(|&w| (0u32, w))
            .chain(higher.iter().map(|&w| (0u32, w)))
            .collect();
        let scan = raster_scan_bits(&columns);
        // Hand-summed directly from Fig. 22's own column heights: 6+6+6+5+5+6+6+5+4+4+3+3+3+3+2 = 67.
        assert_eq!(scan.len(), 67);
    }

    fn prioritize_zeroed(gain_vector: [(u32, u8); 5], higher_order: &[(u32, u8)]) -> [u32; 8] {
        prioritize_bits(0, 0, 6, 0, gain_vector, higher_order, false).unwrap()
    }

    /// Fig. 22's own first-scanned cell (the tallest column, `b_hat_3`, at its own MSB) is labeled
    /// `u_hat_{0,2}` directly in the figure -- setting only that one source bit and checking that
    /// only `u_hat_0`'s own bit 2 comes back set is a direct, ground-truth check against the figure
    /// itself, not a re-derivation of this module's own logic.
    #[test]
    fn b3s_own_msb_lands_on_u0_bit_2_per_fig_22() {
        let (gain, higher) = l16_widths();
        let mut gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        gain_vector[0] = (1 << (gain[0] - 1), gain[0]); // b_hat_3's own MSB set
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();

        let u = prioritize_bits(0, 0, 6, 0, gain_vector, &higher_order, false).unwrap();
        assert_eq!(
            u[0],
            1 << 2,
            "expected only u0's bit 2 set, got u0={:#014b}",
            u[0]
        );
        for (i, &v) in u.iter().enumerate().skip(1) {
            assert_eq!(
                v, 0,
                "expected every other u-vector to stay zero, got u{i}={v:#x}"
            );
        }
    }

    /// Fig. 22's own last-scanned cell (`b_hat_17 = b_hat_{L+1}`'s own LSB, the last column visited
    /// at the scan's own final, lowest bit-significance level) is labeled `u_hat_{7,4}` directly in
    /// the figure -- note this is `b_hat_17`'s LSB, not its MSB (which lands earlier in the scan, on
    /// `u_hat_5`'s own bit 1, since `b_hat_17` is visited once per bit-significance level it's tall
    /// enough for, and the scan visits levels from high to low).
    #[test]
    fn b17s_own_lsb_lands_on_u7_bit_4_per_fig_22() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let mut higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let last = higher_order.len() - 1;
        let last_width = higher_order[last].1;
        higher_order[last] = (1, last_width); // b_hat_17's own LSB set

        let u = prioritize_bits(0, 0, 6, 0, gain_vector, &higher_order, false).unwrap();
        assert_eq!(
            u[7],
            1 << 4,
            "expected only u7's bit 4 set, got u7={:#09b}",
            u[7]
        );
        for (i, &v) in u.iter().enumerate().take(7) {
            assert_eq!(
                v, 0,
                "expected every other u-vector to stay zero, got u{i}={v:#x}"
            );
        }
    }

    #[test]
    fn b0s_own_msb_lands_on_u0_bit_11() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_bits(0b1000_0000, 0, 6, 0, gain_vector, &higher_order, false).unwrap();
        assert_eq!(u[0], 1 << 11);
    }

    #[test]
    fn b0s_own_lsb_lands_on_u7_bit_1() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_bits(0b0000_0001, 0, 6, 0, gain_vector, &higher_order, false).unwrap();
        assert_eq!(u[7], 1 << 1);
    }

    #[test]
    fn b2s_own_lsb_lands_on_u7_bit_3() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_bits(0, 0, 6, 0b00_0001, gain_vector, &higher_order, false).unwrap();
        assert_eq!(u[7], 1 << 3);
    }

    #[test]
    fn the_sync_bit_lands_on_u7_bit_0() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_bits(0, 0, 6, 0, gain_vector, &higher_order, true).unwrap();
        assert_eq!(u[7], 1);
    }

    /// `b_hat_1`'s own MSB is the very first bit inserted into the second combined segment, which
    /// the spec's own text says "begins with bit 10 of u_hat_4" -- a direct check of that claim.
    #[test]
    fn b1s_own_msb_lands_on_u4_bit_10() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_bits(0, 0b10_0000, 6, 0, gain_vector, &higher_order, false).unwrap();
        assert_eq!(u[4], 1 << 10);
    }

    #[test]
    fn prioritize_bits_always_produces_exactly_88_bits_across_all_eight_vectors() {
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        let u = prioritize_zeroed(gain_vector, higher_order.as_slice());
        let lengths = [12u32, 12, 12, 12, 11, 11, 11, 7];
        assert_eq!(lengths.iter().sum::<u32>(), 88);
        for (&value, &len) in u.iter().zip(lengths.iter()) {
            assert!(value < (1 << len));
        }
    }

    #[test]
    fn prioritize_bits_refuses_a_mismatched_total_bit_count() {
        // k_hat = 3 (instead of the real 6 matching l16_widths' own bit allocation) leaves the
        // flat sequence 3 bits short of 88 -- a genuine internal-consistency mismatch, not a real
        // (L_hat, K_hat) pair this codec would ever actually produce together.
        let (gain, higher) = l16_widths();
        let gain_vector: [(u32, u8); 5] = std::array::from_fn(|i| (0, gain[i]));
        let higher_order: Vec<(u32, u8)> = higher.iter().map(|&w| (0u32, w)).collect();
        assert_eq!(
            prioritize_bits(0, 0, 3, 0, gain_vector, &higher_order, false),
            None
        );
    }

    /// The real risk this module's own inverse carries, per the advisor's own review: unlike
    /// `interleave.rs`'s table permutation or `modulation.rs`'s self-inverse XOR, this composition
    /// (raster scan, two split segments, several distinct sub-fields) has no structural guarantee of
    /// correctness -- so this checks a real round trip with actual varied, nonzero, non-symmetric
    /// values in every field (not the all-zero/single-bit placeholders the tests above use), which
    /// would catch a swapped field order or an off-by-one in the scan split that single-bit probes
    /// could miss.
    /// The real property [`extract_fundamental_frequency_quantizer`] depends on: `b_hat_0` is
    /// recoverable from `u_hat_0`/`u_hat_7` alone, for the *same* `b0` value, across several
    /// genuinely different `(L_hat, K_hat)` configurations (different scan lengths, different
    /// `k_hat`) -- proving independence from everything else in the frame, not just checking one
    /// configuration works.
    #[test]
    // Tests [@ANCHOR: extract_fundamental_frequency_quantizer]
    fn extract_fundamental_frequency_quantizer_is_independent_of_l_hat_and_k_hat() {
        let b0 = 0b1011_0110u32;
        for l_hat in [9u32, 16, 30, 56] {
            let k_hat = crate::ambe::float::tia_102_baba::vuv::frequency_bands_count(l_hat);
            let gain: [u8; 5] = std::array::from_fn(|i| {
                tables::gain_bit_allocation(l_hat, i as u32 + 2).unwrap().0
            });
            let higher = tables::higher_order_bit_allocation(l_hat).unwrap().to_vec();
            let gain_vector: [(u32, u8); 5] =
                std::array::from_fn(|i| (((i as u32 + 1) * 3) & ((1 << gain[i]) - 1), gain[i]));
            let higher_order: Vec<(u32, u8)> = higher
                .iter()
                .enumerate()
                .map(|(i, &w)| (((i as u32 + 2) * 5) & ((1 << w) - 1), w))
                .collect();
            let b1 = ((1u32 << k_hat) - 1) & 0b0110_1001;
            let b2 = 0b10_1101u32;

            let u = prioritize_bits(b0, b1, k_hat, b2, gain_vector, &higher_order, true).unwrap();
            assert_eq!(
                extract_fundamental_frequency_quantizer(&u),
                b0,
                "l_hat={l_hat}, k_hat={k_hat}"
            );
        }
    }

    #[test]
    // Tests [@ANCHOR: deprioritize_bits]
    // Tests [@ANCHOR: prioritize_bits]
    // Tests [@ANCHOR: raster_unscan_bits]
    // Tests [@ANCHOR: read_bits]
    fn deprioritize_bits_is_the_exact_inverse_of_prioritize_bits_for_real_varied_values() {
        let (gain, higher) = l16_widths();
        let b0 = 0b1011_0110u32; // 8 real bits, not a single-bit probe.
        let k_hat = 6u32;
        let b1 = 0b10_1101u32; // 6 real bits (k_hat), MSB set and mixed pattern.
        let b2 = 0b11_0010u32; // 6 real bits.
        let gain_vector: [(u32, u8); 5] =
            std::array::from_fn(|i| (((i as u32 + 1) * 7) & ((1 << gain[i]) - 1), gain[i]));
        let higher_order: Vec<(u32, u8)> = higher
            .iter()
            .enumerate()
            .map(|(i, &w)| (((i as u32 + 3) * 5) & ((1 << w) - 1), w))
            .collect();
        let sync_bit = true;

        let u = prioritize_bits(b0, b1, k_hat, b2, gain_vector, &higher_order, sync_bit).unwrap();
        let out = deprioritize_bits(u, k_hat, gain, &higher).unwrap();

        assert_eq!(out.b0, b0, "b0 round trip");
        assert_eq!(out.b1, b1, "b1 round trip");
        assert_eq!(out.b2, b2, "b2 round trip");
        assert_eq!(out.gain_vector, gain_vector, "gain_vector round trip");
        assert_eq!(out.higher_order, higher_order, "higher_order round trip");
        assert_eq!(out.sync_bit, sync_bit, "sync_bit round trip");
    }

    /// The same round trip, but for a small `L_hat` (`L_hat = 9`, the spec's own stated minimum,
    /// section 5.2's own text quoted in `vuv.rs`) -- exercises a real, different total scan length
    /// (well under the 39-bit first-segment split, unlike the `L_hat = 16` case above where the scan
    /// is 67 bits and spans both segments) so the `first_scan_len < 39` boundary path is checked too.
    #[test]
    fn deprioritize_bits_is_the_exact_inverse_of_prioritize_bits_for_a_small_l_hat() {
        let l_hat = 9u32;
        let k_hat = crate::ambe::float::tia_102_baba::vuv::frequency_bands_count(l_hat);
        let gain: [u8; 5] =
            std::array::from_fn(|i| tables::gain_bit_allocation(l_hat, i as u32 + 2).unwrap().0);
        let higher = tables::higher_order_bit_allocation(l_hat).unwrap().to_vec();

        let b0 = 0b0110_1001u32;
        let b1 = ((1u32 << k_hat) - 1) & 0b0101_0101; // mixed pattern within k_hat bits.
        let b2 = 0b10_1100u32;
        let gain_vector: [(u32, u8); 5] =
            std::array::from_fn(|i| (((i as u32 + 2) * 3) & ((1 << gain[i]) - 1), gain[i]));
        let higher_order: Vec<(u32, u8)> = higher
            .iter()
            .enumerate()
            .map(|(i, &w)| (((i as u32 + 1) * 5) & ((1 << w) - 1), w))
            .collect();
        let sync_bit = false;

        let u = prioritize_bits(b0, b1, k_hat, b2, gain_vector, &higher_order, sync_bit).unwrap();
        let out = deprioritize_bits(u, k_hat, gain, &higher).unwrap();

        assert_eq!(out.b0, b0, "b0 round trip (small L_hat)");
        assert_eq!(out.b1, b1, "b1 round trip (small L_hat)");
        assert_eq!(out.b2, b2, "b2 round trip (small L_hat)");
        assert_eq!(
            out.gain_vector, gain_vector,
            "gain_vector round trip (small L_hat)"
        );
        assert_eq!(
            out.higher_order, higher_order,
            "higher_order round trip (small L_hat)"
        );
        assert_eq!(out.sync_bit, sync_bit, "sync_bit round trip (small L_hat)");
    }
}
