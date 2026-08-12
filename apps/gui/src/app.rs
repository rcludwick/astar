// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The MVU shell (astar-e39d): `State`, `Message`, `update`, and the run modes.
//!
//! `State` mirrors the latest [`Snapshot`] plus UI-only fields (node-entry text,
//! the active demo state). The poll subscription (see [`crate::poll`]) fires
//! [`Message::Tick`] ~20 Hz; each tick advances the connection clock, pulls a
//! fresh snapshot, and (in `--shot` mode) triggers a one-shot screenshot.

use std::path::PathBuf;

use iced::{window, Element, Subscription, Task};

use crate::conn::{Conn, DemoConn, DemoState, RealConn};
use crate::dial_target::DialTarget;
use crate::icons::StatusIcon;
use crate::m17_dial::parse_m17_dial;
use crate::network::{Network, NetworkCaps};
use crate::poll;
use crate::settings::{
    AudioSettings, MemStore, Settings, SettingsStore, Setup, TomlStore, NONE_SETUP_ID,
};
use crate::snapshot::{Snapshot, Status};
use crate::tray;
use crate::view;

/// How the app was launched, parsed from `std::env::args`.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Live station.
    Real,
    /// Interactive demo, optionally forced to a state.
    Demo(DemoState),
    /// Render one scene, screenshot to `file`, then exit.
    Shot { scene: ShotScene, file: PathBuf },
}

/// What a `--shot` renders: a call scene, or the Quick Config panel expanded
/// (astar-f4d9) — with a compression-on variant so the conditional "Strength"
/// sub-row is captured too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotScene {
    /// One of the scripted call states, panel collapsed.
    State(DemoState),
    /// Idle with the Quick Config panel expanded.
    Config {
        /// Force "Voice compression" on so the "Strength" sub-row shows.
        compression: bool,
    },
    /// Quick Config expanded with the "New setup…" name editor open
    /// (astar-80ae), a draft typed in.
    ConfigNew,
    /// Idle with the "Saved nodes" (favorites + recents) section expanded.
    Favorites,
    /// The "Dialpad" section expanded (astar-75fc / astar-7d21): over the
    /// connected scene with a seeded composed command + history, or over idle
    /// (node-entry mode, A-D dimmed) — both halves of dual-mode are capturable.
    Dialpad {
        /// Render over the connected scene (compose bar + all 16 keys live).
        connected: bool,
    },
    /// The dialpad mid-send (astar-7d21): a seeded command playing out —
    /// locked progress readout (played digits dimmed), Stop button, keys inert.
    DialpadSending,
    /// The network picker's status badge (astar-9b3e): connected, Hamlink
    /// reported available, AllStar selected — the "ASL" badge renders beside
    /// the codec capsule. (The picker row itself only renders in the
    /// disconnected `connect_controls` — see [`ShotScene::NetworkPickerIdle`]
    /// for that half of the capture.)
    NetworkPicker,
    /// The network picker's row itself (astar-9b3e): idle, Hamlink reported
    /// available AND selected — so the picker shows Hamlink accent-selected /
    /// AllStar dim, and the dial field shows the Hamlink placeholder
    /// ("Reflector host / talkgroup"). Complements [`ShotScene::NetworkPicker`],
    /// which captures the connected-only status badge instead.
    NetworkPickerIdle,
    /// The M17 dial scene (M17 Task 10): idle, M17 reported available
    /// (Hamlink off), M17 selected, and an empty persisted callsign — so the
    /// picker shows AllStar + M17, the callsign prompt renders above the dial
    /// row, and the dial field shows the M17 placeholder
    /// ("Reflector host:port / module").
    M17Dial,
}

impl ShotScene {
    /// Parse a `--shot SCENE` token (case-insensitive): a [`DemoState`] name,
    /// `config`, or `config-comp`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "config" => Some(ShotScene::Config { compression: false }),
            "config-comp" => Some(ShotScene::Config { compression: true }),
            "config-new" => Some(ShotScene::ConfigNew),
            "favorites" => Some(ShotScene::Favorites),
            "dialpad" => Some(ShotScene::Dialpad { connected: true }),
            "dialpad-idle" => Some(ShotScene::Dialpad { connected: false }),
            "dialpad-sending" => Some(ShotScene::DialpadSending),
            "network-picker" => Some(ShotScene::NetworkPicker),
            "network-picker-idle" => Some(ShotScene::NetworkPickerIdle),
            "m17-dial" => Some(ShotScene::M17Dial),
            _ => DemoState::parse(s).map(ShotScene::State),
        }
    }

    /// The conn scene behind the shot: config/favorites shots render over
    /// idle; the dialpad shot picks its mode; `NetworkPicker` renders over a
    /// live call (so the status badge has something to show), while
    /// `NetworkPickerIdle` renders over idle (so the picker row/placeholder
    /// are visible instead).
    fn demo_state(self) -> DemoState {
        match self {
            ShotScene::State(state) => state,
            ShotScene::Config { .. }
            | ShotScene::ConfigNew
            | ShotScene::Favorites
            | ShotScene::NetworkPickerIdle
            | ShotScene::M17Dial => DemoState::Idle,
            ShotScene::Dialpad { connected: true }
            | ShotScene::DialpadSending
            | ShotScene::NetworkPicker => DemoState::Connected,
            ShotScene::Dialpad { connected: false } => DemoState::Idle,
        }
    }
}

/// The full application state.
pub struct State {
    /// The active connection (demo or real), behind the seam.
    conn: Box<dyn Conn>,
    /// The latest snapshot pulled from `conn`.
    snap: Snapshot,
    /// UI: text in the node-entry field.
    pub node_entry: String,
    /// The network the next dial goes on (astar-9b3e) — resolved at boot
    /// against the engine's `hamlink_available` capability, then held here
    /// (rather than re-resolved every read) so a mid-session capability
    /// change can't retroactively swap the active pick out from under a
    /// dial in progress.
    pub network: Network,
    /// The networks the engine can drive right now (astar-9b3e), cached at
    /// boot from `conn.hamlink_available()` so the view can borrow a slice
    /// that outlives one `view()` call (a same-call local Vec can't — its
    /// borrow dies with the function's stack frame).
    network_available: Vec<Network>,
    /// UI: the M17 callsign field's local, unpersisted draft (M17 Task 10
    /// fix — mirrors the Mac's `MenuPopover.callsignDraft`). Uppercased as
    /// you type; only written into `settings.m17_callsign` (and saved) on
    /// the field's Enter or right before an M17 Connect — never on every
    /// keystroke, unlike every other per-keystroke field in this app, none of
    /// which touch disk. Seeded from `settings.m17_callsign` at boot.
    m17_callsign_draft: String,
    /// The run mode (drives the screenshot path).
    mode: Mode,
    /// Tick counter, used to delay the screenshot until the window has settled.
    ticks: u64,
    /// Set once the screenshot has been requested so we only fire it once.
    shot_fired: bool,
    /// The settings store (real TOML file, or in-memory for demos/tests).
    store: Box<dyn SettingsStore>,
    /// The loaded settings; mutated by picks, then saved back to `store`.
    settings: Settings,
    /// Capture device names for the "In" picker (enumerated at boot).
    inputs: Vec<String>,
    /// Playback device names for the "Out" picker (enumerated at boot).
    outputs: Vec<String>,
    /// Why the last settings load/save failed; shown on the status card when
    /// the conn has no error of its own.
    settings_error: Option<String>,
    /// UI: whether the "Quick settings" section is expanded (mirrors the Mac
    /// popover's disclosure; session-only for now).
    quick_config_open: bool,
    /// UI: whether the "Saved nodes" (favorites + recents) section is
    /// expanded (session-only, like the quick config disclosure).
    favorites_open: bool,
    /// UI: whether the "Dialpad" section is expanded (session-only, like the
    /// other disclosures; the Mac remembers it across launches).
    dialpad_open: bool,
    /// The DTMF command being composed (connected mode) — editable until Send
    /// plays it as one engine-timed sequence (astar-7d21; mirrors the Mac's
    /// `dtmfCommand`). Reset on a fresh dial and call end.
    dtmf_command: String,
    /// The command currently playing out (field locked, played digits dim
    /// from the snapshot's progress); `None` when nothing is playing.
    dtmf_playing: Option<String>,
    /// Commands sent this call, oldest first — the subdued history line.
    dtmf_history: Vec<String>,
    /// UI: the save-favorite editor's label draft; `None` = editor closed.
    /// Mirrors the Mac's favorite-editor popover and its `favoriteLabel`.
    favorite_editor: Option<String>,
    /// Whether this call's successful connect has been recorded as a recent
    /// — once per call, reset on each fresh dial (mirrors the Mac's
    /// `recordedRecentForCall` edge in `CallSession.poll`).
    recorded_recent: bool,
    /// UI: the "New setup…" editor's name draft; `None` = editor closed
    /// (same idiom as `favorite_editor`).
    setup_editor: Option<String>,
    /// Why the last setup apply was refused (a named USB device unplugged) —
    /// mirrors `SetupController.lastError`. Cleared on the next successful
    /// apply; shown on the status card like `settings_error`.
    setup_error: Option<String>,
    /// The single main window's id, probed on the first tick (iced opens the
    /// boot window itself, so there's no open() call to capture it from).
    main_window: Option<window::Id>,
    /// Whether the main window is currently shown — the tray toggle and the
    /// close-to-tray path keep it honest.
    window_visible: bool,
    /// Whether a tray icon is live (see [`crate::tray`]): the close button
    /// hides instead of quitting, and the asterisk tints with the call.
    tray_active: bool,
    /// The Linux "no tray host" guidance (the GNOME caveat) — rendered as a
    /// dim footer note when the tray was wanted but unavailable.
    tray_hint: Option<&'static str>,
    /// The tray asterisk's current state; re-tinted only when it changes.
    tray_icon: StatusIcon,
    /// The last tray click's icon rect — the popover placement anchor.
    tray_anchor: Option<tray::Anchor>,
    /// Set once the window has been placed near the tray. First show only —
    /// afterwards the window stays wherever the user moved it, mirroring the
    /// Mac's place-under-the-star-on-first-open behavior.
    popover_placed: bool,
    /// Whether the M17 TX-processing override (`settings.m17_audio`) is the
    /// one currently pushed to the conn (astar-5d8e) — mirrors the Mac's
    /// `activeCallNetwork == .m17` gate for this feature specifically. Set
    /// `true` the instant an M17 dial is dispatched, `false` (with a restore
    /// of the standard `settings.audio` chain) on the M17 call ending —
    /// `Disconnect`, the `Tick` poll-path edge for a remote hangup, or an
    /// AllStar dial starting. Distinct from `state.network` (the *picker*
    /// selection, which can move without ending the call — gui-rs has no
    /// richer per-call network state yet, same simplification as the
    /// `Disconnect` handler's own M17 check).
    m17_audio_live: bool,
}

