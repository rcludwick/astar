//! Intra-frame bit interleaving (TIA-102.BABA_2003.pdf Annex H, "Bit Frame Format") -- rearranges
//! the 144 bits across the modulated code vectors `c_hat_0..c_hat_7` ([`super::encode_code_vectors`]'s
//! own output) into 72 two-bit "dibit" symbols, so a short burst of channel errors (which tend to
//! corrupt physically-adjacent symbols) gets spread across several different error-correction code
//! words rather than concentrated in one -- section 7.5's own stated purpose ("used to spread short
//! bursts of errors among several code words... minimum separation between any two bits of the same
//! error correction code is 3 symbols").
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf page 110 (Annex H), following the same
//! discipline as every other numeric table in this spec -- though this table, unlike Annex F/G,
//! turned out to already match `pdftotext`'s own extraction exactly at every one of its 288 numeric
//! fields (no Type3 digit-glyph corruption found here); the 600 DPI render was still checked
//! character-by-character before trusting it, not skipped on the assumption this table would also be
//! clean.
//!
//! **A real, disclosed scope boundary, narrower than the one `mod.rs`'s own module doc already
//! records, not a contradiction of it**: section 7.5's own text states plainly that "the speech coder
//! bits should be inserted into the Project 25 frame format beginning with symbol 0 and ending with
//! symbol 1[,] ... described more completely in the Project 25 Common Air Interface specification."
//! So this module produces the 72 dibit symbols this document fully and completely defines -- but
//! *where* those 72 symbols land inside an actual transmitted P25 channel frame (timing, sync
//! patterns, other channel overhead) remains the CAI's own concern, exactly as `mod.rs`'s doc comment
//! already says for the step before this one.
//!
//! **A real, verified-not-assumed oddity in that exact quote, disclosed rather than silently
//! "corrected"**: "ending with symbol 1" reads as if it should say "symbol 71" (there are 72 symbols,
//! numbered 0 through 71, and "beginning with symbol 0" pairs naturally with "ending with" the last
//! one). Checked three independent ways before concluding it's real, not an artifact: `pdftotext`'s
//! own text layer, a 600 DPI pixel render zoomed in on exactly that word, and -- deepest, and the one
//! that actually settles it -- PyMuPDF's raw per-character extraction (`page.get_text("rawdict")`),
//! which reports the exact character code and x-position PDF places at that spot independent of both
//! `pdftotext`'s broken cmap and the rasterizer. That extraction shows a single glyph, character code
//! `0x01`, at one x-position, with nothing else nearby -- and per this document's own established
//! convention (literal ASCII `'0'` for zero, control codes `0x01`-`0x09` for digits 1-9, confirmed
//! independently across every other table this session transcribed), `0x01` unambiguously means `1`.
//! "symbol 0" two words earlier in the same sentence uses the literal `'0'` character, for direct
//! comparison. This rules out a dropped or merged `7` glyph at every level checkable from the PDF
//! itself: the source document's own author genuinely wrote "symbol 1," not "symbol 71" -- a real
//! error or unclear phrasing in the underlying text, not a rendering or extraction artifact. Since
//! the sentence explicitly defers "more completely" to a separate document this codebase doesn't
//! have, and doesn't affect anything this module actually computes (the 72-symbol interleaving above
//! is unaffected either way), it's recorded here rather than silently resolved in either direction.
//!
//! **Verified against a real structural invariant before being trusted, not just visually
//! re-checked**: the 72 symbols' own Bit1/Bit0 fields, taken together, are a real bijection over all
//! 144 `(c_j, bit_index)` pairs -- every real bit of every code vector (23 bits each for
//! `c_hat_0..c_hat_3`, 15 each for `c_hat_4..c_hat_6`, 7 for `c_hat_7`) appears in exactly one symbol
//! slot, with no gaps and no duplicates. Checked programmatically (a Python script parsing the raw
//! `pdftotext` output directly, cross-checked against the two rows that script's own regex missed due
//! to a `pdftotext` line-wrap artifact -- symbols 28 and 64, both visually confirmed at 600 DPI before
//! being added by hand) before any of the table below was written down.

/// One row of Annex H's own table: `(bit1_source, bit0_source)`, where each source is `(vector_index,
/// bit_index)` -- `vector_index` selects which of `c_hat_0..c_hat_7` the bit comes from, and
/// `bit_index` is that vector's own bit position (bit 0 = LSB, matching this codec's existing
/// convention throughout `fec.rs`/`modulation.rs`).
type BitSource = (u8, u8);

