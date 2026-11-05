//! Fixed-point D-STAR port -- see [`super`]'s own doc comment for the numeric convention. The
//! bit-level FEC/whitening/interleave layer is already pure integer/bitwise arithmetic (reused from
//! [`super::super::float::dstar`] directly) -- only [`decode`]'s own parameter dequantization
//! genuinely needs a fixed-point port, built on [`super::super::general::mbe_speech`]'s shared core
//! (the same one [`super::ambe_plus_2::decode`] uses -- D-STAR and AMBE+2 half-rate's own float
//! `dequantize` bodies are almost line-for-line identical, mbelib's shared heritage).

pub mod decode;
pub mod encode;
pub mod encoder;
pub mod synthesis;
mod tables_q16;
