// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Fitting any picture to an SSTV mode's exact size (`sstv-tx`).
//!
//! [`prepare_picture`] takes an 8-bit RGBA image of any size and returns the
//! [`SstvPicture`] that is both the preview a person sees and the exact pixels
//! that get sent. It is pure arithmetic: no image crate, no I/O, no clock.
//!
//! The resampling is separable. Along an axis that shrinks, each destination
//! pixel is the average of the source pixels its footprint covers, with
//! fractional weight at the edges (a box filter). Along an axis that stays the
//! same or grows, it is a bilinear sample. Values are averaged as they are
//! stored (no gamma conversion) and rounded to nearest at the end. Alpha is
//! composited over black before any averaging.

// Pixel and weight arithmetic moves between u8, usize and f32/f64 on purpose.
// Every value is a pixel index below `MAX_SOURCE_PIXELS` (well inside f64's
// exact range) or a channel value already clamped to 0..=255.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use crate::sstv_tx::{SstvPicture, SstvTxMode, sstv_tx_modes};

/// The largest source image `prepare_picture` accepts, in pixels.
pub const MAX_SOURCE_PIXELS: u64 = 64_000_000;

/// How a picture of another shape is made to fit the mode's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureFit {
    /// Scale to cover the whole frame and crop the overhang, centred.
    Fill,
    /// Scale to fit inside the frame and centre it on black bars.
    Fit,
}

/// Why a picture could not be prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparePictureError {
    /// The width or the height is 0.
    Empty,
    /// More than [`MAX_SOURCE_PIXELS`].
    TooLarge {
        /// The pixel count asked for.
        pixels: u64,
    },
    /// `rgba.len()` is not `width * height * 4`.
    WrongLength {
        /// `width * height * 4`.
        expected: usize,
        /// What was passed.
        got: usize,
    },
}

/// One entry of the send sheet's mode menu.
pub struct SendMode {
    /// The mode itself.
    pub mode: SstvTxMode,
    /// What the sheet calls it.
    pub name: &'static str,
    /// The mode's picture width in pixels.
    pub width: u32,
    /// The mode's picture height in pixels.
    pub height: u32,
}

const fn entry(mode: SstvTxMode, name: &'static str) -> SendMode {
    let (width, height) = mode.resolution();
    SendMode {
        mode,
        name,
        width,
        height,
    }
}

static SEND_MODES: [SendMode; 8] = [
    entry(sstv_tx_modes::ROBOT_36, "Robot 36"),
    entry(sstv_tx_modes::SCOTTIE_1, "Scottie 1"),
    entry(sstv_tx_modes::SCOTTIE_2, "Scottie 2"),
    entry(sstv_tx_modes::SCOTTIE_DX, "Scottie DX"),
    entry(sstv_tx_modes::MARTIN_1, "Martin 1"),
    entry(sstv_tx_modes::MARTIN_2, "Martin 2"),
    entry(sstv_tx_modes::PD_120, "PD120"),
    entry(sstv_tx_modes::PD_180, "PD180"),
];

/// The index in [`send_modes`] the sheet starts on: Scottie 2.
pub const DEFAULT_SEND_MODE: usize = 2;

/// The modes a person can pick, in menu order, with what the sheet shows.
#[must_use]
pub fn send_modes() -> &'static [SendMode] {
    &SEND_MODES
}