/// Annex H's own 72-row table, in symbol order (`BIT_FRAME_FORMAT[n]` is symbol `n`'s own
/// `(bit1_source, bit0_source)`).
#[rustfmt::skip]
const BIT_FRAME_FORMAT: [(BitSource, BitSource); 72] = [
    ((0, 22), (1, 21)), // symbol 0
    ((2, 20), (3, 19)), // symbol 1
    ((4, 10), (5, 1)),  // symbol 2
    ((1, 20), (0, 21)), // symbol 3
    ((3, 18), (2, 19)), // symbol 4
    ((5, 0),  (4, 9)),  // symbol 5
    ((0, 20), (1, 19)), // symbol 6
    ((2, 18), (3, 17)), // symbol 7
    ((4, 8),  (6, 14)), // symbol 8
    ((1, 18), (0, 19)), // symbol 9
    ((3, 16), (2, 17)), // symbol 10
    ((6, 13), (4, 7)),  // symbol 11
    ((0, 18), (1, 17)), // symbol 12
    ((2, 16), (3, 15)), // symbol 13
    ((4, 6),  (6, 12)), // symbol 14
    ((1, 16), (0, 17)), // symbol 15
    ((3, 14), (2, 15)), // symbol 16
    ((6, 11), (4, 5)),  // symbol 17
    ((0, 16), (1, 15)), // symbol 18
    ((2, 14), (3, 13)), // symbol 19
    ((4, 4),  (6, 10)), // symbol 20
    ((1, 14), (0, 15)), // symbol 21
    ((3, 12), (2, 13)), // symbol 22
    ((6, 9),  (4, 3)),  // symbol 23
    ((0, 14), (1, 13)), // symbol 24
    ((2, 12), (3, 11)), // symbol 25
    ((4, 2),  (6, 8)),  // symbol 26
    ((1, 12), (0, 13)), // symbol 27
    ((3, 10), (2, 11)), // symbol 28
    ((6, 7),  (4, 1)),  // symbol 29
    ((0, 12), (1, 11)), // symbol 30
    ((2, 10), (3, 9)),  // symbol 31
    ((4, 0),  (6, 6)),  // symbol 32
    ((1, 10), (0, 11)), // symbol 33
    ((3, 8),  (2, 9)),  // symbol 34
    ((6, 5),  (5, 14)), // symbol 35
    ((0, 10), (1, 9)),  // symbol 36
    ((2, 8),  (3, 7)),  // symbol 37
    ((5, 13), (6, 4)),  // symbol 38
    ((1, 8),  (0, 9)),  // symbol 39
    ((3, 6),  (2, 7)),  // symbol 40
    ((6, 3),  (5, 12)), // symbol 41
    ((0, 8),  (1, 7)),  // symbol 42
    ((2, 6),  (3, 5)),  // symbol 43
    ((5, 11), (6, 2)),  // symbol 44
    ((1, 6),  (0, 7)),  // symbol 45
    ((3, 4),  (2, 5)),  // symbol 46
    ((6, 1),  (5, 10)), // symbol 47
    ((0, 6),  (1, 5)),  // symbol 48
    ((2, 4),  (3, 3)),  // symbol 49
    ((5, 9),  (6, 0)),  // symbol 50
    ((1, 4),  (0, 5)),  // symbol 51
    ((3, 2),  (2, 3)),  // symbol 52
    ((7, 6),  (5, 8)),  // symbol 53
    ((0, 4),  (1, 3)),  // symbol 54
    ((2, 2),  (3, 1)),  // symbol 55
    ((5, 7),  (7, 5)),  // symbol 56
    ((1, 2),  (0, 3)),  // symbol 57
    ((3, 0),  (2, 1)),  // symbol 58
    ((7, 4),  (5, 6)),  // symbol 59
    ((0, 2),  (1, 1)),  // symbol 60
    ((2, 0),  (4, 14)), // symbol 61
    ((5, 5),  (7, 3)),  // symbol 62
    ((1, 0),  (0, 1)),  // symbol 63
    ((4, 13), (3, 22)), // symbol 64
    ((7, 2),  (5, 4)),  // symbol 65
    ((0, 0),  (2, 22)), // symbol 66
    ((3, 21), (4, 12)), // symbol 67
    ((5, 3),  (7, 1)),  // symbol 68
    ((2, 21), (1, 22)), // symbol 69
    ((4, 11), (3, 20)), // symbol 70
    ((7, 0),  (5, 2)),  // symbol 71
];

/// Extracts bit `source.1` (0 = LSB) from code vector `c[source.0]`.
fn extract_bit(c: &[u32; 8], source: BitSource) -> bool {
    (c[source.0 as usize] >> source.1) & 1 == 1
}

/// Interleaves the eight modulated code vectors into Annex H's own 72 dibit symbols, each
/// `(bit1, bit0)` -- `bit1` is the symbol's own MSB, `bit0` its LSB, matching Annex H's own stated
/// convention ("bit N-1 ... is the MSB of each vector and bit 0 is the LSB").
pub fn interleave_to_dibit_symbols(c: [u32; 8]) -> [(bool, bool); 72] {
    std::array::from_fn(|i| {
        let (bit1_source, bit0_source) = BIT_FRAME_FORMAT[i];
        (extract_bit(&c, bit1_source), extract_bit(&c, bit0_source))
    })
}

/// Sets bit `source.1` (0 = LSB) in code vector `c[source.0]` to `value`.
// [@ANCHOR: set_bit]
fn set_bit(c: &mut [u32; 8], source: BitSource, value: bool) {
    if value {
        c[source.0 as usize] |= 1 << source.1;
    }
}

