// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The "Saved nodes" section (astar-ac65): favorites + recents with
//! connect-on-click, the row-list rendering of the Mac popover's directory
//! menu (a Favorites section then a Recents section, "label — node" rows).
//! Where the Mac's menu rows prefill the node field, these rows dial
//! directly — the design's connect-on-click. Favorite rows carry a remove
//! (×) like the Mac's favorites manager; removal keeps a dialed node in
//! Recents, matching the Mac's un-favorite semantics. Rendered only while
//! disconnected, like the Mac's menu (it lives in the connect controls).

use iced::widget::{button, column, container, row, text, Space};
use iced::{Alignment, Background, Border, Element, Fill};

use super::status::badge_capsule;
use super::{disclosure_header, surface, NetworkInfo};
use crate::app::Message;
use crate::settings::NodeEntry;
use crate::theme;

/// Everything the favorites UI renders, borrowed from app state.
pub struct Favorites<'a> {
    /// Whether the "Saved nodes" disclosure is expanded.
    pub open: bool,
    /// Whether the node currently in the entry field is a favorite — fills
    /// the dial-row star (see [`super::dial`]).
    pub entry_is_favorite: bool,
    /// The save-favorite editor's label draft; `None` = editor closed.
    pub editor: Option<&'a str>,
    /// Favorited entries, label-sorted.
    pub favorites: Vec<&'a NodeEntry>,
    /// Recently-connected entries, newest first.
    pub recents: Vec<&'a NodeEntry>,
}

/// The "Saved nodes" disclosure: the toggle row, plus the favorites/recents
/// list card when expanded. `network` gates the row badge (astar-9b3e) the
/// same way the picker/status badge are gated — hidden with nothing to
/// switch to.
pub fn section<'a>(f: &Favorites<'a>, network: NetworkInfo<'a>) -> Element<'a, Message> {
    let header = |open| disclosure_header(open, "Saved nodes", Message::FavoritesToggled);
    if !f.open {
        return header(false);
    }

    let mut body = column![].spacing(6);
    if f.favorites.is_empty() && f.recents.is_empty() {
        // Mirrors the Mac's empty-state wording ("No saved nodes yet" plus
        // the settings hint pointing at the ☆).
        body = body.push(
            text("No saved nodes yet — tap the ☆ by the node field to save one.")
                .size(13)
                .color(theme::MUTED),
        );
    }
    if !f.favorites.is_empty() {
        body = body.push(caption("Favorites"));
        for e in f.favorites.iter().copied() {
            body = body.push(favorite_row(e, network));
        }
    }
    if !f.recents.is_empty() {
        body = body.push(caption("Recents"));
        for e in f.recents.iter().copied() {
            body = body.push(recent_row(e, network));
        }
    }

    column![header(true), surface(body).padding(14)]
        .spacing(14)
        .into()
}

/// A section caption inside the card, matching the config cards' uppercased
/// titles (the Mac menu's Section headers).
fn caption(t: &'static str) -> Element<'static, Message> {
    container(
        text(t.to_ascii_uppercase())
            .size(11)
            .color(theme::MUTED)
            .font(theme::FONT_SEMIBOLD),
    )
    .padding([4, 2])
    .into()
}

/// A favorite: ★, "label — node", dial-on-click, and a trailing remove (×).
fn favorite_row<'a>(e: &'a NodeEntry, network: NetworkInfo<'a>) -> Element<'a, Message> {
    row![dial_row(e, star(true), network), remove_button(e)]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
}

/// A recent: a clock mark and the saved name (or the bare node number for an
/// unnamed recent), dial-on-click.
fn recent_row<'a>(e: &'a NodeEntry, network: NetworkInfo<'a>) -> Element<'a, Message> {
    dial_row(e, text("◷").size(15).color(theme::MUTED).into(), network)
}

/// The shared row body: icon + title + (when there's more than one network to
/// pick from, astar-9b3e) the entry's own network badge, full-width,
/// dials the node on click — the same code path as typing the node and
/// pressing Connect.
fn dial_row<'a>(
    e: &'a NodeEntry,
    icon: Element<'a, Message>,
    network: NetworkInfo<'a>,
) -> Element<'a, Message> {
    let mut label_row = row![
        container(icon).width(22).align_x(Alignment::Center),
        text(display(e)).size(14).color(theme::INK),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    if network.available.len() > 1 {
        label_row = label_row.push(badge_capsule(e.network.badge(), false));
    }
    label_row = label_row.push(Space::new().width(Fill));

    button(label_row)
        .on_press(Message::DialFavorite(e.node.clone()))
        .width(Fill)
        .padding([8, 10])
        .style(|_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => theme::BORDER,
                _ => theme::TRACK,
            })),
            text_color: theme::INK,
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        })
        .into()
}

/// The star mark: amber when filled (a favorite), muted outline otherwise.
/// Shared with the dial row's star toggle (see [`super::dial`]).
pub(super) fn star(filled: bool) -> Element<'static, Message> {
    if filled {
        text("★").size(15).color(theme::CONNECTING).into()
    } else {
        text("☆").size(15).color(theme::MUTED).into()
    }
}

/// The row's remove (×): un-favorites the node — kept as a recent if it has
/// been dialed, dropped entirely otherwise (the Mac's semantics).
fn remove_button(e: &NodeEntry) -> Element<'_, Message> {
    button(text("×").size(16))
        .on_press(Message::RemoveFavorite(e.node.clone()))
        .padding([5, 10])
        .style(|_, status| button::Style {
            background: None,
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed => theme::TX,
                _ => theme::MUTED,
            },
            ..button::Style::default()
        })
        .into()
}

/// A row's title: "label — node", collapsing to the bare node number when
/// the label IS the node (unnamed recents, default-labeled favorites).
fn display(e: &NodeEntry) -> String {
    if e.label == e.node {
        e.node.clone()
    } else {
        format!("{} — {}", e.label, e.node)
    }
}
