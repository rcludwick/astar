// SPDX-License-Identifier: LGPL-3.0-or-later
//! The real DVSI chip's own 144-bit frame layout for the `RATEP` "P25 FEC" rate control words / `RATET(27)` setting
//! (7200 bps: 4400 speech + 2800 FEC): the proprietary wire permutation, the chip's Golay/Hamming block structure
//! (`c0..c3` Golay, `c4..c6` chip-labelled Hamming, `c7` raw), and its DTMF and DTX-silence signalling. Reverse
//! engineered from live captures; see `docs/references/AMBE_CHIP_VALIDATION_FINDINGS.md`.
//!
//! **This is not TIA-102.BABA (P25 Phase 1 IMBE) framing.** The chip's mode at this setting turned out to be a
//! DVSI AMBE-family codec whose channel bytes do not follow the over-the-air P25 interleave and whose pitch/amplitude
//! fields differ from TIA's. [`super::float::tia_102_baba`] is the faithful TIA-102.BABA implementation; this module
//! only describes what the chip emits at this rate.

pub mod dtmf;
pub mod dtx;
pub mod fec;
pub mod frame;
pub mod pitch_map;
pub mod wire_format;
