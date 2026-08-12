// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The Quick Config panel (astar-f4d9), mirroring the Mac's
//! `QuickConfigView`: the Config (setup switcher, astar-80ae) →
//! Station → Mic → Speaker → VOX cards (rows within each card in TX
//! processing order) plus "Save changes". Reached like the Mac popover's
//! "Quick settings" disclosure — a toggle row on the main page that expands
//! in place (the page scrolls when the window is short).

use iced::widget::{
    button, column, container, pick_list, row, slider, text, text_input, toggler, tooltip, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Fill};

use super::devices::{device_picker, menu_style, picker_style};
use super::{surface, tip_style};
use crate::app::Message;
use crate::settings::{AudioSettings, M17AudioOverrides, Setup, NONE_SETUP_ID};
use crate::theme;

/// Everything the panel renders, borrowed from app state.
#[derive(Clone, Copy)]
pub struct QuickConfig<'a> {
    /// Whether the disclosure is expanded.
    pub open: bool,
    /// Capture device names for the "In" picker.
    pub inputs: &'a [String],
    /// Playback device names for the "Out" picker.
    pub outputs: &'a [String],
    /// The live audio settings every control reflects.
    pub audio: &'a AudioSettings,
    /// The persisted M17 TX-processing override (astar-5d8e) — what the four
    /// TX-processing rows in the Mic card read/write instead of `audio`
    /// while `m17_context` is true.
    pub m17_audio: &'a M17AudioOverrides,
    /// Whether the M17 network is selected or its override is currently live
    /// (astar-5d8e) — see `crate::app::State::m17_context`. Binds noise
    /// reduction / voice compression / strength / TX trim to `m17_audio`.
    /// Used to tag the Mic card "M17" and show a "not recommended" caption
    /// until astar-unwarn retired both: Rob's on-air A/B testing found his
    /// AllStar-tuned compression chain actually improved his M17 audio over
    /// the raw mic.
    pub m17_context: bool,
    /// Every saved Setup, in user order (the switcher's entries after None).
    pub setups: &'a [Setup],
    /// The selected setup id ([`NONE_SETUP_ID`] or unknown ids show as None).
    pub selected_setup: Option<&'a str>,
    /// The "New setup…" editor's name draft; `None` = editor closed.
    pub setup_editor: Option<&'a str>,
}

/// Width of the fixed label column, so sliders/pickers align across cards
/// (the Mac's `labelWidth` — sized for "Hang Timeout").
const LABEL_W: f32 = 110.0;

/// The Mac's TX Gain help text, verbatim.
const TX_GAIN_TIP: &str =
    "How loud you sound on the air — applied after compression. 100% = no change.";

/// The Mac's mic-bandwidth help text, verbatim (astar-17b8). The input
/// source caps wideband quality regardless of the negotiated codec.
const MIC_BANDWIDTH_TIP: &str = "Wideband (slin16) is only as wide as this source: radio and \
     USB-CODEC interfaces such as the UCI150 pass about 300–3400 Hz no matter the codec. Use a \
     direct microphone to send true wideband audio.";

/// The "Quick settings" disclosure: the toggle row, plus the config cards
/// when expanded.
pub fn section(config: QuickConfig<'_>) -> Element<'_, Message> {
    let header =
        |open| super::disclosure_header(open, "Quick settings", Message::QuickConfigToggled);
    if !config.open {
        return header(false);
    }
    let mut body = column![
        header(true),
        setup_card(&config),
        station_card(config.audio),
        mic_card(&config),
        speaker_card(&config),
        vox_card(config.audio),
    ]
    .spacing(14);

    // "Save changes to <name>" sits under the cards, like the Mac — only
    // when the applied setup is a real (editable) one; None can't hold state.
    if let Some(s) = selected_editable_setup(&config) {
        body = body.push(save_button(
            format!("Save changes to {}", s.name),
            Message::SetupSaveCurrent,
        ));
    }
    body.push(save_button("Save changes".to_string(), Message::SaveAudio))
        .into()
}

/// The applied setup, if it's a real (editable) one — the Mac's
/// `selectedEditableSetup`.
fn selected_editable_setup<'a>(config: &QuickConfig<'a>) -> Option<&'a Setup> {
    let id = config.selected_setup?;
    if id == NONE_SETUP_ID {
        return None;
    }
    config.setups.iter().find(|s| s.id == id)
}

