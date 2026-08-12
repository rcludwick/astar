// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Window + status icons: the rainbow brand asterisk and its status-tinted
//! variants (astar-6922; asterisk mark astar-cdab).
//!
//! Mirrors the macOS menu-bar asterisk (`StatusItemController`): the rainbow
//! brand asterisk when idle, and a solid asterisk tinted blue/green/pale-red
//! for the connected/receiving/transmitting states. The PNGs are pre-rendered
//! from `art/*.svg` into `assets/icon/` (see `Tools/render-icons.sh`) and
//! embedded here.

use iced::window::icon::{self, Icon};

/// The app icon — the rounded slate badge with the rainbow asterisk.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icon/astar-256.png");

/// The bare rainbow asterisk (no badge), used for the idle status icon.
const RAINBOW_32: &[u8] = include_bytes!("../assets/icon/asterisk-rainbow-32.png");
const RAINBOW_64: &[u8] = include_bytes!("../assets/icon/asterisk-rainbow-64.png");

/// The asterisk silhouette; only its alpha matters — [`tint`] paints the RGB.
const TEMPLATE_32: &[u8] = include_bytes!("../assets/icon/asterisk-template-32.png");
const TEMPLATE_64: &[u8] = include_bytes!("../assets/icon/asterisk-template-64.png");

/// Status-icon states, mirroring the Mac app's `StatusIconState`. The tray
/// shell ([`crate::tray`]) maps each snapshot onto one of these
/// (`tray::status_icon`) and tints the tray asterisk to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusIcon {
    /// Not connected — the rainbow brand asterisk.
    Idle,
    /// Connected, no audio — blue.
    Connected,
    /// Receiving — green.
    Receiving,
    /// Transmitting — pale red.
    Transmitting,
}

impl StatusIcon {
    /// The tint for this state, or `None` for the untinted rainbow asterisk.
    /// Blue matches macOS `systemBlue`; green is the theme RX accent; the pale
    /// red matches the Mac mark's `(0.95, 0.45, 0.45)`.
    fn tint_rgb(self) -> Option<[u8; 3]> {
        match self {
            StatusIcon::Idle => None,
            StatusIcon::Connected => Some([0x0A, 0x84, 0xFF]),
            StatusIcon::Receiving => Some([0x22, 0xC5, 0x5E]),
            StatusIcon::Transmitting => Some([0xF2, 0x73, 0x73]),
        }
    }
}

/// Decode the embedded app icon for the window. `None` only if the embedded
/// PNG is somehow undecodable — callers just skip setting an icon.
pub fn window_icon() -> Option<Icon> {
    let (rgba, w, h) = decode(WINDOW_ICON_PNG)?;
    icon::from_rgba(rgba, w, h).ok()
}

/// The status asterisk for `state` as straight-alpha RGBA, at 32 px (`hidpi` =
/// false) or 64 px — the pixel source for the tray icon backends in
/// [`crate::tray`]. BOTH PNGs render from `art/menubar-rainbow.svg` (see
/// `Tools/render-icons.sh`) so every state shares ONE geometry and the tray
/// asterisk never changes size between states (the Mac's astar-3f57 fix).
pub fn status_asterisk_rgba(state: StatusIcon, hidpi: bool) -> Option<(Vec<u8>, u32, u32)> {
    let (rainbow, template) = if hidpi {
        (RAINBOW_64, TEMPLATE_64)
    } else {
        (RAINBOW_32, TEMPLATE_32)
    };
    match state.tint_rgb() {
        None => decode(rainbow),
        Some(rgb) => {
            let (mut rgba, w, h) = decode(template)?;
            tint(&mut rgba, rgb);
            Some((rgba, w, h))
        }
    }
}

/// Decode a PNG into straight-alpha RGBA bytes plus dimensions.
fn decode(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(png).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Paint every pixel's RGB with `rgb`, leaving alpha untouched — how the Mac
/// app colors its template asterisk, minus AppKit.
fn tint(rgba: &mut [u8], rgb: [u8; 3]) {
    for px in rgba.chunks_exact_mut(4) {
        px[0] = rgb[0];
        px[1] = rgb[1];
        px[2] = rgb[2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_png_decodes_at_expected_size() {
        let (_, w, h) = decode(WINDOW_ICON_PNG).expect("embedded icon decodes");
        assert_eq!((w, h), (256, 256));
    }

    #[test]
    fn window_icon_constructs() {
        assert!(window_icon().is_some());
    }

    #[test]
    fn status_asterisks_decode_at_both_scales() {
        for state in [
            StatusIcon::Idle,
            StatusIcon::Connected,
            StatusIcon::Receiving,
            StatusIcon::Transmitting,
        ] {
            let (_, w, h) = status_asterisk_rgba(state, false).expect("32px asterisk");
            assert_eq!((w, h), (32, 32));
            let (_, w, h) = status_asterisk_rgba(state, true).expect("64px asterisk");
            assert_eq!((w, h), (64, 64));
        }
    }

    #[test]
    fn tint_paints_rgb_and_preserves_alpha() {
        let mut rgba = vec![1, 2, 3, 200, 4, 5, 6, 0];
        tint(&mut rgba, [0x0A, 0x84, 0xFF]);
        assert_eq!(rgba, vec![0x0A, 0x84, 0xFF, 200, 0x0A, 0x84, 0xFF, 0]);
    }

    #[test]
    fn tinted_states_differ_from_the_rainbow_asterisk() {
        let (idle, ..) = status_asterisk_rgba(StatusIcon::Idle, false).unwrap();
        let (conn, ..) = status_asterisk_rgba(StatusIcon::Connected, false).unwrap();
        assert_ne!(idle, conn);
    }

    #[test]
    fn all_states_share_one_alpha_geometry() {
        // The astar-3f57 regression guard: the Mac menu-bar icon once
        // visibly grew/shrank between states because the rainbow and
        // template assets came from different geometry sources. Both tray
        // PNGs now render from the same asterisk shape, so every state's alpha
        // channel — the icon's footprint — must be pixel-identical.
        for hidpi in [false, true] {
            let (idle, ..) = status_asterisk_rgba(StatusIcon::Idle, hidpi).unwrap();
            for state in [
                StatusIcon::Connected,
                StatusIcon::Receiving,
                StatusIcon::Transmitting,
            ] {
                let (tinted, ..) = status_asterisk_rgba(state, hidpi).unwrap();
                let idle_a: Vec<u8> = idle.chunks_exact(4).map(|p| p[3]).collect();
                let tint_a: Vec<u8> = tinted.chunks_exact(4).map(|p| p[3]).collect();
                assert_eq!(
                    idle_a, tint_a,
                    "alpha mismatch for {state:?} (hidpi={hidpi})"
                );
            }
        }
    }
}
