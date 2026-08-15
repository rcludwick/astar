// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The connection seam (astar-22ea).
//!
//! [`Conn`] is the one interface the UI talks to. It hides whether we are
//! driving a live IAX2 [`Station`] or a scripted demo. Two impls:
//!
//! * [`DemoConn`] — produces deterministic [`Snapshot`]s for a chosen
//!   [`DemoState`], with TX/RX VU levels animated by a tick counter (no RNG, no
//!   network, no audio). This is what the screenshot loop and `--demo` exercise.
//! * [`RealConn`] — wraps [`astar_station::Station`] and best-effort maps
//!   its `ConsoleState` into a [`Snapshot`]. Live wiring (PTT, DTMF, connect) is
//!   delegated to the `Station`; pieces not yet wired are marked `TODO`.

use crate::dial_target::DialTarget;
use crate::settings::AudioSettings;
use crate::snapshot::{db_to_unit, Snapshot, Status};

/// The seam the UI renders against. Object-safe so `Box<dyn Conn>` works.
pub trait Conn {
    /// Dial a registrar-resolved node by id (the normal AllStar path).
    fn connect_node(&mut self, node: &str);
    /// Dial via the Web-Transceiver convenience path (mint token, then dial).
    /// Part of the seam API; not yet surfaced by a dedicated UI control.
    #[allow(dead_code)]
    fn connect_wt(&mut self, node: &str);
    /// Dial an explicit `host[:port]` address directly (the smart dial field's
    /// address path, astar-3939 / the Mac's astar-427f): WT token auth
    /// unchanged, only the dial target differs — for a node that isn't
    /// reachable at its published address (e.g. your own node on localhost).
    /// The address string doubles as the display/recents label.
    fn connect_address(&mut self, addr: &str);
    /// Tear down any active call.
    fn disconnect(&mut self);
    /// Key (`true`) or unkey (`false`) the local transmitter.
    fn set_ptt(&mut self, on: bool);
    /// Send a single in-band DTMF digit (`0-9 * # A-D` — the 16 keys the core
    /// accepts). Kept for seam parity with the engine; the dialpad now sends
    /// composed commands via [`Conn::send_dtmf_string`] (astar-7d21).
    #[allow(dead_code)]
    fn send_dtmf(&mut self, digit: char);
    /// The DTMF digits pushed through [`Conn::send_dtmf`], oldest first — a
    /// test readback (like [`Conn::audio`]) so the dialpad tests can prove a
    /// tap became a tone. The demo records; the live impl doesn't (the
    /// station owns the wire).
    #[allow(dead_code)]
    fn sent_dtmf(&self) -> Vec<char> {
        Vec::new()
    }
    /// Send a composed multi-digit DTMF command as one engine-timed sequence
    /// (astar-7d21 / iax-4b7a) — the dialpad's Send. Errors surface on the
    /// status card like [`Conn::send_dtmf`]; progress arrives via
    /// [`Snapshot::dtmf_played`] / [`Snapshot::dtmf_total`].
    fn send_dtmf_string(&mut self, digits: &str);
    /// Drop the un-played remainder of the playing command — the Stop button.
    /// Safe when nothing is playing.
    fn cancel_dtmf(&mut self);
    /// The commands pushed through [`Conn::send_dtmf_string`], oldest first —
    /// a test readback like [`Conn::sent_dtmf`]. The demo records; the live
    /// impl doesn't.
    #[allow(dead_code)]
    fn sent_dtmf_strings(&self) -> Vec<String> {
        Vec::new()
    }
    /// Enumerate the audio devices as `(inputs, outputs)` — capture and
    /// playback device names, picker-ready. Takes `&mut self` so the live impl
    /// can surface an enumeration failure on the status card (it then returns
    /// empty lists; the pickers still offer "System default").
    fn list_devices(&mut self) -> (Vec<String>, Vec<String>);
    /// Select the capture/playback devices; `None` = system default. Applied
    /// to the next connect (the station re-resolves devices per call, so a
    /// persisted choice pushed here survives reconnects). Persistence is the
    /// caller's job — the conn only routes audio.
    fn set_devices(&mut self, input: Option<String>, output: Option<String>);
    /// The currently selected `(input, output)` devices; `None` = system
    /// default. Part of the seam API (exercised by tests) — the pickers
    /// themselves render from persisted settings.
    #[allow(dead_code)]
    fn selected_devices(&self) -> (Option<String>, Option<String>);
    /// Push the quick-config audio preferences (gains, compression, noise
    /// reduction, TX trim, VOX, duplex) as one standing bundle. The live impl
    /// forwards the engine-backed knobs to the station, whose console session
    /// re-pushes standing prefs onto every (re)connect — so pushing once here
    /// survives reconnects. Device names ride [`Conn::set_devices`], not this.
    fn set_audio(&mut self, audio: &AudioSettings);
    /// The audio preferences as last pushed (engine defaults before any push).
    /// Read back by tests to prove the seam carried every knob.
    #[allow(dead_code)]
    fn audio(&self) -> AudioSettings;
    /// Produce the current render-ready snapshot.
    fn snapshot(&self) -> Snapshot;
    /// Advance any internal animation/clock by one tick (~50 ms). Demo impls use
    /// this to animate VU meters; the live impl ignores it (the `Station` has
    /// its own clock).
    fn tick(&mut self);
    /// Whether the engine can currently drive the Hamlink (SvxReflector,
    /// iax-b3d7) network — the gate behind [`crate::network::Network::available`].
    /// Defaults off: no live station reports reflector capability yet, so
    /// only the demo knob ([`DemoConn::set_hamlink_available`]) flips this
    /// for UI development ahead of the engine capability landing.
    #[allow(dead_code)]
    fn hamlink_available(&self) -> bool {
        false
    }
    /// Whether the engine can currently drive the M17 network (iax-f2b8 Task
    /// 4: `Station::m17_available` — codec2 resolved, M17 session support
    /// compiled in) — the gate behind [`crate::network::Network::available`].
    /// Defaults off; `RealConn` delegates to the station, `DemoConn` exposes
    /// [`DemoConn::set_m17_available`] for shots/tests.
    #[allow(dead_code)]
    fn m17_available(&self) -> bool {
        false
    }
    /// Connect to an M17 reflector (iax-f2b8 Task 4/8): `module` is a single
    /// letter, `callsign` is transmitted verbatim in every frame — both are
    /// validated upstream (dial parse + non-empty callsign) before this is
    /// called. Fire-and-forget like the other connect paths: a failure lands
    /// on the status card via the seam's `note()`, not a `Result` here.
    fn connect_m17(&mut self, host: &str, port: u16, module: char, callsign: &str);
    /// Tear down the live M17 session, if any. Fire-and-forget, like
    /// [`Conn::disconnect`].
    fn m17_disconnect(&mut self);
    /// The `(host, port, module, callsign)` tuples pushed through
    /// [`Conn::connect_m17`], oldest first — a test readback (like
    /// [`Conn::sent_dtmf`]) so the dial-dispatch tests can prove a Connect
    /// reached the seam. The demo records; the live impl doesn't (the station
    /// owns the wire).
    #[allow(dead_code)]
    fn sent_m17_connects(&self) -> Vec<(String, u16, char, String)> {
        Vec::new()
    }
}