/// Messages that drive the update loop.
#[derive(Debug, Clone)]
pub enum Message {
    /// The 50 ms poll heartbeat.
    Tick,
    /// Node-entry text changed.
    NodeEntryChanged(String),
    /// A network segment was picked (astar-9b3e).
    NetworkPicked(Network),
    /// The M17 callsign field's draft changed (M17 Task 10) — uppercased,
    /// held in `State::m17_callsign_draft`; NOT persisted (mirrors the Mac's
    /// `callsignDraft.onChange`, which only touches the local draft too — see
    /// [`Message::M17CallsignCommit`] for the write-through).
    M17CallsignChanged(String),
    /// Commit the M17 callsign draft into `settings.m17_callsign` and persist
    /// (M17 Task 10) — fires on the field's Enter (mirrors the Mac's
    /// `commitCallsignDraft`, called from the same field's `.onSubmit`) and
    /// again from the `Connect` dispatch right before an M17 dial, so
    /// type-then-click-Connect commits in one gesture without needing Enter
    /// first. A blank draft is left uncommitted (mirrors the Mac's guard).
    M17CallsignCommit,
    /// Connect to the entered node.
    Connect,
    /// Disconnect the active call.
    Disconnect,
    /// PTT pressed/released.
    Ptt(bool),
    /// Input (capture) device picked; `None` = system default.
    InputDevice(Option<String>),
    /// Output (playback) device picked; `None` = system default.
    OutputDevice(Option<String>),
    /// The "Quick settings" disclosure was toggled open/closed.
    QuickConfigToggled,
    /// The "Saved nodes" disclosure was toggled open/closed.
    FavoritesToggled,
    /// The "Dialpad" disclosure was toggled open/closed.
    DialpadToggled,
    /// A dialpad key was tapped. Dual-mode: appends to the node field when
    /// idle, appends to the composed command when connected (astar-7d21 —
    /// nothing goes on the air until Send).
    Dialpad(char),
    /// The compose field's text changed (typing/paste); filtered to the 16
    /// DTMF keys, uppercased.
    DialpadComposeChanged(String),
    /// Send the composed command as one engine-timed tone sequence.
    DialpadSend,
    /// Stop the playing sequence — drops the un-played remainder.
    DialpadStop,
    /// Clear the composed command.
    DialpadClear,
    /// The dial-row star: favorite the entered node (opens the label editor)
    /// or un-favorite it if it already is one.
    FavoriteToggle,
    /// The save-favorite editor's label draft changed.
    FavoriteLabelChanged(String),
    /// Save the favorite from the editor (label + entered node).
    FavoriteSave,
    /// Close the save-favorite editor without saving.
    FavoriteCancel,
    /// A saved-node row was clicked: dial it (same path as typing + Connect).
    DialFavorite(String),
    /// Remove a favorite (a row's ×); kept as a recent if it has been dialed.
    RemoveFavorite(String),
    /// "Full duplex" toggled (Station card).
    FullDuplex(bool),
    /// "Mic Level" (input gain, 0…2) dragged.
    InputGain(f32),
    /// "Noise reduction" toggled (Mic card).
    NoiseReduction(bool),
    /// "Voice compression" toggled (Mic card).
    Compression(bool),
    /// "Strength" (compression level, 0…1) dragged.
    CompressionLevel(f32),
    /// "TX Gain" (TX trim, 0…2) dragged.
    TxTrim(f32),
    /// "Vol" (output gain, 100%-400%) dragged.
    OutputGain(f32),
    /// "RX compression" toggled (Speaker card, iax-a4e7): automatic leveling
    /// of the received audio. Shared across networks — no M17 override.
    RxCompression(bool),
    /// "Strength" (RX compression level, 0…1) dragged (Speaker card).
    RxCompressionLevel(f32),
    /// "Audio Level" (VOX threshold, −60…0 dBFS) dragged.
    VoxThreshold(f32),
    /// "Hang Timeout" (VOX hangtime, 100…1500 ms) dragged.
    VoxHangtime(u32),
    /// Persist the audio settings: slider releases and the "Save changes"
    /// button (drags apply live but only touch the disk here).
    SaveAudio,
    /// A Setup was picked in the switcher: apply it (devices + overrides) and
    /// persist the selection. [`NONE_SETUP_ID`] picks the built-in None.
    SetupSelected(String),
    /// "New setup…" pressed: open the inline name editor.
    SetupNew,
    /// The new-setup editor's name draft changed.
    SetupNameChanged(String),
    /// Create the setup from the editor: snapshot the current rig under the
    /// drafted name and select it.
    SetupCreate,
    /// Close the new-setup editor without creating.
    SetupCreateCancel,
    /// "Save changes to <name>": snapshot the current audio state into the
    /// selected setup (mirrors `SetupController.saveCurrentToSelected`).
    SetupSaveCurrent,
    /// Delete the selected setup (the built-in None can't be deleted).
    SetupDelete,
    /// A screenshot was captured (`--shot` mode): encode + write, then exit.
    Captured(window::Screenshot),
    /// First-tick probe result: the main window's id.
    MainWindow(Option<window::Id>),
    /// A tray event (star click or menu pick), drained by the tick.
    Tray(tray::Event),
    /// The window close button — hide to the tray when it's live (the Mac's
    /// close-just-hides behavior), else quit.
    CloseRequested,
    /// Popover placement pipeline, step 1: the window's scale factor arrived.
    PopoverScale(f32),
    /// Step 2: the window's logical size arrived.
    PopoverSize { scale: f32, size: iced::Size },
    /// Step 3: the monitor's logical size arrived — compute and move.
    PopoverPlace {
        scale: f32,
        size: iced::Size,
        monitor: Option<iced::Size>,
    },
}

impl State {
    /// Construct the initial state for `mode`, plus any boot task. In `--shot`
    /// mode the boot task is empty — the screenshot is fired from a later tick
    /// (once the renderer has produced a frame).
    pub fn boot(mode: Mode) -> (Self, Task<Message>) {
        match &mode {
            // Live mode persists to the platform config dir.
            Mode::Real => match TomlStore::open_default() {
                Some(store) => Self::boot_with(mode, Box::new(store)),
                None => {
                    // No home dir (unusual): run on the in-memory store and
                    // say so rather than refuse to start.
                    let (mut state, task) =
                        Self::boot_with(mode.clone(), Box::new(MemStore::default()));
                    state.settings_error =
                        Some("No config directory found — settings won't persist".to_string());
                    (state, task)
                }
            },
            // Demos and shots never touch the user's settings file.
            Mode::Demo(_) | Mode::Shot { .. } => {
                Self::boot_with(mode, Box::new(MemStore::default()))
            }
        }
    }

    /// [`State::boot`] with an explicit settings store (tests inject a
    /// [`MemStore`] here). Loads settings, pushes the persisted device choice
    /// through the conn seam, and enumerates the picker lists once.
    fn boot_with(mode: Mode, store: Box<dyn SettingsStore>) -> (Self, Task<Message>) {
        let mut settings_error = None;
        let mut settings = store.load().unwrap_or_else(|e| {
            // A broken/newer file: surface it and run on defaults — never
            // clobber the user's file until they save something themselves.
            settings_error = Some(e.to_string());
            Settings::default()
        });

        // The codec policy is always PreferSlin16 (astar-e542) and pins at
        // station construction; the persisted devices/gains ride in through
        // the later set_audio push (astar-efba, mirroring `CallSession.live`).
        let mut conn: Box<dyn Conn> = match &mode {
            Mode::Real => Box::new(RealConn::new()),
            Mode::Demo(state) => Box::new(DemoConn::new(*state)),
            Mode::Shot { scene, .. } => {
                let mut demo = DemoConn::new(scene.demo_state());
                // The two network-picker shots (astar-9b3e) are the only
                // scenes that need the engine to report reflector
                // capability; every other scene stays off so the rest of
                // the shots are pixel-identical to pre-9b3e.
                // `set_hamlink_available` is a `DemoConn`-only knob, so it
                // must be flipped here, before `demo` is boxed into the
                // trait object.
                if matches!(
                    scene,
                    ShotScene::NetworkPicker | ShotScene::NetworkPickerIdle
                ) {
                    demo.set_hamlink_available(true);
                }
                // The M17 dial shot (M17 Task 10) needs the engine to report
                // M17 capability instead — Hamlink stays off so the picker
                // shows exactly AllStar + M17, matching the scene contract.
                if matches!(scene, ShotScene::M17Dial) {
                    demo.set_m17_available(true);
                }
                Box::new(demo)
            }
        };

        // Demos select the demo devices and seed the demo node directory and
        // Setups so `--shot` screenshots render real picker text, a populated
        // favorites section, and the setup switcher (the in-memory store
        // starts empty). The first demo setup is the launch default — applied
        // below, exactly like a persisted default in live mode — so shots
        // show the switcher with a named rig selected and its overrides live.
        if matches!(mode, Mode::Demo(_) | Mode::Shot { .. }) {
            settings.audio.input = Some(DemoConn::INPUTS[0].to_string());
            settings.audio.output = Some(DemoConn::OUTPUTS[0].to_string());
            settings.directory = DemoConn::demo_directory();
            settings.setups = DemoConn::demo_setups();
            settings.default_setup = settings.setups.first().map(|s| s.id.clone());
        }

        // The idle network-picker shot (astar-9b3e) seeds Hamlink as the
        // selection too, so the capture shows the picker's Hamlink pill
        // accent-selected (AllStar dim) and the dial field's Hamlink
        // placeholder — the half of the picker `NetworkPicker` (connected,
        // AllStar selected) doesn't exercise.
        if matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::NetworkPickerIdle,
                ..
            }
        ) {
            settings.network = Network::Hamlink;
        }

        // The M17 dial shot (M17 Task 10) seeds M17 as the selection so the
        // capture shows the picker's M17 pill accent-selected, the M17
        // placeholder in the dial field, and — since `m17_callsign` stays at
        // its default empty string (never seeded here) — the callsign prompt
        // above the dial row.
        if matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::M17Dial,
                ..
            }
        ) {
            settings.network = Network::M17;
        }

        // Config shots open the panel; the `-comp` variant forces compression
        // on so the conditional "Strength" sub-row is in the capture. (The
        // flag is stamped after the launch-default setup applies, below — the
        // scene contract must win over the demo setup's own compression.)
        let quick_config_open = matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::Config { .. } | ShotScene::ConfigNew,
                ..
            }
        );

        // The config-new shot opens the "New setup…" editor with a draft
        // typed, so the capture shows the name field dressed.
        let setup_editor = if matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::ConfigNew,
                ..
            }
        ) {
            Some("Truck rig".to_string())
        } else {
            None
        };

        // Favorites shots open the "Saved nodes" section the same way.
        let favorites_open = matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::Favorites,
                ..
            }
        );

        // Dialpad shots open the "Dialpad" section. The connected variant
        // seeds a composed command + a history entry (the Mac's example: link
        // node 546054) so the capture shows compose mode fully dressed; the
        // sending variant starts a real (demo) sequence so the capture shows
        // the locked progress readout + Stop (astar-7d21).
        let dialpad_open = matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::Dialpad { .. } | ShotScene::DialpadSending,
                ..
            }
        );
        let (dtmf_command, dtmf_history) = if matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::Dialpad { connected: true },
                ..
            }
        ) {
            ("*3546054".to_string(), vec!["*70".to_string()])
        } else {
            (String::new(), Vec::new())
        };
        let dtmf_playing = if matches!(
            mode,
            Mode::Shot {
                scene: ShotScene::DialpadSending,
                ..
            }
        ) {
            // First digit goes out at send; the pre-capture ticks advance
            // it deterministically (SHOT_DELAY_TICKS is a const).
            conn.send_dtmf_string("*3546054");
            Some("*3546054".to_string())
        } else {
            None
        };

        // Apply the persisted device choice + audio prefs; the station holds
        // them and re-applies on every (re)connect.
        conn.set_devices(settings.audio.input.clone(), settings.audio.output.clone());
        conn.set_audio(&settings.audio);
        let (inputs, outputs) = conn.list_devices();

        // Resolve the persisted network pick against what the engine can
        // actually drive right now — a stale "hamlink"/"m17" from before the
        // capability existed (or from a session where it later dropped)
        // falls back to AllStar rather than wedging the picker (astar-9b3e /
        // M17 Task 10).
        let caps = NetworkCaps {
            hamlink: conn.hamlink_available(),
            m17: conn.m17_available(),
        };
        let network_available = Network::available(caps);
        let network = Network::resolve(persisted_network_raw(settings.network), caps);

        let snap = conn.snapshot();
        let mut state = Self {
            conn,
            snap,
            node_entry: String::new(),
            network,
            network_available,
            // Seed the draft from the persisted value (M17 Task 10 fix) —
            // mirrors the Mac's `m17CallsignField.onAppear`.
            m17_callsign_draft: settings.m17_callsign.clone(),
            mode,
            ticks: 0,
            shot_fired: false,
            store,
            settings,
            inputs,
            outputs,
            settings_error,
            quick_config_open,
            favorites_open,
            dialpad_open,
            dtmf_command,
            dtmf_playing,
            dtmf_history,
            favorite_editor: None,
            recorded_recent: false,
            setup_editor,
            setup_error: None,
            main_window: None,
            window_visible: true,
            tray_active: tray::is_active(),
            tray_hint: tray::fallback_hint(),
            tray_icon: StatusIcon::Idle,
            tray_anchor: None,
            popover_placed: false,
            m17_audio_live: false,
        };

        // Apply the launch-default Setup, if one is set and still exists —
        // mirrors `SetupController.attach`. Its overrides land on top of the
        // persisted globals and ride the same conn seam pushed above.
        if let Some(d) = state.settings.default_setup.clone() {
            if d == NONE_SETUP_ID || state.settings.setups.iter().any(|s| s.id == d) {
                state.apply_setup(&d);
            }
        }

        // Config shots stamp their compression flag last (see above).
        if let Mode::Shot {
            scene: ShotScene::Config { compression },
            ..
        } = state.mode
        {
            state.settings.audio.compression = compression;
            state.apply_audio();
        }

        (state, Task::none())
    }

    /// Push the currently-selected devices to the conn and persist them. A
    /// save failure lands on the status card via `settings_error`.
    fn apply_devices(&mut self) {
        self.conn.set_devices(
            self.settings.audio.input.clone(),
            self.settings.audio.output.clone(),
        );
        // Also write the change into the active setup, else the setup
        // re-applies its old devices at the next launch and the change
        // appears not to persist (mirrors `SetupController.setActiveDevices`).
        let input = self.settings.audio.input.clone();
        let output = self.settings.audio.output.clone();
        if let Some(s) = self.selected_setup_mut() {
            s.input_device = input;
            s.output_device = output;
        }
        self.persist_settings();
    }

    /// The selected setup, if it's a real (editable) one — the built-in None
    /// can't hold state (the Mac's `selectedEditableSetup`).
    fn selected_setup_mut(&mut self) -> Option<&mut Setup> {
        let id = self.settings.selected_setup.clone()?;
        if id == NONE_SETUP_ID {
            return None;
        }
        self.settings.setups.iter_mut().find(|s| s.id == id)
    }

    /// Switch to the setup with `id` — the gui-rs `SetupController.apply`:
    /// refuse (set `setup_error`, change nothing) when a named device isn't
    /// enumerated; otherwise merge the setup's overrides onto the globals,
    /// push the result through the conn seam (the station holds devices and
    /// standing audio prefs and re-applies them on every (re)connect, so the
    /// effective values survive reconnects), and persist the selection.
    /// Unknown ids are a no-op, like the Mac. (No serial to drive yet — the
    /// hardware profile id persists for parity until gui-rs grows serial.)
    fn apply_setup(&mut self, id: &str) {
        let setup = if id == NONE_SETUP_ID {
            Setup::none()
        } else {
            match self.settings.setups.iter().find(|s| s.id == id) {
                Some(s) => s.clone(),
                None => return,
            }
        };

        let missing = setup.missing_devices(&self.inputs, &self.outputs);
        if !missing.is_empty() {
            self.setup_error = Some(format!(
                "“{}” needs {}, which isn’t connected.",
                setup.name,
                missing.join(", ")
            ));
            return;
        }

        setup.apply_to(&mut self.settings.audio);
        self.conn.set_devices(
            self.settings.audio.input.clone(),
            self.settings.audio.output.clone(),
        );
        self.apply_audio();
        self.setup_error = None;
        self.settings.selected_setup = Some(id.to_string());
        self.persist_settings();
    }

    /// Snapshot the current audio state into the selected setup, so switching
    /// to it later restores the whole rig — mirrors
    /// `SetupController.saveCurrentToSelected`. No-op for None / no selection.
    fn save_current_to_selected(&mut self) {
        let audio = self.settings.audio.clone();
        if let Some(s) = self.selected_setup_mut() {
            s.capture_from(&audio);
            self.persist_settings();
        }
    }

    /// Push the audio prefs bundle to the conn (live effect — sliders apply
    /// while dragging, like the Mac). Persisting is separate: toggles save
    /// immediately, sliders on release ([`Message::SaveAudio`]).
    fn apply_audio(&mut self) {
        let audio = self.effective_audio();
        self.conn.set_audio(&audio);
    }

    /// The `AudioSettings` to actually push to the conn right now (astar-5d8e):
    /// the shared settings, with the M17 TX override (`settings.m17_audio`)
    /// layered onto `noise_reduction`/`compression`/`compression_level`/
    /// `tx_trim` while `m17_audio_live` — mirrors the Mac's
    /// `pushM17TxOverrides`/`restoreStandardTxProcessing` split as one pure
    /// function (gui-rs's `Conn::set_audio` already pushes the whole bundle,
    /// so there's only ever one push path here, not two).
    fn effective_audio(&self) -> AudioSettings {
        let mut audio = self.settings.audio.clone();
        if self.m17_audio_live {
            audio.noise_reduction = self.settings.m17_audio.noise_reduction;
            audio.compression = self.settings.m17_audio.compression;
            audio.compression_level = self.settings.m17_audio.compression_level;
            audio.tx_trim = self.settings.m17_audio.tx_trim;
            // Mic (input) gain joined the override at astar-m17defaults;
            // output gain has no M17 counterpart and stays shared.
            audio.input_gain = self.settings.m17_audio.input_gain;
        }
        audio
    }

    /// Whether the M17 TX override is the one in play right now (astar-5d8e):
    /// the M17 network is selected in the picker, or its override is already
    /// live on the conn. Gates which struct the four TX-processing
    /// quick-config messages mutate, and the view's "M17" tag / "not
    /// recommended" captions.
    fn m17_context(&self) -> bool {
        self.network == Network::M17 || self.m17_audio_live
    }

    /// Save the settings; a failure lands on the status card.
    fn persist_settings(&mut self) {
        self.settings_error = self.store.save(&self.settings).err().map(|e| e.to_string());
    }

    /// Commit `m17_callsign_draft` into `settings.m17_callsign` and persist
    /// (M17 Task 10 fix) — mirrors the Mac's `commitCallsignDraft`: a blank
    /// (post-trim) draft is left uncommitted, so this never clobbers an
    /// already-saved callsign with an accidental empty field. Idempotent —
    /// safe to call from both the field's Enter and the Connect dispatch.
    fn commit_m17_callsign(&mut self) {
        let trimmed = self.m17_callsign_draft.trim().to_ascii_uppercase();
        if !trimmed.is_empty() {
            self.settings.m17_callsign = trimmed;
            self.persist_settings();
        }
    }

    /// Number of poll ticks to wait before capturing in `--shot` mode. At 50 ms
    /// each, ~10 ticks (~0.5 s) reliably lands after the first rendered frame.
    const SHOT_DELAY_TICKS: u64 = 10;
}

