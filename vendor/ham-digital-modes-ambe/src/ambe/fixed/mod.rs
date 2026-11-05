//! Fixed-point AMBE implementations, mirroring [`super::float`]'s per-mode layout
//! (`tia_102_baba`/`dstar`/`ambe_plus_2`, plus [`general`] for arithmetic primitives genuinely specific
//! to a fixed-point build -- not to be confused with [`super::general`], the sibling module that
//! holds precision-*independent* shared code like the Golay/Hamming FEC, reused unchanged by both
//! [`super::float`] and this tree). Every function in this tree must compile and run correctly
//! using only integer and fixed-point arithmetic -- no `f32`/`f64`, no soft-float trap/emulation
//! reliance -- so the resulting code is suitable for a CPU with no floating-point unit at all, per
//! Bruce's own direct instruction. Each module's own doc comment states its fixed-point format
//! (e.g. Q-format word width and fractional bits) and cites the floating-point sibling it was
//! derived from, so the two can be cross-checked against the same real chip captures and frozen
//! fixtures the floating-point implementations already use.
//!
//! **Numeric convention, used consistently unless a module's own doc comment says otherwise**: a
//! signed quantity with real fractional range (amplitudes, log-domain values, frequencies in
//! radians/sample) is Q16.16 -- a plain `i32` whose low 16 bits are the fraction, documented per
//! field as `/// Q16.16`, with `i64` used for intermediate products before shifting back down. A
//! phase accumulator (an oscillator's running angle) is instead a wrapping `u32` where a full turn
//! (2*pi radians) is `1u32 << 32` (i.e. it wraps for free on overflow) -- a different convention
//! from Q16.16 because a phase's own overflow-wraps-around behavior *is* the modulo-2*pi reduction,
//! not something to guard against. There is deliberately no generic `Fixed<M, N>` wrapper type:
//! different quantities need different scaling, and plain, individually-documented integers make
//! that visible at every call site instead of hiding it behind one shared type.
//!
//! **Enforcement, not just intent**: Bruce's own words were "without any floating point support
//! whatsoever," so this is checked, not merely written carefully. `#![deny(clippy::float_arithmetic)]`
//! below fails the build on any `f32`/`f64` arithmetic expression anywhere in this module tree
//! (confirmed to actually fire, `cargo clippy -- -D clippy::float_arithmetic`), and
//! `tests/ambe_fixed_no_float_tokens.rs` independently greps every `src/ambe/fixed/**/*.rs` file
//! for the literal tokens `f32`/`f64`/a bare float literal on a non-comment, non-doc-comment line,
//! so a future refactor that routes around the lint (e.g. via a type alias) still gets caught.
//!
//! **Tolerance, decided before any arithmetic is written, not discovered by adjusting a failing
//! test until it's green**: a fixed-point `dequantize`'s per-harmonic amplitude must be within 1%
//! of the floating-point sibling's own output for the same input bits (chosen because 16-bit PCM's
//! own quantization step is already about 0.0015% of full scale, so 1% here is the fixed-point
//! port's own error budget, not an artifact of PCM resolution). Once/if a mode's synthesis is
//! ported, its PCM output must reach at least 40 dB SNR against the floating-point sibling's own
//! PCM for the same input. Code reused unchanged from [`super::general`] (the bit/FEC layer) needs
//! no tolerance at all -- it is byte-identical to the floating-point build by construction, since
//! it is the exact same code.
#![deny(clippy::float_arithmetic)]

#[cfg(feature = "ambe_plus_2")]
pub mod ambe_plus_2;
pub mod dstar;
pub mod general;
pub mod mbe_encode;
pub mod tia_102_baba;
