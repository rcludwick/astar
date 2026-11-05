// SPDX-License-Identifier: LGPL-3.0-or-later
//! Mode-independent MBE frame synthesis shared by D-STAR and AMBE+2 half-rate: turns one frame's
//! already-dequantized `(w0, per-harmonic voicing, Ml)` into 20 ms of PCM by reusing
//! [`super::tia_102_baba::synthesis::SynthesisState`] (spectral enhancement, V/UV and amplitude
//! smoothing, then voiced + unvoiced synthesis, Eq. 105-142).
//!
//! mbelib runs one shared `mbe_synthesizeSpeechf` for P25, D-STAR and AMBE+2, and its
//! `mbe_spectralAmpEnhance` is the same Eq. 105-110 enhancement `tia_102_baba::enhancement` implements
//! (0.96*pi constant, harmonics with `8*l <= L` left unweighted, weights clamped to `[0.5, 1.2]`,
//! energy renormalized), so the parameter set is identical across modes and nothing here is
//! mode-specific. Two deliberate differences from mbelib remain: this crate's unvoiced half is the
//! spec's DFT construction rather than mbelib's multisine mix, and `SynthesisState` also applies the
//! spec's V/UV and amplitude smoothing, which mbelib omits.
//!
//! FEC error statistics feed the same smoothing thresholds as TIA-102.BABA, using the two Golay blocks
//! these two modes actually carry (`epsilon_c0`, `epsilon_c1`) and zero for the vectors they lack.

use super::tia_102_baba::error_estimation::{estimate_errors, FrameErrors};
use super::tia_102_baba::synthesis::SynthesisState;
use super::tia_102_baba::unvoiced_synthesis::N;

/// How a decoder treats frames the channel decoder flags as damaged (`epsilon_c0`/`epsilon_c1` are the numbers of bit
/// errors the two Golay blocks corrected).
///
/// [`ErrorPolicy::Concealing`] (the default) is the audio-quality policy of [`crate::ambe::float::concealment`]; on a stream with no channel errors it decodes
/// bit-for-bit like [`ErrorPolicy::Clean`]. [`ErrorPolicy::Clean`] is mbelib's: a speech frame with more than 3 corrected errors in total repeats
/// the previous frame's parameters without touching the predictor, and after 3 repeats in a row the decoder mutes and
/// restarts. [`ErrorPolicy::ChipCompatible`] reproduces what the real chip does, measured with
/// `examples/{dstar,ambe_plus_2}_field_scan.rs ... errclass`: it repeats a frame whenever the first Golay block
/// corrected 3 errors (whatever the second block did), and after 3 repeats in a row it decodes every further damaged
/// frame as received instead of muting, which produces loud bursts and chirps. That is a defect, so it exists only to
/// let conformance tests compare against the chip without the difference hiding others; nothing should select it in
/// normal operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorPolicy {
    /// mbelib's policy (repeat three times, then mute): kept for reference and for tests; not the default.
    Clean,
    ChipCompatible,
    /// Audio-quality concealment ([`crate::ambe::float::concealment`]): graded, continuity-aware repair of the raw bits and a fading
    /// repeat instead of a hard mute. Implemented in the D-STAR and AMBE+2 synthesis decoders.
    #[default]
    Concealing,
}

/// What to do with one damaged frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadFrameAction {
    Repeat,
    Mute,
    Decode,
}

impl ErrorPolicy {
    /// Whether a speech frame with these corrected-error counts is treated as damaged.
    pub fn is_bad(self, epsilon_c0: u32, epsilon_c1: u32) -> bool {
        match self {
            ErrorPolicy::Clean | ErrorPolicy::Concealing => epsilon_c0 + epsilon_c1 > 3,
            ErrorPolicy::ChipCompatible => epsilon_c0 >= 3,
        }
    }

    /// The action for the `consecutive_bad`-th damaged frame in a row (1 for the first).
    pub fn bad_frame_action(self, consecutive_bad: u32) -> BadFrameAction {
        match (self, consecutive_bad <= 3) {
            (_, true) => BadFrameAction::Repeat,
            (ErrorPolicy::Clean | ErrorPolicy::Concealing, false) => BadFrameAction::Mute,
            (ErrorPolicy::ChipCompatible, false) => BadFrameAction::Decode,
        }
    }
}

