// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Draws frame A of `video-frames.html` from a [`FrameState`].
//!
//! The mockup is laid out in CSS pixels on a 1200×675 frame; every number
//! here is one of those pixels, multiplied by `width / 1200` when it is
//! drawn. The comments name the CSS rule each value comes from.
//!
//! Drawing a 1080p frame from nothing is too slow to do 30 times a second,
//! and almost nothing changes from one frame to the next, so the renderer
//! keeps three layers:
//!
//! 1. the **background**: the blurred colour washes, the talker panel's
//!    glass, the top bar (but not its clock) and the title. Drawn once.
//! 2. the **composite**: the background with the conversation column on it.
//!    The column is redrawn only when its bubbles or its scroll change.
//! 3. the **frame**: the composite with the orb, the talker, the clock and
//!    the footer drawn over it. Only the regions those occupy are refreshed
//!    from the composite each time.
//!
//! A state equal to the previous one returns the previous frame untouched.
//!
//! CSS effects tiny-skia does not have are worked out by hand: Gaussian blur
//! (three box blurs) for the washes, shadows and glows; `inset` box shadows;
//! the column's fade-out mask. Backdrop blur is left out: what sits behind
//! the glass is already a soft blur, so blurring it again changes nothing
//! visible.

// Pixel arithmetic: every cast is on a value bounded by the pixmap or by 0..=255.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]

use std::borrow::Cow;

use astar_netarchive::PEAK_BINS;
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap,
    PixmapPaint, Point, RadialGradient, Rect, Shader, SpreadMode, Stroke, Transform,
};

use crate::font::{self, Weight};
use crate::scene::{BUBBLE_PITCH_PX, Bubble, FrameState, UNKNOWN_STATION};

/// An output size. Frame A is 16:9, and [`Renderer::new`] refuses any
/// other shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const P1080: Size = Size {
        width: 1920,
        height: 1080,
    };
    pub const P720: Size = Size {
        width: 1280,
        height: 720,
    };
}

/// How many frames were drawn and how many were the previous one again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub drawn: u64,
    pub reused: u64,
}

// ── the mockup's numbers, in CSS px ─────────────────────────────────────────

/// The mockup frame's width.
const FRAME_W: f32 = 1200.0;
/// `.in { padding: 34px 40px }`.
const PAD_X: f32 = 40.0;
const PAD_Y: f32 = 34.0;
/// The top row is as tall as the LIVE badge: 13 px × 1.5 + 2 × 5 padding +
/// 2 × 1 border.
const TOP_H: f32 = 31.5;
/// `.title { font-size: 28px; margin-top: 6px }`, line height 1.5.
const TITLE_PX: f32 = 28.0;
const TITLE_TOP: f32 = PAD_Y + TOP_H + 6.0;
/// `.body { margin-top: 22px }`.
const BODY_TOP: f32 = TITLE_TOP + TITLE_PX * 1.5 + 22.0;
/// `.foot { margin-top: 18px; font-size: 14px }`, line height 1.5.
const FOOT_PX: f32 = 14.0;
const FOOT_TOP: f32 = 675.0 - PAD_Y - FOOT_PX * 1.5;
const BODY_BOTTOM: f32 = FOOT_TOP - 18.0;
/// `.now { width: 420px }`, then `.body { gap: 36px }`.
const NOW_W: f32 = 420.0;
const COL_LEFT: f32 = PAD_X + NOW_W + 36.0;
const COL_RIGHT: f32 = FRAME_W - PAD_X;
/// `.glass { border-radius: 20px }`.
const GLASS_R: f32 = 20.0;
/// `.col { padding: 20px 22px }`.
const COL_PAD_Y: f32 = 20.0;
const COL_PAD_X: f32 = 22.0;
/// The talker panel's stack: orb 200, gap 22, talker 52, gap 6, sub 24.
const STACK_H: f32 = 200.0 + 22.0 + 52.0 + 6.0 + 24.0;
const TALKER_PX: f32 = 52.0;
const SUB_PX: f32 = 16.0;
/// `.now { padding: 26px }`: the room text has.
const NOW_TEXT_W: f32 = NOW_W - 2.0 * 26.0;
/// `.b { max-width: 72%; min-width: 250px }`.
const BUBBLE_MIN_W: f32 = 250.0;
const BUBBLE_MAX_FRAC: f32 = 0.72;
/// `.b { padding: 11px 15px 10px }`.
const BUBBLE_PAD_T: f32 = 11.0;
const BUBBLE_PAD_X: f32 = 15.0;
const BUBBLE_PAD_B: f32 = 10.0;
/// `.call` 17 px, `.wave` 28 px with `margin: 6px 0 4px`, `.meta` 13 px.
const CALL_PX: f32 = 17.0;
const WAVE_H: f32 = 28.0;
const META_PX: f32 = 13.0;
/// `.row { margin-bottom: 12px }`.
const ROW_GAP: f32 = 12.0;
/// The bubble's height inside its border.
const BUBBLE_INNER_H: f32 =
    BUBBLE_PAD_T + CALL_PX * 1.5 + 6.0 + WAVE_H + 4.0 + META_PX * 1.5 + BUBBLE_PAD_B;

// ── colours ─────────────────────────────────────────────────────────────────

fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color::from_rgba8(r, g, b, (a * 255.0).round() as u8)
}

/// `--text`.
fn text_c() -> Color {
    rgba(255, 255, 255, 0.92)
}

/// `--text-dim`.
fn dim_c() -> Color {
    rgba(255, 255, 255, 0.45)
}

fn hex(v: u32) -> Color {
    Color::from_rgba8((v >> 16) as u8, (v >> 8) as u8, v as u8, 255)
}

fn with_alpha(c: Color, a: f32) -> Color {
    let mut c = c;
    c.apply_opacity(a);
    c
}

// ── geometry ────────────────────────────────────────────────────────────────

/// Frame A's boxes at one output size.
struct Geometry {
    /// Output px per mockup px.
    k: f32,
    /// The top row's vertical centre.
    top_c: f32,
    /// The talker panel and the conversation column.
    now: Rect,
    col: Rect,
    /// The orb's centre.
    orb: (f32, f32),
}

impl Geometry {
    fn new(size: Size) -> Self {
        let k = size.width as f32 / FRAME_W;
        let rect = |l: f32, t: f32, r: f32, b: f32| {
            Rect::from_ltrb(l * k, t * k, r * k, b * k).expect("frame A's boxes are valid")
        };
        let stack_top = BODY_TOP + (BODY_BOTTOM - BODY_TOP - STACK_H) / 2.0;
        Self {
            k,
            top_c: (PAD_Y + TOP_H / 2.0) * k,
            now: rect(PAD_X, BODY_TOP, PAD_X + NOW_W, BODY_BOTTOM),
            col: rect(COL_LEFT, BODY_TOP, COL_RIGHT, BODY_BOTTOM),
            orb: ((PAD_X + NOW_W / 2.0) * k, (stack_top + 100.0) * k),
        }
    }

    fn s(&self, v: f32) -> f32 {
        v * self.k
    }

    /// The column's content box: inside its 1 px border and padding.
    fn col_inner_left(&self) -> f32 {
        self.col.left() + self.s(1.0 + COL_PAD_X)
    }

    fn col_inner_right(&self) -> f32 {
        self.col.right() - self.s(1.0 + COL_PAD_X)
    }

    fn col_inner_top(&self) -> f32 {
        self.col.top() + self.s(1.0 + COL_PAD_Y)
    }

    fn col_inner_bottom(&self) -> f32 {
        self.col.bottom() - self.s(1.0 + COL_PAD_Y)
    }

    fn bubble_max_w(&self) -> f32 {
        (self.col_inner_right() - self.col_inner_left()) * BUBBLE_MAX_FRAC
    }

    /// A bubble's height and the row pitch below it. Mine have no border.
    fn bubble_h(&self, mine: bool) -> f32 {
        self.s(BUBBLE_INNER_H + if mine { 0.0 } else { 2.0 })
    }
}

/// The baseline that centres a line of `px` text in a line box, as CSS does.
fn baseline(top: f32, line: f32, px: f32, w: Weight) -> f32 {
    let (a, d) = font::v_metrics(w);
    top + (line - (a + d) * px) / 2.0 + a * px
}

/// The baseline of `px` text centred on `centre`.
fn baseline_at(centre: f32, px: f32, w: Weight) -> f32 {
    baseline(centre - px, 2.0 * px, px, w)
}

// ── paths ───────────────────────────────────────────────────────────────────

/// Cubic Bézier handle length for a quarter circle.
const KAPPA: f32 = 0.552_284_8;

/// A rectangle with its corners rounded by `r` = [top-left, top-right,
/// bottom-right, bottom-left].
fn push_rrect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: [f32; 4]) {
    let lim = (w / 2.0).min(h / 2.0).max(0.0);
    let [tl, tr, br, bl] = r.map(|v| v.clamp(0.0, lim));
    let (x1, y1) = (x + w, y + h);
    pb.move_to(x + tl, y);
    pb.line_to(x1 - tr, y);
    pb.cubic_to(
        x1 - tr * (1.0 - KAPPA),
        y,
        x1,
        y + tr * (1.0 - KAPPA),
        x1,
        y + tr,
    );
    pb.line_to(x1, y1 - br);
    pb.cubic_to(
        x1,
        y1 - br * (1.0 - KAPPA),
        x1 - br * (1.0 - KAPPA),
        y1,
        x1 - br,
        y1,
    );
    pb.line_to(x + bl, y1);
    pb.cubic_to(
        x + bl * (1.0 - KAPPA),
        y1,
        x,
        y1 - bl * (1.0 - KAPPA),
        x,
        y1 - bl,
    );
    pb.line_to(x, y + tl);
    pb.cubic_to(
        x,
        y + tl * (1.0 - KAPPA),
        x + tl * (1.0 - KAPPA),
        y,
        x + tl,
        y,
    );
    pb.close();
}

