//! Fixed-point TIA-102.BABA (P25 Phase 1) port -- see [`super`]'s own doc comment for the numeric
//! convention (Q16.16 unless stated) and tolerance policy. Started with `parameter_encoding` (the
//! fundamental-frequency/voicing-decision dequantization), the first stage of
//! [`super::super::float::tia_102_baba::decode`]'s own decode pipeline -- see that module's doc comment
//! for the full decode order this fixed-point port will eventually mirror stage by stage.
//!
//! The bit-level FEC/interleave/wire-format layer (`dvsi_p25fec::fec`, `dvsi_p25fec::frame`,
//! `dvsi_p25fec::wire_format`, `bit_prioritization`, `modulation`, `dvsi_p25fec::dtx`,
//! [`super::super::float::dstar`]'s Golay/Hamming via [`super::super::general::fec`]) is already
//! pure integer/bitwise arithmetic and is reused directly from [`super::super::float::tia_102_baba`]
//! rather than duplicated here -- only the parameter dequantization and synthesis math genuinely
//! needs a fixed-point port.

pub mod decode;
pub mod encode;
pub mod encoder;
pub mod enhancement;
pub mod error_estimation;
pub mod parameter_encoding;
pub mod pitch;
pub mod pitch_refinement;
mod pitch_refinement_tables;
mod pitch_tables;
pub mod prediction;
pub mod quantize;
pub mod reconstruct;
mod reconstruct_tables;
pub mod spectral_amplitude;
pub mod synthesis;
pub mod vuv;