/// The scripted scenes the demo can render. Each maps to a clearly
/// distinct on-screen state so the screenshots are visually unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoState {
    /// Disconnected, idle.
    Idle,
    /// Mid-dial.
    Connecting,
    /// Connected, no audio flowing.
    Connected,
    /// Connected, remote station talking (RX lit, RX bar moving).
    Rx,
    /// Connected, we are transmitting (TX lit, TX bar moving).
    Tx,
    /// A connect failed: disconnected with an error line on the status card.
    Error,
}

impl DemoState {
    /// Parse a `--demo=STATE` / `--shot STATE` token (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "idle" => Some(DemoState::Idle),
            "connecting" => Some(DemoState::Connecting),
            "connected" => Some(DemoState::Connected),
            "rx" => Some(DemoState::Rx),
            "tx" => Some(DemoState::Tx),
            "error" => Some(DemoState::Error),
            _ => None,
        }
    }
}

/// A scripted, deterministic connection. Renders the chosen [`DemoState`] and
/// animates VU levels by an internal tick counter — same state always yields a
/// reproducible sequence (important: screenshots must be stable).
pub struct DemoConn {
    state: DemoState,
    ticks: u64,
    /// The node dialed interactively (via `--demo` clicks); scripted scenes
    /// fall back to a fixed demo node so screenshots stay stable.
    dialed: Option<String>,
    /// Tick at which an in-flight demo dial "answers".
    answer_at: Option<u64>,
    /// The demo's selected `(input, output)` devices; `None` = system default.
    devices: (Option<String>, Option<String>),
    /// The demo's audio preferences, held verbatim so `--shot`/`--demo`
    /// render every quick-config control from real pushed state.
    audio: AudioSettings,
    /// Every digit pushed through [`Conn::send_dtmf`], oldest first — the
    /// dialpad tests read this back to prove a tap became a tone.
    dtmf: Vec<char>,
    /// Every command pushed through [`Conn::send_dtmf_string`], oldest first.
    dtmf_strings: Vec<String>,
    /// The fake in-flight sequence `(digits, played, ticks_until_next)` —
    /// tick-driven like `answer_at` so shots and tests stay deterministic
    /// (astar-7d21). Mirrors the engine: retires one slot after the last
    /// digit, reporting (0, 0) once done.
    dtmf_seq: Option<(Vec<char>, u32, u32)>,
    /// The demo/shot knob for [`Conn::hamlink_available`] (astar-9b3e): off
    /// by default (matches `RealConn` until iax-b3d7 lands engine capability),
    /// flipped via [`Self::set_hamlink_available`] to exercise the network
    /// picker's two-network state ahead of the real capability.
    hamlink_available: bool,
    /// The demo/shot knob for [`Conn::m17_available`] (M17 Task 10): off by
    /// default, flipped via [`Self::set_m17_available`] to exercise the M17
    /// dial scene ahead of a live station reporting the capability.
    m17_available: bool,
    /// Every `(host, port, module, callsign)` pushed through
    /// [`Conn::connect_m17`], oldest first — the M17 dial-dispatch tests read
    /// this back to prove a Connect actually reached the seam.
    m17_connects: Vec<(String, u16, char, String)>,
}

impl DemoConn {
    /// How many ticks (~50 ms each) a demo dial rings before it answers.
    const ANSWER_TICKS: u64 = 20;
    /// Ticks between fake sequenced digits — ~350 ms at the 50 ms poll,
    /// matching the engine's 250 ms tone + 100 ms gap cadence.
    const TICKS_PER_DIGIT: u32 = 7;

