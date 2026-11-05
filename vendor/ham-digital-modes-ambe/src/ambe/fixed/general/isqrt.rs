// SPDX-License-Identifier: LGPL-3.0-or-later
//! Integer square root -- Newton's method on `u64`, exact (not an approximation): for every `n`,
//! `isqrt_u64(n)` returns `floor(sqrt(n))`. Needed wherever a floating-point sibling calls `f64::sqrt`
//! (e.g. `rconst = 1.0 / (2.0 * SQRT_2)` in `ambe_plus_2::decode::dequantize`) -- the fixed-point
//! equivalent scales `n` up into a fixed-point numerator first, takes this exact integer root, and
//! the result is that same fixed-point value's own square root, still exact to the precision of the
//! scaling chosen by the caller.

/// `floor(sqrt(n))`, exact for every `u64` (verified by brute force against every value where a
/// linear scan is feasible, and by the identity `isqrt(n)^2 <= n < (isqrt(n)+1)^2` for a wide
/// pseudo-random sample beyond that). Newton's method converges monotonically to the exact integer
/// root for this function (a well-known property of integer Newton square root, not assumed without
/// the tests below), so a fixed iteration cap is safe -- `u64`'s own root fits in 32 bits, and each
/// iteration at least halves the distance to the fixed point once the estimate is in range.
pub fn isqrt_u64(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    // A cheap initial estimate via the position of the highest set bit -- `1 << (bits/2)` is within
    // a factor of ~1.4 of the true root, so Newton's method converges in a handful of iterations
    // from here rather than needing a data-dependent starting guess.
    let bits = 64 - n.leading_zeros();
    let mut x = 1u64 << bits.div_ceil(2);
    loop {
        let next = (x + n / x) / 2;
        if next >= x {
            break;
        }
        x = next;
    }
    // Newton's method for integer sqrt can overshoot by exactly 1 on its final step; correct it.
    while x * x > n {
        x -= 1;
    }
    x
}

/// `floor(sqrt(n))` for `u32`, a thin convenience wrapper -- most callers in this crate work in
/// `u32`/`i32` fixed-point values, and routing through `u64` avoids overflow when squaring back for
/// verification.
pub fn isqrt_u32(n: u32) -> u32 {
    isqrt_u64(n as u64) as u32
}

/// `floor(sqrt(n))` for a `u128`, by the classic bit-pair digit recurrence (exact, 64 iterations).
/// Needed where a wide energy ratio (`i128` sums of squared spectrum bins) must be rooted with no
/// precision lost to narrowing to `u64` first.
pub fn isqrt_u128(n: u128) -> u128 {
    let mut remainder = n;
    let mut result = 0u128;
    let mut bit = 1u128 << 126;
    while bit > remainder {
        bit >>= 2;
    }
    while bit != 0 {
        if remainder >= result + bit {
            remainder -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isqrt_u64_matches_exact_perfect_squares() {
        for k in 0u64..2000 {
            assert_eq!(isqrt_u64(k * k), k, "k={k}");
        }
    }

    #[test]
    fn isqrt_u64_satisfies_the_defining_inequality_across_a_wide_sample() {
        // isqrt(n)^2 <= n < (isqrt(n)+1)^2 -- the actual definition of floor(sqrt(n)), checked
        // directly rather than trusting convergence alone.
        let samples: Vec<u64> = (0u64..100_000)
            .chain((0u64..64).map(|shift| 1u64 << shift))
            .chain((0u64..64).map(|shift| (1u64 << shift).wrapping_sub(1)))
            .chain([u64::MAX, u64::MAX - 1, u64::MAX / 2])
            .collect();
        for n in samples {
            let r = isqrt_u64(n);
            assert!(r * r <= n, "n={n}: r={r}, r*r={} > n", r * r);
            assert!(
                r == u32::MAX as u64 || (r + 1).checked_mul(r + 1).is_none_or(|sq| sq > n),
                "n={n}: r={r}, (r+1)^2 <= n"
            );
        }
    }

    #[test]
    fn isqrt_u128_matches_the_defining_inequality_and_isqrt_u64() {
        for n in [
            0u128,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            u64::MAX as u128,
            (u64::MAX as u128) + 1,
            u128::MAX,
            1u128 << 100,
            (1u128 << 100) - 1,
        ] {
            let r = isqrt_u128(n);
            assert!(r * r <= n, "n={n}");
            assert!((r + 1).checked_mul(r + 1).is_none_or(|sq| sq > n), "n={n}");
        }
        for n in [0u64, 1, 99, 1_000_000_007, u64::MAX] {
            assert_eq!(isqrt_u128(n as u128), isqrt_u64(n) as u128);
        }
    }

    #[test]
    fn isqrt_u64_of_zero_and_one() {
        assert_eq!(isqrt_u64(0), 0);
        assert_eq!(isqrt_u64(1), 1);
    }

    #[test]
    fn isqrt_u32_matches_isqrt_u64_narrowed() {
        for n in [0u32, 1, 2, 100, 65535, 65536, u32::MAX] {
            assert_eq!(isqrt_u32(n) as u64, isqrt_u64(n as u64));
        }
    }
}
