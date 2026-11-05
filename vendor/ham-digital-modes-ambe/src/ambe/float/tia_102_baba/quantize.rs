//! Block partitioning, per-block DCT, and quantization of the spectral amplitude prediction
//! residuals (TIA-102.BABA_2003.pdf section 6.3, Eq. 58-63) -- the last step before the resulting
//! bits get priority-scanned and error-protected (Fig. 22, [`super::fec`]).
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 42-46, following the same
//! discipline as every other body-text equation in this spec (Type3 digit font defeats
//! `pdftotext`). Reuses [`super::tables`]'s already-transcribed-and-verified Annexes E, F, G, and J
//! rather than re-deriving anything from the PDF a second time.

use std::f64::consts::PI;

use super::tables;

/// Splits `l` prediction residuals (`T_hat_1..T_hat_l`, from [`super::prediction`]) into six blocks
/// per Annex J's own lengths (Eq. 58-59: the six lengths sum to `l` and are non-decreasing
/// low-to-high frequency). Returns `None` for an `l` outside Annex J's tabulated range, matching
/// [`tables::block_lengths_for_l`]'s own "give up, don't guess" discipline.
// [@ANCHOR: partition_into_blocks]
pub fn partition_into_blocks(residuals: &[f64], l: u32) -> Option<[Vec<f64>; 6]> {
    let lengths = tables::block_lengths_for_l(l)?;
    if residuals.len() != l as usize {
        return None;
    }
    let mut blocks: [Vec<f64>; 6] = Default::default();
    let mut offset = 0usize;
    for (block, &len) in blocks.iter_mut().zip(lengths.iter()) {
        *block = residuals[offset..offset + len as usize].to_vec();
        offset += len as usize;
    }
    Some(blocks)
}

/// The per-block DCT (Eq. 60): transforms one block's own `c` values (length `J_i`) into `J_i` DCT
/// coefficients `C_i,k` for `1 <= k <= J_i` (returned 0-indexed, `result[0] == C_i,1`). The same
/// formula as [`super::gain_vector_dct`], generalized from the fixed `J=6` second-stage transform to
/// an arbitrary block length -- not shared code with it since that function's own fixed-size
/// `[f64; 6]` signature is a real, separate, already-tested public API this module doesn't need to
/// disturb.
// [@ANCHOR: block_dct]
pub fn block_dct(c: &[f64]) -> Vec<f64> {
    let j = c.len();
    if j == 0 {
        return Vec::new();
    }
    (1..=j)
        .map(|k| {
            let sum: f64 = c
                .iter()
                .enumerate()
                .map(|(idx, &c_ij)| {
                    let j_index = (idx + 1) as f64; // 1-indexed j, matches Eq. 60
                    c_ij * (PI * (k as f64 - 1.0) * (j_index - 0.5) / j as f64).cos()
                })
                .sum();
            sum / j as f64
        })
        .collect()
}

/// The shared saturating uniform quantizer both Eq. 62 (gain vector) and Eq. 63 (higher order DCT
/// coefficients) use -- textually identical in the spec (same three-branch floor/saturate shape,
/// only the symbol names differ), so implemented once rather than twice: `0` if the value floors
/// below `-2^(bits-1)`, `2^bits - 1` if it floors at or above `2^(bits-1)`, otherwise the floored,
/// zero-offset index.
// [@ANCHOR: saturating_uniform_quantize]
fn saturating_uniform_quantize(value: f64, bits: u8, step_size: f64) -> u32 {
    let half_range = 1i64 << (bits - 1); // 2^(bits-1)
    let idx = (value / step_size).floor() as i64;
    if idx < -half_range {
        0
    } else if idx >= half_range {
        ((1i64 << bits) - 1) as u32
    } else {
        (idx + half_range) as u32
    }
}