/// Scale and crop (or letterbox) `rgba` to exactly `mode`'s size.
///
/// `rgba` is 8-bit RGBA, row by row, not premultiplied. Alpha is composited
/// over black. The result is exactly the mode's size.
///
/// # Errors
///
/// [`PreparePictureError::Empty`] for a zero side, `TooLarge` above
/// [`MAX_SOURCE_PIXELS`] (checked before the buffer length), and `WrongLength`
/// when the buffer is not `width * height * 4` bytes.
pub fn prepare_picture(
    rgba: &[u8],
    width: u32,
    height: u32,
    mode: SstvTxMode,
    fit: PictureFit,
) -> Result<SstvPicture, PreparePictureError> {
    if width == 0 || height == 0 {
        return Err(PreparePictureError::Empty);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_SOURCE_PIXELS {
        return Err(PreparePictureError::TooLarge { pixels });
    }
    let expected = pixels as usize * 4;
    if rgba.len() != expected {
        return Err(PreparePictureError::WrongLength {
            expected,
            got: rgba.len(),
        });
    }

    let (dw, dh) = mode.resolution();
    let (sw, sh) = (f64::from(width), f64::from(height));
    let (fx, fy) = (f64::from(dw) / sw, f64::from(dh) / sh);

    // The part of the source that is used, and the size it is drawn at.
    let (region_x, region_y, draw_w, draw_h) = match fit {
        PictureFit::Fill => {
            let s = fx.max(fy);
            let (cw, ch) = ((f64::from(dw) / s).min(sw), (f64::from(dh) / s).min(sh));
            (((sw - cw) / 2.0, cw), ((sh - ch) / 2.0, ch), dw, dh)
        }
        PictureFit::Fit => {
            let s = fx.min(fy);
            let w = ((sw * s).round() as u32).clamp(1, dw);
            let h = ((sh * s).round() as u32).clamp(1, dh);
            ((0.0, sw), (0.0, sh), w, h)
        }
    };

    let xs = axis_weights(width as usize, region_x.0, region_x.1, draw_w as usize);
    let ys = axis_weights(height as usize, region_y.0, region_y.1, draw_h as usize);
    let drawn = resample(rgba, width as usize, &xs, &ys);

    let mut rgb = vec![0u8; dw as usize * dh as usize * 3];
    let (ox, oy) = (((dw - draw_w) / 2) as usize, ((dh - draw_h) / 2) as usize);
    let row_bytes = draw_w as usize * 3;
    for y in 0..draw_h as usize {
        let at = ((oy + y) * dw as usize + ox) * 3;
        rgb[at..at + row_bytes].copy_from_slice(&drawn[y * row_bytes..(y + 1) * row_bytes]);
    }
    Ok(SstvPicture {
        width: dw,
        height: dh,
        rgb,
    })
}

/// For each destination index along one axis: the first source index and the
/// weights over the source pixels from there. Weights sum to 1.
struct AxisWeights {
    first: Vec<usize>,
    weights: Vec<Vec<f32>>,
}

/// `start`/`len` is the source span (in source pixels, may be fractional) that
/// is stretched over `n` destination pixels.
fn axis_weights(src: usize, start: f64, len: f64, n: usize) -> AxisWeights {
    let scale = len / n as f64;
    let mut first = Vec::with_capacity(n);
    let mut weights = Vec::with_capacity(n);
    for d in 0..n {
        if scale > 1.0 {
            // Shrinking: average what the footprint covers.
            let lo = start + d as f64 * scale;
            let hi = lo + scale;
            let i0 = (lo.floor().max(0.0) as usize).min(src - 1);
            let i1 = (hi.ceil().max(1.0) as usize).min(src);
            let mut w: Vec<f64> = (i0..i1.max(i0 + 1))
                .map(|i| (hi.min(i as f64 + 1.0) - lo.max(i as f64)).max(0.0))
                .collect();
            let mut total: f64 = w.iter().sum();
            if total <= 0.0 {
                // Only when float error leaves the footprint past the last
                // source pixel: take that pixel whole.
                w = vec![1.0];
                total = 1.0;
            }
            first.push(i0);
            weights.push(w.iter().map(|v| (v / total) as f32).collect());
        } else {
            // Same size or growing: bilinear between pixel centres.
            let c = (start + (d as f64 + 0.5) * scale - 0.5).clamp(0.0, (src - 1) as f64);
            let i0 = c.floor() as usize;
            let t = (c - i0 as f64) as f32;
            first.push(i0);
            if i0 + 1 < src && t > 0.0 {
                weights.push(vec![1.0 - t, t]);
            } else {
                weights.push(vec![1.0]);
            }
        }
    }
    AxisWeights { first, weights }
}

/// Resample one source row at a time: filter it horizontally into a single
/// row buffer, then add it, weighted, into the destination rows that read it.
/// Peak extra memory is the destination picture in f32 plus one row, however
/// large or thin the source is. Alpha is applied (over black) as each source
/// pixel is read.
fn resample(rgba: &[u8], src_w: usize, xs: &AxisWeights, ys: &AxisWeights) -> Vec<u8> {
    let out_w = xs.first.len();
    let out_h = ys.first.len();
    let row_len = out_w * 3;
    let row_lo = ys.first.iter().copied().min().unwrap_or(0);
    let row_hi = ys
        .first
        .iter()
        .zip(&ys.weights)
        .map(|(f, w)| f + w.len())
        .max()
        .unwrap_or(0);

    let mut acc = vec![0f32; out_h * row_len];
    let mut mid = vec![0f32; row_len];
    // Destination rows read source rows in order, so this only moves forward.
    let mut dy_lo = 0;
    for src_row in row_lo..row_hi {
        let row = &rgba[src_row * src_w * 4..(src_row + 1) * src_w * 4];
        for dx in 0..out_w {
            let (f, w) = (xs.first[dx], &xs.weights[dx]);
            let mut sum = [0f32; 3];
            for (k, wk) in w.iter().enumerate() {
                let p = &row[(f + k) * 4..(f + k) * 4 + 4];
                let a = f32::from(p[3]) / 255.0;
                for c in 0..3 {
                    sum[c] += f32::from(p[c]) * a * wk;
                }
            }
            mid[dx * 3..dx * 3 + 3].copy_from_slice(&sum);
        }
        while dy_lo < out_h && ys.first[dy_lo] + ys.weights[dy_lo].len() <= src_row {
            dy_lo += 1;
        }
        let mut dy = dy_lo;
        while dy < out_h && ys.first[dy] <= src_row {
            let wk = ys.weights[dy][src_row - ys.first[dy]];
            for (a, m) in acc[dy * row_len..(dy + 1) * row_len].iter_mut().zip(&mid) {
                *a += m * wk;
            }
            dy += 1;
        }
    }

    acc.iter()
        .map(|v| v.round().clamp(0.0, 255.0) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Station;

    fn scottie2() -> SstvTxMode {
        sstv_tx_modes::SCOTTIE_2
    }

    fn px(p: &SstvPicture, x: usize, y: usize) -> [u8; 3] {
        let i = (y * p.width as usize + x) * 3;
        [p.rgb[i], p.rgb[i + 1], p.rgb[i + 2]]
    }

    fn near(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
        a.iter()
            .zip(&b)
            .all(|(x, y)| (i32::from(*x) - i32::from(*y)).abs() <= tol)
    }

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const BLACK: [u8; 3] = [0, 0, 0];

    fn rgba_from(cols: usize, rows: usize, color: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(cols * rows * 4);
        for y in 0..rows {
            for x in 0..cols {
                let [r, g, b] = color(x, y);
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        out
    }

    /// Red, green, blue in 1:2:1 along `len`.
    fn band(i: usize, len: usize) -> [u8; 3] {
        if i < len / 4 {
            RED
        } else if i < len * 3 / 4 {
            GREEN
        } else {
            BLUE
        }
    }

    #[test]
    fn a_mode_sized_image_is_unchanged() {
        let g = |x: usize, y: usize| [x as u8, y as u8, (x + y) as u8];
        let src = rgba_from(320, 256, g);
        for fit in [PictureFit::Fill, PictureFit::Fit] {
            let p = prepare_picture(&src, 320, 256, scottie2(), fit).unwrap();
            assert_eq!((p.width, p.height), (320, 256));
            for y in 0..256 {
                for x in 0..320 {
                    assert_eq!(px(&p, x, y), g(x, y), "{fit:?} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn fill_crops_the_sides_of_a_wide_image() {
        let src = rgba_from(640, 256, |x, _| band(x, 640));
        let p = prepare_picture(&src, 640, 256, scottie2(), PictureFit::Fill).unwrap();
        for y in 0..256 {
            for x in 0..320 {
                assert!(near(px(&p, x, y), GREEN, 1), "at {x},{y}");
            }
        }
    }

    #[test]
    fn fit_letterboxes_a_wide_image() {
        let src = rgba_from(640, 256, |x, _| band(x, 640));
        let p = prepare_picture(&src, 640, 256, scottie2(), PictureFit::Fit).unwrap();
        for y in (0..64).chain(192..256) {
            for x in 0..320 {
                assert_eq!(px(&p, x, y), BLACK, "bar at {x},{y}");
            }
        }
        assert!(near(px(&p, 20, 128), RED, 1));
        assert!(near(px(&p, 160, 128), GREEN, 1));
        assert!(near(px(&p, 300, 128), BLUE, 1));
    }

    #[test]
    fn fill_crops_top_and_bottom_of_a_tall_image() {
        let src = rgba_from(320, 640, |_, y| band(y, 640));
        let p = prepare_picture(&src, 320, 640, scottie2(), PictureFit::Fill).unwrap();
        // 320x640 scaled 1:1 in x; 256 of the 640 rows survive, all green.
        for y in 0..256 {
            for x in 0..320 {
                assert!(near(px(&p, x, y), GREEN, 1), "at {x},{y}");
            }
        }
    }

    #[test]
    fn fit_pillarboxes_a_tall_image() {
        let src = rgba_from(128, 512, |_, y| band(y, 512));
        let p = prepare_picture(&src, 128, 512, scottie2(), PictureFit::Fit).unwrap();
        // 128x512 into 320x256: drawn 64x256, columns 128..192.
        for y in 0..256 {
            for x in (0..128).chain(192..320) {
                assert_eq!(px(&p, x, y), BLACK, "bar at {x},{y}");
            }
        }
        assert!(near(px(&p, 160, 20), RED, 1));
        assert!(near(px(&p, 160, 128), GREEN, 1));
        assert!(near(px(&p, 160, 235), BLUE, 1));
    }

    #[test]
    fn shrinking_averages() {
        let src = rgba_from(
            640,
            512,
            |x, y| if (x + y) % 2 == 0 { [0; 3] } else { [255; 3] },
        );
        let p = prepare_picture(&src, 640, 512, scottie2(), PictureFit::Fill).unwrap();
        assert!(p.rgb.iter().all(|&v| v == 127 || v == 128));
    }

    #[test]
    fn enlarging_is_smooth() {
        let src = rgba_from(
            2,
            2,
            |x, y| if (x + y) % 2 == 0 { [0; 3] } else { [255; 3] },
        );
        let p = prepare_picture(&src, 2, 2, sstv_tx_modes::ROBOT_36, PictureFit::Fill).unwrap();
        let (w, h) = (p.width as usize, p.height as usize);
        let c = px(&p, w / 2, h / 2);
        assert!(c.iter().all(|&v| (120..=136).contains(&v)), "{c:?}");
        assert!(near(px(&p, 0, 0), [0; 3], 8));
        assert!(near(px(&p, w - 1, 0), [255; 3], 8));
        assert!(near(px(&p, 0, h - 1), [255; 3], 8));
        assert!(near(px(&p, w - 1, h - 1), [0; 3], 8));
    }

    #[test]
    fn alpha_is_over_black() {
        let clear: Vec<u8> = [255, 255, 255, 0].repeat(4);
        let p = prepare_picture(&clear, 2, 2, scottie2(), PictureFit::Fill).unwrap();
        assert!(p.rgb.iter().all(|&v| v == 0));
        let half: Vec<u8> = [255, 255, 255, 128].repeat(4);
        let p = prepare_picture(&half, 2, 2, scottie2(), PictureFit::Fill).unwrap();
        assert!(p.rgb.iter().all(|&v| v == 128 || v == 127));
    }

    #[test]
    fn odd_shapes() {
        for (w, h) in [(1u32, 1u32), (1, 4000), (4000, 1)] {
            let src = vec![200u8; w as usize * h as usize * 4];
            for m in send_modes() {
                for fit in [PictureFit::Fill, PictureFit::Fit] {
                    let p = prepare_picture(&src, w, h, m.mode, fit).unwrap();
                    assert_eq!((p.width, p.height), (m.width, m.height), "{}", m.name);
                    assert_eq!(p.rgb.len(), m.width as usize * m.height as usize * 3);
                }
            }
        }
    }

    #[test]
    fn refusals() {
        let m = scottie2();
        let f = PictureFit::Fill;
        assert_eq!(
            prepare_picture(&[], 0, 10, m, f).unwrap_err(),
            PreparePictureError::Empty
        );
        assert_eq!(
            prepare_picture(&[], 10, 0, m, f).unwrap_err(),
            PreparePictureError::Empty
        );
        assert_eq!(
            prepare_picture(&[0; 4], 8001, 8001, m, f).unwrap_err(),
            PreparePictureError::TooLarge {
                pixels: 8001 * 8001
            }
        );
        assert_eq!(
            prepare_picture(&[0; 399], 10, 10, m, f).unwrap_err(),
            PreparePictureError::WrongLength {
                expected: 400,
                got: 399
            }
        );
    }

    /// What each menu entry must be: name, VIS code identity, literal size.
    fn expected_modes() -> [(&'static str, SstvTxMode, u32, u32); 8] {
        use sstv_tx_modes::{
            MARTIN_1, MARTIN_2, PD_120, PD_180, ROBOT_36, SCOTTIE_1, SCOTTIE_2, SCOTTIE_DX,
        };
        [
            ("Robot 36", ROBOT_36, 320, 240),
            ("Scottie 1", SCOTTIE_1, 320, 256),
            ("Scottie 2", SCOTTIE_2, 320, 256),
            ("Scottie DX", SCOTTIE_DX, 320, 256),
            ("Martin 1", MARTIN_1, 320, 256),
            ("Martin 2", MARTIN_2, 320, 256),
            ("PD120", PD_120, 640, 496),
            ("PD180", PD_180, 640, 496),
        ]
    }

    #[test]
    fn send_modes_are_the_eight() {
        let want = expected_modes();
        assert_eq!(send_modes().len(), want.len());
        for (got, (name, mode, width, height)) in send_modes().iter().zip(want) {
            assert_eq!(got.name, name);
            assert_eq!(got.mode.vis_code(), mode.vis_code(), "{name}: wrong mode");
            assert_eq!((got.width, got.height), (width, height), "{name}");
            assert_eq!(got.mode.resolution(), (width, height), "{name}");
            assert!(Station::picture_duration(got.mode) < std::time::Duration::from_secs(600));
        }
        let default = &send_modes()[DEFAULT_SEND_MODE];
        assert_eq!(default.name, "Scottie 2");
        assert_eq!(default.mode.vis_code(), scottie2().vis_code());
    }

    #[test]
    fn letterbox_and_pillarbox_extents_are_exact() {
        let wide = rgba_from(640, 256, |_, _| GREEN);
        let p = prepare_picture(&wide, 640, 256, scottie2(), PictureFit::Fit).unwrap();
        assert_eq!(px(&p, 160, 63), BLACK);
        assert_eq!(px(&p, 160, 64), GREEN);
        assert_eq!(px(&p, 160, 191), GREEN);
        assert_eq!(px(&p, 160, 192), BLACK);
        let tall = rgba_from(128, 512, |_, _| GREEN);
        let p = prepare_picture(&tall, 128, 512, scottie2(), PictureFit::Fit).unwrap();
        assert_eq!(px(&p, 127, 128), BLACK);
        assert_eq!(px(&p, 128, 128), GREEN);
        assert_eq!(px(&p, 191, 128), GREEN);
        assert_eq!(px(&p, 192, 128), BLACK);
    }

    #[test]
    fn odd_centring_is_within_one_pixel() {
        // 321x1 into 320x256 Fit: drawn 320x1, centred at row 127 or 128.
        let src = rgba_from(321, 1, |_, _| GREEN);
        let p = prepare_picture(&src, 321, 1, scottie2(), PictureFit::Fit).unwrap();
        let rows: Vec<usize> = (0..256).filter(|&y| px(&p, 160, y) != BLACK).collect();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!((127..=128).contains(&rows[0]), "{rows:?}");
        // 1x321 into 320x256 Fit: drawn 1x256 wide of 1, columns near 160.
        let src = rgba_from(1, 321, |_, _| GREEN);
        let p = prepare_picture(&src, 1, 321, scottie2(), PictureFit::Fit).unwrap();
        let cols: Vec<usize> = (0..320).filter(|&x| px(&p, x, 128) != BLACK).collect();
        assert_eq!(cols.len(), 1, "{cols:?}");
        assert!((159..=160).contains(&cols[0]), "{cols:?}");
    }

    #[test]
    fn odd_crop_is_centred_within_one_pixel() {
        // 321 wide, left column red, right column blue, rest green; Fill to
        // 320x256 at scale 1 drops exactly one column: left or right, never both.
        let src = rgba_from(321, 256, |x, _| match x {
            0 => RED,
            320 => BLUE,
            _ => GREEN,
        });
        let p = prepare_picture(&src, 321, 256, scottie2(), PictureFit::Fill).unwrap();
        let left = px(&p, 0, 100);
        let right = px(&p, 319, 100);
        let lost_left = near(left, GREEN, 140) && near(right, BLUE, 140);
        let lost_right = near(left, RED, 140) && near(right, GREEN, 140);
        assert!(lost_left || lost_right, "{left:?} {right:?}");
        // The middle is untouched either way.
        assert!(near(px(&p, 160, 100), GREEN, 1));
    }

    #[test]
    fn very_thin_sources_stay_small() {
        // 8 million thin pixels take ~2 s in a debug build; 2 million are enough
        // to show memory does not scale with the source's long side.
        let n = 2_000_000usize;
        let tall = vec![180u8; n * 4];
        let wide = tall.clone();
        for fit in [PictureFit::Fill, PictureFit::Fit] {
            let p = prepare_picture(&tall, 1, n as u32, scottie2(), fit).unwrap();
            assert_eq!((p.width, p.height), (320, 256));
            let p = prepare_picture(&wide, n as u32, 1, scottie2(), fit).unwrap();
            assert_eq!((p.width, p.height), (320, 256));
        }
    }

    #[test]
    #[ignore = "timing probe: run with --release -- --ignored --nocapture"]
    fn twelve_megapixels_timing() {
        let src = vec![90u8; 4000 * 3000 * 4];
        let t = std::time::Instant::now();
        let p = prepare_picture(&src, 4000, 3000, scottie2(), PictureFit::Fill).unwrap();
        eprintln!("12 MP prepare_picture: {:?}", t.elapsed());
        assert_eq!(p.width, 320);
    }
}
