// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The SSTV modes astar names, independent of the decoder's own types, and
//! slowrx's scan timing in the form the `sstv` crate's de-jitter reads.

use serde::{Serialize, Serializer};
use slowrx::{ChannelLayout, ModeSpec, SstvMode};
use sstv::timing::{ScanTiming, SyncPosition};

/// An SSTV transmission mode, as the VIS header announced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Pd120,
    Pd180,
    Pd240,
    Robot24,
    Robot36,
    Robot72,
    Robot24Bw,
    Robot12Bw,
    Robot8Bw,
    Scottie1,
    Scottie2,
    ScottieDx,
    Martin1,
    Martin2,
    Wraase120,
    Wraase180,
    /// A mode a newer decoder knows and this build does not name.
    Other,
}

impl Mode {
    /// The name people know it by, as shown in the app: `"Robot 36"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pd120 => "PD120",
            Self::Pd180 => "PD180",
            Self::Pd240 => "PD240",
            Self::Robot24 => "Robot 24",
            Self::Robot36 => "Robot 36",
            Self::Robot72 => "Robot 72",
            Self::Robot24Bw => "Robot 24 B/W",
            Self::Robot12Bw => "Robot 12 B/W",
            Self::Robot8Bw => "Robot 8 B/W",
            Self::Scottie1 => "Scottie 1",
            Self::Scottie2 => "Scottie 2",
            Self::ScottieDx => "Scottie DX",
            Self::Martin1 => "Martin 1",
            Self::Martin2 => "Martin 2",
            Self::Wraase120 => "Wraase 120",
            Self::Wraase180 => "Wraase 180",
            Self::Other => "SSTV",
        }
    }

    /// The name with no spaces or slashes, for file names: `"Robot36"`.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pd120 => "PD120",
            Self::Pd180 => "PD180",
            Self::Pd240 => "PD240",
            Self::Robot24 => "Robot24",
            Self::Robot36 => "Robot36",
            Self::Robot72 => "Robot72",
            Self::Robot24Bw => "Robot24BW",
            Self::Robot12Bw => "Robot12BW",
            Self::Robot8Bw => "Robot8BW",
            Self::Scottie1 => "Scottie1",
            Self::Scottie2 => "Scottie2",
            Self::ScottieDx => "ScottieDX",
            Self::Martin1 => "Martin1",
            Self::Martin2 => "Martin2",
            Self::Wraase120 => "Wraase120",
            Self::Wraase180 => "Wraase180",
            Self::Other => "SSTV",
        }
    }

    pub(crate) fn from_slowrx(mode: slowrx::SstvMode) -> Self {
        use slowrx::SstvMode as S;
        match mode {
            S::Pd120 => Self::Pd120,
            S::Pd180 => Self::Pd180,
            S::Pd240 => Self::Pd240,
            S::Robot24 => Self::Robot24,
            S::Robot36 => Self::Robot36,
            S::Robot72 => Self::Robot72,
            S::Robot24Bw => Self::Robot24Bw,
            S::Robot12Bw => Self::Robot12Bw,
            S::Robot8Bw => Self::Robot8Bw,
            S::Scottie1 => Self::Scottie1,
            S::Scottie2 => Self::Scottie2,
            S::ScottieDx => Self::ScottieDx,
            S::Martin1 => Self::Martin1,
            S::Martin2 => Self::Martin2,
            S::Wraase120 => Self::Wraase120,
            S::Wraase180 => Self::Wraase180,
            _ => Self::Other,
        }
    }
}

/// slowrx's timing for a mode, as the `sstv` crate's [`ScanTiming`], so the
/// de-jitter in front of slowrx puts the lines where slowrx looks for them.
///
/// Every field means what it means in the `sstv` crate, with slowrx's own
/// numbers: a line is sync pulse to sync pulse (a PD line carries two rows),
/// `porch_seconds` is slowrx's separator between scans, and `pixel_seconds`
/// is a pixel of the first scan as slowrx scans it (Robot 24 and 36 send
/// their luminance at half speed).
pub(crate) fn scan_timing(spec: &ModeSpec) -> ScanTiming {
    let (rows_per_line, channels) = match spec.channel_layout {
        ChannelLayout::PdYcbcr => (2, 4),
        // Luminance and one colour difference a line.
        ChannelLayout::RobotYuv if spec.mode != SstvMode::Robot72 => (1, 2),
        ChannelLayout::Monochrome => (1, 1),
        _ => (1, 3),
    };
    let luminance_at_half_speed =
        spec.channel_layout == ChannelLayout::RobotYuv && spec.mode != SstvMode::Robot72;
    ScanTiming {
        line_seconds: spec.line_seconds,
        sync_seconds: spec.sync_seconds,
        porch_seconds: spec.septr_seconds,
        pixel_seconds: if luminance_at_half_speed {
            2.0 * spec.pixel_seconds
        } else {
            spec.pixel_seconds
        },
        line_pixels: spec.line_pixels,
        image_lines: spec.image_lines,
        sync_position: match spec.sync_position {
            slowrx::SyncPosition::Scottie => SyncPosition::Scottie,
            _ => SyncPosition::Start,
        },
        rows_per_line,
        channels,
    }
}