fn rrect(x: f32, y: f32, w: f32, h: f32, r: [f32; 4]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    push_rrect(&mut pb, x, y, w, h, r);
    pb.finish()
}

/// `rrect` minus the same box inset by `t`: a border `t` wide.
fn ring_rrect(x: f32, y: f32, w: f32, h: f32, r: [f32; 4], t: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    push_rrect(&mut pb, x, y, w, h, r);
    push_rrect(
        &mut pb,
        x + t,
        y + t,
        w - 2.0 * t,
        h - 2.0 * t,
        r.map(|v| (v - t).max(0.0)),
    );
    pb.finish()
}

/// An arc of radius `r` from angle `start`, `sweep` radians clockwise on
/// screen (y grows down).
fn arc(cx: f32, cy: f32, r: f32, start: f32, sweep: f32) -> Option<Path> {
    let n = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / n as f32;
    let h = 4.0 / 3.0 * (step / 4.0).tan() * r;
    let mut pb = PathBuilder::new();
    let at = |a: f32| (cx + r * a.cos(), cy + r * a.sin());
    let (x0, y0) = at(start);
    pb.move_to(x0, y0);
    for i in 0..n {
        let a0 = start + step * i as f32;
        let a1 = a0 + step;
        let (p0x, p0y) = at(a0);
        let (p1x, p1y) = at(a1);
        pb.cubic_to(
            p0x - h * a0.sin(),
            p0y + h * a0.cos(),
            p1x + h * a1.sin(),
            p1y - h * a1.cos(),
            p1x,
            p1y,
        );
    }
    pb.finish()
}

fn solid(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

fn shaded(shader: Shader<'static>) -> Paint<'static> {
    Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    }
}

fn stops(colors: &[(f32, Color)]) -> Vec<GradientStop> {
    colors
        .iter()
        .map(|&(at, c)| GradientStop::new(at, c))
        .collect()
}

// ── pixels ──────────────────────────────────────────────────────────────────

/// `color` over premultiplied pixel `i` with coverage `a` (0..=1).
#[inline]
fn blend(data: &mut [u8], i: usize, c: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let inv = 1.0 - a;
    for k in 0..3 {
        data[i + k] = (c[k] * a + f32::from(data[i + k]) * inv).round().min(255.0) as u8;
    }
    data[i + 3] = (255.0 * a + f32::from(data[i + 3]) * inv)
        .round()
        .min(255.0) as u8;
}

fn rgb(c: Color) -> [f32; 3] {
    let u = c.to_color_u8();
    [
        f32::from(u.red()),
        f32::from(u.green()),
        f32::from(u.blue()),
    ]
}

/// One box blur of radius `r` along rows of `buf` (`w` wide), edges read as 0.
fn box_rows(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut dst[y * w..(y + 1) * w];
        let mut sum: f32 = row[..r.min(w)].iter().sum();
        for x in 0..w {
            if x + r < w {
                sum += row[x + r];
            }
            out[x] = sum * norm;
            if x >= r {
                sum -= row[x - r];
            }
        }
    }
}

fn transpose(src: &[f32], dst: &mut [f32], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            dst[x * h + y] = src[y * w + x];
        }
    }
}

/// An approximate Gaussian blur of standard deviation `sigma` (in cells):
/// three box blurs each way.
fn blur(buf: &mut [f32], w: usize, h: usize, sigma: f32) {
    let ideal = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    let r = ((ideal - 1.0) / 2.0).round().max(0.0) as usize;
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0.0; buf.len()];
    for _ in 0..3 {
        box_rows(buf, &mut tmp, w, h, r);
        buf.copy_from_slice(&tmp);
    }
    transpose(buf, &mut tmp, w, h);
    let mut t2 = vec![0.0; buf.len()];
    for _ in 0..3 {
        box_rows(&tmp, &mut t2, h, w, r);
        tmp.copy_from_slice(&t2);
    }
    transpose(&tmp, buf, h, w);
}

/// A path's coverage over a `w`×`h` window whose top-left is (`x0`, `y0`).
fn coverage(path: &Path, x0: i32, y0: i32, w: u32, h: u32, dx: f32, dy: f32) -> Vec<f32> {
    let Some(mut m) = Mask::new(w, h) else {
        return Vec::new();
    };
    m.fill_path(
        path,
        FillRule::Winding,
        true,
        Transform::from_translate(dx - x0 as f32, dy - y0 as f32),
    );
    m.data().iter().map(|&v| f32::from(v) / 255.0).collect()
}

/// What a shadow does to the shape that casts it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cast {
    /// `box-shadow`: drawn only outside the shape.
    Outside,
    /// `filter: drop-shadow`: drawn everywhere (the shape covers it).
    Under,
    /// `box-shadow: inset`: the outside of the shape, offset and blurred,
    /// drawn only inside it.
    Inset,
}

/// A blurred shadow of `shape`, offset by (`dx`, `dy`), in `color`.
fn shadow(p: &mut Pixmap, shape: &Path, dx: f32, dy: f32, sigma: f32, color: Color, cast: Cast) {
    let b = shape.bounds();
    let m = (3.0 * sigma).ceil() + 2.0;
    let x0 = ((b.left() + dx.min(0.0) - m).floor() as i32).max(0);
    let y0 = ((b.top() + dy.min(0.0) - m).floor() as i32).max(0);
    let x1 = ((b.right() + dx.max(0.0) + m).ceil() as i32).min(p.width() as i32);
    let y1 = ((b.bottom() + dy.max(0.0) + m).ceil() as i32).min(p.height() as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let mut cast_by = coverage(shape, x0, y0, w, h, dx, dy);
    if cast == Cast::Inset {
        for v in &mut cast_by {
            *v = 1.0 - *v;
        }
    }
    blur(&mut cast_by, w as usize, h as usize, sigma);
    let own = match cast {
        Cast::Under => None,
        Cast::Outside | Cast::Inset => Some(coverage(shape, x0, y0, w, h, 0.0, 0.0)),
    };
    let (c, ca) = (rgb(color), color.alpha());
    let pw = p.width() as usize;
    let data = p.data_mut();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let j = y * w as usize + x;
            let keep = match (&own, cast) {
                (Some(o), Cast::Outside) => 1.0 - o[j],
                (Some(o), Cast::Inset) => o[j],
                _ => 1.0,
            };
            let a = cast_by[j] * ca * keep;
            let i = ((y0 as usize + y) * pw + x0 as usize + x) * 4;
            blend(data, i, c, a);
        }
    }
}

/// Copy `r` (clamped to the frame) from `src` into `dst`, which share a size.
fn copy_rect(dst: &mut Pixmap, src: &Pixmap, r: Rect) {
    let w = src.width() as i32;
    let h = src.height() as i32;
    let x0 = (r.left().floor() as i32).clamp(0, w) as usize;
    let x1 = (r.right().ceil() as i32).clamp(0, w) as usize;
    let y0 = (r.top().floor() as i32).clamp(0, h) as usize;
    let y1 = (r.bottom().ceil() as i32).clamp(0, h) as usize;
    let stride = w as usize * 4;
    let (s, d) = (src.data(), dst.data_mut());
    for y in y0..y1 {
        let a = y * stride + x0 * 4;
        let b = y * stride + x1 * 4;
        d[a..b].copy_from_slice(&s[a..b]);
    }
}

/// An ordered (4×4 Bayer) dither in −0.5..0.5, so the dark washes do not
/// band. Ordered rather than random, so the golden PNGs stay small.
fn dither(x: u32, y: u32) -> f32 {
    const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    (f32::from(BAYER[(y % 4) as usize][(x % 4) as usize]) + 0.5) / 16.0 - 0.5
}

// ── text ────────────────────────────────────────────────────────────────────

/// Width of `text` with `spacing` between letters (none after the last).
fn spaced_w(text: &str, px: f32, w: Weight, spacing: f32, tabular: bool) -> f32 {
    let n = text.chars().filter(|c| !c.is_control()).count();
    font::measure(text, px, w, tabular) + spacing * n.saturating_sub(1) as f32
}

