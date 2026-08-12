// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar — native Iced GUI client over the astar station core.
//!
//! Run modes (parsed from `std::env::args`):
//!
//! * (no args) — live station ([`conn::RealConn`]).
//! * `--demo[=STATE]` — interactive demo backed by [`conn::DemoConn`]; `STATE`
//!   forces a scene (default `connected`).
//! * `--shot STATE FILE.png` — render `STATE`, capture the window to `FILE`,
//!   then exit. Used by `shots.sh` to generate verifiable PNGs.
//!
//! The architecture is the standard Iced MVU split across modules: a connection
//! [`conn::Conn`] seam (demo vs live), a [`snapshot::Snapshot`] projection, a
//! ~20 Hz [`poll`] subscription, a slate [`theme`], and a [`view`].

// Release builds on Windows link as a GUI-subsystem app so no console window
// opens behind ours; debug builds keep the console for --shot/log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod conn;
mod dial_target;
mod icons;
mod m17_dial;
mod network;
mod peak;
mod poll;
// The persistence substrate (astar-de0a). The device pickers (astar-335a),
// quick config (astar-f4d9), favorites (astar-ac65), and setups (astar-80ae)
// consume their fields; the rest (serial, …) is claimed as those nuggets
// land — hence the allow.
#[allow(dead_code)]
mod settings;
mod snapshot;
mod theme;
mod tray;
mod view;

use std::path::PathBuf;

use app::{Mode, ShotScene, State};
use conn::DemoState;

/// Parse the run mode from CLI args. On a usage error, print to stderr and exit.
fn parse_mode() -> Mode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--shot SCENE FILE.png`
    if let Some(pos) = args.iter().position(|a| a == "--shot") {
        let scene = args
            .get(pos + 1)
            .and_then(|s| ShotScene::parse(s))
            .unwrap_or_else(|| {
                usage_exit(
                    "--shot needs a SCENE (idle|connecting|connected|rx|tx|error|config|config-comp|config-new|favorites|dialpad|dialpad-idle|dialpad-sending|network-picker|network-picker-idle|m17-dial)",
                )
            });
        let file = args
            .get(pos + 2)
            .map(PathBuf::from)
            .unwrap_or_else(|| usage_exit("--shot needs a FILE.png"));
        return Mode::Shot { scene, file };
    }

    // `--demo` or `--demo=STATE`
    if let Some(demo) = args.iter().find(|a| a.starts_with("--demo")) {
        let state = demo
            .split_once('=')
            .and_then(|(_, s)| DemoState::parse(s))
            .unwrap_or(DemoState::Connected);
        return Mode::Demo(state);
    }

    Mode::Real
}

/// Print a usage message and exit non-zero. Never returns.
fn usage_exit(msg: &str) -> ! {
    eprintln!("astar: {msg}");
    eprintln!("usage: astar-gui [--demo[=STATE]] [--shot SCENE FILE.png]");
    eprintln!("       STATE = idle | connecting | connected | rx | tx | error");
    eprintln!("       SCENE = STATE | config | config-comp | config-new | favorites | dialpad | dialpad-idle | dialpad-sending | network-picker | network-picker-idle | m17-dial");
    eprintln!("               (config = Quick Config open; favorites = Saved nodes open;");
    eprintln!("                dialpad = Dialpad open, connected / idle / mid-send;");
    eprintln!("                network-picker = Hamlink available, connected, ASL status badge;");
    eprintln!("                network-picker-idle = Hamlink available + selected, idle, picker row + placeholder;");
    eprintln!("                m17-dial = M17 available, idle, M17 selected, callsign prompt + placeholder)");
    std::process::exit(2);
}

fn main() -> iced::Result {
    let mode = parse_mode();

    // Put the asterisk in the tray (astar-22cf) BEFORE the event loop runs: on
    // Windows the icon must be created on the thread that pumps messages —
    // the winit main thread. `--shot` runs headless (CI, Xvfb) and skips the
    // tray entirely; a missing tray host (GNOME without the AppIndicator
    // extension) fails gracefully and the app runs as a normal window.
    if tray::wants_tray(&mode) {
        tray::init();
    }

    // Config/favorites shots grow the window so every expanded card is
    // captured without scrolling; the interactive default stays compact (the
    // page scrolls when a panel is expanded).
    let height = match &mode {
        // Tall enough for the Config (setup switcher) card atop the stack
        // plus both save buttons under the cards.
        Mode::Shot {
            scene: ShotScene::Config { .. } | ShotScene::ConfigNew,
            ..
        } => 1460.0,
        Mode::Shot {
            scene: ShotScene::Favorites,
            ..
        } => 940.0,
        Mode::Shot {
            scene: ShotScene::Dialpad { .. } | ShotScene::DialpadSending,
            ..
        } => 1080.0,
        _ => 700.0,
    };

    // In `--shot` mode we still open a real window (Iced's screenshot path needs
    // a rendered viewport), but it self-closes after capture.
    iced::application(move || State::boot(mode.clone()), app::update, app::view)
        .title(app::title)
        .subscription(app::subscription)
        .centered()
        // Bundled Inter (see assets/fonts) — the cross-platform SF Pro
        // stand-in. Regular carries the UI; Medium/Semibold the emphasis.
        .font(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-Medium.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice())
        .default_font(theme::FONT)
        .window(iced::window::Settings {
            // Tall enough for the full card stack (status → dial → meters →
            // quick settings, collapsed) without scrolling.
            size: iced::Size::new(560.0, height),
            icon: icons::window_icon(),
            // Route the close button through `update` (Message::CloseRequested)
            // so it can hide to the tray instead of quitting when the tray is
            // live; without a tray, `update` quits — same net behavior.
            exit_on_close_request: false,
            ..Default::default()
        })
        .run()
}
