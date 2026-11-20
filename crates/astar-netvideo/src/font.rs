// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Text in Inter (SIL OFL 1.1; see `assets/fonts/VENDORED.md`), drawn onto a
//! `tiny_skia::Pixmap`.

// Pixel arithmetic: every cast is on a value bounded by the pixmap or by 0..=255.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use std::borrow::Cow;
use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, ScaleFont};
use tiny_skia::{Color, Pixmap};

/// The four Inter weights the video uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    SemiBold,
    Bold,
    ExtraBold,
}

static FACES: OnceLock<[FontRef<'static>; 4]> = OnceLock::new();

fn face(w: Weight) -> &'static FontRef<'static> {
    let faces = FACES.get_or_init(|| {
        let load =
            |bytes: &'static [u8]| FontRef::try_from_slice(bytes).expect("bundled Inter parses");
        [
            load(include_bytes!("../../../assets/fonts/Inter-Regular.ttf")),
            load(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf")),
            load(include_bytes!("../../../assets/fonts/Inter-Bold.ttf")),
            load(include_bytes!("../../../assets/fonts/Inter-ExtraBold.ttf")),
        ]
    });
    &faces[w as usize]
}

/// Scale that makes one em `px` pixels tall. `PxScale` is the font's full
/// height (ascent plus descent), which is taller than its em.
fn scale(f: &FontRef<'_>, px: f32) -> ab_glyph::PxScale {
    let em = f.units_per_em().unwrap_or(1000.0);
    ab_glyph::PxScale::from(px * f.height_unscaled() / em)
}

/// The widest digit advance at this scale, which every digit takes when
/// `tabular` is set (`ab_glyph` does not apply OpenType `tnum`).
fn digit_advance<F: Font>(s: &ab_glyph::PxScaleFont<&F>) -> f32 {
    ('0'..='9')
        .map(|d| s.h_advance(s.glyph_id(d)))
        .fold(0.0, f32::max)
}

/// Where each visible character goes: (char, pen x relative to the start, extra
/// shift to centre a tabular digit in its cell), plus the total width.
fn layout(text: &str, px: f32, w: Weight, tabular: bool) -> (Vec<(char, f32, f32)>, f32) {
    let f = face(w);
    let s = f.as_scaled(scale(f, px));
    let wide = digit_advance(&s);
    let mut pen = 0.0_f32;
    let mut prev = None;
    let mut out = Vec::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        let id = s.glyph_id(c);
        if let Some(p) = prev {
            pen += s.kern(p, id);
        }
        let own = s.h_advance(id);
        let (adv, shift) = if tabular && c.is_ascii_digit() {
            (wide, (wide - own) / 2.0)
        } else {
            (own, 0.0)
        };
        out.push((c, pen, shift));
        pen += adv;
        prev = Some(id);
    }
    (out, pen)
}

/// The face's ascent and descent per pixel of em (both positive), for
/// placing a baseline inside a CSS line box.
#[must_use]
pub(crate) fn v_metrics(w: Weight) -> (f32, f32) {
    let f = face(w);
    let em = f.units_per_em().unwrap_or(1000.0);
    (f.ascent_unscaled() / em, -f.descent_unscaled() / em)
}

/// Whether Inter has a glyph for `c` (emoji and most scripts it does not).
#[must_use]
pub(crate) fn has_glyph(c: char, w: Weight) -> bool {
    face(w).glyph_id(c).0 != 0
}

/// Width of `text` in pixels. Control characters take no room.
#[must_use]
pub fn measure(text: &str, px: f32, w: Weight, tabular: bool) -> f32 {
    layout(text, px, w, tabular).1
}

/// `text` without control characters, cut and ended with `…` so it measures at
/// most `max_w`. Text that already fits comes back unchanged.
///
/// Off-air text can be any length, so only its first
/// [`MAX_TEXT_CHARS`](crate::scene::MAX_TEXT_CHARS) characters are looked at,
/// and the cut is found by binary search: a hostile 100 000-character name
/// costs a handful of measures, not a hundred thousand.
///
/// Characters are laid out one by one, left to right: there is no shaping
/// (ligatures, Arabic joining, Indic reordering) and no bidi reordering, so
/// right-to-left text draws in logical order. Kerning comes from the legacy
/// `kern` table only; Inter keeps its kerning in GPOS, which `ab_glyph` does
/// not read, so in practice none is applied. Callsigns are Latin letters
/// and digits, which none of this affects.
#[must_use]
pub fn fit(text: &str, max_w: f32, px: f32, w: Weight) -> Cow<'_, str> {
    let limit = crate::scene::MAX_TEXT_CHARS;
    let clean: Cow<'_, str> =
        if text.chars().any(char::is_control) || text.chars().nth(limit).is_some() {
            Cow::Owned(
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(limit)
                    .collect(),
            )
        } else {
            Cow::Borrowed(text)
        };
    if measure(&clean, px, w, false) <= max_w {
        return clean;
    }
    let chars: Vec<char> = clean.chars().collect();
    let cut = |n: usize| {
        let mut s: String = chars[..n].iter().collect::<String>().trim_end().to_owned();
        s.push('…');
        s
    };
    // The longest prefix whose cut fits; `lo` always fits, or is 0.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&cut(mid), px, w, false) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 && measure("…", px, w, false) > max_w {
        return Cow::Borrowed("");
    }
    Cow::Owned(cut(lo))
}

