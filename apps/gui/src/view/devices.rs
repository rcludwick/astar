// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The audio device pickers (astar-335a): "In" (capture) and "Out"
//! (playback), mirroring the Mac Quick Config's device rows. Each is an Iced
//! `pick_list` over the conn-enumerated device names plus a leading
//! "System default" entry (= `None`, the persisted no-preference value).
//! Rendered inside the Quick Config Mic/Speaker cards (astar-f4d9), which own
//! the row labels — this module supplies the picker widget itself.

use iced::widget::{overlay::menu, pick_list};
use iced::{Background, Border, Element, Fill};

use crate::app::Message;
use crate::theme;

/// One entry in a device picker. A dedicated type (rather than raw device
/// names) so "System default" is a real first-class choice, not a magic
/// string that could collide with a device named the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChoice {
    /// Follow the OS default device (`None` in settings and on the conn).
    SystemDefault,
    /// A specific device, by cpal-enumerated name.
    Named(String),
}

impl DeviceChoice {
    /// The settings/conn representation: `None` = system default.
    fn into_option(self) -> Option<String> {
        match self {
            DeviceChoice::SystemDefault => None,
            DeviceChoice::Named(name) => Some(name),
        }
    }

    /// The picker representation of a persisted selection.
    fn from_option(selected: Option<&str>) -> Self {
        match selected {
            None => DeviceChoice::SystemDefault,
            Some(name) => DeviceChoice::Named(name.to_string()),
        }
    }
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceChoice::SystemDefault => f.write_str("System default"),
            DeviceChoice::Named(name) => f.write_str(name),
        }
    }
}

/// A device picker over `devices` with `selected` highlighted (`None` =
/// system default). The caller supplies the row label.
pub(super) fn device_picker(
    devices: &[String],
    selected: Option<&str>,
    msg: fn(Option<String>) -> Message,
) -> Element<'static, Message> {
    let mut options = Vec::with_capacity(devices.len() + 1);
    options.push(DeviceChoice::SystemDefault);
    options.extend(devices.iter().cloned().map(DeviceChoice::Named));

    // NB: an unplugged persisted device still displays by name (pick_list
    // renders the selection's text even when it's absent from `options`);
    // picking anything else simply moves on.
    pick_list(
        options,
        Some(DeviceChoice::from_option(selected)),
        move |choice| msg(choice.into_option()),
    )
    .width(Fill)
    .text_size(14)
    .padding([8, 12])
    .style(picker_style)
    .menu_style(menu_style)
    .into()
}

/// The closed picker: an inset track like the node-entry field, border
/// brightening on hover/open. Shared with the setup switcher (config.rs).
pub(super) fn picker_style(_theme: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let border_color = match status {
        pick_list::Status::Active => theme::BORDER,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => theme::MUTED,
    };
    pick_list::Style {
        text_color: theme::INK,
        placeholder_color: theme::MUTED,
        handle_color: theme::MUTED,
        background: Background::Color(theme::TRACK),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 8.0.into(),
        },
    }
}

/// The open dropdown: a slate surface with the RX-green selection tint the
/// text inputs also use. Shared with the setup switcher (config.rs).
pub(super) fn menu_style(_theme: &iced::Theme) -> menu::Style {
    menu::Style {
        background: Background::Color(theme::SURFACE),
        border: Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: theme::INK,
        selected_text_color: theme::INK,
        selected_background: Background::Color(theme::dim(theme::RX)),
        shadow: iced::Shadow::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_default_maps_to_none_and_back() {
        assert_eq!(DeviceChoice::SystemDefault.into_option(), None);
        assert_eq!(DeviceChoice::from_option(None), DeviceChoice::SystemDefault);
        assert_eq!(DeviceChoice::SystemDefault.to_string(), "System default");
    }

    #[test]
    fn named_device_round_trips_by_name() {
        let choice = DeviceChoice::from_option(Some("USB Audio CODEC"));
        assert_eq!(choice.to_string(), "USB Audio CODEC");
        assert_eq!(choice.into_option().as_deref(), Some("USB Audio CODEC"));
    }
}