/// `text` cut with `…` to fit `max_w`, measured with letter spacing. A
/// binary search, so a 256-character callsign costs a handful of measures.
/// No shaping or bidi, as in [`font::fit`].
/// Control characters, and characters Inter cannot draw (emoji), are left
/// out rather than drawn as empty boxes.
fn fit(text: &str, max_w: f32, px: f32, w: Weight, spacing: f32, tabular: bool) -> Cow<'_, str> {
    let keep = |c: &char| !c.is_control() && (*c == ' ' || font::has_glyph(*c, w));
    let clean: Cow<'_, str> = if text.chars().all(|c| keep(&c)) {
        Cow::Borrowed(text)
    } else {
        let kept: String = text.chars().filter(keep).collect();
        Cow::Owned(kept.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    if spaced_w(&clean, px, w, spacing, tabular) <= max_w {
        return clean;
    }
    let chars: Vec<char> = clean.chars().collect();
    let cut = |n: usize| {
        let mut s: String = chars[..n].iter().collect::<String>().trim_end().to_owned();
        s.push('…');
        s
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if spaced_w(&cut(mid), px, w, spacing, tabular) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 && spaced_w("…", px, w, spacing, tabular) > max_w {
        return Cow::Borrowed("");
    }
    Cow::Owned(cut(lo))
}

/// Draw `text` from `x` with `spacing` between letters.
fn draw_spaced(
    p: &mut Pixmap,
    text: &str,
    x: f32,
    base: f32,
    px: f32,
    w: Weight,
    color: Color,
    spacing: f32,
    tabular: bool,
) {
    if spacing == 0.0 {
        font::draw_text(p, text, x, base, px, w, color, tabular);
        return;
    }
    // Each letter at the kerned width of what precedes it, plus spacing.
    for (n, (i, c)) in text
        .char_indices()
        .filter(|(_, c)| !c.is_control())
        .enumerate()
    {
        let pen = font::measure(&text[..i], px, w, tabular) + spacing * n as f32;
        let mut buf = [0u8; 4];
        font::draw_text(
            p,
            c.encode_utf8(&mut buf),
            x + pen,
            base,
            px,
            w,
            color,
            tabular,
        );
    }
}

// ── the renderer ────────────────────────────────────────────────────────────

/// What the background was drawn from.
#[derive(Clone, PartialEq, Eq)]
struct BgKey {
    live: bool,
    brand: &'static str,
    chip: String,
    target: String,
    title: String,
}

impl BgKey {
    fn of(s: &FrameState) -> Self {
        Self {
            live: s.live,
            brand: s.brand,
            chip: s.chip.clone(),
            target: s.target.clone(),
            title: s.title.clone(),
        }
    }
}

/// The orb's unchanging parts (halo, track, core), drawn once.
struct OrbBase {
    pix: Pixmap,
    /// Where `pix`'s top-left goes in the frame.
    at: (i32, i32),
}

/// Draws frame A, keeping what it can between frames.
pub struct Renderer {
    size: Size,
    g: Geometry,
    background: Pixmap,
    composite: Pixmap,
    frame: Pixmap,
    /// The column's box in whole pixels: its top-left and size. Everything
    /// `col_*` is in these coordinates.
    col_at: (i32, i32),
    col_size: (u32, u32),
    /// The column's glass, drawn once.
    col_glass: Pixmap,
    /// The column's padding box: bubbles are clipped to it.
    col_clip: Mask,
    /// `.col`'s fade-out mask, one value per row.
    col_fade: Vec<f32>,
    /// The glass over the background (`col_over`), and that faded by the
    /// mask (`col_base`): the column with no bubbles. Premultiplied RGBA.
    col_over: Vec<u8>,
    col_base: Vec<u8>,
    /// The bubbles drawn for the last column, reused while they stay the same.
    sprites: Vec<Sprite>,
    orb: OrbBase,
    /// The ring layer for one ring value, in `orb`'s coordinates.
    ring: Option<(u16, Pixmap)>,
    /// The outer glow of a playing bubble: (key, pixmap, offset from the
    /// bubble's integer top-left).
    glow: Option<(GlowKey, Pixmap, (i32, i32))>,
    bg_key: Option<BgKey>,
    /// Right edge of the target text, where the clock's region starts.
    clock_left: f32,
    col_key: Option<(Vec<Bubble>, u16)>,
    /// The column changed since the frame last took it.
    col_dirty: bool,
    /// The frame needs a full copy of the composite.
    frame_stale: bool,
    last: Option<FrameState>,
    stats: RenderStats,
    pub(crate) column_draws: u64,
}

/// What a bubble sprite was drawn from: the bubble and where it sits
/// within a pixel.
#[derive(Clone, PartialEq)]
struct SpriteKey {
    bubble: Bubble,
    h: u32,
    fx: u32,
    fy: u32,
}

/// One bubble (with its glow), drawn alone.
struct Sprite {
    key: SpriteKey,
    pix: Pixmap,
    /// Margin around the bubble, in pixels.
    margin: i32,
}

#[derive(Clone, Copy, PartialEq)]
struct GlowKey {
    w: f32,
    h: f32,
    mine: bool,
    fx: f32,
    fy: f32,
}

fn pixmap(w: u32, h: u32) -> Pixmap {
    Pixmap::new(w.max(1), h.max(1)).expect("frame-sized pixmaps fit in memory")
}

impl Renderer {
    /// A renderer for `size`. It draws its first frame from nothing.
    ///
    /// # Panics
    /// When `size` has a zero side, is not 16:9 (to within a pixel of
    /// height, so 854×480 passes), or does not fit in memory. Frame A's
    /// layout is the mockup's 16:9 scaled by the width: a shorter frame
    /// would cut off its footer and a taller one would leave a band, so
    /// any other shape is refused here rather than drawn wrong.
    #[must_use]
    pub fn new(size: Size) -> Self {
        assert!(size.width > 0 && size.height > 0, "a frame needs pixels");
        let (w9, h16) = (u64::from(size.width) * 9, u64::from(size.height) * 16);
        assert!(
            w9.abs_diff(h16) < 16,
            "frame A is 16:9; {}×{} is not",
            size.width,
            size.height
        );
        let g = Geometry::new(size);
        let col_at = (g.col.left().floor() as i32, g.col.top().floor() as i32);
        let cw = (g.col.right().ceil() as i32 - col_at.0) as u32;
        let ch = (g.col.bottom().ceil() as i32 - col_at.1) as u32;
        let (glass, clip) = Self::column_glass(&g, col_at, cw, ch);
        let fade = (0..ch)
            .map(|y| {
                // `mask: linear-gradient(transparent, #000 18%)`.
                let gy = col_at.1 as f32 + y as f32 + 0.5 - g.col.top();
                (gy / (0.18 * g.col.height())).clamp(0.0, 1.0)
            })
            .collect();
        let orb = Self::orb_base(&g);
        Self {
            size,
            background: pixmap(size.width, size.height),
            composite: pixmap(size.width, size.height),
            frame: pixmap(size.width, size.height),
            col_at,
            col_size: (cw, ch),
            col_glass: glass,
            col_clip: clip,
            col_fade: fade,
            col_over: Vec::new(),
            col_base: Vec::new(),
            sprites: Vec::new(),
            orb,
            ring: None,
            glow: None,
            bg_key: None,
            clock_left: 0.0,
            col_key: None,
            col_dirty: true,
            frame_stale: true,
            last: None,
            stats: RenderStats::default(),
            column_draws: 0,
            g,
        }
    }

    /// Bubbles that can show in the column at once, counting one sliding in
    /// and one fading out under the top of the column. Give it to
    /// [`crate::SceneBuilder::set_capacity`].
    #[must_use]
    pub fn column_capacity(&self) -> usize {
        let g = &self.g;
        let pitch = g.bubble_h(true) + g.s(ROW_GAP);
        let room = g.col_inner_bottom() - g.col.top() + pitch;
        ((room + g.s(ROW_GAP)) / pitch).ceil() as usize
    }

    #[must_use]
    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// The frame for `s`: opaque RGBA, `width × height × 4` bytes. A state
    /// equal to the last one returns the same buffer, untouched.
    pub fn render(&mut self, s: &FrameState) -> &[u8] {
        if self.last.as_ref() == Some(s) {
            self.stats.reused += 1;
            return self.frame.data();
        }
        self.stats.drawn += 1;
        let key = BgKey::of(s);
        if self.bg_key.as_ref() != Some(&key) {
            self.draw_background(s);
            self.bg_key = Some(key);
            self.composite
                .data_mut()
                .copy_from_slice(self.background.data());
            self.col_key = None;
            self.frame_stale = true;
        }
        let col_key = (s.bubbles.clone(), s.scroll);
        if self.col_key.as_ref() != Some(&col_key) {
            self.draw_column(&s.bubbles, s.scroll);
            self.col_key = Some(col_key);
            self.col_dirty = true;
        }
        let g = &self.g;
        if self.frame_stale {
            self.frame.data_mut().copy_from_slice(self.composite.data());
            self.frame_stale = false;
            self.col_dirty = false;
        } else {
            if self.col_dirty {
                copy_rect(&mut self.frame, &self.composite, g.col);
                self.col_dirty = false;
            }
            for r in self.dynamic_rects() {
                copy_rect(&mut self.frame, &self.composite, r);
            }
        }
        self.draw_now_panel(s);
        self.draw_clock_and_footer(s);
        self.last = Some(s.clone());
        self.frame.data()
    }

    /// Everything drawn over the composite each frame lies inside these.
    fn dynamic_rects(&self) -> [Rect; 3] {
        let g = &self.g;
        let full = |l: f32, t: f32, r: f32, b: f32| Rect::from_ltrb(l, t, r, b).unwrap_or(g.now);
        [
            g.now,
            full(
                self.clock_left,
                g.s(PAD_Y) - 2.0,
                g.s(COL_RIGHT) + 2.0,
                g.s(PAD_Y + TOP_H) + 2.0,
            ),
            full(
                g.s(PAD_X) - 2.0,
                g.s(FOOT_TOP) - 2.0,
                g.s(COL_RIGHT) + 2.0,
                g.s(FOOT_TOP + FOOT_PX * 1.5) + 4.0,
            ),
        ]
    }

    // ── layer 1: the background ─────────────────────────────────────────────

    fn draw_background(&mut self, s: &FrameState) {
        let (w, h) = (self.size.width, self.size.height);
        washes(&mut self.background, self.g.k);
        let g = &self.g;
        let p = &mut self.background;
        glass(p, g.now, g.s(GLASS_R), g.k, true);

        // The top row: `.brand`, `.chip`, `.subt`, then the LIVE badge or
        // (drawn per frame) the clock.
        let c = g.top_c;
        let mut x = g.s(PAD_X);
        let brand_px = g.s(14.0);
        let brand_sp = brand_px * 0.04;
        draw_spaced(
            p,
            s.brand,
            x,
            baseline_at(c, brand_px, Weight::ExtraBold),
            brand_px,
            Weight::ExtraBold,
            dim_c(),
            brand_sp,
            false,
        );
        x += spaced_w(s.brand, brand_px, Weight::ExtraBold, brand_sp, false) + brand_sp + g.s(14.0);

        let chip_px = g.s(13.0);
        let chip = fit(&s.chip, g.s(200.0), chip_px, Weight::SemiBold, 0.0, false);
        let chip_w = font::measure(&chip, chip_px, Weight::SemiBold, false) + g.s(20.0);
        let chip_h = g.s(13.0 * 1.5 + 6.0);
        if let Some(path) = rrect(x, c - chip_h / 2.0, chip_w, chip_h, [chip_h / 2.0; 4]) {
            p.fill_path(
                &path,
                &solid(rgba(48, 209, 88, 0.18)),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        font::draw_text(
            p,
            &chip,
            x + g.s(10.0),
            baseline_at(c, chip_px, Weight::SemiBold),
            chip_px,
            Weight::SemiBold,
            hex(0x30_d1_58),
            false,
        );
        x += chip_w + g.s(14.0);

        let right = if s.live {
            live_badge(p, g)
        } else {
            g.s(COL_RIGHT) - font::measure("00:00:00", g.s(FOOT_PX), Weight::Regular, true)
        } - g.s(14.0);
        self.clock_left = right;
        let target_px = g.s(15.0);
        let target = fit(&s.target, right - x, target_px, Weight::Regular, 0.0, false);
        font::draw_text(
            p,
            &target,
            x,
            baseline_at(c, target_px, Weight::Regular),
            target_px,
            Weight::Regular,
            dim_c(),
            false,
        );

        let title_px = g.s(TITLE_PX);
        let title = fit(
            &s.title,
            g.s(FRAME_W - 2.0 * PAD_X),
            title_px,
            Weight::Bold,
            0.0,
            false,
        );
        font::draw_text(
            p,
            &title,
            g.s(PAD_X),
            baseline(g.s(TITLE_TOP), title_px * 1.5, title_px, Weight::Bold),
            title_px,
            Weight::Bold,
            text_c(),
            false,
        );
        debug_assert_eq!((p.width(), p.height()), (w, h));
        self.column_base();
    }

    /// `col_over` and `col_base` from the background and the glass. The
    /// mask fades the whole column, so a faded pixel is the background
    /// mixed toward the column drawn over it.
    fn column_base(&mut self) {
        let (cw, ch) = (self.col_size.0 as usize, self.col_size.1 as usize);
        let fw = self.background.width() as i32;
        let fh = self.background.height() as i32;
        let bg = self.background.data();
        let glass = self.col_glass.data();
        self.col_over = vec![0; cw * ch * 4];
        self.col_base = vec![0; cw * ch * 4];
        for ly in 0..ch {
            let fy = self.col_at.1 + ly as i32;
            let f = self.col_fade[ly];
            for lx in 0..cw {
                let fx = self.col_at.0 + lx as i32;
                if fx < 0 || fy < 0 || fx >= fw || fy >= fh {
                    continue;
                }
                let bi = (fy as usize * fw as usize + fx as usize) * 4;
                let li = (ly * cw + lx) * 4;
                let ga = f32::from(glass[li + 3]) / 255.0;
                for k in 0..4 {
                    let b = f32::from(bg[bi + k]);
                    let over = f32::from(glass[li + k]) + b * (1.0 - ga);
                    self.col_over[li + k] = over.round().min(255.0) as u8;
                    self.col_base[li + k] = (b + (over - b) * f).round().min(255.0) as u8;
                }
            }
        }
    }

    // ── layer 2: the conversation column ────────────────────────────────────

    /// `.glass.col`, in a column-sized pixmap at `at`, and the padding box
    /// as a clip mask.
    fn column_glass(g: &Geometry, at: (i32, i32), w: u32, h: u32) -> (Pixmap, Mask) {
        let mut p = pixmap(w, h);
        let local = Rect::from_xywh(
            g.col.left() - at.0 as f32,
            g.col.top() - at.1 as f32,
            g.col.width(),
            g.col.height(),
        )
        .expect("the column has a size");
        // The column's own shadow falls outside its fade mask's box, so
        // CSS clips it away; it is not drawn.
        glass(&mut p, local, g.s(GLASS_R), g.k, false);
        let mut clip = Mask::new(w.max(1), h.max(1)).expect("column mask");
        let b = g.s(1.0);
        if let Some(path) = rrect(
            local.left() + b,
            local.top() + b,
            local.width() - 2.0 * b,
            local.height() - 2.0 * b,
            [g.s(GLASS_R) - b; 4],
        ) {
            clip.fill_path(&path, FillRule::Winding, true, Transform::identity());
        }
        (p, clip)
    }

    fn draw_column(&mut self, bubbles: &[Bubble], scroll: u16) {
        self.column_draws += 1;
        // Start from the column with no bubbles.
        let (cw, ch) = (self.col_size.0 as usize, self.col_size.1 as usize);
        let fw = self.composite.width() as usize;
        {
            let comp = self.composite.data_mut();
            for ly in 0..ch {
                let fy = self.col_at.1 as usize + ly;
                let at = (fy * fw + self.col_at.0 as usize) * 4;
                comp[at..at + cw * 4]
                    .copy_from_slice(&self.col_base[ly * cw * 4..(ly + 1) * cw * 4]);
            }
        }

        // Bubbles stack down from the top of the content box until they
        // overflow it; then the newest sits at the bottom and older ones
        // run up under the fade. A new bubble pushes the stack up by its
        // pitch, easing out as `scroll` falls to 0, and fades in as it does.
        let g = &self.g;
        let heights: Vec<f32> = bubbles.iter().map(|b| g.bubble_h(b.mine)).collect();
        let total: f32 =
            heights.iter().sum::<f32>() + g.s(ROW_GAP) * heights.len().saturating_sub(1) as f32;
        let room = g.col_inner_bottom() - g.col_inner_top();
        let over = (total - room).max(0.0);
        let frac = f32::from(scroll) / f32::from(BUBBLE_PITCH_PX);
        let pitch = heights.last().map_or(0.0, |h| h + g.s(ROW_GAP));
        let shift = (frac * pitch).min(over);
        let mut y = g.col_inner_top() - over + shift;
        let n = bubbles.len();
        let mut old = std::mem::take(&mut self.sprites);
        let mut kept = Vec::with_capacity(n);
        for (i, (b, &h)) in bubbles.iter().zip(&heights).enumerate() {
            let top = y;
            y += h + g.s(ROW_GAP);
            if top + h <= g.col.top() || top >= g.col.bottom() {
                continue;
            }
            let text = bubble_text(g, b);
            let x = if b.mine {
                g.col_inner_right() - text.w
            } else {
                g.col_inner_left()
            };
            let key = SpriteKey {
                bubble: b.clone(),
                h: h.to_bits(),
                fx: (x - x.floor()).to_bits(),
                fy: (top - top.floor()).to_bits(),
            };
            let sprite = if let Some(j) = old.iter().position(|s| s.key == key) {
                old.swap_remove(j)
            } else {
                let margin = if b.on { g.s(50.0).ceil() as i32 } else { 2 };
                let mut pix = pixmap(
                    text.w.ceil() as u32 + 2 * margin as u32 + 2,
                    h.ceil() as u32 + 2 * margin as u32 + 2,
                );
                let (lx, ly) = (
                    x - x.floor() + margin as f32,
                    top - top.floor() + margin as f32,
                );
                draw_bubble(&mut pix, g, b, &text, lx, ly, h, &mut self.glow);
                Sprite { key, pix, margin }
            };
            let opacity = if i + 1 == n && scroll > 0 {
                (1.0 - frac).clamp(0.0, 1.0)
            } else {
                1.0
            };
            blit(
                &mut self.composite,
                &ColumnView {
                    at: self.col_at,
                    size: self.col_size,
                    clip: &self.col_clip,
                    fade: &self.col_fade,
                    over: &self.col_over,
                },
                &sprite.pix,
                x.floor() as i32 - sprite.margin,
                top.floor() as i32 - sprite.margin,
                opacity,
            );
            kept.push(sprite);
        }
        self.sprites = kept;
    }

    // ── layer 3: the talker panel, the clock and the footer ────────────────

    /// `.orb`'s halo, track and core, around the orb's centre.
    fn orb_base(g: &Geometry) -> OrbBase {
        let side = g.s(240.0).ceil() as u32;
        let at = (
            (g.orb.0 - side as f32 / 2.0).floor() as i32,
            (g.orb.1 - side as f32 / 2.0).floor() as i32,
        );
        let (cx, cy) = (g.orb.0 - at.0 as f32, g.orb.1 - at.1 as f32);
        let mut p = pixmap(side, side);
        let v = (176, 74, 232);

        // `.halo`: inset −18 px, `radial-gradient(circle, v .5, transparent
        // 68%)`, 68 % of the box's corner distance.
        let halo_r = g.s(118.0 * std::f32::consts::SQRT_2 * 0.68);
        if let (Some(sh), Some(path)) = (
            RadialGradient::new(
                Point::from_xy(cx, cy),
                Point::from_xy(cx, cy),
                halo_r,
                stops(&[
                    (0.0, rgba(v.0, v.1, v.2, 0.5)),
                    (1.0, rgba(v.0, v.1, v.2, 0.0)),
                ]),
                SpreadMode::Pad,
                Transform::identity(),
            ),
            PathBuilder::from_circle(cx, cy, halo_r),
        ) {
            p.fill_path(
                &path,
                &shaded(sh),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        // `.track`: r 95, stroke 7.
        if let Some(path) = PathBuilder::from_circle(cx, cy, g.s(95.0)) {
            let stroke = Stroke {
                width: g.s(7.0),
                ..Stroke::default()
            };
            p.stroke_path(
                &path,
                &solid(rgba(255, 255, 255, 0.08)),
                &stroke,
                Transform::identity(),
                None,
            );
        }

        // `.core`: inset 20 px, `radial-gradient(circle at 35% 28%, v1 0%,
        // v2 40%, v3 100%)`, sized to the farthest corner of its box.
        let r = g.s(80.0);
        let (fx, fy) = (cx - r + 0.35 * 2.0 * r, cy - r + 0.28 * 2.0 * r);
        let far = ((cx + r - fx).powi(2) + (cy + r - fy).powi(2)).sqrt();
        let Some(core) = PathBuilder::from_circle(cx, cy, r) else {
            return OrbBase { pix: p, at };
        };
        if let Some(sh) = RadialGradient::new(
            Point::from_xy(fx, fy),
            Point::from_xy(fx, fy),
            far,
            stops(&[
                (0.0, hex(0xef_b0_fd)),
                (0.4, hex(0xb0_4a_e8)),
                (1.0, hex(0x6e_1a_9e)),
            ]),
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            p.fill_path(
                &core,
                &shaded(sh),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        // `box-shadow: inset 0 -14px 26px rgba(60,0,80,.55)`.
        shadow(
            &mut p,
            &core,
            0.0,
            g.s(-14.0),
            g.s(13.0),
            rgba(60, 0, 80, 0.55),
            Cast::Inset,
        );
        // `inset 0 2px 0 rgba(255,255,255,.6)`: a crisp crescent at the top.
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, r);
        pb.push_circle(cx, cy + g.s(2.0), r);
        let mut clip = Mask::new(side, side).expect("orb mask");
        clip.fill_path(&core, FillRule::Winding, true, Transform::identity());
        if let Some(path) = pb.finish() {
            p.fill_path(
                &path,
                &solid(rgba(255, 255, 255, 0.6)),
                FillRule::EvenOdd,
                Transform::identity(),
                Some(&clip),
            );
        }
        OrbBase { pix: p, at }
    }

    /// The progress ring for `ring` thousandths, with its glow, in the orb
    /// base's coordinates.
    fn ring_layer(&self, ring: u16) -> Pixmap {
        let g = &self.g;
        let mut p = pixmap(self.orb.pix.width(), self.orb.pix.height());
        if ring == 0 {
            return p;
        }
        let (cx, cy) = (
            g.orb.0 - self.orb.at.0 as f32,
            g.orb.1 - self.orb.at.1 as f32,
        );
        let r = g.s(95.0);
        let sweep = std::f32::consts::TAU * f32::from(ring.min(1000)) / 1000.0;
        let Some(path) = arc(cx, cy, r, -std::f32::consts::FRAC_PI_2, sweep) else {
            return p;
        };
        let stroke = Stroke {
            width: g.s(7.0),
            line_cap: LineCap::Round,
            ..Stroke::default()
        };
        // `filter: drop-shadow(0 0 8px rgba(176,74,232,.95))`: the stroke's
        // own shape, blurred, under it.
        if let Some(outline) = path.stroke(&stroke, 1.0) {
            shadow(
                &mut p,
                &outline,
                0.0,
                0.0,
                g.s(4.0),
                rgba(176, 74, 232, 0.95),
                Cast::Under,
            );
        }
        // `#vg` runs corner to corner of the circle's box, and the SVG is
        // turned −90°, so on screen it goes from bottom-left to top-right.
        if let Some(sh) = LinearGradient::new(
            Point::from_xy(cx - r, cy + r),
            Point::from_xy(cx + r, cy - r),
            stops(&[
                (0.0, hex(0xe8_79_f9)),
                (0.5, hex(0xb0_4a_e8)),
                (1.0, hex(0x7c_3a_ed)),
            ]),
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            p.stroke_path(&path, &shaded(sh), &stroke, Transform::identity(), None);
        }
        p
    }

    fn draw_now_panel(&mut self, s: &FrameState) {
        let dimmed = s.talking.is_none();
        let opacity = if dimmed { 0.45 } else { 1.0 };
        let paint = PixmapPaint {
            opacity,
            ..PixmapPaint::default()
        };
        let (ox, oy) = self.orb.at;
        self.frame.draw_pixmap(
            ox,
            oy,
            self.orb.pix.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
        if self.ring.as_ref().map(|(r, _)| *r) != Some(s.ring) {
            let layer = self.ring_layer(s.ring);
            self.ring = Some((s.ring, layer));
        }
        if let Some((_, ring)) = &self.ring
            && s.ring > 0
        {
            self.frame
                .draw_pixmap(ox, oy, ring.as_ref(), &paint, Transform::identity(), None);
        }
        let g = &self.g;
        let p = &mut self.frame;

        // `.meter`: six bars, 8 px wide, 5 px apart, radius 4.
        let bars = s.talking.as_ref().map_or([22; 6], |t| t.bars);
        let bar_c = with_alpha(text_c(), opacity);
        // Six bars and five gaps: 73 px, centred.
        let x0 = g.orb.0 - g.s(36.5);
        let mut pb = PathBuilder::new();
        for (i, b) in bars.iter().enumerate() {
            let h = g.s(f32::from(*b));
            push_rrect(
                &mut pb,
                x0 + g.s(13.0) * i as f32,
                g.orb.1 - h / 2.0,
                g.s(8.0),
                h,
                [g.s(4.0); 4],
            );
        }
        if let Some(path) = pb.finish() {
            p.fill_path(
                &path,
                &solid(bar_c),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        // `.talker` and `.talker-sub`.
        let (call, suffix, sub, color) = match (&s.talking, &s.last_talker) {
            (Some(t), _) => (
                Some(t.call.as_str()),
                t.suffix.as_deref(),
                t.sub.as_str(),
                text_c(),
            ),
            (None, Some(last)) => (Some(last.as_str()), None, "last heard", dim_c()),
            (None, None) => (None, None, "waiting for the first over", dim_c()),
        };
        let talker_top = g.orb.1 + g.s(100.0 + 22.0);
        let max_w = g.s(NOW_TEXT_W);
        if let Some(call) = call {
            let px = g.s(TALKER_PX);
            let sp = px * 0.03;
            let base = baseline(talker_top, px, px, Weight::ExtraBold);
            let suf_px = g.s(24.0);
            let gap = g.s(10.0);
            let suffix = suffix.map(|x| fit(x, g.s(110.0), suf_px, Weight::SemiBold, 0.0, false));
            let suf_w = suffix.as_deref().map_or(0.0, |x| {
                gap + font::measure(x, suf_px, Weight::SemiBold, false)
            });
            let mut shown = fit(call, max_w - suf_w, px, Weight::ExtraBold, sp, false);
            if shown.is_empty() {
                shown = fit(
                    UNKNOWN_STATION,
                    max_w - suf_w,
                    px,
                    Weight::ExtraBold,
                    sp,
                    false,
                );
            }
            let w = spaced_w(&shown, px, Weight::ExtraBold, sp, false);
            let x = g.orb.0 - f32::midpoint(w, suf_w);
            draw_spaced(p, &shown, x, base, px, Weight::ExtraBold, color, sp, false);
            if let Some(sfx) = suffix {
                font::draw_text(
                    p,
                    &sfx,
                    x + w + gap,
                    base,
                    suf_px,
                    Weight::SemiBold,
                    dim_c(),
                    false,
                );
            }
        }
        let px = g.s(SUB_PX);
        let sub = fit(sub, max_w, px, Weight::Regular, 0.0, true);
        let w = font::measure(&sub, px, Weight::Regular, true);
        let top = talker_top + g.s(TALKER_PX + 6.0);
        font::draw_text(
            p,
            &sub,
            g.orb.0 - w / 2.0,
            baseline(top, px * 1.5, px, Weight::Regular),
            px,
            Weight::Regular,
            dim_c(),
            true,
        );
    }

    fn draw_clock_and_footer(&mut self, s: &FrameState) {
        let g = &self.g;
        let p = &mut self.frame;
        let px = g.s(FOOT_PX);
        if !s.live && !s.clock.is_empty() {
            let clock = fit(
                &s.clock,
                g.s(COL_RIGHT) - self.clock_left,
                px,
                Weight::Regular,
                0.0,
                true,
            );
            let w = font::measure(&clock, px, Weight::Regular, true);
            font::draw_text(
                p,
                &clock,
                g.s(COL_RIGHT) - w,
                baseline_at(g.top_c, px, Weight::Regular),
                px,
                Weight::Regular,
                dim_c(),
                true,
            );
        }
        // `.foot { gap: 16px }`.
        let base = baseline(g.s(FOOT_TOP), px * 1.5, px, Weight::Regular);
        let mut x = g.s(PAD_X);
        let right = g.s(COL_RIGHT);
        for (i, part) in [&s.footer_time, &s.footer_count].into_iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            if i > 0 {
                x += g.s(16.0);
            }
            let t = fit(part, (right - x).max(0.0), px, Weight::Regular, 0.0, true);
            font::draw_text(p, &t, x, base, px, Weight::Regular, dim_c(), true);
            x += font::measure(&t, px, Weight::Regular, true);
        }
    }
}

/// The column's fixed parts, for [`blit`].
struct ColumnView<'a> {
    at: (i32, i32),
    size: (u32, u32),
    clip: &'a Mask,
    fade: &'a [f32],
    over: &'a [u8],
}

/// Draw a bubble sprite whose top-left is at frame (`x0`, `y0`) into
/// the composite's column: clipped to the padding box, then faded by
/// the mask. Below the fade this is plain source-over; in it, the
/// bubble's change to the unfaded column is scaled by the mask.
fn blit(comp: &mut Pixmap, v: &ColumnView<'_>, sprite: &Pixmap, x0: i32, y0: i32, opacity: f32) {
    let (cw, ch) = (v.size.0 as i32, v.size.1 as i32);
    let fw = comp.width() as usize;
    let sw = sprite.width() as i32;
    let sd = sprite.data();
    let clip = v.clip.data();
    let comp = comp.data_mut();
    for sy in 0..sprite.height() as i32 {
        let ly = y0 + sy - v.at.1;
        if ly < 0 || ly >= ch {
            continue;
        }
        let f = v.fade[ly as usize];
        if f <= 0.0 {
            continue;
        }
        for sx in 0..sw {
            let lx = x0 + sx - v.at.0;
            if lx < 0 || lx >= cw {
                continue;
            }
            let si = ((sy * sw + sx) * 4) as usize;
            if sd[si + 3] == 0 {
                continue;
            }
            let li = (ly * cw + lx) as usize;
            let k = f32::from(clip[li]) / 255.0 * opacity;
            if k <= 0.0 {
                continue;
            }
            let sa = f32::from(sd[si + 3]) / 255.0 * k;
            let ci = (((v.at.1 + ly) as usize) * fw + (v.at.0 + lx) as usize) * 4;
            for c in 0..3 {
                let src = f32::from(sd[si + c]) * k;
                let cur = f32::from(comp[ci + c]);
                let out = if f >= 1.0 {
                    src + cur * (1.0 - sa)
                } else {
                    cur + f * (src - sa * f32::from(v.over[li * 4 + c]))
                };
                comp[ci + c] = out.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// `.frame`'s three blurred washes (`.g1`, `.g2`, `.g3`) on black, worked
/// out on a coarse grid and scaled up: they are blurred by 80 px, so the
/// grid is invisible.
fn washes(p: &mut Pixmap, k: f32) {
    const CELL: f32 = 8.0;
    // (centre x, centre y, radius where it reaches transparent, colour,
    // opacity), mockup px. Each is `radial-gradient(circle, c,
    // transparent 70%)` in a square box: 70 % of the corner distance.
    let blobs = [
        (90.0, 30.0, 350.0, 0x1e_3a_8a, 0.55),
        (1080.0, 635.0, 320.0, 0x6d_28_d9, 0.55),
        (730.0, 490.0, 190.0, 0x9a_34_12, 0.3),
    ];
    let (w, h) = (p.width() as f32, p.height() as f32);
    let sigma = 80.0 * k / CELL;
    let margin = (3.0 * sigma).ceil() as usize + 2;
    let gw = (w / CELL).ceil() as usize + 2 * margin + 2;
    let gh = (h / CELL).ceil() as usize + 2 * margin + 2;
    // Premultiplied RGBA, painted in order.
    let mut acc = vec![[0.0f32; 4]; gw * gh];
    let mut plane = vec![0.0f32; gw * gh];
    for (bx, by, br, col, op) in blobs {
        let c = rgb(hex(col));
        let (bx, by, br) = (bx * k, by * k, br * std::f32::consts::SQRT_2 * 0.7 * k);
        for gy in 0..gh {
            for gx in 0..gw {
                let x = (gx as f32 - margin as f32) * CELL;
                let y = (gy as f32 - margin as f32) * CELL;
                let d = ((x - bx).powi(2) + (y - by).powi(2)).sqrt();
                plane[gy * gw + gx] = op * (1.0 - d / br).clamp(0.0, 1.0);
            }
        }
        blur(&mut plane, gw, gh, sigma);
        for (a, v) in acc.iter_mut().zip(&plane) {
            let inv = 1.0 - v;
            for ch in 0..3 {
                a[ch] = c[ch] * v + a[ch] * inv;
            }
            a[3] = v + a[3] * inv;
        }
    }
    let pw = p.width();
    let data = p.data_mut();
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = (i as u32 % pw, i as u32 / pw);
        let fx = (x as f32 + 0.5) / CELL + margin as f32;
        let fy = (y as f32 + 0.5) / CELL + margin as f32;
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let at = |xx: usize, yy: usize| acc[yy.min(gh - 1) * gw + xx.min(gw - 1)];
        let (a, b, c, d) = (
            at(x0, y0),
            at(x0 + 1, y0),
            at(x0, y0 + 1),
            at(x0 + 1, y0 + 1),
        );
        let n = dither(x, y);
        for ch in 0..3 {
            let top = a[ch] + (b[ch] - a[ch]) * tx;
            let bot = c[ch] + (d[ch] - c[ch]) * tx;
            px[ch] = (top + (bot - top) * ty + n).round().clamp(0.0, 255.0) as u8;
        }
        px[3] = 255;
    }
}

/// `.glass`: the translucent card, its 1 px edge, the inner top highlight
/// and (when `shadowed`) `box-shadow: 0 20px 50px rgba(0,0,0,.5)`.
fn glass(p: &mut Pixmap, r: Rect, radius: f32, k: f32, shadowed: bool) {
    let (x, y, w, h) = (r.left(), r.top(), r.width(), r.height());
    let Some(shape) = rrect(x, y, w, h, [radius; 4]) else {
        return;
    };
    if shadowed {
        shadow(
            p,
            &shape,
            0.0,
            20.0 * k,
            25.0 * k,
            rgba(0, 0, 0, 0.5),
            Cast::Outside,
        );
    }
    p.fill_path(
        &shape,
        &solid(rgba(22, 22, 26, 0.55)),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let b = k;
    // `inset 0 1px 0 var(--edge-glow)`: inside the border, the padding box
    // minus itself moved down a pixel.
    let (ix, iy, iw, ih, ir) = (x + b, y + b, w - 2.0 * b, h - 2.0 * b, radius - b);
    let mut pb = PathBuilder::new();
    push_rrect(&mut pb, ix, iy, iw, ih, [ir; 4]);
    push_rrect(&mut pb, ix, iy + b, iw, ih, [ir; 4]);
    if let (Some(path), Some(mut clip)) = (pb.finish(), Mask::new(p.width(), p.height())) {
        if let Some(inner) = rrect(ix, iy, iw, ih, [ir; 4]) {
            clip.fill_path(&inner, FillRule::Winding, true, Transform::identity());
        }
        p.fill_path(
            &path,
            &solid(rgba(255, 255, 255, 0.22)),
            FillRule::EvenOdd,
            Transform::identity(),
            Some(&clip),
        );
    }
    if let Some(edge) = ring_rrect(x, y, w, h, [radius; 4], b) {
        p.fill_path(
            &edge,
            &solid(rgba(255, 255, 255, 0.14)),
            FillRule::EvenOdd,
            Transform::identity(),
            None,
        );
    }
}

/// `.live`: the badge at the right of the top row. Returns its left edge.
fn live_badge(p: &mut Pixmap, g: &Geometry) -> f32 {
    let px = g.s(13.0);
    let sp = px * 0.06;
    let label = "LIVE";
    let text_w = spaced_w(label, px, Weight::Bold, sp, false) + sp;
    let w = g.s(1.0 + 12.0 + 8.0 + 8.0 + 12.0 + 1.0) + text_w;
    let h = g.s(TOP_H);
    let x = g.s(COL_RIGHT) - w;
    let y = g.top_c - h / 2.0;
    if let Some(shape) = rrect(x, y, w, h, [h / 2.0; 4]) {
        p.fill_path(
            &shape,
            &solid(rgba(255, 69, 58, 0.12)),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if let Some(edge) = ring_rrect(x, y, w, h, [h / 2.0; 4], g.s(1.0)) {
        p.fill_path(
            &edge,
            &solid(rgba(255, 69, 58, 0.45)),
            FillRule::EvenOdd,
            Transform::identity(),
            None,
        );
    }
    // `.live i`: an 8 px red dot with `box-shadow: 0 0 10px var(--red)`.
    let (dx, dr) = (x + g.s(1.0 + 12.0 + 4.0), g.s(4.0));
    if let Some(dot) = PathBuilder::from_circle(dx, g.top_c, dr) {
        shadow(p, &dot, 0.0, 0.0, g.s(5.0), hex(0xff_45_3a), Cast::Outside);
        p.fill_path(
            &dot,
            &solid(hex(0xff_45_3a)),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    draw_spaced(
        p,
        label,
        x + g.s(1.0 + 12.0 + 8.0 + 8.0),
        baseline_at(g.top_c, px, Weight::Bold),
        px,
        Weight::Bold,
        hex(0xff_b4_ae),
        sp,
        false,
    );
    x
}

/// A bubble's text, fitted, and its width.
struct BubbleText<'a> {
    call: Cow<'a, str>,
    meta: Cow<'a, str>,
    w: f32,
}

/// `.b { min-width: 250px; max-width: 72% }` around its callsign and meta
/// line, both cut to fit.
fn bubble_text<'a>(g: &Geometry, b: &'a Bubble) -> BubbleText<'a> {
    let border = if b.mine { 0.0 } else { g.s(1.0) };
    let pad_x = g.s(BUBBLE_PAD_X);
    let call_px = g.s(CALL_PX);
    let meta_px = g.s(META_PX);
    let max_w = g.bubble_max_w();
    let text_max = max_w - 2.0 * (pad_x + border);
    let call = fit(&b.call, text_max, call_px, Weight::Bold, 0.0, false);
    // A call made only of characters Inter cannot draw.
    let call = if call.is_empty() {
        fit(UNKNOWN_STATION, text_max, call_px, Weight::Bold, 0.0, false)
    } else {
        call
    };
    let meta = fit(&b.meta, text_max, meta_px, Weight::Regular, 0.0, true);
    let content = font::measure(&call, call_px, Weight::Bold, false).max(font::measure(
        &meta,
        meta_px,
        Weight::Regular,
        true,
    ));
    let w = (content + 2.0 * (pad_x + border)).clamp(g.s(BUBBLE_MIN_W), max_w);
    BubbleText { call, meta, w }
}

/// One `.b` bubble with its top-left at (`x`, `y`) in `p`, `h` tall. Mine
/// are in the blue gradient; the playing one has the magenta edge and
/// glow, and its waveform lit up to `played`.
fn draw_bubble(
    p: &mut Pixmap,
    g: &Geometry,
    b: &Bubble,
    t: &BubbleText<'_>,
    x: f32,
    y: f32,
    h: f32,
    glow: &mut Option<(GlowKey, Pixmap, (i32, i32))>,
) {
    let border = if b.mine { 0.0 } else { g.s(1.0) };
    let pad_x = g.s(BUBBLE_PAD_X);
    let call_px = g.s(CALL_PX);
    let meta_px = g.s(META_PX);
    let (w, call, meta) = (t.w, &t.call, &t.meta);
    let (big, small) = (g.s(20.0), g.s(7.0));
    let radii = if b.mine {
        [big, big, small, big]
    } else {
        [big, big, big, small]
    };
    let Some(shape) = rrect(x, y, w, h, radii) else {
        return;
    };

    if b.on {
        // `box-shadow: 0 0 30px rgba(168,85,247,.4)`, cached: it only
        // changes with the bubble's size and sub-pixel position.
        let key = GlowKey {
            w,
            h,
            mine: b.mine,
            fx: x.fract(),
            fy: y.fract(),
        };
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        if glow.as_ref().is_none_or(|(k, _, _)| *k != key) {
            let m = g.s(50.0).ceil() as i32;
            let mut gp = pixmap(
                w.ceil() as u32 + 2 * m as u32 + 2,
                h.ceil() as u32 + 2 * m as u32 + 2,
            );
            let local = rrect(
                x - ix as f32 + m as f32,
                y - iy as f32 + m as f32,
                w,
                h,
                radii,
            );
            if let Some(local) = local {
                shadow(
                    &mut gp,
                    &local,
                    0.0,
                    0.0,
                    g.s(15.0),
                    rgba(168, 85, 247, 0.4),
                    Cast::Outside,
                );
            }
            *glow = Some((key, gp, (-m, -m)));
        }
        if let Some((_, gp, (gx, gy))) = glow.as_ref() {
            p.draw_pixmap(
                ix + gx,
                iy + gy,
                gp.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
    }

    if b.mine {
        // `linear-gradient(135deg, #1d4ed8, #312e81)`: toward the
        // bottom-right corner, the gradient line as long as CSS makes it.
        let half = (w + h) * std::f32::consts::FRAC_1_SQRT_2 / 2.0;
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let d = half * std::f32::consts::FRAC_1_SQRT_2;
        if let Some(sh) = LinearGradient::new(
            Point::from_xy(cx - d, cy - d),
            Point::from_xy(cx + d, cy + d),
            stops(&[(0.0, hex(0x1d_4e_d8)), (1.0, hex(0x31_2e_81))]),
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            p.fill_path(
                &shape,
                &shaded(sh),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    } else {
        p.fill_path(
            &shape,
            &solid(rgba(255, 255, 255, 0.05)),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let edge = if b.on {
            rgba(217, 70, 239, 0.6)
        } else {
            rgba(255, 255, 255, 0.14)
        };
        if let Some(ring) = ring_rrect(x, y, w, h, radii, border) {
            p.fill_path(
                &ring,
                &solid(edge),
                FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
        }
    }

    let ix = x + border + pad_x;
    let mut top = y + border + g.s(BUBBLE_PAD_T);
    font::draw_text(
        p,
        call,
        ix,
        baseline(top, call_px * 1.5, call_px, Weight::Bold),
        call_px,
        Weight::Bold,
        text_c(),
        false,
    );
    top += call_px * 1.5 + g.s(6.0);

    // `.wave`: PEAK_BINS bars sharing the width, 2 px apart, each as tall
    // as its peak against the over's loudest (at least 12 %), as the
    // recordings page draws them.
    let inner_w = w - 2.0 * (border + pad_x);
    let gap = g.s(2.0);
    let bw = (inner_w - gap * (PEAK_BINS as f32 - 1.0)) / PEAK_BINS as f32;
    let wave_h = g.s(WAVE_H);
    let n = b.peaks.len();
    let loudest = b.peaks.iter().copied().max().unwrap_or(0);
    let quiet = with_alpha(text_c(), 0.3);
    let (lit_a, lit_b) = if b.mine {
        (hex(0xff_ff_ff), hex(0xe8_79_f9))
    } else {
        (hex(0xe8_79_f9), hex(0xb0_4a_e8))
    };
    let mut quiet_pb = PathBuilder::new();
    for i in 0..PEAK_BINS {
        let peak = if n == 0 {
            0
        } else {
            b.peaks[i * n / PEAK_BINS]
        };
        let pct = if loudest > 0 {
            (f32::from(peak) * 100.0 / f32::from(loudest)).round()
        } else {
            0.0
        }
        .max(12.0);
        let bh = wave_h * pct / 100.0;
        let bx = ix + i as f32 * (bw + gap);
        let by = top + (wave_h - bh) / 2.0;
        let lit = b.on && i < usize::from(b.played);
        if lit {
            let Some(rect) = Rect::from_xywh(bx, by, bw, bh) else {
                continue;
            };
            if let Some(sh) = LinearGradient::new(
                Point::from_xy(bx, by),
                Point::from_xy(bx, by + bh),
                stops(&[(0.0, lit_a), (1.0, lit_b)]),
                SpreadMode::Pad,
                Transform::identity(),
            ) {
                // A path, not `fill_rect`: tiny-skia's anti-aliased rect
                // fast path trips its own debug assertion on a bar under a
                // pixel wide, which smaller frames draw.
                p.fill_path(
                    &PathBuilder::from_rect(rect),
                    &shaded(sh),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        } else {
            push_rrect(&mut quiet_pb, bx, by, bw, bh, [g.s(1.0); 4]);
        }
    }
    if let Some(path) = quiet_pb.finish() {
        p.fill_path(
            &path,
            &solid(quiet),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    top += wave_h + g.s(4.0);

    let meta_c = if b.mine {
        rgba(255, 255, 255, 0.7)
    } else {
        dim_c()
    };
    font::draw_text(
        p,
        meta,
        ix,
        baseline(top, meta_px * 1.5, meta_px, Weight::Regular),
        meta_px,
        Weight::Regular,
        meta_c,
        true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;
    use crate::scene::{Mode, SceneBuilder, SessionInfo};
    use std::path::{Path, PathBuf};

    fn scenes() -> Vec<(&'static str, Size, FrameState)> {
        let cap = |s: Size| Renderer::new(s).column_capacity();
        vec![
            (
                "live-talking-1080",
                Size::P1080,
                demo::live_talking(cap(Size::P1080)),
            ),
            (
                "recorded-gap-720",
                Size::P720,
                demo::recorded_gap(cap(Size::P720)),
            ),
            (
                "hostile-names-720",
                Size::P720,
                demo::hostile_names(cap(Size::P720)),
            ),
            (
                "live-talking-720",
                Size::P720,
                demo::live_talking(cap(Size::P720)),
            ),
        ]
    }

    fn crate_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// A golden PNG as RGBA. Goldens are stored as RGB (every frame is
    /// opaque), which keeps them small; an RGBA one still reads.
    fn read_png(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
        let file = std::fs::File::open(path).ok()?;
        let mut reader = png::Decoder::new(std::io::BufReader::new(file))
            .read_info()
            .ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        if info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf
                .chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            _ => return None,
        };
        Some((info.width, info.height, rgba))
    }

    /// Writes an opaque RGBA frame as an RGB PNG, as small as `png` makes it.
    /// The same pixels always give the same bytes.
    fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let file = std::fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::High);
        enc.set_filter(png::Filter::Adaptive);
        enc.write_header()
            .expect("png header")
            .write_image_data(&rgb)
            .expect("png data");
    }

    #[test]
    fn golden_frames_match() {
        let bless = std::env::var_os("NETVIDEO_BLESS").is_some();
        let out = crate_dir().join("../../target/netvideo-golden");
        let mut failures = Vec::new();
        for (name, size, state) in scenes() {
            let mut r = Renderer::new(size);
            let actual = r.render(&state).to_vec();
            let golden = crate_dir().join(format!("tests/golden/{name}.png"));
            if bless {
                write_png(&golden, size.width, size.height, &actual);
                continue;
            }
            let Some((w, h, expected)) = read_png(&golden) else {
                write_png(
                    &out.join(format!("{name}.actual.png")),
                    size.width,
                    size.height,
                    &actual,
                );
                failures.push(format!("{name}: no golden at {}", golden.display()));
                continue;
            };
            if (w, h) != (size.width, size.height) {
                write_png(
                    &out.join(format!("{name}.actual.png")),
                    size.width,
                    size.height,
                    &actual,
                );
                failures.push(format!(
                    "{name}: golden is {w}×{h}, the frame is {}×{}",
                    size.width, size.height
                ));
                continue;
            }
            let mut diff = vec![0u8; actual.len()];
            let mut off = 0usize;
            for (i, (a, e)) in actual.chunks(4).zip(expected.chunks(4)).enumerate() {
                let worst = (0..4).map(|k| a[k].abs_diff(e[k])).max().unwrap_or(0);
                if worst > 3 {
                    off += 1;
                    diff[i * 4..i * 4 + 4].copy_from_slice(&[255, 0, 255, 255]);
                } else {
                    diff[i * 4 + 3] = 255;
                }
            }
            let total = (size.width * size.height) as usize;
            if off * 200 > total {
                write_png(&out.join(format!("{name}.actual.png")), w, h, &actual);
                write_png(&out.join(format!("{name}.diff.png")), w, h, &diff);
                failures.push(format!("{name}: {off} of {total} pixels differ"));
            }
        }
        assert!(
            failures.is_empty(),
            "{failures:#?} (NETVIDEO_BLESS=1 rewrites the goldens)"
        );
    }

    #[test]
    #[should_panic(expected = "frame A is 16:9")]
    fn a_square_frame_is_refused_clearly() {
        let _ = Renderer::new(Size {
            width: 1000,
            height: 1000,
        });
    }

    #[test]
    #[should_panic(expected = "frame A is 16:9")]
    fn a_letterbox_frame_is_refused_clearly() {
        let _ = Renderer::new(Size {
            width: 1920,
            height: 800,
        });
    }

    #[test]
    fn near_16_9_sizes_draw() {
        // 854×480 is 16:9 to within a pixel.
        let size = Size {
            width: 854,
            height: 480,
        };
        let mut r = Renderer::new(size);
        let state = demo::live_talking(r.column_capacity());
        assert_eq!(r.render(&state).len(), (854 * 480 * 4) as usize);
    }

    #[test]
    fn same_state_reuses_the_frame() {
        let state = demo::live_talking(5);
        let mut r = Renderer::new(Size::P720);
        let first = r.render(&state).as_ptr();
        let second = r.render(&state).as_ptr();
        assert_eq!(first, second);
        assert_eq!(
            r.stats(),
            RenderStats {
                drawn: 1,
                reused: 1
            }
        );
    }

    #[test]
    fn a_clock_tick_redraws_without_redrawing_the_column() {
        let state = demo::recorded_gap(5);
        let mut r = Renderer::new(Size::P720);
        let before = r.render(&state).to_vec();
        assert_eq!(r.column_draws, 1);
        let mut tick = state.clone();
        tick.clock = "19:03:03".into();
        tick.footer_time = "3:03 / 10:00".into();
        let after = r.render(&tick).to_vec();
        assert_eq!(r.column_draws, 1, "the column was redrawn for a clock tick");
        assert_eq!(r.stats().drawn, 2);
        assert_ne!(before, after, "the clock did not change the frame");
        let mut scrolled = tick.clone();
        scrolled.scroll = 40;
        r.render(&scrolled);
        assert_eq!(r.column_draws, 2);
    }

    /// Whether any pixel in `x0..x1 × y0..y1` differs from `base` by more than
    /// a few levels.
    fn strip_differs(
        a: &[u8],
        base: &[u8],
        width: u32,
        x0: u32,
        x1: u32,
        y0: u32,
        y1: u32,
    ) -> Option<(u32, u32)> {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * width + x) * 4) as usize;
                if (0..3).any(|k| a[i + k].abs_diff(base[i + k]) > 6) {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn hostile_names_stay_inside_their_bubbles() {
        let size = Size::P1080;
        let mut r = Renderer::new(size);
        let state = demo::hostile_names(r.column_capacity());
        assert!(
            state
                .bubbles
                .iter()
                .any(|b| b.mine && b.call.chars().count() >= 40)
        );
        assert!(state.bubbles.iter().any(|b| b.call.contains('🙂')));
        let frame = r.render(&state).to_vec();
        let mut empty = state.clone();
        empty.bubbles.clear();
        empty.talking = None;
        empty.last_talker = None;
        let mut r2 = Renderer::new(size);
        let base = r2.render(&empty).to_vec();
        let g = Geometry::new(size);
        // Right of the column's inner edge, down its whole height.
        let x0 = (g.col_inner_right()).ceil() as u32 + 1;
        let x1 = g.col.right().floor() as u32;
        let (y0, y1) = (g.col.top().ceil() as u32, g.col.bottom().floor() as u32);
        assert_eq!(
            strip_differs(&frame, &base, size.width, x0, x1, y0, y1),
            None,
            "a bubble spilled past the column's inner edge"
        );
        // Between the talker panel and the column.
        let x0 = g.now.right().ceil() as u32 + 1;
        let x1 = g.col.left().floor() as u32;
        assert_eq!(
            strip_differs(&frame, &base, size.width, x0, x1, y0, y1),
            None,
            "the talker spilled out of its panel"
        );
    }

    #[test]
    fn buffer_is_opaque() {
        for (_, size, state) in scenes() {
            let mut r = Renderer::new(size);
            let buf = r.render(&state);
            assert_eq!(buf.len(), (size.width * size.height * 4) as usize);
            assert!(buf.chunks(4).all(|p| p[3] == 255));
        }
    }

    /// The perf test's scene: KX0AAA's over, one frame per 1/30 s, with its
    /// audio fed as it would arrive live (the level bars only see the last
    /// second of audio).
    fn talking_frames(n: u64) -> Vec<FrameState> {
        let mut b = SceneBuilder::new(
            SessionInfo::from_meta(&demo::meta("Tuesday hub net", "M17-XXX C"), &[]).expect("meta"),
            Mode::Live,
        );
        b.set_capacity(Renderer::new(Size::P1080).column_capacity());
        let start = demo::MOCKUP_OVERS[2].2;
        demo::feed(&mut b, &demo::MOCKUP_OVERS, start);
        let mut fed = 0u64;
        (0..n)
            .map(|k| {
                let t = start + k * 1_000 / 30;
                let upto = (t - start) * 8;
                let block: Vec<i16> = (fed..upto).map(|i| demo::voice(3, i)).collect();
                b.audio(start + fed / 8, &block);
                fed = upto;
                b.frame_at(t)
            })
            .collect()
    }

    /// CPU time of this thread, so a loaded machine does not fail the test.
    #[cfg(unix)]
    fn thread_cpu() -> std::time::Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime only writes one timespec into `ts`.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
        assert_eq!(rc, 0, "clock_gettime");
        std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    /// Without a thread clock, wall time since first use.
    #[cfg(not(unix))]
    fn thread_cpu() -> std::time::Duration {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }

    #[test]
    #[ignore = "perf: run with `just netvideo-perf` (release)"]
    fn perf_renderer_keeps_up() {
        let frames = talking_frames(900);
        let mut r = Renderer::new(Size::P1080);
        let cpu = thread_cpu();
        let wall = std::time::Instant::now();
        for f in &frames {
            std::hint::black_box(r.render(f));
        }
        let (cpu, wall) = (thread_cpu().saturating_sub(cpu), wall.elapsed());
        let fps = 900.0 / cpu.as_secs_f64();
        println!(
            "talking: 900 frames at 1080p in {:.3} s CPU ({:.3} s wall): {fps:.0} frames per CPU second; drawn {} reused {}",
            cpu.as_secs_f64(),
            wall.as_secs_f64(),
            r.stats().drawn,
            r.stats().reused
        );
        assert!(fps >= 90.0, "{fps:.0} frames per CPU second");

        let gap = demo::recorded_gap(r.column_capacity());
        let mut r = Renderer::new(Size::P1080);
        r.render(&gap);
        let cpu = thread_cpu();
        for _ in 0..900 {
            std::hint::black_box(r.render(&gap));
        }
        let cpu = thread_cpu().saturating_sub(cpu);
        println!("silent gap: 900 frames in {:.6} s CPU", cpu.as_secs_f64());
        assert!(cpu.as_secs_f64() < 0.1, "{cpu:?}");
    }
}
