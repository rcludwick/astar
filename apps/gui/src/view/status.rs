// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The connection status card: state, dialed node, resolved name, and the
//! negotiated-codec badge.

use iced::widget::{column, container, row, text, tooltip};
use iced::{Background, Border, Element};

use super::{dot, surface, tip_style, NetworkInfo};
use crate::app::Message;
use crate::network::Network;
use crate::snapshot::{Heard, Snapshot, Status};
use crate::theme;

/// Status text + dialed node + friendly name.
pub fn status_card<'a>(snap: &'a Snapshot, network: NetworkInfo<'a>) -> Element<'a, Message> {
    let accent = match snap.status {
        Status::Disconnected => theme::MUTED,
        Status::Connecting => theme::CONNECTING,
        Status::Connected => theme::RX,
    };

    // A small status dot followed by the status label.
    let mut status_line = row![
        dot(accent, 12.0),
        text(snap.status.label())
            .size(22)
            .color(theme::INK)
            .font(theme::FONT_MEDIUM),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center);

    // Codec tag (astar-eb6c/astar-efba, always-on astar-ef35): names the
    // negotiated codec whenever the call has one — green only for wideband
    // (slin16), muted for the narrowband baseline. Placement mirrors the Mac:
    // a small capsule right of the status title.
    if let (Some(badge), Some(description), Some(bitrate)) = (
        snap.codec_badge(),
        snap.codec_description(),
        snap.codec_bitrate(),
    ) {
        status_line = status_line.push(
            tooltip(
                badge_capsule(badge, snap.codec_is_wideband()),
                container(
                    text(format!(
                        "This call negotiated {description} audio · {bitrate}"
                    ))
                    .size(13)
                    .color(theme::INK),
                )
                .padding([8, 12])
                .max_width(300)
                .style(tip_style),
                tooltip::Position::Bottom,
            )
            .gap(6),
        );
    }

    // M17 codec badge (astar-bitrate): the engine only supports Codec 2 voice
    // at 3,200 bit/s today (M17 Task 8), so this is a fixed label rather than
    // a per-call negotiated codec — `codec_badge`/`codec_description` above
    // stay `None` for M17 (M17 doesn't negotiate a `VoiceFormat`). When the
    // engine gains other M17 modes it should start reporting one, and this
    // should read from the snapshot like the AllStar badge does. gui-rs has
    // no per-call network state yet (iax-b3d7), so this derives from the
    // picker selection while connected — same simplification as the network
    // badge below.
    if snap.status == Status::Connected && network.selected == Network::M17 {
        status_line = status_line.push(
            tooltip(
                badge_capsule("C2 3200", false),
                container(
                    text("Codec 2 voice at 3,200 bit/s (M17)")
                        .size(13)
                        .color(theme::INK),
                )
                .padding([8, 12])
                .max_width(300)
                .style(tip_style),
                tooltip::Position::Bottom,
            )
            .gap(6),
        );
    }

    // Network badge (astar-9b3e): the ACTIVE CALL's network, once there's
    // more than one to choose from. gui-rs has no per-call network state
    // yet, and only AllStar can actually connect today — so this derives the
    // badge from the current picker selection while connected, a
    // simplification that holds until real per-call network state arrives
    // with the engine capability (iax-b3d7).
    if snap.status == Status::Connected && network.available.len() > 1 {
        status_line = status_line.push(badge_capsule(network.selected.badge(), false));
    }

    let node_line: Element<'_, Message> = match &snap.dialed_node {
        Some(node) => text(format!("Node {node}"))
            .size(15)
            .color(theme::MUTED)
            .into(),
        None => text("No node dialed").size(15).color(theme::MUTED).into(),
    };

    let mut lines = column![status_line, node_line].spacing(6);

    if let Some(name) = &snap.node_name {
        lines = lines.push(text(name.clone()).size(15).color(theme::INK));
    }

    // Why the last action failed — the seam's error surface (no panics, no
    // silent logs; the card says what went wrong).
    if let Some(error) = &snap.error {
        lines = lines.push(text(error.clone()).size(14).color(theme::TX));
    }

    // No credential source configured (mirrors the Mac's hint text). AllStar
    // ONLY (M17 Task 10, mirrors the Mac's `needsAccount` gate — M17 has its
    // own callsign requirement, not an AllStarLink account, so this hint
    // would mislead while an M17/Hamlink dial is selected).
    if snap.needs_account && network.selected == Network::Allstar {
        lines = lines.push(
            text("Add your AllStarLink account to connect (see Settings).")
                .size(13)
                .color(theme::MUTED),
        );
    }

    // Last heard (astar-heard): up to three stations that keyed the live
    // digital link, newest first, in the same secondary style as the hints
    // above. The strings come from `heard_lines` so the wording is testable
    // without rendering; this loop only paints them.
    for line in heard_lines(&snap.heard) {
        lines = lines.push(text(line).size(13).color(theme::MUTED));
    }

    surface(lines).into()
}

