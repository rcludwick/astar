// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The dial / call controls (astar-e461), mirroring the Mac app's grouping:
//! disconnected shows the node entry + favorite star + Connect (plus the
//! inline save-favorite editor when open); once a call is in flight the card
//! swaps to hold-to-talk PTT + Disconnect.
//!
//! Also home to the DTMF dialpad (astar-75fc), the Mac popover's "Dialpad"
//! disclosure: dual-mode — idle taps append into the node field above; when
//! connected each tap sends one live tone and a read-only readout shows the
//! tones sent this call.

use iced::widget::{button, column, container, mouse_area, row, text, text_input, tooltip};
use iced::{Alignment, Background, Border, Color, Element, Fill};

use super::{surface, tip_style, Favorites, NetworkInfo};
use crate::app::Message;
use crate::dial_target::DialTarget;
use crate::m17_dial::parse_m17_dial;
use crate::network::Network;
use crate::snapshot::{Snapshot, Status};
use crate::theme;

/// The interactive controls card for `snap`'s state. `network.m17_callsign`
/// is only read while disconnected; in-call the M17 session is already
/// running.
pub fn controls<'a>(
    snap: &'a Snapshot,
    node_entry: &'a str,
    fav: &Favorites<'a>,
    network: NetworkInfo<'a>,
) -> Element<'a, Message> {
    match snap.status {
        Status::Disconnected => connect_controls(node_entry, fav, network),
        Status::Connecting | Status::Connected => call_controls(snap),
    }
}

/// Idle: the network picker (astar-9b3e, when there's more than one to pick
/// from) above the smart dial entry (node number OR host:port, astar-3939) +
/// favorite star + Connect (WT-primary dial on submit or click), plus the
/// inline save-favorite editor when open. Connect stays disabled until the
/// text parses as a dialable target (mirrors the Mac) — for M17 (M17 Task 10)
/// that also requires a non-empty callsign, prompted for inline above the
/// dial row while `network.m17_callsign` is empty (mirrors the Mac's
/// `needsM17Callsign` prompt).
fn connect_controls<'a>(
    node_entry: &'a str,
    fav: &Favorites<'a>,
    network: NetworkInfo<'a>,
) -> Element<'a, Message> {
    let entry = text_input(network.selected.dial_placeholder(), node_entry)
        .on_input(Message::NodeEntryChanged)
        .on_submit(Message::Connect)
        .padding(10)
        .style(input_style);

    // The Mac field's `.help` tooltip — the old Advanced-options guidance.
    let entry = tooltip(
        entry,
        container(
            text(
                "A node number dials through the AllStarLink registrar. \
                 An IP or hostname (with optional :port, default 4569) \
                 dials that address directly — for a node that isn’t \
                 reachable at its published address (e.g. your own \
                 node on localhost).",
            )
            .size(13)
            .color(theme::INK),
        )
        .padding([8, 12])
        .max_width(320)
        .style(tip_style),
        tooltip::Position::Bottom,
    )
    .gap(6);

    // Connect validity is per-network (M17 Task 10): M17's grammar and
    // callsign requirement are entirely separate from the AllStar/Hamlink
    // `DialTarget` path. The callsign check reads EITHER the committed value
    // OR the still-uncommitted draft (mirrors the Mac's
    // `needsM17CallsignToConnect`) — Connect commits the draft right before
    // dialing, so the button must not stay disabled just because the user
    // hasn't hit Enter in the callsign field yet.
    let dialable = if network.selected == Network::M17 {
        let has_callsign = !network.m17_callsign.trim().is_empty()
            || !network.m17_callsign_draft.trim().is_empty();
        parse_m17_dial(node_entry).is_some() && has_callsign
    } else {
        DialTarget::parse(node_entry).is_some()
    };
    let mut content = column![].spacing(12);

    // The network picker (astar-9b3e): hidden entirely with nothing to
    // switch to, so today's AllStar-only UI is unchanged.
    if network.available.len() > 1 {
        content = content.push(network_picker(network));
    }

    // The M17 callsign prompt (M17 Task 10): only for the M17 network, and
    // only until a callsign is COMMITTED — mirrors the Mac's
    // `needsM17Callsign`, which gates on the persisted value only (never the
    // draft), so the field can't vanish out from under the user mid-keystroke.
    if network.selected == Network::M17 && network.m17_callsign.trim().is_empty() {
        content = content.push(m17_callsign_field(network.m17_callsign_draft));
    }

    content = content.push(
        row![
            container(entry).width(Fill),
            star_button(node_entry, fav.entry_is_favorite),
            pill_button("Connect", theme::RX, dialable.then_some(Message::Connect))
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    );

    if let Some(draft) = fav.editor {
        content = content.push(favorite_editor(node_entry, draft));
    }

    surface(content).into()
}

