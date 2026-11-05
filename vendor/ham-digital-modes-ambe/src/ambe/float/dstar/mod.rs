//! D-STAR's own AMBE variant: a 72-bit, 9-byte frame every 20ms (3600 total / 2400 speech / 1200
//! FEC bps), an older and structurally different generation from the P25 half/full-rate codec in
//! `super::tia_102_baba` (built from the published TIA-102.BABA text). DVSI has never published a spec for
//! this D-STAR variant; its real frame structure and quantizer tables here are reverse-derived from
//! mbelib (<https://github.com/szechyjs/mbelib>, ISC-licensed, a real working open-source decoder),
//! confirmed against two independent primary sources for the on-chip configuration: DVSI's own
//! USB-3000 Manual (Table 30, "Custom Rate Control Words," explicitly labeled "interoperable with
//! D-STAR") and G4KLX's AMBETools source -- both give the identical RATEP rate-control-word
//! `0x0130 0x0763 0x4000 0x0000 0x0000 0x0048`. See
//! `hams_open/daemons/ham_digital_modes/docs/references/AMBE_CHIP_VALIDATION_FINDINGS.md` for the
//! full research trail.
//!
//! # Wire format vs. logical frame
//!
//! **The real 9-byte CHAND frame exchanged with the chip is not simply `C0||C1||C2||C3`
//! concatenated MSB-first.** An earlier version of this module assumed it was; live validation
//! against the real chip (see `AMBE_CHIP_VALIDATION_FINDINGS.md`) found that assumption wrong in two
//! independent ways, both now fixed:
//!
//! 1. **Wire-format interleave.** The 72 bits are interleaved across the four blocks in exactly the
//!    same pattern D-STAR uses over the air (confirmed against `szechyjs/dsd`'s real `dW`/`dX`
//!    tables) -- apparently so a repeater can relay CHAND bits to/from RF with no separate interleave
//!    step of its own. Each byte's bits are read LSB-first. See [`interleave`] for the real tables
//!    and the [`interleave::wire_bytes_to_frame`]/[`interleave::frame_to_wire_bytes`] conversion
//!    functions -- use these at the actual chip/UDP boundary.
//! 2. **`C0`'s spare bit position.** It's `C0`'s own LSB, not its MSB (confirmed against mbelib's
//!    real `mbe_eccAmbe3600x2400C0`: `ambe_fr[0][0]` is the spare, `ambe_fr[0][23..1]` is the
//!    codeword, MSB-first).
//!
//! [`decode::parse_frame`]/[`encode::build_frame`] operate on this module's own **logical frame**
//! format -- the already-deinterleaved, correctly-oriented `C0||C1||C2||C3` concatenation described
//! below -- not on raw wire bytes directly. Convert with [`interleave`] first.
//!
//! # Frame structure (logical frame, post-deinterleave)
//!
//! Four sub-blocks, each an MSB-first bitfield, concatenated `C0 || C1 || C2 || C3` for exactly
//! `24 + 23 + 11 + 14 = 72` bits:
//!
//! - **`C0`** (24 bits): a 23-bit `[23,12]` Golay codeword (MSB-first) followed by 1 spare bit as its
//!   own LSB (never checked by any known decoder, including mbelib's own).
//! - **`C1`** (23 bits): a second `[23,12]` Golay codeword, but -- unlike `C0` -- **whitened**: XORed
//!   with a pseudo-random sequence seeded from `C0`'s own already-Golay-corrected 12 data bits (see
//!   [`whiten_c1`]) both before encoding and after decoding. `C2`/`C3` are never whitened.
//! - **`C2`** (11 bits) and **`C3`** (14 bits): carried completely unprotected (no FEC at all).
//!
//! Golay-decoding `C0` and `C1` (in that order -- `C1`'s whitening seed depends on `C0`'s own
//! corrected data) yields `12 + 12 = 24` protected data bits; concatenated with `C2`'s 11 and `C3`'s
//! 14 raw bits, that's **49 total decoded parameter bits** (72 - 49 = 23 bits of real FEC overhead).
//! This crate's own [`super::general::fec::golay_encode`]/[`golay_decode`](super::general::fec::golay_decode)
//! are reused directly here (not reimplemented): both use the identical "12 data bits then 11 parity
//! bits, MSB-first" systematic convention mbelib's own Golay implementation does, confirmed by
//! direct comparison before relying on it.
//!
//! # The 49 decoded bits -> nine parameters (`b0..b8`)
//!
//! Numbering the 49 decoded bits `d[0..49)` in the concatenation order above (`d[0..12)` = `C0`'s
//! data, `d[12..24)` = `C1`'s data, `d[24..35)` = `C2`, `d[35..49)` = `C3`), mbelib's own real decode
//! logic (traced directly from its source, not guessed) extracts:
//!
//! | Parameter | Bits | Meaning | Source bits (`d[]` indices) |
//! |---|---|---|---|
//! | `b0` | 7 | Pitch/harmonic-count index -- [`tables::L_TABLE`] | `d[0..6)` (bits 6..1) + `d[48]` (bit 0) |
//! | `b1` | 4 | Voiced/unvoiced pattern -- [`tables::VUV`] | `d[38..42)` |
//! | `b2` | 6 | Gain delta `Δγ` -- [`tables::DG`] | `d[6..10)` + `d[42..44)` |
//! | `b3` | 9 | PRBA gains for harmonics 2-4 -- [`tables::PRBA24`] | `d[10..12)` + `d[12..17)` + `d[44..46)` |
//! | `b4` | 7 | PRBA gains for harmonics 5-8 -- [`tables::PRBA58`] | `d[17..22)` + `d[46..48)` |
//! | `b5` | 4 | Higher-order coefficients, block 1 -- [`tables::HOC_B5`] | `d[22..24)` + `d[25..27)` |
//! | `b6` | 4 | Higher-order coefficients, block 2 -- [`tables::HOC_B6`] | `d[27..31)` |
//! | `b7` | 4 | Higher-order coefficients, block 3 -- [`tables::HOC_B7`] | `d[31..35)` |
//! | `b8` | 4 | Higher-order coefficients, block 4 -- [`tables::HOC_B8`] | `d[35..38)` + a forced-0 LSB |
//!
//! `d[24]` (`C2`'s own first bit) is a real, transmitted bit that no known decoder (including
//! mbelib's own) ever reads -- genuinely unused, not a transcription gap.
//!
//! # Tone frames: a different `b1`/`b2` scatter entirely
//!
//! The table above is only for ordinary speech frames. When `b0 & 0x7E == 0x7E` (`b0` is exactly
//! 126 or 127 -- [`decode::classify_b0`]), the frame is a tone frame, and mbelib's real decoder
//! reads a completely different `index`/`volume` pair from different `d[]` bits (three of `index`'s
//! bits even go through per-value lookup tables keyed on `d[6..9)`, not a plain field) -- see
//! [`decode::decode_tone`]. Confirmed directly against a real chip capture (D-STAR RATEP,
//! `ECMODE_IN`'s `TD_ENABLE` bit on): all 16 DTMF digits and a plain test tone reliably produced
//! `b0 in {126,127}`, and the dual-tone `index` recovered `128 + row + 4*col` exactly for every
//! digit -- see [`decode::dtmf_digit_from_tone_index`] and `AMBE_CHIP_VALIDATION_FINDINGS.md`'s
//! cross-mode DTX/DTMF section for the full chip trace.
//!
//! ## Where this decoder deliberately differs from mbelib (measured on the real chip)
//!
//! mbelib's D-STAR parameter formulas are guesses; the real chip's decoder was mapped with per-field oracle scans
//! (`examples/dstar_field_scan.rs`), and this module follows the chip:
//!
//! | Quantity | mbelib | chip / this module |
//! |---|---|---|
//! | pitch | `2^(-4.3118 - 0.021336*(b0+0.5))` (46.9 steps per octave) | `2^(-4.258618 - 0.021766*b0)` (45.94 steps per octave, 0.2% fit error) |
//! | harmonic count `L` | `AmbePlusLtable` | `floor(0.9254 * 4000 / f0)`, one fewer for most indices ([`tables::L_TABLE`]) |
//! | amplitude predictor weight | `0.65` | `0.8` ([`decode::PREDICTOR_RHO`]) |
//! | gain | `DG + 0.5*gamma_prev` | `2*DG`, no memory ([`decode::GAMMA_SCALE`], [`decode::GAMMA_MEMORY`]) |
//!
//! With these, envelope correlation against the chip's decoded speech is 0.995-0.998 (it was 0.84-0.94 with mbelib's
//! values). The output also carries a small high-frequency lift ([`crate::ambe::float::mbe_synthesis::HIGH_LIFT_WEIGHT`]).
//! How damaged frames are handled is selectable ([`crate::ambe::float::mbe_synthesis::ErrorPolicy`]); the chip's own
//! behaviour there is a defect and is opt-in.

pub mod decode;
pub mod encode;
pub mod encoder;
pub mod interleave;
pub mod quantize;
pub mod synthesis;
pub mod tables;
pub mod whitening;

/// Total frame size: `24 + 23 + 11 + 14`, per this module's own doc comment.
pub const FRAME_BITS: usize = 72;
/// Real decoded parameter bits after Golay correction: `12 + 12 + 11 + 14`.
pub const DECODED_BITS: usize = 49;

pub use whitening::whiten_c1;