    /// The fixed demo capture devices — stable names so `--shot`/`--demo`
    /// render the pickers identically on every machine, no audio hardware
    /// needed.
    pub const INPUTS: [&'static str; 2] = ["Demo Microphone", "USB Audio CODEC"];
    /// The fixed demo playback devices.
    pub const OUTPUTS: [&'static str; 2] = ["Demo Speakers", "USB Audio CODEC"];

    /// The fixed demo node directory — stable entries so `--shot`/`--demo`
    /// render the favorites section identically everywhere: two favorites
    /// (one never dialed, one that's also a recent) plus a bare recent.
    #[must_use]
    pub fn demo_directory() -> Vec<crate::settings::NodeEntry> {
        let entry = |label: &str, node: &str, favorite: bool, last_used: Option<i64>| {
            crate::settings::NodeEntry {
                id: node.to_string(),
                label: label.to_string(),
                node: node.to_string(),
                favorite,
                last_used,
                note: None,
                network: crate::network::Network::default(),
            }
        };
        vec![
            entry("AJ7HR", "77777", true, None),
            entry("W6ABC Repeater", "546054", true, Some(1_750_000_000)),
            entry("2000", "2000", false, Some(1_749_000_000)),
        ]
    }

    /// The fixed demo Setups (astar-80ae) — stable entries so `--shot`/`--demo`
    /// render the switcher identically everywhere: a UCI150 rig with audio
    /// overrides (so shots show controls at non-default values) and a headset
    /// rig with none (so the None-= keep-global merge is on display too).
    #[must_use]
    pub fn demo_setups() -> Vec<crate::settings::Setup> {
        vec![
            crate::settings::Setup {
                id: "demo-uci150".to_string(),
                name: "UCI150 desk".to_string(),
                hardware_profile_id: "uci150".to_string(),
                input_device: Some("USB Audio CODEC".to_string()),
                output_device: Some("USB Audio CODEC".to_string()),
                input_gain: Some(0.75),
                output_gain: Some(1.10),
                compression: Some(true),
                compression_level: Some(0.60),
                tx_trim: Some(0.90),
                noise_reduction: Some(true),
                ..crate::settings::Setup::default()
            },
            crate::settings::Setup {
                id: "demo-headset".to_string(),
                name: "Jabra mobile".to_string(),
                hardware_profile_id: "headset".to_string(),
                input_device: Some("Demo Microphone".to_string()),
                output_device: Some("Demo Speakers".to_string()),
                ..crate::settings::Setup::default()
            },
        ]
    }

    /// Create a demo connection forced to `state`.
    #[must_use]
    pub fn new(state: DemoState) -> Self {
        // Seed the tick counter so a freshly-captured screenshot lands on a
        // visibly non-zero meter level (a fresh `--shot tx` shouldn't catch the
        // bar at its trough).
        Self {
            state,
            ticks: 7,
            dialed: None,
            answer_at: None,
            devices: (None, None),
            audio: AudioSettings::default(),
            dtmf: Vec::new(),
            dtmf_strings: Vec::new(),
            dtmf_seq: None,
            hamlink_available: false,
            m17_available: false,
            m17_connects: Vec::new(),
        }
    }

    /// Seed a mid-flight sequence directly: `played` of the command's digits
    /// already out, the rest pending. Deterministic — no ticking required to
    /// render it. (The `dialpad-sending` shot boots via the trait's
    /// `send_dtmf_string` + the pre-capture ticks instead; this is the
    /// test-facing shortcut.)
    #[allow(dead_code)]
    pub fn seed_dtmf_progress(&mut self, command: &str, played: u32) {
        let digits: Vec<char> = command.chars().collect();
        let played = played.min(u32::try_from(digits.len()).unwrap_or(u32::MAX));
        self.dtmf_seq = Some((digits, played, Self::TICKS_PER_DIGIT));
    }

    /// Start an interactive demo dial: ring for [`Self::ANSWER_TICKS`], then
    /// answer. Shared by both connect paths — the demo doesn't distinguish
    /// WT from generic dialing.
    fn dial(&mut self, node: &str) {
        self.dialed = Some(node.to_string());
        self.state = DemoState::Connecting;
        self.answer_at = Some(self.ticks + Self::ANSWER_TICKS);
    }

    /// Override the forced demo state. Kept for a future demo-state picker UI;
    /// the current `--demo` flow sets the scene via PTT/connect actions.
    #[allow(dead_code)]
    pub fn set_state(&mut self, state: DemoState) {
        self.state = state;
    }

    /// The current forced state.
    #[allow(dead_code)]
    #[must_use]
    pub fn state(&self) -> DemoState {
        self.state
    }

    /// Flip the demo's Hamlink-availability knob (astar-9b3e) — lets
    /// `--demo`/`--shot` and tests exercise the network picker's two-network
    /// state ahead of the real engine capability (iax-b3d7).
    #[allow(dead_code)]
    pub fn set_hamlink_available(&mut self, available: bool) {
        self.hamlink_available = available;
    }

    /// Flip the demo's M17-availability knob (M17 Task 10) — lets
    /// `--demo`/`--shot` and tests exercise the M17 dial scene ahead of the
    /// real engine capability (iax-f2b8 Task 4).
    #[allow(dead_code)]
    pub fn set_m17_available(&mut self, available: bool) {
        self.m17_available = available;
    }

    /// A deterministic, smooth 0..1 oscillation driven by the tick counter,
    /// `phase` offsets TX vs RX so the two bars don't move in lockstep. Uses a
    /// sine so the motion looks like a real VU meter, not a sawtooth.
    fn wave(&self, phase: f32) -> f32 {
        // ~0.9 s period at 20 Hz; centered at 0.6 with ±0.35 swing.
        let t = self.ticks as f32 * 0.35 + phase;
        0.6 + 0.35 * t.sin()
    }
}

impl Conn for DemoConn {
    fn connect_node(&mut self, node: &str) {
        self.dial(node);
    }
    fn connect_wt(&mut self, node: &str) {
        self.dial(node);
    }
    fn connect_address(&mut self, addr: &str) {
        // The demo accepts both dial shapes so `--demo`/`--shot` render an
        // address dial exactly like a node dial.
        self.dial(addr);
    }
    fn disconnect(&mut self) {
        self.state = DemoState::Idle;
        self.dialed = None;
        self.answer_at = None;
    }
    fn set_ptt(&mut self, on: bool) {
        // Reflect PTT in the scene so interactive `--demo` feels live — but
        // only within a connected scene; PTT can't conjure a call from idle.
        if matches!(
            self.state,
            DemoState::Connected | DemoState::Rx | DemoState::Tx
        ) {
            self.state = if on {
                DemoState::Tx
            } else {
                DemoState::Connected
            };
        }
    }
    fn send_dtmf(&mut self, digit: char) {
        self.dtmf.push(digit);
    }

    fn sent_dtmf(&self) -> Vec<char> {
        self.dtmf.clone()
    }

    fn send_dtmf_string(&mut self, digits: &str) {
        self.dtmf_strings.push(digits.to_string());
        // Mirror the engine: the first digit goes out immediately, the rest
        // advance on ticks.
        let seq: Vec<char> = digits.chars().collect();
        if !seq.is_empty() {
            self.dtmf_seq = Some((seq, 1, Self::TICKS_PER_DIGIT));
        }
    }

    fn cancel_dtmf(&mut self) {
        self.dtmf_seq = None;
    }

    fn sent_dtmf_strings(&self) -> Vec<String> {
        self.dtmf_strings.clone()
    }

    fn list_devices(&mut self) -> (Vec<String>, Vec<String>) {
        (
            Self::INPUTS.iter().map(ToString::to_string).collect(),
            Self::OUTPUTS.iter().map(ToString::to_string).collect(),
        )
    }

    fn set_devices(&mut self, input: Option<String>, output: Option<String>) {
        self.devices = (input, output);
    }

    fn selected_devices(&self) -> (Option<String>, Option<String>) {
        self.devices.clone()
    }

    fn set_audio(&mut self, audio: &AudioSettings) {
        self.audio = audio.clone();
    }

    fn audio(&self) -> AudioSettings {
        self.audio.clone()
    }

    fn snapshot(&self) -> Snapshot {
        let node = || self.dialed.clone().or_else(|| Some("546054".to_string()));
        let name = || Some("W6ABC Repeater".to_string());
        // The demo call negotiated slin16 (astar-efba): the connected scenes
        // render the wideband badge for shots, and it holds through PTT/RX so
        // it never flickers while keying in an interactive demo.
        let wideband = Some(astar_station::VoiceFormat::Slin16);
        let mut snap = match self.state {
            DemoState::Idle => Snapshot::default(),
            DemoState::Connecting => Snapshot {
                status: Status::Connecting,
                dialed_node: node(),
                node_name: None,
                ..Snapshot::default()
            },
            DemoState::Connected => Snapshot {
                status: Status::Connected,
                dialed_node: node(),
                node_name: name(),
                negotiated_format: wideband,
                ..Snapshot::default()
            },
            DemoState::Rx => Snapshot {
                status: Status::Connected,
                dialed_node: node(),
                node_name: name(),
                receiving: true,
                rx_level: self.wave(0.0),
                negotiated_format: wideband,
                ..Snapshot::default()
            },
            DemoState::Tx => Snapshot {
                status: Status::Connected,
                dialed_node: node(),
                node_name: name(),
                transmitting: true,
                tx_level: self.wave(1.5),
                negotiated_format: wideband,
                ..Snapshot::default()
            },
            DemoState::Error => Snapshot {
                status: Status::Disconnected,
                dialed_node: node(),
                error: Some("Couldn't connect: token mint failed (check your account)".to_string()),
                ..Snapshot::default()
            },
        };
        // The fake sequence's progress rides every connected scene (astar-7d21).
        if let Some((digits, played, _)) = &self.dtmf_seq {
            snap.dtmf_played = *played;
            snap.dtmf_total = u32::try_from(digits.len()).unwrap_or(u32::MAX);
        }
        snap
    }

    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        // Answer an in-flight demo dial once it has rung long enough.
        if let Some(at) = self.answer_at {
            if self.ticks >= at && self.state == DemoState::Connecting {
                self.state = DemoState::Connected;
                self.answer_at = None;
            }
        }
        // Advance the fake DTMF sequence (astar-7d21): one digit per
        // TICKS_PER_DIGIT, then one more slot before the queue retires.
        if let Some((digits, played, ticks_left)) = &mut self.dtmf_seq {
            if *ticks_left > 1 {
                *ticks_left -= 1;
            } else if (*played as usize) < digits.len() {
                *played += 1;
                *ticks_left = Self::TICKS_PER_DIGIT;
            } else {
                self.dtmf_seq = None; // last digit's slot elapsed — done
            }
        }
    }

    fn hamlink_available(&self) -> bool {
        self.hamlink_available
    }

    fn m17_available(&self) -> bool {
        self.m17_available
    }

    fn connect_m17(&mut self, host: &str, port: u16, module: char, callsign: &str) {
        self.m17_connects
            .push((host.to_string(), port, module, callsign.to_string()));
        // The demo answers an M17 dial exactly like any other (astar-9b3e's
        // dial() scene, reused) — the display label mirrors the raw grammar.
        self.dial(&format!("{host}:{port}/{module}"));
    }

    fn m17_disconnect(&mut self) {
        self.disconnect();
    }

    fn sent_m17_connects(&self) -> Vec<(String, u16, char, String)> {
        self.m17_connects.clone()
    }
}

/// Map the core's [`astar_station::CallStatus`] to the UI status plus an
/// error line for the status card. Shared by [`RealConn::snapshot`]; pure so
/// it's testable without a station.
fn map_call_status(cs: &astar_station::CallStatus) -> (Status, Option<String>) {
    use astar_station::CallStatus;
    match cs {
        CallStatus::Idle => (Status::Disconnected, None),
        CallStatus::Dialing => (Status::Connecting, None),
        CallStatus::Answered => (Status::Connected, None),
        // A cause string from the peer (reject, busy, timeout…) goes on the
        // status card; a clean no-cause hangup isn't an error.
        CallStatus::Hangup { reason } if reason.is_empty() => (Status::Disconnected, None),
        CallStatus::Hangup { reason } => {
            (Status::Disconnected, Some(format!("Call ended: {reason}")))
        }
        CallStatus::Failed { reason } if reason.is_empty() => {
            (Status::Disconnected, Some("Connect failed".to_string()))
        }
        CallStatus::Failed { reason } => (
            Status::Disconnected,
            Some(format!("Connect failed: {reason}")),
        ),
    }
}

/// The connect-failure line for the status card — the Mac's
/// `connectFailureMessage` resolve mapping (astar-427f). A resolve failure
/// for a node dial means no live registration (wording per Rob: short and
/// plain); for an address-shaped dial the registrar was never asked, so "not
/// registered" would mislead — name the address instead. Every other failure
/// keeps the generic seam wording. Pure so it's testable without a station.
fn connect_failure_message(error: &astar_station::StationError, dialed: &str) -> String {
    match error {
        astar_station::StationError::Resolve(_) => {
            if let Some(DialTarget::Address(_)) = DialTarget::parse(dialed) {
                format!("Couldn’t reach {dialed}.")
            } else {
                format!("Node {dialed} is not registered in Allstar.")
            }
        }
        e => format!("Connect failed: {e}"),
    }
}

/// Map held dBFS meter levels to the two normalized VU bars. TX shows the
/// floor unless keyed — the mic stays open for metering, so ungated it would
/// never rest at zero (mirrors the Mac: `session.ptt ? txDBHeld : -60`). RX
/// is never gated by our PTT. Pure so it's testable without a station.
fn meter_levels(ptt: bool, tx_held_db: f32, rx_held_db: f32) -> (f32, f32) {
    let tx = if ptt { db_to_unit(tx_held_db) } else { 0.0 };
    (tx, db_to_unit(rx_held_db))
}

/// WT portal credentials from the environment — the STOPGAP credential source
/// until the WT-credentials nugget (astar-1efc) lands keyring + UI storage.
fn env_portal() -> Option<astar_station::PortalCredentials> {
    Some(astar_station::PortalCredentials {
        user: std::env::var("ASTAR_WT_USER").ok()?,
        password: std::env::var("ASTAR_WT_PASSWORD").ok()?,
        node: std::env::var("ASTAR_WT_NODE").ok()?,
    })
}

/// The `StationConfig` a live station is built from: portal credentials plus
/// the codec policy, which is always `PreferSlin16` — wideband is always on
/// (astar-e542) and the node decides the narrowband fallback in IAX2
/// negotiation (nodes without allow=slin16 answer µ-law). Pure, so tests can
/// prove the wiring without constructing an engine (mirrors the Mac's
/// `CallSession.stationConfig`, astar-eb6c/astar-efba).
fn station_config(
    portal: Option<astar_station::PortalCredentials>,
) -> astar_station::StationConfig {
    astar_station::StationConfig {
        portal,
        codec_policy: astar_station::CodecPolicy::PreferSlin16,
        ..astar_station::StationConfig::default()
    }
}

/// Generic node auth (calling node + secret + station name) from the
/// environment — the advanced dial path, same stopgap status as [`env_portal`].
fn env_node_auth() -> Option<(String, String, String)> {
    let calling = std::env::var("ASTAR_CALLING_NODE").ok()?;
    let secret = std::env::var("ASTAR_NODE_SECRET").ok()?;
    let name = std::env::var("ASTAR_STATION_NAME").unwrap_or_else(|_| "astar".to_string());
    Some((calling, secret, name))
}

/// A live connection backed by [`astar_station::Station`].
///
/// Credentials come from the environment for now (`ASTAR_WT_USER` /
/// `ASTAR_WT_PASSWORD` / `ASTAR_WT_NODE` for the primary WT path;
/// `ASTAR_CALLING_NODE` / `ASTAR_NODE_SECRET` / `ASTAR_STATION_NAME` for the
/// generic path; `ASTAR_WT_ADDR` dials an explicit `host:port` like the Mac
/// app's host override). The credentials nuggets replace this with stored
/// account settings.
pub struct RealConn {
    station: astar_station::Station,
    /// The node we last asked to dial, so the UI can show it before/while the
    /// `Station` reports a call (its snapshot only carries a node in `calls`).
    dialed: Option<String>,
    /// Whether the station was configured with WT portal credentials.
    has_portal: bool,
    /// The last action failure, held until the next action clears it.
    last_error: Option<String>,
    /// ~250 ms sliding-max smoothers for the VU meters (astar-3f3a), advanced
    /// from [`Conn::tick`]; the held dB values below are what `snapshot` maps.
    tx_peak: crate::peak::PeakHold,
    rx_peak: crate::peak::PeakHold,
    tx_held_db: f32,
    rx_held_db: f32,
    /// The audio prefs as last pushed (readback for [`Conn::audio`]). The
    /// station holds its own copy of the engine-backed knobs and re-pushes
    /// them per connect; this mirror also carries the not-yet-engine-backed
    /// VOX/duplex values.
    audio: AudioSettings,
}

impl RealConn {
    /// Build a live connection. No audio or network starts until
    /// [`Conn::connect_node`]/[`Conn::connect_wt`]. The codec policy is always
    /// `PreferSlin16` (astar-e542) and pins at engine creation; the persisted
    /// devices/gains ride in through the later [`Conn::set_audio`] push.
    #[must_use]
    pub fn new() -> Self {
        let portal = env_portal();
        let has_portal = portal.is_some();
        let config = station_config(portal);
        Self {
            station: astar_station::Station::new(config),
            dialed: None,
            has_portal,
            last_error: None,
            tx_peak: crate::peak::PeakHold::new(crate::peak::PeakHold::METER_WINDOW),
            rx_peak: crate::peak::PeakHold::new(crate::peak::PeakHold::METER_WINDOW),
            tx_held_db: -60.0,
            rx_held_db: -60.0,
            audio: AudioSettings::default(),
        }
    }