/// The window title (varies by mode so demo windows are identifiable).
#[must_use]
pub fn title(state: &State) -> String {
    match &state.mode {
        Mode::Real => "astar".to_string(),
        Mode::Demo(s) => format!("astar — demo ({s:?})"),
        Mode::Shot { scene, .. } => format!("astar — shot ({scene:?})"),
    }
}

/// The update handler.
pub fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            state.ticks += 1;
            // In `--shot` mode, freeze the demo animation a couple of ticks
            // before capture: iced 0.14's offscreen screenshot drops text that
            // changed in the very last frame (the live window renders it fine),
            // so the scene must hold still for a beat to capture faithfully.
            let freeze = matches!(state.mode, Mode::Shot { .. })
                && state.ticks + 2 >= State::SHOT_DELAY_TICKS;
            if !freeze {
                state.conn.tick();
            }
            state.snap = state.conn.snapshot();
            // DTMF sequence lifecycle (astar-7d21), mirroring the Mac's
            // onChange hooks: progress falling back to 0 while we hold a
            // playing command = completed (move it to the history line);
            // leaving Connected discards compose state with the call.
            if state.snap.status == Status::Connected {
                if state.snap.dtmf_total == 0 {
                    if let Some(done) = state.dtmf_playing.take() {
                        state.dtmf_history.push(done);
                    }
                }
            } else if state.snap.status == Status::Disconnected {
                state.dtmf_command.clear();
                state.dtmf_playing = None;
                state.dtmf_history.clear();
                // M17 TX-override teardown on the poll path (astar-5d8e): a
                // call that ends without the user hitting Disconnect (the
                // remote end/node drops us) never reaches that handler, so
                // this edge is the only place that restores the standard TX
                // chain for that case. `Message::Disconnect` already clears
                // `m17_audio_live` before this can run again, so a
                // user-initiated hangup can't double-fire the restore.
                if state.m17_audio_live {
                    state.m17_audio_live = false;
                    state.apply_audio();
                }
            }
            // A refused setup apply or a settings problem is worth a status
            // line too — but never louder than a live conn error.
            if state.snap.error.is_none() {
                state.snap.error = state
                    .setup_error
                    .clone()
                    .or_else(|| state.settings_error.clone());
            }

            // On a SUCCESSFUL connect (status reaches Connected), auto-record
            // the dialed node as a recent — once per call (the flag resets on
            // each fresh dial). Re-dialing the same node updates its last_used
            // rather than duplicating. Mirrors the Mac's answered edge in
            // `CallSession.poll`.
            if state.snap.status == Status::Connected && !state.recorded_recent {
                if let Some(node) = state.snap.dialed_node.clone() {
                    state.recorded_recent = true;
                    state.settings.record_recent(&node, unix_now());
                    state.persist_settings();
                }
            }

            let mut tasks = Vec::new();

            // Learn the single main window's id (needed to hide/show/move it
            // from the tray); iced opened it for us, so probe until it lands.
            if state.main_window.is_none() {
                tasks.push(window::oldest().map(Message::MainWindow));
            }

            // Keep the tray asterisk tinted like the Mac's menu-bar item, and
            // route tray clicks/menu picks through the normal message flow.
            if state.tray_active {
                let icon = tray::status_icon(&state.snap);
                if icon != state.tray_icon {
                    state.tray_icon = icon;
                    tray::set_icon(icon);
                }
                for ev in tray::poll_events() {
                    tasks.push(Task::done(Message::Tray(ev)));
                }
            }

            // In `--shot` mode, once the window has settled, capture it.
            if let Mode::Shot { .. } = &state.mode {
                if !state.shot_fired && state.ticks >= State::SHOT_DELAY_TICKS {
                    state.shot_fired = true;
                    tasks.push(
                        window::oldest()
                            .and_then(window::screenshot)
                            .map(Message::Captured),
                    );
                }
            }
            Task::batch(tasks)
        }
        Message::NodeEntryChanged(mut s) => {
            // Admit node chars (digits, * # command dials) plus hostname/IP
            // chars (letters, dots, colons, hyphens); drop anything else as
            // it's typed or pasted (so e.g. the spacebar PTT shortcut can
            // never leak spaces into the field) — the Mac's onChange filter,
            // now per-network (astar-9b3e; AllStar's filter is identical to
            // the old inline one, verbatim).
            s.retain(|c| state.network.admits_dial_char(c));
            state.node_entry = s;
            Task::none()
        }
        Message::NetworkPicked(network) => {
            state.network = network;
            state.settings.network = network;
            state.persist_settings();
            Task::none()
        }
        Message::M17CallsignChanged(text) => {
            state.m17_callsign_draft = text.to_ascii_uppercase();
            Task::none()
        }
        Message::M17CallsignCommit => {
            state.commit_m17_callsign();
            Task::none()
        }
        Message::Connect => {
            // Smart dial routing (astar-3939, the Mac's astar-427f): digits
            // dial a node through the registrar (WT primary — the conn
            // decides how to authenticate); a host/host:port dials that
            // address directly, the typed address doubling as the
            // display/recents label. Unparseable text is unreachable via the
            // button (disabled) but can arrive via on_submit — refuse it the
            // same way as empty input. M17 (M17 Task 10) is a third, distinct
            // grammar entirely (`host[:port]/module`), so it's dispatched
            // before the AllStar/Hamlink `DialTarget` path rather than
            // reusing it — mirrors the Mac's `connect(node:network:)` split.
            if state.network == Network::M17 {
                // Commit any uncommitted draft first (mirrors the Mac's
                // `connect()` calling `commitCallsignDraft()` right before
                // dialing) — so typing a callsign then clicking Connect
                // works in one gesture, without needing Enter in the
                // callsign field first.
                state.commit_m17_callsign();
                let callsign = state.settings.m17_callsign.trim().to_string();
                if let Some((host, port, module)) = parse_m17_dial(&state.node_entry) {
                    if !callsign.is_empty() {
                        state.recorded_recent = false;
                        state.favorite_editor = None;
                        state.dtmf_command.clear();
                        state.dtmf_playing = None;
                        state.dtmf_history.clear();
                        state.conn.connect_m17(&host, port, module, &callsign);
                        // The M17 TX override goes live the instant the dial
                        // succeeds (astar-5d8e) — not at answer.
                        state.m17_audio_live = true;
                        state.apply_audio();
                    }
                }
            } else if let Some(target) = DialTarget::parse(&state.node_entry) {
                // An AllStar/Hamlink dial starting undoes a still-live M17 TX
                // override (astar-5d8e) — before dispatching, so the standard
                // chain is what's live for the call about to go out.
                if state.m17_audio_live {
                    state.m17_audio_live = false;
                    state.apply_audio();
                }
                // A fresh dial re-arms the one-per-call recent recording,
                // drops any half-finished favorite editor (the connect card
                // swaps away for the call controls), and starts a fresh
                // dialpad (compose + history are per-call).
                state.recorded_recent = false;
                state.favorite_editor = None;
                state.dtmf_command.clear();
                state.dtmf_playing = None;
                state.dtmf_history.clear();
                match target {
                    DialTarget::Node(node) => state.conn.connect_wt(&node),
                    DialTarget::Address(addr) => state.conn.connect_address(&addr),
                }
            }
            Task::none()
        }
        Message::Disconnect => {
            // Belt-and-suspenders M17 teardown (mirrors the Mac's
            // `CallSession.disconnect`): `Station::disconnect` already tears
            // down the active call regardless of network, but calling
            // `m17_disconnect` explicitly keeps the M17-specific teardown
            // observable in tests and reads clearly at the call site rather
            // than relying on an implementation detail one layer down. gui-rs
            // has no per-call network state yet — same simplification as the
            // status badge (astar-9b3e): approximate with the current picker
            // selection, which can only drift from the true active-call
            // network once switching networks mid-call is wired (it isn't
            // yet).
            if state.network == Network::M17 {
                state.conn.m17_disconnect();
            }
            // The M17 call is ending — undo its TX override (astar-5d8e), if
            // it was ever live (a no-op guard against a redundant push).
            if state.m17_audio_live {
                state.m17_audio_live = false;
                state.apply_audio();
            }
            state.conn.disconnect();
            Task::none()
        }
        Message::Ptt(on) => {
            // Hold-to-talk is only live once the call is answered (mirrors
            // the Mac PTT button) — never key a dialing/idle station. Unkey
            // always passes through so a release can't be lost mid-teardown.
            if !on || state.snap.status == Status::Connected {
                state.conn.set_ptt(on);
            }
            Task::none()
        }
        Message::InputDevice(device) => {
            state.settings.audio.input = device;
            state.apply_devices();
            Task::none()
        }
        Message::OutputDevice(device) => {
            state.settings.audio.output = device;
            state.apply_devices();
            Task::none()
        }
        Message::QuickConfigToggled => {
            state.quick_config_open = !state.quick_config_open;
            Task::none()
        }
        Message::FavoritesToggled => {
            state.favorites_open = !state.favorites_open;
            Task::none()
        }
        Message::DialpadToggled => {
            state.dialpad_open = !state.dialpad_open;
            Task::none()
        }
        Message::Dialpad(key) => {
            // One tap = one action, keyed off the call state (the Mac's
            // `dialpadTap`).
            match state.snap.status {
                // Connected: append to the composed command — nothing goes on
                // the air until Send (astar-7d21). Taps are inert while a
                // sequence plays (the keys render disabled; this backs it up).
                Status::Connected => {
                    if state.dtmf_playing.is_none() {
                        state.dtmf_command.push(key);
                    }
                }
                // Mid-dial: no composing before answer.
                Status::Connecting => {}
                // Idle: the keypad is an input method for the node field.
                // A-D are DTMF-only — not valid node characters — so they
                // render disabled; this guard backs the view up.
                Status::Disconnected => {
                    if !matches!(key, 'A'..='D') {
                        state.node_entry.push(key);
                    }
                }
            }
            Task::none()
        }
        Message::DialpadComposeChanged(s) => {
            // The Mac's DialpadComposer.filtered: paste-safe 16-key filter,
            // lowercase a-d uppercased, everything else dropped.
            state.dtmf_command = s
                .to_ascii_uppercase()
                .chars()
                .filter(|c| c.is_ascii_digit() || matches!(c, '*' | '#' | 'A'..='D'))
                .collect();
            Task::none()
        }
        Message::DialpadSend => {
            // Send is gated like the Mac's DialpadComposer.canSend: a
            // command, a connected call, nothing already playing. Engine
            // validation is all-or-nothing — the refreshed snapshot says
            // whether a sequence actually started; if not (error on the
            // status card), the command stays editable in the field.
            if !state.dtmf_command.is_empty()
                && state.snap.status == Status::Connected
                && state.dtmf_playing.is_none()
            {
                state.conn.send_dtmf_string(&state.dtmf_command);
                state.snap = state.conn.snapshot();
                if state.snap.dtmf_total > 0 {
                    state.dtmf_playing = Some(std::mem::take(&mut state.dtmf_command));
                }
            }
            Task::none()
        }
        Message::DialpadStop => {
            // Drop the un-played remainder; the played prefix still counts
            // in the history line (clamped like the Mac's progressSplit).
            if let Some(playing) = state.dtmf_playing.take() {
                state.conn.cancel_dtmf();
                let played = state.snap.dtmf_played as usize;
                let prefix: String = playing.chars().take(played).collect();
                if !prefix.is_empty() {
                    state.dtmf_history.push(prefix);
                }
                state.snap = state.conn.snapshot();
            }
            Task::none()
        }
        Message::DialpadClear => {
            state.dtmf_command.clear();
            Task::none()
        }
        Message::FavoriteToggle => {
            let node = state.node_entry.trim().to_string();
            if node.is_empty() {
                // Inert with no node entered (the Mac disables the star).
            } else if state.settings.is_favorite(&node) {
                state.settings.remove_favorite(&node);
                state.persist_settings();
            } else {
                // Open the inline label editor, the draft defaulting to the
                // node number (mirrors the Mac's favorite-editor popover).
                state.favorite_editor = Some(node);
            }
            Task::none()
        }
        Message::FavoriteLabelChanged(label) => {
            if state.favorite_editor.is_some() {
                state.favorite_editor = Some(label);
            }
            Task::none()
        }
        Message::FavoriteSave => {
            let node = state.node_entry.trim().to_string();
            if !node.is_empty() {
                if let Some(label) = state.favorite_editor.take() {
                    state.settings.add_favorite(&node, &label);
                    state.persist_settings();
                }
            }
            Task::none()
        }
        Message::FavoriteCancel => {
            state.favorite_editor = None;
            Task::none()
        }
        Message::DialFavorite(node) => {
            // A saved node carries its own network (astar-9b3e) — switch to
            // it before dialing, so a Hamlink favorite doesn't get filtered
            // or placeholder-labeled as an AllStar dial (and vice versa).
            if let Some(network) = state
                .settings
                .directory
                .iter()
                .find(|e| e.node == node)
                .map(|e| e.network)
            {
                state.network = network;
                state.settings.network = network;
                state.persist_settings();
            }
            // Fill the node field and dial — literally the same code path as
            // typing the node and pressing Connect.
            state.node_entry = node;
            update(state, Message::Connect)
        }
        Message::RemoveFavorite(node) => {
            state.settings.remove_favorite(&node);
            state.persist_settings();
            Task::none()
        }
        // Toggles: apply live AND persist immediately (mirrors the Mac, where
        // every switch writes through to the settings store on flip).
        Message::FullDuplex(on) => {
            state.settings.audio.full_duplex = on;
            state.apply_audio();
            state.persist_settings();
            Task::none()
        }
        // These bind to the M17 override (`settings.m17_audio`) instead of
        // the shared `settings.audio` whenever `m17_context()` is true — M17
        // gets its own tuned profile (astar-5d8e; mic level joined at
        // astar-m17defaults), edited here without disturbing what AllStar is
        // tuned to. `apply_audio` always pushes the right one (see
        // `effective_audio`). `Message::InputGain` below joins this group.
        Message::NoiseReduction(on) => {
            if state.m17_context() {
                state.settings.m17_audio.noise_reduction = on;
            } else {
                state.settings.audio.noise_reduction = on;
            }
            state.apply_audio();
            state.persist_settings();
            Task::none()
        }
        Message::Compression(on) => {
            if state.m17_context() {
                state.settings.m17_audio.compression = on;
            } else {
                state.settings.audio.compression = on;
            }
            state.apply_audio();
            state.persist_settings();
            Task::none()
        }
        // Sliders: apply live while dragging; the disk write waits for the
        // release (`SaveAudio`), so a drag isn't a hundred TOML writes.
        // Mic Level binds to the M17 override in M17 context (astar-m17defaults),
        // like the other four TX-processing controls below.
        Message::InputGain(gain) => {
            if state.m17_context() {
                state.settings.m17_audio.input_gain = gain;
            } else {
                state.settings.audio.input_gain = gain;
            }
            state.apply_audio();
            Task::none()
        }
        Message::CompressionLevel(level) => {
            if state.m17_context() {
                state.settings.m17_audio.compression_level = level;
            } else {
                state.settings.audio.compression_level = level;
            }
            state.apply_audio();
            Task::none()
        }
        Message::TxTrim(gain) => {
            if state.m17_context() {
                state.settings.m17_audio.tx_trim = gain;
            } else {
                state.settings.audio.tx_trim = gain;
            }
            state.apply_audio();
            Task::none()
        }
        Message::OutputGain(gain) => {
            state.settings.audio.output_gain = gain;
            state.apply_audio();
            Task::none()
        }
        // RX/output compression (iax-a4e7): shared across networks — unlike
        // the TX-processing group above, this never binds to the M17
        // override (output is listener-side).
        Message::RxCompression(on) => {
            state.settings.audio.rx_compression = on;
            state.apply_audio();
            state.persist_settings();
            Task::none()
        }
        Message::RxCompressionLevel(level) => {
            state.settings.audio.rx_compression_level = level;
            state.apply_audio();
            Task::none()
        }
        Message::VoxThreshold(dbfs) => {
            state.settings.audio.vox_threshold_dbfs = dbfs;
            state.apply_audio();
            Task::none()
        }
        Message::VoxHangtime(ms) => {
            state.settings.audio.vox_hangtime_ms = ms;
            state.apply_audio();
            Task::none()
        }
        Message::SaveAudio => {
            state.persist_settings();
            Task::none()
        }
        Message::SetupSelected(id) => {
            state.apply_setup(&id);
            Task::none()
        }
        Message::SetupNew => {
            state.setup_editor = Some(String::new());
            Task::none()
        }
        Message::SetupNameChanged(name) => {
            if state.setup_editor.is_some() {
                state.setup_editor = Some(name);
            }
            Task::none()
        }
        Message::SetupCreate => {
            if let Some(draft) = state.setup_editor.take() {
                // A blank name falls back to the Mac's `addNew` placeholder.
                let name = draft.trim();
                let name = if name.is_empty() {
                    "New config".to_string()
                } else {
                    name.to_string()
                };
                // Where the Mac's addNew creates a blank card to edit in
                // Settings, gui-rs has no editor card — so create snapshots
                // the current rig (like the Mac's first-run seed) and selects
                // it. Applying is pointless: it already equals the live state.
                let mut setup = Setup {
                    id: fresh_setup_id(),
                    name,
                    hardware_profile_id: "headset".to_string(),
                    ..Setup::default()
                };
                setup.capture_from(&state.settings.audio);
                state.settings.selected_setup = Some(setup.id.clone());
                state.settings.upsert_setup(setup);
                state.persist_settings();
            }
            Task::none()
        }
        Message::SetupCreateCancel => {
            state.setup_editor = None;
            Task::none()
        }
        Message::SetupSaveCurrent => {
            state.save_current_to_selected();
            Task::none()
        }
        Message::SetupDelete => {
            // Delete the selected setup — mirrors `SetupController.delete`:
            // the selection clears, and so does the launch default if it
            // pointed here. The built-in None can't be deleted.
            let selected = state.settings.selected_setup.clone();
            if let Some(id) = selected.filter(|id| id != NONE_SETUP_ID) {
                state.settings.remove_setup(&id);
                state.settings.selected_setup = None;
                if state.settings.default_setup.as_deref() == Some(id.as_str()) {
                    state.settings.default_setup = None;
                }
                state.persist_settings();
            }
            Task::none()
        }
        Message::Captured(shot) => {
            if let Mode::Shot { file, .. } = &state.mode {
                if let Err(e) = save_png(&shot, file) {
                    eprintln!("astar: failed to write screenshot {}: {e}", file.display());
                } else {
                    eprintln!(
                        "astar: wrote {} ({}x{})",
                        file.display(),
                        shot.size.width,
                        shot.size.height
                    );
                }
            }
            // Done — quit the app so `--shot` is a one-shot CLI.
            iced::exit()
        }
        Message::MainWindow(id) => {
            state.main_window = id;
            Task::none()
        }
        Message::Tray(ev) => match ev {
            tray::Event::Toggle { anchor } => toggle_window(state, anchor),
            tray::Event::ShowHide => toggle_window(state, None),
            tray::Event::Quit => iced::exit(),
        },
        Message::CloseRequested => {
            if state.tray_active {
                // The Mac's close-just-hides: the tray asterisk stays as the way
                // back in, and the app keeps running.
                state.window_visible = false;
                match state.main_window {
                    Some(id) => window::set_mode(id, window::Mode::Hidden),
                    None => Task::none(),
                }
            } else {
                // No tray (fallback / GNOME without the extension): closing
                // the only window quits, or the app would be unreachable.
                iced::exit()
            }
        }
        Message::PopoverScale(scale) => match state.main_window {
            Some(id) => window::size(id).map(move |size| Message::PopoverSize { scale, size }),
            None => Task::none(),
        },
        Message::PopoverSize { scale, size } => match state.main_window {
            Some(id) => window::monitor_size(id).map(move |monitor| Message::PopoverPlace {
                scale,
                size,
                monitor,
            }),
            None => Task::none(),
        },
        Message::PopoverPlace {
            scale,
            size,
            monitor,
        } => match (state.main_window, state.tray_anchor) {
            (Some(id), Some(anchor)) => {
                window::move_to(id, tray::popover_point(anchor, size, monitor, scale))
            }
            _ => Task::none(),
        },
    }
}

