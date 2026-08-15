// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! TTS synthesis (iax-be03): Piper subprocess → PCM at the station rate,
//! with a test seam.

use std::path::PathBuf;
use std::time::Duration;

/// Configuration for the TTS subsystem.
#[derive(Clone, Debug)]
pub struct TtsConfig {
    pub enabled: bool,
    pub binary: String,
    pub voice: Option<PathBuf>,
    pub timeout: Duration,
    /// Output gain applied to synthesized PCM, in decibels. `0.0` is unity
    /// (no change); negative values attenuate (use this when a voice renders
    /// too hot), positive values boost. Saturates at i16 bounds.
    pub gain_db: f32,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            binary: "piper".into(),
            voice: None,
            timeout: Duration::from_secs(4),
            gain_db: 0.0,
        }
    }
}

/// Scale PCM samples in place by `gain_db` decibels, saturating at i16 bounds.
///
/// `0.0` dB is a no-op. Conversion: `factor = 10^(gain_db / 20)`.
fn apply_gain_db(pcm: &mut [i16], gain_db: f32) {
    if gain_db == 0.0 {
        return;
    }
    let factor = 10f32.powf(gain_db / 20.0);
    for s in pcm.iter_mut() {
        let scaled = f32::from(*s) * factor;
        #[allow(
            clippy::cast_possible_truncation,
            // clamped to the i16 range immediately below, so the cast is exact
        )]
        let clamped = scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        *s = clamped;
    }
}

/// Errors from TTS synthesis.
#[derive(Debug)]
#[non_exhaustive]
pub enum TtsError {
    Disabled,
    Spawn(String),
    Exit(String),
    Decode(String),
    Timeout,
}

/// Text → mono PCM at the requested sample rate. Implemented by
/// `PiperEngine`; faked in tests.
pub trait TtsEngine: Send {
    fn synth(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, TtsError>;
}

/// Shells out to the Piper binary on the calling (blocking) thread.
pub struct PiperEngine {
    cfg: TtsConfig,
}

impl PiperEngine {
    #[must_use]
    pub fn new(cfg: TtsConfig) -> Self {
        Self { cfg }
    }
}

impl TtsEngine for PiperEngine {
    fn synth(&self, text: &str, sample_rate: u32) -> Result<Vec<i16>, TtsError> {
        if !self.cfg.enabled {
            return Err(TtsError::Disabled);
        }
        // iax-e6f1: the gain is applied INSIDE the decode/resample (float
        // domain, pre-clamp) — applying it here, after the resampler's
        // full-scale clamp, let a hot voice clip first and merely lowered the
        // already-flattened waveform.
        synth_via_piper(&self.cfg, text, sample_rate)
    }
}

/// Build a canonical 16-bit mono WAV in memory (test helper + fixtures).
///
/// Layout: RIFF header (44 bytes) + raw little-endian 16-bit PCM samples.
#[cfg(test)]
pub(crate) fn wav_bytes(rate: u32, pcm: &[i16]) -> Vec<u8> {
    #[allow(
        clippy::cast_possible_truncation,
        // pcm.len()*2 fits u32 for any realistic test buffer
    )]
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for &s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Decode a mono 16-bit WAV at any sample rate, apply `gain_db`, and resample
/// to `target_rate`.
///
/// The gain applies in the FLOAT domain, BEFORE the full-scale clamp on the
/// resample path (iax-e6f1): a hot voice attenuated here never clips, whereas
/// post-clamp gain merely lowered an already-flattened waveform. `0.0` dB is
/// unity. If the source rate already matches `target_rate` the samples get
/// the same (saturating) gain and no resample.
pub(crate) fn wav_to_pcm(
    bytes: &[u8],
    target_rate: u32,
    gain_db: f32,
) -> Result<Vec<i16>, TtsError> {
    let (rate, mut pcm) = decode_wav_mono(bytes).map_err(TtsError::Decode)?;
    if rate == target_rate {
        apply_gain_db(&mut pcm, gain_db);
        return Ok(pcm);
    }
    let factor = if gain_db == 0.0 {
        1.0
    } else {
        10f32.powf(gain_db / 20.0)
    };
    // i16 → f32 for the resampler, with the gain folded in pre-clamp.
    let f: Vec<f32> = pcm
        .iter()
        .map(|&s| f32::from(s) / 32768.0 * factor)
        .collect();
    // Anti-aliased offline resample (iax-e6f1): piper renders at 22.05 kHz
    // and the station runs 8/16 kHz — the realtime `Resampler1` (filterless
    // linear interpolation) folded everything above the target Nyquist back
    // into the voice band, which is what made the greeting sound overdriven.
    let out = astar_audio::resample_offline(&f, rate, target_rate)
        .map_err(|e| TtsError::Decode(e.to_string()))?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        // clamp to [-1,1] then scale to [-32767,32767]: fits i16
    )]
    Ok(out
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect())
}

