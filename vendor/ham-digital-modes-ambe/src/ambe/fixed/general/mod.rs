//! Fixed-point arithmetic primitives shared across every mode's own fixed-point port -- the
//! integer/table-based equivalents of the handful of `f64` operations the floating-point
//! implementations call directly (`sin`/`cos`, `exp2`/`log2`-style operations via `.exp()`/`.log2()`
//! on a base-e or base-2 quantity, and `sqrt`). See [`super`]'s own doc comment for the numeric
//! convention (Q16.16 unless stated otherwise) and the tolerance each primitive is held to.

pub mod concealment;
pub mod explog;
mod explog_table;
pub mod fixed_ops;
pub mod isqrt;
pub mod mbe_encode;
pub mod mbe_speech;
pub mod mbe_synthesis;
pub mod tone_detect;
pub mod tone_synthesis;
pub mod trig;
mod trig_table;
pub mod unvoiced_synthesis;
pub mod voiced_synthesis;
