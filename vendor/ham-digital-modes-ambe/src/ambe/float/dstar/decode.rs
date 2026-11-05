//! Bit-level frame parsing (FEC + whitening) and parameter dequantization for D-STAR's AMBE frame --
//! see `mod.rs`'s own doc comment for the real, source-verified frame structure and the `b0..b8`
//! bit-index mapping this module implements directly.
//!
//! **Scope, stated honestly**: this module recovers the real semantic parameters (harmonic count,
//! fundamental frequency, per-harmonic voicing, and reconstructed spectral amplitudes `Ml`) a real
//! decoder needs -- enough to validate against the real chip's own bitstream and to compare decoded
//! *parameters* directly. It does not yet include the final audio-synthesis stage (windowed
//! overlap-add voiced/unvoiced synthesis) that turns those parameters into PCM -- a real, separate,
//! disclosed next step, not silently skipped.

use super::tables;
use crate::ambe::general::fec::golay_decode;

/// The real FEC/whitening outcome of parsing one raw 72-bit frame: the 49 decoded data bits plus
/// both Golay blocks' own corrected-error counts. **The same caveat the P25 chip-validation
/// investigation found applies here too, since this reuses the identical `[23,12,7]` Golay code**:
/// it is a genuine perfect code (covering radius equals packing radius, exactly 3), so *every*
/// possible 23-bit input decodes to *some* codeword within distance <=3 -- `epsilon_c0`/`epsilon_c1`
/// can never exceed 3 regardless of whether the input was ever a real, intentionally-encoded
/// codeword. A single frame's own low corrected-error count is therefore not, by itself, strong
/// evidence of correct framing (see `AMBE_CHIP_VALIDATION_FINDINGS.md`'s own account of exactly this
/// trap) -- real validation needs either a genuine round trip against known data (this module's own
/// tests do that) or the same kind of multi-frame, perturbation-based empirical check the P25
/// investigation used against the real chip.
pub struct ParsedFrame {
    /// The 49 decoded data bits, packed MSB-first into a `u64` (bit 48 down to bit 0).
    pub d: u64,
    pub epsilon_c0: u32,
    pub epsilon_c1: u32,
}

/// Parses a raw 72-bit D-STAR AMBE frame (this crate's own logical frame layout -- see
/// `interleave::wire_bytes_to_frame` for converting real 9-byte chip/wire data into this format --
/// packed MSB-first into the low 72 bits of `frame`, i.e. `frame`'s bit 71 is `C0`'s own first data
/// bit) into its 49 real decoded data bits, applying Golay correction to `C0`/`C1` and de-whitening
/// `C1` using `C0`'s own corrected data (in that order -- see `mod.rs`'s own doc comment for why the
/// order matters).
pub fn parse_frame(frame: u128) -> ParsedFrame {
    let frame = frame & ((1u128 << 72) - 1);
    let c0 = ((frame >> 48) & 0xFF_FFFF) as u32; // top 24 bits
    let c1_raw = ((frame >> 25) & 0x7F_FFFF) as u32; // next 23 bits
    let c2 = ((frame >> 14) & 0x7FF) as u32; // next 11 bits
    let c3 = (frame & 0x3FFF) as u32; // low 14 bits

    // C0: bit 0 (the field's own LSB) is the spare bit, never checked; bits 23..1 are the Golay
    // codeword, MSB-first -- confirmed against mbelib's real `mbe_eccAmbe3600x2400C0`
    // (`in[j] = ambe_fr[0][j+1]`, so `ambe_fr[0][0]` is the spare, not `ambe_fr[0][23]`). Shifting
    // right (not masking off the top bit) is the fix: the earlier, wrong version of this line kept
    // the spare and dropped the true codeword MSB instead, which happened to still Golay-decode
    // "successfully" on some frames by coincidence, to the wrong data -- corrupting every C1
    // whitening seed downstream. See `interleave.rs`'s own doc comment for the full story.
    let c0_codeword = c0 >> 1;
    let (c0_data, epsilon_c0) = golay_decode(c0_codeword);

    // De-whiten C1 using C0's own corrected data, then Golay-decode the result.
    let c1_dewhitened = super::whiten_c1(c1_raw, c0_data);
    let (c1_data, epsilon_c1) = golay_decode(c1_dewhitened);

    let d: u64 =
        ((c0_data as u64) << 37) | ((c1_data as u64) << 25) | ((c2 as u64) << 14) | (c3 as u64);

    ParsedFrame {
        d,
        epsilon_c0,
        epsilon_c1,
    }
}

/// Reads `width` bits starting at bit position `msb_index` (0 = the overall 49-bit field's own
/// MSB, i.e. `d`'s bit 48) -- the natural indexing this module's own `mod.rs` doc-comment table
/// uses (`d[a..b)`), rather than raw bit-shift arithmetic scattered through the caller.
fn bits(d: u64, msb_index: usize, width: usize) -> u32 {
    let shift = 49 - msb_index - width;
    ((d >> shift) & ((1u64 << width) - 1)) as u32
}