    /// True when no credential source is configured at all — the status card
    /// shows the add-your-account hint.
    fn needs_account(&self) -> bool {
        !self.has_portal && env_node_auth().is_none()
    }

    /// Record a station error for the status card.
    fn note<T>(&mut self, what: &str, res: Result<T, astar_station::StationError>) {
        if let Err(e) = res {
            self.last_error = Some(format!("{what}: {e}"));
        }
    }

    /// Record a connect failure for the status card, with the Mac's
    /// two-variant resolve copy (see [`connect_failure_message`]).
    fn note_connect<T>(&mut self, dialed: &str, res: Result<T, astar_station::StationError>) {
        if let Err(e) = res {
            self.last_error = Some(connect_failure_message(&e, dialed));
        }
    }
}

impl Default for RealConn {
    fn default() -> Self {
        Self::new()
    }
}

impl Conn for RealConn {
    /// The generic/advanced dial path: registrar-resolved, node-secret auth.
    fn connect_node(&mut self, node: &str) {
        self.dialed = Some(node.to_string());
        self.last_error = None;
        match env_node_auth() {
            Some((calling, secret, name)) => {
                let res = self.station.connect(node, &calling, &secret, &name);
                self.note_connect(node, res);
            }
            None => {
                self.last_error =
                    Some("No node auth: set ASTAR_CALLING_NODE + ASTAR_NODE_SECRET".to_string());
            }
        }
    }