// -- the setup switcher (astar-80ae) -------------------------------------------

/// One entry in the setup switcher. A dedicated type (like
/// [`super::devices::DeviceChoice`]) so the built-in None is a first-class
/// choice rather than a magic name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SetupChoice {
    /// The built-in "None (system default)" — [`NONE_SETUP_ID`].
    None,
    /// A saved Setup, by id, displaying its name.
    Named { id: String, name: String },
}

impl SetupChoice {
    /// The id the update loop applies.
    fn id(&self) -> String {
        match self {
            SetupChoice::None => NONE_SETUP_ID.to_string(),
            SetupChoice::Named { id, .. } => id.clone(),
        }
    }

    /// The picker representation of the persisted selection: the None entry
    /// for no selection, the explicit None pick, or a stale unknown id.
    fn from_selection(setups: &[Setup], selected: Option<&str>) -> Self {
        selected
            .filter(|id| *id != NONE_SETUP_ID)
            .and_then(|id| setups.iter().find(|s| s.id == id))
            .map_or(SetupChoice::None, |s| SetupChoice::Named {
                id: s.id.clone(),
                name: s.name.clone(),
            })
    }
}

impl std::fmt::Display for SetupChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupChoice::None => f.write_str("None (system default)"),
            SetupChoice::Named { name, .. } => f.write_str(name),
        }
    }
}

/// The Config card: the setup switcher (the Mac's `setupRow` picker), plus —
/// since gui-rs has no Settings pane yet — the manage actions the Mac keeps
/// in its `SetupsView`: "New setup…" (with an inline name editor) and Delete.
fn setup_card<'a>(config: &QuickConfig<'a>) -> Element<'a, Message> {
    let mut options = Vec::with_capacity(config.setups.len() + 1);
    options.push(SetupChoice::None);
    options.extend(config.setups.iter().map(|s| SetupChoice::Named {
        id: s.id.clone(),
        name: s.name.clone(),
    }));
    let selected = SetupChoice::from_selection(config.setups, config.selected_setup);

    let picker = pick_list(options, Some(selected), |choice| {
        Message::SetupSelected(choice.id())
    })
    .width(Fill)
    .text_size(14)
    .padding([8, 12])
    .style(picker_style)
    .menu_style(menu_style);

    let mut rows = column![row![label("Config"), picker]
        .spacing(10)
        .align_y(Alignment::Center)]
    .spacing(10);

    if let Some(draft) = config.setup_editor {
        rows = rows.push(setup_editor(draft));
    } else {
        let mut actions = row![small_button("New setup…", theme::INK, Message::SetupNew)]
            .spacing(8)
            .align_y(Alignment::Center);
        if selected_editable_setup(config).is_some() {
            actions = actions.push(Space::new().width(Fill)).push(small_button(
                "Delete",
                theme::TX,
                Message::SetupDelete,
            ));
        }
        rows = rows.push(actions);
    }

    card("Config", rows)
}

