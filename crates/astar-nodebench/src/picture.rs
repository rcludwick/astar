// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The SSTV test picture, and how far a decoded picture is from it.
//!
//! [`test_picture`] has detail in every colour channel: colour bars across
//! the top half, a grey ramp below them, and a diagonal line through the
//! whole picture, so a picture that comes back shifted, smeared or with one
//! channel missing shows up as a large error.
//!
//! [`mean_error`] is the plain measure: the mean absolute difference per
//! colour channel, 0 to 255. [`fitted_error`] forgives a decoder that maps
//! tones to brightness on its own scale: it first fits, per channel, the
//! straight line `decoded ≈ gain × original + offset`, and measures what is
//! left over. The outside decoder (RIG.md, "SSTV: the outside decoder")
//! needs that: its bars come out darker and its ramp reaches white early,
//! which says nothing about whether the picture arrived.

use astar_station::SstvPicture;

/// Seven colour bars (75 % white, yellow, cyan, green, magenta, red, blue).
const BARS: [[u8; 3]; 7] = [
    [191, 191, 191],
    [191, 191, 0],
    [0, 191, 191],
    [0, 191, 0],
    [191, 0, 191],
    [191, 0, 0],
    [0, 0, 191],
];

/// The test picture at `width` × `height`: colour bars over the top half, a
/// grey ramp from black to white across the bottom half, and a white
/// diagonal from the top-left corner to the bottom-right one.
#[must_use]
pub fn test_picture(width: u32, height: u32) -> SstvPicture {
    let (w, h) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let mut rgb = Vec::with_capacity(usize::try_from(w * h * 3).unwrap_or(0));
    for y in 0..h {
        for x in 0..w {
            // The diagonal: about 5 pixels wide across (at 320 × 240), so a
            // decoder's smoothing keeps it.
            let on_diagonal = (x * h).abs_diff(y * w) < 2 * w.max(h);
            let px = if on_diagonal {
                [255, 255, 255]
            } else if y < h / 2 {
                BARS[usize::try_from(x * 7 / w).unwrap_or(0)]
            } else {
                let grey = u8::try_from(x * 255 / (w - 1).max(1)).unwrap_or(u8::MAX);
                [grey, grey, grey]
            };
            rgb.extend_from_slice(&px);
        }
    }
    SstvPicture { width, height, rgb }
}

/// Mean absolute difference per colour channel, 0–255. Sizes must match.
///
/// # Panics
/// If the two pictures differ in length.
#[must_use]
pub fn mean_error(a_rgb: &[u8], b_rgb: &[u8]) -> f64 {
    assert_eq!(a_rgb.len(), b_rgb.len(), "pictures differ in size");
    if a_rgb.is_empty() {
        return 0.0;
    }
    let total: u64 = a_rgb
        .iter()
        .zip(b_rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total as f64 / a_rgb.len() as f64;
    mean
}

/// The error left after forgiving a decoder its own brightness scale.
///
/// Per colour channel, fit `decoded ≈ gain × original + offset` by least
/// squares, then take the mean absolute residual over all channels, 0–255.
/// The gain is held at zero or above, so an inverted channel is not
/// forgiven; a channel whose original is flat (no variation to fit against)
/// is fitted with its offset only. Sizes must match.
///
/// # Panics
/// If the two pictures differ in length.
#[must_use]
pub fn fitted_error(original_rgb: &[u8], decoded_rgb: &[u8]) -> f64 {
    assert_eq!(
        original_rgb.len(),
        decoded_rgb.len(),
        "pictures differ in size"
    );
    if original_rgb.is_empty() {
        return 0.0;
    }
    let mut residual = 0.0;
    for ch in 0..3 {
        let xs: Vec<f64> = original_rgb
            .iter()
            .skip(ch)
            .step_by(3)
            .map(|&v| f64::from(v))
            .collect();
        let ys: Vec<f64> = decoded_rgb
            .iter()
            .skip(ch)
            .step_by(3)
            .map(|&v| f64::from(v))
            .collect();
        #[allow(clippy::cast_precision_loss)]
        let n = xs.len() as f64;
        let mx = xs.iter().sum::<f64>() / n;
        let my = ys.iter().sum::<f64>() / n;
        let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
        let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
        let gain = if sxx > 0.0 { (sxy / sxx).max(0.0) } else { 0.0 };
        let offset = my - gain * mx;
        residual += xs
            .iter()
            .zip(&ys)
            .map(|(x, y)| (y - (gain * x + offset)).abs())
            .sum::<f64>();
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = residual / original_rgb.len() as f64;
    mean
}

/// The first `rows` rows of a `width`-wide RGB picture.
#[must_use]
pub fn top_rows(rgb: &[u8], width: u32, rows: u32) -> &[u8] {
    let len = usize::try_from(u64::from(width) * u64::from(rows) * 3).unwrap_or(usize::MAX);
    &rgb[..len.min(rgb.len())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_picture_against_itself_is_zero() {
        let p = test_picture(320, 240);
        assert_eq!(p.rgb.len(), 320 * 240 * 3);
        assert!(mean_error(&p.rgb, &p.rgb).abs() < f64::EPSILON);
        assert!(fitted_error(&p.rgb, &p.rgb) < 1e-9);
    }

    #[test]
    fn a_picture_against_its_inverse_is_over_100() {
        let p = test_picture(320, 240);
        let inverse: Vec<u8> = p.rgb.iter().map(|v| 255 - v).collect();
        let e = mean_error(&p.rgb, &inverse);
        assert!(e > 100.0, "{e}");
    }

    #[test]
    fn every_channel_has_detail() {
        let p = test_picture(320, 240);
        for ch in 0..3 {
            let distinct: HashSet<u8> = p.rgb.iter().skip(ch).step_by(3).copied().collect();
            assert!(distinct.len() > 16, "channel {ch}: {}", distinct.len());
        }
    }

    #[test]
    fn fitted_error_forgives_a_brightness_scale() {
        // A decoder that reads everything 20 % dark and 10 levels high is
        // far off by the plain measure and spot on by the fitted one.
        let p = test_picture(320, 240);
        let scaled: Vec<u8> = p
            .rgb
            .iter()
            .map(|&v| {
                let y = f64::from(v) * 0.8 + 10.0;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let y = y.round() as u8;
                y
            })
            .collect();
        assert!(mean_error(&p.rgb, &scaled) > 10.0);
        assert!(fitted_error(&p.rgb, &scaled) < 0.6);
    }

    #[test]
    fn fitted_error_still_sees_a_wrong_picture() {
        // A picture shifted sideways cannot be fitted away.
        let p = test_picture(320, 240);
        let mut shifted = p.rgb.clone();
        shifted.rotate_left(40 * 3);
        assert!(fitted_error(&p.rgb, &shifted) > 20.0);
        // An inverse would be a perfect fit with a negative gain; the gain
        // is held at zero or above, so it is not forgiven.
        let inverse: Vec<u8> = p.rgb.iter().map(|v| 255 - v).collect();
        assert!(fitted_error(&p.rgb, &inverse) > 50.0);
    }

    #[test]
    fn top_rows_cuts_the_picture() {
        let p = test_picture(640, 496);
        assert_eq!(top_rows(&p.rgb, 640, 494).len(), 640 * 494 * 3);
        assert_eq!(top_rows(&p.rgb, 640, 600).len(), p.rgb.len());
    }
}
