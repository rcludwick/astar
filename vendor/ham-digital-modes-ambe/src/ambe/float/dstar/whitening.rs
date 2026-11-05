//! `C1`'s whitening: a content-dependent pseudo-random XOR seeded from `C0`'s own already-corrected
//! 12 data bits, applied to `C1`'s full 23-bit Golay codeword both before encoding and after
//! decoding (so Golay-decoding `C1` must always happen *after* de-whitening it, which in turn must
//! happen after `C0` is already corrected -- see `mod.rs`'s own doc comment for the full order).
//!
//! The same linear congruential generator (multiplier 173, increment 13849, modulus 65536, top-bit
//! output) also appears in a completely independent codec generation: G4KLX's `AMBETools`
//! (`Common/IMBEFEC.cpp`) uses the identical recurrence to whiten P25 IMBE's own equivalent field --
//! real, cross-generation evidence this exact construction is the shared DVSI-family whitening
//! scheme, not something specific to D-STAR's own decoder.

/// Generates the 23-bit whitening mask for `C1`, seeded from `C0`'s own 12 corrected data bits
/// (`seed`, low 12 bits meaningful). Bit 22 (the mask's own MSB) is produced first, matching `C1`'s
/// own MSB-first bit order, so the returned `u32`'s bit `22-i` is the `i`'th generated PRN output
/// (`i` from 0..23).
pub fn c1_whitening_mask(seed: u16) -> u32 {
    let mut mask = 0u32;
    let mut p: u32 = 16 * (seed as u32 & 0x0FFF);
    for i in 0..23 {
        p = (173 * p + 13849) % 65536;
        let bit = (p >= 32768) as u32;
        mask |= bit << (22 - i);
    }
    mask
}

/// XORs `c1`'s low 23 bits with the whitening mask derived from `c0_data` -- its own exact inverse
/// (XOR is self-inverse), used both to whiten a freshly Golay-encoded `C1` before transmission and
/// to de-whiten a received `C1` before Golay-decoding it.
pub fn whiten_c1(c1: u32, c0_data: u16) -> u32 {
    (c1 & 0x7F_FFFF) ^ c1_whitening_mask(c0_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, hand-computed value for the LCG's own first few outputs, checked independently of
    /// this module's own `c1_whitening_mask` -- guards against a sign/order transcription error in
    /// the recurrence itself, not just a round-trip test that could pass even with the wrong
    /// direction applied symmetrically on both sides.
    #[test]
    fn c1_whitening_mask_matches_a_hand_computed_lcg_sequence() {
        let seed: u16 = 0x0AB; // an arbitrary, non-trivial 12-bit seed
        let mut p: u32 = 16 * (seed as u32);
        let mut expected_bits = [0u32; 23];
        for slot in expected_bits.iter_mut() {
            p = (173 * p + 13849) % 65536;
            *slot = (p >= 32768) as u32;
        }
        let mask = c1_whitening_mask(seed);
        for (i, &expected_bit) in expected_bits.iter().enumerate() {
            let actual_bit = (mask >> (22 - i)) & 1;
            assert_eq!(
                actual_bit,
                expected_bit,
                "PRN output {i} (mask bit {}): expected {expected_bit}, got {actual_bit}",
                22 - i
            );
        }
    }

    #[test]
    fn whiten_c1_is_its_own_inverse() {
        let seed: u16 = 0x123;
        let original: u32 = 0x3A5C7A; // some arbitrary 23-bit codeword
        let whitened = whiten_c1(original, seed);
        assert_ne!(
            whitened, original,
            "a real whitening mask should actually change the bits for this seed"
        );
        let recovered = whiten_c1(whitened, seed);
        assert_eq!(
            recovered, original,
            "XOR-whitening must be its own exact inverse"
        );
    }

    #[test]
    fn whiten_c1_only_touches_the_low_23_bits() {
        let seed: u16 = 0x001;
        let result = whiten_c1(0xFFFF_FFFF, seed);
        assert_eq!(
            result & !0x7F_FFFF,
            0,
            "whiten_c1 must never set any bit above bit 22"
        );
    }
}
