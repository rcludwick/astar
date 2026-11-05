// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::dstar::decode`'s own `dequantize` -- built on
//! [`crate::ambe::fixed::general::mbe_speech`]'s shared MBE speech-dequantize core (see that
//! module's own doc comment for why D-STAR shares it with AMBE+2 half-rate rather than duplicating).
//! Frame classification, raw bit extraction, and every tone/DTMF function (`classify_b0`,
//! `classify_tone_index`, `decode_tone`, `dtmf_digit_from_tone_index`) are already pure integer
//! arithmetic in the float sibling (`classify_tone_index`'s own `hz: f64` field is the sole
//! exception -- reported here as `hz_q16` instead, an exact conversion since `31.25` is itself
//! exactly representable in Q16.16) and are otherwise reused directly.

use super::tables_q16::{
    DG_Q16_16, HOC_B5_Q16_16, HOC_B6_Q16_16, HOC_B7_Q16_16, HOC_B8_Q16_16, PRBA24_Q16_16,
    PRBA58_Q16_16, W0_TABLE_Q16_16, W0_TABLE_Q32,
};
use crate::ambe::fixed::general::fixed_ops::{mul_q16, TWO_PI_Q16_16};
use crate::ambe::fixed::general::mbe_speech::{
    dequantize_speech, MbeDecoderState, RawSpeechParameters, SpeechParameters, SpeechTables,
};
use crate::ambe::float::dstar::decode::FrameKind;
use crate::ambe::float::dstar::tables::{LMPRBL, L_TABLE, VUV};

// Already pure integer in the float sibling -- reused directly, not duplicated.
pub use crate::ambe::float::dstar::decode::{
    classify_b0, decode_tone, dtmf_digit_from_tone_index, extract_raw_parameters, TonePayload,
};

/// `round(31.25 * 65536)`, computed exactly (`31.25 = 31 + 1/4`, and both `31` and `1/4` are exactly
/// representable in 16 fractional bits) -- see [`ToneKind::Single`]'s own doc comment.
const HZ_PER_INDEX_Q16_16: i32 = 2_048_000;

/// The fixed-point equivalent of `ambe::float::dstar::decode::ToneKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneKind {
    Invalid,
    /// `hz_q16` is exact, not approximated -- `index * HZ_PER_INDEX_Q16_16` has no rounding error
    /// beyond `HZ_PER_INDEX_Q16_16`'s own (exact) representation of `31.25`.
    Single {
        hz_q16: i32,
    },
    Dual,
}

/// The fixed-point equivalent of `ambe::float::dstar::decode::classify_tone_index`.
pub fn classify_tone_index(index: u32) -> ToneKind {
    match index {
        5..=122 => ToneKind::Single {
            hz_q16: (index as i32) * HZ_PER_INDEX_Q16_16,
        },
        128..=163 => ToneKind::Dual,
        _ => ToneKind::Invalid,
    }
}

/// The fixed-point equivalent of `ambe::float::dstar::decode::DequantizedFrame`.
pub enum DequantizedFrame {
    Speech(SpeechParameters),
    Tone(TonePayload),
}

fn speech_tables() -> SpeechTables<'static> {
    SpeechTables {
        rho_q16: crate::ambe::fixed::general::mbe_speech::POINT_80_Q16_16,
        gamma_scale_q16: crate::ambe::fixed::general::mbe_speech::GAMMA_SCALE_2_Q16_16,
        gamma_memory_q16: crate::ambe::fixed::general::mbe_speech::GAMMA_MEMORY_ZERO_Q16_16,
        vuv: &VUV,
        dg_q16: &DG_Q16_16,
        prba24_q16: &PRBA24_Q16_16,
        prba58_q16: &PRBA58_Q16_16,
        lmprbl: &LMPRBL,
        hoc_b5_q16: &HOC_B5_Q16_16,
        hoc_b6_q16: &HOC_B6_Q16_16,
        hoc_b7_q16: &HOC_B7_Q16_16,
        hoc_b8_q16: &HOC_B8_Q16_16,
    }
}

/// The fixed-point equivalent of `ambe::float::dstar::decode::dequantize`.
pub fn dequantize(d: u64, state: &mut MbeDecoderState) -> DequantizedFrame {
    let raw = extract_raw_parameters(d);
    if classify_b0(raw.b0) == FrameKind::Tone {
        return DequantizedFrame::Tone(decode_tone(d));
    }

    let l = L_TABLE[(raw.b0 as usize).min(L_TABLE.len() - 1)];
    // W0_TABLE_Q16_16 here is D-STAR's own f0 (not w0) -- see this module's own doc comment and
    // `ambe::fixed::ambe_plus_2::decode`'s identical naming note.
    let w0_q16 = mul_q16(
        W0_TABLE_Q16_16[(raw.b0 as usize).min(W0_TABLE_Q16_16.len() - 1)],
        TWO_PI_Q16_16,
    );
    let w0_q32 = W0_TABLE_Q32[(raw.b0 as usize).min(W0_TABLE_Q32.len() - 1)];
    let raw_speech = RawSpeechParameters {
        b1: raw.b1,
        b2: raw.b2,
        b3: raw.b3,
        b4: raw.b4,
        b5: raw.b5,
        b6: raw.b6,
        b7: raw.b7,
        b8: raw.b8,
    };
    let tables = speech_tables();
    let vuv_w0_q16 = w0_q16;
    let params = dequantize_speech(l, w0_q16, w0_q32, vuv_w0_q16, &raw_speech, &tables, state);
    DequantizedFrame::Speech(params)
}