/// Weight of the second difference in the output's high-frequency lift `y[n] = x[n] + g*(x[n] - 2x[n-1] + x[n-2])`.
/// The chip's D-STAR and AMBE+2 output is flat against this synthesis below about 2.4 kHz and then rises to +3.2 dB at
/// 3.6 kHz (measured per harmonic, at three different pitches, with `examples/dstar_field_scan.rs`; a real-speech
/// long-term spectrum agrees), which this two-tap-zero filter reproduces within about 1 dB.
pub const HIGH_LIFT_WEIGHT: f64 = 0.10;

/// Gain on the unvoiced (noise) half of the synthesis. With the lift above, this minimizes the difference in long-term band
/// level (100-1000, 1000-2000, 2000-3800 Hz) between this decoder and the chip over 600+ frames of each of four real speakers
/// (`AMBE_CODEC_STATUS.md`): the standard's synthesis was 1.1 to 1.7 dB louder than the chip on noise-like frames.
pub const UNVOICED_GAIN: f64 = 0.87;

pub struct MbeSynthesizer {
    synthesis: SynthesisState,
    error_rate_prev: f64,
    lift_history: [f64; 2],
}

impl MbeSynthesizer {
    fn lift(&mut self, frame: [f64; N]) -> [f64; N] {
        let mut out = frame;
        for (o, &x) in out.iter_mut().zip(frame.iter()) {
            *o = x + HIGH_LIFT_WEIGHT * (x - 2.0 * self.lift_history[0] + self.lift_history[1]);
            self.lift_history = [x, self.lift_history[0]];
        }
        out
    }
}

impl MbeSynthesizer {
    pub fn new() -> Self {
        Self {
            synthesis: {
                let mut s = SynthesisState::new();
                s.set_unvoiced_gain(UNVOICED_GAIN);
                s
            },
            error_rate_prev: 0.0,
            lift_history: [0.0; 2],
        }
    }

    fn errors_for(&mut self, epsilon_c0: u32, epsilon_c1: u32) -> FrameErrors {
        let errors = estimate_errors(
            &[epsilon_c0, epsilon_c1, 0, 0, 0, 0, 0],
            self.error_rate_prev,
        );
        self.error_rate_prev = errors.rate;
        errors
    }

    /// Synthesizes one speech frame. `voiced` and `ml` are both 1-indexed by harmonic (index 0 is
    /// unused padding), exactly as `dstar::decode::DStarParameters` and
    /// `ambe_plus_2::decode::Parameters` carry them. Returns `None` on a length mismatch.
    pub fn synthesize_speech(
        &mut self,
        w0: f64,
        voiced: &[bool],
        ml: &[f64],
        epsilon_c0: u32,
        epsilon_c1: u32,
    ) -> Option<[f64; N]> {
        if voiced.len() != ml.len() || voiced.len() < 2 {
            return None;
        }
        let errors = self.errors_for(epsilon_c0, epsilon_c1);
        let frame = self
            .synthesis
            .synthesize_frame(&ml[1..], w0, &voiced[1..], &errors)?;
        Some(self.lift(frame))
    }

    /// Repeats the previous frame's parameters (an erasure); `None` before any real frame has run.
    pub fn synthesize_repeat(&mut self) -> Option<[f64; N]> {
        let frame = self.synthesis.synthesize_repeated_frame()?;
        Some(self.lift(frame))
    }

    /// Repeats the previous frame with its amplitudes scaled by `scale` (fading a run of damaged frames).
    pub fn synthesize_repeat_scaled(&mut self, scale: f64) -> Option<[f64; N]> {
        let frame = self.synthesis.synthesize_repeated_frame_scaled(scale)?;
        Some(self.lift(frame))
    }

    /// A silence frame: all zeros, like mbelib's `mbe_synthesizeSilencef`.
    pub fn synthesize_silence(&self) -> [f64; N] {
        [0.0; N]
    }
}

impl Default for MbeSynthesizer {
    fn default() -> Self {
        Self::new()
    }
}