/// Show or hide the main window from the tray — the Mac star's left click.
/// The FIRST show also kicks off the popover placement pipeline (position the
/// window near the tray icon); afterwards it reappears wherever the user last
/// put it, matching the Mac's position-under-the-star-once behavior.
fn toggle_window(state: &mut State, anchor: Option<tray::Anchor>) -> Task<Message> {
    let Some(id) = state.main_window else {
        // The id probe hasn't landed yet (sub-second startup race) — the
        // next click will find it.
        return Task::none();
    };
    if state.window_visible {
        state.window_visible = false;
        window::set_mode(id, window::Mode::Hidden)
    } else {
        state.window_visible = true;
        let mut tasks = vec![
            window::set_mode(id, window::Mode::Windowed),
            window::gain_focus(id),
        ];
        if !state.popover_placed && anchor.is_some() {
            state.popover_placed = true;
            state.tray_anchor = anchor;
            tasks.push(window::scale_factor(id).map(Message::PopoverScale));
        }
        Task::batch(tasks)
    }
}

/// The view.
pub fn view(state: &State) -> Element<'_, Message> {
    view::view(
        &state.snap,
        &state.node_entry,
        view::Favorites {
            open: state.favorites_open,
            entry_is_favorite: state.settings.is_favorite(state.node_entry.trim()),
            editor: state.favorite_editor.as_deref(),
            favorites: state.settings.favorites(),
            recents: state.settings.recents(),
        },
        view::Dialpad {
            open: state.dialpad_open,
            command: &state.dtmf_command,
            playing: state
                .dtmf_playing
                .as_deref()
                .map(|c| (c, state.snap.dtmf_played)),
            history: &state.dtmf_history,
        },
        view::QuickConfig {
            open: state.quick_config_open,
            inputs: &state.inputs,
            outputs: &state.outputs,
            audio: &state.settings.audio,
            m17_audio: &state.settings.m17_audio,
            m17_context: state.m17_context(),
            setups: &state.settings.setups,
            selected_setup: state.settings.selected_setup.as_deref(),
            setup_editor: state.setup_editor.as_deref(),
        },
        view::NetworkInfo {
            selected: state.network,
            available: &state.network_available,
            m17_callsign: &state.settings.m17_callsign,
            m17_callsign_draft: &state.m17_callsign_draft,
        },
        state.tray_hint,
    )
}

/// The subscriptions: the poll heartbeat, spacebar hold-to-talk (press keys,
/// release unkeys; `update` gates keying on an answered call), and the close
/// button (routed as a message so it can hide-to-tray instead of quitting).
pub fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        poll::subscription(),
        iced::event::listen_with(space_ptt),
        window::close_requests().map(|_| Message::CloseRequested),
    ])
}

/// Map spacebar events to PTT. Presses only count when no widget consumed
/// them (so typing in a focused field can never key the radio); releases
/// always count (an unkey must never be lost).
fn space_ptt(
    event: iced::Event,
    status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    use iced::keyboard::{key::Named, Event as KeyEvent, Key};

    match (event, status) {
        (
            iced::Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Named(Named::Space),
                ..
            }),
            iced::event::Status::Ignored,
        ) => Some(Message::Ptt(true)),
        (
            iced::Event::Keyboard(KeyEvent::KeyReleased {
                key: Key::Named(Named::Space),
                ..
            }),
            _,
        ) => Some(Message::Ptt(false)),
        _ => None,
    }
}