/// The exact inverse of [`interleave_to_dibit_symbols`]: reassembles the eight code vectors from 72
/// received dibit symbols. Total by construction (every real bit position is written exactly once,
/// per this module's own bijection check on `BIT_FRAME_FORMAT`), so it never panics and every real
/// bit of every vector is always written -- no zero-initialization gap to worry about.
// [@ANCHOR: deinterleave_from_dibit_symbols]
pub fn deinterleave_from_dibit_symbols(symbols: [(bool, bool); 72]) -> [u32; 8] {
    let mut c = [0u32; 8];
    for (i, &(bit1, bit0)) in symbols.iter().enumerate() {
        let (bit1_source, bit0_source) = BIT_FRAME_FORMAT[i];
        set_bit(&mut c, bit1_source, bit1);
        set_bit(&mut c, bit0_source, bit0);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real, load-bearing structural check: Annex H's own table, taken as a whole, must be a
    /// bijection over all 144 real `(vector, bit)` positions across the eight code vectors (23 bits
    /// each for `c0..c3`, 15 each for `c4..c6`, 7 for `c7`) -- every real bit appears in exactly one
    /// symbol's Bit1 or Bit0 slot, with no gaps and no duplicates. This is what was checked
    /// programmatically (see this module's own doc comment) before any of `BIT_FRAME_FORMAT` was
    /// trusted, and it's re-checked here so a future edit to the table can't silently break it.
    #[test]
    fn bit_frame_format_is_a_real_bijection_over_all_144_bit_positions() {
        let widths: [u8; 8] = [23, 23, 23, 23, 15, 15, 15, 7];
        let mut seen: std::collections::HashSet<(u8, u8)> = std::collections::HashSet::new();
        for &(bit1, bit0) in BIT_FRAME_FORMAT.iter() {
            for source in [bit1, bit0] {
                assert!(
                    source.1 < widths[source.0 as usize],
                    "vector {} bit {} exceeds its own real width {}",
                    source.0,
                    source.1,
                    widths[source.0 as usize]
                );
                assert!(
                    seen.insert(source),
                    "vector {} bit {} appears more than once across BIT_FRAME_FORMAT",
                    source.0,
                    source.1
                );
            }
        }
        let expected_total: usize = widths.iter().map(|&w| w as usize).sum();
        assert_eq!(expected_total, super::super::FRAME_BITS);
        assert_eq!(seen.len(), expected_total);
    }

    #[test]
    fn interleave_to_dibit_symbols_extracts_the_real_bits_per_annex_h() {
        // c0 = all-ones (23 bits), everything else zero -- every symbol whose bit1/bit0 source is
        // c0 should read `true`; every other symbol should read `false` in that slot.
        let mut c = [0u32; 8];
        c[0] = (1 << 23) - 1;
        let symbols = interleave_to_dibit_symbols(c);
        for (i, &(bit1, bit0)) in symbols.iter().enumerate() {
            let (bit1_source, bit0_source) = BIT_FRAME_FORMAT[i];
            assert_eq!(bit1, bit1_source.0 == 0, "symbol {i} bit1");
            assert_eq!(bit0, bit0_source.0 == 0, "symbol {i} bit0");
        }
    }

    #[test]
    fn symbol_0_matches_annex_h_hand_read_values() {
        // Symbol 0: bit1 = c0's bit 22 (its own MSB), bit0 = c1's bit 21.
        let mut c = [0u32; 8];
        c[0] = 1 << 22;
        c[1] = 1 << 21;
        let symbols = interleave_to_dibit_symbols(c);
        assert_eq!(symbols[0], (true, true));
    }

    /// The real property [`deinterleave_from_dibit_symbols`] depends on: since `BIT_FRAME_FORMAT` is
    /// a checked bijection over all 144 real bit positions (see the bijection test above),
    /// deinterleaving an interleaved frame must recover it exactly, for any real bit pattern -- not
    /// just a hand-picked one.
    #[test]
    // Tests [@ANCHOR: deinterleave_from_dibit_symbols]
    // Tests [@ANCHOR: set_bit]
    fn deinterleave_is_the_exact_inverse_of_interleave_for_several_real_bit_patterns() {
        let widths: [u32; 8] = [23, 23, 23, 23, 15, 15, 15, 7];
        let patterns: [[u32; 8]; 4] = [
            [0; 8],
            std::array::from_fn(|i| (1u32 << widths[i]) - 1), // all real bits set
            [
                0b101_0101_0101_0101_0101_0101,
                0x2AAAAA,
                0x555555,
                0x2AAAAA,
                0x5555,
                0x2AAA,
                0x5555,
                0x55,
            ],
            [1, 2, 4, 8, 1, 2, 4, 1],
        ];
        for c in patterns {
            let symbols = interleave_to_dibit_symbols(c);
            let recovered = deinterleave_from_dibit_symbols(symbols);
            for i in 0..8 {
                let mask = (1u32 << widths[i]) - 1;
                assert_eq!(
                    recovered[i] & mask,
                    c[i] & mask,
                    "vector {i}: recovered {:#x}, expected {:#x}",
                    recovered[i] & mask,
                    c[i] & mask
                );
            }
        }
    }
}