fn bit(d: u64, msb_index: usize) -> u32 {
    bits(d, msb_index, 1)
}

/// The nine raw parameter indices `b0..b8`, extracted from the 49 decoded data bits per the exact
/// bit mapping documented in `mod.rs`'s own doc comment (traced directly from mbelib's real source,
/// not guessed).
pub struct RawParameters {
    pub b0: u32,
    pub b1: u32,
    pub b2: u32,
    pub b3: u32,
    pub b4: u32,
    pub b5: u32,
    pub b6: u32,
    pub b7: u32,
    pub b8: u32,
}

// [@ANCHOR: dstar_extract_raw_parameters]
pub fn extract_raw_parameters(d: u64) -> RawParameters {
    RawParameters {
        b0: (bits(d, 0, 6) << 1) | bit(d, 48),
        b1: bits(d, 38, 4),
        b2: (bits(d, 6, 4) << 2) | bits(d, 42, 2),
        b3: (bits(d, 10, 2) << 7) | (bits(d, 12, 5) << 2) | bits(d, 44, 2),
        b4: (bits(d, 17, 5) << 2) | bits(d, 46, 2),
        b5: (bits(d, 22, 2) << 2) | bits(d, 25, 2),
        b6: bits(d, 27, 4),
        b7: bits(d, 31, 4),
        b8: bits(d, 35, 3) << 1, // LSB forced 0, per mod.rs's own doc comment
    }
}

/// D-STAR's own real special-value trigger for `b0`, traced directly from mbelib's real
/// `ambe3600x2400.c` (`if ((b0&0x7E) == 0x7E) // frame is tone`) -- confirmed against a real,
/// captured chip frame under `ECMODE_IN`'s `TD_ENABLE` bit (see
/// `AMBE_CHIP_VALIDATION_FINDINGS.md`'s cross-mode DTX/DTMF section), not merely read off the
/// source. This is narrower than AMBE+2 half-rate's own 120-127 eight-value block
/// ([`crate::ambe::float::ambe_plus_2::decode::classify_b0`]): D-STAR only ever special-cases `b0` being
/// *exactly* 126 or 127, treating every other value -- including 120-125 -- as an ordinary (if
/// perhaps unusual) voiced/unvoiced speech pitch code. This matters concretely: the same live chip,
/// configured for D-STAR's RATEP and with `DTX_ENABLE` on, was observed driving silence frames to
/// `b0=120` -- a value this real reference decoder does **not** recognize as special, so a DTX-
/// silence frame under D-STAR is decoded as ordinary (if pitch-unusual) speech by mbelib itself, not
/// just by this crate. That is a real property of D-STAR's own encoding, not a gap this function
/// needs to paper over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `b0` anything except 126/127: a real, voiced/unvoiced speech frame -- the common case,
    /// and (per the doc comment above) also what a DTX-silence frame at `b0=120` decodes as.
    Speech,
    /// `b0` 126 or 127 exactly (`b0 & 0x7E == 0x7E`): a tone frame. Its `b1`/`b2` are **not** the
    /// ordinary speech fields this module's own [`extract_raw_parameters`] returns -- see
    /// [`decode_tone`] for the real, separately bit-scattered tone index/volume.
    Tone,
}

pub fn classify_b0(b0: u32) -> FrameKind {
    if b0 & 0x7E == 0x7E {
        FrameKind::Tone
    } else {
        FrameKind::Speech
    }
}

/// A tone frame's own real `index` value, classified per mbelib's real range table:
///
/// | `index` range | Meaning |
/// |---|---|
/// | `0..5` | Invalid/reserved -- a real encoder never emits this |
/// | `5..=122` | A single tone at `index * 31.25` Hz |
/// | `123..128` | Invalid/reserved |
/// | `128..=163` | A dual tone (DTMF is one, not the only, dual-tone signal) -- see [`dtmf_digit_from_tone_index`] |
/// | `164..=255` (`index`'s remaining 8-bit values) | Invalid/reserved |
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToneKind {
    Invalid,
    Single { hz: f64 },
    Dual,
}

/// Classifies a tone frame's `index` per mbelib's real range table (see [`ToneKind`]'s own doc
/// comment for the full table) -- confirmed directly against a live chip capture: a plain 200Hz
/// test tone produced `index=6`, which this function reads as `Single { hz: 187.5 }`, one
/// quantization step (31.25Hz) below the true stimulus frequency.
pub fn classify_tone_index(index: u32) -> ToneKind {
    match index {
        5..=122 => ToneKind::Single {
            hz: index as f64 * 31.25,
        },
        128..=163 => ToneKind::Dual,
        _ => ToneKind::Invalid,
    }
}

/// A tone frame's own real payload: `index` (see [`classify_tone_index`]/[`ToneKind`] for what it
/// means) and `volume`, an 8-bit level with no further documented structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TonePayload {
    pub index: u32,
    pub volume: u32,
}