    /// The primary dial path: Web-Transceiver token auth, falling back to the
    /// generic path when only node auth is configured.
    fn connect_wt(&mut self, node: &str) {
        if !self.has_portal {
            // No portal account: try the generic path (it reports the
            // no-credentials hint itself if that's not configured either).
            return self.connect_node(node);
        }
        self.dialed = Some(node.to_string());
        self.last_error = None;
        let res = match std::env::var("ASTAR_WT_ADDR") {
            Ok(addr) => self.station.connect_wt_at(node, &addr),
            Err(_) => self.station.connect_wt(node),
        };
        self.note_connect(node, res);
    }

    /// The direct-address dial path (astar-3939): WT token auth exactly as
    /// [`Conn::connect_wt`], but dial the typed `host[:port]` instead of the
    /// registrar-resolved node address — the station's `connect_wt_at`. The
    /// address string doubles as the dest selector/display label, mirroring
    /// the Mac's `connectWT(destNode:address:)` call with `node == address`.
    fn connect_address(&mut self, addr: &str) {
        self.dialed = Some(addr.to_string());
        self.last_error = None;
        if self.has_portal {
            let res = self.station.connect_wt_at(addr, addr);
            self.note_connect(addr, res);
        } else {
            // The generic node-secret path resolves through the registrar
            // only — it has no explicit-address dial — so an address dial
            // needs the portal account.
            self.last_error = Some(
                "No account: set ASTAR_WT_USER + ASTAR_WT_PASSWORD + ASTAR_WT_NODE".to_string(),
            );
        }
    }

    fn disconnect(&mut self) {
        self.station.disconnect();
        self.dialed = None;
        self.last_error = None;
    }

    fn m17_available(&self) -> bool {
        self.station.m17_available()
    }

    fn connect_m17(&mut self, host: &str, port: u16, module: char, callsign: &str) {
        self.dialed = Some(format!("{host}:{port}/{module}"));
        self.last_error = None;
        let res = self.station.m17_connect(host, port, module, callsign);
        self.note("M17 connect failed", res);
    }