/// The M17 callsign prompt (M17 Task 10): M17 transmits it verbatim in every
/// frame, so it's asked for once, up front, above the dial row — mirrors the
/// Mac's `m17CallsignField`. Uppercases as you type (the message handler
/// normalizes the local draft); Enter commits the draft into the persisted
/// settings (the Connect dispatch commits it too, right before dialing, so
/// Enter here is a convenience, not a requirement).
fn m17_callsign_field<'a>(draft: &'a str) -> Element<'a, Message> {
    let entry = text_input("Your callsign", draft)
        .on_input(Message::M17CallsignChanged)
        .on_submit(Message::M17CallsignCommit)
        .padding(10)
        .style(input_style);

    column![
        entry,
        text("M17 transmits your callsign.")
            .size(12)
            .color(theme::MUTED),
    ]
    .spacing(4)
    .into()
}

/// The network picker (astar-9b3e): one pill segment per available network,
/// reusing [`pill_button`]'s styling family — filled accent when selected, the
/// same dim track color the Cancel button uses otherwise. Every segment stays
/// clickable, including the active one (re-picking it is a harmless no-op).
fn network_picker(network: NetworkInfo) -> Element<'static, Message> {
    let mut row = row![].spacing(8);
    for n in network.available.iter().copied() {
        let accent = if n == network.selected {
            theme::ACCENT
        } else {
            theme::TRACK
        };
        row = row.push(pill_button(
            n.display_name(),
            accent,
            Some(Message::NetworkPicked(n)),
        ));
    }
    row.into()
}

/// The dial-row star (the Mac's `favoriteToggle`): filled while the entered
/// node is a favorite; press to open the save editor, or to un-favorite.
/// Inert while the field is empty (the Mac disables it there too).
fn star_button(node_entry: &str, is_favorite: bool) -> Element<'static, Message> {
    let mut star = button(super::favorites::star(is_favorite))
        .padding([8, 6])
        .style(|_, _| button::Style {
            background: None,
            ..button::Style::default()
        });
    if !node_entry.trim().is_empty() {
        star = star.on_press(Message::FavoriteToggle);
    }
    star.into()
}