/// Draws `text` with its left edge at `x` and its baseline at `baseline`.
/// `tabular` gives every digit the widest digit's advance, so a ticking clock
/// does not shimmer.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    p: &mut Pixmap,
    text: &str,
    x: f32,
    baseline: f32,
    px: f32,
    w: Weight,
    color: Color,
    tabular: bool,
) {
    let f = face(w);
    let sc = scale(f, px);
    let (glyphs, _) = layout(text, px, w, tabular);
    let (width, height) = (p.width(), p.height());
    let (wi, hi) = (width as i32, height as i32);
    let c = color.to_color_u8();
    let data = p.data_mut();
    for (ch, pen, shift) in glyphs {
        let glyph = f
            .glyph_id(ch)
            .with_scale_and_position(sc, ab_glyph::point(x + pen + shift, baseline));
        let Some(outlined) = f.outline_glyph(glyph) else {
            continue;
        };
        let b = outlined.px_bounds();
        outlined.draw(|gx, gy, cov| {
            let (px_x, px_y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if px_x < 0 || px_y < 0 || px_x >= wi || px_y >= hi {
                return;
            }
            let a = cov.clamp(0.0, 1.0) * f32::from(c.alpha()) / 255.0;
            let i = (px_y as usize * width as usize + px_x as usize) * 4;
            let src = [c.red(), c.green(), c.blue()];
            for k in 0..3 {
                let s = f32::from(src[k]) * a;
                let d = f32::from(data[i + k]) * (1.0 - a);
                data[i + k] = (s + d).round().min(255.0) as u8;
            }
            let da = f32::from(data[i + 3]) * (1.0 - a);
            data[i + 3] = (a * 255.0 + da).round().min(255.0) as u8;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wider_weight_measures_wider() {
        let bold = measure("KX0AAA", 52.0, Weight::ExtraBold, false);
        let plain = measure("KX0AAA", 52.0, Weight::Regular, false);
        assert!(bold > plain, "{bold} vs {plain}");
    }

    #[test]
    fn tabular_digits_share_one_advance() {
        let ones = measure("1111", 40.0, Weight::Bold, true);
        let zeros = measure("0000", 40.0, Weight::Bold, true);
        assert!((ones - zeros).abs() < f32::EPSILON, "{ones} vs {zeros}");
        assert!(
            measure("1111", 40.0, Weight::Bold, false) < measure("0000", 40.0, Weight::Bold, false)
        );
    }

    #[test]
    fn fit_adds_an_ellipsis_and_stays_inside() {
        let call = "KX0AAA-KX0BBB-KX0CCC-KX0DDD-KX0EEE-KX0FFF";
        assert_eq!(call.chars().count(), 41);
        let shown = fit(call, 300.0, 30.0, Weight::Bold);
        assert!(shown.ends_with('…'), "{shown}");
        assert!(measure(&shown, 30.0, Weight::Bold, false) <= 300.0);
        assert!(matches!(
            fit("N0CALL", 300.0, 30.0, Weight::Bold),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn fit_caps_hostile_input_before_measuring() {
        // 100 000 characters: an uncapped, quadratic fit would take minutes.
        let long = "W".repeat(100_000);
        let start = std::time::Instant::now();
        let shown = fit(&long, 300.0, 30.0, Weight::Bold);
        assert!(shown.ends_with('…'), "{shown}");
        assert!(shown.chars().count() <= crate::scene::MAX_TEXT_CHARS + 1);
        let wide = fit(&long, 1.0e9, 30.0, Weight::Bold);
        assert_eq!(wide.chars().count(), crate::scene::MAX_TEXT_CHARS);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(fit("A\u{7}B\nC", 1000.0, 20.0, Weight::Regular), "ABC");
    }

    #[test]
    fn unknown_glyphs_draw_without_panicking() {
        let mut p = Pixmap::new(200, 50).unwrap();
        draw_text(
            &mut p,
            "网 🙂 Ωmega",
            4.0,
            34.0,
            28.0,
            Weight::Regular,
            Color::WHITE,
            false,
        );
    }

    #[test]
    fn text_lands_inside_its_box() {
        let mut p = Pixmap::new(200, 60).unwrap();
        p.fill(Color::BLACK);
        draw_text(
            &mut p,
            "N0CALL",
            10.0,
            40.0,
            30.0,
            Weight::Bold,
            Color::WHITE,
            false,
        );
        let mut inside = false;
        for y in 0..60u32 {
            for x in 0..200u32 {
                let px = p.pixel(x, y).unwrap();
                if px.red() > 200 {
                    assert!(x < 140, "bright pixel outside the box at x={x}");
                    if (10..120).contains(&x) && (10..45).contains(&y) {
                        inside = true;
                    }
                }
            }
        }
        assert!(inside, "no bright pixel inside the box");
    }
}
