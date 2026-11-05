//! Quantizer and bit-allocation tables from TIA-102.BABA_2003.pdf's Annexes E and J. Unlike
//! `fec.rs`'s Golay/Hamming matrices, these are plain decimal numbers and small integers in the
//! spec's own text -- confirmed real, extractable PDF text (not a scanned raster) via `pdfimages
//! -list` (zero embedded images on these pages) exactly the same way `fec.rs` confirmed it for the
//! FEC matrix pages, so there is no pixel-level ambiguity risk transcribing them the way a bit
//! matrix carries. Each table below was parsed programmatically from the real `pdftotext` output
//! (not read digit-by-digit by eye) and checked against a real structural invariant before being
//! trusted -- see each table's own doc comment for what was checked.

/// The 6-bit non-uniform quantizer for the gain vector's first element (`G_hat_1`, the overall
/// level), Annex E ("Gain Quantizer Levels"). `b_hat_2` is the index of the value in this table
/// nearest to `G_hat_1` -- see TIA-102.BABA_2003.pdf section 6.3.1: "The 6 bit value `b_hat_2` is
/// defined as the index of the quantizer value... which is nearest to `G_hat_1`."
///
/// Parsed from the spec's own text and checked against a real structural invariant before being
/// trusted: a valid non-uniform quantizer's own level table must be monotonically increasing (each
/// entry strictly greater than the last) -- confirmed for all 64 entries (see
/// `gain_quantizer_levels_is_strictly_monotonically_increasing` below) -- and must have exactly one
/// entry for every index 0 through 63 with no gaps or duplicates (also confirmed during parsing,
/// before this array was ever written down).
pub const GAIN_QUANTIZER_LEVELS: [f64; 64] = [
    -2.842205, -2.694235, -2.55826, -2.38285, -2.221042, -2.095574, -1.980845, -1.836058,
    -1.645556, -1.417658, -1.261301, -1.125631, -0.958207, -0.781591, -0.555837, -0.346976,
    -0.147249, 0.027755, 0.211495, 0.38838, 0.552873, 0.737223, 0.932197, 1.139032, 1.320955,
    1.483433, 1.648297, 1.801447, 1.942731, 2.118613, 2.321486, 2.504443, 2.653909, 2.780654,
    2.925355, 3.07639, 3.220825, 3.402869, 3.585096, 3.784606, 3.955521, 4.155636, 4.314009,
    4.44415, 4.577542, 4.735552, 4.909493, 5.085264, 5.254767, 5.411894, 5.568094, 5.738523,
    5.919215, 6.087701, 6.280685, 6.464201, 6.647736, 6.834672, 7.022583, 7.211777, 7.471016,
    7.738948, 8.124863, 8.695827,
];

/// Finds the index of the [`GAIN_QUANTIZER_LEVELS`] entry nearest to `g_hat_1`, i.e. `b_hat_2` per
/// the spec's own definition (section 6.3.1).
// [@ANCHOR: quantize_gain_index]
pub fn quantize_gain_index(g_hat_1: f64) -> u8 {
    let mut best_idx = 0usize;
    let mut best_dist = f64::INFINITY;
    for (i, &level) in GAIN_QUANTIZER_LEVELS.iter().enumerate() {
        let dist = (level - g_hat_1).abs();
        if dist < best_dist {
            best_dist = dist;
            best_idx = i;
        }
    }
    best_idx as u8
}

/// Annex J ("Log Magnitude Prediction Residual Block Lengths"): for a given number of harmonics
/// `L` (9 to 56, the real range this codec operates over per the spec's own Annex tables), returns
/// the six block lengths `[J_1..J_6]` used to split the `L` prediction-residual values into six
/// blocks before the per-block DCT (`ambe::mod`'s own Fig. 17/18 pipeline documentation -- this
/// table is the concrete data that pipeline's `J_hat_i` lengths come from).
///
/// Returns `None` for `L` outside the spec's own tabulated 9..=56 range, rather than guessing or
/// extrapolating -- matching this codebase's own "give up, don't guess" discipline
/// (`AUTO_TUNE_AND_MODE_DETECTION.md`'s own SSB-deferral reasoning is the same shape of decision).
///
/// Checked, not merely transcribed: every row's six lengths must sum to exactly `L` (each of the
/// `L` prediction-residual values belongs to exactly one of the six blocks) -- confirmed for all 48
/// rows during parsing (see `block_lengths_always_sum_to_l` below), and the six lengths are always
/// non-decreasing left to right in every real spec row (low-frequency blocks are never longer than
/// higher-frequency ones) -- also confirmed for all 48 rows.
// [@ANCHOR: block_lengths_for_l]
pub fn block_lengths_for_l(l: u32) -> Option<[u32; 6]> {
    if !(9..=56).contains(&l) {
        return None;
    }
    Some(BLOCK_LENGTHS[(l - 9) as usize])
}