/// The inline new-setup name editor (the favorites editor idiom): name the
/// rig, save snapshots the current state under it.
fn setup_editor(draft: &str) -> Element<'_, Message> {
    let name = text_input("Setup name (e.g. UCI150 desk)", draft)
        .on_input(Message::SetupNameChanged)
        .on_submit(Message::SetupCreate)
        .size(14)
        .padding([8, 12])
        .style(super::dial::input_style);
    row![
        name,
        small_button("Cancel", theme::MUTED, Message::SetupCreateCancel),
        small_button("Save", theme::INK, Message::SetupCreate),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// A small bordered action button (New setup… / Delete / the editor's pair);
/// `tint` colors the label so Delete can read as destructive.
fn small_button(label: &'static str, tint: Color, msg: Message) -> Element<'static, Message> {
    button(text(label).size(13).color(tint))
        .on_press(msg)
        .padding([6, 12])
        .style(|_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => theme::SURFACE,
                _ => theme::TRACK,
            })),
            text_color: theme::INK,
            border: Border {
                color: theme::BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..button::Style::default()
        })
        .into()
}

// -- cards -------------------------------------------------------------------

/// Duplex is a station behavior, not part of either audio chain — it sits
/// above both so the Mic → Speaker cards read as one continuous
/// input → output audio path (mirrors the Mac comment).
fn station_card(audio: &AudioSettings) -> Element<'static, Message> {
    card(
        "Station",
        column![toggle_row(
            "Full duplex",
            audio.full_duplex,
            Message::FullDuplex
        )]
        .spacing(10),
    )
}

/// Input side: device on top, then rows following the TX processing chain
/// top-to-bottom: mic gain → noise reduction → compression → TX gain (final
/// stage). No mic-profile row yet — that's astar-6193, phase 2.
///
/// The five TX-processing rows (mic level, noise reduction, voice
/// compression, its strength, TX trim) read from `config.m17_audio` instead
/// of `config.audio` while `config.m17_context` is true (astar-5d8e; mic
/// level joined at astar-m17defaults) — M17 gets its own tuned profile,
/// edited here without disturbing what AllStar is tuned to. The messages
/// fired are the same either way; `update` decides which struct they land
/// in.
///
/// No "M17" badge or "not recommended" caption render here (astar-unwarn
/// retired both): Rob's on-air A/B testing found his AllStar-tuned
/// compression chain actually improved his M17 audio over the raw mic — the
/// override mechanism above stays, only the discouraging UI is gone.
fn mic_card<'a>(config: &QuickConfig<'a>) -> Element<'a, Message> {
    let audio = config.audio;
    let m17 = config.m17_context;
    let input_gain = if m17 {
        config.m17_audio.input_gain
    } else {
        audio.input_gain
    };
    let noise_reduction = if m17 {
        config.m17_audio.noise_reduction
    } else {
        audio.noise_reduction
    };
    let compression = if m17 {
        config.m17_audio.compression
    } else {
        audio.compression
    };
    let compression_level = if m17 {
        config.m17_audio.compression_level
    } else {
        audio.compression_level
    };
    let tx_trim = if m17 {
        config.m17_audio.tx_trim
    } else {
        audio.tx_trim
    };

    let mut rows = column![
        tooltip(
            picker_row(
                "In",
                config.inputs,
                audio.input.as_deref(),
                Message::InputDevice
            ),
            container(text(MIC_BANDWIDTH_TIP).size(13).color(theme::INK))
                .padding([8, 12])
                .max_width(320)
                .style(tip_style),
            tooltip::Position::Bottom,
        )
        .gap(6),
        gain_row("Mic Level", theme::TX, input_gain, Message::InputGain),
        toggle_row("Noise reduction", noise_reduction, Message::NoiseReduction),
        toggle_row("Voice compression", compression, Message::Compression),
    ]
    .spacing(10);

    // "Strength" only makes sense with the compressor on (Mac parity).
    if compression {
        rows = rows.push(sub_slider_row(
            "Strength",
            slider(0.0..=1.0, compression_level, Message::CompressionLevel)
                .step(0.01_f32)
                .on_release(Message::SaveAudio)
                .style(slider_style(theme::ACCENT)),
            percent(compression_level),
        ));
    }

    // TX Gain (internally tx_trim): the always-on final TX gain stage,
    // applied after compression (100% = unity) — set apart with extra
    // breathing room, like the Mac's `.padding(.top, 12)`.
    rows = rows.push(Space::new().height(2));
    rows = rows.push(
        tooltip(
            gain_row("TX Gain", theme::TX, tx_trim, Message::TxTrim),
            container(text(TX_GAIN_TIP).size(13).color(theme::INK))
                .padding([8, 12])
                .max_width(320)
                .style(tip_style),
            tooltip::Position::Top,
        )
        .gap(6),
    );

    card("Mic", rows)
}