/// The heard-history rows as text, newest first, at most three.
///
/// The first row carries the Mac's caption verbatim — "Last heard W6VS · now"
/// — so the two clients name the current talker the same way; the rows under
/// it are bare `callsign · age`, the Mac's history lines.
///
/// Callsigns arrive off the air and are attacker-controlled text. Nothing
/// here interprets them: they are interpolated into a plain `String` and
/// Iced's `text()` renders that verbatim (no markup, no format string), so a
/// hostile callsign is only ever a strange-looking row.
#[must_use]
pub(super) fn heard_lines(rows: &[Heard]) -> Vec<String> {
    rows.iter()
        .take(3)
        .enumerate()
        .map(|(i, row)| {
            let age = age_label(row.age_ms);
            if i == 0 {
                format!("Last heard {} · {age}", row.callsign)
            } else {
                format!("{} · {age}", row.callsign)
            }
        })
        .collect()
}

/// How long ago a station was heard, in the shortest honest unit — the same
/// table as the Mac's `HeardAge` so both clients read identically. Integer
/// division throughout: 90 s is "1 min", not "1.5 min".
#[must_use]
pub(super) fn age_label(ms: u64) -> String {
    match ms {
        ms if ms < 2_000 => "now".to_string(),
        ms if ms < 60_000 => format!("{} s", ms / 1_000),
        ms if ms < 3_600_000 => format!("{} min", ms / 60_000),
        ms => format!("{} h", ms / 3_600_000),
    }
}

/// The codec capsule (the Mac's caption2-semibold badge with a tinted capsule
/// background): green for wideband, muted for narrowband (astar-ef35). Shared
/// (`pub(super)`) with the favorites rows' network badge (astar-9b3e), which
/// always passes `wideband: false` — a network tag isn't a quality signal.
pub(super) fn badge_capsule(label: &'static str, wideband: bool) -> Element<'static, Message> {
    let tint = if wideband { theme::RX } else { theme::MUTED };
    container(text(label).size(11).color(tint).font(theme::FONT_SEMIBOLD))
        .padding([2, 7])
        .style(move |_| container::Style {
            background: Some(Background::Color(iced::Color { a: 0.15, ..tint })),
            border: Border {
                radius: 999.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::{age_label, heard_lines};
    use crate::snapshot::Heard;

    fn heard(callsign: &str, age_ms: u64) -> Heard {
        Heard {
            callsign: callsign.to_string(),
            age_ms,
        }
    }

    #[test]
    fn the_first_heard_row_carries_the_macs_caption() {
        let rows = [heard("W6VS", 900), heard("KF5ILA", 9_000)];
        assert_eq!(
            heard_lines(&rows),
            vec![
                "Last heard W6VS · now".to_string(),
                "KF5ILA · 9 s".to_string()
            ]
        );
    }

    #[test]
    fn heard_rows_stop_at_three_and_empty_yields_nothing() {
        let rows = [
            heard("A", 0),
            heard("B", 2_000),
            heard("C", 60_000),
            heard("D", 3_600_000),
        ];
        let lines = heard_lines(&rows);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "C · 1 min");
        assert!(heard_lines(&[]).is_empty());
    }

    #[test]
    fn age_labels_match_the_macs_table() {
        // Verbatim from the Mac's `HeardAge` (Task 6) — the two clients must
        // never disagree about how old a station is.
        assert_eq!(age_label(0), "now");
        assert_eq!(age_label(1_999), "now");
        assert_eq!(age_label(2_000), "2 s");
        assert_eq!(age_label(59_999), "59 s");
        assert_eq!(age_label(60_000), "1 min");
        assert_eq!(age_label(3_599_000), "59 min");
        assert_eq!(age_label(3_600_000), "1 h");
        assert_eq!(age_label(90_000_000), "25 h");
    }
}