/// Parse a mono 16-bit WAV from raw bytes, returning `(sample_rate, samples)`.
///
/// Tolerates non-standard chunk ordering (walks chunks instead of assuming
/// fixed offsets). Returns an `Err` string on malformed/truncated input —
/// does not panic.
fn decode_wav_mono(bytes: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }

    let mut sample_rate: Option<u32> = None;
    let mut pcm: Option<Vec<i16>> = None;

    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        #[allow(clippy::cast_possible_truncation)] // chunk size: intentional file-offset cast
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;

        if id == b"fmt " {
            // Minimum PCM fmt chunk is 16 bytes.
            if sz < 16 || i + 8 + sz > bytes.len() {
                return Err("truncated fmt chunk".into());
            }
            let fmt_base = i + 8;
            let audio_format = u16::from_le_bytes([bytes[fmt_base], bytes[fmt_base + 1]]);
            if audio_format != 1 {
                return Err(format!(
                    "unsupported audio format {audio_format} (expected PCM=1)"
                ));
            }
            let channels = u16::from_le_bytes([bytes[fmt_base + 2], bytes[fmt_base + 3]]);
            if channels != 1 {
                return Err(format!("expected mono (1 channel), got {channels}"));
            }
            let rate = u32::from_le_bytes([
                bytes[fmt_base + 4],
                bytes[fmt_base + 5],
                bytes[fmt_base + 6],
                bytes[fmt_base + 7],
            ]);
            if rate == 0 {
                return Err("zero sample rate in fmt chunk".into());
            }
            let bits = u16::from_le_bytes([bytes[fmt_base + 14], bytes[fmt_base + 15]]);
            if bits != 16 {
                return Err(format!("expected 16-bit samples, got {bits}"));
            }
            sample_rate = Some(rate);
        } else if id == b"data" {
            let start = i + 8;
            let end = (start + sz).min(bytes.len());
            let samples: Vec<i16> = bytes[start..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            pcm = Some(samples);
        }

        // Advance past this chunk (odd-size chunks are padded to even boundary).
        i += 8 + sz + (sz & 1);
    }

    let rate = sample_rate.ok_or("no fmt chunk found")?;
    let samples = pcm.ok_or("no data chunk found")?;
    Ok((rate, samples))
}

/// Shell out to the Piper TTS binary and decode its WAV stdout to PCM at
/// `sample_rate`.
///
/// Piper is invoked as:
///   `<binary> --output_file - [--model <voice>]`
/// with `text` written to stdin. WAV output is read from stdout and
/// resampled to `sample_rate` via `wav_to_pcm`.
///
/// # Timeout
/// `cfg.timeout` is enforced: if piper does not complete within the deadline
/// the child process is killed and `TtsError::Timeout` is returned.
///
/// Implementation: stdin is written and closed on the calling thread; the
/// child's pid is captured; the child is moved into a bounded helper thread
/// that calls `wait_with_output()` and sends the result over a channel. The
/// calling thread does `recv_timeout(cfg.timeout)`: on success the WAV bytes
/// are decoded; on timeout the child is killed via its pid and
/// `TtsError::Timeout` is returned. The helper thread will reap the killed
/// process shortly after (it exits `wait_with_output` as soon as the child
/// terminates).
///
/// This avoids the stdout-pipe-buffer deadlock that occurs when reading stdout
/// while the child is still running.
///
/// # Residual
/// This function still runs under the `Station` session mutex (phrase
/// resolution happens inside `Manager::announce`). The lock is held for up to
/// `cfg.timeout`. A follow-up could resolve phrases off-lock to remove this
/// bound entirely.
fn synth_via_piper(cfg: &TtsConfig, text: &str, sample_rate: u32) -> Result<Vec<i16>, TtsError> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    let mut cmd = Command::new(&cfg.binary);
    cmd.arg("--output_file").arg("-"); // WAV to stdout
    if let Some(v) = &cfg.voice {
        cmd.arg("--model").arg(v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| TtsError::Spawn(e.to_string()))?;

    // Write + close stdin so piper sees EOF and starts producing WAV output.
    // Announcement text is short; a separate writer thread is not needed here.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| TtsError::Spawn("failed to open piper stdin".into()))?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|e| TtsError::Spawn(e.to_string()))?;
    drop(stdin); // close stdin so piper sees EOF

    // Capture the pid before moving the child into the helper thread so we
    // can kill the process on timeout without needing to share the `Child`.
    let child_pid = child.id();

    // Spawn a short-lived helper thread that does the blocking wait and sends
    // the result over a channel. The thread is bounded — it exits as soon as
    // piper exits (or is killed by the timeout path below).
    let (tx, rx) = mpsc::channel::<Result<std::process::Output, String>>();
    std::thread::spawn(move || {
        let result = child.wait_with_output().map_err(|e| e.to_string());
        // Ignore send errors: the calling thread may have timed out and
        // returned already; that's fine — the thread just exits.
        let _ = tx.send(result);
    });

    // Wait for the helper thread's result, bounded by cfg.timeout.
    let out = match rx.recv_timeout(cfg.timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(TtsError::Exit(e)),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Kill the piper child process so the helper thread unblocks and
            // the subprocess doesn't linger. Best-effort: ignore errors (the
            // process may have exited between the timeout and this call).
            kill_child(child_pid);
            return Err(TtsError::Timeout);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Helper thread panicked or dropped the sender without sending.
            return Err(TtsError::Exit("piper helper thread disconnected".into()));
        }
    };

    if !out.status.success() {
        return Err(TtsError::Exit(format!(
            "piper exit {:?}",
            out.status.code()
        )));
    }

    wav_to_pcm(&out.stdout, sample_rate, cfg.gain_db)
}