/// Quantizes one element of the gain vector's own remaining five elements (Eq. 62): `gain_value` is
/// `G_hat_element` for `element` in `2..=6` (the five non-DC gain vector elements from
/// [`super::gain_vector_dct`]).
///
/// **A real, worth-recording index-convention check**: Eq. 62's own formula subscripts its bits/step
/// parameters as `B_hat_m`/`Delta_hat_m` for `m` in `3..=7`, quantizing `G_hat_{m-1}`; it would be
/// easy to assume Annex F's table is indexed the same way and pass `m` (not `m-1`) here. It isn't --
/// confirmed directly against Annex F's own worked example (Table 6, `L_hat=16`): that table's own
/// `m` column runs `2..=6` and lists `G_hat_m` directly (not `G_hat_{m-1}`), and its five rows match
/// [`tables::gain_bit_allocation`]'s stored data for `L=16` exactly, entry for entry. So Annex F's
/// table -- and this function's own `element` parameter -- already use the gain-vector element index
/// directly, not Eq. 62's shifted `b_hat` output index; no off-by-one translation is needed here.
/// `l` and `element` (`2..=6`) select the bit allocation and step size from Annex F. Returns `None`
/// for an out-of-range `l`/`element` (matching [`tables::gain_bit_allocation`]'s own range).
pub fn quantize_gain_vector_element(gain_value: f64, l: u32, element: u32) -> Option<u32> {
    let (bits, step_size) = tables::gain_bit_allocation(l, element)?;
    Some(saturating_uniform_quantize(gain_value, bits, step_size))
}

/// Quantizes all five non-DC gain vector elements (`G_hat_2..G_hat_6`, `g_hat[1..6]`) into
/// `(value, bits)` pairs, ready for [`super::bit_prioritization::prioritize_bits`]'s own
/// `gain_vector` parameter -- pairing each quantized value with its own bit width by construction
/// (both drawn from the same [`tables::gain_bit_allocation`] call) rather than relying on a caller to
/// independently re-derive the matching bit width and keep it in sync.
// [@ANCHOR: quantize_gain_vector]
pub fn quantize_gain_vector(g_hat: &[f64; 6], l: u32) -> Option<[(u32, u8); 5]> {
    let mut out = [(0u32, 0u8); 5];
    for (idx, element) in (2..=6u32).enumerate() {
        let (bits, _) = tables::gain_bit_allocation(l, element)?;
        let value = quantize_gain_vector_element(g_hat[(element - 1) as usize], l, element)?;
        out[idx] = (value, bits);
    }
    Some(out)
}

/// The `(block_index, position)` pairs, `1 <= block_index <= 6` and `2 <= position <= J_i`, in the
/// same flat order Annex G's own bit-allocation table uses (the spec's own stated convention:
/// `[b_hat_8, ..., b_hat_{L+1}]` correspond to `[C_1,2, ..., C_1,J1, ..., C_6,2, ..., C_6,J6]`).
// [@ANCHOR: higher_order_coefficient_positions]
pub fn higher_order_coefficient_positions(l: u32) -> Option<Vec<(usize, usize)>> {
    let lengths = tables::block_lengths_for_l(l)?;
    let mut positions = Vec::new();
    for (idx, &j_i) in lengths.iter().enumerate() {
        for k in 2..=j_i {
            positions.push((idx, k as usize));
        }
    }
    Some(positions)
}

