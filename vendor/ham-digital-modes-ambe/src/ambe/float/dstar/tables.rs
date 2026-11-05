//! D-STAR's own AMBE quantizer tables -- unlike the P25 half/full-rate codec in `super::tia_102_baba`
//! (built directly from the published TIA-102.BABA text), DVSI has never published a spec for
//! D-STAR's own, older/smaller AMBE variant. These table *values* are transcribed from mbelib
//! (<https://github.com/szechyjs/mbelib>, `ambe3600x2400_const.h`), a real, independently
//! reverse-engineered, working open-source D-STAR decoder -- confirmed ISC-licensed (a permissive
//! license, not GPL). Per this project's own established position
//! (`hams_com/docs/AMBE_TABLE_COPYRIGHTABILITY_ANALYSIS.md`), functional numeric codebook/quantizer
//! data of this kind is not the kind of expression copyright protects (17 U.S.C. § 102(b)) --
//! independent of that, mbelib's own ISC license already permits this. Only the numeric values are
//! taken from mbelib; every function that uses them here is this project's own, freshly written.
//!
//! See `mod.rs`'s own doc comment for how these tables fit into the overall 72-bit frame and which
//! bits of `b0..b8` select into each one.

/// `L~` (harmonic count) selected by `b0`'s own 7-bit pitch index. Measured on the real chip's D-STAR decoder, not
/// mbelib's `AmbePlusLtable`: the chip's highest harmonic is `floor(3702 / f0_hz)` (the standard `0.9254 * 4000 / f0`)
/// with `f0_hz` this crate's `f0_from_b0`. `examples/dstar_field_scan.rs lscan` finds the chip's last harmonic from
/// the block-4 sensitivity to `b8`; this formula matches 78 of 80 pitch indices in b0 20..=99 (the other two are off by
/// one). Clamped to the real range 9..=56.
pub const L_TABLE: [u32; 126] = [
    9, 9, 9, 9, 9, 9, 9, 9, 9, 10, 10, 10, 10, 10, 10, 11, 11, 11, 11, 11, 11, 12, 12, 12, 12, 12,
    13, 13, 13, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 15, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18,
    18, 19, 19, 19, 20, 20, 20, 20, 21, 21, 21, 22, 22, 22, 23, 23, 23, 24, 24, 25, 25, 25, 26, 26,
    27, 27, 27, 28, 28, 29, 29, 30, 30, 30, 31, 31, 32, 32, 33, 33, 34, 34, 35, 36, 36, 37, 37, 38,
    38, 39, 40, 40, 41, 41, 42, 43, 43, 44, 45, 45, 46, 47, 47, 48, 49, 50, 50, 51, 52, 53, 54, 54,
    55, 56, 56, 56,
];

/// V/UV (voiced/unvoiced) pattern vectors, `AmbePlusVuv` in mbelib: `b1` (4 bits, 0..16) selects a
/// row; `jl` (0..8, itself derived from harmonic index `l` and the frame's own fundamental
/// frequency) selects which of the row's 8 entries applies to that harmonic.
pub const VUV: [[bool; 8]; 16] = [
    [false, false, false, false, false, false, false, false],
    [false, false, false, false, false, false, true, true],
    [false, false, false, false, true, true, false, false],
    [false, false, false, false, true, true, true, true],
    [false, false, true, true, false, false, false, false],
    [false, false, true, true, false, false, true, true],
    [false, false, true, true, true, true, false, false],
    [false, false, true, true, true, true, true, true],
    [true, true, false, false, false, false, false, false],
    [true, true, false, false, false, false, true, true],
    [true, true, false, false, true, true, false, false],
    [true, true, false, false, true, true, true, true],
    [true, true, true, true, false, false, false, false],
    [true, true, true, true, false, false, true, true],
    [true, true, true, true, true, true, false, false],
    [true, true, true, true, true, true, true, true],
];

/// Higher-order-coefficient block lengths `J_1..J_4`, `AmbePlusLmprbl` in mbelib: indexed by `L~`
/// (harmonic count, 0..57 -- entries below 9 are unused padding since `L~` is never below 9 per
/// [`L_TABLE`]). Each of the four spectral blocks gets its own coefficient count, always summing to
/// exactly `L~` (checked for every `L~` from 9 to 56, in both modes; the longest block has 17 entries).
pub const LMPRBL: [[u32; 4]; 57] = [
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [2, 2, 2, 3],
    [2, 2, 3, 3],
    [2, 3, 3, 3],
    [2, 3, 3, 4],
    [3, 3, 3, 4],
    [3, 3, 4, 4],
    [3, 3, 4, 5],
    [3, 4, 4, 5],
    [3, 4, 5, 5],
    [4, 4, 5, 5],
    [4, 4, 5, 6],
    [4, 4, 6, 6],
    [4, 5, 6, 6],
    [4, 5, 6, 7],
    [5, 5, 6, 7],
    [5, 5, 7, 7],
    [5, 6, 7, 7],
    [5, 6, 7, 8],
    [5, 6, 8, 8],
    [6, 6, 8, 8],
    [6, 6, 8, 9],
    [6, 7, 8, 9],
    [6, 7, 9, 9],
    [6, 7, 9, 10],
    [7, 7, 9, 10],
    [7, 8, 9, 10],
    [7, 8, 10, 10],
    [7, 8, 10, 11],
    [8, 8, 10, 11],
    [8, 9, 10, 11],
    [8, 9, 11, 11],
    [8, 9, 11, 12],
    [8, 9, 11, 13],
    [8, 9, 12, 13],
    [8, 10, 12, 13],
    [9, 10, 12, 13],
    [9, 10, 12, 14],
    [9, 10, 13, 14],
    [9, 11, 13, 14],
    [10, 11, 13, 14],
    [10, 11, 13, 15],
    [10, 11, 14, 15],
    [10, 12, 14, 15],
    [10, 12, 14, 16],
    [11, 12, 14, 16],
    [11, 12, 15, 16],
    [11, 12, 15, 17],
    [11, 13, 15, 17],
];

/// The gain-delta quantizer `Δγ`, `AmbePlusDg` in mbelib: `b2` (6 bits, 0..64) selects one of 64
/// levels, added to half the previous frame's own `γ` (Eq.-style recursion: `γ = Δγ + 0.5·γ_prev`,
/// mirrored exactly in [`super::decode::decode_frame`]).
pub const DG: [f64; 64] = [
    0.000000, 0.118200, 0.215088, 0.421167, 0.590088, 0.749075, 0.879395, 0.996388, 1.092285,
    1.171577, 1.236572, 1.313450, 1.376465, 1.453342, 1.516357, 1.600346, 1.669189, 1.742847,
    1.803223, 1.880234, 1.943359, 2.025067, 2.092041, 2.178042, 2.248535, 2.331718, 2.399902,
    2.492343, 2.568115, 2.658677, 2.732910, 2.816496, 2.885010, 2.956386, 3.014893, 3.078890,
    3.131348, 3.206615, 3.268311, 3.344785, 3.407471, 3.484885, 3.548340, 3.623339, 3.684814,
    3.764509, 3.829834, 3.915298, 3.985352, 4.072560, 4.144043, 4.231251, 4.302734, 4.399066,
    4.478027, 4.572883, 4.650635, 4.760785, 4.851074, 4.972361, 5.071777, 5.226203, 5.352783,
    5.352783,
];

include!("tables_prba.rs");
include!("tables_hoc.rs");