/// Output side: device on top, then volume (100%-400%, floored at unity —
/// no UI attenuation; the half-duplex mute still writes 0 straight to the
/// station, unaffected by this floor), then RX compression (automatic
/// leveling of the received audio, iax-a4e7) — mirroring how the Mic card's
/// compression row sits under its gain slider. Shared across networks: no
/// M17 override, unlike the Mic card's TX-processing rows.
/// (RX noise reduction — denoising the speaker path like the mic path —
/// joins this card via astar-70b9; it slots in right after "Vol".)
fn speaker_card<'a>(config: &QuickConfig<'a>) -> Element<'a, Message> {
    let audio = config.audio;
    let mut rows = column![
        picker_row(
            "Out",
            config.outputs,
            audio.output.as_deref(),
            Message::OutputDevice
        ),
        gain_row_range(
            "Vol",
            theme::RX,
            1.0..=4.0,
            audio.output_gain,
            Message::OutputGain
        ),
        toggle_row(
            "RX compression",
            audio.rx_compression,
            Message::RxCompression
        ),
    ]
    .spacing(10);

    // "Strength" only makes sense with the compressor on (Mac parity).
    if audio.rx_compression {
        rows = rows.push(sub_slider_row(
            "Strength",
            slider(
                0.0..=1.0,
                audio.rx_compression_level,
                Message::RxCompressionLevel,
            )
            .step(0.01_f32)
            .on_release(Message::SaveAudio)
            .style(slider_style(theme::ACCENT)),
            percent(audio.rx_compression_level),
        ));
    }

    card("Speaker", rows)
}

/// VOX calibration: the trigger threshold and how long PTT hangs on after
/// the voice drops below it.
/// (VOX pre-roll joins here once the core lands it — astar-d263 / iax-2733.)
fn vox_card(audio: &AudioSettings) -> Element<'static, Message> {
    card(
        "VOX",
        column![
            sub_slider_row(
                "Audio Level",
                slider(-60.0..=0.0, audio.vox_threshold_dbfs, Message::VoxThreshold)
                    .step(1.0_f32)
                    .on_release(Message::SaveAudio)
                    .style(slider_style(theme::CONNECTING)),
                db_label(audio.vox_threshold_dbfs),
            ),
            sub_slider_row(
                "Hang Timeout",
                slider(
                    100u32..=1500u32,
                    audio.vox_hangtime_ms,
                    Message::VoxHangtime
                )
                .step(50u32)
                .on_release(Message::SaveAudio)
                .style(slider_style(theme::CONNECTING)),
                ms_label(audio.vox_hangtime_ms),
            ),
        ]
        .spacing(10),
    )
}

/// A bordered, full-width save button (the Mac's button style): the generic
/// "Save changes" (writes the quick config to disk on demand; astar-d0df
/// later makes it dirty-only) and "Save changes to <setup>" (snapshots the
/// rig into the selected setup).
fn save_button(label: String, msg: Message) -> Element<'static, Message> {
    button(
        text(label)
            .size(14)
            .color(theme::INK)
            .width(Fill)
            .align_x(iced::alignment::Horizontal::Center),
    )
    .on_press(msg)
    .width(Fill)
    .padding([9, 0])
    .style(|_, status| button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered | button::Status::Pressed => theme::SURFACE,
            _ => theme::TRACK,
        })),
        text_color: theme::INK,
        border: Border {
            color: theme::BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..button::Style::default()
    })
    .into()
}

// -- rows ----------------------------------------------------------------------