/// Quantizes every higher-order DCT coefficient across all six blocks (Eq. 63), given each block's
/// own [`block_dct`] output (`dct_blocks[i][k-1] == C_{i+1,k}`, 0-indexed per that function's own
/// return convention -- named `dct_blocks`, not `blocks`, precisely so it can't be confused with
/// [`partition_into_blocks`]'s own same-shaped `[Vec<f64>; 6]` of raw residuals, which this function
/// must never be called with directly) and `l`. Coefficients with a `0`-bit allocation (a real,
/// observed value in Annex G, not hypothetical) are skipped entirely -- a real, disclosed reading of
/// the spec's own text: "zero bits" means that coefficient is never transmitted, so it contributes no
/// entry to the returned vector, in the same flat `[C_1,2, ..., C_6,J6]` order
/// [`higher_order_coefficient_positions`] produces. Each returned `(value, bits)` pair carries its own
/// bit width alongside its quantized value -- both drawn from the same Annex G lookup here, by
/// construction, rather than leaving a caller to independently re-filter `higher_order_bit_allocation`
/// the same way and hope the two stay in lockstep (a real risk once this feeds
/// [`super::bit_prioritization::prioritize_bits`]'s own `higher_order` parameter, where a silent
/// misalignment would produce a plausible-looking but wrong frame). Returns `None` for an
/// out-of-range `l`.
// [@ANCHOR: quantize_higher_order_coefficients]
pub fn quantize_higher_order_coefficients(
    dct_blocks: &[Vec<f64>; 6],
    l: u32,
) -> Option<Vec<(u32, u8)>> {
    let bit_allocation = tables::higher_order_bit_allocation(l)?;
    let positions = higher_order_coefficient_positions(l)?;
    if bit_allocation.len() != positions.len() {
        return None; // a real invariant Annex G/J are supposed to guarantee together
    }
    Some(
        positions
            .iter()
            .zip(bit_allocation.iter())
            .filter_map(|(&(block_idx, k), &bits)| {
                if bits == 0 {
                    return None;
                }
                let sigma = tables::higher_order_coefficient_sigma(k as u32)?;
                let multiplier = tables::higher_order_step_multiplier(bits)?;
                let step_size = multiplier * sigma;
                let c_ik = *dct_blocks[block_idx].get(k - 1)?;
                Some((saturating_uniform_quantize(c_ik, bits, step_size), bits))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Tests [@ANCHOR: partition_into_blocks]
    fn partition_into_blocks_matches_annex_j_lengths_and_covers_every_residual() {
        let l = 20;
        let residuals: Vec<f64> = (0..l).map(|i| i as f64).collect();
        let blocks = partition_into_blocks(&residuals, l).unwrap();
        let lengths = tables::block_lengths_for_l(l).unwrap();
        for (block, &len) in blocks.iter().zip(lengths.iter()) {
            assert_eq!(block.len(), len as usize);
        }
        let total: usize = blocks.iter().map(|b| b.len()).sum();
        assert_eq!(total, l as usize);
        // The blocks are contiguous, in order -- reassembling them reproduces the input exactly.
        let reassembled: Vec<f64> = blocks.into_iter().flatten().collect();
        assert_eq!(reassembled, residuals);
    }

    #[test]
    fn partition_into_blocks_refuses_a_mismatched_residual_count() {
        assert_eq!(partition_into_blocks(&[1.0, 2.0, 3.0], 9), None);
    }

    #[test]
    // Tests [@ANCHOR: block_dct]
    fn block_dct_of_a_constant_block_is_zero_except_the_first_coefficient() {
        // The same real, checkable DC-only property mod.rs's own gain_vector_dct test already
        // leans on, here for an arbitrary block length instead of the fixed J=6 case.
        for &j in &[3usize, 5, 10] {
            let c = vec![2.5; j];
            let coeffs = block_dct(&c);
            assert_eq!(coeffs.len(), j);
            assert!(
                (coeffs[0] - 2.5).abs() < 1e-9,
                "J={j}: DC term should equal the constant input"
            );
            for (k, &coeff) in coeffs.iter().enumerate().skip(1) {
                assert!(coeff.abs() < 1e-9, "J={j}, k={k}: expected 0, got {coeff}");
            }
        }
    }

    #[test]
    // Tests [@ANCHOR: saturating_uniform_quantize]
    fn saturating_uniform_quantize_matches_eq62_63_in_all_three_branches() {
        let bits = 4u8;
        let step = 0.1;
        // Comfortably in range: idx = floor(0.25/0.1) = 2, offset by 2^(4-1)=8 -> 10.
        assert_eq!(saturating_uniform_quantize(0.25, bits, step), 10);
        // Saturates low: a very negative value floors far below -8.
        assert_eq!(saturating_uniform_quantize(-100.0, bits, step), 0);
        // Saturates high: a very positive value floors far above 8, clamped to 2^4 - 1 = 15.
        assert_eq!(saturating_uniform_quantize(100.0, bits, step), 15);
        // Exactly at the boundary idx == -2^(bits-1) is NOT the saturate-low branch (Eq. 62/63's
        // own strict "<"), so it lands on index 0 via the normal (idx + 2^(bits-1)) path too --
        // same output as the saturate branch here, but for a different reason worth a comment.
        assert_eq!(saturating_uniform_quantize(-0.8, bits, step), 0);
    }

    #[test]
    fn quantize_gain_vector_element_uses_annex_f_and_stays_in_range() {
        let l = 20;
        let element = 2;
        let (bits, _) = tables::gain_bit_allocation(l, element).unwrap();
        let idx = quantize_gain_vector_element(0.0, l, element).unwrap();
        assert!(
            idx < (1u32 << bits),
            "quantizer index must fit in {bits} bits, got {idx}"
        );
    }

    #[test]
    // Tests [@ANCHOR: quantize_higher_order_coefficients]
    // Tests [@ANCHOR: higher_order_coefficient_positions]
    fn quantize_higher_order_coefficients_skips_zero_bit_entries_and_stays_in_range() {
        let l = 32; // HIGHER_ORDER_BIT_ALLOCATION for L=32 includes a real 0-bit final entry
        let lengths = tables::block_lengths_for_l(l).unwrap();
        let blocks: [Vec<f64>; 6] = std::array::from_fn(|i| vec![0.05; lengths[i] as usize]);
        let bit_allocation = tables::higher_order_bit_allocation(l).unwrap();
        let zero_bit_count = bit_allocation.iter().filter(|&&b| b == 0).count();
        assert!(
            zero_bit_count > 0,
            "test assumes L=32 has a real 0-bit entry to skip"
        );

        let quantized = quantize_higher_order_coefficients(&blocks, l).unwrap();
        assert_eq!(quantized.len(), bit_allocation.len() - zero_bit_count);
        for &(idx, bits) in quantized.iter() {
            assert!(
                idx < (1u32 << bits),
                "index {idx} doesn't fit in {bits} bits"
            );
        }
    }

    /// The composition none of the per-function tests above can catch: wires the whole encoder
    /// parameter pipeline together end to end (`partition_into_blocks` -> `block_dct` per block ->
    /// extract each block's own DC term into `R_i` -> [`crate::ambe::float::tia_102_baba::gain_vector_dct`] -> both
    /// quantizers), using a constant residual input specifically because it produces a fully
    /// checkable expectation at every stage: a constant block's own DCT is zero except its DC term
    /// (already proven per-block above), so every higher-order coefficient and every non-DC
    /// gain-vector element should quantize to the uniform quantizer's own "zero" bin
    /// (`2^(bits-1)`, the zero-offset middle index) -- a real, structural check, not merely "runs
    /// without panicking".
    #[test]
    fn end_to_end_pipeline_with_a_constant_residual_input_quantizes_to_the_zero_bin() {
        let l = 20;
        let constant = 3.0;
        let residuals = vec![constant; l as usize];

        let blocks = partition_into_blocks(&residuals, l).unwrap();
        let dct_blocks: [Vec<f64>; 6] = std::array::from_fn(|i| block_dct(&blocks[i]));

        let r_hat: [f64; 6] = std::array::from_fn(|i| dct_blocks[i][0]);
        for &r in &r_hat {
            assert!(
                (r - constant).abs() < 1e-9,
                "expected each block's own DC term to equal the constant input, got {r}"
            );
        }

        let g_hat = crate::ambe::float::tia_102_baba::gain_vector_dct(&r_hat);
        assert!((g_hat[0] - constant).abs() < 1e-9);
        for &g in &g_hat[1..] {
            assert!(
                g.abs() < 1e-9,
                "expected every higher gain-vector term to vanish for a constant R_i, got {g}"
            );
        }

        // Real, found while writing this test, not hypothetical: `gain_vector_dct`'s own cosine
        // sum leaves values that are mathematically exactly zero as a tiny nonzero float (e.g.
        // -3e-16), well under the `1e-9` tolerance checked above -- but `saturating_uniform_
        // quantize`'s own `floor()` is exquisitely sensitive to that sign right at a bin boundary
        // (`floor(-1e-14)` is `-1`, not `0`), so the quantized index can land one bin below the
        // idealized "exact zero" bin depending on which way the roundoff fell. That's a real,
        // inherent property of floor-based uniform quantization at an exact bin boundary, not a
        // bug in the quantizer -- so this checks "at or one bin below the zero bin", not exact
        // equality.
        let gain_vector = quantize_gain_vector(&g_hat, l).unwrap();
        for &(idx, bits) in gain_vector.iter() {
            let zero_bin = 1u32 << (bits - 1);
            assert!(
                idx == zero_bin || idx == zero_bin - 1,
                "expected the zero bin ({zero_bin}) or one below it, got {idx}"
            );
        }

        let quantized = quantize_higher_order_coefficients(&dct_blocks, l).unwrap();
        for &(idx, bits) in quantized.iter() {
            let zero_bin = 1u32 << (bits - 1);
            assert!(
                idx == zero_bin || idx == zero_bin - 1,
                "expected the zero bin ({zero_bin}) or one below it for a zero-valued coefficient, got {idx}"
            );
        }
    }

    #[test]
    // Tests [@ANCHOR: quantize_gain_vector]
    fn quantize_gain_vector_matches_element_by_element_quantization() {
        let l = 20;
        let g_hat = [1.0, 0.1, -0.2, 0.05, -0.05, 0.02];
        let pairs = quantize_gain_vector(&g_hat, l).unwrap();
        for (idx, element) in (2..=6u32).enumerate() {
            let (bits, _) = tables::gain_bit_allocation(l, element).unwrap();
            let expected_value =
                quantize_gain_vector_element(g_hat[(element - 1) as usize], l, element).unwrap();
            assert_eq!(pairs[idx], (expected_value, bits));
        }
    }
}
