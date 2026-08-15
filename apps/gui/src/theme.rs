// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The astar slate palette and small style helpers.
//!
//! A single dark theme: a slate `#1E293B` background, lighter slate surfaces for
//! cards/bars, and near-white ink for text. TX is red, RX is green — the colors
//! the indicators and VU bars use so states read clearly at a glance.

use iced::font::Weight;
use iced::{Color, Font};

/// The app font — Inter (bundled in `assets/fonts`, loaded in `main`), the
/// cross-platform stand-in for the Mac app's SF Pro.
pub const FONT: Font = Font::with_name("Inter");
/// Medium weight, for emphasized labels.
pub const FONT_MEDIUM: Font = Font {
    weight: Weight::Medium,
    ..FONT
};
/// Semibold weight, for the wordmark and headings.
pub const FONT_SEMIBOLD: Font = Font {
    weight: Weight::Semibold,
    ..FONT
};

/// `rgb8` helper at compile time (Iced has no const `from_rgb8`).
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
}

/// App background — slate `#1E293B`.
pub const BG: Color = rgb(0x1E, 0x29, 0x3B);
/// Card / panel surface — a lighter slate `#27374D`.
pub const SURFACE: Color = rgb(0x27, 0x37, 0x4D);
/// Inset VU-bar track — darker than the surface so the fill stands out.
pub const TRACK: Color = rgb(0x14, 0x1E, 0x2E);
/// Hairline border on surfaces — `#33455E`.
pub const BORDER: Color = rgb(0x33, 0x45, 0x5E);

/// Primary text — near white `#F1F5F9`.
pub const INK: Color = rgb(0xF1, 0xF5, 0xF9);
/// Muted/secondary text — slate `#94A3B8`.
pub const MUTED: Color = rgb(0x94, 0xA3, 0xB8);

/// TX accent — red `#EF4444` (lit while transmitting).
pub const TX: Color = rgb(0xEF, 0x44, 0x44);
/// RX accent — green `#22C55E` (lit while receiving).
pub const RX: Color = rgb(0x22, 0xC5, 0x5E);
/// Connecting accent — amber `#F59E0B`. Doubles as the VOX sliders' tint
/// (the Mac uses orange there).
pub const CONNECTING: Color = rgb(0xF5, 0x9E, 0x0B);
/// Control accent — blue `#3B82F6`: on-state togglers and the compression
/// "Strength" slider (the Mac's `.tint(.blue)` / default switch tint).
pub const ACCENT: Color = rgb(0x3B, 0x82, 0xF6);

/// A dimmed version of an accent for the unlit indicator pill.
#[must_use]
pub fn dim(c: Color) -> Color {
    Color { a: 0.22, ..c }
}