/// A labeled card grouping quick settings, with the Mac's uppercased
/// caption title (`groupCard`).
fn card<'a>(title: &'static str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    card_tagged(title, None, content)
}

/// [`card`], with an optional small accent-capsule tag next to the title
/// (the Mac's `groupCard(_:tag:)`) — for making it obvious which profile a
/// card's controls are editing when it isn't the standing shared one.
/// Unused for now: astar-5d8e's "M17" badge on the Mic card was retired at
/// astar-unwarn, but the capsule stays available for a future card.
fn card_tagged<'a>(
    title: &'static str,
    tag: Option<&'static str>,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut header = row![text(title.to_ascii_uppercase())
        .size(11)
        .color(theme::MUTED)
        .font(theme::FONT_SEMIBOLD),]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(tag) = tag {
        header = header.push(
            container(
                text(tag)
                    .size(11)
                    .color(Color::WHITE)
                    .font(theme::FONT_SEMIBOLD),
            )
            .padding([1, 5])
            .style(|_| container::Style {
                background: Some(Background::Color(theme::ACCENT)),
                border: Border {
                    radius: 8.0.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }),
        );
    }
    surface(column![header, content.into()].spacing(10))
        .padding(14)
        .into()
}

/// A top-level row label — bright, matching the toggle rows, so every
/// control in a card carries the same visual weight (Mac `label()`).
fn label(t: &'static str) -> Element<'static, Message> {
    container(text(t).size(14).color(theme::INK))
        .width(LABEL_W)
        .into()
}

/// A sub-control label (Strength, the VOX rows) — dimmed to read
/// subordinate to its parent row (Mac `sublabel()`).
fn sublabel(t: &'static str) -> Element<'static, Message> {
    container(text(t).size(14).color(theme::MUTED))
        .width(LABEL_W)
        .into()
}

/// A label + trailing switch, so every toggle aligns at the card's right
/// edge (matching the sliders' value column).
fn toggle_row(t: &'static str, on: bool, msg: fn(bool) -> Message) -> Element<'static, Message> {
    row![
        text(t).size(14).color(theme::INK),
        Space::new().width(Fill),
        toggler(on).on_toggle(msg).size(20).style(toggler_style),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// A bright-label device-picker row ("In" / "Out").
fn picker_row(
    t: &'static str,
    devices: &[String],
    selected: Option<&str>,
    msg: fn(Option<String>) -> Message,
) -> Element<'static, Message> {
    row![label(t), device_picker(devices, selected, msg)]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

/// A top-level gain slider row (0…2, 100% = unity) with a percent readout.
fn gain_row(
    t: &'static str,
    accent: Color,
    value: f32,
    msg: fn(f32) -> Message,
) -> Element<'static, Message> {
    gain_row_range(t, accent, 0.0..=2.0, value, msg)
}

/// [`gain_row`] with an explicit range — the Speaker card's "Vol" is
/// 100%-400% (floored at unity, iax-a4e7) where every other gain row stays
/// 0…2.
fn gain_row_range(
    t: &'static str,
    accent: Color,
    range: std::ops::RangeInclusive<f32>,
    value: f32,
    msg: fn(f32) -> Message,
) -> Element<'static, Message> {
    row![
        label(t),
        slider(range, value, msg)
            .step(0.01_f32)
            .on_release(Message::SaveAudio)
            .style(slider_style(accent)),
        readout(percent(value)),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// A dim-label slider row (Strength, the VOX rows) with a value readout.
fn sub_slider_row<'a>(
    t: &'static str,
    control: impl Into<Element<'a, Message>>,
    value_text: String,
) -> Element<'a, Message> {
    row![sublabel(t), control.into(), readout(value_text)]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
}

/// The fixed-width trailing value column, so slider tracks all end on the
/// same edge and don't jiggle as values change.
fn readout(value: String) -> Element<'static, Message> {
    container(
        text(value)
            .size(13)
            .color(theme::MUTED)
            .font(theme::FONT_MEDIUM),
    )
    .width(52)
    .align_x(Alignment::End)
    .into()
}

// -- formatting ----------------------------------------------------------------

/// A gain/level as a whole percent, 100% = unity (Mac `.percent` format).
fn percent(v: f32) -> String {
    format!("{}%", (v * 100.0).round() as i32)
}

/// A dBFS value as a whole-dB readout.
fn db_label(v: f32) -> String {
    format!("{} dB", v.round() as i32)
}

/// A milliseconds readout.
fn ms_label(ms: u32) -> String {
    format!("{ms} ms")
}

// -- styles ----------------------------------------------------------------------

/// An accent-railed slider: filled rail in the row's accent, inset track
/// remainder, bright circular handle.
fn slider_style(accent: Color) -> impl Fn(&iced::Theme, slider::Status) -> slider::Style {
    move |_, status| {
        let handle = match status {
            slider::Status::Dragged => Color {
                a: 0.85,
                ..theme::INK
            },
            _ => theme::INK,
        };
        slider::Style {
            rail: slider::Rail {
                backgrounds: (Background::Color(accent), Background::Color(theme::TRACK)),
                width: 6.0,
                border: Border {
                    color: theme::BORDER,
                    width: 1.0,
                    radius: 3.0.into(),
                },
            },
            handle: slider::Handle {
                shape: slider::HandleShape::Circle { radius: 8.0 },
                background: Background::Color(handle),
                border_width: 0.0,
                border_color: Color::TRANSPARENT,
            },
        }
    }
}

/// The switch: accent-filled when on, inset track when off, bright knob.
fn toggler_style(_theme: &iced::Theme, status: toggler::Status) -> toggler::Style {
    let on = matches!(
        status,
        toggler::Status::Active { is_toggled: true }
            | toggler::Status::Hovered { is_toggled: true }
            | toggler::Status::Disabled { is_toggled: true }
    );
    toggler::Style {
        background: Background::Color(if on { theme::ACCENT } else { theme::TRACK }),
        background_border_width: 1.0,
        background_border_color: theme::BORDER,
        foreground: Background::Color(theme::INK),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: None,
        border_radius: None,
        padding_ratio: 0.15,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_readout_matches_the_mac_format() {
        assert_eq!(percent(1.0), "100%", "unity reads 100%");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(2.0), "200%");
        assert_eq!(percent(0.905), "91%", "whole percents, rounded");
    }

    #[test]
    fn setup_choice_maps_selection_and_ids() {
        let setups = vec![
            Setup {
                id: "s-1".into(),
                name: "UCI150 desk".into(),
                hardware_profile_id: "uci150".into(),
                ..Setup::default()
            },
            Setup {
                id: "s-2".into(),
                name: "Jabra mobile".into(),
                hardware_profile_id: "headset".into(),
                ..Setup::default()
            },
        ];

        // No selection, the explicit None pick, and a stale unknown id all
        // render the built-in None entry.
        for sel in [None, Some(NONE_SETUP_ID), Some("gone")] {
            let choice = SetupChoice::from_selection(&setups, sel);
            assert_eq!(choice, SetupChoice::None, "selection {sel:?}");
            assert_eq!(choice.to_string(), "None (system default)");
            assert_eq!(choice.id(), NONE_SETUP_ID);
        }

        // A live selection displays its name and applies by id.
        let choice = SetupChoice::from_selection(&setups, Some("s-2"));
        assert_eq!(choice.to_string(), "Jabra mobile");
        assert_eq!(choice.id(), "s-2");
    }

    #[test]
    fn vox_readouts_are_whole_units() {
        assert_eq!(db_label(-40.0), "-40 dB");
        assert_eq!(db_label(-33.6), "-34 dB");
        assert_eq!(db_label(0.0), "0 dB");
        assert_eq!(ms_label(500), "500 ms");
        assert_eq!(ms_label(1500), "1500 ms");
    }
}