/// The inline save-favorite editor (the Mac's popover, flattened into the
/// card): name the favorite (callsign or label) and save.
fn favorite_editor<'a>(node_entry: &'a str, draft: &'a str) -> Element<'a, Message> {
    let label = text_input("Label (callsign or name)", draft)
        .on_input(Message::FavoriteLabelChanged)
        .on_submit(Message::FavoriteSave)
        .padding(10)
        .style(input_style);

    column![
        text(format!("Save favorite for node {}", node_entry.trim()))
            .size(14)
            .color(theme::INK)
            .font(theme::FONT_MEDIUM),
        row![
            container(label).width(Fill),
            pill_button("Cancel", theme::TRACK, Some(Message::FavoriteCancel)),
            pill_button("Save", theme::ACCENT, Some(Message::FavoriteSave)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(8)
    .into()
}

/// The dark inset text-field style shared by the node entry, the
/// favorite-label editor, and the new-setup name editor (config.rs).
pub(super) fn input_style(_theme: &iced::Theme, _status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(theme::TRACK),
        border: Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        icon: theme::MUTED,
        placeholder: theme::MUTED,
        value: theme::INK,
        selection: theme::dim(theme::RX),
    }
}

/// In-call: hold-to-talk PTT (live once answered) + Disconnect.
fn call_controls(snap: &Snapshot) -> Element<'_, Message> {
    surface(
        row![
            ptt_button(snap),
            pill_button("Disconnect", theme::TX, Some(Message::Disconnect)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    )
    .into()
}

/// A rounded, filled accent button. `None` renders it disabled (dimmed and
/// inert — the Connect button while the dial text doesn't parse).
fn pill_button(label: &str, accent: Color, msg: Option<Message>) -> Element<'static, Message> {
    // No explicit text color — the style's `text_color` carries it, so the
    // disabled state can dim the label too.
    let mut b = button(text(label.to_string()).size(15))
        .padding([10, 18])
        .style(move |_, status| {
            let (bg, fg) = match status {
                button::Status::Disabled => (theme::dim(accent), theme::MUTED),
                button::Status::Hovered | button::Status::Pressed => {
                    (Color { a: 0.85, ..accent }, theme::INK)
                }
                _ => (accent, theme::INK),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: fg,
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            }
        });
    if let Some(msg) = msg {
        b = b.on_press(msg);
    }
    b.into()
}

// -- dialpad (astar-75fc / astar-7d21) ----------------------------------------

/// Everything the dialpad renders, borrowed from app state.
#[derive(Clone, Copy)]
pub struct Dialpad<'a> {
    /// Whether the disclosure is expanded.
    pub open: bool,
    /// The DTMF command being composed (connected mode, editable until Send).
    pub command: &'a str,
    /// The command currently playing out, with how many digits are already
    /// sent — `Some` locks the bar into the progress readout + Stop.
    pub playing: Option<(&'a str, u32)>,
    /// Commands sent this call, oldest first (the subdued history line).
    pub history: &'a [String],
}

/// The full 16-key grid: phone layout with the A-D column on the right,
/// mirroring the Mac's `dialpadKeypad`.
const KEYS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// The "Dialpad" disclosure: the toggle row, plus (when expanded) one card
/// holding the connected-mode command bar (astar-7d21), the per-call history
/// line, the 4×4 keypad, and a mode hint.
pub fn dialpad_section<'a>(snap: &Snapshot, dialpad: Dialpad<'a>) -> Element<'a, Message> {
    let header = |open| super::disclosure_header(open, "Dialpad", Message::DialpadToggled);
    if !dialpad.open {
        return header(false);
    }

    // "In call" is dialing-or-answered, like the Mac's `isInCall` — the
    // command bar and hint swap to connected mode as soon as a dial starts.
    let in_call = snap.status != Status::Disconnected;

    let mut card = column![].spacing(12);
    if in_call {
        card = card.push(command_bar(snap, dialpad));
        if !dialpad.history.is_empty() {
            card = card.push(
                text(format!("Sent: {}", dialpad.history.join(" · ")))
                    .size(12)
                    .font(iced::Font::MONOSPACE)
                    .color(theme::MUTED),
            );
        }
    }
    card = card.push(keypad(snap, dialpad.playing.is_some())).push(
        text(if in_call {
            "Compose a command, then Send plays it as tones (e.g. *3<node>)."
        } else {
            "Tap to type into the node number above, or use your keyboard."
        })
        .size(12)
        .color(theme::MUTED),
    );

    column![header(true), surface(card)].spacing(14).into()
}