/// A unique id for a new Setup: Unix nanos plus a process-wide counter. The
/// Mac uses UUID strings; upsert/select only need uniqueness within one
/// settings file, so this avoids a uuid dependency.
fn fresh_setup_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    format!("setup-{nanos:x}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// The raw persisted-string form of a [`Network`] — the inverse of the
/// literals [`Network::resolve`] parses, so a `Settings.network` already
/// decoded to an enum can still be run back through the availability check
/// at boot without duplicating `resolve`'s fallback logic.
fn persisted_network_raw(n: Network) -> &'static str {
    match n {
        Network::Allstar => "allstar",
        Network::Hamlink => "hamlink",
        Network::M17 => "m17",
    }
}

/// Seconds since the Unix epoch — the `last_used` clock for recents.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Encode an Iced [`window::Screenshot`] (RGBA, sRGB) to a PNG on disk.
fn save_png(shot: &window::Screenshot, path: &std::path::Path) -> Result<(), String> {
    let (w, h) = (shot.size.width, shot.size.height);
    let buf = image::RgbaImage::from_raw(w, h, shot.rgba.to_vec())
        .ok_or_else(|| format!("RGBA buffer size mismatch for {w}x{h}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    buf.save(path).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Status;

    fn demo_state(scene: DemoState) -> State {
        State::boot(Mode::Demo(scene)).0
    }

    /// Drive one poll tick so `snap` refreshes from the conn.
    fn tick(state: &mut State) {
        let _ = update(state, Message::Tick);
    }

    #[test]
    fn node_entry_admits_node_and_address_characters_only() {
        let mut s = demo_state(DemoState::Idle);
        // `*` and `#` are valid node-string characters (command prefixes) —
        // the Mac's field takes them and the dialpad appends them idle.
        let _ = update(&mut s, Message::NodeEntryChanged("*3 546054#".into()));
        assert_eq!(s.node_entry, "*3546054#", "spaces never land in the field");
        // The smart field (astar-3939) also admits hostname/IP shapes:
        // letters, dots, colons, hyphens; anything else is dropped as typed
        // or pasted (mirrors the Mac's onChange filter).
        let _ = update(
            &mut s,
            Message::NodeEntryChanged("my node.example-net.com:4569!".into()),
        );
        assert_eq!(s.node_entry, "mynode.example-net.com:4569");
    }

    #[test]
    fn connect_with_empty_entry_is_a_no_op() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    #[test]
    fn connect_with_unparseable_entry_is_a_no_op() {
        // The button is disabled for unparseable text, but on_submit still
        // fires — refuse it the same way as empty input (the Mac's guard).
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NodeEntryChanged("*".into()));
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(
            s.snap.status,
            Status::Disconnected,
            "a bare command prefix is not a dial"
        );
    }

    #[test]
    fn address_dial_routes_directly_and_records_the_recent_under_the_address() {
        // The smart dial field passes the typed address as BOTH the display
        // label and the dial target (astar-3939, the Mac's astar-427f). On
        // answer it must be recorded as a recent keyed by the address string
        // (the settings directory keys by string, no numeric assumption).
        let mut s = demo_state(DemoState::Idle);
        let _ = update(
            &mut s,
            Message::NodeEntryChanged("node.example-net.com:4569".into()),
        );
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connecting);
        assert_eq!(
            s.snap.dialed_node.as_deref(),
            Some("node.example-net.com:4569")
        );

        while s.snap.status != Status::Connected {
            tick(&mut s);
        }
        let e = saved_entry(&s, "node.example-net.com:4569")
            .expect("an answered address dial records a recent under the address string");
        assert_eq!(e.label, "node.example-net.com:4569");
        assert!(e.last_used.is_some());
    }

    #[test]
    fn connect_dials_the_entered_node() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NodeEntryChanged("546054".into()));
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connecting);
        assert_eq!(s.snap.dialed_node.as_deref(), Some("546054"));
    }

    #[test]
    fn boot_seeds_demo_devices_and_pushes_selection_to_the_conn() {
        let s = demo_state(DemoState::Idle);
        // The demo boots with the seeded default Setup ("UCI150 desk")
        // applied — its devices land in the settings and are pushed through
        // the seam, so shots show real picker text with a rig selected.
        assert_eq!(s.settings.audio.input.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(s.settings.audio.output.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(
            s.conn.selected_devices(),
            (
                Some("USB Audio CODEC".into()),
                Some("USB Audio CODEC".into())
            )
        );
        // And the pickers have lists to show.
        assert!(!s.inputs.is_empty() && !s.outputs.is_empty());
    }

    #[test]
    fn selecting_a_device_persists_it_and_pushes_it_to_the_conn() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::InputDevice(Some("Demo Microphone".into())));
        assert_eq!(
            s.conn.selected_devices().0.as_deref(),
            Some("Demo Microphone")
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.audio.input.as_deref(), Some("Demo Microphone"));
        // Output side untouched by an input pick (still the active setup's).
        assert_eq!(saved.audio.output.as_deref(), Some("USB Audio CODEC"));
    }

    #[test]
    fn selecting_system_default_persists_none() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::OutputDevice(None));
        assert_eq!(s.conn.selected_devices().1, None);
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.audio.output, None);
    }

    // -- quick config (astar-f4d9) ---------------------------------------

    #[test]
    fn boot_pushes_audio_prefs_to_the_conn() {
        let s = demo_state(DemoState::Idle);
        assert_eq!(
            s.conn.audio(),
            s.settings.audio,
            "boot pushes standing prefs"
        );
    }

    #[test]
    fn toggles_apply_live_and_persist_immediately() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::Compression(true));
        let _ = update(&mut s, Message::NoiseReduction(true));
        let _ = update(&mut s, Message::FullDuplex(true));

        let pushed = s.conn.audio();
        assert!(pushed.compression && pushed.noise_reduction && pushed.full_duplex);

        let saved = s.store.load().expect("store readable");
        assert!(saved.audio.compression, "toggle writes through on flip");
        assert!(saved.audio.noise_reduction && saved.audio.full_duplex);
    }

    #[test]
    fn sliders_apply_live_but_persist_on_release() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::TxTrim(0.4));
        assert_eq!(s.conn.audio().tx_trim, 0.4, "drag applies live");
        let saved = s.store.load().expect("store readable");
        // Disk still holds the boot-applied setup's trim, not the drag.
        assert_eq!(saved.audio.tx_trim, 0.9, "disk untouched until release");

        let _ = update(&mut s, Message::SaveAudio);
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.audio.tx_trim, 0.4, "release persists");
    }

    #[test]
    fn every_slider_reaches_the_conn() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::InputGain(1.3));
        let _ = update(&mut s, Message::CompressionLevel(0.25));
        // 100%-400% range (iax-a4e7) — 2.5 is a mid-range value in the new
        // UI floor, not the pre-astar-outchain 0…2 range.
        let _ = update(&mut s, Message::OutputGain(2.5));
        let _ = update(&mut s, Message::RxCompressionLevel(0.65));
        let _ = update(&mut s, Message::VoxThreshold(-25.0));
        let _ = update(&mut s, Message::VoxHangtime(1200));

        let pushed = s.conn.audio();
        assert_eq!(pushed.input_gain, 1.3);
        assert_eq!(pushed.compression_level, 0.25);
        assert_eq!(pushed.output_gain, 2.5);
        assert_eq!(pushed.rx_compression_level, 0.65);
        assert_eq!(pushed.vox_threshold_dbfs, -25.0);
        assert_eq!(pushed.vox_hangtime_ms, 1200);
    }

    #[test]
    fn rx_compression_toggle_applies_live_and_persists_immediately() {
        // Mirrors `toggles_apply_live_and_persist_immediately`: RX compression
        // is a shared listener-side setting (no M17 override), so it follows
        // the same toggle contract as Compression/NoiseReduction/FullDuplex.
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::RxCompression(true));

        assert!(s.conn.audio().rx_compression, "applies live");
        let saved = s.store.load().expect("store readable");
        assert!(saved.audio.rx_compression, "toggle writes through on flip");
    }

    #[test]
    fn rx_compression_level_applies_live_but_persists_on_release() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::RxCompressionLevel(0.42));
        assert_eq!(
            s.conn.audio().rx_compression_level,
            0.42,
            "drag applies live"
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(
            saved.audio.rx_compression_level, 0.90,
            "disk untouched until release"
        );

        let _ = update(&mut s, Message::SaveAudio);
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.audio.rx_compression_level, 0.42, "release persists");
    }

    #[test]
    fn quick_config_toggles_open_and_closed() {
        let mut s = demo_state(DemoState::Idle);
        assert!(
            !s.quick_config_open,
            "starts collapsed, like the Mac default"
        );
        let _ = update(&mut s, Message::QuickConfigToggled);
        assert!(s.quick_config_open);
        let _ = update(&mut s, Message::QuickConfigToggled);
        assert!(!s.quick_config_open);
    }

    #[test]
    fn shot_scene_parses_config_variants_and_states() {
        assert_eq!(
            ShotScene::parse("config"),
            Some(ShotScene::Config { compression: false })
        );
        assert_eq!(
            ShotScene::parse("CONFIG-COMP"),
            Some(ShotScene::Config { compression: true })
        );
        assert_eq!(ShotScene::parse("config-new"), Some(ShotScene::ConfigNew));
        assert_eq!(
            ShotScene::parse("tx"),
            Some(ShotScene::State(DemoState::Tx))
        );
        assert_eq!(ShotScene::parse("bogus"), None);
    }

    #[test]
    fn config_new_shot_boots_open_with_the_editor_drafted() {
        let (s, _) = State::boot(Mode::Shot {
            scene: ShotScene::ConfigNew,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.quick_config_open, "renders the panel expanded");
        assert!(
            s.setup_editor.as_deref().is_some_and(|d| !d.is_empty()),
            "with a drafted name in the editor"
        );
    }

    #[test]
    fn config_shot_boots_open_with_the_chosen_compression() {
        let (s, _) = State::boot(Mode::Shot {
            scene: ShotScene::Config { compression: true },
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(
            s.quick_config_open,
            "config shots render the panel expanded"
        );
        assert!(
            s.settings.audio.compression,
            "-comp variant forces compression on"
        );
        assert!(s.conn.audio().compression, "and pushes it through the seam");

        let (s, _) = State::boot(Mode::Shot {
            scene: ShotScene::Config { compression: false },
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.quick_config_open);
        assert!(!s.settings.audio.compression);
    }

    // -- setups (astar-80ae) -------------------------------------------------
    // Mirror SetupController: apply merges Optional overrides onto the
    // globals; None reverts devices; saveCurrentToSelected snapshots; delete
    // clears selection/default; a missing device refuses the apply.

    /// The persisted setup with `id`, if any.
    fn saved_setup(s: &State, id: &str) -> Option<crate::settings::Setup> {
        let saved = s.store.load().expect("store readable");
        saved.setups.into_iter().find(|x| x.id == id)
    }

    #[test]
    fn boot_applies_the_demo_default_setup() {
        let s = demo_state(DemoState::Idle);
        // The seeded launch default ("UCI150 desk") applied at boot — its
        // overrides landed on the globals (mirrors SetupController.attach)…
        assert_eq!(s.settings.selected_setup.as_deref(), Some("demo-uci150"));
        assert_eq!(s.settings.audio.input_gain, 0.75);
        assert_eq!(s.settings.audio.output_gain, 1.10);
        assert!(s.settings.audio.compression && s.settings.audio.noise_reduction);
        assert_eq!(s.settings.audio.tx_trim, 0.9);
        // …were pushed through the seam (the station re-pushes standing prefs
        // on every (re)connect, so these effective values survive reconnects)…
        assert_eq!(s.conn.audio(), s.settings.audio);
        // …and non-overridden knobs kept their global defaults.
        assert_eq!(s.settings.audio.vox_threshold_dbfs, -40.0);
        assert!(!s.settings.audio.full_duplex);
    }

    #[test]
    fn selecting_a_setup_applies_overrides_and_persists_the_selection() {
        let mut s = demo_state(DemoState::Idle);
        // Switch from the boot default (UCI150 desk) to Jabra mobile: its
        // devices apply, and its None overrides keep the standing values.
        let _ = update(&mut s, Message::SetupSelected("demo-headset".into()));
        assert_eq!(
            s.conn.selected_devices(),
            (Some("Demo Microphone".into()), Some("Demo Speakers".into()))
        );
        assert_eq!(
            s.settings.audio.input_gain, 0.75,
            "None override = keep global"
        );
        assert!(s.settings.audio.compression, "None override = keep global");
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.selected_setup.as_deref(), Some("demo-headset"));
        assert_eq!(saved.audio.input.as_deref(), Some("Demo Microphone"));
    }

    #[test]
    fn selecting_none_reverts_devices_and_keeps_the_knobs() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::SetupSelected(NONE_SETUP_ID.into()));
        // Devices back to the system default, pushed and persisted…
        assert_eq!(s.conn.selected_devices(), (None, None));
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.audio.input, None);
        assert_eq!(saved.audio.output, None);
        // …the explicit None pick is remembered (the Mac persists noneID)…
        assert_eq!(saved.selected_setup.as_deref(), Some(NONE_SETUP_ID));
        // …and the audio knobs stand as the previous setup left them.
        assert_eq!(s.settings.audio.input_gain, 0.75);
        assert!(s.settings.audio.compression);
    }

    #[test]
    fn selecting_an_unknown_setup_is_a_no_op() {
        let mut s = demo_state(DemoState::Idle);
        let before = s.settings.clone();
        let _ = update(&mut s, Message::SetupSelected("gone".into()));
        assert_eq!(
            s.settings, before,
            "unknown ids change nothing, like the Mac"
        );
        assert!(s.setup_error.is_none());
    }

    #[test]
    fn apply_refuses_when_a_named_device_is_unplugged() {
        let mut s = demo_state(DemoState::Idle);
        s.settings.upsert_setup(crate::settings::Setup {
            id: "s-gone".into(),
            name: "Unplugged rig".into(),
            hardware_profile_id: "uci150".into(),
            input_device: Some("UCI150 Audio".into()),
            output_device: Some("UCI150 Audio".into()),
            input_gain: Some(1.5),
            ..crate::settings::Setup::default()
        });
        let _ = update(&mut s, Message::SetupSelected("s-gone".into()));

        // Refused: the error names the rig and device, and NOTHING changed —
        // selection, devices, and knobs all stand (mirrors the Mac's guard).
        let err = s.setup_error.clone().expect("refused apply sets the error");
        assert!(err.contains("Unplugged rig") && err.contains("UCI150 Audio"));
        assert_eq!(s.settings.selected_setup.as_deref(), Some("demo-uci150"));
        assert_eq!(s.settings.audio.input_gain, 0.75, "override not applied");
        assert_eq!(
            s.conn.selected_devices().0.as_deref(),
            Some("USB Audio CODEC")
        );

        // The error rides the status card…
        tick(&mut s);
        assert_eq!(s.snap.error, Some(err));

        // …and clears on the next successful apply, like the Mac's lastError.
        let _ = update(&mut s, Message::SetupSelected("demo-headset".into()));
        assert!(s.setup_error.is_none());
    }

    #[test]
    fn save_current_to_selected_snapshots_the_rig() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::SetupSelected("demo-headset".into()));
        // Tweak the live state, then save it into the selected setup.
        let _ = update(&mut s, Message::InputGain(1.25));
        let _ = update(&mut s, Message::FullDuplex(true));
        let _ = update(&mut s, Message::SetupSaveCurrent);

        let saved = saved_setup(&s, "demo-headset").expect("setup persisted");
        assert_eq!(saved.input_gain, Some(1.25));
        assert_eq!(saved.full_duplex, Some(true));
        assert_eq!(saved.input_device.as_deref(), Some("Demo Microphone"));
        assert_eq!(saved.name, "Jabra mobile", "identity untouched");

        // Re-selecting it later restores the snapshot.
        let _ = update(&mut s, Message::SetupSelected(NONE_SETUP_ID.into()));
        let _ = update(&mut s, Message::InputGain(0.5));
        let _ = update(&mut s, Message::SetupSelected("demo-headset".into()));
        assert_eq!(s.settings.audio.input_gain, 1.25);
        assert!(s.settings.audio.full_duplex);
    }

    #[test]
    fn save_current_is_a_no_op_for_none() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::SetupSelected(NONE_SETUP_ID.into()));
        let before = s.store.load().expect("store readable");
        let _ = update(&mut s, Message::SetupSaveCurrent);
        assert_eq!(s.store.load().expect("store readable"), before);
    }

    #[test]
    fn new_setup_editor_creates_a_named_snapshot_and_selects_it() {
        let mut s = demo_state(DemoState::Idle);
        assert!(s.setup_editor.is_none(), "editor starts closed");
        let _ = update(&mut s, Message::SetupNew);
        assert_eq!(
            s.setup_editor.as_deref(),
            Some(""),
            "opens with a blank draft"
        );
        let _ = update(&mut s, Message::SetupNameChanged("Shack rig".into()));
        let _ = update(&mut s, Message::SetupCreate);
        assert!(s.setup_editor.is_none(), "create closes the editor");

        // The new setup snapshots the current rig (the boot default's state)
        // and becomes the selection.
        let id = s
            .settings
            .selected_setup
            .clone()
            .expect("new setup selected");
        assert_ne!(id, "demo-uci150");
        let saved = saved_setup(&s, &id).expect("new setup persisted");
        assert_eq!(saved.name, "Shack rig");
        assert_eq!(saved.input_device.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(saved.input_gain, Some(0.75));
        assert_eq!(saved.compression, Some(true));
        assert_eq!(s.settings.setups.len(), 3, "appended after the demo setups");
    }

    #[test]
    fn new_setup_blank_name_defaults_and_ids_stay_unique() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::SetupNew);
        let _ = update(&mut s, Message::SetupCreate);
        let first = s.settings.selected_setup.clone().expect("first created");
        assert_eq!(
            saved_setup(&s, &first).expect("persisted").name,
            "New config",
            "blank name falls back to the Mac's addNew placeholder"
        );
        let _ = update(&mut s, Message::SetupNew);
        let _ = update(&mut s, Message::SetupCreate);
        let second = s.settings.selected_setup.clone().expect("second created");
        assert_ne!(first, second, "every create gets a fresh id");
        assert_eq!(s.settings.setups.len(), 4);
    }

    #[test]
    fn new_setup_cancel_discards() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::SetupNew);
        let _ = update(&mut s, Message::SetupNameChanged("Draft".into()));
        let _ = update(&mut s, Message::SetupCreateCancel);
        assert!(s.setup_editor.is_none());
        assert_eq!(s.settings.setups.len(), 2, "nothing created on cancel");
        // Create without opening the editor is inert too.
        let _ = update(&mut s, Message::SetupCreate);
        assert_eq!(s.settings.setups.len(), 2);
    }

    #[test]
    fn delete_removes_the_selection_and_clears_the_default() {
        let mut s = demo_state(DemoState::Idle);
        // The boot default (demo-uci150) is selected AND the launch default.
        let _ = update(&mut s, Message::SetupDelete);
        assert!(
            saved_setup(&s, "demo-uci150").is_none(),
            "removed from the store"
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.selected_setup, None, "selection cleared");
        assert_eq!(saved.default_setup, None, "launch default cleared");
        assert_eq!(saved.setups.len(), 1, "the other setup remains");

        // Deleting with None selected is a no-op (None can't be deleted).
        let _ = update(&mut s, Message::SetupSelected(NONE_SETUP_ID.into()));
        let _ = update(&mut s, Message::SetupDelete);
        assert_eq!(s.settings.setups.len(), 1);
    }

    #[test]
    fn device_picks_write_through_to_the_active_setup() {
        let mut s = demo_state(DemoState::Idle);
        // With UCI150 desk active, re-picking a device must land in the setup
        // too, else it re-applies its old device at the next launch (mirrors
        // SetupController.setActiveDevices).
        let _ = update(&mut s, Message::InputDevice(Some("Demo Microphone".into())));
        let saved = saved_setup(&s, "demo-uci150").expect("setup persisted");
        assert_eq!(saved.input_device.as_deref(), Some("Demo Microphone"));
        assert_eq!(
            saved.output_device.as_deref(),
            Some("USB Audio CODEC"),
            "output untouched"
        );

        // With None active there is no setup to write into.
        let _ = update(&mut s, Message::SetupSelected(NONE_SETUP_ID.into()));
        let _ = update(&mut s, Message::OutputDevice(Some("Demo Speakers".into())));
        let saved = saved_setup(&s, "demo-uci150").expect("setup persisted");
        assert_eq!(saved.output_device.as_deref(), Some("USB Audio CODEC"));
    }

    // -- favorites + recents (astar-ac65) ----------------------------------

    /// Find the persisted directory entry for `node`, if any.
    fn saved_entry(s: &State, node: &str) -> Option<crate::settings::NodeEntry> {
        let saved = s.store.load().expect("store readable");
        saved.directory.into_iter().find(|e| e.node == node)
    }

    #[test]
    fn boot_seeds_demo_directory() {
        let s = demo_state(DemoState::Idle);
        assert!(
            !s.settings.favorites().is_empty(),
            "demo favorites render in shots"
        );
        assert!(
            !s.settings.recents().is_empty(),
            "demo recents render in shots"
        );
    }

    #[test]
    fn successful_connect_records_a_recent_on_the_answered_edge() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NodeEntryChanged("12345".into()));
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connecting);
        assert!(
            saved_entry(&s, "12345").is_none(),
            "dialing alone records nothing"
        );

        while s.snap.status != Status::Connected {
            tick(&mut s);
        }
        let e = saved_entry(&s, "12345").expect("answered call recorded as recent");
        assert_eq!(e.label, "12345", "unnamed recents are labeled by node");
        assert!(!e.favorite);
        assert!(e.last_used.is_some());
    }

    #[test]
    fn redialing_a_saved_node_updates_rather_than_duplicates() {
        let mut s = demo_state(DemoState::Idle);
        // The demo directory already holds 546054 as a favorite ("W6ABC
        // Repeater") — a successful call to it must bump last_used in place.
        let seeded_last_used = s
            .settings
            .directory
            .iter()
            .find(|e| e.node == "546054")
            .unwrap()
            .last_used;

        let _ = update(&mut s, Message::NodeEntryChanged("546054".into()));
        let _ = update(&mut s, Message::Connect);
        while s.snap.status != Status::Connected {
            tick(&mut s);
        }
        let saved = s.store.load().expect("store readable");
        let entries: Vec<_> = saved
            .directory
            .iter()
            .filter(|e| e.node == "546054")
            .collect();
        assert_eq!(entries.len(), 1, "upsert by node — no duplicate");
        assert_eq!(
            entries[0].label, "W6ABC Repeater",
            "curated label preserved"
        );
        assert!(entries[0].favorite, "favorite flag preserved");
        assert!(entries[0].last_used >= seeded_last_used, "recency bumped");
    }

    #[test]
    fn clicking_a_saved_node_dials_it_via_the_connect_path() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::DialFavorite("77777".into()));
        tick(&mut s);
        assert_eq!(
            s.node_entry, "77777",
            "the row fills the node field, like typing"
        );
        assert_eq!(s.snap.status, Status::Connecting);
        assert_eq!(s.snap.dialed_node.as_deref(), Some("77777"));
    }

    #[test]
    fn star_opens_the_label_editor_with_the_node_as_draft() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::FavoriteToggle);
        assert_eq!(
            s.favorite_editor, None,
            "star is inert with no node entered"
        );

        let _ = update(&mut s, Message::NodeEntryChanged("12345".into()));
        let _ = update(&mut s, Message::FavoriteToggle);
        assert_eq!(
            s.favorite_editor.as_deref(),
            Some("12345"),
            "label defaults to the node"
        );

        let _ = update(&mut s, Message::FavoriteLabelChanged("K7XYZ".into()));
        let _ = update(&mut s, Message::FavoriteSave);
        assert_eq!(s.favorite_editor, None, "save closes the editor");
        let e = saved_entry(&s, "12345").expect("favorite persisted");
        assert!(e.favorite);
        assert_eq!(e.label, "K7XYZ");
    }

    #[test]
    fn favorite_editor_cancel_discards() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NodeEntryChanged("12345".into()));
        let _ = update(&mut s, Message::FavoriteToggle);
        let _ = update(&mut s, Message::FavoriteCancel);
        assert_eq!(s.favorite_editor, None);
        assert!(
            saved_entry(&s, "12345").is_none(),
            "nothing saved on cancel"
        );
    }

    #[test]
    fn star_on_a_favorite_unfavorites_it() {
        let mut s = demo_state(DemoState::Idle);
        // 77777 is a seeded never-dialed favorite: the star removes it fully.
        let _ = update(&mut s, Message::NodeEntryChanged("77777".into()));
        assert!(s.settings.is_favorite("77777"));
        let _ = update(&mut s, Message::FavoriteToggle);
        assert!(!s.settings.is_favorite("77777"));
        assert_eq!(
            s.favorite_editor, None,
            "un-favoriting never opens the editor"
        );
        let saved = s.store.load().expect("store readable");
        assert!(
            !saved.directory.iter().any(|e| e.node == "77777"),
            "a never-dialed favorite is removed entirely"
        );
    }

    #[test]
    fn removing_a_favorite_row_keeps_its_recency() {
        let mut s = demo_state(DemoState::Idle);
        // 546054 is seeded as a favorite WITH a last_used: the row's remove
        // un-favorites but keeps it in recents (mirrors the Mac semantics).
        let _ = update(&mut s, Message::RemoveFavorite("546054".into()));
        assert!(!s.settings.is_favorite("546054"));
        assert!(
            s.settings.recents().iter().any(|e| e.node == "546054"),
            "still a recent after un-favoriting"
        );
    }

    #[test]
    fn favorites_section_toggles_open_and_closed() {
        let mut s = demo_state(DemoState::Idle);
        assert!(
            !s.favorites_open,
            "starts collapsed to keep the page compact"
        );
        let _ = update(&mut s, Message::FavoritesToggled);
        assert!(s.favorites_open);
        let _ = update(&mut s, Message::FavoritesToggled);
        assert!(!s.favorites_open);
    }

    #[test]
    fn favorites_shot_scene_parses_and_boots_open() {
        assert_eq!(ShotScene::parse("favorites"), Some(ShotScene::Favorites));
        let (s, _) = State::boot(Mode::Shot {
            scene: ShotScene::Favorites,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(
            s.favorites_open,
            "favorites shots render the section expanded"
        );
        assert!(
            !s.settings.favorites().is_empty(),
            "with seeded entries to show"
        );
    }

    // -- dialpad (astar-75fc) ----------------------------------------------

    #[test]
    fn dialpad_toggles_open_and_closed() {
        let mut s = demo_state(DemoState::Idle);
        assert!(!s.dialpad_open, "starts collapsed, like the Mac default");
        let _ = update(&mut s, Message::DialpadToggled);
        assert!(s.dialpad_open);
        let _ = update(&mut s, Message::DialpadToggled);
        assert!(!s.dialpad_open);
    }

    #[test]
    fn idle_dialpad_keys_fill_the_node_field() {
        let mut s = demo_state(DemoState::Idle);
        for key in ['*', '3', '5', '#'] {
            let _ = update(&mut s, Message::Dialpad(key));
        }
        assert_eq!(s.node_entry, "*35#", "idle taps append to the node field");
        assert!(s.conn.sent_dtmf().is_empty(), "idle taps never send tones");
        assert_eq!(s.dtmf_command, "", "composing is a connected-mode thing");
    }

    #[test]
    fn idle_letter_keys_are_inert() {
        // A-D are DTMF-only — not valid node characters (disabled on the Mac).
        let mut s = demo_state(DemoState::Idle);
        for key in ['A', 'B', 'C', 'D'] {
            let _ = update(&mut s, Message::Dialpad(key));
        }
        assert_eq!(s.node_entry, "");
        assert!(s.conn.sent_dtmf().is_empty());
    }

    #[test]
    fn connected_dialpad_keys_compose_without_sending() {
        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        for key in ['*', '3', '0', 'D'] {
            let _ = update(&mut s, Message::Dialpad(key));
        }
        assert_eq!(s.dtmf_command, "*30D", "taps build the composed command");
        assert!(
            s.conn.sent_dtmf().is_empty(),
            "composing must not touch the air"
        );
        assert!(s.conn.sent_dtmf_strings().is_empty());
        assert_eq!(
            s.node_entry, "",
            "connected taps never touch the node field"
        );
    }

    #[test]
    fn dialing_dialpad_keys_are_inert() {
        // Mid-dial: no composing before answer (mirrors the Mac's
        // answered-only gate), and no leaking into the node field either.
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NodeEntryChanged("546054".into()));
        let _ = update(&mut s, Message::Connect);
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connecting);
        let _ = update(&mut s, Message::Dialpad('1'));
        assert!(s.conn.sent_dtmf().is_empty());
        assert_eq!(s.dtmf_command, "");
        assert_eq!(s.node_entry, "546054");
    }

    #[test]
    fn compose_filter_uppercases_and_strips_junk() {
        let mut s = demo_state(DemoState::Connected);
        let _ = update(&mut s, Message::DialpadComposeChanged("*3 a460-x!".into()));
        assert_eq!(
            s.dtmf_command, "*3A460",
            "16-key paste-safe filter, uppercased"
        );
        let _ = update(&mut s, Message::DialpadClear);
        assert_eq!(s.dtmf_command, "");
    }

    #[test]
    fn send_moves_command_to_playing_and_engine() {
        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        let _ = update(&mut s, Message::DialpadComposeChanged("*30".into()));
        let _ = update(&mut s, Message::DialpadSend);
        assert_eq!(
            s.conn.sent_dtmf_strings(),
            vec!["*30"],
            "one Send = one command"
        );
        assert_eq!(
            s.dtmf_playing.as_deref(),
            Some("*30"),
            "the bar locks into progress"
        );
        assert_eq!(s.dtmf_command, "", "the field empties for the next command");
        assert!(
            s.snap.dtmf_total > 0,
            "the refreshed snapshot shows the sequence"
        );
    }

    #[test]
    fn sequence_completion_lands_in_history() {
        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        let _ = update(&mut s, Message::DialpadComposeChanged("*30".into()));
        let _ = update(&mut s, Message::DialpadSend);
        // 3 digits at TICKS_PER_DIGIT plus the retire slot — 40 is plenty.
        for _ in 0..40 {
            tick(&mut s);
        }
        assert!(
            s.dtmf_playing.is_none(),
            "the bar unlocks when the queue drains"
        );
        assert_eq!(
            s.dtmf_history,
            vec!["*30"],
            "the finished command joins the history"
        );
    }

    #[test]
    fn stop_cancels_and_keeps_played_prefix_in_history() {
        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        let _ = update(&mut s, Message::DialpadComposeChanged("*30546".into()));
        let _ = update(&mut s, Message::DialpadSend);
        for _ in 0..8 {
            tick(&mut s); // ~2 digits played
        }
        let played = s.snap.dtmf_played as usize;
        assert!(played >= 1, "precondition: something has played");
        let _ = update(&mut s, Message::DialpadStop);
        assert!(s.dtmf_playing.is_none());
        assert_eq!(s.dtmf_history.len(), 1, "the played prefix still counts");
        assert_eq!(s.dtmf_history[0].chars().count(), played);
        assert_eq!(s.snap.dtmf_total, 0, "cancel cleared the engine queue");
    }

    #[test]
    fn fresh_dial_resets_compose_and_history() {
        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        let _ = update(&mut s, Message::DialpadComposeChanged("*70".into()));
        let _ = update(&mut s, Message::DialpadSend);
        for _ in 0..40 {
            tick(&mut s); // completes into history
        }
        assert!(!s.dtmf_history.is_empty());
        let _ = update(&mut s, Message::Disconnect);
        let _ = update(&mut s, Message::NodeEntryChanged("2000".into()));
        let _ = update(&mut s, Message::Connect);
        assert_eq!(s.dtmf_command, "", "a fresh dial starts a fresh dialpad");
        assert!(s.dtmf_playing.is_none());
        assert!(s.dtmf_history.is_empty());
    }

    #[test]
    fn dialpad_shot_scene_parses_and_boots_open_in_call() {
        assert_eq!(
            ShotScene::parse("dialpad"),
            Some(ShotScene::Dialpad { connected: true })
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::Dialpad { connected: true },
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.dialpad_open, "dialpad shots render the section expanded");
        assert!(
            !s.dtmf_command.is_empty(),
            "with a seeded composed command to show"
        );
        assert!(!s.dtmf_history.is_empty(), "and a seeded history line");
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connected, "over the connected scene");
    }

    #[test]
    fn dialpad_idle_shot_scene_boots_open_and_disconnected() {
        assert_eq!(
            ShotScene::parse("dialpad-idle"),
            Some(ShotScene::Dialpad { connected: false })
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::Dialpad { connected: false },
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.dialpad_open);
        assert_eq!(s.dtmf_command, "", "nothing to compose in node-entry mode");
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    #[test]
    fn dialpad_sending_shot_scene_boots_mid_sequence() {
        assert_eq!(
            ShotScene::parse("dialpad-sending"),
            Some(ShotScene::DialpadSending)
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::DialpadSending,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.dialpad_open);
        assert_eq!(
            s.dtmf_playing.as_deref(),
            Some("*3546054"),
            "a command is playing"
        );
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connected);
        let (played, total) = (s.snap.dtmf_played, s.snap.dtmf_total);
        assert_eq!(total, 8);
        assert!(
            played >= 1 && played < total,
            "captured mid-sequence ({played}/{total})"
        );
    }

    // -- network picker (astar-9b3e) ---------------------------------------

    #[test]
    fn network_picker_is_hidden_when_only_allstar_exists() {
        let s = demo_state(DemoState::Idle);
        let caps = NetworkCaps {
            hamlink: s.conn.hamlink_available(),
            m17: s.conn.m17_available(),
        };
        assert_eq!(Network::available(caps), vec![Network::Allstar]);
    }

    #[test]
    fn picking_a_network_swaps_the_dial_filter() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::Hamlink));
        let _ = update(&mut s, Message::NodeEntryChanged("refl/9*".into()));
        assert_eq!(
            s.node_entry, "refl/9",
            "hamlink filter admits '/' and drops '*'"
        );
        let _ = update(&mut s, Message::NetworkPicked(Network::Allstar));
        let _ = update(&mut s, Message::NodeEntryChanged("refl/9*".into()));
        assert_eq!(
            s.node_entry, "refl9*",
            "allstar filter admits '*' and drops '/'"
        );
    }

    #[test]
    fn network_picker_shot_scene_boots_with_hamlink_available() {
        assert_eq!(
            ShotScene::parse("network-picker"),
            Some(ShotScene::NetworkPicker)
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::NetworkPicker,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.conn.hamlink_available());
        assert_eq!(
            s.network,
            Network::Allstar,
            "AllStar stays the default selection"
        );
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connected);
    }

    #[test]
    fn network_picker_idle_shot_scene_boots_with_hamlink_selected() {
        assert_eq!(
            ShotScene::parse("network-picker-idle"),
            Some(ShotScene::NetworkPickerIdle)
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::NetworkPickerIdle,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(s.conn.hamlink_available());
        assert_eq!(
            s.network,
            Network::Hamlink,
            "the idle shot seeds Hamlink as the selection"
        );
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    // -- M17 (M17 Task 10) --------------------------------------------------

    #[test]
    fn m17_dial_shot_scene_boots_with_m17_available_and_selected() {
        assert_eq!(ShotScene::parse("m17-dial"), Some(ShotScene::M17Dial));
        assert_eq!(
            ShotScene::parse("M17-DIAL"),
            Some(ShotScene::M17Dial),
            "case-insensitive"
        );
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::M17Dial,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert!(
            s.conn.m17_available(),
            "the scene flips the M17 capability knob"
        );
        assert!(
            !s.conn.hamlink_available(),
            "Hamlink stays off — only AllStar + M17 show"
        );
        assert_eq!(
            s.network,
            Network::M17,
            "the scene seeds M17 as the selection"
        );
        assert_eq!(
            s.settings.m17_callsign, "",
            "empty so the callsign field shows"
        );
        assert_eq!(
            s.m17_callsign_draft, "",
            "the draft seeds empty too, from the same source"
        );
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    #[test]
    fn m17_callsign_typing_uppercases_the_draft_but_does_not_persist() {
        // The M17 callsign field is the ONLY per-keystroke text field in
        // gui-rs that must NOT write through to disk on every keystroke
        // (mirrors the Mac's local, unpersisted `callsignDraft` — every other
        // field here, e.g. `NodeEntryChanged`/`FavoriteLabelChanged`/
        // `SetupNameChanged`, mutates transient state and defers the save
        // too, but none of them ever touch settings.* at all until a
        // dedicated commit action).
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::M17CallsignChanged("aj7hr".into()));
        assert_eq!(
            s.m17_callsign_draft, "AJ7HR",
            "the draft uppercases as you type"
        );
        assert_eq!(
            s.settings.m17_callsign, "",
            "typing alone must not touch the persisted value"
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.m17_callsign, "", "typing alone must not touch disk");
    }

    #[test]
    fn m17_callsign_submit_commits_and_persists_the_draft() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::M17CallsignChanged("aj7hr".into()));
        let _ = update(&mut s, Message::M17CallsignCommit);
        assert_eq!(
            s.settings.m17_callsign, "AJ7HR",
            "Enter commits the draft, uppercased"
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.m17_callsign, "AJ7HR", "and persists it immediately");
    }

    #[test]
    fn m17_callsign_commit_ignores_a_blank_draft() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::M17CallsignCommit);
        assert_eq!(
            s.settings.m17_callsign, "",
            "a blank (post-trim) draft is never committed"
        );
        let _ = update(&mut s, Message::M17CallsignChanged("   ".into()));
        let _ = update(&mut s, Message::M17CallsignCommit);
        assert_eq!(
            s.settings.m17_callsign, "",
            "whitespace-only draft is also never committed"
        );
    }

    #[test]
    fn m17_network_admits_slash_and_space_but_not_star() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::M17));
        let _ = update(
            &mut s,
            Message::NodeEntryChanged("m17.example.net:17000/A*".into()),
        );
        assert_eq!(
            s.node_entry, "m17.example.net:17000/A",
            "m17 filter admits '/' and ':' and drops '*'"
        );
    }

    #[test]
    fn m17_connect_commits_the_draft_before_dialing() {
        // Type a callsign then click Connect directly — NO M17CallsignCommit
        // (no Enter in the callsign field) — must still commit the draft and
        // dial in one gesture (mirrors the Mac's `connect()` calling
        // `commitCallsignDraft()` right before dialing).
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::M17));
        let _ = update(&mut s, Message::M17CallsignChanged("aj7hr".into()));
        let _ = update(
            &mut s,
            Message::NodeEntryChanged("m17.example.net/A".into()),
        );
        assert_eq!(
            s.settings.m17_callsign, "",
            "precondition: nothing committed yet"
        );
        let _ = update(&mut s, Message::Connect);
        assert_eq!(
            s.conn.sent_m17_connects(),
            vec![(
                "m17.example.net".to_string(),
                17000,
                'A',
                "AJ7HR".to_string()
            )]
        );
        assert_eq!(
            s.settings.m17_callsign, "AJ7HR",
            "Connect committed the draft"
        );
        let saved = s.store.load().expect("store readable");
        assert_eq!(saved.m17_callsign, "AJ7HR", "and persisted it");
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Connecting);
    }

    #[test]
    fn m17_connect_refuses_without_a_callsign() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::M17));
        // No callsign set — the parse succeeds but the dial must not reach
        // the seam (mirrors the Mac's `missingCallsign` guard).
        let _ = update(
            &mut s,
            Message::NodeEntryChanged("m17.example.net/A".into()),
        );
        let _ = update(&mut s, Message::Connect);
        assert!(s.conn.sent_m17_connects().is_empty());
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    #[test]
    fn m17_connect_refuses_an_unparseable_target_even_with_a_callsign() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::M17));
        let _ = update(&mut s, Message::M17CallsignChanged("aj7hr".into()));
        // No `/` or ` ` separator: not a valid M17 target — the button would
        // be disabled, but on_submit can still fire; refuse it like the
        // AllStar/Hamlink path does for unparseable text.
        s.node_entry = "m17.example.net".to_string();
        let _ = update(&mut s, Message::Connect);
        assert!(s.conn.sent_m17_connects().is_empty());
        tick(&mut s);
        assert_eq!(s.snap.status, Status::Disconnected);
    }

    // -- M17 TX-processing override (astar-5d8e) ------------------------------

    /// Dial the demo M17 target with `callsign` already committed as a draft
    /// — the shared setup used by every test below.
    fn dial_m17(s: &mut State, callsign: &str) {
        let _ = update(s, Message::NetworkPicked(Network::M17));
        let _ = update(s, Message::M17CallsignChanged(callsign.into()));
        let _ = update(s, Message::NodeEntryChanged("m17.example.net/A".into()));
        let _ = update(s, Message::Connect);
    }

    #[test]
    fn m17_connect_pushes_robs_field_tested_recipe_onto_the_conn() {
        // astar-m17defaults: 25% mic level, compression ON at 80% strength,
        // 80% TX trim; noise reduction stays off.
        let mut s = demo_state(DemoState::Idle);
        dial_m17(&mut s, "aj7hr");

        assert!(s.m17_audio_live);
        let pushed = s.conn.audio();
        assert!(!pushed.noise_reduction);
        assert!(pushed.compression);
        assert_eq!(pushed.compression_level, 0.80);
        assert_eq!(pushed.tx_trim, 0.80);
        assert_eq!(pushed.input_gain, 0.25);
    }

    #[test]
    fn m17_live_effective_audio_leaves_rx_compression_untouched() {
        // Regression guard (iax-a4e7/astar-outchain review): RX/output
        // compression is a shared listener-side setting with NO M17 override
        // — unlike the five TX-processing fields `effective_audio()` layers
        // from `m17_audio` while `m17_audio_live`, `rx_compression`/
        // `rx_compression_level` must ride straight through from
        // `settings.audio` unchanged. If either field is ever added to
        // `M17AudioOverrides` "for completeness," this must fail.
        let mut s = demo_state(DemoState::Idle);
        s.settings.audio.rx_compression = true;
        s.settings.audio.rx_compression_level = 0.42;

        dial_m17(&mut s, "aj7hr");

        assert!(s.m17_audio_live);
        let pushed = s.conn.audio();
        assert!(
            pushed.rx_compression,
            "shared rx_compression rides through unchanged"
        );
        assert_eq!(
            pushed.rx_compression_level, 0.42,
            "shared rx_compression_level rides through unchanged"
        );
    }

    #[test]
    fn m17_connect_pushes_whatever_override_was_edited_before_dialing() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::NetworkPicked(Network::M17));
        // Editing the TX-processing controls while M17 is merely SELECTED
        // (not yet live) lands in `settings.m17_audio`, not the shared
        // `settings.audio` — `m17_context()` is already true from the picker
        // alone. `InputGain` joins this group at astar-m17defaults.
        let _ = update(&mut s, Message::NoiseReduction(true));
        let _ = update(&mut s, Message::TxTrim(0.42));
        let _ = update(&mut s, Message::InputGain(0.6));
        dial_m17(&mut s, "aj7hr");

        assert!(s.settings.m17_audio.noise_reduction);
        assert_eq!(s.settings.m17_audio.tx_trim, 0.42);
        assert_eq!(s.settings.m17_audio.input_gain, 0.6);
        let pushed = s.conn.audio();
        assert!(pushed.noise_reduction);
        assert_eq!(pushed.tx_trim, 0.42);
        assert_eq!(pushed.input_gain, 0.6);
    }

    #[test]
    fn editing_m17_override_while_the_call_is_live_pushes_live() {
        let mut s = demo_state(DemoState::Idle);
        let standard_level_before = s.settings.audio.compression_level;
        dial_m17(&mut s, "aj7hr");
        assert!(s.m17_audio_live);

        let _ = update(&mut s, Message::CompressionLevel(0.33));

        assert_eq!(
            s.conn.audio().compression_level,
            0.33,
            "pushed live, mid-call"
        );
        assert_eq!(
            s.settings.m17_audio.compression_level, 0.33,
            "and persisted to the override"
        );
        assert_eq!(
            s.settings.audio.compression_level, standard_level_before,
            "the shared/AllStar settings are untouched"
        );
    }

    #[test]
    fn editing_m17_input_gain_while_the_call_is_live_pushes_live() {
        let mut s = demo_state(DemoState::Idle);
        let standard_gain_before = s.settings.audio.input_gain;
        dial_m17(&mut s, "aj7hr");
        assert!(s.m17_audio_live);

        let _ = update(&mut s, Message::InputGain(0.5));

        assert_eq!(s.conn.audio().input_gain, 0.5, "pushed live, mid-call");
        assert_eq!(
            s.settings.m17_audio.input_gain, 0.5,
            "and persisted to the override"
        );
        assert_eq!(
            s.settings.audio.input_gain, standard_gain_before,
            "the shared/AllStar settings are untouched"
        );
    }

    #[test]
    fn allstar_dial_after_m17_restores_the_standard_audio() {
        let mut s = demo_state(DemoState::Idle);
        // Distinguish the shared/standard chain from the M17 default.
        s.settings.audio.compression = true;
        s.settings.audio.compression_level = 0.5;
        s.settings.audio.tx_trim = 0.3;
        s.settings.audio.noise_reduction = true;
        s.settings.audio.input_gain = 0.66;
        dial_m17(&mut s, "aj7hr");
        assert!(s.m17_audio_live);

        let _ = update(&mut s, Message::NetworkPicked(Network::Allstar));
        let _ = update(&mut s, Message::NodeEntryChanged("55553".into()));
        let _ = update(&mut s, Message::Connect);

        assert!(
            !s.m17_audio_live,
            "an AllStar dial starting ends the M17 override"
        );
        let pushed = s.conn.audio();
        assert!(pushed.compression);
        assert_eq!(pushed.compression_level, 0.5);
        assert_eq!(pushed.tx_trim, 0.3);
        assert!(pushed.noise_reduction);
        assert_eq!(pushed.input_gain, 0.66, "shared mic gain restored");
    }

    #[test]
    fn disconnect_after_m17_restores_the_standard_audio() {
        let mut s = demo_state(DemoState::Idle);
        s.settings.audio.noise_reduction = true;
        s.settings.audio.input_gain = 0.77;
        dial_m17(&mut s, "aj7hr");
        assert!(s.m17_audio_live);

        let _ = update(&mut s, Message::Disconnect);

        assert!(!s.m17_audio_live);
        assert!(s.conn.audio().noise_reduction, "standard value restored");
        assert_eq!(s.conn.audio().input_gain, 0.77, "shared mic gain restored");
    }

    #[test]
    fn remote_hangup_during_m17_restores_the_standard_audio_via_the_poll_path() {
        let mut s = demo_state(DemoState::Idle);
        s.settings.audio.tx_trim = 0.6;
        s.settings.audio.input_gain = 0.44;
        dial_m17(&mut s, "aj7hr");
        assert!(s.m17_audio_live);

        // The far end hangs up — the conn reports the drop on its own, with
        // no `Message::Disconnect` ever firing (mirrors the Mac's poll-path
        // edge for a remote hangup: `Message::Tick` is gui-rs's poll path).
        s.conn.disconnect();
        tick(&mut s);

        assert!(!s.m17_audio_live, "the poll-path edge clears the flag");
        assert_eq!(s.conn.audio().tx_trim, 0.6, "standard value restored");
        assert_eq!(s.conn.audio().input_gain, 0.44, "shared mic gain restored");

        // A further tick at the same terminal status must not re-fire it —
        // nothing left to restore, so the conn shouldn't see another push.
        let before = s.conn.audio();
        tick(&mut s);
        assert_eq!(s.conn.audio(), before, "edge-triggered — no repeat push");
    }

    #[test]
    fn dialing_a_hamlink_favorite_auto_switches_the_network() {
        // The NetworkPicker shot scene is the only seam that flips
        // `hamlink_available` on the demo conn before it's boxed — reuse it
        // rather than adding a new one. It boots with `Network::Allstar`
        // selected (the default), so a dialed Hamlink entry proves the
        // DialFavorite handler switches network from the entry, not just
        // leaving whatever was already selected.
        let (mut s, _) = State::boot(Mode::Shot {
            scene: ShotScene::NetworkPicker,
            file: std::path::PathBuf::from("unused.png"),
        });
        assert_eq!(s.network, Network::Allstar, "starts on the default network");

        s.settings.directory.push(crate::settings::NodeEntry {
            id: "refl9".into(),
            label: "Reflector 9".into(),
            node: "refl9".into(),
            favorite: true,
            last_used: None,
            note: None,
            network: Network::Hamlink,
        });
        let _ = update(&mut s, Message::DialFavorite("refl9".into()));
        assert_eq!(
            s.network,
            Network::Hamlink,
            "auto-switches to the entry's network"
        );
    }

    #[test]
    fn ptt_is_ignored_unless_connected() {
        let mut s = demo_state(DemoState::Idle);
        let _ = update(&mut s, Message::Ptt(true));
        tick(&mut s);
        assert!(!s.snap.transmitting, "PTT must not key while disconnected");

        let mut s = demo_state(DemoState::Connected);
        tick(&mut s);
        let _ = update(&mut s, Message::Ptt(true));
        tick(&mut s);
        assert!(s.snap.transmitting, "PTT keys once connected");
    }

    // --- Tray + popover shell (astar-22cf) ---

    /// A demo state wired up as if the tray attached: window id known,
    /// tray live. (Tests never touch the real tray plumbing — `tray::init`
    /// is never called, so the module-level statics stay inert.)
    fn tray_state(scene: DemoState) -> State {
        let mut s = demo_state(scene);
        s.main_window = Some(window::Id::unique());
        s.tray_active = true;
        s
    }

    #[test]
    fn tray_toggle_hides_then_shows_the_window() {
        let mut s = tray_state(DemoState::Idle);
        assert!(s.window_visible, "boots visible");
        let _ = update(&mut s, Message::Tray(tray::Event::Toggle { anchor: None }));
        assert!(!s.window_visible, "first click hides");
        let _ = update(&mut s, Message::Tray(tray::Event::Toggle { anchor: None }));
        assert!(s.window_visible, "second click shows again");
        // The menu's Show/Hide item drives the same toggle.
        let _ = update(&mut s, Message::Tray(tray::Event::ShowHide));
        assert!(!s.window_visible);
    }

    #[test]
    fn tray_toggle_waits_for_the_window_id() {
        // A click during the sub-second id-probe window must not wedge the
        // visibility bookkeeping.
        let mut s = demo_state(DemoState::Idle);
        s.tray_active = true;
        assert!(s.main_window.is_none());
        let _ = update(&mut s, Message::Tray(tray::Event::Toggle { anchor: None }));
        assert!(s.window_visible, "no window id yet — nothing to hide");
    }

    #[test]
    fn close_hides_when_the_tray_is_live() {
        let mut s = tray_state(DemoState::Connected);
        let _ = update(&mut s, Message::CloseRequested);
        assert!(!s.window_visible, "close is the Mac's hide, not quit");
        // Without a tray (GNOME fallback), close quits instead — the state
        // side of that is that visibility does NOT flip (the app exits).
        let mut s = demo_state(DemoState::Connected);
        assert!(!s.tray_active);
        let _ = update(&mut s, Message::CloseRequested);
        assert!(s.window_visible);
    }

    #[test]
    fn first_tray_show_anchors_the_popover_once() {
        let mut s = tray_state(DemoState::Idle);
        let anchor = tray::Anchor {
            x: 1800.0,
            y: 4.0,
            w: 40.0,
            h: 44.0,
        };
        // Hide, then show with an anchor: placement kicks off exactly once.
        let _ = update(&mut s, Message::Tray(tray::Event::Toggle { anchor: None }));
        let _ = update(
            &mut s,
            Message::Tray(tray::Event::Toggle {
                anchor: Some(anchor),
            }),
        );
        assert!(s.popover_placed);
        assert_eq!(s.tray_anchor, Some(anchor));
        // Later shows leave the window where the user put it (Mac parity).
        let moved = tray::Anchor {
            x: 10.0,
            y: 900.0,
            w: 40.0,
            h: 44.0,
        };
        let _ = update(&mut s, Message::Tray(tray::Event::Toggle { anchor: None }));
        let _ = update(
            &mut s,
            Message::Tray(tray::Event::Toggle {
                anchor: Some(moved),
            }),
        );
        assert_eq!(s.tray_anchor, Some(anchor), "anchor is captured only once");
    }

    #[test]
    fn tick_tracks_the_tray_asterisk_state() {
        // The 20 Hz tick mirrors the call state onto the tray asterisk exactly
        // like the Mac's Combine pipeline onto StatusIconState.
        let mut s = tray_state(DemoState::Tx);
        assert_eq!(s.tray_icon, StatusIcon::Idle);
        tick(&mut s);
        assert_eq!(s.tray_icon, StatusIcon::Transmitting);

        let mut s = tray_state(DemoState::Rx);
        tick(&mut s);
        assert_eq!(s.tray_icon, StatusIcon::Receiving);

        let mut s = tray_state(DemoState::Connected);
        tick(&mut s);
        assert_eq!(s.tray_icon, StatusIcon::Connected);

        let mut s = tray_state(DemoState::Idle);
        tick(&mut s);
        assert_eq!(s.tray_icon, StatusIcon::Idle);
    }

    #[test]
    fn tray_stays_untinted_when_inactive() {
        // Shots / fallback mode: the tick must not touch the tray bookkeeping.
        let mut s = demo_state(DemoState::Tx);
        assert!(!s.tray_active);
        tick(&mut s);
        assert_eq!(s.tray_icon, StatusIcon::Idle);
    }
}