/// Annex F ("Bit Allocation and Step Size for Transformed Gain Vector"): for a given `L` (9..=56)
/// and gain-vector element index `m` (2..=6), returns `(bits, step_size)` -- the number of bits
/// `B_hat_m` and the uniform quantizer step size `Delta_hat_m` Eq. 62 needs to quantize `G_hat_m`.
/// Returns `None` outside the spec's own tabulated ranges, same "give up, don't guess" discipline as
/// [`block_lengths_for_l`].
///
/// **A real parsing complication, resolved by cross-checking rather than guessed past**: four of the
/// 48 rows (L=10, 11, 16, 17) have one entry's own `G_m`/`b_index` text label missing from the raw
/// extracted text -- confirmed directly (not assumed) to be a real PDF text-layer artifact where the
/// diagonal "Limited Use Only" watermark's own text objects interleave with and appear to have
/// displaced that one label per affected row, while the label's own adjacent numeric data (bit count,
/// step size) survives intact in the right column position. Recovered by elimination: since every
/// real row has exactly one entry for each `m` in 2..=6, the specific missing `m` for each of these
/// four rows is unambiguous (whichever of 2..=6 doesn't already have an entry from that same row's
/// other four, unambiguously-labeled columns), not guessed. Checked afterward against two real
/// structural invariants that hold across the entire table, not just the four recovered entries:
/// `bits` (`B_hat_m`) is non-increasing as `L` grows for fixed `m` (more harmonics to encode leaves
/// less bit budget per coefficient), and `step_size` is non-decreasing as `L` grows for fixed `m` --
/// both properties hold for all 48 rows and all 5 values of `m` (see the tests below), which the four
/// recovered entries would have been very unlikely to satisfy by coincidence if the elimination logic
/// had picked the wrong `m`.
// [@ANCHOR: gain_bit_allocation]
pub fn gain_bit_allocation(l: u32, m: u32) -> Option<(u8, f64)> {
    if !(9..=56).contains(&l) || !(2..=6).contains(&m) {
        return None;
    }
    Some(GAIN_BIT_ALLOCATION[(l - 9) as usize][(m - 2) as usize])
}

/// The `bits` half of [`gain_bit_allocation`] alone, as a pure-integer accessor -- needed by
/// `ambe::fixed::tia_102_baba::reconstruct`, which must never call any function that constructs an `f64`
/// value even transiently (Bruce's own "no floating point support whatsoever" requirement means no
/// FPU instruction should be reachable, not merely "the f64 result goes unused" -- a compiler is not
/// guaranteed to elide the construction just because a caller discards it). Same underlying table,
/// same validity range, just narrowed to the field the fixed-point port can actually touch.
pub fn gain_vector_bits(l: u32, m: u32) -> Option<u8> {
    if !(9..=56).contains(&l) || !(2..=6).contains(&m) {
        return None;
    }
    Some(GAIN_BIT_ALLOCATION[(l - 9) as usize][(m - 2) as usize].0)
}