/// Connected-mode command bar (astar-7d21): an editable compose field with
/// Clear + Send while idle; a locked progress readout (played digits dimmed)
/// with Stop while the sequence plays. Enter in the field = Send; the playing
/// readout has no submit at all (Enter inert, per the spec).
fn command_bar<'a>(snap: &Snapshot, dialpad: Dialpad<'a>) -> Element<'a, Message> {
    if let Some((playing, _)) = dialpad.playing {
        // Progress render: the snapshot's live count wins over the (possibly
        // stale) boot-seeded one, clamped to the command length.
        let played = (snap.dtmf_played as usize).min(playing.chars().count());
        let cut = playing
            .char_indices()
            .nth(played)
            .map_or(playing.len(), |(i, _)| i);
        let readout = container(row![
            text(playing[..cut].to_string())
                .size(17)
                .font(iced::Font::MONOSPACE)
                .color(theme::MUTED),
            text(playing[cut..].to_string())
                .size(17)
                .font(iced::Font::MONOSPACE)
                .color(theme::INK),
        ])
        .padding([8, 10])
        .width(Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(theme::TRACK)),
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });

        row![
            readout,
            pill_button("Stop", theme::TX, Some(Message::DialpadStop)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    } else {
        let entry = text_input("Command (e.g. *3 node)", dialpad.command)
            .on_input(Message::DialpadComposeChanged)
            .on_submit(Message::DialpadSend)
            .font(iced::Font::MONOSPACE)
            .padding([8, 10])
            .style(input_style);

        let sendable = !dialpad.command.is_empty() && snap.status == Status::Connected;
        row![
            entry,
            readout_button("✕", !dialpad.command.is_empty(), Message::DialpadClear),
            pill_button(
                "Send",
                theme::ACCENT,
                sendable.then_some(Message::DialpadSend)
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    }
}

/// A borderless readout edit button (backspace / clear), inert while the
/// readout is empty — mirrors the Mac's disabled borderless buttons.
fn readout_button(glyph: &'static str, enabled: bool, msg: Message) -> Element<'static, Message> {
    let mut b = button(text(glyph).size(16))
        .padding([6, 4])
        .style(move |_, status| button::Style {
            background: None,
            text_color: match status {
                _ if !enabled => theme::dim(theme::MUTED),
                button::Status::Hovered | button::Status::Pressed => theme::INK,
                _ => theme::MUTED,
            },
            ..button::Style::default()
        });
    if enabled {
        b = b.on_press(msg);
    }
    b.into()
}

/// The 4×4 key grid, each key an even share of the row's width. Keys go
/// inert while a sequence plays (`playing`).
fn keypad(snap: &Snapshot, playing: bool) -> Element<'static, Message> {
    let mut grid = column![].spacing(8);
    for row_keys in KEYS {
        let mut r = row![].spacing(8);
        for key in row_keys {
            r = r.push(dialpad_key(snap, key, playing));
        }
        grid = grid.push(r);
    }
    grid.into()
}

/// Whether a given key is live (the Mac's `dialpadKeyEnabled`): connected —
/// all 16 compose, unless a sequence is playing (taps never interrupt a
/// playing command, astar-7d21); mid-dial — nothing; idle — the 12
/// node-string keys feed the node field, A-D are DTMF-only.
fn key_enabled(snap: &Snapshot, key: char, playing: bool) -> bool {
    match snap.status {
        Status::Connected => !playing,
        Status::Connecting => false,
        Status::Disconnected => !matches!(key, 'A'..='D'),
    }
}

/// A single tactile dialpad key: a rounded, filled cell that brightens under
/// the pointer and flashes accent-blue while pressed (the Mac's tap flash).
/// Disabled keys dim, like the Mac's `.opacity(0.4)`.
fn dialpad_key(snap: &Snapshot, key: char, playing: bool) -> Element<'static, Message> {
    let enabled = key_enabled(snap, key, playing);

    // The key face: text fonts draw `*` as a small, raised glyph, so the
    // asterisk key renders the geometrically centered asterisk-operator
    // (U+2217) a touch larger instead — the Mac swaps in the SF-symbol
    // asterisk for the same reason (astar-f3e9). Every other key (including
    // `#`) stays a text glyph.
    let face = if key == '*' {
        text("∗").size(24).font(theme::FONT_SEMIBOLD)
    } else {
        text(key.to_string()).size(19).font(theme::FONT_MEDIUM)
    };
    let face = face
        .color(if enabled { theme::INK } else { theme::MUTED })
        .width(Fill)
        .align_x(iced::alignment::Horizontal::Center);

    let mut b = button(container(face).width(Fill).center_y(40))
        .padding(0)
        .width(Fill)
        .style(move |_, status| {
            let bg = match status {
                button::Status::Pressed => theme::ACCENT,
                button::Status::Hovered => theme::BORDER,
                button::Status::Active => theme::TRACK,
                button::Status::Disabled => Color {
                    a: 0.45,
                    ..theme::TRACK
                },
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: theme::INK,
                border: Border {
                    color: theme::dim(theme::BORDER),
                    width: 0.5,
                    radius: 9.0.into(),
                },
                ..button::Style::default()
            }
        });
    if enabled {
        b = b.on_press(Message::Dialpad(key));
    }
    b.into()
}

/// Hold-to-talk: press (or hold Space) to key, release to unkey — mirrors the
/// Mac button. Filled red + "ON AIR" while keyed; a dark track with a red
/// outline when idle; dimmed and inert until the call is answered.
fn ptt_button(snap: &Snapshot) -> Element<'static, Message> {
    let answered = snap.status == Status::Connected;
    let keyed = snap.transmitting;

    let label = if keyed {
        "ON AIR"
    } else {
        "Hold to Talk  (Space)"
    };
    let fg = if keyed {
        theme::BG
    } else if answered {
        theme::TX
    } else {
        theme::MUTED
    };

    let face = container(
        text(label)
            .size(16)
            .font(theme::FONT_MEDIUM)
            .color(fg)
            .width(Fill)
            .align_x(iced::alignment::Horizontal::Center),
    )
    .padding([12, 0])
    .width(Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(if keyed {
            theme::TX
        } else {
            theme::TRACK
        })),
        border: Border {
            color: if answered {
                theme::TX
            } else {
                theme::dim(theme::TX)
            },
            width: 1.5,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    });

    if answered {
        // Release anywhere (including dragging off the button) always unkeys —
        // a radio must never be left stuck transmitting.
        mouse_area(face)
            .on_press(Message::Ptt(true))
            .on_release(Message::Ptt(false))
            .on_exit(Message::Ptt(false))
            .into()
    } else {
        face.into()
    }
}