impl Serialize for Mode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, scan_timing};
    use slowrx::SstvMode as S;
    use sstv::modes;

    /// slowrx's timing as astar hands it to the de-jitter agrees with the
    /// `sstv` crate's for every mode both know, to within 1 µs a field.
    ///
    /// The exceptions are where the two crates model a mode differently.
    /// Each is pinned to the exact values, so a change to either crate shows
    /// here. Only the first is a field the de-jitter reads for that mode;
    /// there each decoder's de-jitter uses its own decoder's line length.
    ///
    /// * Scottie 1's line: slowrx 428.38 ms, the `sstv` crate 428.22 ms (the
    ///   sum of the published scan, sync and separator times).
    /// * The Robot modes' separator: slowrx counts the separator and the
    ///   porch after it as one (6 ms, 4.7 ms); the `sstv` crate the separator
    ///   tone alone (4.5 ms). A Robot line starts with its sync pulse, so the
    ///   de-jitter never reads it.
    /// * Robot 72's pixel: slowrx scans its three channels at one speed;
    ///   the `sstv` crate scans luminance 1.5 times slower. Unread, as above.
    #[test]
    fn scan_timing_agrees_with_slowrx() {
        let pairs = [
            (S::Robot36, modes::ROBOT_36),
            (S::Robot72, modes::ROBOT_72),
            (S::Scottie1, modes::SCOTTIE_1),
            (S::Scottie2, modes::SCOTTIE_2),
            (S::ScottieDx, modes::SCOTTIE_DX),
            (S::Martin1, modes::MARTIN_1),
            (S::Martin2, modes::MARTIN_2),
            (S::Pd120, modes::PD_120),
            (S::Pd180, modes::PD_180),
            (S::Pd240, modes::PD_240),
            (S::Wraase180, modes::WRASSE_SC2_180),
        ];
        let close = |a: f64, b: f64| (a - b).abs() <= 1e-6;
        for (slow, fork) in pairs {
            let ours = scan_timing(&slowrx::for_mode(slow));
            let theirs = fork.scan_timing();
            let differs = |field: &str, a: f64, b: f64| -> bool {
                let known = match (slow, field) {
                    (S::Scottie1, "line") => Some((0.428_38, 0.428_22)),
                    (S::Robot36, "porch") => Some((0.006, 0.004_5)),
                    (S::Robot72, "porch") => Some((0.004_7, 0.004_5)),
                    (S::Robot72, "pixel") => Some((0.000_287_5, 0.000_431_25)),
                    _ => None,
                };
                match known {
                    Some((x, y)) => {
                        assert!(close(a, x) && close(b, y), "{slow:?} {field}: {a} vs {b}");
                        false
                    }
                    None => !close(a, b),
                }
            };
            assert!(
                !differs("line", ours.line_seconds, theirs.line_seconds),
                "{slow:?} line: {} vs {}",
                ours.line_seconds,
                theirs.line_seconds
            );
            assert!(
                !differs("sync", ours.sync_seconds, theirs.sync_seconds),
                "{slow:?} sync"
            );
            assert!(
                !differs("porch", ours.porch_seconds, theirs.porch_seconds),
                "{slow:?} porch: {} vs {}",
                ours.porch_seconds,
                theirs.porch_seconds
            );
            assert!(
                !differs("pixel", ours.pixel_seconds, theirs.pixel_seconds),
                "{slow:?} pixel: {} vs {}",
                ours.pixel_seconds,
                theirs.pixel_seconds
            );
            assert_eq!(
                (ours.line_pixels, ours.image_lines, ours.sync_position),
                (theirs.line_pixels, theirs.image_lines, theirs.sync_position),
                "{slow:?}"
            );
            assert_eq!(
                (ours.rows_per_line, ours.channels),
                (theirs.rows_per_line, theirs.channels),
                "{slow:?}"
            );
        }
    }

    const ALL: [Mode; 17] = [
        Mode::Pd120,
        Mode::Pd180,
        Mode::Pd240,
        Mode::Robot24,
        Mode::Robot36,
        Mode::Robot72,
        Mode::Robot24Bw,
        Mode::Robot12Bw,
        Mode::Robot8Bw,
        Mode::Scottie1,
        Mode::Scottie2,
        Mode::ScottieDx,
        Mode::Martin1,
        Mode::Martin2,
        Mode::Wraase120,
        Mode::Wraase180,
        Mode::Other,
    ];

    #[test]
    fn tokens_and_names_match_r7() {
        assert_eq!(Mode::Pd120.token(), "PD120");
        assert_eq!(Mode::Robot36.token(), "Robot36");
        assert_eq!(Mode::Martin1.token(), "Martin1");
        assert_eq!(Mode::ScottieDx.token(), "ScottieDX");
        assert_eq!(Mode::Robot24Bw.token(), "Robot24BW");
        assert_eq!(Mode::Wraase180.token(), "Wraase180");
        assert_eq!(Mode::Other.token(), "SSTV");
        assert_eq!(Mode::Pd120.name(), "PD120");
        assert_eq!(Mode::Robot36.name(), "Robot 36");
        assert_eq!(Mode::ScottieDx.name(), "Scottie DX");
        assert_eq!(Mode::Robot24Bw.name(), "Robot 24 B/W");
        assert_eq!(Mode::Other.name(), "SSTV");
        // A token is always the display name without spaces or slashes.
        for m in ALL {
            assert_eq!(m.token(), m.name().replace([' ', '/'], ""), "{m:?}");
        }
        assert_eq!(
            serde_json::to_string(&Mode::Robot36).unwrap(),
            "\"Robot 36\""
        );
    }
}