const GAIN_BIT_ALLOCATION: [[(u8, f64); 5]; 48] = [
    [
        (10, 0.0031),
        (9, 0.00402),
        (9, 0.00336),
        (9, 0.0029),
        (9, 0.00264),
    ], // L=9
    [
        (9, 0.0062),
        (9, 0.00402),
        (8, 0.00672),
        (8, 0.0058),
        (8, 0.00528),
    ], // L=10
    [
        (8, 0.0124),
        (8, 0.00804),
        (8, 0.00672),
        (7, 0.0116),
        (7, 0.01056),
    ], // L=11
    [
        (8, 0.0124),
        (7, 0.01608),
        (7, 0.01344),
        (7, 0.0116),
        (7, 0.01056),
    ], // L=12
    [
        (7, 0.0248),
        (7, 0.01608),
        (7, 0.01344),
        (6, 0.02175),
        (6, 0.0198),
    ], // L=13
    [
        (7, 0.0248),
        (6, 0.03015),
        (6, 0.0252),
        (6, 0.02175),
        (6, 0.0198),
    ], // L=14
    [
        (7, 0.0248),
        (6, 0.03015),
        (6, 0.0252),
        (6, 0.02175),
        (5, 0.03696),
    ], // L=15
    [
        (6, 0.0465),
        (6, 0.03015),
        (6, 0.0252),
        (5, 0.0406),
        (5, 0.03696),
    ], // L=16
    [
        (6, 0.0465),
        (6, 0.03015),
        (5, 0.04704),
        (5, 0.0406),
        (5, 0.03696),
    ], // L=17
    [
        (6, 0.0465),
        (5, 0.05628),
        (5, 0.04704),
        (5, 0.0406),
        (5, 0.03696),
    ], // L=18
    [
        (6, 0.0465),
        (5, 0.05628),
        (5, 0.04704),
        (4, 0.058),
        (4, 0.0528),
    ], // L=19
    [
        (6, 0.0465),
        (5, 0.05628),
        (5, 0.04704),
        (4, 0.058),
        (4, 0.0528),
    ], // L=20
    [
        (5, 0.0868),
        (5, 0.05628),
        (5, 0.04704),
        (4, 0.058),
        (4, 0.0528),
    ], // L=21
    [
        (5, 0.0868),
        (5, 0.05628),
        (4, 0.0672),
        (4, 0.058),
        (4, 0.0528),
    ], // L=22
    [
        (5, 0.0868),
        (4, 0.0804),
        (4, 0.0672),
        (4, 0.058),
        (4, 0.0528),
    ], // L=23
    [
        (5, 0.0868),
        (4, 0.0804),
        (4, 0.0672),
        (4, 0.058),
        (4, 0.0528),
    ], // L=24
    [
        (5, 0.0868),
        (4, 0.0804),
        (4, 0.0672),
        (4, 0.058),
        (3, 0.0858),
    ], // L=25
    [
        (5, 0.0868),
        (4, 0.0804),
        (4, 0.0672),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=26
    [
        (5, 0.0868),
        (4, 0.0804),
        (4, 0.0672),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=27
    [
        (4, 0.124),
        (4, 0.0804),
        (4, 0.0672),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=28
    [
        (4, 0.124),
        (4, 0.0804),
        (4, 0.0672),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=29
    [
        (4, 0.124),
        (4, 0.0804),
        (4, 0.0672),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=30
    [
        (4, 0.124),
        (4, 0.0804),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=31
    [
        (4, 0.124),
        (4, 0.0804),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=32
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=33
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=34
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=35
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (3, 0.0858),
    ], // L=36
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (2, 0.1122),
    ], // L=37
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (2, 0.1122),
    ], // L=38
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (2, 0.1122),
    ], // L=39
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (3, 0.09425),
        (2, 0.1122),
    ], // L=40
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=41
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=42
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=43
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=44
    [
        (4, 0.124),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=45
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=46
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=47
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=48
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=49
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=50
    [
        (3, 0.2015),
        (3, 0.13065),
        (3, 0.1092),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=51
    [
        (3, 0.2015),
        (3, 0.13065),
        (2, 0.1428),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=52
    [
        (3, 0.2015),
        (3, 0.13065),
        (2, 0.1428),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=53
    [
        (3, 0.2015),
        (3, 0.13065),
        (2, 0.1428),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=54
    [
        (3, 0.2015),
        (3, 0.13065),
        (2, 0.1428),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=55
    [
        (3, 0.2015),
        (3, 0.13065),
        (2, 0.1428),
        (2, 0.12325),
        (2, 0.1122),
    ], // L=56
];

const BLOCK_LENGTHS: [[u32; 6]; 48] = [
    [1, 1, 1, 2, 2, 2],   // L=9
    [1, 1, 2, 2, 2, 2],   // L=10
    [1, 2, 2, 2, 2, 2],   // L=11
    [2, 2, 2, 2, 2, 2],   // L=12
    [2, 2, 2, 2, 2, 3],   // L=13
    [2, 2, 2, 2, 3, 3],   // L=14
    [2, 2, 2, 3, 3, 3],   // L=15
    [2, 2, 3, 3, 3, 3],   // L=16
    [2, 3, 3, 3, 3, 3],   // L=17
    [3, 3, 3, 3, 3, 3],   // L=18
    [3, 3, 3, 3, 3, 4],   // L=19
    [3, 3, 3, 3, 4, 4],   // L=20
    [3, 3, 3, 4, 4, 4],   // L=21
    [3, 3, 4, 4, 4, 4],   // L=22
    [3, 4, 4, 4, 4, 4],   // L=23
    [4, 4, 4, 4, 4, 4],   // L=24
    [4, 4, 4, 4, 4, 5],   // L=25
    [4, 4, 4, 4, 5, 5],   // L=26
    [4, 4, 4, 5, 5, 5],   // L=27
    [4, 4, 5, 5, 5, 5],   // L=28
    [4, 5, 5, 5, 5, 5],   // L=29
    [5, 5, 5, 5, 5, 5],   // L=30
    [5, 5, 5, 5, 5, 6],   // L=31
    [5, 5, 5, 5, 6, 6],   // L=32
    [5, 5, 5, 6, 6, 6],   // L=33
    [5, 5, 6, 6, 6, 6],   // L=34
    [5, 6, 6, 6, 6, 6],   // L=35
    [6, 6, 6, 6, 6, 6],   // L=36
    [6, 6, 6, 6, 6, 7],   // L=37
    [6, 6, 6, 6, 7, 7],   // L=38
    [6, 6, 6, 7, 7, 7],   // L=39
    [6, 6, 7, 7, 7, 7],   // L=40
    [6, 7, 7, 7, 7, 7],   // L=41
    [7, 7, 7, 7, 7, 7],   // L=42
    [7, 7, 7, 7, 7, 8],   // L=43
    [7, 7, 7, 7, 8, 8],   // L=44
    [7, 7, 7, 8, 8, 8],   // L=45
    [7, 7, 8, 8, 8, 8],   // L=46
    [7, 8, 8, 8, 8, 8],   // L=47
    [8, 8, 8, 8, 8, 8],   // L=48
    [8, 8, 8, 8, 8, 9],   // L=49
    [8, 8, 8, 8, 9, 9],   // L=50
    [8, 8, 8, 9, 9, 9],   // L=51
    [8, 8, 9, 9, 9, 9],   // L=52
    [8, 9, 9, 9, 9, 9],   // L=53
    [9, 9, 9, 9, 9, 9],   // L=54
    [9, 9, 9, 9, 9, 10],  // L=55
    [9, 9, 9, 9, 10, 10], // L=56
];

/// Annex G ("Bit Allocation for Higher Order DCT Coefficients"): for a given `L` (9..=56), returns
/// the bit allocation for the higher-order DCT coefficients `b_8` through `b_{L+1}`, in that order
/// (`C_{1,2}, C_{1,3}, ..., C_{1,J_1}, C_{2,2}, ..., C_{6,J_6}` per the spec's own indexing -- the
/// `(block, coefficient)` label for each entry is fully determined by its position here together
/// with [`block_lengths_for_l`], so this table only needs to carry the bit counts themselves).
/// Returns `None` outside `9..=56`, same "give up, don't guess" discipline as the other lookups here.
///
/// **Real methodology, genuinely harder than every other table in this module, resolved via a
/// different technique**: this annex spans 19 pages and pdftotext's own text-layout reconstruction
/// drops or garbles entries on 31 of the 48 rows (`ambe/mod.rs`'s own doc comment records this as
/// the reason Annex G was originally left deferred). Rather than guess at the missing entries,
/// extracted the PDF's raw per-character glyph stream directly (via PyMuPDF/`fitz`'s `rawdict`,
/// bypassing `pdftotext`'s own line-reconstruction heuristic entirely), which requires decoding this
/// document's custom Type 3 font encoding -- confirmed empirically against Annex E's own clean,
/// unambiguous index column (0-63) that the encoding is trivial: a control character with `ord(c)`
/// in 1..=9 represents that digit, and literal ASCII `'0'` represents zero. Applied that decoder to
/// every character in Annex G, filtered to the real content fonts (excluding the diagonal "Limited
/// Use Only" watermark, which uses a distinct `Arial` font entirely -- confirmed via `pdfimages
/// -list` finding zero embedded images on any of these pages, so watermark and content are both real
/// vector text on separate font/layer, not a raster collision), and read each entry's own `C_{i,k}`/
/// `b`-index subscript labels directly by character position rather than inferring order from page
/// layout (an earlier attempt assuming a fixed reading direction produced the right VALUES but a
/// scrambled order for larger `L`, since one page uses a different single-column layout than the
/// rest -- discovered and corrected by cross-checking against known values, not assumed correct).
///
/// **Verified three independent ways before being trusted**: (1) every one of the 1207 `(L, b_idx)`
/// values `pdftotext` DID capture correctly matches this extraction exactly, zero mismatches; (2) the
/// full 1272-entry table (`sum(L-6) for L in 9..=56`) satisfies the real, published-elsewhere-in-this-
/// module per-block non-increasing invariant (`higher_order_bit_allocation_is_non_increasing_within_
/// each_block` below) for all 288 blocks (48 L values x 6 blocks each) with zero exceptions; (3) every
/// `L` has exactly `L-6` entries with zero missing or duplicate-conflicting `b_idx` values.
// [@ANCHOR: higher_order_bit_allocation]
pub fn higher_order_bit_allocation(l: u32) -> Option<&'static [u8]> {
    if !(9..=56).contains(&l) {
        return None;
    }
    Some(HIGHER_ORDER_BIT_ALLOCATION[(l - 9) as usize])
}

/// Table 3 ("Uniform Quantizer Step Size for Higher Order DCT Coefficients", TIA-102.BABA_2003.pdf
/// section 6.3.2): the step-size multiplier for a given bit allocation `bits` (1..=10), to be scaled
/// by the coefficient's own standard deviation ([`higher_order_coefficient_sigma`]) per the spec's own
/// worked example ("if 4 bits are allocated... the step size, Delta, equals .40 sigma"). This is a
/// small, universal, `L`-independent table -- distinct from Annex F's per-`L` gain-vector step sizes.
///
/// A coefficient with `bits == 0` (a real, observed value in [`HIGHER_ORDER_BIT_ALLOCATION`], not a
/// hypothetical) has no entry here and returns `None`: zero bits means that coefficient isn't
/// transmitted at all, so no step size is ever needed for it.
// [@ANCHOR: higher_order_step_multiplier]
pub fn higher_order_step_multiplier(bits: u8) -> Option<f64> {
    if !(1..=10).contains(&bits) {
        return None;
    }
    Some(HIGHER_ORDER_STEP_MULTIPLIER[(bits - 1) as usize])
}

const HIGHER_ORDER_STEP_MULTIPLIER: [f64; 10] =
    [1.2, 0.85, 0.65, 0.40, 0.28, 0.15, 0.08, 0.04, 0.02, 0.01];

/// Table 4 ("Standard Deviation of Higher Order DCT Coefficients", TIA-102.BABA_2003.pdf section
/// 6.3.2): the standard deviation `sigma` of the `k`'th DCT coefficient position within *any* block
/// (the spec's own text: "if this was the third DCT coefficient from any block (i.e. `C_i,3`), then
/// `sigma = .241`" -- notably independent of the block number `i`, only the position `k` within it).
/// `k` ranges 2..=10 (position 1 is each block's own DC term, already split off into the gain vector
/// and quantized separately via Annex E/F, never through this table).
///
/// Returns `None` for `k` outside 2..=10 -- the spec's own table doesn't go further because no block
/// length this codec ever produces (Annex J, [`block_lengths_for_l`]) exceeds 10 (checked directly:
/// every one of the 288 real block lengths across all 48 `L` values is 10 or less).
// [@ANCHOR: higher_order_coefficient_sigma]
pub fn higher_order_coefficient_sigma(k: u32) -> Option<f64> {
    if !(2..=10).contains(&k) {
        return None;
    }
    Some(HIGHER_ORDER_COEFFICIENT_SIGMA[(k - 2) as usize])
}

const HIGHER_ORDER_COEFFICIENT_SIGMA: [f64; 9] = [
    0.307, 0.241, 0.207, 0.190, 0.179, 0.173, 0.165, 0.170, 0.170,
];

const HIGHER_ORDER_BIT_ALLOCATION: [&[u8]; 48] = [
    &[9, 8, 7],                                                    // L=9
    &[9, 7, 6, 5],                                                 // L=10
    &[9, 7, 6, 5, 4],                                              // L=11
    &[8, 7, 6, 5, 4, 3],                                           // L=12
    &[7, 7, 6, 5, 4, 3, 3],                                        // L=13
    &[7, 7, 5, 4, 4, 3, 4, 3],                                     // L=14
    &[6, 7, 5, 4, 4, 3, 3, 3, 3],                                  // L=15
    &[6, 6, 5, 4, 4, 3, 3, 3, 3, 2],                               // L=16
    &[5, 5, 5, 4, 4, 4, 3, 3, 2, 3, 2],                            // L=17
    &[5, 4, 5, 5, 4, 3, 3, 3, 3, 2, 2, 2],                         // L=18
    &[5, 4, 5, 4, 4, 3, 3, 3, 3, 2, 3, 2, 1],                      // L=19
    &[5, 4, 5, 4, 4, 3, 3, 2, 3, 2, 1, 3, 2, 1],                   // L=20
    &[4, 4, 5, 4, 4, 3, 3, 2, 2, 3, 2, 1, 3, 2, 1],                // L=21
    &[4, 4, 4, 4, 4, 3, 2, 3, 2, 2, 3, 2, 1, 2, 2, 1],             // L=22
    &[4, 3, 4, 4, 3, 4, 3, 2, 3, 2, 2, 2, 2, 1, 2, 2, 1],          // L=23
    &[4, 3, 3, 4, 3, 3, 3, 3, 2, 3, 2, 1, 2, 2, 1, 2, 2, 1],       // L=24
    &[4, 3, 3, 4, 3, 3, 3, 3, 2, 3, 2, 1, 2, 2, 1, 2, 1, 1, 1],    // L=25
    &[4, 3, 3, 4, 3, 3, 3, 2, 2, 3, 2, 1, 2, 2, 1, 1, 2, 2, 1, 1], // L=26
    &[
        4, 3, 2, 4, 3, 2, 3, 2, 2, 3, 2, 2, 1, 2, 2, 1, 1, 2, 2, 1, 1,
    ], // L=27
    &[
        4, 3, 2, 4, 3, 2, 3, 2, 2, 2, 3, 2, 1, 1, 2, 2, 1, 1, 2, 1, 1, 1,
    ], // L=28
    &[
        3, 3, 2, 4, 3, 2, 2, 3, 2, 2, 2, 3, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 1,
    ], // L=29
    &[
        3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 2, 1, 3, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 1,
    ], // L=30
    &[
        3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 2, 1, 2, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 1, 1,
    ], // L=31
    &[
        3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 2, 1, 2, 2, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 0,
    ], // L=32
    &[
        3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1,
    ], // L=33
    &[
        3, 2, 2, 2, 3, 2, 2, 2, 3, 2, 2, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 0,
    ], // L=34
    &[
        3, 2, 2, 2, 3, 2, 2, 2, 2, 3, 2, 1, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 0, 2, 1, 1, 1, 0,
    ], // L=35
    &[
        3, 2, 2, 2, 1, 3, 2, 2, 2, 1, 3, 2, 1, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 0, 2, 1, 1, 1, 0,
    ], // L=36
    &[
        3, 2, 2, 2, 1, 3, 2, 2, 2, 2, 3, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 0, 2, 1, 1, 1, 1, 0,
    ], // L=37
    &[
        3, 2, 2, 2, 1, 3, 2, 2, 2, 1, 3, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1, 0, 2, 1, 1, 1,
        1, 0,
    ], // L=38
    &[
        3, 2, 2, 2, 1, 3, 2, 2, 2, 1, 3, 2, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1, 1, 1, 1, 0, 2, 1, 1,
        1, 0, 0,
    ], // L=39
    &[
        3, 2, 2, 2, 1, 3, 2, 2, 1, 1, 3, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1, 1, 1, 1, 0, 2, 1,
        1, 1, 0, 0,
    ], // L=40
    &[
        3, 2, 2, 1, 1, 3, 2, 2, 2, 1, 1, 3, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1, 1, 1, 1, 0, 2,
        1, 1, 1, 0, 0,
    ], // L=41
    &[
        3, 2, 2, 2, 1, 1, 3, 2, 2, 2, 1, 1, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1, 1, 1, 0, 0,
        2, 1, 1, 1, 0, 0,
    ], // L=42
    &[
        3, 2, 2, 2, 1, 1, 3, 2, 2, 2, 1, 1, 2, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1, 0, 2, 1, 1, 1, 0, 0,
        2, 1, 1, 1, 1, 0, 0,
    ], // L=43
    &[
        3, 2, 2, 1, 1, 1, 3, 2, 2, 2, 1, 1, 2, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1, 0, 2, 1, 1, 1, 1, 0,
        0, 2, 1, 1, 1, 1, 0, 0,
    ], // L=44
    &[
        3, 2, 2, 1, 1, 1, 3, 2, 2, 1, 1, 1, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 0, 2, 1, 1, 1, 1,
        0, 0, 2, 1, 1, 1, 1, 0, 0,
    ], // L=45
    &[
        3, 2, 2, 1, 1, 1, 3, 2, 2, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 0, 2, 1, 1, 1,
        1, 0, 0, 2, 1, 1, 1, 1, 0, 0,
    ], // L=46
    &[
        3, 2, 2, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 0, 2, 1, 1,
        1, 1, 0, 0, 2, 1, 1, 1, 0, 0, 0,
    ], // L=47
    &[
        3, 2, 2, 1, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 0, 2, 1,
        1, 1, 0, 0, 0, 2, 1, 1, 1, 0, 0, 0,
    ], // L=48
    &[
        3, 2, 2, 1, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0, 0, 2, 1,
        1, 1, 0, 0, 0, 2, 1, 1, 1, 1, 0, 0, 0,
    ], // L=49
    &[
        3, 2, 2, 1, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0, 0, 2, 1,
        1, 1, 1, 0, 0, 0, 2, 1, 1, 1, 0, 0, 0, 0,
    ], // L=50
    &[
        3, 2, 2, 1, 1, 1, 1, 3, 2, 1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0, 0, 0, 2,
        1, 1, 1, 1, 0, 0, 0, 2, 1, 1, 1, 1, 0, 0, 0,
    ], // L=51
    &[
        3, 2, 1, 1, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0, 0, 0,
        2, 1, 1, 1, 1, 0, 0, 0, 2, 1, 1, 1, 1, 0, 0, 0,
    ], // L=52
    &[
        3, 2, 1, 1, 1, 1, 1, 3, 2, 2, 1, 1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0, 0,
        0, 2, 1, 1, 1, 1, 0, 0, 0, 2, 1, 1, 1, 0, 0, 0, 0,
    ], // L=53
    &[
        3, 2, 2, 1, 1, 1, 1, 0, 3, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0,
        0, 0, 2, 1, 1, 1, 1, 0, 0, 0, 2, 1, 1, 1, 0, 0, 0, 0,
    ], // L=54
    &[
        3, 2, 2, 1, 1, 1, 1, 0, 3, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0,
        0, 0, 2, 1, 1, 1, 0, 0, 0, 0, 2, 1, 1, 1, 1, 0, 0, 0, 0,
    ], // L=55
    &[
        3, 2, 2, 1, 1, 1, 1, 0, 3, 2, 2, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 1, 1, 0, 2, 2, 1, 1, 1, 0,
        0, 0, 2, 1, 1, 1, 1, 0, 0, 0, 0, 2, 1, 1, 1, 0, 0, 0, 0, 0,
    ], // L=56
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_quantizer_levels_is_strictly_monotonically_increasing() {
        for i in 1..GAIN_QUANTIZER_LEVELS.len() {
            assert!(
                GAIN_QUANTIZER_LEVELS[i] > GAIN_QUANTIZER_LEVELS[i - 1],
                "index {}: {} is not greater than index {}: {}",
                i,
                GAIN_QUANTIZER_LEVELS[i],
                i - 1,
                GAIN_QUANTIZER_LEVELS[i - 1]
            );
        }
    }

    #[test]
    // Tests [@ANCHOR: quantize_gain_index]
    fn quantize_gain_index_finds_the_real_nearest_level() {
        assert_eq!(quantize_gain_index(-2.842205), 0);
        assert_eq!(quantize_gain_index(8.695827), 63);
        assert_eq!(quantize_gain_index(0.0), 17); // nearest to 0.027755, index 17
                                                  // Well below the table's own lowest level: still clamps to the nearest (lowest) index,
                                                  // matching Eq. 62's own three-case clamping behavior for the OTHER gain elements -- b_hat_2
                                                  // itself is always "nearest," so this is the expected, correct behavior here too, not an
                                                  // unhandled edge case.
        assert_eq!(quantize_gain_index(-100.0), 0);
        assert_eq!(quantize_gain_index(100.0), 63);
    }

    /// Spot-checks `GAIN_BIT_ALLOCATION` (Annex F) and `HIGHER_ORDER_BIT_ALLOCATION` (Annex G) against
    /// fresh 600 DPI renders of `TIA-102.BABA_2003.pdf` pages 87-90 (Annex F) and 91-101 (Annex G),
    /// for every `L` that actually recurred in a real 200-frame chip-vs-float speech comparison
    /// (`ratet27_diagnose_gain_scale_mismatch.rs`'s own live-chip run). Written after that
    /// investigation's live bit-flip probe (`ratet27_bit_flip_semantic_probe.rs`) found real evidence
    /// that this crate's own TIA Fig. 22 bit-prioritization layout for `b2` is very likely correct --
    /// this test rules out the other candidate explanation for the residual chip/float PCM gap (a
    /// single-digit Annex F/G transcription error silently compressing one `L` value's spectrum,
    /// which would survive fixed-vs-float validation since both sides share this same table) for the
    /// specific `L` values real speech actually exercises. Not exhaustive across all 48 rows -- a
    /// future session extending this coverage should add more `L` values here, not re-render pages
    /// already covered.
    #[test]
    fn gain_and_higher_order_bit_allocation_match_annex_f_g_for_recurring_l_values() {
        // (L, [(bits, step_size); 5]) for GAIN_BIT_ALLOCATION, read directly off the Annex F render.
        let annex_f: [(u32, [(u8, f64); 5]); 9] = [
            (
                24,
                [
                    (5, 0.0868),
                    (4, 0.0804),
                    (4, 0.0672),
                    (4, 0.058),
                    (4, 0.0528),
                ],
            ),
            (
                30,
                [
                    (4, 0.124),
                    (4, 0.0804),
                    (4, 0.0672),
                    (3, 0.09425),
                    (3, 0.0858),
                ],
            ),
            (
                32,
                [
                    (4, 0.124),
                    (4, 0.0804),
                    (3, 0.1092),
                    (3, 0.09425),
                    (3, 0.0858),
                ],
            ),
            (
                33,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (3, 0.09425),
                    (3, 0.0858),
                ],
            ),
            (
                34,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (3, 0.09425),
                    (3, 0.0858),
                ],
            ),
            (
                36,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (3, 0.09425),
                    (3, 0.0858),
                ],
            ),
            (
                40,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (3, 0.09425),
                    (2, 0.1122),
                ],
            ),
            (
                42,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (2, 0.12325),
                    (2, 0.1122),
                ],
            ),
            (
                44,
                [
                    (4, 0.124),
                    (3, 0.13065),
                    (3, 0.1092),
                    (2, 0.12325),
                    (2, 0.1122),
                ],
            ),
        ];
        for (l, expected) in annex_f {
            for (idx, &(bits, step)) in expected.iter().enumerate() {
                let (actual_bits, actual_step) = gain_bit_allocation(l, idx as u32 + 2).unwrap();
                assert_eq!(
                    actual_bits,
                    bits,
                    "L={l} m={}: bits mismatch vs Annex F render",
                    idx + 2
                );
                assert!(
                    (actual_step - step).abs() < 1e-9,
                    "L={l} m={}: step {actual_step} vs Annex F render {step}",
                    idx + 2
                );
            }
        }

        // (L, [bits; ...]) for HIGHER_ORDER_BIT_ALLOCATION (b8..b_{L+1}), read directly off the
        // Annex G render.
        let annex_g: [(u32, &[u8]); 9] = [
            (24, &[4, 3, 3, 4, 3, 3, 3, 3, 2, 3, 2, 1, 2, 2, 1, 2, 2, 1]),
            (
                30,
                &[
                    3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 2, 1, 3, 2, 1, 1, 2, 1, 1, 1, 2, 1, 1, 1,
                ],
            ),
            (
                32,
                &[
                    3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 2, 1, 2, 2, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 0,
                ],
            ),
            (
                33,
                &[
                    3, 3, 2, 2, 3, 3, 2, 2, 3, 2, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1,
                ],
            ),
            (
                34,
                &[
                    3, 2, 2, 2, 3, 2, 2, 2, 3, 2, 2, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 1, 2, 1, 1,
                    1, 0,
                ],
            ),
            (
                36,
                &[
                    3, 2, 2, 2, 1, 3, 2, 2, 2, 1, 3, 2, 1, 1, 1, 2, 2, 1, 1, 1, 2, 1, 1, 1, 0, 2,
                    1, 1, 1, 0,
                ],
            ),
            (
                40,
                &[
                    3, 2, 2, 2, 1, 3, 2, 2, 1, 1, 3, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1, 1, 1,
                    1, 0, 2, 1, 1, 1, 0, 0,
                ],
            ),
            (
                42,
                &[
                    3, 2, 2, 2, 1, 1, 3, 2, 2, 2, 1, 1, 2, 2, 1, 1, 1, 1, 2, 2, 1, 1, 1, 0, 2, 1,
                    1, 1, 0, 0, 2, 1, 1, 1, 0, 0,
                ],
            ),
            (
                44,
                &[
                    3, 2, 2, 1, 1, 1, 3, 2, 2, 2, 1, 1, 2, 2, 1, 1, 1, 1, 2, 1, 1, 1, 1, 0, 2, 1,
                    1, 1, 1, 0, 0, 2, 1, 1, 1, 1, 0, 0,
                ],
            ),
        ];
        for (l, expected) in annex_g {
            let actual = higher_order_bit_allocation(l).unwrap();
            assert_eq!(
                actual, expected,
                "L={l}: HIGHER_ORDER_BIT_ALLOCATION mismatch vs Annex G render"
            );
        }
    }

    #[test]
    // Tests [@ANCHOR: block_lengths_for_l]
    fn block_lengths_always_sum_to_l() {
        for l in 9..=56u32 {
            let lengths = block_lengths_for_l(l).unwrap();
            let sum: u32 = lengths.iter().sum();
            assert_eq!(
                sum, l,
                "L={l}: block lengths {lengths:?} sum to {sum}, not {l}"
            );
        }
    }

    #[test]
    fn block_lengths_are_non_decreasing_left_to_right() {
        for l in 9..=56u32 {
            let lengths = block_lengths_for_l(l).unwrap();
            for i in 1..6 {
                assert!(
                    lengths[i] >= lengths[i - 1],
                    "L={l}: block {i} ({}) is shorter than block {} ({})",
                    lengths[i],
                    i - 1,
                    lengths[i - 1]
                );
            }
        }
    }

    #[test]
    fn block_lengths_for_l_refuses_out_of_range_values_rather_than_guessing() {
        assert_eq!(block_lengths_for_l(8), None);
        assert_eq!(block_lengths_for_l(57), None);
    }

    #[test]
    // Tests [@ANCHOR: gain_bit_allocation]
    fn gain_bit_allocation_bits_are_non_increasing_as_l_grows_for_each_fixed_m() {
        // The real, independent structural check that recovered this table's own four
        // watermark-displaced entries (L=10,11,16,17) by elimination rather than guessing --
        // see gain_bit_allocation's own doc comment. More harmonics (higher L) leaves less bit
        // budget per gain-vector coefficient, so bits must never increase as L grows.
        for m in 2..=6u32 {
            let mut prev = gain_bit_allocation(9, m).unwrap().0;
            for l in 10..=56u32 {
                let bits = gain_bit_allocation(l, m).unwrap().0;
                assert!(bits <= prev, "m={m}, L={l}: bits {bits} > previous {prev}");
                prev = bits;
            }
        }
    }

    #[test]
    fn gain_bit_allocation_step_size_is_non_decreasing_as_l_grows_for_each_fixed_m() {
        for m in 2..=6u32 {
            let mut prev = gain_bit_allocation(9, m).unwrap().1;
            for l in 10..=56u32 {
                let step = gain_bit_allocation(l, m).unwrap().1;
                assert!(
                    step >= prev - 1e-9,
                    "m={m}, L={l}: step {step} < previous {prev}"
                );
                prev = step;
            }
        }
    }

    #[test]
    fn gain_bit_allocation_refuses_out_of_range_values_rather_than_guessing() {
        assert_eq!(gain_bit_allocation(8, 3), None);
        assert_eq!(gain_bit_allocation(57, 3), None);
        assert_eq!(gain_bit_allocation(20, 1), None);
        assert_eq!(gain_bit_allocation(20, 7), None);
    }

    #[test]
    // Tests [@ANCHOR: higher_order_bit_allocation]
    fn higher_order_bit_allocation_has_exactly_l_minus_6_entries_for_every_l() {
        for l in 9..=56u32 {
            let entries = higher_order_bit_allocation(l).unwrap();
            assert_eq!(
                entries.len() as u32,
                l - 6,
                "L={l}: expected {} entries, got {}",
                l - 6,
                entries.len()
            );
        }
    }

    #[test]
    fn higher_order_bit_allocation_is_non_increasing_within_each_block() {
        // The real, independent structural check this table's own doc comment describes: each of
        // the six frequency blocks (per block_lengths_for_l's own J_i lengths) must have
        // non-increasing bit allocation across its own coefficients (k=2..J_i) -- standard
        // perceptual-coding practice (earlier/lower-order coefficients within a block get at least
        // as many bits as later ones), and this held for all 288 blocks (48 L values x 6 blocks)
        // during validation, with zero exceptions -- see fec.rs's own doc comment for the sibling
        // discipline (an independent, spec-external invariant, not merely "read carefully").
        for l in 9..=56u32 {
            let entries = higher_order_bit_allocation(l).unwrap();
            let block_lengths = block_lengths_for_l(l).unwrap();
            let mut idx = 0usize;
            for (block_num, &j) in block_lengths.iter().enumerate() {
                let count = (j.saturating_sub(1)) as usize; // k=2..=J_i
                let block = &entries[idx..idx + count];
                idx += count;
                for w in block.windows(2) {
                    assert!(
                        w[0] >= w[1],
                        "L={l}, block {}: {:?} is not non-increasing",
                        block_num + 1,
                        block
                    );
                }
            }
            assert_eq!(
                idx,
                entries.len(),
                "L={l}: block lengths didn't cover all entries"
            );
        }
    }

    #[test]
    fn higher_order_bit_allocation_refuses_out_of_range_values_rather_than_guessing() {
        assert_eq!(higher_order_bit_allocation(8), None);
        assert_eq!(higher_order_bit_allocation(57), None);
    }

    #[test]
    // Tests [@ANCHOR: higher_order_step_multiplier]
    fn higher_order_step_multiplier_matches_table_3_and_refuses_out_of_range_bits() {
        assert!((higher_order_step_multiplier(1).unwrap() - 1.2).abs() < 1e-12);
        assert!((higher_order_step_multiplier(4).unwrap() - 0.40).abs() < 1e-12);
        assert!((higher_order_step_multiplier(10).unwrap() - 0.01).abs() < 1e-12);
        assert_eq!(
            higher_order_step_multiplier(0),
            None,
            "zero bits means not transmitted"
        );
        assert_eq!(higher_order_step_multiplier(11), None);
    }

    #[test]
    // Tests [@ANCHOR: higher_order_coefficient_sigma]
    fn higher_order_coefficient_sigma_matches_table_4_and_refuses_out_of_range_k() {
        assert!((higher_order_coefficient_sigma(2).unwrap() - 0.307).abs() < 1e-12);
        assert!((higher_order_coefficient_sigma(3).unwrap() - 0.241).abs() < 1e-12);
        assert!((higher_order_coefficient_sigma(10).unwrap() - 0.170).abs() < 1e-12);
        assert_eq!(
            higher_order_coefficient_sigma(1),
            None,
            "k=1 is the block's own DC term"
        );
        assert_eq!(higher_order_coefficient_sigma(11), None);
    }

    #[test]
    fn table_3_worked_example_from_the_spec_matches_exactly() {
        // The spec's own worked example (section 6.3.2): "if 4 bits are allocated... the step
        // size, Delta, equals .40 sigma. If this was the third DCT coefficient from any block
        // (i.e. C_i,3), then sigma = .241... this multiplication gives a step size of .0964."
        let multiplier = higher_order_step_multiplier(4).unwrap();
        let sigma = higher_order_coefficient_sigma(3).unwrap();
        let step_size = multiplier * sigma;
        assert!(
            (step_size - 0.0964).abs() < 1e-9,
            "expected the spec's own worked example to reproduce 0.0964, got {step_size}"
        );
    }

    /// A real, load-bearing cross-check spanning two Annex tables transcribed by two entirely
    /// different techniques at two different times (Annex F: recovered-by-elimination `pdftotext`;
    /// Annex G: raw glyph-stream extraction) -- if either had a transcription error, this is far
    /// more likely to catch it than either table's own internal invariant checks above, since it's
    /// derived independently rather than checked against itself.
    ///
    /// `VOICE_BITS` (88) splits as: `b_hat_0` (8) + `b_hat_1` (`K_hat` bits) + `b_hat_2` (6) +
    /// the five gain-vector elements (Annex F, `m=2..=6`) + every higher-order DCT coefficient
    /// (Annex G) + `b_hat_{L+2}` (the sync bit, 1). So the gain-vector and higher-order bits alone
    /// must sum to `88 - 8 - K_hat - 6 - 1 = 73 - K_hat` for every real `(L, K_hat)` pair -- checked
    /// here for all 48 tabulated `L` values, not just the `L=16` worked example
    /// `bit_prioritization.rs`'s own tests already confirm by hand. `prioritize_bits` returns `None`
    /// whenever this doesn't hold, so any `L` failing this check is an `L` where the encoder would
    /// silently refuse to ever produce a frame -- exactly the failure this test exists to catch
    /// before it's discovered that way.
    #[test]
    fn gain_and_higher_order_bit_totals_satisfy_73_minus_k_hat_for_every_l() {
        for l in 9..=56u32 {
            let k_hat = super::super::vuv::frequency_bands_count(l);

            let gain_bits: u32 = (2..=6u32)
                .map(|m| gain_bit_allocation(l, m).unwrap().0 as u32)
                .sum();
            let higher_order_bits: u32 = higher_order_bit_allocation(l)
                .unwrap()
                .iter()
                .map(|&b| b as u32)
                .sum();

            let total = gain_bits + higher_order_bits;
            let expected = 73 - k_hat;
            assert_eq!(
                total, expected,
                "L={l}, K_hat={k_hat}: expected gain+higher-order bits to sum to {expected} \
                 (73 - K_hat), got {total} (gain={gain_bits}, higher_order={higher_order_bits})"
            );
        }
    }
}
