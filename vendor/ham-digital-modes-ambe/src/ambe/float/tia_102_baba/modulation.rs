//! Bit modulation (TIA-102.BABA_2003.pdf section 7.4, Eq. 84-94): XORs each of the eight FEC code
//! vectors `nu_hat_0..nu_hat_7` ([`super::fec`]'s Golay/Hamming output) with a data-dependent
//! pseudo-random sequence, so that a corrupted `nu_hat_0` (undetected by the `[23,12]` Golay code's
//! own 3-error correction limit) desynchronizes the decoder's own copy of the sequence -- turning an
//! otherwise-silent decoding error into a detectable, effectively-50%-BER garbling of
//! `nu_hat_1..nu_hat_6`, which the decoder's later frame-repeat/mute logic can catch.
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 43-44.

/// The pseudo-random sequence `p_r(n)` for `n` in `0..=114` (Eq. 84-85): a linear congruential
/// generator seeded from `u_hat_0` (interpreted as a plain unsigned 12-bit number, `0..=4095`, per
/// the spec's own stated range for that bit vector) and iterated `173*p_r(n-1) + 13849 mod 65536`.
// [@ANCHOR: pseudo_random_sequence]
pub fn pseudo_random_sequence(u0: u32) -> [u16; 115] {
    let mut pr = [0u16; 115];
    pr[0] = (16 * u0) as u16; // u0 <= 4095, so 16*u0 <= 65520, always fits in 16 bits
    for n in 1..=114 {
        let prev = pr[n - 1] as u32;
        pr[n] = ((173 * prev + 13849) % 65536) as u16;
    }
    pr
}

/// The eight binary modulation vectors `m_hat_0..m_hat_7` (Eq. 86-93), packed as plain integers
/// (matching [`super::fec::golay_encode`]/[`super::fec::hamming_encode`]'s own return convention):
/// `m_hat_0` and `m_hat_7` are always all-zero (`nu_hat_0` is never modulated, so the decoder can
/// always Golay-decode it correctly to bootstrap the rest; `nu_hat_7` carries no FEC to desync in
/// the first place), while `m_hat_1..m_hat_3` (23 bits each, matching the `[23,12]` Golay codeword
/// length) and `m_hat_4..m_hat_6` (15 bits each, matching the `[15,11]` Hamming codeword length) are
/// each built from their own successive, non-overlapping slice of `p_r(n)`'s own top bit
/// (`floor(p_r(n)/32768)`, MSB-first).
// [@ANCHOR: modulation_vectors]
pub fn modulation_vectors(u0: u32) -> [u32; 8] {
    let pr = pseudo_random_sequence(u0);
    let top_bit = |n: usize| -> u32 {
        if pr[n] as u32 >= 32768 {
            1
        } else {
            0
        }
    };
    let build = |range: std::ops::RangeInclusive<usize>| -> u32 {
        range.fold(0u32, |acc, n| (acc << 1) | top_bit(n))
    };
    [
        0,
        build(1..=23),
        build(24..=46),
        build(47..=69),
        build(70..=84),
        build(85..=99),
        build(100..=114),
        0,
    ]
}

/// The modulated code vectors `c_hat_0..c_hat_7` (Eq. 94): each FEC code vector XORed
/// (`+` modulo 2) with its own modulation vector.
pub fn modulate_code_vectors(nu: [u32; 8], u0: u32) -> [u32; 8] {
    let m = modulation_vectors(u0);
    std::array::from_fn(|i| nu[i] ^ m[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Tests [@ANCHOR: pseudo_random_sequence]
    fn pseudo_random_sequence_matches_eq84_85_at_hand_computed_values() {
        let pr = pseudo_random_sequence(0);
        assert_eq!(pr[0], 0, "Eq. 84: p_r(0) = 16*0 = 0");
        assert_eq!(
            pr[1], 13849,
            "Eq. 85: p_r(1) = (173*0 + 13849) mod 65536 = 13849"
        );
        let expected_pr2 = (173u32 * 13849 + 13849) % 65536;
        assert_eq!(pr[2] as u32, expected_pr2);
    }

    #[test]
    fn pseudo_random_sequence_seeds_from_u0_per_eq84() {
        let pr = pseudo_random_sequence(100);
        assert_eq!(pr[0], 1600, "Eq. 84: p_r(0) = 16*100 = 1600");
    }

    #[test]
    // Tests [@ANCHOR: modulation_vectors]
    fn modulation_vectors_m0_and_m7_are_always_zero() {
        for u0 in [0u32, 1, 100, 4095] {
            let m = modulation_vectors(u0);
            assert_eq!(m[0], 0, "m_hat_0 must always be all-zero (Eq. 86)");
            assert_eq!(m[7], 0, "m_hat_7 must always be all-zero (Eq. 93)");
        }
    }

    #[test]
    fn modulation_vectors_first_bit_matches_prs_own_top_bit() {
        let u0 = 4095;
        let pr = pseudo_random_sequence(u0);
        let m = modulation_vectors(u0);
        let expected_bit1_msb = if pr[1] as u32 >= 32768 { 1u32 } else { 0 };
        assert_eq!(
            (m[1] >> 22) & 1,
            expected_bit1_msb,
            "m_hat_1's own MSB is p_r(1)'s top bit"
        );
    }

    #[test]
    fn modulate_code_vectors_leaves_c0_and_c7_unchanged() {
        let nu = [
            0b101, 0xABCDEF, 0x123456, 0x654321, 0x1234, 0x5678, 0x4321, 0b1010101,
        ];
        for u0 in [0u32, 4095] {
            let c = modulate_code_vectors(nu, u0);
            assert_eq!(c[0], nu[0], "nu_hat_0 is never modulated");
            assert_eq!(c[7], nu[7], "nu_hat_7 is never modulated");
        }
    }

    #[test]
    fn modulate_code_vectors_is_its_own_inverse() {
        // XOR with the same modulation vector twice recovers the original -- the real property
        // the decoder's own demodulation step relies on.
        let nu = [
            0b101, 0xABCDEF, 0x123456, 0x654321, 0x1234, 0x5678, 0x4321, 0b1010101,
        ];
        let u0 = 2024;
        let c = modulate_code_vectors(nu, u0);
        let recovered = modulate_code_vectors(c, u0);
        assert_eq!(recovered, nu);
    }
}