    fn m17_disconnect(&mut self) {
        self.station.m17_disconnect();
        self.dialed = None;
        self.last_error = None;
    }

    fn set_ptt(&mut self, on: bool) {
        let res = self.station.set_ptt(on);
        self.note("PTT failed", res);
    }

    fn send_dtmf(&mut self, digit: char) {
        let res = self.station.send_dtmf(digit);
        self.note("DTMF failed", res);
    }

    fn send_dtmf_string(&mut self, digits: &str) {
        let res = self.station.send_dtmf_string(digits);
        self.note("DTMF failed", res);
    }

    fn cancel_dtmf(&mut self) {
        self.station.cancel_dtmf();
    }

    fn list_devices(&mut self) -> (Vec<String>, Vec<String>) {
        // Enumerates via cpal (names the console session resolves against).
        // On failure the status card says so and the pickers fall back to
        // offering only "System default".
        match self.station.list_devices() {
            Ok(lists) => lists,
            Err(e) => {
                self.last_error = Some(format!("Audio devices: {e}"));
                (Vec::new(), Vec::new())
            }
        }
    }

    fn set_devices(&mut self, input: Option<String>, output: Option<String>) {
        // The station holds the selection and applies it on each connect, so
        // this survives reconnects without re-pushing.
        self.station.set_devices(input, output);
    }

    fn selected_devices(&self) -> (Option<String>, Option<String>) {
        self.station.selected_devices()
    }

    fn set_audio(&mut self, audio: &AudioSettings) {
        // The engine-backed knobs go to the station as standing prefs; its
        // console session re-pushes them onto every (re)connect, so quick
        // config survives reconnects without the UI re-sending.
        self.station.set_input_gain(audio.input_gain);
        self.station.set_output_gain(audio.output_gain);
        self.station.set_noise_reduction(audio.noise_reduction);
        self.station.set_compression(audio.compression);
        self.station.set_compression_level(audio.compression_level);
        self.station.set_tx_trim(audio.tx_trim);
        // RX/output compression (iax-a4e7): automatic leveling of the
        // received audio, reusing the mic-path compressor on the output bus.
        // Shared across networks (no M17 override), like output gain above.
        self.station.set_rx_compression(audio.rx_compression);
        self.station
            .set_rx_compression_level(audio.rx_compression_level);
        // VOX threshold/hangtime and full duplex have no station API yet —
        // on the Mac they live in the app-side VOX gate / RX-mute logic
        // (AstarCore CallSession), which gui-rs grows in a later nugget. The
        // values persist and ride this seam so that nugget only adds the gate.
        self.audio = audio.clone();
    }

    fn audio(&self) -> AudioSettings {
        self.audio.clone()
    }

    fn snapshot(&self) -> Snapshot {
        let cs = self.station.snapshot();
        let (status, call_error) = map_call_status(&cs.status);
        let (tx_level, rx_level) = meter_levels(cs.ptt, self.tx_held_db, self.rx_held_db);

        // Prefer the live call's node id; fall back to what we dialed.
        let dialed_node = cs
            .calls
            .first()
            .map(|c| c.node.clone())
            .or_else(|| self.dialed.clone());

        Snapshot {
            status,
            dialed_node,
            // TODO: the core has no friendly node name yet; surfaced by the
            // node-name lookup nugget once the core provides one.
            node_name: None,
            transmitting: cs.ptt,
            receiving: cs.remote_ptt,
            tx_level,
            rx_level,
            // An action error (ours) wins over the call-state cause; both
            // land on the status card.
            error: self.last_error.clone().or(call_error),
            needs_account: self.needs_account(),
            // The console session already clears this when the call ends.
            negotiated_format: cs.negotiated_format,
            // Engine sequencer progress (astar-7d21): 0/0 when nothing plays.
            dtmf_played: cs.dtmf_played,
            dtmf_total: cs.dtmf_total,
        }
    }