/// Decodes a tone frame's real `index`/`volume`, per mbelib's own `ambe3600x2400.c` tone branch --
/// **a completely different bit scatter from [`extract_raw_parameters`]'s ordinary speech `b1`/`b2`**
/// (three of `index`'s bits are looked up through per-value tables keyed on `d[6..9)`, not read as a
/// plain contiguous field), so calling this on a frame [`classify_b0`] didn't call `Tone` produces a
/// meaningless value, not an error -- callers must check `classify_b0(b0)` first. Only valid to call
/// when the caller already has the frame's decoded 49-bit `d`.
///
/// The three lookup tables below (`T5TAB`/`T6TAB`/`T7TAB`) are transcribed directly from mbelib
/// (<https://github.com/szechyjs/mbelib>, ISC-licensed, `mbelib Author`), the same source and the
/// same functional-numeric-data status `tables.rs`'s own doc comment and this project's established
/// position (`hams_com/docs/AMBE_TABLE_COPYRIGHTABILITY_ANALYSIS.md`) already cover -- only the eight
/// values in each table are taken from mbelib; this function's own logic is freshly written.
pub fn decode_tone(d: u64) -> TonePayload {
    let bit = |msb_index: usize| -> u32 { ((d >> (48 - msb_index)) & 1) as u32 };
    // Verified/guessed status per mbelib's own inline comments; kept in code since it's load-bearing,
    // not just documentation -- these three tables cover the 3-bit `d[6..9)` selector's 8 outcomes.
    const T7TAB: [u32; 8] = [1, 0, 0, 0, 0, 1, 1, 1];
    const T6TAB: [u32; 8] = [0, 0, 0, 1, 1, 1, 1, 0];
    const T5TAB: [u32; 8] = [0, 0, 1, 0, 1, 1, 0, 1];
    let sel = ((bit(6) << 2) | (bit(7) << 1) | bit(8)) as usize;
    let index = (T7TAB[sel] << 7)
        | (T6TAB[sel] << 6)
        | (T5TAB[sel] << 5)
        | (bit(9) << 4)
        | (bit(42) << 3)
        | (bit(43) << 2)
        | (bit(10) << 1)
        | bit(11);
    let volume = (bit(12) << 7)
        | (bit(13) << 6)
        | (bit(14) << 5)
        | (bit(15) << 4)
        | (bit(16) << 3)
        | (bit(44) << 2)
        | (bit(45) << 1)
        | bit(17);
    TonePayload { index, volume }
}

/// Maps a dual-tone `index` (128-163 per mbelib -- see [`ToneKind::Dual`]) to a DTMF `(row, col)`
/// pair using the same 4x4 row/column numbering `ambe::dvsi_p25fec::dtmf::decode_dtmf_digit` and this
/// crate's other DTMF tooling use (row 0-3 = 697/770/852/941 Hz, col 0-3 = 1209/1336/1477/1633 Hz)
/// -- confirmed directly against a real chip capture of all 16 DTMF digits under D-STAR's RATEP with
/// `TD_ENABLE` on: `index == 128 + row + 4*col` held exactly for every one of the 16 digits,
/// independent of `DTX_ENABLE`'s own state. `index` 144-163 are real, mbelib-documented dual-tone
/// codes this function correctly reports as "not a DTMF digit" (`None`) rather than rejecting as
/// invalid input -- this crate has no evidence about what those 20 codes represent (a larger
/// dual-tone signaling space the chip supports beyond the 16-digit DTMF keypad, most plausibly, but
/// unconfirmed), so `classify_tone_index` still calls them `Dual`, only this function's own
/// DTMF-specific mapping returns `None` for them.
pub fn dtmf_digit_from_tone_index(index: u32) -> Option<(u8, u8)> {
    if !(128..=163).contains(&index) {
        return None;
    }
    let offset = index - 128;
    if offset > 15 {
        return None;
    }
    Some(((offset % 4) as u8, (offset / 4) as u8))
}

/// Persistent decoder state across frames -- the previous frame's own `L~`, its (post-inverse-DCT)
/// `log2` spectral-amplitude history, and `γ`, all of which the gain/spectral-magnitude recursion
/// below genuinely needs (mirroring mbelib's own `prev_mp` argument).
pub struct DStarDecoderState {
    pub l: u32,
    pub log2_ml: Vec<f64>,
    pub gamma: f64,
}

impl DStarDecoderState {
    /// A reasoned initial state for the very first frame -- no real prior history exists, so `L`
    /// starts at the table's own smallest real value (9, [`tables::L_TABLE`]'s first entry), and
    /// `log2_ml`/`gamma` both start at zero (unity amplitude in the log domain, the same
    /// "flat, constant, therefore low-stakes" choice `super::super::tia_102_baba::FrameState::initial` makes for
    /// its own P25 codec, for the same reason: this recursion's own gain term is a *difference*
    /// from the previous frame, so a constant initial value doesn't bias frame 0 in any particular
    /// direction).
    pub fn initial() -> Self {
        DStarDecoderState {
            l: 9,
            log2_ml: vec![0.0; 10],
            gamma: 0.0,
        }
    }
}

