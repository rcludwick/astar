// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The TX/RX indicator pills and VU bars.

use iced::widget::{column, container, row, text, Space};
use iced::{Background, Border, Color, Element, Fill, Length};

use super::surface;
use crate::app::Message;
use crate::snapshot::Snapshot;
use crate::theme;

/// The TX/RX indicator pills.
pub fn indicators(snap: &Snapshot) -> Element<'_, Message> {
    row![
        indicator("TX", theme::TX, snap.transmitting),
        indicator("RX", theme::RX, snap.receiving),
    ]
    .spacing(14)
    .width(Fill)
    .into()
}

/// One indicator pill: filled with its accent and dark text when `lit`,
/// otherwise a dim outline with muted text.
fn indicator(label: &str, accent: Color, lit: bool) -> Element<'static, Message> {
    let (bg, fg) = if lit {
        (accent, theme::BG)
    } else {
        (theme::dim(accent), theme::MUTED)
    };
    container(text(label.to_string()).size(20).color(fg))
        .padding([12, 0])
        .width(Fill)
        .center_x(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(bg)),
            border: Border {
                color: accent,
                width: 1.5,
                radius: 10.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// The two VU bars (TX then RX), each labelled, with a percent readout.
/// TX brightens while keyed, RX while receiving (mirrors the Mac popover).
pub fn vu_bars(snap: &Snapshot) -> Element<'_, Message> {
    surface(
        column![
            vu_row("TX", theme::TX, snap.tx_level, snap.transmitting),
            vu_row("RX", theme::RX, snap.rx_level, snap.receiving),
        ]
        .spacing(14),
    )
    .into()
}

/// The bar's fill as a whole-number percent label — derived from the SAME
/// clamped level that sizes the fill, so the readout always matches the bar.
fn percent_label(level: f32) -> String {
    format!("{}%", (level.clamp(0.0, 1.0) * 100.0).round() as u16)
}

/// One labelled VU row: fixed-width label, filled track, then the percent —
/// also fixed-width so the bar's trailing edge doesn't jiggle as it changes.
fn vu_row(label: &str, accent: Color, level: f32, active: bool) -> Element<'static, Message> {
    row![
        container(text(label.to_string()).size(14).color(theme::MUTED)).width(34),
        vu_bar(accent, level, active),
        container(
            text(percent_label(level))
                .size(13)
                .color(theme::MUTED)
                .font(theme::FONT_MEDIUM)
        )
        .width(44)
        .align_x(iced::Alignment::End),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center)
    .into()
}

/// A horizontal VU bar: a dark inset track with an accent-colored fill whose
/// width is `level` (0..1) of the track; the fill dims when the channel is
/// inactive (Swift: `tint.opacity(active ? 1.0 : 0.7)`). Implemented as two
/// nested fixed-height containers using FillPortion so the fill scales with
/// the available width.
fn vu_bar(accent: Color, level: f32, active: bool) -> Element<'static, Message> {
    let accent = if active {
        accent
    } else {
        Color { a: 0.7, ..accent }
    };
    let level = level.clamp(0.0, 1.0);
    // Quantize to integer portions so the fill/rest split is well-defined.
    let filled = (level * 1000.0).round() as u16;
    let rest = 1000u16.saturating_sub(filled);

    let fill = container(Space::new())
        .width(Length::FillPortion(filled.max(1)))
        .height(Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(accent)),
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });

    let gap = container(Space::new()).width(Length::FillPortion(rest.max(1)));

    let bar = if filled == 0 {
        // Avoid a stray sliver of fill when truly silent.
        row![container(Space::new()).width(Fill)]
    } else if rest == 0 {
        row![fill]
    } else {
        row![fill, gap]
    };

    // Thin capsule track, per the Mac's compact LevelMeter (a 6 pt capsule);
    // 8 px here to suit the larger window scale.
    container(bar)
        .width(Fill)
        .height(8)
        .padding(0)
        .style(|_| container::Style {
            background: Some(Background::Color(theme::TRACK)),
            border: Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_label_matches_the_fill_fraction() {
        assert_eq!(percent_label(0.0), "0%");
        assert_eq!(percent_label(1.0), "100%");
        assert_eq!(percent_label(0.5), "50%");
        // Same clamping as the bar fill — out-of-range never shows >100%.
        assert_eq!(percent_label(1.7), "100%");
        assert_eq!(percent_label(-0.3), "0%");
    }
}