    fn tick(&mut self) {
        // Advance the meter smoothers (~20 Hz): push the station's raw dBFS
        // levels through the sliding-max windows so the bars read steadily.
        let cs = self.station.snapshot();
        let now = std::time::Instant::now();
        self.tx_held_db = self.tx_peak.push(cs.tx_level_db, now);
        self.rx_held_db = self.rx_peak.push(cs.rx_level_db, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_states_round_trip() {
        assert_eq!(DemoState::parse("idle"), Some(DemoState::Idle));
        assert_eq!(DemoState::parse("RX"), Some(DemoState::Rx));
        assert_eq!(DemoState::parse("Tx"), Some(DemoState::Tx));
        assert_eq!(DemoState::parse("error"), Some(DemoState::Error));
        assert_eq!(DemoState::parse("bogus"), None);
    }

    #[test]
    fn error_scene_is_disconnected_with_an_error_line() {
        let c = DemoConn::new(DemoState::Error);
        let s = c.snapshot();
        assert_eq!(s.status, Status::Disconnected);
        assert!(s.error.as_deref().is_some_and(|e| !e.is_empty()));
    }

    #[test]
    fn demo_connect_dials_then_answers() {
        let mut c = DemoConn::new(DemoState::Idle);
        c.connect_wt("546054");
        assert_eq!(c.snapshot().status, Status::Connecting);
        assert_eq!(c.snapshot().dialed_node.as_deref(), Some("546054"));
        for _ in 0..40 {
            c.tick();
        }
        assert_eq!(c.snapshot().status, Status::Connected, "demo call answers");
        c.disconnect();
        assert_eq!(c.snapshot().status, Status::Disconnected);
    }

    #[test]
    fn demo_connect_node_behaves_like_wt() {
        let mut c = DemoConn::new(DemoState::Idle);
        c.connect_node("2000");
        assert_eq!(c.snapshot().status, Status::Connecting);
        assert_eq!(c.snapshot().dialed_node.as_deref(), Some("2000"));
    }

    #[test]
    fn demo_connect_address_behaves_like_wt() {
        // The smart dial field's address path (astar-3939): the demo accepts
        // both shapes, the address string doubling as the display label.
        let mut c = DemoConn::new(DemoState::Idle);
        c.connect_address("node.example-net.com:4569");
        assert_eq!(c.snapshot().status, Status::Connecting);
        assert_eq!(
            c.snapshot().dialed_node.as_deref(),
            Some("node.example-net.com:4569")
        );
    }

    #[test]
    fn connect_failure_copy_distinguishes_node_from_address_dials() {
        use astar_station::{Asl3Error, StationError};

        let resolve = StationError::Resolve(Asl3Error::Dns("lookup failed".into()));
        // A node dial's resolve failure means no live registration (the Mac's
        // wording, per Rob: short and plain).
        assert_eq!(
            connect_failure_message(&resolve, "61057"),
            "Node 61057 is not registered in Allstar."
        );
        // An address-shaped dial (astar-427f) never asked the registrar, so
        // "not registered" would mislead — name the address instead.
        assert_eq!(
            connect_failure_message(&resolve, "127.0.0.1:4569"),
            "Couldn’t reach 127.0.0.1:4569."
        );
        assert_eq!(
            connect_failure_message(&resolve, "my-node.example.com"),
            "Couldn’t reach my-node.example.com."
        );
        // Every other failure keeps the generic seam wording.
        assert_eq!(
            connect_failure_message(&StationError::AlreadyConnected, "61057"),
            "Connect failed: a call is already in progress"
        );
    }

    #[test]
    fn call_status_maps_to_ui_status_and_error() {
        use astar_station::CallStatus;
        let map = |cs: &CallStatus| map_call_status(cs);

        assert_eq!(map(&CallStatus::Idle), (Status::Disconnected, None));
        assert_eq!(map(&CallStatus::Dialing), (Status::Connecting, None));
        assert_eq!(map(&CallStatus::Answered), (Status::Connected, None));

        let (st, err) = map(&CallStatus::Failed {
            reason: "REJECTED".into(),
        });
        assert_eq!(st, Status::Disconnected);
        assert!(err.is_some_and(|e| e.contains("REJECTED")));

        let (st, err) = map(&CallStatus::Hangup {
            reason: "busy".into(),
        });
        assert_eq!(st, Status::Disconnected);
        assert!(err.is_some_and(|e| e.contains("busy")));

        // A clean hangup with no cause isn't an error.
        assert_eq!(
            map(&CallStatus::Hangup {
                reason: String::new()
            }),
            (Status::Disconnected, None)
        );
    }

    #[test]
    fn tx_meter_is_floored_unless_keyed() {
        // Unkeyed: the mic stays open for metering, but the TX meter must sit
        // at the floor (mirrors the Mac: `session.ptt ? txDBHeld : -60`).
        assert_eq!(meter_levels(false, -10.0, -60.0), (0.0, 0.0));
        // Keyed: the held TX level shows.
        let (tx, rx) = meter_levels(true, -30.0, -60.0);
        assert_eq!(tx, 0.5);
        assert_eq!(rx, 0.0);
        // RX is never gated by our PTT.
        let (_, rx) = meter_levels(false, -60.0, -30.0);
        assert_eq!(rx, 0.5);
    }

    #[test]
    fn station_config_carries_portal_and_codec_policy() {
        use astar_station::{CodecPolicy, PortalCredentials};

        // The construction-time seam (astar-efba, mirroring the Mac's
        // `CallSession.stationConfig`): wideband is always on (astar-e542),
        // so the codec policy is unconditionally PreferSlin16 alongside the
        // credentials — with or without a portal.
        let portal = Some(PortalCredentials {
            user: "AJ7HR".into(),
            password: "pw".into(),
            node: "77777".into(),
        });
        let config = station_config(portal);
        assert!(config.portal.is_some());
        assert_eq!(config.codec_policy, CodecPolicy::PreferSlin16);

        let config = station_config(None);
        assert!(config.portal.is_none());
        assert_eq!(config.codec_policy, CodecPolicy::PreferSlin16);
    }

    #[test]
    fn demo_connected_scenes_negotiate_wideband() {
        use astar_station::VoiceFormat;

        // The connected scene negotiates slin16, so shots render the badge…
        let c = DemoConn::new(DemoState::Connected);
        assert_eq!(c.snapshot().negotiated_format, Some(VoiceFormat::Slin16));
        assert_eq!(c.snapshot().codec_badge(), Some("slin16"));
        assert!(c.snapshot().codec_is_wideband());
        // …and a wideband call stays wideband through PTT/RX, so the badge
        // never flickers while keying in an interactive demo.
        assert_eq!(
            DemoConn::new(DemoState::Tx).snapshot().negotiated_format,
            Some(VoiceFormat::Slin16)
        );
        assert_eq!(
            DemoConn::new(DemoState::Rx).snapshot().negotiated_format,
            Some(VoiceFormat::Slin16)
        );
    }

    #[test]
    fn demo_uncalled_scenes_negotiate_nothing() {
        // No call = no codec: idle/connecting/error shots show no badge.
        for state in [DemoState::Idle, DemoState::Connecting, DemoState::Error] {
            let c = DemoConn::new(state);
            assert_eq!(c.snapshot().negotiated_format, None, "{state:?}");
            assert_eq!(c.snapshot().codec_badge(), None, "{state:?}");
        }
    }

    #[test]
    fn demo_lists_fixed_devices() {
        let mut c = DemoConn::new(DemoState::Idle);
        let (inputs, outputs) = c.list_devices();
        assert_eq!(inputs, ["Demo Microphone", "USB Audio CODEC"]);
        assert_eq!(outputs, ["Demo Speakers", "USB Audio CODEC"]);
        // Stable across calls — screenshots and pickers must not jitter.
        assert_eq!(c.list_devices(), (inputs, outputs));
    }

    #[test]
    fn demo_device_selection_round_trips() {
        let mut c = DemoConn::new(DemoState::Idle);
        // Fresh demo: system default on both sides.
        assert_eq!(c.selected_devices(), (None, None));
        c.set_devices(Some("USB Audio CODEC".into()), None);
        assert_eq!(c.selected_devices(), (Some("USB Audio CODEC".into()), None));
        // Back to system default.
        c.set_devices(None, None);
        assert_eq!(c.selected_devices(), (None, None));
    }

    #[test]
    fn demo_audio_prefs_default_to_engine_defaults_then_round_trip() {
        let mut c = DemoConn::new(DemoState::Idle);
        // Before any push: the engine defaults (mirrors AudioSettings).
        assert_eq!(c.audio(), AudioSettings::default());

        // Every quick-config knob survives the seam verbatim.
        let prefs = AudioSettings {
            input_gain: 1.5,
            output_gain: 2.5,
            compression: true,
            compression_level: 0.42,
            tx_trim: 0.8,
            noise_reduction: true,
            rx_compression: true,
            rx_compression_level: 0.65,
            full_duplex: true,
            vox_threshold_dbfs: -25.0,
            vox_hangtime_ms: 1200,
            ..AudioSettings::default()
        };
        c.set_audio(&prefs);
        assert_eq!(c.audio(), prefs);
    }

    #[test]
    fn demo_records_sent_dtmf_in_order() {
        let mut c = DemoConn::new(DemoState::Connected);
        assert!(c.sent_dtmf().is_empty());
        for d in ['*', '3', '0', '#', 'A'] {
            c.send_dtmf(d);
        }
        assert_eq!(c.sent_dtmf(), ['*', '3', '0', '#', 'A']);
    }

    // -- send_dtmf_string sequencer seam (astar-7d21) -----------------------

    #[test]
    fn demo_records_sent_strings_in_order() {
        let mut c = DemoConn::new(DemoState::Connected);
        assert!(c.sent_dtmf_strings().is_empty());
        c.send_dtmf_string("*3546054");
        c.send_dtmf_string("*70");
        assert_eq!(c.sent_dtmf_strings(), vec!["*3546054", "*70"]);
    }

    #[test]
    fn demo_sequence_progresses_per_tick_and_completes() {
        let mut c = DemoConn::new(DemoState::Connected);
        c.send_dtmf_string("*30");
        // The first digit goes out immediately (mirrors the engine).
        let s = c.snapshot();
        assert_eq!((s.dtmf_played, s.dtmf_total), (1, 3));
        // Each digit advances after TICKS_PER_DIGIT ticks; the queue then
        // retires one slot after the last digit.
        for _ in 0..40 {
            c.tick();
        }
        let s = c.snapshot();
        assert_eq!(
            (s.dtmf_played, s.dtmf_total),
            (0, 0),
            "a finished sequence clears its progress"
        );
    }

    #[test]
    fn demo_cancel_clears_progress() {
        let mut c = DemoConn::new(DemoState::Connected);
        c.send_dtmf_string("*3546054");
        c.tick();
        c.cancel_dtmf();
        let s = c.snapshot();
        assert_eq!((s.dtmf_played, s.dtmf_total), (0, 0));
    }

    #[test]
    fn demo_seeded_progress_renders_mid_sequence() {
        // The dialpad-sending shot boots from this: deterministic, no ticking.
        let mut c = DemoConn::new(DemoState::Connected);
        c.seed_dtmf_progress("*3546054", 3);
        let s = c.snapshot();
        assert_eq!((s.dtmf_played, s.dtmf_total), (3, 8));
    }

    #[test]
    fn demo_setups_are_stable_and_switcher_ready() {
        let setups = DemoConn::demo_setups();
        assert_eq!(setups.len(), 2);
        // Stable ids/names — screenshots and tests key off them.
        assert_eq!(setups[0].id, "demo-uci150");
        assert_eq!(setups[0].name, "UCI150 desk");
        assert_eq!(setups[1].id, "demo-headset");
        assert_eq!(setups[1].name, "Jabra mobile");
        // The UCI150 rig carries audio overrides (shots show them applied);
        // its devices exist in the demo device lists so an apply never trips
        // the missing-device guard.
        assert!(setups[0].input_gain.is_some() && setups[0].compression == Some(true));
        assert!(DemoConn::INPUTS.contains(&setups[0].input_device.as_deref().unwrap()));
        assert!(DemoConn::OUTPUTS.contains(&setups[0].output_device.as_deref().unwrap()));
        // The headset rig has no overrides — the keep-global half of the merge.
        assert_eq!(setups[1].input_gain, None);
        assert_eq!(setups[1].compression, None);
        assert_eq!(DemoConn::demo_setups(), setups, "stable across calls");
    }

    #[test]
    fn idle_snapshot_is_disconnected() {
        let c = DemoConn::new(DemoState::Idle);
        assert_eq!(c.snapshot().status, Status::Disconnected);
    }

    #[test]
    fn tx_snapshot_keys_tx_only() {
        let c = DemoConn::new(DemoState::Tx);
        let s = c.snapshot();
        assert!(s.transmitting && !s.receiving);
        assert!(s.tx_level > 0.0);
    }

    #[test]
    fn rx_snapshot_keys_rx_only() {
        let c = DemoConn::new(DemoState::Rx);
        let s = c.snapshot();
        assert!(s.receiving && !s.transmitting);
        assert!(s.rx_level > 0.0);
    }

    #[test]
    fn levels_are_deterministic_and_animate() {
        let mut a = DemoConn::new(DemoState::Tx);
        let mut b = DemoConn::new(DemoState::Tx);
        // Same tick count → identical level (deterministic, not random).
        assert_eq!(a.snapshot().tx_level, b.snapshot().tx_level);
        a.tick();
        b.tick();
        assert_eq!(a.snapshot().tx_level, b.snapshot().tx_level);
        // Advancing ticks changes the level (it animates).
        let before = a.snapshot().tx_level;
        for _ in 0..3 {
            a.tick();
        }
        assert_ne!(a.snapshot().tx_level, before);
    }

    #[test]
    fn hamlink_availability_defaults_off_and_demo_knob_flips_it() {
        let mut c = DemoConn::new(DemoState::Connected);
        assert!(!c.hamlink_available());
        c.set_hamlink_available(true);
        assert!(c.hamlink_available());
    }

    #[test]
    fn m17_availability_defaults_off_and_demo_knob_flips_it() {
        let mut c = DemoConn::new(DemoState::Connected);
        assert!(!c.m17_available());
        c.set_m17_available(true);
        assert!(c.m17_available());
    }

    #[test]
    fn demo_connect_m17_records_the_tuple_and_dials_then_answers() {
        let mut c = DemoConn::new(DemoState::Idle);
        assert!(c.sent_m17_connects().is_empty());
        c.connect_m17("m17.example.net", 17000, 'A', "AJ7HR");
        assert_eq!(
            c.sent_m17_connects(),
            vec![(
                "m17.example.net".to_string(),
                17000,
                'A',
                "AJ7HR".to_string()
            )]
        );
        assert_eq!(c.snapshot().status, Status::Connecting);
        for _ in 0..40 {
            c.tick();
        }
        assert_eq!(
            c.snapshot().status,
            Status::Connected,
            "the demo M17 dial answers"
        );
        c.m17_disconnect();
        assert_eq!(c.snapshot().status, Status::Disconnected);
    }

    #[test]
    fn wave_stays_in_unit_range() {
        let mut c = DemoConn::new(DemoState::Tx);
        for _ in 0..200 {
            let s = c.snapshot();
            assert!((0.0..=1.0).contains(&s.tx_level), "tx={}", s.tx_level);
            c.tick();
        }
    }
}