/// One frame's own real, decoded semantic parameters -- harmonic count, fundamental frequency,
/// per-harmonic voicing, and reconstructed spectral amplitudes `Ml[1..=l]` (1-indexed to match the
/// spec-style harmonic numbering `mod.rs`'s own doc comment and this whole module use throughout;
/// index 0 is unused padding).
pub struct DStarParameters {
    pub l: u32,
    pub w0: f64,
    pub voiced: Vec<bool>,
    pub ml: Vec<f64>,
}

/// The real outcome of dequantizing one frame: ordinary speech parameters, or a tone frame's own
/// separately-scattered payload (see [`decode_tone`]) -- never speech parameters computed from a
/// tone frame's ordinary-speech-shaped `b0..b8` misinterpretation, which is exactly what this crate
/// did before `classify_b0`/[`FrameKind::Tone`] existed (see this module's own doc comment history:
/// a live chip capture with `TD_ENABLE` on and a DTMF/tone stimulus reliably produces `b0 in
/// {126,127}`, which the pre-fix `dequantize` ran straight through the voiced-speech path).
pub enum DequantizedFrame {
    Speech(DStarParameters),
    Tone(TonePayload),
}

/// The gain recursion `gamma = GAMMA_SCALE*DG[b2] + GAMMA_MEMORY*gamma_prev`. mbelib uses `1` and `0.5`; the chip's
/// D-STAR decoder has no gain memory at all and its gain is twice the table value: after a gain step its output level
/// settles in two frames and returns to the old level in one, while mbelib's recursion needs five or six
/// (`examples/dstar_field_scan.rs ... ljump <row>` with `FIELD=b2`; settled levels agree, the trajectories then match
/// to about 2 dB). AMBE+2 half-rate does follow mbelib's recursion.
pub const GAMMA_SCALE: f64 = 2.0;
pub const GAMMA_MEMORY: f64 = 0.0;

/// The weight of the previous frame's log amplitudes in the amplitude predictor. mbelib (and AMBE+2 half-rate) use `0.65`; the
/// chip's D-STAR decoder settles with a per-frame step-response ratio of about `0.8`
/// (`examples/dstar_field_scan.rs ... rho`: first-frame response identical, later increments shrink by 0.78-0.81 per
/// frame on the chip versus 0.65-0.69 on mbelib's constant).
pub const PREDICTOR_RHO: f64 = 0.8;

/// `f0 = 2^(-4.258618 - 0.021766*b0)` cycles per sample: the real chip's D-STAR pitch map, fitted to the harmonic
/// peak frequencies of the chip's own output at 100 pitch indices (`examples/dstar_field_scan.rs ... f0scan`; maximum
/// residual 0.2%). It is logarithmic at 45.94 steps per octave, half of the P25-mode chip's 92.02, so the two are
/// sibling maps. mbelib's own guess formula `2^(-4.311767578125 - 2.1336e-2*(b0+0.5))` has 46.87 steps per octave and
/// is up to 1.7% off at either end of the range.
pub fn f0_from_b0(b0: u32) -> f64 {
    2f64.powf(-4.258618 - 0.021766 * b0 as f64)
}

/// Inverse of [`f0_from_b0`], fractional.
pub fn b0_from_f0(f0: f64) -> f64 {
    (-f0.log2() - 4.258618) / 0.021766
}

