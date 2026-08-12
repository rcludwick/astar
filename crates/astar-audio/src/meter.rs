// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Cheap peak-amplitude level metering.
//!
//! Computed inside the cpal audio callback once per buffer, then handed to
//! the sink/source via [`InputSink::write`](crate::InputSink::write) /
//! [`OutputSource::meter`](crate::OutputSource::meter). The meter is the
//! maximum absolute sample value, normalised to `0.0..=1.0` (assuming
//! standard f32 PCM clamped at ±1.0).
//!
//! We use peak rather than RMS because:
//! - it's a single branchless `max` in the hot loop;
//! - VU-style mic meters in voice apps want responsiveness to peaks
//!   (the use case here is astar's PTT mic indicator);
//! - downstream UIs can apply their own smoothing.

/// Compute the peak amplitude across the slice. Returns `0.0` for an empty
/// slice. Treats NaN as zero (NaN doesn't compare with `max`).
#[must_use]
pub fn peak(samples: &[f32]) -> f32 {
    let mut p = 0.0_f32;
    for &s in samples {
        let a = s.abs();
        if a > p {
            p = a;
        }
    }
    p
}

/// Convert a peak amplitude in `0.0..=1.0` to dBFS, clamped to `[-60, 0]`.
/// Silence (`<= 1e-6`) and `NaN` floor at `-60`; overdriven input (`> 1.0`)
/// reports `0` dBFS.
#[must_use]
pub fn peak_to_dbfs(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 1e-6 {
        -60.0
    } else {
        (20.0 * peak.log10()).clamp(-60.0, 0.0)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn peak_empty_is_zero() {
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn peak_picks_largest_absolute() {
        assert!((peak(&[0.1, -0.7, 0.3, -0.5]) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn peak_handles_clamped_signal() {
        assert!((peak(&[1.0, -1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn peak_ignores_nan() {
        // NaN.abs() is NaN; comparisons against NaN are false, so p stays
        // at the previously-seen real peak.
        let p = peak(&[0.4, f32::NAN, -0.2]);
        assert!((p - 0.4).abs() < 1e-6);
    }
}
