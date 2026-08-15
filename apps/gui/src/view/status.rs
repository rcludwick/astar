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
use crate::snapshot::{Snapshot, Status};
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

    surface(lines).into()
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