/// Dequantizes one frame's decoded 49-bit `d` into real synthesis-ready parameters (or a tone
/// frame's own payload), advancing `state` in place for the `Speech` case -- the direct,
/// freshly-written equivalent of mbelib's own `mbe_decodeAmbe2400Parms`, verified stage by stage
/// against that real source (see this function's own inline citations) rather than guessed. Takes
/// `d` directly (not just `RawParameters`) because a real tone frame's `index`/`volume` are read
/// from a different bit scatter than the ordinary speech `b1`/`b2` [`extract_raw_parameters`]
/// returns -- see [`decode_tone`].
// [@ANCHOR: dstar_dequantize]
pub fn dequantize(d: u64, state: &mut DStarDecoderState) -> DequantizedFrame {
    let raw = extract_raw_parameters(d);
    if classify_b0(raw.b0) == FrameKind::Tone {
        return DequantizedFrame::Tone(decode_tone(d));
    }

    let l = tables::L_TABLE[(raw.b0 as usize).min(tables::L_TABLE.len() - 1)];
    let f0 = f0_from_b0(raw.b0);
    let w0 = f0 * 2.0 * std::f64::consts::PI;

    let mut voiced = vec![false; l as usize + 1];
    for (harmonic, slot) in voiced.iter_mut().enumerate().skip(1) {
        let jl = (harmonic as f64 * 16.0 * f0) as usize;
        *slot = tables::VUV[raw.b1 as usize][jl.min(7)];
    }

    let delta_gamma = tables::DG[raw.b2 as usize];
    let gamma = GAMMA_SCALE * delta_gamma + GAMMA_MEMORY * state.gamma;

    // PRBA -> Gm -> Ri (8-point cosine sum) -> Cik's own first two elements per block.
    let prba24 = tables::PRBA24[raw.b3 as usize];
    let prba58 = tables::PRBA58[raw.b4 as usize];
    let gm: [f64; 9] = [
        0.0, 0.0, prba24[0], prba24[1], prba24[2], prba58[0], prba58[1], prba58[2], prba58[3],
    ];
    let mut ri = [0.0f64; 9];
    for (i, slot) in ri.iter_mut().enumerate().skip(1) {
        let mut sum = 0.0;
        for (m, &gm_m) in gm.iter().enumerate().skip(1) {
            let am = if m == 1 { 1.0 } else { 2.0 };
            sum += am
                * gm_m
                * (std::f64::consts::PI * (m as f64 - 1.0) * (i as f64 - 0.5) / 8.0).cos();
        }
        *slot = sum;
    }

    let rconst = 1.0 / (2.0 * std::f64::consts::SQRT_2);
    // Cik[1..=4][1..=block_len], 1-indexed (index 0 of each axis unused); sized to 18 columns to
    // match mbelib's own real local-variable declaration (`float Cik[5][18]`), since a block's own
    // `J_i` (tables::LMPRBL) can reach 17 -- every coefficient beyond index 6 stays zero (no HOC
    // table provides more than 4 entries per block, k=3..=6), matching mbelib's own explicit
    // `if (k > 6) Cik[i][k] = 0;` branch.
    let mut cik = [[0.0f64; 18]; 5];
    cik[1][1] = 0.5 * (ri[1] + ri[2]);
    cik[1][2] = rconst * (ri[1] - ri[2]);
    cik[2][1] = 0.5 * (ri[3] + ri[4]);
    cik[2][2] = rconst * (ri[3] - ri[4]);
    cik[3][1] = 0.5 * (ri[5] + ri[6]);
    cik[3][2] = rconst * (ri[5] - ri[6]);
    cik[4][1] = 0.5 * (ri[7] + ri[8]);
    cik[4][2] = rconst * (ri[7] - ri[8]);

    let ji = tables::LMPRBL[l as usize];
    let hoc_tables = [
        &tables::HOC_B5,
        &tables::HOC_B6,
        &tables::HOC_B7,
        &tables::HOC_B8,
    ];
    // b8's own low bit is always 0 (mbelib's own real convention, `mod.rs`'s doc comment) -- indexed
    // directly, matching mbelib's own `AmbePlusHOCb8[b8]` (half the table, odd indices, is simply
    // unreachable by real transmitted data, not a bug).
    let hoc_indices = [raw.b5, raw.b6, raw.b7, raw.b8];
    for block in 0..4 {
        for k in 3..=ji[block] {
            if k <= 6 {
                cik[block + 1][k as usize] =
                    hoc_tables[block][hoc_indices[block] as usize][(k - 3) as usize];
            }
        }
    }

    // Inverse DCT each block's own Cik into Tl (log-domain per-harmonic residual).
    let mut tl = vec![0.0f64; l as usize + 1];
    let mut harmonic = 1usize;
    for block in 0..4 {
        let block_len = ji[block] as usize;
        for j in 1..=block_len {
            let mut sum = 0.0;
            for (k, &cik_k) in cik[block + 1]
                .iter()
                .enumerate()
                .take(block_len + 1)
                .skip(1)
            {
                let ak = if k == 1 { 1.0 } else { 2.0 };
                sum += ak
                    * cik_k
                    * (std::f64::consts::PI * (k as f64 - 1.0) * (j as f64 - 0.5)
                        / block_len as f64)
                        .cos();
            }
            if harmonic <= l as usize {
                tl[harmonic] = sum;
            }
            harmonic += 1;
        }
    }

    // Reconstruct log2(Ml) via the previous frame's own resampled history (Sum42/43/BigGamma, per
    // mbelib's own real recursion) -- resample state.log2_ml (length state.l+1) onto the current
    // frame's own L harmonics first.
    let prev_l = state.l.max(1);
    let mut flokl = vec![0.0f64; l as usize + 1];
    let mut intkl = vec![0usize; l as usize + 1];
    let mut deltal = vec![0.0f64; l as usize + 1];
    let mut sum43 = 0.0;
    for h in 1..=l as usize {
        let f = (prev_l as f64 / l as f64) * h as f64;
        let ik = f.floor() as usize;
        flokl[h] = f;
        intkl[h] = ik;
        deltal[h] = f - ik as f64;
        let prev_at = |idx: usize| -> f64 {
            // mbelib sets the previous frame's log2Ml[0] to log2Ml[1] (an index-0 read happens when L grows).
            let idx = if idx == 0 { 1 } else { idx };
            state
                .log2_ml
                .get(idx)
                .copied()
                .unwrap_or(*state.log2_ml.last().unwrap_or(&0.0))
        };
        sum43 += (1.0 - deltal[h]) * prev_at(ik) + deltal[h] * prev_at(ik + 1);
    }
    sum43 *= PREDICTOR_RHO / l as f64;

    let sum42: f64 = tl[1..=l as usize].iter().sum::<f64>() / l as f64;
    let big_gamma = gamma - 0.5 * (l as f64).log2() - sum42;

    let mut log2_ml = vec![0.0f64; l as usize + 1];
    let mut ml = vec![0.0f64; l as usize + 1];
    let unvc = 0.2046 / w0.sqrt();
    for h in 1..=l as usize {
        let prev_at = |idx: usize| -> f64 {
            // mbelib sets the previous frame's log2Ml[0] to log2Ml[1] (an index-0 read happens when L grows).
            let idx = if idx == 0 { 1 } else { idx };
            state
                .log2_ml
                .get(idx)
                .copied()
                .unwrap_or(*state.log2_ml.last().unwrap_or(&0.0))
        };
        let c1 = PREDICTOR_RHO * (1.0 - deltal[h]) * prev_at(intkl[h]);
        let c2 = PREDICTOR_RHO * deltal[h] * prev_at(intkl[h] + 1);
        log2_ml[h] = tl[h] + c1 + c2 - sum43 + big_gamma;
        ml[h] = if voiced[h] {
            (0.693 * log2_ml[h]).exp()
        } else {
            unvc * (0.693 * log2_ml[h]).exp()
        };
    }

    state.l = l;
    state.log2_ml = log2_ml;
    state.gamma = gamma;

    DequantizedFrame::Speech(DStarParameters { l, w0, voiced, ml })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambe::float::dstar::whiten_c1;
    use crate::ambe::general::fec::golay_encode;

    /// A real, self-consistent round trip: build a frame from known C0/C1/C2/C3 values (Golay-encode
    /// C0's own data, whiten and Golay-encode C1's), pack it, and confirm `parse_frame` recovers the
    /// exact original 49 data bits with zero corrected errors -- the same load-bearing check
    /// `ambe::general::fec`'s own tests use for its Golay/Hamming implementations, applied here
    /// to this module's own frame assembly and whitening order.
    #[test]
    fn parse_frame_recovers_a_cleanly_encoded_frame_with_zero_errors() {
        let c0_data: u16 = 0b1010_1100_1101; // arbitrary 12-bit value
        let c1_data: u16 = 0b0110_0111_0010;
        let c2: u32 = 0b101_0110_1101; // 11 bits
        let c3: u32 = 0b11_0101_1010_0110; // 14 bits

        let c0_codeword = golay_encode(c0_data); // 23 bits, data in top 12
        let c1_codeword = golay_encode(c1_data);
        let c1_whitened = whiten_c1(c1_codeword, c0_data);

        // C0 field: 23-bit codeword (MSB-first) followed by 1 spare bit (0) as its own LSB.
        let c0_field = c0_codeword << 1;
        let frame: u128 = ((c0_field as u128) << 48)
            | ((c1_whitened as u128) << 25)
            | ((c2 as u128) << 14)
            | (c3 as u128);

        let parsed = parse_frame(frame);
        assert_eq!(
            parsed.epsilon_c0, 0,
            "a cleanly encoded C0 must decode with zero errors"
        );
        assert_eq!(
            parsed.epsilon_c1, 0,
            "a cleanly encoded, correctly-whitened C1 must decode with zero errors"
        );

        let expected_d: u64 =
            ((c0_data as u64) << 37) | ((c1_data as u64) << 25) | ((c2 as u64) << 14) | (c3 as u64);
        assert_eq!(
            parsed.d, expected_d,
            "recovered 49-bit data must exactly match the original"
        );
    }

    /// If `C1` is Golay-decoded *without* first de-whitening it, the result should generally NOT
    /// match the original data -- a real regression guard for the "de-whiten before Golay-decoding,
    /// using C0's own corrected data" ordering `mod.rs`'s own doc comment insists on.
    #[test]
    fn skipping_dewhitening_before_golay_decode_generally_corrupts_c1() {
        let c0_data: u16 = 0x0AB;
        let c1_data: u16 = 0x0CD;
        let c1_codeword = golay_encode(c1_data);
        let c1_whitened = whiten_c1(c1_codeword, c0_data);

        // Golay-decode the still-whitened C1 directly, skipping de-whitening.
        let (wrong_data, _) = golay_decode(c1_whitened);
        assert_ne!(
            wrong_data, c1_data,
            "decoding a still-whitened C1 should not coincidentally recover the right data"
        );
    }

    /// A basic sanity range check on dequantization: for every real, non-tone `b0` value, the
    /// resulting `l` must fall within `L_TABLE`'s own real range (9..=56), and every per-harmonic
    /// spectral amplitude must be finite and non-negative -- catches an indexing panic or a
    /// NaN/negative amplitude across the full parameter space, not just one hand-picked example.
    #[test]
    // Tests [@ANCHOR: dstar_dequantize]
    fn dequantize_produces_finite_nonnegative_amplitudes_across_the_full_b0_range() {
        for b0 in 0u32..126 {
            let raw = RawParameters {
                b0,
                b1: 5,
                b2: 10,
                b3: 100,
                b4: 20,
                b5: 3,
                b6: 3,
                b7: 3,
                b8: 3 << 1,
            };
            let d = super::super::encode::pack_raw_parameters(&raw);
            let mut state = DStarDecoderState::initial();
            match dequantize(d, &mut state) {
                DequantizedFrame::Speech(params) => {
                    assert!(
                        (9..=56).contains(&params.l),
                        "b0={b0}: l={} out of range",
                        params.l
                    );
                    for (h, &m) in params.ml.iter().enumerate().skip(1) {
                        assert!(m.is_finite() && m >= 0.0, "b0={b0}, harmonic {h}: Ml={m}");
                    }
                }
                DequantizedFrame::Tone(_) => panic!("b0={b0} is not a tone value (126/127)"),
            }
        }
    }

    /// `dequantize` advances the decoder state it is given: after a speech frame `state.l` is that
    /// frame's `L` (from `L_TABLE[b0]`), `state.gamma` is `GAMMA_SCALE * DG[b2] + GAMMA_MEMORY *
    /// previous gamma` (the chip-fitted D-STAR constants, not mbelib's `1`/`0.5`), and `log2_ml` has
    /// one entry per harmonic plus the unused index 0. A tone frame returns its payload and leaves
    /// the state exactly as it was.
    #[test]
    // Tests [@ANCHOR: dstar_dequantize]
    fn dequantize_advances_the_state_for_speech_and_leaves_it_alone_for_a_tone() {
        let raw = RawParameters {
            b0: 40,
            b1: 5,
            b2: 21,
            b3: 100,
            b4: 20,
            b5: 3,
            b6: 3,
            b7: 3,
            b8: 3 << 1,
        };
        let d = super::super::encode::pack_raw_parameters(&raw);
        let mut state = DStarDecoderState::initial();
        state.gamma = 7.0; // would feed through if GAMMA_MEMORY were nonzero
        let params = match dequantize(d, &mut state) {
            DequantizedFrame::Speech(p) => p,
            DequantizedFrame::Tone(_) => panic!("b0=40 is a speech frame"),
        };
        let expected_l = tables::L_TABLE[40];
        assert_eq!(params.l, expected_l);
        assert_eq!(state.l, expected_l);
        assert_eq!(state.log2_ml.len(), expected_l as usize + 1);
        assert_eq!(params.voiced.len(), expected_l as usize + 1);
        assert_eq!(params.ml.len(), expected_l as usize + 1);
        let expected_gamma = GAMMA_SCALE * tables::DG[21] + GAMMA_MEMORY * 7.0;
        assert!((state.gamma - expected_gamma).abs() < 1e-12);
        assert!((params.w0 - f0_from_b0(40) * 2.0 * std::f64::consts::PI).abs() < 1e-12);

        // A tone frame (b0 = 126) returns the tone payload and does not touch the state.
        let tone_d = super::super::encode::pack_tone_parameters(32, 200);
        let before = (state.l, state.gamma, state.log2_ml.clone());
        match dequantize(tone_d, &mut state) {
            DequantizedFrame::Tone(t) => assert_eq!((t.index, t.volume), (32, 200)),
            DequantizedFrame::Speech(_) => panic!("b0=126 must dequantize as a tone"),
        }
        assert_eq!((state.l, state.gamma, state.log2_ml.clone()), before);
    }

    /// Known-answer check of the D-STAR bit scatter, one bit at a time: a single set bit `d[i]`
    /// (0 = most significant of the 49) lands in exactly the parameter and bit position the table
    /// in `mod.rs` gives, and `d[24]` (the one bit no parameter reads) changes nothing.
    #[test]
    // Tests [@ANCHOR: dstar_extract_raw_parameters]
    fn extract_raw_parameters_places_each_single_bit_where_the_layout_table_says() {
        fn only(d_index: usize) -> RawParameters {
            extract_raw_parameters(1u64 << (48 - d_index))
        }
        let zero = extract_raw_parameters(0);
        let fields = |r: &RawParameters| [r.b0, r.b1, r.b2, r.b3, r.b4, r.b5, r.b6, r.b7, r.b8];
        assert_eq!(fields(&zero), [0; 9]);
        // b0 = d[0..6) << 1 | d[48]
        assert_eq!(only(0).b0, 0b100_0000);
        assert_eq!(only(5).b0, 0b000_0010);
        assert_eq!(only(48).b0, 1);
        // b1 = d[38..42)
        assert_eq!(only(38).b1, 0b1000);
        assert_eq!(only(41).b1, 0b0001);
        // b2 = d[6..10) << 2 | d[42..44)
        assert_eq!(only(6).b2, 0b10_0000);
        assert_eq!(only(43).b2, 0b00_0001);
        // b3 = d[10..12) << 7 | d[12..17) << 2 | d[44..46)
        assert_eq!(only(10).b3, 1 << 8);
        assert_eq!(only(12).b3, 1 << 6);
        assert_eq!(only(45).b3, 1);
        // b4 = d[17..22) << 2 | d[46..48)
        assert_eq!(only(17).b4, 1 << 6);
        assert_eq!(only(47).b4, 1);
        // b5 = d[22..24) << 2 | d[25..27)
        assert_eq!(only(22).b5, 0b1000);
        assert_eq!(only(26).b5, 0b0001);
        // b6 = d[27..31), b7 = d[31..35)
        assert_eq!(only(27).b6, 0b1000);
        assert_eq!(only(34).b7, 0b0001);
        // b8 = d[35..38) << 1: its low bit is never filled from the wire
        assert_eq!(only(35).b8, 0b1000);
        assert_eq!(only(37).b8, 0b0010);
        // d[24] is read by no parameter.
        assert_eq!(fields(&only(24)), [0; 9]);
    }

    /// `classify_b0` must match mbelib's own real `ambe3600x2400.c` trigger exactly:
    /// `(b0 & 0x7E) == 0x7E`, i.e. *only* 126 and 127, not the wider 120-127 block AMBE+2 half-rate
    /// reserves -- confirmed against a real chip capture where D-STAR's own `DTX_ENABLE` drove a
    /// silence frame to `b0=120`, a value mbelib itself does not special-case (see this module's own
    /// doc comment above `FrameKind`).
    #[test]
    fn classify_b0_matches_mbelib_exactly_not_the_wider_ambe_plus_2_range() {
        for b0 in 0u32..126 {
            assert_eq!(
                classify_b0(b0),
                FrameKind::Speech,
                "b0={b0} must not classify as Tone"
            );
        }
        assert_eq!(classify_b0(126), FrameKind::Tone);
        assert_eq!(classify_b0(127), FrameKind::Tone);
    }

    /// Real chip captures (D-STAR RATEP, `TD_ENABLE` on, `AMBE_CHIP_VALIDATION_FINDINGS.md`'s
    /// cross-mode DTX/DTMF section) of all 16 DTMF digits and a plain 200Hz tone, decoded through
    /// this module's real wire-format/FEC layer end to end -- confirms `classify_b0` reads `Tone`
    /// for all 17 stimuli, and that `dtmf_digit_from_tone_index(decode_tone(d).index)` recovers the
    /// exact row/column pair for every one of the 16 digits (and correctly returns `None` for the
    /// plain tone), straight from the chip's own live response, not a synthetic round trip.
    #[test]
    fn real_chip_capture_dtmf_digits_and_tone_decode_correctly() {
        type Case = (&'static str, &'static str, Option<(u8, u8)>);
        let cases: [Case; 17] = [
            ("digit 1 (row 0, col 0)", "d30824f38319304110", Some((0, 0))),
            ("digit 2 (row 0, col 1)", "d30824f38359304110", Some((0, 1))),
            ("digit 3 (row 0, col 2)", "d30824f39319304110", Some((0, 2))),
            ("digit A (row 0, col 3)", "d30824f39359304110", Some((0, 3))),
            ("digit 4 (row 1, col 0)", "83092082c51540c62c", Some((1, 0))),
            ("digit 5 (row 1, col 1)", "83092082c55540c62c", Some((1, 1))),
            ("digit 6 (row 1, col 2)", "83092082d51540c62c", Some((1, 2))),
            ("digit B (row 1, col 3)", "83092082d55540c62c", Some((1, 3))),
            ("digit 7 (row 2, col 0)", "83883ce38409418e10", Some((2, 0))),
            ("digit 8 (row 2, col 1)", "83883ce38449418e10", Some((2, 1))),
            ("digit 9 (row 2, col 2)", "83883ce39409418e10", Some((2, 2))),
            ("digit C (row 2, col 3)", "83883ce39449418e10", Some((2, 3))),
            ("digit * (row 3, col 0)", "d30838928201310b2c", Some((3, 0))),
            ("digit 0 (row 3, col 1)", "d30838928241310b2c", Some((3, 1))),
            ("digit # (row 3, col 2)", "d30838929201310b2c", Some((3, 2))),
            ("digit D (row 3, col 3)", "d30838929241310b2c", Some((3, 3))),
            ("plain 200Hz tone (not DTMF)", "c2c92cb20741e00918", None),
        ];
        for (label, hex, expected_row_col) in cases {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let wire_bytes: [u8; 9] = bytes.try_into().unwrap();
            let frame = crate::ambe::float::dstar::interleave::wire_bytes_to_frame(&wire_bytes);
            let parsed = parse_frame(frame);
            assert_eq!(
                parsed.epsilon_c0, 0,
                "{label}: C0 must decode with zero errors"
            );
            assert_eq!(
                parsed.epsilon_c1, 0,
                "{label}: C1 must decode with zero errors"
            );
            let raw = extract_raw_parameters(parsed.d);
            assert_eq!(
                classify_b0(raw.b0),
                FrameKind::Tone,
                "{label}: b0={} must classify as Tone",
                raw.b0
            );
            let tone = decode_tone(parsed.d);
            assert_eq!(
                dtmf_digit_from_tone_index(tone.index),
                expected_row_col,
                "{label}: tone index={}",
                tone.index
            );
        }
    }
}
