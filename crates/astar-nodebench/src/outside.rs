// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The outside SSTV decoder: a decoder that shares no code with astar, run in
//! a container with no network on a WAV file.
//!
//! The image is `localhost/nodebench-sstv-decode` (`rig.sh build` makes it;
//! `images/sstv-decode/`). RIG.md ("SSTV: the outside decoder") says which
//! picture modes it reads.

use std::io::{self, Write};
use std::path::Path;
use std::process::Stdio;

use crate::rig::Podman;

/// The image `rig.sh build` makes.
const IMAGE: &str = "localhost/nodebench-sstv-decode";

/// What the outside decoder made of one recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideDecode {
    /// The mode name the decoder reported, for example `Robot 36`.
    pub mode: String,
    pub width: u32,
    pub height: u32,
    /// Row-major RGB, three bytes a pixel.
    pub rgb: Vec<u8>,
}

/// Writes 16-bit mono PCM as a WAV.
pub fn write_wav(path: &Path, samples: &[i16], rate: u32) -> io::Result<()> {
    let data_len = u32::try_from(samples.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too long for a WAV"))?;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1_u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(path)?.write_all(&out)
}

/// Runs the container with no network (`--network none`) on a WAV file.
///
/// The recording is copied into a scratch directory that the container sees
/// as `/work`; the decoder writes its picture there.
pub fn outside_decode(wav: &Path) -> Result<OutsideDecode, String> {
    static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work = std::env::temp_dir().join(format!("nb-outside-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| format!("scratch dir: {e}"))?;
    let result = run_in(&work, wav);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn run_in(work: &Path, wav: &Path) -> Result<OutsideDecode, String> {
    std::fs::copy(wav, work.join("in.wav")).map_err(|e| format!("{}: {e}", wav.display()))?;
    let podman = Podman {
        bin: "podman".into(),
        connection: std::env::var("CONTAINER_CONNECTION")
            .ok()
            .filter(|c| !c.is_empty()),
    };
    let mount = format!("{}:/work", work.display());
    let out = podman
        .command([
            "run",
            "--rm",
            "--network",
            "none",
            "-v",
            mount.as_str(),
            IMAGE,
            "/work/in.wav",
            "/work/out.png",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("podman: {e}"))?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        return Err(format!("outside decoder failed: {}", said.trim()));
    }
    let mode = said
        .lines()
        .find_map(|l| l.split_once("Detected SSTV mode "))
        .map(|(_, m)| m.trim().to_string())
        .ok_or_else(|| format!("no mode reported: {}", said.trim()))?;
    let file = std::fs::File::open(work.join("out.png")).map_err(|e| format!("no picture: {e}"))?;
    let mut reader = png::Decoder::new(io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("picture: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("picture size")?];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("picture: {e}"))?;
    buf.truncate(info.buffer_size());
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("picture is {:?}, not 8-bit", info.bit_depth));
    }
    // One decoder writes RGB, the other RGBA; keep the colour only.
    let rgb = match info.color_type {
        png::ColorType::Rgb => buf,
        png::ColorType::Rgba => buf
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        other => return Err(format!("picture is {other:?}, not colour")),
    };
    Ok(OutsideDecode {
        mode,
        width: info.width,
        height: info.height,
        rgb,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_precision_loss)]

    use super::*;

    /// The test picture, as `astar-sstv`'s tests draw it: seven colour bars
    /// over the top two thirds, a grey ramp across the bottom third.
    fn test_image(width: u32, height: u32) -> Vec<u8> {
        const BARS: [[u8; 3]; 7] = [
            [191, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
        ];
        let (mw, mh) = (320, 256);
        let mut out = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let (mx, my) = (x * mw / width, y * mh / height);
                let px = if my < mh * 2 / 3 {
                    BARS[(mx * 7 / mw) as usize]
                } else {
                    let g = u8::try_from(mx * 255 / (mw - 1)).unwrap();
                    [g, g, g]
                };
                out.extend_from_slice(&px);
            }
        }
        out
    }

    fn encode(mode: sstv::Mode, rate: u32) -> Vec<i16> {
        let (w, h) = mode.resolution();
        let pixels: Vec<sstv::RgbPixel> = test_image(w, h)
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| sstv::RgbPixel::new(p[0], p[1], p[2]))
            .collect();
        let encoder = sstv::Encoder::new(mode, pixels.into_iter()).expect("encoder");
        sstv::Synthesizer::new(encoder, rate).collect()
    }

    /// Mean absolute difference per channel against the test picture.
    fn mean_error(d: &OutsideDecode) -> f64 {
        let want = test_image(d.width, d.height);
        assert_eq!(want.len(), d.rgb.len());
        let total: u64 = want
            .iter()
            .zip(&d.rgb)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        total as f64 / want.len() as f64
    }

    fn run(mode: sstv::Mode, name: &str) -> OutsideDecode {
        let wav = std::env::temp_dir().join(format!("nb-outside-test-{name}.wav"));
        write_wav(&wav, &encode(mode, 8000), 8000).unwrap();
        let d = outside_decode(&wav).expect("outside decode");
        let _ = std::fs::remove_file(&wav);
        d
    }

    #[test]
    #[ignore = "needs podman and the image; set NODEBENCH_OUTSIDE=1"]
    fn outside_decodes_robot36() {
        if std::env::var("NODEBENCH_OUTSIDE").as_deref() != Ok("1") {
            return;
        }
        let d = run(sstv::modes::ROBOT_36, "robot36");
        assert!(d.mode.contains("Robot 36"), "mode was {:?}", d.mode);
        assert_eq!((d.width, d.height), (320, 240));
        let err = mean_error(&d);
        assert!(err < MEASURED_ROBOT36 * 1.5, "mean error {err}");
    }

    /// Mean error the outside decoder measured on Robot 36 at 8 kHz (RIG.md,
    /// "SSTV: the outside decoder"). Most of it is the decoder's own tone to
    /// brightness scaling, not astar's signal.
    const MEASURED_ROBOT36: f64 = 32.8;
}