/// Send SIGKILL to a child process by pid. Best-effort; errors are ignored.
///
/// # Platform notes
/// On Unix, this issues `kill(2)` with `SIGKILL` (signal 9). On non-Unix
/// targets this is a no-op — piper is a Unix-only binary in practice.
#[allow(unused_variables)]
fn kill_child(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill(2)` with SIGKILL on a known-valid pid. The pid is
        // still valid because the helper thread hasn't seen an exit yet (we
        // just timed out on `recv_timeout`). On pid-reuse races this becomes
        // a benign ESRCH; we ignore the return value intentionally.
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        #[allow(clippy::cast_possible_wrap)] // child pid fits i32 on all supported targets
        unsafe {
            kill(pid as i32, 9 /* SIGKILL */);
        }
    }
}

/// Default fallback table is empty (stage 4 supplies real mappings).
#[must_use]
pub fn fallback_slug(_text: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_db_zero_is_a_noop() {
        let mut pcm = vec![100i16, -200, 300];
        apply_gain_db(&mut pcm, 0.0);
        assert_eq!(pcm, vec![100, -200, 300]);
    }

    #[test]
    fn negative_gain_attenuates() {
        // -6.02 dB ≈ half amplitude.
        let mut pcm = vec![10_000i16, -10_000];
        apply_gain_db(&mut pcm, -6.0205);
        assert_eq!(pcm[0], 5_000);
        assert_eq!(pcm[1], -5_000);
    }

    #[test]
    fn positive_gain_saturates_at_i16_bounds() {
        let mut pcm = vec![30_000i16, -30_000];
        apply_gain_db(&mut pcm, 12.0); // ×~3.98 would overflow
        assert_eq!(pcm[0], i16::MAX);
        assert_eq!(pcm[1], i16::MIN);
    }

    #[test]
    fn downsampling_filters_out_of_band_content_instead_of_aliasing() {
        // A 6 kHz tone at 22.05 kHz is ABOVE the 4 kHz Nyquist of the 8 kHz
        // target. A proper resampler filters it to near-silence; the old
        // filterless linear interpolation folded it back into the band at
        // nearly full amplitude — the "overdriven" greeting (iax-e6f1).
        #[allow(clippy::cast_possible_truncation)]
        let pcm22: Vec<i16> = (0..22_050u32)
            .map(|n| {
                (f64::from(n) * 2.0 * std::f64::consts::PI * 6_000.0 / 22_050.0).sin() * 20_000.0
            })
            .map(|s| s as i16)
            .collect();
        let wav = wav_bytes(22_050, &pcm22);
        let pcm8 = wav_to_pcm(&wav, 8_000, 0.0).expect("decode+resample");
        #[allow(clippy::cast_precision_loss)]
        let rms = (pcm8
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / pcm8.len() as f64)
            .sqrt();
        // Input RMS ≈ 20000/√2 ≈ 14142; the sinc stopband leaves well under
        // 1%. The aliasing path kept most of the energy (rms ≈ 13000+).
        assert!(
            rms < 300.0,
            "6 kHz must be filtered, not folded into the 8 kHz band (rms={rms:.0})"
        );
    }

    #[test]
    fn gain_db_attenuates_across_the_resample_path() {
        // -6.02 dB folded into the float domain: resampled output peaks at
        // ~half the unity-gain peak (in-band 1 kHz tone, no aliasing).
        #[allow(clippy::cast_possible_truncation)]
        let pcm22: Vec<i16> = (0..22_050u32)
            .map(|n| {
                (f64::from(n) * 2.0 * std::f64::consts::PI * 1_000.0 / 22_050.0).sin() * 20_000.0
            })
            .map(|s| s as i16)
            .collect();
        let wav = wav_bytes(22_050, &pcm22);
        let peak = |v: &[i16]| v.iter().map(|s| i32::from(*s).abs()).max().unwrap();
        let unity = peak(&wav_to_pcm(&wav, 8_000, 0.0).expect("unity"));
        let gained = peak(&wav_to_pcm(&wav, 8_000, -6.0205).expect("gained"));
        let half = unity / 2;
        assert!(
            (gained - half).abs() < half / 10,
            "-6 dB ≈ half amplitude (unity={unity}, gained={gained})"
        );
    }

    #[test]
    fn disabled_engine_reports_disabled() {
        let eng = PiperEngine::new(TtsConfig::default()); // disabled by default
        assert!(matches!(eng.synth("hello", 8_000), Err(TtsError::Disabled)));
    }

    // A fake engine proves the resolver wiring without spawning a process.
    struct FakeTts(Vec<i16>);
    impl TtsEngine for FakeTts {
        fn synth(&self, _text: &str, _sample_rate: u32) -> Result<Vec<i16>, TtsError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn fake_engine_yields_pcm() {
        let eng = FakeTts(vec![1, 2, 3]);
        assert_eq!(eng.synth("x", 8_000).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn decodes_and_resamples_a_22k_wav_to_8k() {
        // Build a tiny 22.05k mono WAV in memory: 2205 samples (100 ms) of a tone.
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let pcm22: Vec<i16> = (0..2205_u32)
            .map(|n| ((n as f32 * 0.2).sin() * 10000.0) as i16)
            .collect();
        let wav = wav_bytes(22_050, &pcm22);
        let pcm8 = wav_to_pcm(&wav, 8_000, 0.0).expect("decode+resample");
        // 100 ms at 8 kHz ≈ 800 samples (±resampler tail).
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            // test-only: pcm8.len() is always small (≈800); cast is safe
        )]
        let len_i32 = pcm8.len() as i32;
        assert!(
            (len_i32 - 800).abs() < 64,
            "got {} samples (expected ~800)",
            pcm8.len()
        );
    }

    #[test]
    fn wav_roundtrip_at_8k_no_resample() {
        #[allow(
            clippy::cast_possible_truncation,
            // test-only: range 0..800, fits i16 (max 32767); cast is safe
        )]
        let pcm: Vec<i16> = (0..800_i32).map(|i| i as i16 * 40).collect();
        let wav = wav_bytes(8_000, &pcm);
        let decoded = wav_to_pcm(&wav, 8_000, 0.0).expect("decode");
        assert_eq!(decoded.len(), pcm.len());
        assert_eq!(decoded, pcm);
    }

    #[test]
    fn wav_to_pcm_targets_the_requested_rate() {
        // 16 kHz WAV → 16 kHz target: passthrough length.
        let wav = wav_bytes(16_000, &vec![1000i16; 1600]);
        let pcm = wav_to_pcm(&wav, 16_000, 0.0).unwrap();
        assert_eq!(pcm.len(), 1600);
    }

    #[test]
    fn decode_wav_rejects_malformed_input() {
        assert!(wav_to_pcm(b"not a wav", 8_000, 0.0).is_err());
        assert!(wav_to_pcm(&[], 8_000, 0.0).is_err());
        // Valid header, truncated body.
        let mut truncated = wav_bytes(8_000, &[1i16, 2, 3]);
        truncated.truncate(20);
        assert!(wav_to_pcm(&truncated, 8_000, 0.0).is_err());
    }
}
