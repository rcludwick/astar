//! Each mode's scan timing, in seconds, for code that has to find a line's
//! sync pulse in received audio and map radio lines to picture rows.

use crate::modes::{Mode, step::Step};

/// Where a line's sync pulse sits within the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPosition {
    /// The line starts with its sync pulse.
    Start,
    /// The sync pulse sits between the second and third scans (the Scottie
    /// modes), so a line starts two scans before its pulse.
    Scottie,
}

/// One mode's scan, in seconds. Enough for a de-jitter to find each line's
/// sync pulse and map radio lines to picture rows.
///
/// A "line" here is what one sync pulse starts: the span between two
/// consecutive sync pulses. Robot 36 sends a row per line; the PD modes send
/// two.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanTiming {
    /// The spacing of the sync pulses: the duration of one transmitted line.
    pub line_seconds: f64,
    /// The sync pulse's duration.
    pub sync_seconds: f64,
    /// The separator pulse between two scans of a line (slowrx calls this
    /// `septr_seconds`); zero for modes with none, such as the PD modes.
    pub porch_seconds: f64,
    /// The duration of one pixel within the first scan of a line.
    pub pixel_seconds: f64,
    /// The image width in pixels.
    pub line_pixels: u32,
    /// The image height in rows.
    pub image_lines: u32,
    /// Where the sync pulse sits within a line.
    pub sync_position: SyncPosition,
    /// Picture rows carried by one line: 1 for most modes, 2 for the PD
    /// modes, whose line sends a pair of rows.
    pub rows_per_line: u32,
    /// The scans one line carries (Scottie 2 has three: green, blue, red).
    pub channels: u32,
}

impl ScanTiming {
    /// Seconds from the start of a line to the start of its sync pulse.
    #[must_use]
    pub fn sync_offset(&self) -> f64 {
        match self.sync_position {
            SyncPosition::Start => 0.0,
            SyncPosition::Scottie => {
                2.0 * (self.porch_seconds + f64::from(self.line_pixels) * self.pixel_seconds)
            }
        }
    }
}

fn seconds(duration: crate::Duration) -> f64 {
    duration.ns() as f64 / 1e9
}

impl Mode {
    /// The mode's scan timing, in seconds.
    ///
    /// ```rust
    /// use sstv::modes::ROBOT_36;
    ///
    /// let timing = ROBOT_36.scan_timing();
    /// assert_eq!((timing.line_pixels, timing.image_lines), (320, 240));
    /// ```
    #[must_use]
    pub fn scan_timing(&self) -> ScanTiming {
        let syncs = self.sync_count().max(1);
        let (sync_offset, sync_duration) = self.sync_pulse();
        let first_scan = self.sequence.iter().find_map(|step| match step {
            Step::Scan(_, duration) => Some(*duration),
            Step::Control(_) => None,
        });
        // The first control tone that follows a scan: the separator between
        // two scans.
        let separator = self
            .sequence
            .windows(2)
            .find_map(|pair| match pair {
                [Step::Scan(..), Step::Control(tone)] => Some(tone.duration),
                _ => None,
            })
            .map_or(0.0, seconds);
        let scans = self.sequence.iter().filter(|step| step.is_scan()).count();
        // Every mode's sequence splits evenly over its sync pulses.
        debug_assert_eq!(self.lines_per_sequence % syncs, 0);
        debug_assert_eq!(scans % syncs, 0);

        ScanTiming {
            line_seconds: seconds(self.sync_spacing()),
            sync_seconds: seconds(sync_duration),
            porch_seconds: separator,
            pixel_seconds: first_scan.map_or(0.0, seconds) / self.resolution.0 as f64,
            line_pixels: self.resolution.0 as u32,
            image_lines: self.resolution.1 as u32,
            sync_position: if sync_offset.ns() == 0 {
                SyncPosition::Start
            } else {
                SyncPosition::Scottie
            },
            rows_per_line: (self.lines_per_sequence / syncs) as u32,
            channels: (scans / syncs) as u32,
        }
    }

    /// The whole transmission's duration, the VIS header included.
    #[must_use]
    pub fn duration(&self) -> core::time::Duration {
        let header: crate::Duration = self.header_tones().map(|tone| tone.duration).sum();
        let passes = (self.resolution.1 / self.lines_per_sequence) as u32;
        let image = self.sequence_duration() * passes;
        core::time::Duration::from_nanos((header + image).ns())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::modes::{PD_120, ROBOT_36};

    #[test]
    fn robot36_timing() {
        let timing = ROBOT_36.scan_timing();
        assert_eq!(timing.image_lines, 240);
        assert_eq!(timing.line_pixels, 320);
        assert_eq!(timing.rows_per_line, 1);
        assert_eq!(PD_120.scan_timing().rows_per_line, 2);
    }

    /// The tests that read the encoder's output.
    #[cfg(feature = "encode")]
    mod encoded {
        use std::vec::Vec;

        use super::*;
        use crate::modes::SCOTTIE_2;
        use crate::{Encoder, Mode, RgbPixel, Synthesizer};

        const SAMPLE_RATE: u32 = 8_000;

        fn black_transmission(mode: Mode) -> Vec<i16> {
            let (width, height) = mode.resolution();
            let image = std::vec![RgbPixel::new(0, 0, 0); (width * height) as usize];
            let encoder = Encoder::new(mode, image.into_iter()).unwrap();
            Synthesizer::new(encoder, SAMPLE_RATE).collect()
        }

        /// The frequency of `samples`, from the spacing of its rising zero
        /// crossings.
        fn frequency_of(samples: &[i16]) -> f64 {
            let crossings: Vec<usize> = samples
                .windows(2)
                .enumerate()
                .filter(|(_, pair)| pair[0] < 0 && pair[1] >= 0)
                .map(|(index, _)| index)
                .collect();
            let (Some(first), Some(last)) = (crossings.first(), crossings.last()) else {
                return 0.0;
            };
            if last == first {
                return 0.0;
            }
            (crossings.len() - 1) as f64 * f64::from(SAMPLE_RATE) / (last - first) as f64
        }

        #[test]
        fn duration_matches_encoder() {
            for mode in [ROBOT_36, SCOTTIE_2, PD_120] {
                let samples = black_transmission(mode);
                let expected = mode.duration().as_secs_f64() * f64::from(SAMPLE_RATE);
                assert!(
                    (samples.len() as f64 - expected).abs() <= 8.0,
                    "{mode:?}: {} samples, duration() says {expected}",
                    samples.len(),
                );
            }
        }

        #[test]
        fn sync_pulses_fall_on_the_grid() {
            for mode in [ROBOT_36, SCOTTIE_2, PD_120] {
                let samples = black_transmission(mode);
                let timing = mode.scan_timing();
                let header: f64 = mode
                    .header_tones()
                    .map(|tone| tone.duration.ns() as f64 / 1e9)
                    .sum();
                for k in [0_u32, 10, 100] {
                    let start = header + f64::from(k) * timing.line_seconds + timing.sync_offset();
                    // The middle half of the pulse, clear of its edges.
                    let from = ((start + timing.sync_seconds / 4.0) * f64::from(SAMPLE_RATE)) as usize;
                    let to =
                        ((start + timing.sync_seconds * 3.0 / 4.0) * f64::from(SAMPLE_RATE)) as usize;
                    let hz = frequency_of(&samples[from..to]);
                    assert!(
                        (hz - 1200.0).abs() < 100.0,
                        "{mode:?} line {k}: {hz} Hz where a sync pulse should be",
                    );
                }
            }
        }
    }
}
