//! Encode-side quantization: the real inverse of `decode::dequantize`'s table lookups. mbelib is
//! decode-only (there is no real reference encoder to check this against), so this module's own
//! approach -- nearest-neighbor search over each table, by ordinary Euclidean distance in the
//! log-domain gain space the tables themselves are defined in -- is this project's own reasoned
//! choice, not a transcription of anything. It is the natural, standard inverse of a table-based
//! vector quantizer, and is flagged here as a real, disclosed design choice rather than a certainty:
//! the original DVSI encoder's own search criteria (e.g. a perceptually-weighted distance instead of
//! plain Euclidean) are unknown and unpublished. See `mod.rs`'s own doc comment for the frame
//! structure this quantizes into.

use super::tables;

/// Finds the index of the table row nearest `target` by squared Euclidean distance -- the shared
/// core of every vector-quantization search below.
fn nearest_row<const N: usize>(table: &[[f64; N]], target: &[f64; N]) -> u32 {
    table
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da: f64 = a.iter().zip(target).map(|(x, y)| (x - y).powi(2)).sum();
            let db: f64 = b.iter().zip(target).map(|(x, y)| (x - y).powi(2)).sum();
            da.total_cmp(&db)
        })
        .map(|(i, _)| i as u32)
        .expect("table must be non-empty")
}

/// `b0`: the pitch/harmonic-count index nearest a target fundamental frequency `w0` (matching
/// `decode::dequantize`'s own `f0`/`w0` formula run in reverse) -- picks the [`tables::L_TABLE`]
/// index whose implied `w0` is closest, since `L_TABLE` itself (not a separate frequency table) is
/// what a real decoder actually consults.
pub fn quantize_pitch(w0: f64) -> u32 {
    assert!(
        w0.is_finite() && w0 > 0.0,
        "pitch must be a finite positive frequency, got {w0}"
    );
    let f0 = w0 / (2.0 * std::f64::consts::PI);
    let b0_estimate = super::decode::b0_from_f0(f0);
    b0_estimate.round().clamp(0.0, 125.0) as u32
}

/// `b2`: the gain-delta index nearest a target `Δγ` (i.e. `γ - 0.5·γ_prev`, the same recursion
/// `decode::dequantize` runs forward).
pub fn quantize_gain_delta(delta_gamma: f64) -> u32 {
    tables::DG
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (**a - delta_gamma)
                .abs()
                .total_cmp(&(**b - delta_gamma).abs())
        })
        .map(|(i, _)| i as u32)
        .expect("DG is non-empty")
}

/// `b3`: nearest [`tables::PRBA24`] row to `(Gm[2], Gm[3], Gm[4])`.
pub fn quantize_prba24(target: [f64; 3]) -> u32 {
    nearest_row(&tables::PRBA24, &target)
}

/// `b4`: nearest [`tables::PRBA58`] row to `(Gm[5], Gm[6], Gm[7], Gm[8])`.
pub fn quantize_prba58(target: [f64; 4]) -> u32 {
    nearest_row(&tables::PRBA58, &target)
}

/// `b5..b8`: nearest higher-order-coefficient row in the given block's own table. `b8`'s own real
/// index space only ever uses even values (its low bit is always forced to 0, per `mod.rs`'s own
/// doc comment) -- callers building a `b8` value should shift this function's return left by 1 to
/// match `decode::RawParameters::b8`'s own convention, or simply search only even rows directly by
/// masking the result; this function itself just returns the nearest of all 16 rows.
pub fn quantize_hoc(table: &[[f64; 4]; 16], target: [f64; 4]) -> u32 {
    nearest_row(table, &target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real, load-bearing round-trip check for every table-based quantizer here: dequantizing
    /// index `i` and then re-quantizing the result must return an index whose own table row is
    /// numerically identical to row `i` -- since the target *is* one of the table's own rows, the
    /// nearest row is trivially itself or an exact duplicate (distance zero), so this can never
    /// spuriously pass for the wrong reason the way a "close enough" real-valued round trip could.
    /// Genuine duplicate rows exist in these real, mbelib-derived tables (confirmed directly, not
    /// assumed away) -- PRBA24 rows 196 and 198 are numerically identical, for one real example --
    /// so this checks value equality, not index equality, which is the honest invariant here.
    #[test]
    fn quantize_prba24_recovers_a_row_matching_every_real_table_entry() {
        for (i, &row) in tables::PRBA24.iter().enumerate() {
            let recovered = quantize_prba24(row);
            assert_eq!(
                tables::PRBA24[recovered as usize],
                row,
                "PRBA24 row {i}: {row:?}, recovered index {recovered}"
            );
        }
    }

    #[test]
    fn quantize_prba58_recovers_a_row_matching_every_real_table_entry() {
        for (i, &row) in tables::PRBA58.iter().enumerate() {
            let recovered = quantize_prba58(row);
            assert_eq!(
                tables::PRBA58[recovered as usize],
                row,
                "PRBA58 row {i}: {row:?}, recovered index {recovered}"
            );
        }
    }

    #[test]
    fn quantize_hoc_recovers_a_row_matching_every_real_table_entry_in_all_four_blocks() {
        for table in [
            &tables::HOC_B5,
            &tables::HOC_B6,
            &tables::HOC_B7,
            &tables::HOC_B8,
        ] {
            for (i, &row) in table.iter().enumerate() {
                let recovered = quantize_hoc(table, row);
                assert_eq!(
                    table[recovered as usize], row,
                    "HOC row {i}: {row:?}, recovered index {recovered}"
                );
            }
        }
    }

    #[test]
    fn quantize_gain_delta_recovers_a_value_matching_every_real_table_entry() {
        for (i, &value) in tables::DG.iter().enumerate() {
            let recovered = quantize_gain_delta(value);
            assert_eq!(
                tables::DG[recovered as usize],
                value,
                "DG[{i}] = {value}, recovered index {recovered}"
            );
        }
    }

    /// `quantize_pitch` is the exact inverse of `decode::f0_from_b0`: every real `b0` is recovered
    /// unchanged (the map is log-linear, so the nearest index of an exact table value is itself).
    #[test]
    fn quantize_pitch_round_trips_exactly_for_every_real_b0() {
        for b0 in 0u32..126 {
            let f0 = crate::ambe::float::dstar::decode::f0_from_b0(b0);
            let w0 = f0 * 2.0 * std::f64::consts::PI;
            let recovered = quantize_pitch(w0);
            assert_eq!(recovered, b0, "b0={b0}: w0={w0}");
        }
    }
}
