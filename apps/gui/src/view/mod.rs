// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The main screen view: renders a [`Snapshot`] into a themed layout.
//!
//! Layout (top → bottom): a header, a status card ([`status`]), the dial /
//! call controls ([`dial`]), the TX/RX indicators + VU bars ([`meters`]), the
//! "Dialpad" disclosure ([`dial::dialpad_section`]), and the "Quick settings"
//! disclosure ([`config`], with the device pickers from
//! [`devices`] inside its Mic/Speaker cards). The stack scrolls (like the Mac
//! popover's middle) so an expanded Quick Config never pushes controls off a
//! short window. Split per the 2026-06-29 design: one module per screen
//! region, shared style helpers here.

mod config;
mod devices;
mod dial;
mod favorites;
mod meters;
mod status;

pub use config::QuickConfig;
pub use dial::Dialpad;
pub use favorites::Favorites;

use std::sync::LazyLock;

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Fill};

use crate::app::Message;
use crate::network::Network;
use crate::snapshot::{Snapshot, Status};
use crate::theme;

/// The network selection state the dial controls, status badge, and
/// favorites rows all read (astar-9b3e): the current pick, and what's
/// available right now. `available.len() <= 1` is the master gate — every
/// picker/badge stays hidden so the UI is pixel-identical to pre-9b3e until
/// the engine reports reflector capability (iax-b3d7).
#[derive(Clone, Copy)]
pub struct NetworkInfo<'a> {
    /// The active pick — drives the dial placeholder, the dialpad gate, and
    /// (while connected) the status badge.
    pub selected: Network,
    /// The networks the engine can actually drive right now.
    pub available: &'a [Network],
    /// The PERSISTED M17 callsign (M17 Task 10) — gates whether the callsign
    /// prompt shows at all (mirrors the Mac's `needsM17Callsign`, which reads
    /// `session.m17Callsign`, never the local draft, so the field can't
    /// vanish out from under the user mid-keystroke). Irrelevant for every
    /// other network, but rides along here rather than as its own `view()`
    /// parameter — it's part of the same "what does the network picker need
    /// to render" bundle.
    pub m17_callsign: &'a str,
    /// The M17 callsign field's local, unpersisted draft (M17 Task 10 fix) —
    /// what the field actually displays/edits, and (OR'd with
    /// [`Self::m17_callsign`]) part of the Connect button's M17 validity
    /// gate, mirroring the Mac's `needsM17CallsignToConnect` checking both
    /// the committed value and the draft.
    pub m17_callsign_draft: &'a str,
}

/// The version this build reports in the footer, derived from the crate's own
/// `version` (inherited from the workspace) instead of being written out a
/// second time — a literal here goes stale the first time the workspace
/// version is bumped. Cargo demands SemVer's hyphenated pre-release form
/// (`0.1.0-beta`); the hyphen is dropped for display so this matches the macOS
/// bundle's MARKETING_VERSION (`0.1.0beta`) exactly.
static APP_VERSION: LazyLock<String> = LazyLock::new(|| env!("CARGO_PKG_VERSION").replace('-', ""));

/// Build the full screen for `snap`. `node_entry` is the current text in the
/// node-entry field; `favorites` feeds the dial-row star and the "Saved
/// nodes" section; `dialpad` the DTMF keypad section; `config` the Quick
/// Config section; `network` the picker/badge state (astar-9b3e), including
/// the M17 callsign. `tray_hint` is the Linux no-tray-host guidance
/// (astar-22cf) — a dim footer note when set.
pub fn view<'a>(
    snap: &'a Snapshot,
    node_entry: &'a str,
    favorites: Favorites<'a>,
    dialpad: Dialpad<'a>,
    config: QuickConfig<'a>,
    network: NetworkInfo<'a>,
    tray_hint: Option<&'static str>,
) -> Element<'a, Message> {
    let mut content = column![
        header(),
        status::status_card(snap, network),
        dial::controls(snap, node_entry, &favorites, network),
    ]
    .spacing(18);

    // The saved-node list only matters while idle — like the Mac, whose
    // directory menu lives inside the (idle-only) connect controls.
    if snap.status == Status::Disconnected {
        content = content.push(favorites::section(&favorites, network));
    }

    content = content
        .push(meters::indicators(snap))
        .push(meters::vu_bars(snap));

    // Dialpad sits right above Quick settings, like the Mac popover — an
    // AllStar-only concern for now (astar-9b3e): hidden entirely for
    // networks that don't show it (reflectors bring their own sections
    // later).
    if network.selected.shows_dialpad() {
        content = content.push(dial::dialpad_section(snap, dialpad));
    }
    let mut content = content.push(config::section(config));

    // The tray-fallback guidance (Linux without a StatusNotifier host): a
    // quiet footer, informative but never in the way.
    if let Some(hint) = tray_hint {
        content = content.push(text(hint).size(12).color(theme::MUTED));
    }

    // The running build's version, so a user can say what they are on without
    // hunting for it. Mirrors the Mac popover's footer.
    content = content.push(
        text(format!("astar {}", APP_VERSION.as_str()))
            .size(11)
            .color(theme::MUTED),
    );

    let content = content.max_width(560).width(Fill);

    container(
        scrollable(container(content).padding(28).width(Fill).center_x(Fill))
            .width(Fill)
            .height(Fill)
            .style(scroll_style),
    )
    .width(Fill)
    .height(Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(theme::BG)),
        text_color: Some(theme::INK),
        ..container::Style::default()
    })
    .into()
}

/// Slate scrollbars over the app background (the default theme's rails are
/// tuned for light palettes).
fn scroll_style(_theme: &iced::Theme, _status: scrollable::Status) -> scrollable::Style {
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(theme::BORDER),
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(theme::SURFACE),
            border: Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: iced::Shadow::default(),
            icon: theme::MUTED,
        },
    }
}

/// "astar" wordmark + tagline.
fn header() -> Element<'static, Message> {
    column![
        text("astar")
            .size(34)
            .color(theme::INK)
            .font(theme::FONT_SEMIBOLD),
        text("AllStarLink station").size(13).color(theme::MUTED),
    ]
    .spacing(2)
    .into()
}

/// A rounded slate surface used for the cards.
fn surface<'a>(inner: impl Into<Element<'a, Message>>) -> container::Container<'a, Message> {
    container(inner)
        .padding(18)
        .width(Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(theme::SURFACE)),
            border: Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 12.0.into(),
            },
            text_color: Some(theme::INK),
            ..container::Style::default()
        })
}

/// A clickable disclosure row with a chevron — the Iced stand-in for the Mac
/// popover's `DisclosureGroup` label. Shared by "Quick settings" and
/// "Saved nodes".
fn disclosure_header(open: bool, label: &'static str, msg: Message) -> Element<'static, Message> {
    let chevron = if open { "▾" } else { "▸" };
    button(
        row![
            text(chevron).size(14).color(theme::MUTED),
            text(label)
                .size(15)
                .color(theme::INK)
                .font(theme::FONT_MEDIUM),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(msg)
    .padding([6, 2])
    .style(|_, status| button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => theme::INK,
            _ => theme::MUTED,
        },
        ..button::Style::default()
    })
    .into()
}

/// A tooltip bubble: a small slate card. Shared by the Quick Config tips and
/// the status card's wideband badge.
fn tip_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme::SURFACE)),
        border: Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: Some(theme::INK),
        ..container::Style::default()
    }
}

/// A small filled circle used as a status dot.
fn dot(color: Color, size: f32) -> Element<'static, Message> {
    container(Space::new())
        .width(size)
        .height(size)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                radius: (size / 2.0).into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}
