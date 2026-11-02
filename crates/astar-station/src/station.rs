// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use astar_audio::AudioBackend;
use astar_console::{
    CallStatus, ConsoleConfig, ConsoleSession, ConsoleState, HeardEntry, LinkConnectSpec,
    M17MonitorEdge, M17MonitorStatus, OperatingMode, RecordTaps, RegisterOutcome,
};
use astar_iax::{CallMode, CodecPolicy, IncomingCallPolicy, LinkEvent, LinkMode, LinkRoster};
use astar_iax_core::session::auth::Secret;
use astar_record::frames::FrameEvent;
use astar_record::tap::{
    FRAME_TAP_CAPACITY, HEARD_TAP_CAPACITY, SENT_TAP_CAPACITY, Tap, tap_channel,
};
use astar_record::{Layout, RecordError, RecordState, Recorder, RecorderConfig, RecorderInputs};

use crate::config::NodeConfig;
use crate::{RegisterConfig, StationConfig, StationError, StationEvent};

type BackendFactory = Box<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync>;

/// Default in-band DTMF tone duration (ms) for [`Station::send_dtmf`]. One
/// keypress = one complete tone of this length.
pub const DEFAULT_DTMF_MS: u32 = 250;

/// Floor for a DTMF tone duration: short enough to feel responsive, long enough
/// that the receiver's Goertzel decoder sees several analysis blocks (~25 ms
/// each) and reliably confirms the digit.
const MIN_DTMF_MS: u32 = 60;

/// Announce priority for an injected DTMF tone — above the default spoken-
/// announcement priority (5) so a dialed digit preempts a routine announcement.
const DTMF_ANNOUNCE_PRIORITY: u8 = 8;

/// Silence between sequenced digits for [`Station::send_dtmf_string`] (ms).
/// Long enough that the far-end decoder sees a clean falling edge between two
/// identical digits; also comfortably above the protocol-mode FSM rate limit
/// (50 ms), so no sequenced digit is ever dropped by it.
pub const DEFAULT_DTMF_GAP_MS: u32 = 100;

/// The active multi-digit command (iax-4b7a). Poll-driven: [`Station::snapshot`]
/// pumps it via `pump_dtmf`, so it advances at the embedder's poll cadence and
/// costs nothing while `None` (byte-identical defaults for embedders that never
/// call `send_dtmf_string`). Lock order is `dtmf_queue` → `session`, never the
/// reverse.
struct DtmfQueue {
    /// The validated command, in send order.
    digits: Vec<char>,
    /// Digits already handed to the single-digit send path.
    played: u32,
    /// When the next digit may start (previous tone + inter-digit gap).
    next_due: std::time::Instant,
}

/// Whether `c` is one of the 16 DTMF keys (`0-9`, `*`, `#`, `A-D`). Delegates to
/// the codec's keypad table so the dialer surface and the generator stay in sync.
fn is_dtmf_key(c: char) -> bool {
    astar_codec::dtmf::is_dtmf_digit(c)
}

/// How [`Station::send_dtmf`] emits a digit on the active call (iax-7fff).
/// Settable per station via [`Station::set_dtmf_mode`]; applies to the digits
/// sent after the change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DtmfMode {
    /// Synthesize the dual-tone waveform into the call's TX audio path
    /// (in-band; the iax-68eb generator injected via the announce path). The
    /// default — the behavior [`Station::send_dtmf`] has always had. For
    /// nodes/paths that only decode audio tones.
    #[default]
    InBand,
    /// Send out-of-band IAX2 protocol DTMF frames (`DtmfBegin`/`DtmfEnd`,
    /// RFC 5456 §6.7) — what Asterisk/`AllStar` expects by default. No tone
    /// audio is injected; the tone duration argument is ignored (the frame
    /// pair carries the digit only).
    Protocol,
}

/// What one [`Station::send_dtmf_for`] call will emit, decided by
/// [`plan_dtmf`]. Split out so the mode switch is unit-testable without a live
/// call: the `Tone` PCM is exactly what the in-band path injects, and
/// `Protocol` proves no PCM is synthesized at all.
#[derive(Debug, PartialEq, Eq)]
enum DtmfEmission {
    /// In-band: inject this PCM to-air via the announce path.
    Tone(Vec<i16>),
    /// Out-of-band: hand the digit to the protocol engine
    /// (`DtmfBegin`/`DtmfEnd` frames).
    Protocol(char),
}

/// Validate `digit` and decide the emission for `mode`. The digit contract is
/// mode-independent: the 16 DTMF keys are accepted, everything else is
/// [`StationError::InvalidDigit`] — before any connected-state check.
///
/// `sample_rate` is the LIVE station pipeline rate (iax-8d2f): the announce
/// path plays [`DtmfEmission::Tone`] PCM at that rate unresampled, so a tone
/// synthesized at a fixed 8 kHz comes out at double pitch / half duration on
/// a slin16 (16 kHz) pipeline — garbling the digit for far-end decoders.
fn plan_dtmf(
    mode: DtmfMode,
    digit: char,
    duration_ms: u32,
    sample_rate: u32,
) -> Result<DtmfEmission, StationError> {
    // Accept the full 16-key DTMF set (0-9 * # A-D); the codec generator
    // synthesizes all of them. Anything else is rejected up front.
    if !is_dtmf_key(digit) {
        return Err(StationError::InvalidDigit);
    }
    match mode {
        DtmfMode::InBand => {
            let ms = duration_ms.max(MIN_DTMF_MS);
            // Synthesize the in-band dual-tone (iax-68eb) at the pipeline rate.
            let pcm = astar_codec::dtmf::synthesize(
                digit,
                sample_rate,
                ms,
                astar_codec::dtmf::DEFAULT_TONE_DBFS,
            )
            .ok_or(StationError::InvalidDigit)?;
            Ok(DtmfEmission::Tone(pcm))
        }
        DtmfMode::Protocol => Ok(DtmfEmission::Protocol(digit)),
    }
}

/// A runtime hook that resolves a secret (the registrar password) by username,
/// on demand. The resolved secret is consumed immediately into the registrar —
/// it is never stored in config, a snapshot, an event, or a log.
pub type SecretResolver = dyn Fn(&str) -> String + Send + Sync;

/// A request to monitor one M17 reflector module, receive-only (see
/// [`Station::m17_monitor_connect`]).
///
/// It is a type of its own, not [`Station::m17_connect`]'s positional
/// arguments, because it names a different kind of leg. A monitor never
/// transmits, is not excluded by an IAX2 call, and [`Station::disconnect`]
/// never ends it: a hang-up takes the call and leaves the engine the monitor
/// feeds. It is feature-independent, so a
/// caller builds it the same way in every build. It carries no secret: an
/// M17 link needs none, and the callsign is the station's public identity on
/// the reflector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M17MonitorRequest {
    /// Reflector host name or IP literal. Resolved on the monitor's own
    /// thread, never on the caller's.
    pub host: String,
    /// Reflector UDP port.
    pub port: u16,
    /// Module letter. Lowercase is accepted and uppercased.
    pub module: char,
    /// Callsign the monitor links as. It goes out in CONN, PONG and DISC only.
    pub callsign: String,
}

/// What [`Station::record_start`] records, and where. Carries no secret.
///
/// A struct rather than positional arguments because the app's human file
/// name (`<date> <time> <network> <node or reflector>.flac`) needs the
/// operator's label and time zone as well as the folder and layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRequest {
    /// The recordings folder. Created if missing.
    pub dir: std::path::PathBuf,
    pub layout: Layout,
    /// Also write unamblify codec-frame captures under `dir/captured/`.
    pub frames: bool,
    /// The `<network> <node or reflector>` part of the file name, e.g.
    /// `"AllStar 2000"`.
    pub label: String,
    /// The operator's offset from UTC, for the date and time in the name.
    pub utc_offset_secs: i32,
}

/// The app recorder and the server's frame tap, behind one lock. Lock order
/// is `recording` → `session`, never the reverse.
#[derive(Default)]
struct RecordSlot {
    /// The app recording, while one runs (or until a stop collects one that
    /// stopped by itself).
    recorder: Option<Recorder>,
    /// The frame tap the app recorder installed, if it asked for frames.
    app_frames: Option<Tap<FrameEvent>>,
    /// The server's frame tap, set through [`Station::set_frame_tap`].
    server_frames: Option<Tap<FrameEvent>>,
    /// The last recording's final state, for [`Station::record_state`] after
    /// a stop. `None` before any recording.
    last: Option<RecordState>,
    /// `record_start` is making a recorder with the lock released. It
    /// refuses a second start; a stop in this window sets `stop_requested`.
    starting: bool,
    /// A stop arrived while `starting`: the start, on retaking the lock,
    /// stops the recorder it made instead of installing it.
    stop_requested: bool,
    /// A recorder is being joined with the lock released. `record_start`
    /// refuses while it is set.
    stopping: bool,
    /// Test seam: `record_start` waits on the first barrier and then the
    /// second once it has released the lock, so a test can land a stop
    /// inside the window.
    #[cfg(test)]
    start_pause: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
}

impl RecordSlot {
    fn busy(&self) -> bool {
        self.starting || self.stopping
    }
}

/// Clears the slot's `starting`/`stopping`/`stop_requested` flags if the
/// window they cover unwinds, so a panic there cannot refuse every later
/// start. On the normal path the owner clears them under the lock and calls
/// [`BusyGuard::disarm`]. At most one of `starting`/`stopping` is ever set
/// (each refuses or bypasses the other), so clearing both is safe.
struct BusyGuard<'a> {
    slot: &'a Mutex<RecordSlot>,
    armed: bool,
}

impl<'a> BusyGuard<'a> {
    fn new(slot: &'a Mutex<RecordSlot>) -> Self {
        BusyGuard { slot, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.starting = false;
            slot.stopping = false;
            slot.stop_requested = false;
        }
    }
}

/// How long an M17 monitor goes without hearing its reflector before it gives
/// the link up. This is `SessionFsm`'s own default, and the figure
/// [`Station::m17_connect`] uses, so the two kinds of leg fail on the same
/// silence.
#[cfg(feature = "m17")]
const M17_MONITOR_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// One station from the `EchoLink` directory's list, as
/// [`Station::echolink_stations`] hands it out.
///
/// It carries no IP address. The directory lists every station's address,
/// and the engine needs it to place a call, but a front-end only ever needs
/// the callsign or node number to name one, so the address stays inside the
/// engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLinkStationInfo {
    /// Callsign, uppercased.
    pub call: String,
    /// The station's description, with the directory's status tag removed.
    pub description: String,
    /// Node number.
    pub node: u32,
    /// `"user"`, `"link"` (callsign ends in `-L`), `"repeater"` (`-R`) or
    /// `"conference"` (callsign wrapped in stars, like `*ECHOTEST*`).
    pub kind: &'static str,
    /// `"online"` (free), `"busy"` (in a contact) or `"unknown"` (the
    /// description carried no status tag).
    pub status: &'static str,
}

#[cfg(feature = "echolink")]
impl From<&astar_console::echolink::StationEntry> for EchoLinkStationInfo {
    fn from(e: &astar_console::echolink::StationEntry) -> Self {
        use astar_console::echolink::{StationKind, StationStatus};
        Self {
            call: e.call.clone(),
            description: e.description.clone(),
            node: e.node,
            kind: match e.kind {
                StationKind::User => "user",
                StationKind::Link => "link",
                StationKind::Repeater => "repeater",
                StationKind::Conference => "conference",
            },
            status: match e.status {
                StationStatus::Online => "online",
                StationStatus::Busy => "busy",
                StationStatus::Unknown => "unknown",
            },
        }
    }
}

/// What [`Station::echolink_state`], [`Station::echolink_node_poll`] and
/// [`Station::echolink_node_peers`] hand out: astar-console's own types when
/// the `echolink` feature is on.
#[cfg(feature = "echolink")]
pub use astar_console::echolink::{EchoLinkNodeEdge, EchoLinkPeerStatus, EchoLinkState};

/// Stand-ins for the `EchoLink` state types in a build without the
/// `echolink` feature, so every `Station::echolink_*` method keeps the same
/// signature in every build. They have no values: the methods that would
/// return one return `None` or an empty list.
#[cfg(not(feature = "echolink"))]
mod echolink_off {
    /// No `EchoLink` in this build; [`crate::Station::echolink_state`] is
    /// always `None`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EchoLinkState {}
    /// No `EchoLink` in this build; [`crate::Station::echolink_node_poll`]
    /// is always empty.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EchoLinkNodeEdge {}
    /// No `EchoLink` in this build; [`crate::Station::echolink_node_peers`]
    /// is always empty.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum EchoLinkPeerStatus {}
}
#[cfg(not(feature = "echolink"))]
pub use echolink_off::{EchoLinkNodeEdge, EchoLinkPeerStatus, EchoLinkState};

/// The refusal for an `EchoLink` password the directory line cannot carry.
/// It names the kinds of character, never the password.
pub const ECHOLINK_PASSWORD_UNSENDABLE: &str = "The EchoLink password has a character the \
     directory can't take: a line break, or a non-ASCII character such as ¬.";

/// Whether `password` can go into the directory's login line as it is.
///
/// The line is `l` + callsign + `0xAC 0xAC` + password + CR + …, written as
/// UTF-8 bytes, so a CR or LF would end the password's field early and a
/// `0xAC` byte (which `¬` and some other non-ASCII characters carry) would
/// look like the separator. A password holding any of those three bytes is
/// refused before anything is sent.
#[must_use]
pub fn echolink_password_is_sendable(password: &str) -> bool {
    !password.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0xAC))
}

/// The sentence every `EchoLink` call returns in a build without the
/// `echolink` feature.
#[cfg(not(feature = "echolink"))]
const ECHOLINK_NOT_COMPILED: &str = "echolink support not compiled";

/// An IAX2 station: owns the single-call session, audio, and the ASL3 connect
/// recipe. PTT-source-agnostic: callers drive keying via [`Station::set_ptt`];
/// hardware PTT lives in `astar-ptt` / `astar-serial-sys`. Internally
/// thread-safe (the session sits behind an
/// `Arc<Mutex<>>`), so every method takes `&self`.
///
/// Vendor-neutral: [`Station::connect`] is the generic IAX2 path (works against
/// stock Asterisk); [`Station::connect_wt`] is one `AllStar` convenience method.
/// A cached capture-rate probe: which input device was asked about, and what
/// rate it said it would open at (`0` = could not tell).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureProbe {
    device: Option<String>,
    rate: u32,
}

pub struct Station {
    session: Arc<Mutex<ConsoleSession>>,
    make_backend: BackendFactory,
    config: StationConfig,
    /// Selected `(input, output)` capture/playback device substrings applied to
    /// the next `connect`. Seeded from [`StationConfig`]; mutable via
    /// [`Station::set_devices`] so a UI (or FFI consumer) can list devices, let
    /// the user pick, and connect — without rebuilding the station. `None` =
    /// system default.
    devices: Mutex<(Option<String>, Option<String>)>,
    /// Last drained lifecycle edges, for [`Station::next_event`] diffing.
    last_event_state: Mutex<EventTracker>,
    /// A runtime override for the Node-mode config, set via
    /// [`Station::set_node_config`]. Takes precedence over `config.node` and the
    /// `NodeConfig::default()` fallback when starting the engine.
    node_config: Mutex<Option<NodeConfig>>,
    /// Runtime secret resolver for outbound node registration (Phase 7). The
    /// registrar password is resolved on demand and consumed straight into the
    /// registrar — never stored in config/snapshot/event/log.
    secret_resolver: Mutex<Option<Box<SecretResolver>>>,
    /// Cached answer to "what rate would the selected capture device open
    /// at", for the idle noise-reduction capability line.
    ///
    /// Keyed on the selected input device, and refreshed only when that
    /// changes. The probe costs a host construction plus one enumeration —
    /// measured at ~1.4 ms for the default device, ~0.9 ms for a named one —
    /// and `snapshot()` is polled at roughly 20 Hz, so doing it per poll
    /// would burn about 2.8% of a core to redraw a caption, inside the
    /// session lock. Once per device change is enough.
    ///
    /// The tradeoff this accepts: a device that changes its own advertised
    /// rates underneath us (a re-plugged interface reporting differently)
    /// keeps the cached answer until the selection changes. The line is a
    /// prediction either way, and the live status supersedes it the moment
    /// a stream opens.
    capture_probe: Mutex<Option<CaptureProbe>>,
    /// Mic monitor (iax-2377): opens the input device WITHOUT a call so a
    /// front-end can preview/characterize the mic. `Some` only while monitoring,
    /// holding the RESOLVED device name beside the monitor so `monitor_start`
    /// can tell "this device again" (a no-op) from "a different device" (a
    /// switch). Never started while a call is active (the device would be
    /// double-opened); `Drop` (on `monitor_stop`) releases the device.
    monitor: Mutex<Option<(String, astar_audio::MicMonitor)>>,
    /// Buffered link lifecycle events (iax-1075): [`Station::next_link_event`]
    /// drains the session in bursts and hands them out one per poll.
    link_events_buf: Mutex<std::collections::VecDeque<astar_iax::LinkEvent>>,
    /// Re-register supervision (iax-177d): the last [`Station::register`]
    /// config (secret-free — the resolver re-mints the credential on each
    /// attempt) plus the pending retry `(due, next_backoff)`. Armed when a
    /// `RegisterFailed` outcome drains through [`Station::next_event`];
    /// cleared by [`Station::deregister`] and on the next success.
    reg_supervise: Mutex<RegSupervise>,
    /// How [`Station::send_dtmf`] emits digits (iax-7fff): in-band tone
    /// (default) or out-of-band protocol frames. Set via
    /// [`Station::set_dtmf_mode`]; applies to the digits sent after the change.
    dtmf_mode: Mutex<DtmfMode>,
    /// Active `send_dtmf_string` sequence, if any (iax-4b7a).
    dtmf_queue: Mutex<Option<DtmfQueue>>,
    /// Extra directories to search for a runtime `libcodec2`, ahead of the
    /// hard-coded system paths (iax-f2b8 Task 4). `None`/default = empty (the
    /// system search paths only). Set via [`Station::set_codec_dirs`] so the
    /// app can point at its own bundled library before
    /// [`Station::m17_connect`].
    codec_dirs: Mutex<Vec<std::path::PathBuf>>,
    /// Memoized [`Station::m17_available`] probe, keyed on the exact
    /// `codec_dirs` value it was computed against (iax-f2b8 Task 4).
    /// `codec2_available`'s static backend does a full `Codec2::new()`
    /// construction (iax-f2b8 Task 2's ledgered "don't call in hot paths"
    /// warning), so this re-probes only when `codec_dirs` actually changes
    /// (typically once, at startup) rather than on every call — unlike a
    /// single cache-forever `OnceLock`, which would bake in a stale `false`
    /// from before an app ever called [`Station::set_codec_dirs`] pointing at
    /// its bundled `libcodec2`. Only compiled/read when the `m17` feature is
    /// enabled.
    #[cfg(feature = "m17")]
    codec2_probe_cache: Mutex<Option<(Vec<std::path::PathBuf>, bool)>>,
    /// Call recording: the app recorder and the server's frame tap. See
    /// [`Station::record_start`] and [`Station::set_frame_tap`].
    recording: Mutex<RecordSlot>,
}

/// Supervision state for automatic re-registration (iax-177d). Without it a
/// single failed refresh round killed the registration thread permanently and
/// the node silently fell out of the `AllStar` directory.
#[derive(Default)]
struct RegSupervise {
    cfg: Option<RegisterConfig>,
    retry: Option<(std::time::Instant, std::time::Duration)>,
    /// Which of `cfg.candidates()` the next attempt uses. Advanced on every
    /// retry, so sustained failure walks the list rather than hammering one
    /// address that has stopped answering. See `RegisterConfig::fallbacks`
    /// for why a registrar hostname's addresses are not interchangeable.
    peer_index: usize,
}

/// First re-register attempt lands 5 s after a failure; each subsequent
/// attempt doubles the wait, capped at 60 s (one `AllStar` refresh period).
const REG_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(5);
const REG_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Edge-detection state for [`Station::next_event`]. The console folds lifecycle
/// into a pollable [`ConsoleState`] with no event queue, so the station derives
/// discrete events by comparing each snapshot to the last observed edges.
#[derive(Default)]
struct EventTracker {
    /// Last observed `ConsoleState::answered_seq`; a change is a new-call
    /// answered edge (iax-a82f — replaces the old single-call `answered` bool
    /// that never reset in a multi-call node).
    answered_seq: u64,
    remote_ptt: bool,
    hung_up: bool,
    mode: OperatingMode,
}

impl Station {
    /// Create a station with the real `CpalBackend` (feature
    /// `device-audio`, on by default).
    #[cfg(feature = "device-audio")]
    #[must_use]
    pub fn new(config: StationConfig) -> Self {
        Self::with_backend_factory(
            config,
            Box::new(|| Box::new(astar_audio::CpalBackend::new())),
        )
    }

    /// Create a station with a custom backend factory (used by tests to inject
    /// a hardware-free backend).
    #[must_use]
    pub fn with_backend_factory(config: StationConfig, make_backend: BackendFactory) -> Self {
        let devices = Mutex::new((config.input.clone(), config.output.clone()));
        // Build idle (mode = Wt); a Node-mode config is applied below so the
        // listener actually binds. If that start fails (e.g. the bind port is
        // taken), the station falls back to Wt — the next `set_mode(Node)` can
        // retry.
        let configured_mode = config.mode;
        let station = Self {
            session: Arc::new(Mutex::new(ConsoleSession::new())),
            make_backend,
            config,
            devices,
            last_event_state: Mutex::new(EventTracker::default()),
            node_config: Mutex::new(None),
            secret_resolver: Mutex::new(None),
            monitor: Mutex::new(None),
            capture_probe: Mutex::new(None),
            link_events_buf: Mutex::new(std::collections::VecDeque::new()),
            reg_supervise: Mutex::new(RegSupervise::default()),
            dtmf_mode: Mutex::new(DtmfMode::default()),
            dtmf_queue: Mutex::new(None),
            codec_dirs: Mutex::new(Vec::new()),
            #[cfg(feature = "m17")]
            codec2_probe_cache: Mutex::new(None),
            recording: Mutex::new(RecordSlot::default()),
        };
        station.pin_station_policy();
        station.set_vocoder_trust(station.config.vocoder_trust.clone());
        if configured_mode == OperatingMode::Node {
            let _ = station.set_mode(OperatingMode::Node);
        }
        station
    }

    /// Embed into a host that already owns the [`ConsoleSession`]: share the
    /// **same** `Arc<Mutex<ConsoleSession>>` the host holds (this clones the
    /// `Arc`, it does not create a new session), so the station drives the WT
    /// connect/PTT/disconnect recipe over the host's session while the host
    /// keeps its own clone for diagnostic handlers it owns directly. One
    /// session, no divergence.
    ///
    /// Used by the `astar-inspect` harness, which dogfoods [`Station::connect_wt`]
    /// while retaining its own session-based parrot/DTMF/calibration handlers.
    ///
    /// Dropping the station ends every network on the shared session, the
    /// `EchoLink` directory login included, even though the host keeps the
    /// session itself.
    #[must_use]
    pub fn with_shared_session(
        config: StationConfig,
        session: Arc<Mutex<ConsoleSession>>,
        make_backend: BackendFactory,
    ) -> Self {
        let devices = Mutex::new((config.input.clone(), config.output.clone()));
        let configured_mode = config.mode;
        let station = Self {
            session,
            make_backend,
            config,
            devices,
            last_event_state: Mutex::new(EventTracker::default()),
            node_config: Mutex::new(None),
            secret_resolver: Mutex::new(None),
            monitor: Mutex::new(None),
            capture_probe: Mutex::new(None),
            link_events_buf: Mutex::new(std::collections::VecDeque::new()),
            reg_supervise: Mutex::new(RegSupervise::default()),
            dtmf_mode: Mutex::new(DtmfMode::default()),
            dtmf_queue: Mutex::new(None),
            codec_dirs: Mutex::new(Vec::new()),
            #[cfg(feature = "m17")]
            codec2_probe_cache: Mutex::new(None),
            recording: Mutex::new(RecordSlot::default()),
        };
        station.pin_station_policy();
        station.set_vocoder_trust(station.config.vocoder_trust.clone());
        if configured_mode == OperatingMode::Node {
            let _ = station.set_mode(OperatingMode::Node);
        }
        station
    }

    /// Replace the vocoder trust list: the USB serials of the AMBE sticks
    /// later D-Star/YSF/NXDN/DMR connects (and the `*_available` probes) may
    /// consider. A live session keeps the stick it already opened. Seeded
    /// from [`StationConfig::vocoder_trust`] at construction.
    pub fn set_vocoder_trust(&self, trust: astar_codec::vocoder_trust::VocoderTrust) {
        self.session.lock().unwrap().set_vocoder_trust(trust);
    }

    /// The current vocoder trust list.
    #[must_use]
    pub fn vocoder_trust(&self) -> astar_codec::vocoder_trust::VocoderTrust {
        self.session.lock().unwrap().vocoder_trust().clone()
    }

    /// Step 1 of every AMBE-network connect, under the session lock: open and
    /// reserve the ONE audio lane, and take a copy of the vocoder trust list
    /// the off-lock vocoder open will use.
    #[cfg(any(feature = "dstar", feature = "ysf", feature = "nxdn", feature = "dmr"))]
    fn open_voice_route_with_trust(
        &self,
        input: Option<&str>,
        output: Option<&str>,
    ) -> Result<
        (
            astar_audio::CallAudio,
            astar_codec::vocoder_trust::VocoderTrust,
        ),
        StationError,
    > {
        let mut s = self.session.lock().unwrap();
        let audio = s
            .open_voice_route(input, output, || (self.make_backend)())
            .map_err(map_console_err)?;
        Ok((audio, s.vocoder_trust().clone()))
    }

    /// Pin the session's station codec policy from [`StationConfig`] at
    /// construction (iax-4348), BEFORE any path can build the engine.
    ///
    /// The pipeline sample rate is fixed when the `Manager` is built and
    /// every dial's codec policy is capped to it, so a `prefer_slin16`
    /// station — which the macOS app and `astar-server` both configure — only
    /// actually offers slin16 if the policy is known first. It used to be
    /// learned from the first IAX2 `connect` or `start_inbound`; a digital
    /// session (M17/D-Star/System Fusion) reaching the engine before either
    /// of those pinned the whole station to 8 kHz for the rest of the
    /// process, and the node dialed narrowband with nothing in the log.
    ///
    /// Idempotent and safe on a shared session: an engine that is already
    /// busy keeps its rate (the session logs the mismatch).
    fn pin_station_policy(&self) {
        if let Ok(mut s) = self.session.lock() {
            s.set_station_policy(self.config.codec_policy);
        }
    }

    /// Default an inbound policy's codec policy to the station's own
    /// (iax-4348). `IncomingCallPolicy::codec_policy` has no "unset" state, so
    /// "the caller did not choose" is read as the library default
    /// (`UlawOnly`); anything else the caller set is left alone, which keeps
    /// asymmetric inbound/outbound policy available.
    ///
    /// Without this a `prefer_slin16` station drops to 8 kHz the moment the
    /// listener starts — `start_inbound` pins `station_policy` from the
    /// inbound policy — and then STAYS there, because a listening engine is
    /// never idle enough to rebuild. Worse, on an engine already up at 16 kHz
    /// the 8 kHz leg the listener builds is refused outright at adopt
    /// ("listener/station sample-rate mismatch").
    fn inbound_policy(&self, mut policy: IncomingCallPolicy) -> IncomingCallPolicy {
        if policy.codec_policy == CodecPolicy::default() {
            policy.codec_policy = self.config.codec_policy;
        }
        policy
    }

    /// Select the capture/playback devices applied to the next [`Station::connect`]
    /// / [`Station::connect_wt`]. `None` (or an empty string, filtered by the
    /// caller) means the system default. Typical flow: [`Station::list_devices`]
    /// → user picks → `set_devices` → `connect`.
    pub fn set_devices(&self, input: Option<String>, output: Option<String>) {
        *self.devices.lock().unwrap() = (input, output);
    }

    /// The currently selected `(input, output)` device substrings (what the next
    /// connect will use). `None` = system default.
    #[must_use]
    pub fn selected_devices(&self) -> (Option<String>, Option<String>) {
        self.devices.lock().unwrap().clone()
    }

    /// Enumerate `(inputs, outputs)` device-name lists.
    ///
    /// # Errors
    /// [`StationError::Audio`] if enumeration fails.
    pub fn list_devices(&self) -> Result<(Vec<String>, Vec<String>), StationError> {
        let backend = (self.make_backend)();
        astar_console::list_devices(backend.as_ref())
            .map_err(|e| StationError::Audio(e.to_string()))
    }

    /// Manual/non-WT connect (the vendor-neutral, generic IAX2 path): explicit
    /// dest, calling node, secret, and name. Works against stock Asterisk and
    /// the local parrot. `secret` is a **call-time argument** — consumed into
    /// the session, never stored on the station or logged.
    ///
    /// # Errors
    /// [`StationError::Resolve`] if the node can't be resolved;
    /// [`StationError::AlreadyConnected`] / [`StationError::Iax`] /
    /// [`StationError::Audio`] otherwise.
    pub fn connect(
        &self,
        dest: &str,
        calling: &str,
        secret: &str,
        name: &str,
    ) -> Result<(), StationError> {
        let peer: SocketAddr = astar_asl3::resolve_node(dest).map_err(StationError::Resolve)?;
        self.connect_peer(peer, dest, calling, secret, name)
    }

    /// The post-resolution dial core shared by [`Station::connect`] (registrar-
    /// resolved) and [`Station::connect_wt_at`] (explicit address): build the
    /// `ConsoleConfig`, open the backend, and dial the already-resolved `peer`.
    /// Address resolution is the caller's responsibility.
    ///
    /// # Errors
    /// [`StationError::AlreadyConnected`] / [`StationError::Iax`] /
    /// [`StationError::Audio`].
    fn connect_peer(
        &self,
        peer: SocketAddr,
        dest: &str,
        calling: &str,
        secret: &str,
        name: &str,
    ) -> Result<(), StationError> {
        let (input_device, output_device) = self.selected_devices();
        let cfg = ConsoleConfig {
            node: dest.to_string(),
            calling_node: calling.to_string(),
            secret: secret.to_string(),
            name: name.to_string(),
            input_device,
            output_device,
            codec_policy: self.config.codec_policy,
        };
        let backend = (self.make_backend)();
        let mut sess = self.session.lock().unwrap();
        sess.connect(backend, peer, cfg).map_err(map_console_err)
    }

    /// Web-Transceiver connect: mint a token (asl3, from `config.portal`),
    /// resolve the node, then dial with `CALLING_NAME=token`,
    /// `CALLING_NUMBER=dest`. This is the `AllStar` convenience path; it reuses
    /// [`Station::connect`], which carries the same `CallMode::WebTransceiver`
    /// dial recipe the console already implements ([`Station::connect`] remains
    /// the generic, vendor-neutral path).
    ///
    /// # Errors
    /// [`StationError::Portal`] if no portal config or token mint fails;
    /// [`StationError::Resolve`] / [`StationError::Iax`] otherwise.
    pub fn connect_wt(&self, dest_node: &str) -> Result<(), StationError> {
        let token = self.mint_wt_token()?;
        let secret = self.config.secret.clone();
        // WT mode: name = minted token, calling_node = the dest selector.
        self.connect(dest_node, dest_node, &secret, &token)
    }

    /// Web-Transceiver connect to an EXPLICIT address: mint the WT token exactly
    /// as [`Station::connect_wt`] does (from `config.portal`), but dial the
    /// caller-supplied `addr` instead of the registrar-resolved node address.
    /// `addr` is `host:port` or a bare `host` (defaulting to 4569); `host` may be
    /// an IP literal or a DNS name. The auth model is unchanged — only the dial
    /// target differs. This is astar's "Advanced options" manual-address path
    /// (localhost / a LAN address the registrar's public IP can't reach behind
    /// NAT — the hairpin case).
    ///
    /// `addr` is parsed FIRST, so a bad address fails fast (no token mint).
    ///
    /// # Errors
    /// [`StationError::Resolve`] if `addr` is empty/unparseable/unreachable;
    /// [`StationError::Portal`] if no portal config or the token mint fails;
    /// [`StationError::Iax`] / [`StationError::Audio`] otherwise.
    pub fn connect_wt_at(&self, dest_node: &str, addr: &str) -> Result<(), StationError> {
        let peer: SocketAddr = astar_asl3::resolve_addr(addr).map_err(StationError::Resolve)?;
        let token = self.mint_wt_token()?;
        let secret = self.config.secret.clone();
        // WT mode: name = minted token, calling_node = the dest selector.
        self.connect_peer(peer, dest_node, dest_node, &secret, &token)
    }

    // -----------------------------------------------------------------------
    // Link surface (iax-1075): node-to-node links over standard IAX2, thin
    // passthroughs to the session/Manager link layer. Vendor-neutral and
    // secret-free (the dial secret is consumed at connect, never stored).
    // -----------------------------------------------------------------------

    /// Connect a node link: resolve `node` via the `AllStar` registrar (same
    /// resolution as [`Station::connect`]) and dial it in `mode`. Returns the
    /// link's opaque call id (as reported in the roster).
    ///
    /// # Errors
    /// [`StationError::Resolve`] / [`StationError::Link`] /
    /// [`StationError::Audio`].
    pub fn link_connect(
        &self,
        node: &str,
        mode: LinkMode,
        caller_id: &str,
        secret: &str,
        shape: CallMode,
        permanent: bool,
    ) -> Result<u64, StationError> {
        let peer: SocketAddr = astar_asl3::resolve_node(node).map_err(StationError::Resolve)?;
        self.link_connect_peer(node, peer, mode, caller_id, secret, shape, permanent)
    }

    /// [`Station::link_connect`] with an explicit `host:port` address instead
    /// of registrar resolution (private nodes / test rigs).
    ///
    /// # Errors
    /// [`StationError::Link`] on an unparseable address or link failure.
    #[allow(clippy::too_many_arguments)]
    pub fn link_connect_at(
        &self,
        node: &str,
        addr: &str,
        mode: LinkMode,
        caller_id: &str,
        secret: &str,
        shape: CallMode,
        permanent: bool,
    ) -> Result<u64, StationError> {
        use std::net::ToSocketAddrs;
        let peer = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| StationError::Link(format!("unresolvable address {addr}")))?;
        self.link_connect_peer(node, peer, mode, caller_id, secret, shape, permanent)
    }

    /// The post-resolution link dial core (mirrors [`Station::connect_peer`]).
    #[allow(clippy::too_many_arguments)]
    fn link_connect_peer(
        &self,
        node: &str,
        peer: SocketAddr,
        mode: LinkMode,
        caller_id: &str,
        secret: &str,
        shape: CallMode,
        permanent: bool,
    ) -> Result<u64, StationError> {
        let backend = (self.make_backend)();
        self.session
            .lock()
            .expect("station session poisoned")
            .link_connect(
                LinkConnectSpec {
                    node: node.to_string(),
                    peer,
                    mode,
                    caller_id: caller_id.to_string(),
                    secret: secret.to_string(),
                    shape,
                    permanent,
                },
                backend,
            )
            .map_err(|e| StationError::Link(e.to_string()))
    }

    /// Tear a link down by node label.
    ///
    /// # Errors
    /// [`StationError::Link`] if no link is registered for `node`.
    pub fn link_disconnect(&self, node: &str) -> Result<(), StationError> {
        self.session
            .lock()
            .expect("station session poisoned")
            .link_disconnect(node)
            .map_err(|e| StationError::Link(e.to_string()))
    }

    /// Change a link's mode by node label (Transceive gets the default mic
    /// routed so it is immediately key-able).
    ///
    /// # Errors
    /// [`StationError::Link`] if no link is registered for `node`.
    pub fn link_set_mode(&self, node: &str, mode: LinkMode) -> Result<(), StationError> {
        self.session
            .lock()
            .expect("station session poisoned")
            .link_set_mode(node, mode)
            .map_err(|e| StationError::Link(e.to_string()))
    }

    /// Key / unkey a link by node label (refused for non-transmit modes).
    ///
    /// # Errors
    /// [`StationError::Link`] if no link is registered for `node`, or the link
    /// is not transmit-capable.
    pub fn link_key(&self, node: &str, on: bool) -> Result<(), StationError> {
        self.session
            .lock()
            .expect("station session poisoned")
            .link_key(node, on)
            .map_err(|e| StationError::Link(e.to_string()))
    }

    /// Snapshot of all live links — secret-free, empty before any link.
    #[must_use]
    pub fn link_roster(&self) -> LinkRoster {
        self.session
            .lock()
            .expect("station session poisoned")
            .link_roster()
            .unwrap_or(LinkRoster { links: Vec::new() })
    }

    /// Who has keyed up on the live digital link, newest first (empty on
    /// `AllStar` and while idle). See `astar_console::HeardLog`.
    #[must_use]
    pub fn heard(&self) -> Vec<HeardEntry> {
        self.session
            .lock()
            .expect("station session poisoned")
            .heard()
    }

    /// Announce to every non-link conference member (iax-9e02) — the
    /// web-transceiver users on this node, never the linked nodes. Returns how
    /// many members were reached.
    pub fn announce_to_non_link_members(&self, req: astar_iax::AnnounceRequest) -> usize {
        self.session
            .lock()
            .expect("station session poisoned")
            .announce_to_non_link_members(req)
    }

    /// Propagate member keying onto transceive links (iax-7d51). Idempotent;
    /// the node pump calls it every tick. Returns the number of links whose
    /// keyed state changed.
    pub fn sync_link_keying(&self) -> usize {
        self.session
            .lock()
            .expect("station session poisoned")
            .sync_link_keying()
    }

    /// Drain DTMF command digits from the engine (iax-d254): in-band
    /// conference-member tones + out-of-band protocol DTMF, as
    /// `(raw call id, digit)`. Poll-style; draining consumes. Feeds the
    /// node daemon's `*`-command mapper.
    #[must_use]
    pub fn drain_dtmf_digits(&self) -> Vec<(u64, char)> {
        self.session
            .lock()
            .expect("station session poisoned")
            .drain_dtmf_digits()
    }

    /// Drain the next pending link lifecycle event, if any (poll style).
    /// Events drained from the session in a burst are buffered so none are
    /// lost between polls.
    #[must_use]
    pub fn next_link_event(&self) -> Option<LinkEvent> {
        let mut buf = self
            .link_events_buf
            .lock()
            .expect("link event buffer poisoned");
        if buf.is_empty() {
            buf.extend(
                self.session
                    .lock()
                    .expect("station session poisoned")
                    .drain_link_events(),
            );
        }
        buf.pop_front()
    }

    /// Portal login + WT-token mint, **without** dialing — the mint-only core
    /// shared by [`Station::connect_wt`] and [`Station::test_mint_token`].
    ///
    /// Uses the station's already-held [`PortalCredentials`] (consumed from
    /// `IaxConfig` at init); it takes **no** secret argument. The minted token
    /// is returned to the caller; it is never stored on the station or logged.
    ///
    /// # Errors
    /// [`StationError::Portal`] if no portal config is held or the mint fails
    /// (bad password / unknown node / network — categorized by the wrapped
    /// [`astar_asl3::Asl3Error`]).
    /// Public since iax-b7f2: the node daemon mints a fresh token per
    /// `wt-guest` link dial (the ASL3 `[allstar-public]` dialplan validates
    /// `CALLING_NAME` server-side and clears tokenless calls ~1 s after
    /// answer). NOTE: performs blocking network I/O (HTTPS to the portal).
    pub fn mint_wt_token(&self) -> Result<String, StationError> {
        let creds = self
            .config
            .portal
            .as_ref()
            .ok_or(StationError::Portal(astar_asl3::Asl3Error::TokenNotFound))?;
        match self.config.portal_base.as_deref() {
            Some(base) => astar_asl3::mint_wt_token_at(base, creds),
            None => astar_asl3::mint_wt_token(creds),
        }
        .map_err(StationError::Portal)
    }

    /// Validate the station's portal/WebTransceiver credentials WITHOUT placing
    /// a call: run the portal login + WT-token mint from the already-held
    /// [`PortalCredentials`], then **discard** the token (it is never returned,
    /// stored, or logged). This is the front-end "Test credentials" path
    /// (astar-2fde): it opens **no** IAX call and **no** UDP call socket.
    ///
    /// Takes no secret argument — it reuses the portal credentials the station
    /// already holds, exactly as [`Station::connect_wt`] does.
    ///
    /// # Errors
    /// [`StationError::Portal`] when there is no portal config or the mint fails.
    /// The failure category (bad password / unknown node / network) is carried
    /// in the wrapped [`astar_asl3::Asl3Error`] for programmatic matching;
    /// the credentials never appear in the error.
    ///
    /// NOTE: performs blocking network I/O (HTTPS to the portal); call it off
    /// any UI thread.
    pub fn test_mint_token(&self) -> Result<(), StationError> {
        // Mint and drop: the token is bound only long enough to confirm the
        // mint succeeded, then dropped at the end of this statement. No dial,
        // no call socket.
        let _ = self.mint_wt_token()?;
        Ok(())
    }

    /// Tear down any active call. Idempotent.
    ///
    /// Mirrors the harness: detach under the lock, then hang up off-lock
    /// (`Call::hangup` joins the runtime thread / drops cpal streams, which can
    /// briefly block — never under the session lock).
    pub fn disconnect(&self) {
        // Finish an app recording before the call it records goes away. A
        // no-op, taps untouched, when none is running.
        let _ = self.record_stop();
        // Teardown cancels any in-flight DTMF sequence (iax-4b7a) — the
        // remainder must never play into a later call.
        self.cancel_dtmf();
        // Always clear any live/failed M17 session too (iax-f2b8-fix Fix 1).
        // `detach()` below only touches `self.active` (the IAX2/WT call slot)
        // and early-returns doing nothing when it's `None` — it never reaches
        // `self.m17`. Both astar and gui-rs call `disconnect()` as their
        // universal pre-dial teardown, so without this, a failed M17 link
        // left `m17_is_active()` (and thus the AlreadyConnected mutual
        // exclusion in `m17_connect`/`connect`) wedged until process restart.
        // No-op when no M17 session is up. Mirrors `Drop`, which relied on
        // this same call running separately — now redundant there but still
        // harmless (idempotent).
        self.m17_disconnect();
        // iax-a9d4 Task 6: same lesson, same fix, for a live/failed D-Star
        // session — `detach()` below never touches `self.dstar` either.
        self.dstar_disconnect();
        self.ysf_disconnect();
        self.nxdn_disconnect();
        self.dmr_disconnect();
        // The EchoLink call, if any; the directory login stays up.
        self.echolink_disconnect();
        if self.mode() == OperatingMode::Node {
            // Node mode: hang up the active inbound-adopted call but keep the
            // listener running for the next caller (the session retains its
            // Manager + inbound listener; only the call is torn down).
            self.session.lock().unwrap().disconnect_inbound();
            return;
        }
        let detached = self.session.lock().unwrap().detach();
        if let Some((call, manager)) = detached {
            // Off-lock: join the call thread, then drop the Manager (its router
            // closes the cpal streams). Both can briefly block on CoreAudio.
            let _ = call.hangup(None);
            drop(manager);
        }
    }

    // ── EchoLink ────────────────────────────────────────────────────────

    /// Use `net` for every later `EchoLink` login and call: where the link
    /// binds, how it finds a station's ports, and the protocol timings.
    /// Production never needs this (the session starts on
    /// [`astar_console::echolink::EchoLinkNetwork::production`]); tests and
    /// embedders point it at loopback fakes.
    #[cfg(feature = "echolink")]
    pub fn set_echolink_network(&self, net: astar_console::echolink::EchoLinkNetwork) {
        self.session.lock().unwrap().set_echolink_network(net);
    }

    /// Start logging in to the `EchoLink` directory as `callsign`, trying
    /// `servers` in turn (`IP`, `IP:port`, `name` or `name:port`). Returns at
    /// once; the login runs on its own thread, and
    /// [`Station::echolink_state`] shows how it went. The location last given
    /// to [`Station::echolink_set_location`] goes out beside the callsign.
    ///
    /// `password` is taken by value and wrapped in `Zeroizing` before
    /// anything else happens, so it is zeroed when it is dropped, whichever
    /// way this returns. It moves into the directory thread and is never
    /// stored on the station, printed, logged or put in an error.
    ///
    /// A login already running is replaced; the new login's thread logs the
    /// old one off first.
    ///
    /// # Errors
    /// [`StationError::EchoLink`] for an empty callsign, an empty password,
    /// or no non-empty server, all checked before anything starts (so a
    /// running login stands), and when the `echolink` feature isn't compiled
    /// in.
    pub fn echolink_login(
        &self,
        callsign: &str,
        password: String,
        servers: &[String],
    ) -> Result<(), StationError> {
        let password = zeroize::Zeroizing::new(password);
        let callsign = callsign.trim();
        if callsign.is_empty() {
            return Err(StationError::EchoLink(
                "Enter a callsign to log in to EchoLink".into(),
            ));
        }
        if password.is_empty() {
            return Err(StationError::EchoLink(
                "Enter a password to log in to EchoLink".into(),
            ));
        }
        if !echolink_password_is_sendable(&password) {
            return Err(StationError::EchoLink(ECHOLINK_PASSWORD_UNSENDABLE.into()));
        }
        let servers: Vec<String> = servers
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if servers.is_empty() {
            return Err(StationError::EchoLink(
                "Add an EchoLink directory server to log in".into(),
            ));
        }
        #[cfg(feature = "echolink")]
        {
            let login = astar_console::echolink::EchoLinkLogin {
                callsign: callsign.to_owned(),
                servers,
                // Empty: the session uses the location last set.
                location: String::new(),
            };
            self.session
                .lock()
                .unwrap()
                .echolink_login(login, password)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            drop((password, servers));
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Hang up any `EchoLink` call and every station on the node's leg, then
    /// log off the directory. No-op when not logged in (and without the
    /// `echolink` feature).
    ///
    /// The calls are hung up under the session lock, so the far stations
    /// hear a BYE before this station leaves the list. The logoff itself
    /// waits on any directory exchange already in flight, about 80 s at
    /// worst, so the login is taken out under the lock and logged off only
    /// after every lock is released: this call can block that long, but
    /// nothing else on the station waits for it.
    pub fn echolink_logout(&self) {
        #[cfg(feature = "echolink")]
        {
            let presence = self.session.lock().unwrap().echolink_logout();
            if let Some(presence) = presence {
                presence.logout();
            }
        }
    }

    /// Call an `EchoLink` station, named by node number or callsign as the
    /// directory lists it, over the station's selected devices. Returns as
    /// soon as the first SDES is out; the answer shows in
    /// [`Station::snapshot`] and [`Station::echolink_state`].
    ///
    /// # Errors
    /// [`StationError::EchoLink`] for an empty target, when not logged in,
    /// when no listed station matches, when the UDP ports cannot be opened,
    /// and when the `echolink` feature isn't compiled in;
    /// [`StationError::AlreadyConnected`] while any other network, another
    /// `EchoLink` call or the node's `EchoLink` leg holds the station;
    /// [`StationError::Audio`] when the output device cannot be opened.
    pub fn echolink_connect(&self, node_or_call: &str) -> Result<(), StationError> {
        let target = node_or_call.trim();
        if target.is_empty() {
            return Err(StationError::EchoLink(
                "Enter a node number or callsign to call".into(),
            ));
        }
        #[cfg(feature = "echolink")]
        {
            let (input, output) = self.selected_devices();
            let backend = (self.make_backend)();
            self.session
                .lock()
                .unwrap()
                .echolink_connect(backend, target, input.as_deref(), output.as_deref())
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Hang up the `EchoLink` call, if any. The directory login stays up.
    /// No-op with no call (and without the `echolink` feature).
    pub fn echolink_disconnect(&self) {
        #[cfg(feature = "echolink")]
        {
            self.session.lock().unwrap().echolink_disconnect();
        }
    }

    /// The directory's latest station list. Empty when not logged in, before
    /// the first list arrives, and without the `echolink` feature. Carries no
    /// IP address (see [`EchoLinkStationInfo`]).
    #[must_use]
    pub fn echolink_stations(&self) -> Vec<EchoLinkStationInfo> {
        #[cfg(feature = "echolink")]
        {
            let stations = self.session.lock().unwrap().echolink_stations();
            stations.iter().map(EchoLinkStationInfo::from).collect()
        }
        #[cfg(not(feature = "echolink"))]
        {
            Vec::new()
        }
    }

    /// Ask the `EchoLink` directory for a fresh station list now, rather than
    /// at the next 600 s refresh. Returns `Ok(true)` when it was asked, and
    /// `Ok(false)`, asking nothing, when the last list came less than 30 s
    /// ago: the caller says "try again in a moment". The new list shows in
    /// [`Station::echolink_stations`] once it arrives.
    ///
    /// # Errors
    /// [`StationError::EchoLink`] when not logged in, and when the
    /// `echolink` feature isn't compiled in.
    pub fn echolink_refresh_list(&self) -> Result<bool, StationError> {
        #[cfg(feature = "echolink")]
        {
            self.session
                .lock()
                .unwrap()
                .echolink_refresh_list()
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Send a line of text chat to the station on the `EchoLink` call.
    ///
    /// # Errors
    /// [`StationError::NotConnected`] with no call in hand, checked first;
    /// [`StationError::EchoLink`] for an empty line, and when the
    /// `echolink` feature isn't compiled in.
    pub fn echolink_send_chat(&self, text: &str) -> Result<(), StationError> {
        #[cfg(feature = "echolink")]
        {
            let session = self.session.lock().unwrap();
            // `echolink_state` carries a call exactly while one is in hand.
            if session.echolink_state().and_then(|s| s.call).is_none() {
                return Err(StationError::NotConnected);
            }
            if text.trim().is_empty() {
                return Err(StationError::EchoLink("Type something to send".into()));
            }
            session.echolink_send_chat(text).map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            let _ = text;
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Where the `EchoLink` login and call stand. `Some` while a login is
    /// running, logged in or failed (a failed login carries its sentence in
    /// `login.error`), or while a call is in hand; `None` once logged out,
    /// and always without the `echolink` feature. Holds no password.
    #[must_use]
    pub fn echolink_state(&self) -> Option<EchoLinkState> {
        #[cfg(feature = "echolink")]
        {
            self.session.lock().unwrap().echolink_state()
        }
        #[cfg(not(feature = "echolink"))]
        {
            None
        }
    }

    /// Set the location text other stations see beside the callsign. It may
    /// come before a login (which then carries it); while logged in it goes
    /// out with the next status refresh. No-op without the `echolink`
    /// feature.
    pub fn echolink_set_location(&self, text: &str) {
        #[cfg(feature = "echolink")]
        {
            self.session.lock().unwrap().echolink_set_location(text);
        }
        #[cfg(not(feature = "echolink"))]
        {
            let _ = text;
        }
    }

    /// The sentence saying why the `EchoLink` login failed, for a person to
    /// read (for example `"EchoLink said the password is wrong"`). `None` when
    /// the login has not failed, when logged out, and without the
    /// `echolink` feature.
    #[must_use]
    pub fn echolink_last_error(&self) -> Option<String> {
        #[cfg(feature = "echolink")]
        {
            self.echolink_state().and_then(|s| s.login.error)
        }
        #[cfg(not(feature = "echolink"))]
        {
            None
        }
    }

    /// `true` when this crate was built with its own `echolink` feature.
    ///
    /// Deliberately NOT [`astar_console::echolink_available`]: Cargo unifies
    /// features, so astar-console can carry `EchoLink` (pulled in by another
    /// crate in the build) while this crate's methods are the "not compiled"
    /// stubs. What a caller can use is what this crate compiled.
    #[must_use]
    pub fn echolink_available(&self) -> bool {
        cfg!(feature = "echolink")
    }

    /// Dial an `EchoLink` station, named by node number or callsign, into the
    /// node's conference as a peer member. Returns as soon as the first SDES
    /// is out; the answer shows in [`Station::echolink_node_peers`] and as an
    /// `EchoLinkNodeEdge::Connected` from [`Station::echolink_node_poll`].
    ///
    /// # Errors
    /// [`StationError::EchoLink`] for an empty target, when not logged in,
    /// when no listed station matches, when the node has no conference
    /// (`"no conference: …"`), when another station on the leg shares the
    /// IP, when the UDP ports cannot be opened, and when the `echolink`
    /// feature isn't compiled in; [`StationError::AlreadyConnected`] while the
    /// app's own `EchoLink` call is in hand or the station is already on the
    /// leg.
    pub fn echolink_node_connect(&self, target: &str) -> Result<(), StationError> {
        let target = target.trim();
        if target.is_empty() {
            return Err(StationError::EchoLink(
                "Enter a node number or callsign to call".into(),
            ));
        }
        #[cfg(feature = "echolink")]
        {
            // Only waits for the leg's thread to take the station, at most
            // one 50 ms link pass: short enough for the session lock.
            self.session
                .lock()
                .unwrap()
                .echolink_node_connect(target)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Hang up on one station on the node's `EchoLink` leg, by callsign.
    ///
    /// # Errors
    /// [`StationError::NotConnected`] when no such station is on the leg;
    /// [`StationError::EchoLink`] for an empty callsign, and when the
    /// `echolink` feature isn't compiled in.
    pub fn echolink_node_disconnect(&self, call: &str) -> Result<(), StationError> {
        let call = call.trim();
        if call.is_empty() {
            return Err(StationError::EchoLink(
                "Enter the callsign of the station to hang up on".into(),
            ));
        }
        #[cfg(feature = "echolink")]
        {
            self.session
                .lock()
                .unwrap()
                .echolink_node_disconnect(call)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "echolink"))]
        {
            let _ = call;
            Err(StationError::EchoLink(ECHOLINK_NOT_COMPILED.into()))
        }
    }

    /// Drain what has happened on the node's `EchoLink` leg since the last
    /// poll: stations answering, and contacts ending on their own. Empty with
    /// no leg, and without the `echolink` feature.
    #[must_use]
    pub fn echolink_node_poll(&self) -> Vec<EchoLinkNodeEdge> {
        #[cfg(feature = "echolink")]
        {
            self.session.lock().unwrap().echolink_node_poll()
        }
        #[cfg(not(feature = "echolink"))]
        {
            Vec::new()
        }
    }

    /// Every station on the node's `EchoLink` leg and where its contact
    /// stands. Empty with no leg, and without the `echolink` feature.
    #[must_use]
    pub fn echolink_node_peers(&self) -> Vec<EchoLinkPeerStatus> {
        #[cfg(feature = "echolink")]
        {
            self.session.lock().unwrap().echolink_node_peers()
        }
        #[cfg(not(feature = "echolink"))]
        {
            Vec::new()
        }
    }

    /// Start recording the live connection: heard audio, sent audio and,
    /// with `req.frames`, the codec frames, into `req.dir`.
    ///
    /// The taps go onto the console and their receivers to a new
    /// [`Recorder`], which drains them on its own 20 ms clock. The rate is
    /// the pipeline's (16 kHz on a `prefer_slin16` station).
    ///
    /// No lock is held across the recorder's file I/O or a join: the slot is
    /// marked as starting while the recorder is made, so a concurrent start
    /// refuses and [`Station::record_state`] keeps reporting the last state.
    /// A [`Station::record_stop`] (or [`Station::disconnect`]) landing in that
    /// window is honoured: the new recorder is stopped before any tap is
    /// installed, its final state becomes the last one, and this returns
    /// `Ok`.
    ///
    /// # Errors
    /// [`StationError::Record`] `("already recording")` while one runs, is
    /// starting or is still finishing, `("frame tap in use")` while the
    /// server's frame tap is set (the two share one slot), and the recorder's
    /// own message when the folder cannot be used or the disk is short of
    /// space; [`StationError::NotConnected`] with no call or digital session
    /// live, including one that ended while the recorder was being made.
    pub fn record_start(&self, req: RecordRequest) -> Result<(), StationError> {
        let mut guard = BusyGuard::new(&self.recording);
        guard.disarm(); // armed below, once `starting` is set
        let (sample_rate, contended, finished) = {
            let mut slot = self.recording.lock().unwrap();
            if slot.busy() || slot.recorder.as_ref().is_some_and(|r| r.state().recording) {
                return Err(StationError::Record(
                    RecordError::AlreadyRecording.to_string(),
                ));
            }
            if slot.server_frames.is_some() {
                return Err(StationError::Record("frame tap in use".to_owned()));
            }
            let mut s = self.session.lock().unwrap();
            // A recorder that stopped by itself (low disk, folder gone) is
            // collected: its taps come off first, so a restart that fails
            // below leaves no dead taps behind.
            let finished = slot.recorder.take();
            if finished.is_some() {
                s.set_record_taps(RecordTaps {
                    frames: slot.server_frames.clone(),
                    ..RecordTaps::default()
                });
                slot.app_frames = None;
            }
            if !s.has_connection() {
                drop(s);
                drop(slot);
                self.finish_collected(finished);
                return Err(StationError::NotConnected);
            }
            slot.starting = true;
            slot.stop_requested = false;
            guard.armed = true;
            #[cfg(test)]
            let pause = slot.start_pause.clone();
            drop(s);
            drop(slot);
            #[cfg(test)]
            if let Some((a, b)) = pause {
                a.wait();
                b.wait();
            }
            let s = self.session.lock().unwrap();
            (
                s.pipeline_sample_rate(),
                s.record_contended_counters(),
                finished,
            )
        };
        // Its thread has already left its loop, so the join is immediate;
        // still, no lock is held.
        self.finish_collected(finished);

        let (started, taps) = Self::make_recorder(req, sample_rate, contended);

        let mut slot = self.recording.lock().unwrap();
        slot.starting = false;
        let stop_requested = std::mem::take(&mut slot.stop_requested);
        guard.disarm();
        let recorder = started.map_err(|e| StationError::Record(e.to_string()))?;
        let mut s = self.session.lock().unwrap();
        let connected = s.has_connection();
        if stop_requested || !connected {
            // Stopped (or the connection went away) while the recorder was
            // being made: no app tap goes on, and the recorder is finished
            // outside the locks like any other stop.
            s.set_record_taps(RecordTaps {
                frames: slot.server_frames.clone(),
                ..RecordTaps::default()
            });
            drop(s);
            self.abandon_new_recorder(slot, recorder);
            return if stop_requested {
                Ok(())
            } else {
                Err(StationError::NotConnected)
            };
        }
        // A server tap set meanwhile keeps the frame slot (see
        // `set_frame_tap`).
        let app_frames = taps.frames.clone();
        s.set_record_taps(RecordTaps {
            frames: slot.server_frames.clone().or_else(|| taps.frames.clone()),
            ..taps
        });
        drop(s);
        slot.app_frames = app_frames;
        slot.recorder = Some(recorder);
        Ok(())
    }

    /// Finish a recorder [`Station::record_start`] made but will not install:
    /// mark the slot as stopping, join outside the lock, and keep the final
    /// state (`recording: false`) as the last one.
    fn abandon_new_recorder(
        &self,
        mut slot: std::sync::MutexGuard<'_, RecordSlot>,
        recorder: Recorder,
    ) {
        slot.app_frames = None;
        slot.stopping = true;
        slot.last = Some(RecordState {
            recording: false,
            ..recorder.state()
        });
        let mut guard = BusyGuard::new(&self.recording);
        drop(slot);
        let state = recorder.stop();
        let mut slot = self.recording.lock().unwrap();
        slot.stopping = false;
        guard.disarm();
        slot.last = Some(state);
    }

    /// Make the three taps and a recorder reading them, with no lock held.
    /// Returns the senders for the console beside the recorder.
    fn make_recorder(
        req: RecordRequest,
        sample_rate: u32,
        contended: Vec<Arc<std::sync::atomic::AtomicU64>>,
    ) -> (Result<Recorder, RecordError>, RecordTaps) {
        let (heard_tap, heard) = tap_channel(HEARD_TAP_CAPACITY);
        let (sent_tap, sent) = tap_channel(SENT_TAP_CAPACITY);
        let (frames_tap, frames) = if req.frames {
            let (t, r) = tap_channel(FRAME_TAP_CAPACITY);
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        let mut drop_counters = vec![heard_tap.dropped_counter(), sent_tap.dropped_counter()];
        drop_counters.extend(frames_tap.as_ref().map(Tap::dropped_counter));
        drop_counters.extend(contended);
        let started = Recorder::start(
            RecorderConfig {
                dir: req.dir,
                layout: req.layout,
                frames: req.frames,
                label: req.label,
                utc_offset_secs: req.utc_offset_secs,
                sample_rate,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            RecorderInputs {
                heard,
                sent,
                frames,
                drop_counters,
            },
        );
        let taps = RecordTaps {
            heard: Some(heard_tap),
            sent: Some(sent_tap),
            frames: frames_tap,
        };
        (started, taps)
    }

    /// Join a recorder collected by [`Station::record_start`] (no lock held)
    /// and keep its final state as the last one.
    fn finish_collected(&self, finished: Option<Recorder>) {
        if let Some(old) = finished {
            let state = old.stop();
            self.recording.lock().unwrap().last = Some(state);
        }
    }

    /// Stop the app recording, finish its file(s) and return the final
    /// state. Idempotent: with none running it returns the last state and
    /// leaves the taps alone, so the server's frame tap is never touched.
    ///
    /// The taps come off first, so nothing more arrives; the recorder is
    /// joined after both locks are released (a join can take one tick).
    /// During that join a concurrent stop returns the last state, already
    /// `recording: false`, and a concurrent start is refused rather than
    /// overlapping the finishing recorder. A stop while a start is still
    /// making its recorder marks it stopped: that start finishes the
    /// recorder instead of installing it (see [`Station::record_start`]).
    pub fn record_stop(&self) -> RecordState {
        let mut guard = BusyGuard::new(&self.recording);
        guard.disarm();
        let recorder = {
            let mut slot = self.recording.lock().unwrap();
            if slot.starting {
                slot.stop_requested = true;
            }
            let Some(r) = slot.recorder.take() else {
                return slot.last.clone().unwrap_or_else(RecordState::idle);
            };
            // Clear only what `record_start` installed: the server's frame
            // tap, if it set one since, stays.
            self.session.lock().unwrap().set_record_taps(RecordTaps {
                frames: slot.server_frames.clone(),
                ..RecordTaps::default()
            });
            slot.app_frames = None;
            slot.stopping = true;
            guard.armed = true;
            slot.last = Some(RecordState {
                recording: false,
                ..r.state()
            });
            r
        };
        let state = recorder.stop();
        let mut slot = self.recording.lock().unwrap();
        slot.stopping = false;
        guard.disarm();
        slot.last = Some(state.clone());
        state
    }

    /// The live recording state; after a stop (and while one is finishing),
    /// the last recording's state with `recording: false` and its `error`,
    /// until the next start; [`RecordState::idle`] before any.
    #[must_use]
    pub fn record_state(&self) -> RecordState {
        let slot = self.recording.lock().unwrap();
        match slot.recorder.as_ref() {
            Some(r) => r.state(),
            None => slot.last.clone().unwrap_or_else(RecordState::idle),
        }
    }

    /// Install (or, with `None`, remove) the server's codec-frame tap. It
    /// changes only the console's frame slot, never the heard/sent taps, and
    /// survives [`Station::record_stop`] and [`Station::disconnect`]. While
    /// it is set, [`Station::record_start`] refuses.
    ///
    /// The app recorder and the server share that one slot. Setting a server
    /// tap during an app recording takes the slot: the app's frame capture
    /// gets nothing (its audio continues) until the server tap is removed,
    /// which hands the slot back to the app recording.
    pub fn set_frame_tap(&self, tap: Option<Tap<FrameEvent>>) {
        let mut slot = self.recording.lock().unwrap();
        slot.server_frames.clone_from(&tap);
        let effective = tap.or_else(|| slot.app_frames.clone());
        self.session.lock().unwrap().set_frame_tap(effective);
    }

    /// Extra directories to search for a runtime `libcodec2`, ahead of the
    /// hard-coded system paths (iax-f2b8 Task 4). Call before
    /// [`Station::m17_connect`] — e.g. to point at an app bundle's own copy
    /// of the library. Default empty (the system search paths only).
    pub fn set_codec_dirs(&self, dirs: Vec<std::path::PathBuf>) {
        *self.codec_dirs.lock().unwrap() = dirs;
    }

    /// Connect to an M17 reflector (iax-f2b8 Task 4): resolves `host`/`port`,
    /// validates `module` (uppercased; must land on `A`-`Z`) and a non-empty
    /// `callsign`, and opens the session over the station's currently
    /// selected devices (mirrors how [`Station::connect_peer`] resolves
    /// them) plus any codec search dirs set via [`Station::set_codec_dirs`].
    ///
    /// Mutually exclusive with an active IAX2 call: the session refuses with
    /// [`StationError::AlreadyConnected`] while one is live; the reverse
    /// exclusion (an IAX2 `connect`/inbound-adopt while M17 is live) is
    /// enforced by the same session.
    ///
    /// # Errors
    /// [`StationError::M17`] for an invalid module/callsign, or when the
    /// `m17` feature isn't compiled in; [`StationError::AlreadyConnected`] /
    /// [`StationError::Audio`] otherwise (mapped from the session's
    /// [`astar_console::ConsoleError`]).
    pub fn m17_connect(
        &self,
        host: &str,
        port: u16,
        module: char,
        callsign: &str,
    ) -> Result<(), StationError> {
        let module = module.to_ascii_uppercase();
        if !module.is_ascii_uppercase() {
            return Err(StationError::M17("module must be A-Z".into()));
        }
        if callsign.is_empty() {
            return Err(StationError::M17("callsign must not be empty".into()));
        }
        #[cfg(feature = "m17")]
        {
            let module_byte = u8::try_from(module).expect("ASCII-validated above");
            let (input, output) = self.selected_devices();
            let codec_dirs = self.codec_dirs.lock().unwrap().clone();
            let cfg = astar_console::M17Config {
                host: host.to_string(),
                port,
                module: module_byte,
                callsign: callsign.to_string(),
                codec_dirs,
                // iax-f2b8 Task 4: no `M17Config::default()` exists yet
                // (ledgered in the Task 3 report) — hard-coded here, matching
                // `SessionFsm`'s own 30 s default.
                keepalive_timeout: std::time::Duration::from_secs(30),
            };
            let backend = (self.make_backend)();
            self.session
                .lock()
                .unwrap()
                .m17_connect(backend, cfg, input.as_deref(), output.as_deref())
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "m17"))]
        {
            let _ = (host, port);
            Err(StationError::M17("m17 support not compiled".into()))
        }
    }

    /// Disconnect the live M17 session, if any (iax-f2b8 Task 4). No-op when
    /// none is active (and when the `m17` feature isn't compiled in).
    pub fn m17_disconnect(&self) {
        #[cfg(feature = "m17")]
        {
            self.session.lock().unwrap().m17_disconnect();
        }
    }

    /// `true` when M17 voice is available: the `m17` feature is compiled in
    /// AND a working Codec 2 backend was found — probed against the CURRENT
    /// [`Station::set_codec_dirs`] value (iax-f2b8 Task 4), NOT the
    /// hard-coded system paths alone. Deliberately does NOT delegate to
    /// [`astar_console::m17_available`] (that free function always probes
    /// `&[]` and caches forever — right for `ConsoleState::m17_available`'s
    /// "is M17 compiled in at all" mirror, wrong here: a bundled-dylib
    /// deployment that calls `set_codec_dirs` would show unavailable
    /// forever). See `codec2_probe_cache`'s field docs for the memoization.
    #[must_use]
    pub fn m17_available(&self) -> bool {
        #[cfg(feature = "m17")]
        {
            let dirs = self.codec_dirs.lock().unwrap().clone();
            {
                let cache = self.codec2_probe_cache.lock().unwrap();
                if let Some((cached_dirs, available)) = cache.as_ref()
                    && *cached_dirs == dirs
                {
                    return *available;
                }
            }
            let available = astar_codec::codec2::codec2_available(&dirs);
            *self.codec2_probe_cache.lock().unwrap() = Some((dirs, available));
            available
        }
        #[cfg(not(feature = "m17"))]
        {
            false
        }
    }

    /// Link a receive-only M17 monitor to `req`'s reflector module. What it
    /// hears feeds the conference as a contributing-only member.
    ///
    /// This is deliberately NOT [`Station::m17_connect`]. That is a
    /// transmit-capable client that takes the voice route, so a pooled IAX2
    /// link refuses it and [`Station::set_ptt`] keys it. A monitor takes
    /// neither. The node's links keep running beside it, and a key request
    /// with only a monitor up returns [`StationError::NotConnected`].
    ///
    /// A monitor needs a conference to join: Bridge or Conference topology,
    /// with the engine built. It uses the codec search dirs from
    /// [`Station::set_codec_dirs`] and a 30 s keepalive. Name resolution,
    /// codec open and CONN all run on the monitor's own thread, so this
    /// returns without waiting on the network.
    ///
    /// # Errors
    /// [`StationError::M17`] in these cases:
    /// - the module is outside `A`-`Z`;
    /// - the callsign is empty or cannot be encoded;
    /// - no engine or no conference exists;
    /// - the `m17` feature is not compiled in (`"m17 support not compiled"`).
    ///
    /// [`StationError::AlreadyConnected`] while a monitor is already live.
    pub fn m17_monitor_connect(&self, req: M17MonitorRequest) -> Result<(), StationError> {
        let module = req.module.to_ascii_uppercase();
        let module_byte = match u8::try_from(module) {
            Ok(byte) if byte.is_ascii_uppercase() => byte,
            _ => return Err(StationError::M17("module must be A-Z".into())),
        };
        if req.callsign.is_empty() {
            return Err(StationError::M17("callsign must not be empty".into()));
        }
        #[cfg(feature = "m17")]
        {
            let codec_dirs = self
                .codec_dirs
                .lock()
                .expect("station codec_dirs poisoned")
                .clone();
            let cfg = astar_console::M17MonitorConfig {
                host: req.host,
                port: req.port,
                module: module_byte,
                callsign: req.callsign,
                codec_dirs,
                keepalive_timeout: M17_MONITOR_KEEPALIVE,
            };
            // `m17_monitor_connect` only validates, enrols and spawns a
            // thread, so it is safe to call under the session lock.
            self.session
                .lock()
                .expect("station session poisoned")
                .m17_monitor_connect(cfg)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "m17"))]
        {
            let _ = (req.host, req.port, module_byte);
            Err(StationError::M17("m17 support not compiled".into()))
        }
    }

    /// Stop the M17 monitor, if one is up: withdraw it from the conference
    /// and send DISC.
    ///
    /// The monitor is taken out under the session lock, and its thread is
    /// joined only after the lock is released. The join can wait out one
    /// 50 ms socket poll. Holding the session lock that long would stall
    /// every snapshot, key request and link command on the node.
    ///
    /// This is a no-op with no monitor up, and without the `m17` feature.
    /// [`Station::disconnect`] never calls it, and never takes the monitor's
    /// conference away: a handset hang-up must not drop a reflector link
    /// someone else dialled. With the inbound listener up (the node's normal
    /// state, where [`Station::mode`] reads Node) `disconnect` hangs up only
    /// the handset call. With no listener and a WT call up it goes through
    /// [`ConsoleSession::detach`], which keeps the engine in the session
    /// while a monitor is up and hands back only the call.
    pub fn m17_monitor_disconnect(&self) {
        #[cfg(feature = "m17")]
        {
            let taken = self
                .session
                .lock()
                .expect("station session poisoned")
                .m17_monitor_take();
            // The guard above was a temporary and is already released.
            if let Some(monitor) = taken {
                monitor.disconnect();
            }
        }
    }

    /// The monitor's next lifecycle edge, if any. A monitor that has ended is
    /// cleared away as a side effect.
    ///
    /// Returns [`M17MonitorEdge::Linked`] once, when the reflector accepts
    /// the link. Returns [`M17MonitorEdge::Ended`] with its reason when the
    /// leg is reaped (refused, gone silent, conference gone). The facade has
    /// no callbacks, so the caller polls this on its own cadence. A reaped
    /// monitor is joined after the session lock is released, for the same
    /// reason as in [`Station::m17_monitor_disconnect`].
    ///
    /// Returns `None` when there is nothing to report, and always without
    /// the `m17` feature.
    #[must_use]
    pub fn m17_monitor_poll(&self) -> Option<M17MonitorEdge> {
        #[cfg(feature = "m17")]
        {
            let (edge, reaped) = self
                .session
                .lock()
                .expect("station session poisoned")
                .m17_monitor_poll();
            if let Some(monitor) = reaped {
                monitor.disconnect();
            }
            edge
        }
        #[cfg(not(feature = "m17"))]
        {
            None
        }
    }

    /// A poll-cheap, secret-free view of the monitor: its link state, whether
    /// audio is arriving, the last talker, why it failed, and its drop and
    /// refusal counters.
    ///
    /// It reads the monitor's atomics and never waits on the network, so it
    /// is safe on a status pump. `None` with no monitor up, and without the
    /// `m17` feature.
    #[must_use]
    pub fn m17_monitor_status(&self) -> Option<M17MonitorStatus> {
        #[cfg(feature = "m17")]
        {
            self.session
                .lock()
                .expect("station session poisoned")
                .m17_monitor_status()
        }
        #[cfg(not(feature = "m17"))]
        {
            None
        }
    }

    /// The monitor's own last-heard list, for a node's last-heard display.
    ///
    /// Callsigns come off the reflector and are attacker-supplied, so render
    /// them as text only. Empty with no monitor up, and without the `m17`
    /// feature.
    #[must_use]
    pub fn m17_monitor_heard(&self) -> Vec<HeardEntry> {
        #[cfg(feature = "m17")]
        {
            self.session
                .lock()
                .expect("station session poisoned")
                .m17_monitor_heard()
        }
        #[cfg(not(feature = "m17"))]
        {
            Vec::new()
        }
    }

    /// Connect to a D-Star `DExtra` reflector, full-transceive (iax-a9d4
    /// Task 6 built RX; iax-2f6b adds TX): resolves `host`/`port`, validates
    /// `module` (uppercased; must land on `A`-`Z`) and a non-empty
    /// `callsign`, and opens the session over the station's currently
    /// selected input+output devices (mirrors [`Station::m17_connect`]'s
    /// device resolution exactly).
    ///
    /// `reflector_callsign` names the DESTINATION reflector as the
    /// directories list it (`"XLX836"`, `"XRF757"`) and fills the transmitted
    /// RF header's `RPT1`/`RPT2` — the fields that say where a transmission
    /// is going, which `xlxd` gates REF-family traffic on and every receiving
    /// radio and dashboard renders. `None` derives it from `host`'s first DNS
    /// label (`xlx836.…` → `XLX836`, addressed as `XRF836` on the `DExtra`
    /// wire), which covers every reflector reached by its published hostname;
    /// pass it explicitly when connecting by bare IP, where there is nothing
    /// to derive from and the header would otherwise go out blank.
    ///
    /// The station-level signature deliberately mirrors
    /// [`Station::m17_connect`]'s primitive-args shape (rather than taking
    /// an `astar_console::DstarConfig` directly): that type only exists
    /// when the `dstar` feature is compiled in, and this method must stay
    /// byte-identically callable (same signature, same "feature not
    /// compiled" error) whether or not it is — exactly the contract
    /// `m17_connect` already gives callers.
    ///
    /// Mutually exclusive with an active IAX2 call and a live M17 session:
    /// the session refuses with [`StationError::AlreadyConnected`] while
    /// either is live; the reverse exclusions (an IAX2 `connect`/inbound
    /// adopt, or `m17_connect`, while D-Star is live) are enforced by the
    /// same session.
    ///
    /// D-Star is hardware-only (iax-b3e7 M0): this always opens the `ThumbDV`
    /// AMBE dongle — there is no software-codec fallback and no preference
    /// knob (removed; see the crate-level docs). A build without the `dstar`
    /// feature, or a `dstar` build with no dongle attached, fails the connect
    /// outright rather than silently substituting a software decoder.
    ///
    /// # Blocking
    /// The `ThumbDV` probe/init handshake and the socket bind run with the
    /// session mutex NOT held: the audio lane is opened first (under the
    /// lock, exactly as an IAX2 dial has always opened its devices), the
    /// lock is released for the session construction, and only the install
    /// via `ConsoleSession::dstar_adopt` takes it again. A slow or flaky
    /// dongle therefore delays only the caller of this method; concurrent
    /// [`Station::snapshot`]/`dstar_state`/[`Station::set_ptt`] polls — which
    /// all take that same mutex — are unaffected for the whole of it, per
    /// the `AstarStation` poll-and-snapshot contract.
    ///
    /// # Errors
    /// [`StationError::Dstar`] for an invalid module/callsign, when the
    /// `dstar` feature isn't compiled in, and — since iax-b3e7 M0 made
    /// D-Star hardware-only — for every vocoder-availability failure:
    /// `DstarSession::connect` classifies "no dongle" vs. "dongle busy" vs.
    /// "wrong device" (spec §4) and returns `ConsoleError::Dstar` carrying
    /// the operator-facing message (e.g. `ThumbDV at /dev/cu.usbserial-… is
    /// busy — another process has it open`), which maps here to
    /// `StationError::Dstar`. It is NOT [`StationError::Audio`] — that
    /// variant now means a genuine audio device/stream failure and nothing
    /// else. [`StationError::AlreadyConnected`] while an IAX2 call or an M17
    /// session is live.
    pub fn dstar_connect(
        &self,
        host: &str,
        port: u16,
        module: char,
        callsign: &str,
        reflector_callsign: Option<&str>,
    ) -> Result<(), StationError> {
        let module = module.to_ascii_uppercase();
        if !module.is_ascii_uppercase() {
            return Err(StationError::Dstar("module must be A-Z".into()));
        }
        if callsign.is_empty() {
            return Err(StationError::Dstar("callsign must not be empty".into()));
        }
        #[cfg(feature = "dstar")]
        {
            let module_byte = u8::try_from(module).expect("ASCII-validated above");
            let (input, output) = self.selected_devices();
            let cfg = astar_console::DstarConfig {
                host: host.to_string(),
                port,
                module: module_byte,
                callsign: callsign.to_string(),
                // The destination reflector's callsign fills the TX header's
                // RPT1/RPT2 (see `astar_console::dstar`'s
                // `tx_repeater_fields`). `None` means "derive it from `host`"
                // — `xlx836.…` → `XLX836` → `XRF836` on the wire — which is
                // right for every reflector reached by its published
                // hostname; a caller connecting by bare IP passes it
                // explicitly instead of transmitting blank fields.
                reflector_callsign: reflector_callsign.map(str::to_string),
            };
            // Step 1, under the lock: open the ONE audio lane on the
            // station's router and reserve it. The reservation is also the
            // mutual-exclusion token for the gap before the adopt below —
            // every other connect path refuses while it is held — and the
            // pref push, so the session starts at the operator's chosen
            // volume rather than the router's unity default.
            let (audio, trust) =
                self.open_voice_route_with_trust(input.as_deref(), output.as_deref())?;

            // Step 2, deliberately OFF the session mutex: a candidate-port
            // scan, then per candidate and per baud rate an open plus an
            // eight-transaction init cookbook (each bounded at 300 ms), then
            // the socket bind — seconds, on a flaky dongle. Holding the
            // mutex across that blocked every snapshot/state poll for the
            // whole window.
            let session = match astar_console::DstarSession::connect(cfg, audio, &trust) {
                Ok(s) => s,
                Err(e) => {
                    // The lane opened but the session did not: give it back,
                    // or the station stays reserved forever. Dropped off the
                    // lock — a CoreAudio stream drop can stall.
                    let handles = self.session.lock().unwrap().release_voice_route();
                    drop(handles);
                    return Err(map_console_err(e));
                }
            };

            // Step 3: re-take the lock only to install (and to re-check
            // exclusion, since the state could have changed while it was
            // released).
            self.session
                .lock()
                .unwrap()
                .dstar_adopt(session)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "dstar"))]
        {
            let _ = (host, port, reflector_callsign);
            Err(StationError::Dstar("dstar support not compiled".into()))
        }
    }

    /// Disconnect the live D-Star session, if any (iax-a9d4 Task 6). No-op
    /// when none is active (and when the `dstar` feature isn't compiled
    /// in).
    pub fn dstar_disconnect(&self) {
        #[cfg(feature = "dstar")]
        {
            self.session.lock().unwrap().dstar_disconnect();
        }
    }

    /// `true` when D-Star voice is available: the `dstar` feature is
    /// compiled in AND a working AMBE backend was found (iax-a9d4 Task 6).
    /// Delegates directly to [`astar_console::dstar_available`]'s
    /// process-wide memoized probe — unlike [`Station::m17_available`],
    /// there is no dirs-injection knob to make this dirs-aware (D-Star's
    /// AMBE backends have no `set_codec_dirs` analogue in this milestone),
    /// so no station-level override/cache is needed.
    #[must_use]
    pub fn dstar_available(&self) -> bool {
        #[cfg(feature = "dstar")]
        {
            astar_console::dstar_available(&self.vocoder_trust())
        }
        #[cfg(not(feature = "dstar"))]
        {
            false
        }
    }

    /// Link to a `YSFReflector` and decode the audio on it (astar-e7b3 §2).
    ///
    /// `host` is `host:port` — YSF has no single conventional port and the
    /// directory publishes one per reflector, so there is nothing to default
    /// to. `options` is the YCS room request; `None` for a plain reflector.
    ///
    /// Primitive args rather than an `astar_console::YsfConfig` for the same
    /// reason [`Station::dstar_connect`] takes them: that type only exists
    /// when the feature is compiled in, and this method must stay
    /// byte-identically callable either way.
    ///
    /// Transceive. `Station::set_ptt` keys a live YSF link exactly as it
    /// keys D-Star; nothing keys on its own.
    ///
    /// Mutually exclusive with an IAX2 call, an M17 session and a D-Star
    /// session. One `ThumbDV`, one link.
    ///
    /// # Blocking
    /// The `ThumbDV` probe/init and the audio-device open both run with the
    /// session mutex NOT held, exactly as `dstar_connect` does and for the
    /// same reason: every `Station` method takes that mutex, and the contract
    /// is poll-and-snapshot.
    ///
    /// # Errors
    /// [`StationError::Ysf`] for an empty callsign, when the `ysf` feature
    /// isn't compiled in, and for every vocoder-availability failure (the
    /// message names the specific port when a dongle is merely busy).
    /// [`StationError::AlreadyConnected`] while another network is live.
    pub fn ysf_connect(
        &self,
        host: &str,
        callsign: &str,
        options: Option<&str>,
    ) -> Result<(), StationError> {
        if callsign.is_empty() {
            return Err(StationError::Ysf("callsign must not be empty".into()));
        }
        #[cfg(feature = "ysf")]
        {
            let (input, output) = self.selected_devices();
            let cfg = astar_console::YsfConfig {
                host: host.to_string(),
                callsign: callsign.to_string(),
                options: options.map(str::to_string),
            };
            // Step 1, under the lock: open the ONE audio lane on the
            // station's router and reserve it. The reservation is also the
            // mutual-exclusion token for the gap before the adopt below —
            // every other connect path refuses while it is held — and the
            // pref push, so the link starts at the operator's chosen volume
            // rather than the router's unity default.
            let (audio, trust) =
                self.open_voice_route_with_trust(input.as_deref(), output.as_deref())?;

            // Step 2, deliberately OFF the session mutex: the `ThumbDV`
            // candidate-port scan and init cookbook, then the socket bind —
            // seconds, on a flaky dongle. Holding the mutex across that
            // blocked every snapshot/state poll for the whole window.
            let link = match astar_console::YsfLink::connect_with_audio(&cfg, audio, &trust) {
                Ok(l) => l,
                Err(e) => {
                    // The lane opened but the link did not: give it back, or
                    // the station stays reserved forever. Dropped off the
                    // lock — a CoreAudio stream drop can stall.
                    let handles = self.session.lock().unwrap().release_voice_route();
                    drop(handles);
                    return Err(map_console_err(e));
                }
            };

            // Step 3: re-take the lock only to install (and to re-check
            // exclusion, since the state could have changed while it was
            // released).
            self.session
                .lock()
                .unwrap()
                .ysf_adopt(link)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "ysf"))]
        {
            let _ = (host, options);
            Err(StationError::Ysf("ysf support not compiled".into()))
        }
    }

    /// Disconnect the live YSF link, if any. No-op when none is active (and
    /// when the `ysf` feature isn't compiled in).
    pub fn ysf_disconnect(&self) {
        #[cfg(feature = "ysf")]
        {
            self.session.lock().unwrap().ysf_disconnect();
        }
    }

    /// `true` when System Fusion voice is available: the `ysf` feature is
    /// compiled in AND a `ThumbDV` is attached. The same cached probe
    /// [`Station::dstar_available`] reads — one dongle, one answer.
    #[must_use]
    pub fn ysf_available(&self) -> bool {
        #[cfg(feature = "ysf")]
        {
            astar_console::ysf_available(&self.vocoder_trust())
        }
        #[cfg(not(feature = "ysf"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live M17 session's state, or `None` when
    /// none is active — M17's equivalent of [`Station::dstar_state`] and
    /// [`Station::ysf_state`].
    ///
    /// Everything network-agnostic (PTT, remote PTT, call status, the level
    /// meters) is already in [`ConsoleState`]/the snapshot; what is only here
    /// is the last-heard `talker` — who most recently keyed up on the
    /// reflector — which has no snapshot equivalent, exactly as D-Star's
    /// talker has none.
    ///
    /// Only compiled when the `m17` feature is enabled — which, unlike
    /// `dstar`/`ysf`, it is by default.
    #[cfg(feature = "m17")]
    #[must_use]
    pub fn m17_state(&self) -> Option<astar_console::M17SnapshotState> {
        self.session.lock().unwrap().m17_state()
    }

    /// A poll-cheap snapshot of the live YSF link, or `None`. Only compiled
    /// when the `ysf` feature is enabled.
    #[cfg(feature = "ysf")]
    #[must_use]
    pub fn ysf_state(&self) -> Option<astar_console::YsfSnapshot> {
        self.session.lock().unwrap().ysf_state()
    }

    /// Link to an `NXDNReflector` talkgroup and decode the audio on it
    /// (iax-b9c2).
    ///
    /// `host` is `host:port`. NXDN addresses stations by NUMBER: `radio_id`
    /// is this station's registration and `talkgroup` is the TG to join —
    /// the reflector answers only polls carrying its own
    /// (`NXDNReflector.cpp`: `if (id == tg)`), so a wrong or zero talkgroup
    /// is a link that never comes up rather than one that comes up quiet.
    ///
    /// Primitive args rather than an `astar_console::NxdnConfig` for the same
    /// reason [`Station::ysf_connect`] takes them: that type only exists when
    /// the feature is compiled in, and this method must stay byte-identically
    /// callable either way.
    ///
    /// **Receive only.** [`Station::set_ptt`] refuses a key-down while an
    /// NXDN link is live — see `astar_console::nxdn`'s Transmit section for
    /// why a refusal beats a key that silently does nothing.
    ///
    /// Mutually exclusive with an IAX2 call and with every other digital
    /// network. One `ThumbDV`, one link.
    ///
    /// # Blocking
    /// The `ThumbDV` probe/init and the socket bind both run with the session
    /// mutex NOT held, exactly as `ysf_connect` does and for the same reason:
    /// every `Station` method takes that mutex, and the contract is
    /// poll-and-snapshot.
    ///
    /// # Errors
    /// [`StationError::Nxdn`] for an empty callsign, a zero radio id, a zero
    /// talkgroup, when the `nxdn` feature isn't compiled in, and for every
    /// vocoder-availability failure (the message names the specific port when
    /// a dongle is merely busy). [`StationError::AlreadyConnected`] while
    /// another network is live.
    pub fn nxdn_connect(
        &self,
        host: &str,
        callsign: &str,
        radio_id: u16,
        talkgroup: u16,
    ) -> Result<(), StationError> {
        // Validated BEFORE the `#[cfg]` block, so the refusals hold
        // identically with the feature off — an operator who mistypes gets
        // the same answer either way, and no lane is opened for a connect
        // that cannot succeed.
        if callsign.is_empty() {
            return Err(StationError::Nxdn("callsign must not be empty".into()));
        }
        // A radio id is a registration, not a default. Zero is what an unset
        // field looks like, and a transmission claiming it would claim
        // somebody else's number — or nobody's.
        if radio_id == 0 {
            return Err(StationError::Nxdn(
                "radio id must be set: NXDN addresses stations by number, and 0 is not a \
                 registration"
                    .into(),
            ));
        }
        // `NXDNReflector.cpp` registers a client only when the poll's TG
        // matches its own, and `Reflectors.h: CNXDNReflector::isEmpty` is
        // `m_id == 0` — so a zero talkgroup is a link that can never come
        // up, refused here rather than left to time out.
        if talkgroup == 0 {
            return Err(StationError::Nxdn(
                "talkgroup must be set: a reflector answers only polls carrying its own".into(),
            ));
        }
        #[cfg(feature = "nxdn")]
        {
            let (input, output) = self.selected_devices();
            let cfg = astar_console::NxdnConfig {
                host: host.to_string(),
                callsign: callsign.to_string(),
                radio_id,
                talkgroup,
            };
            // Step 1, under the lock: open the ONE audio lane on the
            // station's router and reserve it. The reservation is also the
            // mutual-exclusion token for the gap before the adopt below —
            // every other connect path refuses while it is held — and the
            // pref push, so the link starts at the operator's chosen volume
            // rather than the router's unity default.
            let (audio, trust) =
                self.open_voice_route_with_trust(input.as_deref(), output.as_deref())?;

            // Step 2, deliberately OFF the session mutex: the `ThumbDV`
            // candidate-port scan and init cookbook, then the socket bind —
            // seconds, on a flaky dongle. Holding the mutex across that
            // blocks every snapshot/state poll for the whole window.
            let link = match astar_console::NxdnLink::connect_with_audio(&cfg, audio, &trust) {
                Ok(l) => l,
                Err(e) => {
                    // The lane opened but the link did not: give it back, or
                    // the station stays reserved forever. Dropped off the
                    // lock — a CoreAudio stream drop can stall.
                    let handles = self.session.lock().unwrap().release_voice_route();
                    drop(handles);
                    return Err(map_console_err(e));
                }
            };

            // Step 3: re-take the lock only to install (and to re-check
            // exclusion, since the state could have changed while it was
            // released).
            self.session
                .lock()
                .unwrap()
                .nxdn_adopt(link)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "nxdn"))]
        {
            let _ = host;
            Err(StationError::Nxdn("nxdn support not compiled".into()))
        }
    }

    /// Disconnect the live NXDN link, if any. No-op when none is active (and
    /// when the `nxdn` feature isn't compiled in).
    pub fn nxdn_disconnect(&self) {
        #[cfg(feature = "nxdn")]
        {
            self.session.lock().unwrap().nxdn_disconnect();
        }
    }

    /// `true` when NXDN voice is available: the `nxdn` feature is compiled in
    /// AND a `ThumbDV` is attached. The same cached probe
    /// [`Station::dstar_available`] and [`Station::ysf_available`] read — one
    /// dongle, one answer.
    #[must_use]
    pub fn nxdn_available(&self) -> bool {
        #[cfg(feature = "nxdn")]
        {
            astar_console::nxdn_available(&self.vocoder_trust())
        }
        #[cfg(not(feature = "nxdn"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live NXDN link, or `None`. Only compiled
    /// when the `nxdn` feature is enabled.
    #[cfg(feature = "nxdn")]
    #[must_use]
    pub fn nxdn_state(&self) -> Option<astar_console::NxdnSnapshot> {
        self.session.lock().unwrap().nxdn_state()
    }

    /// Link to a DMR master's talkgroup and decode the audio on it
    /// (iax-d4f7).
    ///
    /// `system` names the network this master belongs to and is carried
    /// through verbatim. **Any non-empty string is accepted**, because two
    /// vocabularies arrive at this argument: a family slug
    /// (`astar_dmr::DmrNetwork::from_slug` — `tgif`, `brandmeister`, nine of
    /// them) and a directory row's server name
    /// (`DmrNetwork::from_system_slug` — `freedmr-network`, `ipsc2-poland`,
    /// `xlx696`, 111 of them in the 2026-09-07 feed, none of which equals a
    /// family slug). Both doors are tried; a `system` neither claims is dialed
    /// anyway, since the family is read for the consent gate and nothing
    /// else. `radio_id` is the operator's registration (radioid.net),
    /// `talkgroup` the room and `timeslot` 1 or 2. DMR addresses stations by
    /// NUMBER — a `DMRD` carries `srcId` and no callsign at all — so
    /// `callsign` rides only in the `RPTC` config.
    ///
    /// Primitive args rather than an `astar_console::DmrConfig` for the same
    /// reason [`Station::nxdn_connect`] takes them: that type only exists when
    /// the feature is compiled in, and this method must stay byte-identically
    /// callable either way.
    ///
    /// # The password
    ///
    /// `password` is taken **by value** — the one place in this facade that
    /// does. It is moved into the `DmrConfig` built here, moved on into the
    /// link's FSM, spent on one `RPTK` digest and dropped. No `Station` field
    /// holds it, `StationConfig` grows none, no snapshot, event, error or log
    /// carries it, and the config is rebuilt from the in-args on every call
    /// rather than cached. A `&str` would have left the caller holding the
    /// secret with nothing saying when to drop it; this does not.
    ///
    /// # BrandMeister
    ///
    /// BrandMeister is refused unless the operator has explicitly opted in,
    /// and **nothing in this build can opt in yet** — see
    /// `BRANDMEISTER_CONSENTED` below. The gate is `astar_dmr::dialable`, written
    /// once so no call site can forget it, and it is checked HERE, before a
    /// socket or a dongle is touched: the risk it guards is to the operator's
    /// own account on someone else's private network, so it is presented
    /// before it is taken rather than after. See
    /// `docs/design/dmr-networks.md`.
    ///
    /// **Receive only.** [`Station::set_ptt`] refuses a key-down while a DMR
    /// link is live — see `astar_console::dmr`'s Transmit section for why a
    /// refusal beats a key that silently does nothing.
    ///
    /// Mutually exclusive with an IAX2 call and with every other digital
    /// network. One `ThumbDV`, one link.
    ///
    /// # Blocking
    /// The `ThumbDV` probe/init, the socket bind AND the homebrew login —
    /// which is a multi-round-trip conversation with the master, slower than
    /// anything YSF or NXDN does — all run with the session mutex NOT held.
    /// Every `Station` method takes that mutex, and the contract is
    /// poll-and-snapshot.
    ///
    /// # Errors
    /// [`StationError::Dmr`] for an empty system, an ungated
    /// BrandMeister target, a zero or over-wide radio id, an empty callsign,
    /// a zero or over-wide talkgroup, a timeslot that is not 1 or 2, an empty
    /// password,
    /// when the `dmr` feature isn't compiled in, and for every
    /// vocoder-availability failure (the message names the specific port when
    /// a dongle is merely busy). [`StationError::AlreadyConnected`] while
    /// another network is live. Never carrying the password.
    #[allow(clippy::too_many_arguments)]
    pub fn dmr_connect(
        &self,
        system: &str,
        host: &str,
        port: u16,
        radio_id: u32,
        callsign: &str,
        talkgroup: u32,
        timeslot: u8,
        password: String,
    ) -> Result<(), StationError> {
        // Everything down to the `#[cfg]` block is validated with the feature
        // OFF as well as on, so an operator who mistypes gets the same answer
        // either way and no lane is opened for a connect that cannot succeed.
        //
        // The family is resolved FIRST because the consent gate hangs off it,
        // and the consent gate must run before anything is opened.
        //
        // **`system` is a name, not an enumeration.** Two vocabularies meet
        // at this argument: `DmrNetwork::slug` names one of nine families,
        // and a directory row's `system` names one server — `freedmr-network`,
        // `ipsc2-poland`, `xlx696` — of which a family has many. Checked
        // 2026-09-07: not one of the feed's 111 `system` values equals a
        // family slug, so refusing everything that is not a slug (which this
        // did until the seam was found) made every one of the app's 185
        // directory rows undialable. Both doors are tried, and a `system` no
        // family claims is dialed anyway with `family: None`: the family is
        // consulted for the consent gate and nothing else, and a network
        // astar cannot name is not BrandMeister.
        let trimmed = system.trim();
        if trimmed.is_empty() {
            return Err(StationError::Dmr(
                "a DMR target must name its network: a talkgroup number means a different room \
                 on each of them"
                    .into(),
            ));
        }
        let family = astar_dmr::DmrNetwork::from_slug(trimmed)
            .or_else(|| astar_dmr::DmrNetwork::from_system_slug(trimmed));
        // Belt and braces: the family answer AND the raw spelling. The bridge
        // above already resolves `brandmeister`, `brandmeister-3102` and
        // `brandmeister_uk`, but this gate protects an operator's account on
        // somebody else's private network, so a spelling that slips past the
        // separator rule (`brandmeister3102`) must not slip past this.
        let gated = if trimmed.to_ascii_lowercase().starts_with("brandmeister") {
            Some(astar_dmr::DmrNetwork::BrandMeister)
        } else {
            family
        };
        // `dialable` is the gate written once so no call site can forget it,
        // and this facade is a call site. Asking it about whatever family
        // resolved — rather than testing `requires_consent()` here, or naming
        // BrandMeister — is what keeps the policy in one place: a second gated
        // network would be refused here without this line changing.
        if let Some(network) = gated
            && !astar_dmr::dialable(BRANDMEISTER_CONSENTED).contains(&network)
        {
            return Err(StationError::Dmr(format!(
                "{} requires the operator to opt in first: it is a private network whose \
                 operators set the terms, and connecting with a third-party client is a risk \
                 to your own access there",
                network.label()
            )));
        }
        // A radio id is a registration, not a default. Zero is what an unset
        // field looks like, and a transmission claiming it would claim
        // somebody else's number — or nobody's; anything past 24 bits cannot
        // go on the wire at all. `RadioId::new` is the one definition of
        // both, reused rather than restated — and asked HERE so the answer is
        // the same with the feature off, where the link layer that would
        // otherwise catch it is not compiled.
        if let Err(e) = astar_dmr::RadioId::new(radio_id) {
            return Err(StationError::Dmr(format!(
                "radio id {radio_id} is not a usable registration: {e}"
            )));
        }
        if callsign.is_empty() {
            return Err(StationError::Dmr("callsign must not be empty".into()));
        }
        if talkgroup == 0 {
            return Err(StationError::Dmr(
                "talkgroup must be set: a master routes by the room you joined".into(),
            ));
        }
        // And a talkgroup is a `DMRD` DESTINATION id, which is the same 24
        // bits the source id gets — so the same bound, from the same
        // constant, asked in the same place. Without it a wider number
        // reached the wire silently truncated: TG 16,777,217 would have
        // joined room 1, which is somebody else's room.
        if talkgroup > astar_dmr::RADIO_ID_MAX {
            return Err(StationError::Dmr(format!(
                "talkgroup {talkgroup} is past the {} a DMRD destination id holds — 24 bits",
                astar_dmr::RADIO_ID_MAX
            )));
        }
        // Secret-free by construction: the refusal names the field, never the
        // value.
        if password.is_empty() {
            return Err(StationError::Dmr(
                "the master password must not be empty".into(),
            ));
        }
        // 1 and 2 are the only slots a DMR frame can name. Mapped here rather
        // than passed through as a number so a caller crossing the C ABI with
        // a 0 gets a refusal instead of a silent TS1.
        let slot = match timeslot {
            1 => astar_dmr::Timeslot::Ts1,
            2 => astar_dmr::Timeslot::Ts2,
            other => {
                return Err(StationError::Dmr(format!(
                    "timeslot must be 1 or 2, got {other}"
                )));
            }
        };
        #[cfg(feature = "dmr")]
        {
            let (input, output) = self.selected_devices();
            // Built from the in-args, every call. Nothing on this station
            // caches it, and `DmrConfig` is deliberately not `Clone` — the
            // password inside it is moved, not copied.
            let cfg = astar_console::DmrConfig {
                system: trimmed.to_string(),
                family,
                host: host.to_string(),
                port,
                radio_id,
                callsign: callsign.to_string(),
                talkgroup,
                timeslot: slot,
                password,
            };
            // Step 1, under the lock: open the ONE audio lane on the
            // station's router and reserve it. The reservation is also the
            // mutual-exclusion token for the gap before the adopt below —
            // every other connect path refuses while it is held — and the
            // pref push, so the link starts at the operator's chosen volume
            // rather than the router's unity default.
            let (audio, trust) =
                self.open_voice_route_with_trust(input.as_deref(), output.as_deref())?;

            // Step 2, deliberately OFF the session mutex: the `ThumbDV`
            // candidate-port scan and init cookbook, the socket bind, and
            // then the homebrew login — `RPTL`, `RPTK`, `RPTC`, each a
            // round trip to the master, with retries. Seconds, on a flaky
            // dongle or a slow master. Holding the mutex across that blocks
            // every snapshot/state poll for the whole window.
            let link = match astar_console::DmrLink::connect_with_audio(cfg, audio, &trust) {
                Ok(l) => l,
                Err(e) => {
                    // The lane opened but the link did not: give it back, or
                    // the station stays reserved forever. Dropped off the
                    // lock — a CoreAudio stream drop can stall.
                    let handles = self.session.lock().unwrap().release_voice_route();
                    drop(handles);
                    return Err(map_console_err(e));
                }
            };

            // Step 3: re-take the lock only to install (and to re-check
            // exclusion, since the state could have changed while it was
            // released). A refused adopt tears the link down and releases
            // the route inside `dmr_adopt`.
            self.session
                .lock()
                .unwrap()
                .dmr_adopt(link)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "dmr"))]
        {
            // `slot` is validated above either way — that is the point — and
            // with no link to build there is nowhere for it to go.
            let _ = (host, port, slot);
            // `password` is dropped here with everything else. It was never
            // stored, and this build has nowhere to send it.
            drop(password);
            Err(StationError::Dmr("dmr support not compiled".into()))
        }
    }

    /// Disconnect the live DMR link, if any. No-op when none is active (and
    /// when the `dmr` feature isn't compiled in).
    pub fn dmr_disconnect(&self) {
        #[cfg(feature = "dmr")]
        {
            self.session.lock().unwrap().dmr_disconnect();
        }
    }

    /// `true` when DMR voice is available: the `dmr` feature is compiled in
    /// AND a `ThumbDV` is attached. The same cached probe
    /// [`Station::dstar_available`], [`Station::ysf_available`] and
    /// [`Station::nxdn_available`] read — one dongle, one answer.
    #[must_use]
    pub fn dmr_available(&self) -> bool {
        #[cfg(feature = "dmr")]
        {
            astar_console::dmr_available(&self.vocoder_trust())
        }
        #[cfg(not(feature = "dmr"))]
        {
            false
        }
    }

    /// A poll-cheap snapshot of the live DMR link, or `None`. Only compiled
    /// when the `dmr` feature is enabled.
    ///
    /// Carries no password — there is none on the link to carry.
    #[cfg(feature = "dmr")]
    #[must_use]
    pub fn dmr_state(&self) -> Option<astar_console::DmrSnapshot> {
        self.session.lock().unwrap().dmr_state()
    }

    /// A poll-cheap snapshot of the live D-Star session's state (iax-a9d4
    /// Task 6), or `None` when no session is active. Unlike M17's state,
    /// this is NOT mirrored into [`ConsoleState`] — D-Star has no
    /// transmit-side concept to reuse `ConsoleState`'s call-shaped fields
    /// for, and its own fields (talker/slow-data/backend) have no
    /// `ConsoleState` equivalent. Only compiled when the `dstar` feature is
    /// enabled.
    #[cfg(feature = "dstar")]
    #[must_use]
    pub fn dstar_state(&self) -> Option<astar_console::DstarSnapshotState> {
        self.session.lock().unwrap().dstar_state()
    }

    /// The live operating mode, derived from capability state: `Node` when the
    /// inbound listener is running, `Wt` otherwise. This stays consistent whether
    /// the listener was started via [`Station::set_mode`] or
    /// [`Station::enable_inbound`] directly.
    #[must_use]
    pub fn mode(&self) -> OperatingMode {
        if self.is_listening() {
            OperatingMode::Node
        } else {
            OperatingMode::Wt
        }
    }

    /// Override the Node-mode configuration used the next time the engine
    /// starts (on the next [`Station::set_mode`]`(Node)`). Takes precedence over
    /// the `node` field of the [`StationConfig`] the station was built with.
    pub fn set_node_config(&self, cfg: NodeConfig) {
        *self.node_config.lock().unwrap() = Some(cfg);
    }

    /// Register the secret resolver used for outbound node registration (Phase 7)
    /// and any future auth needs. The secret is resolved on demand and consumed
    /// immediately into the registrar — never stored in config/snapshot/log.
    pub fn set_secret_resolver(&self, f: Box<SecretResolver>) {
        *self.secret_resolver.lock().unwrap() = Some(f);
    }

    /// Resolve the effective Node config: the runtime override, else the
    /// constructor `config.node`, else `NodeConfig::default()`. Returns an owned
    /// value built by re-reading the relevant fields (`NodeConfig` isn't Clone).
    fn effective_node_config(&self) -> NodeConfig {
        if let Some(over) = self.node_config.lock().unwrap().take() {
            return over;
        }
        match &self.config.node {
            Some(c) => clone_node_config(c),
            None => NodeConfig::default(),
        }
    }

    /// Stop the inbound listener: hang up its active call, then drop the
    /// listener (its `Drop` stops the actor thread + frees the port).
    fn stop_inbound(&self) {
        self.session.lock().unwrap().stop_inbound();
    }

    /// Start the inbound listener with an explicit [`InboundConfig`] (iax-a1fb P2).
    ///
    /// Builds the audio backend via the station's factory, then calls
    /// `session.start_inbound(cfg.bind, cfg.policy, cfg.answer, ...)`.
    /// The station's operating mode is NOT changed — use [`Station::set_mode`]
    /// if you also want to flip the WT/Node mode flag.
    ///
    /// # Errors
    /// [`StationError::Listen`] if the listener cannot bind the port.
    /// [`StationError::Audio`] / [`StationError::Iax`] on other failures.
    pub fn enable_inbound(&self, cfg: crate::InboundConfig) -> Result<(), StationError> {
        let make_backend = &self.make_backend;
        let devices = self.selected_devices();
        let mut sess = self.session.lock().unwrap();
        sess.start_inbound_with_allowlist(
            cfg.bind,
            self.inbound_policy(cfg.policy),
            cfg.answer,
            cfg.max_calls,
            cfg.allowlist,
            || make_backend(),
            devices,
        )
        .map_err(|e| match e {
            // IAX-level error from start_inbound is always a bind/start failure
            // → map to the dedicated Listen variant so callers can distinguish
            // "port taken" from audio / device errors.
            astar_console::ConsoleError::Iax(_) => StationError::Listen(e.to_string()),
            other => map_console_err(other),
        })
    }

    /// Stop the inbound listener and hang up any active inbound-adopted call.
    /// Idempotent — no-op when the listener is not running.
    /// The station's operating mode is NOT changed.
    pub fn disable_inbound(&self) {
        self.session.lock().unwrap().stop_inbound();
    }

    /// `true` while the inbound listener is running (whether started via
    /// [`Station::enable_inbound`] or [`Station::set_mode`]).
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.session.lock().unwrap().has_inbound()
    }

    /// Start outbound node registration using the provided [`RegisterConfig`].
    /// The secret is resolved via the station's `secret_resolver` on demand
    /// and consumed immediately into the registrar — it is never stored on
    /// the station or surfaced in any event, snapshot, log, or `Debug` output.
    ///
    /// When no resolver is set, a `RegisterFailed` event is queued immediately
    /// and the method returns `Ok(())` (no panic, no error — the failure is
    /// surfaced through the normal [`Station::next_event`] poll path).
    ///
    /// # Errors
    /// [`StationError::Iax`] if the underlying UDP socket or mio poll cannot
    /// be created (only when a resolver IS set and the OS rejects the bind).
    pub fn register(&self, cfg: RegisterConfig) -> Result<(), StationError> {
        // iax-177d: remember the (secret-free) config so a later registration
        // failure can be retried automatically; a fresh explicit register
        // restarts the backoff ladder.
        let attempt = cfg.clone();
        {
            let mut s = self.reg_supervise.lock().unwrap();
            s.cfg = Some(cfg);
            s.retry = None;
            // An explicit register starts from the top of the list, not
            // wherever a previous failure left the rotation.
            s.peer_index = 0;
        }
        self.do_register(&attempt)
    }

    /// Resolve the secret and start the registration thread, WITHOUT touching
    /// the supervision state (shared by [`Station::register`] and the retry
    /// path in [`Station::next_event`]).
    fn do_register(&self, cfg: &RegisterConfig) -> Result<(), StationError> {
        let peer = {
            let s = self.reg_supervise.lock().unwrap();
            let candidates = cfg.candidates();
            candidates[s.peer_index % candidates.len()]
        };
        self.do_register_to(cfg, peer)
    }

    /// [`Self::do_register`] against one chosen candidate.
    fn do_register_to(
        &self,
        cfg: &RegisterConfig,
        peer: std::net::SocketAddr,
    ) -> Result<(), StationError> {
        // Resolve the secret on-demand; pass None if no resolver is set.
        let secret: Option<Arc<Secret>> = self
            .secret_resolver
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| Arc::new(Secret::new(f(&cfg.username))));

        self.session
            .lock()
            .unwrap()
            .start_register(peer, cfg.username.clone(), cfg.refresh, secret)
            .map_err(|e| StationError::Iax(e.to_string()))
    }

    /// Stop the outbound registration. Drops the registration handle (its
    /// `Drop` sends REGREL to the registrar and joins the thread). Also ends
    /// re-register supervision (iax-177d) — deregister means STAY down.
    /// Idempotent.
    pub fn deregister(&self) {
        *self.reg_supervise.lock().unwrap() = RegSupervise::default();
        self.session.lock().unwrap().stop_register();
    }

    /// `true` once a successful `Registered` event has been observed and
    /// `deregister` has not been called since.
    #[must_use]
    pub fn is_registered(&self) -> bool {
        self.session.lock().unwrap().is_registered()
    }

    /// Switch the operating mode live. Switching to the current mode is a no-op.
    /// Switching mode hangs up any active call first. The change surfaces as a
    /// [`StationEvent::ModeChanged`] on the next [`Station::next_event`].
    ///
    /// Switching to [`OperatingMode::Node`] binds the inbound listener on the
    /// session's shared `Manager`; if the effective `NodeConfig` has a
    /// `register` entry, outbound node registration is also started. Switching
    /// back to [`OperatingMode::Wt`] drops the listener and calls `deregister`.
    ///
    /// # Errors
    /// [`StationError::Audio`] / [`StationError::Iax`] if the listener cannot
    /// resolve its devices or bind.
    pub fn set_mode(&self, mode: OperatingMode) -> Result<(), StationError> {
        if self.mode() == mode {
            return Ok(()); // no-op, no event
        }
        match mode {
            OperatingMode::Node => {
                self.disconnect(); // hang up any active WT call off-lock
                let cfg = self.effective_node_config();
                let reg_cfg = cfg.register.clone();
                // Start the inbound listener.
                {
                    let make_backend = &self.make_backend;
                    let devices = self.selected_devices();
                    let mut sess = self.session.lock().unwrap();
                    sess.start_inbound_with_allowlist(
                        cfg.bind,
                        self.inbound_policy(crate::config::clone_policy(&cfg.policy)),
                        cfg.answer,
                        cfg.max_calls,
                        cfg.allowlist.clone(),
                        || make_backend(),
                        devices,
                    )
                    .map_err(map_console_err)?;
                }
                // Start outbound registration if configured.
                if let Some(r) = reg_cfg {
                    let _ = self.register(r);
                }
                Ok(())
            }
            OperatingMode::Wt => {
                self.stop_inbound();
                self.deregister();
                Ok(())
            }
        }
    }

    /// The bound address of the inbound listener, or `None` when not listening.
    #[must_use]
    pub fn node_bind_addr(&self) -> Option<SocketAddr> {
        self.session.lock().unwrap().inbound_addr()
    }

    /// The number of IAX2 calls that have been placed since the session's engine
    /// was first started (lifetime counter). Returns `0` before the first dial
    /// (no engine yet). Delegates to [`ConsoleSession::call_count`].
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.session.lock().unwrap().call_count()
    }

    /// Latest call state (meters, status, `ptt`/`remote_ptt`, rtt). Cheap; poll it.
    #[must_use]
    pub fn snapshot(&self) -> ConsoleState {
        // The DTMF sequencer is poll-driven off this method (iax-4b7a): the
        // pump runs before the session snapshot so progress below is current.
        // No-op (one uncontended try-lock) when no sequence is active.
        self.pump_dtmf();
        let mode = self.mode();
        let (dtmf_played, dtmf_total) = self.dtmf_progress();
        let mut snap = self.session.lock().unwrap().snapshot();
        snap.mode = mode;
        snap.dtmf_played = dtmf_played;
        snap.dtmf_total = dtmf_total;
        // iax-f2b8-fix Fix 2: override the session's dirs-blind
        // `astar_console::m17_available()` mirror with the dirs-aware,
        // memoized `Station::m17_available()` — the contract documented at
        // the FFI/header/Swift-binding layers (a bundled-dylib deployment via
        // `set_codec_dirs` must show available) otherwise silently breaks.
        snap.m17_available = self.m17_available();
        // The mic monitor is a Station-level lane the console session knows
        // nothing about, so with no active call `session.snapshot()` writes the
        // -60 silence floor into every level field. That is right for TX and RX
        // — there is no call to have levels — but wrong for the continuous mic
        // INPUT level, which exists precisely to be metered with no call up: it
        // is what the VOX calibration meter watches while you set a threshold.
        // Without this the meter only ever moved mid-call.
        //
        // Guarded on there being no active call: a call's own routed mic wins,
        // and nothing stops a call starting while the monitor is still open
        // (`monitor_start` declines during a call, but not the reverse).
        let call_active = self.session.lock().unwrap().is_active();
        if let Some(db) = self.monitor_input_dbfs().filter(|_| !call_active) {
            snap.input_level_db = db;
        }
        // Which noise-reduction chain WOULD run, when none is running to
        // measure. Idle is when this gets configured, so a capability line
        // and a strength slider that only appear mid-call are of no use to
        // anyone. `live` stays false, and the summary says "when you key" —
        // a prediction must not read like an observation.
        if !snap.denoise_status.live {
            // Both operands in locals, deliberately. A temporary `MutexGuard`
            // lives to the end of the enclosing statement, so passing
            // `self.session.lock().unwrap().denoise()` and
            // `self.capture_rate_cached()` as sibling arguments would hold the
            // session lock across the probe — ~1.4 ms on a cache miss, during
            // which `try_snapshot` returns `None` and an event loop drops a
            // frame. The lock is released before the probe runs.
            let denoise_on = self.session.lock().unwrap().denoise();
            let device_rate = self.capture_rate_cached();
            snap.denoise_status = astar_audio::DenoiseStatus::predicted(
                denoise_on,
                astar_audio::DenoiseMode::from_env(),
                device_rate,
            );
        }
        snap
    }

    /// The rate the selected capture device would open at, cached per device
    /// selection. `0` when it cannot be determined.
    ///
    /// See `capture_probe` for why this is cached rather than probed per
    /// poll: `snapshot()` runs ~20 Hz and the probe is ~1.4 ms.
    fn capture_rate_cached(&self) -> u32 {
        let device = self.devices.lock().unwrap().0.clone();
        let mut slot = self.capture_probe.lock().unwrap();
        if let Some(p) = slot.as_ref()
            && p.device == device
        {
            return p.rate;
        }
        let rate = (self.make_backend)()
            .input_capture_rate(device.as_deref())
            .unwrap_or(0);
        *slot = Some(CaptureProbe { device, rate });
        rate
    }

    /// Non-blocking snapshot: returns `None` when the session lock is held
    /// (e.g. by a slow connect/disconnect audio teardown) instead of blocking.
    ///
    /// A live event loop (such as the harness SSE stream) uses this to re-emit
    /// the last known state while connect/disconnect hold the lock, so the
    /// stream stays responsive and never freezes on the lock.
    #[must_use]
    pub fn try_snapshot(&self) -> Option<ConsoleState> {
        let mode = self.mode();
        self.session.try_lock().ok().map(|mut s| {
            let mut snap = s.snapshot();
            snap.mode = mode;
            // iax-f2b8-fix Fix 2: same dirs-aware override as `snapshot()`.
            snap.m17_available = self.m17_available();
            snap
        })
    }

    /// Push an announcement service config into the session (and the live
    /// `Manager`, if one exists). The config is replayed when the `Manager` is
    /// (re)built so it survives connect/disconnect cycles.
    pub fn set_announce_config(&self, cfg: astar_iax::ServiceConfig) {
        self.session.lock().unwrap().set_announce_config(cfg);
    }

    /// Set the bridge/conference configuration (iax-647d). The Station library
    /// default stays handset (1:1), so this is a no-op for existing embedders
    /// until called. The node daemon calls it with `Bridge` (its default) and on
    /// `POST /bridge`. Applied immediately if a `Manager` exists (re-wiring live
    /// calls) and replayed when the `Manager` is (re)built.
    ///
    /// # Errors
    /// [`StationError::Iax`] if re-wiring live calls fails.
    pub fn set_bridge_config(&self, cfg: astar_iax::BridgeConfig) -> Result<(), StationError> {
        self.session
            .lock()
            .unwrap()
            .set_bridge_config(cfg)
            .map_err(map_console_err)
    }

    /// Current bridge/conference configuration (iax-647d).
    #[must_use]
    pub fn bridge_config(&self) -> astar_iax::BridgeConfig {
        self.session.lock().unwrap().bridge_config()
    }

    /// Route the whole engine — outgoing dials, the inbound listener, and
    /// outbound registration — through one shared userspace `WireGuard`
    /// tunnel (iax-5bbd). Call BEFORE connect/enable-inbound: the transport
    /// is immutable while a session is up (switching = disconnect/reconnect,
    /// then set again). Never calling this is byte-identical to today (plain
    /// OS UDP).
    ///
    /// `private_key` is the caller-held key material (e.g. from the Keychain),
    /// a **call-time argument** wrapped in a one-shot resolver answering only
    /// for `cfg`'s key reference — consumed at tunnel build, never stored in
    /// config/snapshot/event/log types, per the house secret rule.
    ///
    /// # Errors
    /// [`StationError::AlreadyConnected`] if a call is pooled (the engine's
    /// `CallInProgress` refusal); [`StationError::Link`] if the tunnel
    /// config/key is unusable — the message names the key *reference*, never
    /// material. With no engine yet the selection is deferred, and a bad key
    /// surfaces (naming the ref) from the connect/enable-inbound call that
    /// first builds the engine.
    pub fn set_wireguard(
        &self,
        cfg: astar_iax::WgLinkConfig,
        private_key: &str,
    ) -> Result<(), StationError> {
        let key_ref = cfg.private_key_ref().to_string();
        let key = private_key.to_string();
        let resolver: Box<astar_console::LinkKeyResolver> = Box::new(move |r: &str| {
            if r == key_ref {
                key.clone()
            } else {
                String::new()
            }
        });
        self.session
            .lock()
            .unwrap()
            .set_link_transport(astar_iax::LinkTransport::Wireguard(cfg), resolver)
            .map_err(map_console_err)
    }

    /// Clear the link transport back to plain OS UDP (iax-5bbd) — the inverse
    /// of [`Station::set_wireguard`]. Same immutability rule: refused while a
    /// session is up.
    ///
    /// # Errors
    /// [`StationError::AlreadyConnected`] if a call is pooled.
    pub fn clear_wireguard(&self) -> Result<(), StationError> {
        self.session
            .lock()
            .unwrap()
            .set_link_transport(
                astar_iax::LinkTransport::Udp,
                Box::new(|_: &str| String::new()),
            )
            .map_err(map_console_err)
    }

    /// Set local transmit (PTT) on the active call, whatever network that
    /// call is on: an IAX2 call, an M17 session, or — since iax-2f6b — a
    /// D-Star session.
    ///
    /// # Remote-control surfaces
    ///
    /// This is the ONE entry point that can key a transmitter, and it is
    /// network-agnostic by design, so anything that can reach a `Station` can
    /// key it. `astar-server` exposes exactly that: `POST /key` (`http.rs`)
    /// and a TUI keystroke, both landing here.
    ///
    /// D-Star is not remotely keyable, and that is ENFORCED rather than
    /// arranged: `astar-server`'s `NodeCommand::Key` handler refuses while
    /// [`ConsoleState::dstar_active`](astar_console::ConsoleState) is set
    /// (iax-d9f4, fixed in iax-4c8e). The refusal reads a feature-independent
    /// snapshot flag rather than being `#[cfg]`-gated, because the hazard it
    /// closes is precisely a feature appearing where the manifest didn't ask
    /// for it: `astar-sys` enables `astar-station/dstar` for the
    /// C-ABI, and Cargo unifies features across a build, so the node compiles
    /// WITH `dstar` in any workspace build that includes the C-ABI.
    ///
    /// Anything else that grows a remote-keying surface must make the same
    /// check — this method will not make it for you.
    ///
    /// # Errors
    /// [`StationError::NotConnected`] if no call is active.
    pub fn set_ptt(&self, on: bool) -> Result<(), StationError> {
        // Uniform: the session keys the active call's routed mic regardless of
        // whether it was dialed (WT) or adopted from an inbound offer (Node).
        self.session
            .lock()
            .unwrap()
            .set_ptt(on)
            .map_err(map_console_err)
    }

    /// Select how [`Station::send_dtmf`] emits digits (iax-7fff): an in-band
    /// tone in the TX audio path ([`DtmfMode::InBand`], the default) or
    /// out-of-band IAX2 protocol frames ([`DtmfMode::Protocol`]). Applies to
    /// the digits sent after the change; the active call is untouched.
    pub fn set_dtmf_mode(&self, mode: DtmfMode) {
        *self.dtmf_mode.lock().unwrap() = mode;
    }

    /// The current DTMF emission mode (see [`Station::set_dtmf_mode`]).
    #[must_use]
    pub fn dtmf_mode(&self) -> DtmfMode {
        *self.dtmf_mode.lock().unwrap()
    }

    /// Send a single DTMF digit to the active call's peer (the binding-exposure
    /// slice of iax-3746). How the digit is emitted follows the station's
    /// [`DtmfMode`] (iax-7fff): by default one complete, fixed-duration
    /// **in-band tone** ([`DEFAULT_DTMF_MS`]; see [`Station::send_dtmf_for`]
    /// for a duration override); in [`DtmfMode::Protocol`] one out-of-band
    /// `DtmfBegin`/`DtmfEnd` frame pair.
    ///
    /// `digit` must be one of the 16 DTMF keys (`0-9`, `*`, `#`, `A-D`). Any
    /// other character is rejected with [`StationError::InvalidDigit`].
    ///
    /// In-band, the tone is synthesized with the iax-68eb generator and
    /// injected to-air on the active call via the announce path (it briefly
    /// auto-keys the mic for the tone's duration and auto-unkeys, never
    /// dropping an operator's existing PTT). Input command only: nothing is
    /// stored, returned, logged, or surfaced in a snapshot/event.
    ///
    /// # Errors
    /// [`StationError::InvalidDigit`] for a non-keypad character;
    /// [`StationError::NotConnected`] if no call is active.
    pub fn send_dtmf(&self, digit: char) -> Result<(), StationError> {
        self.send_dtmf_for(digit, DEFAULT_DTMF_MS)
    }

    /// As [`Station::send_dtmf`], but with an explicit tone duration in
    /// milliseconds. `duration_ms` is clamped to a sane floor so the receiver's
    /// Goertzel decoder always sees enough analysis blocks to confirm the
    /// digit. In [`DtmfMode::Protocol`] the duration is ignored — the
    /// out-of-band frame pair carries the digit only.
    ///
    /// # Errors
    /// [`StationError::InvalidDigit`] for a non-keypad character;
    /// [`StationError::NotConnected`] if no call is active.
    pub fn send_dtmf_for(&self, digit: char, duration_ms: u32) -> Result<(), StationError> {
        // iax-8d2f: synthesize at the LIVE pipeline rate (16 kHz on a
        // prefer-slin16 station) so the tone plays at true pitch.
        let rate = self
            .session
            .lock()
            .expect("station session poisoned")
            .pipeline_sample_rate();
        match plan_dtmf(self.dtmf_mode(), digit, duration_ms, rate)? {
            DtmfEmission::Tone(pcm) => {
                // Inject the tone to-air on the active call via the announce path.
                let req = astar_iax::AnnounceRequest {
                    phrase: astar_iax::Phrase::Pcm(pcm.into()),
                    destination: astar_iax::Destination::ToAir,
                    policy: astar_iax::AnnouncePolicyReq::Seize,
                    priority: DTMF_ANNOUNCE_PRIORITY,
                };
                self.announce(req)
            }
            DtmfEmission::Protocol(d) => self
                .session
                .lock()
                .unwrap()
                .send_dtmf(d)
                .map_err(map_console_err),
        }
    }

    /// Send a multi-digit DTMF command to the active call as one engine-timed
    /// sequence (iax-4b7a): each digit is a [`DEFAULT_DTMF_MS`] tone followed
    /// by a [`DEFAULT_DTMF_GAP_MS`] gap, honoring the current
    /// [`Station::dtmf_mode`]. Validation is all-or-nothing: every character
    /// must be one of the 16 DTMF keys or nothing is sent. The first digit
    /// goes out synchronously (proving connectivity); the rest are queued and
    /// advanced by [`Station::snapshot`] polls, with progress surfaced as
    /// `ConsoleState::dtmf_played` / `dtmf_total`. Input command only: the
    /// digits are never stored beyond the queue, logged, or echoed in events.
    ///
    /// # Errors
    /// [`StationError::InvalidDigit`] if the command is empty or any character
    /// is not a DTMF key (nothing sent); [`StationError::DtmfBusy`] while a
    /// previous sequence is still playing; [`StationError::NotConnected`] if
    /// no call is active (nothing queued).
    pub fn send_dtmf_string(&self, digits: &str) -> Result<(), StationError> {
        let keys: Vec<char> = digits.chars().collect();
        if keys.is_empty() || !keys.iter().all(|&c| is_dtmf_key(c)) {
            return Err(StationError::InvalidDigit);
        }
        let mut queue = self.dtmf_queue.lock().unwrap();
        if queue.is_some() {
            return Err(StationError::DtmfBusy);
        }
        // First digit synchronously: connectivity failures surface here and
        // nothing is queued on error. (Lock order dtmf_queue → session holds:
        // send_dtmf_for takes the session lock only.)
        self.send_dtmf_for(keys[0], DEFAULT_DTMF_MS)?;
        *queue = Some(DtmfQueue {
            digits: keys,
            played: 1,
            next_due: std::time::Instant::now()
                + std::time::Duration::from_millis(u64::from(
                    DEFAULT_DTMF_MS + DEFAULT_DTMF_GAP_MS,
                )),
        });
        Ok(())
    }

    /// Drop the un-played remainder of a [`Station::send_dtmf_string`]
    /// command (iax-4b7a). The digit currently sounding finishes its tone.
    /// Safe to call when nothing is playing. Also invoked by
    /// [`Station::disconnect`] so a torn-down call never leaks digits into a
    /// later one.
    pub fn cancel_dtmf(&self) {
        *self.dtmf_queue.lock().unwrap() = None;
    }

    /// Progress of the active sequence: `(played, total)`, `(0, 0)` when none.
    fn dtmf_progress(&self) -> (u32, u32) {
        match self.dtmf_queue.lock().unwrap().as_ref() {
            Some(q) => (q.played, u32::try_from(q.digits.len()).unwrap_or(u32::MAX)),
            None => (0, 0),
        }
    }

    /// Advance the active sequence, if any (iax-4b7a). Called from
    /// [`Station::snapshot`]; a no-op when no sequence is playing. Pumps the
    /// announce queue while active (which also releases the in-band tone's
    /// auto-key once its injection drains), sends the next digit when its
    /// tone+gap slot arrives, and retires the queue after the last digit's
    /// slot elapses. A send failure mid-sequence (call went away) drops the
    /// remainder — the UI sees progress reset to 0/0.
    fn pump_dtmf(&self) {
        let mut guard = self.dtmf_queue.lock().unwrap();
        if guard.is_none() {
            return;
        }
        // Keep the announce path draining while a sequence is live (lock
        // order dtmf_queue → session, consistent with send_dtmf_string).
        self.session.lock().unwrap().poll_announcements();
        let Some(q) = guard.as_mut() else { return };
        if std::time::Instant::now() < q.next_due {
            return;
        }
        if (q.played as usize) >= q.digits.len() {
            *guard = None; // last digit's tone+gap elapsed — sequence complete
            return;
        }
        let digit = q.digits[q.played as usize];
        match self.send_dtmf_for(digit, DEFAULT_DTMF_MS) {
            Ok(()) => {
                q.played += 1;
                q.next_due = std::time::Instant::now()
                    + std::time::Duration::from_millis(u64::from(
                        DEFAULT_DTMF_MS + DEFAULT_DTMF_GAP_MS,
                    ));
            }
            Err(_) => *guard = None,
        }
    }

    /// Play an announcement on the active call. Node-or-WT agnostic, but the
    /// node daemon is the intended caller (iax-da05).
    ///
    /// # Errors
    /// [`StationError::NotConnected`] if no call is active.
    pub fn announce(&self, req: astar_iax::AnnounceRequest) -> Result<(), StationError> {
        self.session
            .lock()
            .unwrap()
            .announce(req)
            .map(|_handle| ()) // pump drives completion via poll_announcements
            .map_err(map_console_err)
    }

    /// Play a private announcement to the active call's conference-member leg
    /// (iax-c4ea): the per-user node-id join greeting, heard by that one joining
    /// user only. The active call must be enrolled as a conference member
    /// (Bridge/Conference mode).
    ///
    /// # Errors
    /// [`StationError::NotConnected`] if no call is active; [`StationError::Iax`]
    /// if the leg is not a conference member or the phrase cannot be resolved.
    pub fn announce_to_member(&self, req: astar_iax::AnnounceRequest) -> Result<(), StationError> {
        self.session
            .lock()
            .unwrap()
            .announce_to_active_member(req)
            .map_err(map_console_err)
    }

    /// Advance announcement queues (call from the node pump).
    pub fn poll_announcements(&self) {
        self.session.lock().unwrap().poll_announcements();
    }

    /// Enroll a receive-only external conference member — the attachment point
    /// for the Discord bridge. The member receives the mix and contributes
    /// nothing; see `Manager::add_bridge_member`.
    ///
    /// `None` when no `Manager` has been built yet (the engine is lazy — see
    /// [`Self::enable_inbound`]/[`Self::connect`]) or the current bridge
    /// topology is not a plain mix-minus conference.
    ///
    /// `.unwrap()` on the lock is deliberate, matching this file's dominant
    /// idiom (`announce_to_member` and 50+ other call sites): a poisoned
    /// session mutex means the node is already broken, and must panic loudly
    /// rather than be reported as the benign "no conference right now" `None`.
    pub fn add_bridge_member(&self) -> Option<astar_iax::BridgeMemberHandle> {
        self.session.lock().unwrap().add_bridge_member()
    }

    /// Whether a member [`Station::add_bridge_member`] enrolled is still in
    /// the current conference (a Handset switch drops it).
    #[must_use]
    pub fn has_bridge_member(&self, id: astar_audio::MemberId) -> bool {
        self.session.lock().unwrap().has_bridge_member(id)
    }

    /// Withdraw a member enrolled by [`Station::add_bridge_member`].
    pub fn remove_bridge_member(&self, id: astar_audio::MemberId) {
        self.session.lock().unwrap().remove_bridge_member(id);
    }

    /// Number of members currently enrolled in the mix-minus conference,
    /// including any [`Station::add_bridge_member`] enrollees. `0` with no
    /// `Manager` built yet. Introspection for tests that need to prove a
    /// member was (or was not) actually withdrawn.
    #[must_use]
    pub fn conference_member_count(&self) -> usize {
        self.session.lock().unwrap().conference_member_count()
    }

    /// Speak a private announcement to the Discord bridge's conference member.
    /// Reaches that member only — never the air.
    ///
    /// # Errors
    /// [`StationError::Iax`] if no `Manager` has been built yet, the member id
    /// is unknown to the conference, or the phrase cannot be resolved.
    pub fn announce_to_bridge_member(
        &self,
        member: astar_audio::MemberId,
        req: astar_iax::AnnounceRequest,
        lead: std::time::Duration,
    ) -> Result<(), StationError> {
        self.session
            .lock()
            .unwrap()
            .announce_to_bridge_member(member, req, lead)
            .map_err(map_console_err)
    }

    /// Answer a ringing inbound call (Node/Manual).
    ///
    /// # Errors
    /// [`StationError::NoPendingCall`] in WT mode or when nothing is ringing.
    pub fn answer(&self) -> Result<(), StationError> {
        if self.mode() != OperatingMode::Node {
            return Err(StationError::NoPendingCall);
        }
        self.session
            .lock()
            .unwrap()
            .answer_pending()
            .map_err(|e| match e {
                // No parked offer → no pending call.
                astar_console::ConsoleError::NotConnected => StationError::NoPendingCall,
                other => map_console_err(other),
            })
    }

    /// Reject a ringing inbound call (Node/Manual).
    ///
    /// # Errors
    /// [`StationError::NoPendingCall`] in WT mode or when nothing is ringing.
    pub fn reject(&self) -> Result<(), StationError> {
        if self.mode() != OperatingMode::Node {
            return Err(StationError::NoPendingCall);
        }
        self.session
            .lock()
            .unwrap()
            .reject_pending()
            .map_err(|e| match e {
                astar_console::ConsoleError::NotConnected => StationError::NoPendingCall,
                other => map_console_err(other),
            })
    }

    /// Set the input (TX/mic) gain multiplier (clamped `[0.0, 4.0]`).
    pub fn set_input_gain(&self, gain: f32) {
        self.session.lock().unwrap().set_input_gain(gain);
    }

    /// Set the output (RX/speaker) gain multiplier (clamped `[0.0, 4.0]`:
    /// 100%-400% headroom for boosting a quiet station, iax-a4e7).
    pub fn set_output_gain(&self, gain: f32) {
        self.session.lock().unwrap().set_output_gain(gain);
    }

    /// Toggle mic voice compression on the live/next call (WT/console path).
    /// Takes effect immediately on an active call's capture lane.
    pub fn set_compression(&self, on: bool) {
        self.session.lock().unwrap().set_compress(on);
    }

    /// Set the mic voice-compression strength (`0.0..=1.0`, clamped) on the
    /// live/next call. `0.0` = light, `1.0` = most aggressive; the default is
    /// `0.90`. Takes effect immediately when compression is enabled.
    pub fn set_compression_level(&self, level: f32) {
        self.session.lock().unwrap().set_compression_level(level);
    }

    /// Set the neural denoise strength (`0.0..=1.0`, clamped; default
    /// `1.0` = full) on the live/next call. `0.0` is a bypass.
    ///
    /// RNNoise has no strength parameter, so this is a delay-compensated
    /// dry/wet mix inside the stage. It does nothing while the classical
    /// chain is running — see `docs/design/noise-suppression.md`.
    pub fn set_denoise_strength(&self, level: f32) {
        self.session.lock().unwrap().set_denoise_strength(level);
    }

    /// Set the TX trim gain (`0.0..=4.0`, clamped; default `1.0` = unity) on
    /// the live/next call: the always-on FINAL TX gain stage, applied after the
    /// compressor so it attenuates a hot mic that compression makeup gain would
    /// otherwise keep loud (iax-750a). Values above `1.0` boost (clamped at
    /// full scale). Takes effect immediately.
    pub fn set_tx_trim(&self, gain: f32) {
        self.session.lock().unwrap().set_tx_trim(gain);
    }

    /// Toggle RX/output compression on the live/next call (iax-a4e7 PHASE 1):
    /// automatic leveling of the RECEIVED audio — reuses the mic-path
    /// compressor (makeup gain included) on the output bus, applied BEFORE
    /// the output gain multiply so the 100%-400% output-gain range amplifies
    /// the already-leveled signal. Shared across networks (output is
    /// listener-side). Takes effect immediately on an active call's output
    /// bus.
    pub fn set_rx_compression(&self, on: bool) {
        self.session.lock().unwrap().set_rx_compress(on);
    }

    /// Set the RX/output compression strength (`0.0..=1.0`, clamped) on the
    /// live/next call. `0.0` = light, `1.0` = most aggressive; the default is
    /// `0.90`. Takes effect immediately when RX compression is enabled
    /// (iax-a4e7 PHASE 1).
    pub fn set_rx_compression_level(&self, level: f32) {
        self.session.lock().unwrap().set_rx_compression_level(level);
    }

    /// Configure the RX jitter buffer (iax-rxjb): whether received audio is
    /// played out of the adaptive buffer at all, and the window
    /// (`min_ms`..=`max_ms`) its depth may live in.
    ///
    /// The defaults are Asterisk `chan_iax2`'s — on, 40 ms of slack over
    /// measured jitter, a 200 ms ceiling — because the node at the other end
    /// of an `AllStarLink` call is Asterisk. Bigger `min_ms` = more latency,
    /// fewer holes. Both bounds are clamped to `0..=500` ms and a `max_ms`
    /// under `min_ms` is raised to meet it: a setting is repaired, never
    /// refused. Takes effect immediately, mid-call, with no reconnect;
    /// `snapshot()` reports the effective values as `rx_jb_enabled` /
    /// `rx_jb_min_ms` / `rx_jb_max_ms`.
    pub fn set_rx_jitter(&self, enabled: bool, min_ms: u32, max_ms: u32) {
        self.session
            .lock()
            .unwrap()
            .set_rx_jitter(astar_audio::RxJitterConfig {
                enabled,
                min_ms,
                max_ms,
            });
    }

    /// Toggle mic noise reduction (denoise) on the live/next call (WT/console
    /// path). Takes effect immediately on an active call's capture lane.
    pub fn set_noise_reduction(&self, on: bool) {
        self.session.lock().unwrap().set_denoise(on);
    }

    /// Set the VOX pre-roll / look-back length (ms, clamped to `0..=250`) on the
    /// live/next call (iax-2733). When software VOX keys the call, the lane
    /// flushes this much buffered mic audio AHEAD of the live stream so the
    /// speech onset isn't clipped. `0` (the default) disables pre-roll — astar
    /// opts in from its `inputDB`-driven VOX edge (iax-5c30).
    pub fn set_vox_preroll_ms(&self, ms: u32) {
        self.session.lock().unwrap().set_vox_preroll_ms(ms);
    }

    /// Monitor THIS device (iax-2377): open the input device and run the mic lane
    /// WITHOUT a call (no µ-law encode, no TX), so a front-end can preview /
    /// characterize the mic before dialing. `input` is the capture-device
    /// substring (`None`/empty = system default).
    ///
    /// Call it again to change mics: asking for a **different** resolved device
    /// stops the running monitor and opens the new one (a front-end's device
    /// picker must be able to move the stream, not just its label); asking for
    /// the device already monitored is a no-op. Still call-safe: a **no-op** if
    /// a call is already active (the device is already open on the call's mic
    /// lane — never double-open it). Stop it with [`Station::monitor_stop`].
    ///
    /// The device is resolved BEFORE the running monitor is touched, so a name
    /// that resolves to nothing is an error that leaves the current monitor
    /// running.
    ///
    /// # Errors
    /// [`StationError::Audio`] if the device can't be resolved or opened.
    pub fn monitor_start(&self, input: Option<&str>) -> Result<(), StationError> {
        // No-op when a call is live: the device is already open on the call's
        // mic lane; opening it again would contend with CoreAudio.
        if self.session.lock().unwrap().is_active() {
            return Ok(());
        }
        let backend = (self.make_backend)();
        let device =
            astar_console::resolve_device(backend.as_ref(), input, astar_audio::Direction::Input)
                .map_err(|e| StationError::Audio(e.to_string()))?;
        let mut slot = self.monitor.lock().unwrap();
        if slot.as_ref().is_some_and(|(current, _)| current == &device) {
            return Ok(()); // already monitoring THIS device — idempotent
        }
        // A different device: drop the running monitor first so the two never
        // overlap on one capture device, then open the new one.
        *slot = None;
        let backend = (self.make_backend)();
        let monitor =
            astar_audio::MicMonitor::start(backend, &device, astar_audio::StreamConfig::default())
                .map_err(|e| StationError::Audio(e.to_string()))?;
        *slot = Some((device, monitor));
        Ok(())
    }

    /// The resolved capture-device name the mic monitor is open on, or `None`
    /// when not monitoring — the mic the analyzer is actually listening to,
    /// which is not always the one a picker last named (see
    /// [`Station::monitor_start`]). Engine- and test-side only for now: it is on
    /// neither the C ABI nor the Swift binding, so a front-end that wants to
    /// display it needs that binding first.
    #[must_use]
    pub fn monitor_input(&self) -> Option<String> {
        self.monitor
            .lock()
            .unwrap()
            .as_ref()
            .map(|(device, _)| device.clone())
    }

    /// Stop monitor mode and release the input device. Idempotent (no-op if not
    /// monitoring).
    pub fn monitor_stop(&self) {
        // Drop off-lock: dropping the MicMonitor closes the cpal stream, which
        // can briefly block on a CoreAudio mutex.
        let monitor = self.monitor.lock().unwrap().take();
        drop(monitor);
    }

    /// `true` while monitor mode is running (the input device is open without a
    /// call).
    #[must_use]
    pub fn is_monitoring(&self) -> bool {
        self.monitor.lock().unwrap().is_some()
    }

    /// The monitor lane's continuous mic input level (dBFS), or `None` when not
    /// monitoring. Companion to [`Station::mic_spectrum`]: same lane, scalar
    /// level instead of bins. Drives the VOX calibration meter, which runs with
    /// no call up.
    #[must_use]
    pub fn monitor_input_dbfs(&self) -> Option<f32> {
        self.monitor
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|(_, mon)| mon.input_dbfs())
    }

    /// Copy the live voice-band mic spectrum (iax-e73e) into `out` and return the
    /// number of log-binned dBFS values written (`0` if not monitoring). Poll
    /// this ~20 Hz to draw the spectrum. The bins are log-spaced over the voice
    /// band (~100 Hz..3.9 kHz) and peak-held so a steady whine stays visible.
    pub fn mic_spectrum(&self, out: &mut [f32]) -> usize {
        match self.monitor.lock().unwrap().as_ref() {
            Some((_, mon)) => mon.spectrum_into(out),
            None => 0,
        }
    }

    /// Copy the live-call TX spectrum (iax-2b09) into `out` and return the number
    /// of log-binned dBFS values written (`0` if no active call / unrouted mic).
    /// Unlike [`Station::mic_spectrum`] (a no-call mic-monitor view), this taps
    /// the ACTIVE call's mic lane post-DSP, pre-encode PCM — the audio going out
    /// to the node. Same log bins / peak-hold as the mic monitor; poll ~20 Hz.
    /// A pure observer that never perturbs the call or the mic-monitor device.
    pub fn tx_spectrum(&self, out: &mut [f32]) -> usize {
        self.session.lock().unwrap().tx_spectrum(out)
    }

    /// Copy the live-call RX spectrum (iax-2b09) into `out` and return the number
    /// of log-binned dBFS values written (`0` if no active call). Taps the
    /// ACTIVE call's output bus post-mix decoded PCM — the audio received from
    /// the node. Same log bins / peak-hold as the mic monitor; poll ~20 Hz.
    /// A pure observer.
    pub fn rx_spectrum(&self, out: &mut [f32]) -> usize {
        self.session.lock().unwrap().rx_spectrum(out)
    }

    /// Set the spectrum peak-hold decay in dB/SECOND (iax-8616), clamped to a
    /// sane range internally (`1..=500`). The default is 100 dB/s. This drives
    /// the fall-rate of the peak-held spectrum bars; a larger value makes peaks
    /// track downward changes faster, a smaller value holds them longer.
    ///
    /// The decay is shared by all three live analyzers — the mic monitor
    /// ([`Station::mic_spectrum`]), the live-call TX ([`Station::tx_spectrum`]),
    /// and the live-call RX ([`Station::rx_spectrum`]) — so a single call scrubs
    /// every visible spectrum at once. It applies to the analyzers that are
    /// CURRENTLY live: the mic monitor if monitoring, and the active call's TX/RX
    /// if a call is up. Analyzers created later (start monitoring / dial after
    /// this call) come up at the 100 dB/s default; re-apply after they exist to
    /// scrub them too.
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        // Mic monitor (if running).
        if let Some((_, mon)) = self.monitor.lock().unwrap().as_ref() {
            mon.set_spectrum_decay(db_per_sec);
        }
        // Active call TX + RX analyzers (if a call is up).
        self.session.lock().unwrap().set_spectrum_decay(db_per_sec);
    }

    /// Characterize the monitored mic (iax-5fb6): run `characterize()` over the
    /// buffered monitor-mode silence and return the resulting [`MicProfile`]
    /// (secret-free — plain DSP numbers). Call after a few seconds of monitored
    /// silence. `harmonic_comb` enables harmonic-aware notch detection
    /// (**default-off** at the FFI; a learned-fundamental comb that catches
    /// rolled-off upper harmonics). Returns `None` if not currently monitoring.
    #[must_use]
    pub fn characterize(&self, harmonic_comb: bool) -> Option<astar_audio::MicProfile> {
        self.characterize_with(astar_audio::CharacterizeOpts {
            harmonic_comb,
            ..astar_audio::CharacterizeOpts::default()
        })
    }

    /// Characterize the monitored mic with explicit
    /// [`astar_audio::CharacterizeOpts`] — the harmonic-comb toggle plus the
    /// peak margin (how far above the spectral-median floor a tone must stand
    /// to be worth notching). A mic with nothing above its floor at that margin
    /// characterizes as a pass-through profile, which changes nothing in the
    /// mic lane. Returns `None` if not currently monitoring.
    #[must_use]
    pub fn characterize_with(
        &self,
        opts: astar_audio::CharacterizeOpts,
    ) -> Option<astar_audio::MicProfile> {
        self.monitor
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, mon)| mon.characterize(opts))
    }

    /// Apply (or clear) a calibrated per-mic profile (iax-2095). A recalled
    /// profile is pushed onto the session so the LIVE call's `NoiseReducer` comb
    /// is rebuilt from it (and it seeds the next call). Pass `None` to clear back
    /// to the generic noise reducer. The [`astar_audio::MicProfile`] is
    /// secret-free — plain DSP numbers (high-pass, notch list, floor, gate).
    pub fn set_mic_profile(&self, profile: Option<astar_audio::MicProfile>) {
        self.session.lock().unwrap().set_calibrated(profile);
    }

    /// Drain one queued lifecycle event, or `None`. Derived by diffing the
    /// pollable console state (the console has no separate event queue): each
    /// call takes a fresh snapshot and emits at most one edge per call —
    /// `Answered` on first transition to answered, `RemotePtt(b)` on a
    /// remote-ptt change, `Hangup{reason}` on first transition to
    /// hangup/failed. Remaining diffs surface on subsequent calls.
    #[must_use]
    pub fn next_event(&self) -> Option<StationEvent> {
        let snap = self.snapshot();

        // Node/Manual: a newly parked inbound offer surfaces once. This is a
        // session-tracked edge (not a snapshot diff). snapshot() already released
        // the session lock; the EventTracker lock is not yet held — no deadlock.
        if self.mode() == OperatingMode::Node
            && let Some(from) = self.session.lock().unwrap().take_incoming_from()
        {
            return Some(StationEvent::IncomingCall { from });
        }

        // Re-register supervision (iax-177d): if a previous registration
        // failed and its backoff has elapsed, start a fresh attempt. The
        // config is secret-free; `do_register` re-mints the credential via
        // the resolver. Pre-arm the next (doubled) backoff — a success clears
        // it below, so only sustained failure keeps climbing the ladder.
        let retry_cfg = {
            let mut s = self.reg_supervise.lock().unwrap();
            match (&s.cfg, s.retry) {
                (Some(cfg), Some((due, next))) if std::time::Instant::now() >= due => {
                    let cfg = cfg.clone();
                    s.retry = Some((
                        std::time::Instant::now() + next,
                        (next * 2).min(REG_RETRY_MAX),
                    ));
                    // Try the NEXT candidate. A registrar hostname is often
                    // several hosts that do not all answer a given source
                    // address, and the failure is silent — REGREQs go out,
                    // nothing comes back — so retrying the same one forever
                    // is an outage rather than a retry.
                    s.peer_index = s.peer_index.wrapping_add(1);
                    Some(cfg)
                }
                _ => None,
            }
        };
        if let Some(cfg) = retry_cfg {
            // A spawn error leaves the pre-armed retry in place — the next
            // poll tries again.
            let _ = self.do_register(&cfg);
        }

        // Registration edges (Task 3.1): drain one outcome from the session's
        // reg_queue. These are available in any mode (register/deregister are
        // mode-independent; set_mode(Node) auto-starts registration when config
        // includes a RegisterConfig). Secret-free: RegisterOutcome never carries
        // the credential.
        if let Some(outcome) = self.session.lock().unwrap().take_register_event() {
            return Some(match outcome {
                RegisterOutcome::Registered => {
                    // Success ends the supervision backoff (iax-177d). The
                    // candidate that worked is left selected: it is the one
                    // answering, and re-registration should keep using it.
                    self.reg_supervise.lock().unwrap().retry = None;
                    StationEvent::Registered
                }
                RegisterOutcome::Failed(reason) => {
                    // iax-177d: the registration thread is gone. Schedule a
                    // re-register (5 s, doubling to 60 s) unless deregister()
                    // ended supervision. Without this, one failed refresh
                    // round silently dropped the node from the directory
                    // until the next process restart.
                    {
                        let mut s = self.reg_supervise.lock().unwrap();
                        if s.cfg.is_some() && s.retry.is_none() {
                            s.retry = Some((
                                std::time::Instant::now() + REG_RETRY_INITIAL,
                                (REG_RETRY_INITIAL * 2).min(REG_RETRY_MAX),
                            ));
                        }
                    }
                    StationEvent::RegisterFailed { reason }
                }
            });
        }

        let mut t = self.last_event_state.lock().unwrap();

        // Operating-mode change (edge).
        if snap.mode != t.mode {
            t.mode = snap.mode;
            return Some(StationEvent::ModeChanged(snap.mode));
        }

        // Answered (edge). iax-a82f: use the session's monotonic answered
        // counter, not a status-boolean latch. In a multi-call node (parrot/
        // conference) the single `status` stays `Answered` across sequential
        // callers — call N's hangup is lost when call N+1 replaces the
        // session's one event slot — so a status-only edge fired the join
        // greeting on the FIRST caller only. `answered_seq` bumps once per
        // newly-answered call (inbound adopt AND WT dial), so each caller
        // gets a fresh edge.
        if snap.answered_seq != t.answered_seq {
            t.answered_seq = snap.answered_seq;
            // Only a rising edge into an answered call is a join; a bump that
            // coincides with a non-answered snapshot (already torn down) is not.
            if matches!(snap.status, CallStatus::Answered) {
                return Some(StationEvent::Answered);
            }
        }

        // Remote PTT (edge).
        if snap.remote_ptt != t.remote_ptt {
            t.remote_ptt = snap.remote_ptt;
            return Some(StationEvent::RemotePtt(snap.remote_ptt));
        }

        // Hangup / Failed (edge).
        match &snap.status {
            CallStatus::Hangup { reason } | CallStatus::Failed { reason } => {
                if !t.hung_up {
                    t.hung_up = true;
                    return Some(StationEvent::Hangup {
                        reason: reason.clone(),
                    });
                }
            }
            CallStatus::Idle | CallStatus::Dialing => {
                t.hung_up = false;
            }
            CallStatus::Answered => {}
        }
        None
    }
}

/// Prints only what is safe to print: the selected devices and, when
/// compiled in, the `EchoLink` state. Neither of them
/// carries a secret, and no field of `Station` holds one (secrets are
/// in-arguments only). The session is read with `try_lock`, so printing a
/// station never waits on, or deadlocks against, a held session lock.
impl std::fmt::Debug for Station {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Station");
        if let Ok(devices) = self.devices.try_lock() {
            d.field("devices", &*devices);
        }
        #[cfg(feature = "echolink")]
        if let Ok(session) = self.session.try_lock() {
            d.field("echolink", &session.echolink_state());
        }
        d.finish_non_exhaustive()
    }
}

impl Drop for Station {
    fn drop(&mut self) {
        // Finish an open recording first (`disconnect` does too; idempotent).
        let _ = self.record_stop();
        // Hang up any live call. `disconnect()` itself now also clears any
        // live/failed M17 session (iax-f2b8-fix Fix 1), so the explicit call
        // below is a defensive, idempotent no-op kept for clarity at the
        // Drop site rather than relied upon.
        self.disconnect();
        self.m17_disconnect();
        self.dstar_disconnect();
        self.ysf_disconnect();
        self.nxdn_disconnect();
        self.dmr_disconnect();
        // Hang up every EchoLink station and log off the directory. The
        // login is taken out under the session lock and logged off after it
        // is released (it can wait ~80 s on an exchange in flight), so a
        // drop never holds the lock that long, and the session never logs
        // off in its own drop.
        self.echolink_logout();
        // Stop monitor mode (releases the input device).
        self.monitor_stop();
        // Stop outbound registration (sends REGREL, joins the thread).
        self.deregister();
        // Stop the inbound listener (its Drop stops the actor thread).
        self.stop_inbound();
    }
}

/// Clone a [`NodeConfig`] field-by-field (it isn't `Clone`: its
/// `IncomingCallPolicy` holds a non-`Clone`-derived secret map).
fn clone_node_config(c: &NodeConfig) -> NodeConfig {
    NodeConfig {
        bind: c.bind,
        policy: crate::config::clone_policy(&c.policy),
        answer: c.answer,
        register: c.register.clone(),
        max_calls: c.max_calls,
        allowlist: c.allowlist.clone(),
    }
}

/// The operator's BrandMeister opt-in, as this build knows it: **`false`**.
///
/// One constant rather than a literal at the call site, so the thing that has
/// to change when consent becomes settable is findable by name. Nothing in
/// astar can set it today — there is no preference, no config field and no C
/// ABI for it — which is the deliberate outcome of iax-d4f7's Task 2 review:
/// the gate exists and defaults closed, and opening it is its own piece of
/// work with its own UI, because what it opens is a risk to the operator's
/// account on somebody else's private network.
///
/// Not feature-gated: `Station::dmr_connect` refuses BrandMeister identically
/// with the `dmr` feature off, so an operator gets one answer rather than two.
const BRANDMEISTER_CONSENTED: bool = false;

fn map_console_err(e: astar_console::ConsoleError) -> StationError {
    use astar_console::ConsoleError as C;
    match e {
        C::AlreadyConnected => StationError::AlreadyConnected,
        C::NotConnected => StationError::NotConnected,
        C::Resolve { .. } => StationError::Iax("resolve".into()),
        C::Audio(a) => StationError::Audio(a.to_string()),
        C::Device(d) => StationError::Audio(d),
        C::Iax(i) => StationError::Iax(i.to_string()),
        C::Link(m) => StationError::Link(m),
        C::EchoLink(m) => StationError::EchoLink(m),
        C::M17(m) => StationError::M17(m),
        C::Dstar(m) => StationError::Dstar(m),
        C::Ysf(m) => StationError::Ysf(m),
        C::Nxdn(m) => StationError::Nxdn(m),
        C::Dmr(m) => StationError::Dmr(m),
        C::NoCaptureDevice => StationError::Audio("no capture device: cannot transmit".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With this crate's `echolink` feature off, the facade says so, even
    /// though a test build's dev-dependency turns astar-console's on.
    #[cfg(not(feature = "echolink"))]
    #[test]
    fn echolink_off_reports_unavailable_and_stubs_every_call() {
        let station = Station::with_backend_factory(
            StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        assert!(!station.echolink_available());
        let not_compiled = |r: Result<(), StationError>| matches!(r, Err(StationError::EchoLink(ref m)) if m == ECHOLINK_NOT_COMPILED);
        assert!(not_compiled(station.echolink_login(
            "KX7ABC",
            "pw".to_owned(),
            &["127.0.0.1:5200".to_owned()]
        )));
        assert!(not_compiled(station.echolink_connect("9999")));
        assert!(not_compiled(station.echolink_send_chat("hi")));
        assert!(not_compiled(station.echolink_node_connect("9999")));
        assert!(not_compiled(station.echolink_node_disconnect("KX7ABC")));
        assert!(station.echolink_state().is_none());
        assert!(station.echolink_last_error().is_none());
        assert!(station.echolink_stations().is_empty());
        assert!(station.echolink_node_poll().is_empty());
        assert!(station.echolink_node_peers().is_empty());
        station.echolink_set_location("Denver");
        station.echolink_disconnect();
        station.echolink_logout();
    }

    // --- recording: a stop inside record_start's unlocked window ---------

    /// A stop (or a disconnect, which stops first) landing while
    /// `record_start` is making its recorder with the lock released must
    /// win: once both return, no recorder runs and no app tap is installed,
    /// and the last state says `recording: false`. The barrier seam holds
    /// the start inside the window, so the stop lands there every run.
    #[cfg(feature = "m17")]
    #[test]
    fn a_stop_inside_the_start_window_wins() {
        use std::sync::Barrier;
        // An M17 session is a connection the moment it exists; the silent
        // 127.0.0.1 socket never answers, which is all this needs.
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = target.local_addr().expect("addr");
        let station = Station::with_backend_factory(
            StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        station
            .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
            .expect("m17 connect");
        let dir =
            std::env::temp_dir().join(format!("astar-station-startwin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        station.recording.lock().unwrap().start_pause = Some((Arc::clone(&a), Arc::clone(&b)));

        std::thread::scope(|sc| {
            let start = sc.spawn(|| {
                station.record_start(RecordRequest {
                    dir: dir.clone(),
                    layout: Layout::Stereo,
                    frames: true,
                    label: "M17 test".to_owned(),
                    utc_offset_secs: 0,
                })
            });
            a.wait(); // the start has released the lock
            assert!(matches!(
                station.record_start(RecordRequest {
                    dir: dir.clone(),
                    layout: Layout::Stereo,
                    frames: false,
                    label: "M17 test".to_owned(),
                    utc_offset_secs: 0,
                }),
                Err(StationError::Record(m)) if m == "already recording"
            ));
            let stopped = station.record_stop();
            assert!(!stopped.recording, "the stop reports not recording");
            assert!(!station.record_state().recording);
            b.wait(); // let the start go on
            start
                .join()
                .expect("start thread")
                .expect("a start stopped in its window still returns Ok");
        });

        let slot = station.recording.lock().unwrap();
        assert!(slot.recorder.is_none(), "no recorder outlives the stop");
        assert!(!slot.busy() && !slot.stop_requested, "flags are cleared");
        assert!(
            slot.last.as_ref().is_some_and(|l| !l.recording),
            "the stopped recorder's final state is the last one"
        );
        drop(slot);
        {
            let s = station.session.lock().unwrap();
            let taps = s.record_taps();
            assert!(
                taps.heard.is_none() && taps.sent.is_none() && taps.frames.is_none(),
                "no app tap is installed"
            );
        }
        assert!(!station.record_state().recording);
        station.disconnect();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The busy flags are cleared even if the window unwinds.
    #[test]
    fn a_busy_guard_clears_the_flags_on_unwind() {
        let slot = Mutex::new(RecordSlot::default());
        slot.lock().unwrap().starting = true;
        slot.lock().unwrap().stop_requested = true;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = BusyGuard::new(&slot);
            panic!("inside the window");
        }));
        assert!(r.is_err());
        let s = slot.lock().unwrap();
        assert!(!s.busy() && !s.stop_requested);
    }

    // --- iax-f2b8-fix Fix 2: snapshot's m17_available must be dirs-aware ---

    /// Deterministic, hermetic proof that `snapshot()`/`try_snapshot()` read
    /// the dirs-aware `Station::m17_available()` override rather than
    /// `ConsoleSession::snapshot`'s dirs-blind `astar_console::m17_available()`
    /// mirror (a process-wide `OnceLock` — effectively always `true` in this
    /// crate's own test builds, which force the `codec2-static` fallback on).
    /// Priming `codec2_probe_cache` directly sidesteps needing a real
    /// (un)available libcodec2 on the machine running the test: it forces
    /// `Station::m17_available()` to `false` for the station's current
    /// (empty) `codec_dirs`, while the console-level mirror stays `true` —
    /// exactly the divergence the fix must resolve.
    #[cfg(feature = "m17")]
    #[test]
    fn snapshot_mirrors_the_dirs_aware_override_not_the_process_wide_cache() {
        let station = Station::with_backend_factory(
            StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        // Prime the station's OWN memoized probe to `false` for the current
        // (empty) `codec_dirs` — a cache hit, so no real codec2 probe runs.
        *station.codec2_probe_cache.lock().unwrap() = Some((Vec::new(), false));
        assert!(
            !station.m17_available(),
            "the primed cache must be read back verbatim"
        );

        assert!(
            !station.snapshot().m17_available,
            "snapshot() must mirror Station::m17_available()'s dirs-aware override, \
             not astar_console::m17_available()'s dirs-blind process cache"
        );
        assert!(
            !station
                .try_snapshot()
                .expect("uncontended lock")
                .m17_available,
            "try_snapshot() must apply the same override"
        );
    }

    // --- iax-7fff: the DTMF emission switch, at the layer that owns it ---

    /// The byte-identical-default proof: with the default mode, `plan_dtmf`
    /// produces exactly the PCM the pre-iax-7fff `send_dtmf_for` synthesized
    /// (same generator, same rate, same level, same clamped duration), destined
    /// for the same announce-path injection.
    #[test]
    fn default_mode_plans_the_exact_pre_switch_tone() {
        for (digit, ms) in [('5', DEFAULT_DTMF_MS), ('#', 120), ('A', 10)] {
            let plan = plan_dtmf(
                DtmfMode::default(),
                digit,
                ms,
                astar_codec::dtmf::DTMF_SAMPLE_RATE,
            )
            .expect("valid digit");
            let expected = astar_codec::dtmf::synthesize(
                digit,
                astar_codec::dtmf::DTMF_SAMPLE_RATE,
                ms.max(MIN_DTMF_MS),
                astar_codec::dtmf::DEFAULT_TONE_DBFS,
            )
            .expect("valid digit");
            assert_eq!(
                plan,
                DtmfEmission::Tone(expected),
                "default mode must synthesize today's exact in-band tone for {digit}",
            );
        }
    }

    #[test]
    fn in_band_tone_is_synthesized_at_the_live_pipeline_rate() {
        // iax-8d2f: a fixed-8k tone played on a 16 kHz pipeline comes out at
        // double pitch and is undecodable. Round-trip proof: at EACH rate the
        // planned PCM must decode with a rate-matched Goertzel detector.
        for rate in [8_000_u32, 16_000] {
            let plan = plan_dtmf(DtmfMode::InBand, '5', 200, rate).expect("valid digit");
            let DtmfEmission::Tone(pcm) = plan else {
                panic!("in-band must plan a tone");
            };
            assert_eq!(
                pcm.len(),
                rate as usize / 5,
                "200 ms of samples at {rate} Hz"
            );
            let mut det = astar_audio::DtmfDetector::new(rate);
            let block = rate as usize / 50; // 20 ms blocks, conference-style
            let mut decoded = None;
            for chunk in pcm.chunks(block) {
                let f: Vec<f32> = chunk.iter().map(|&s| f32::from(s) / 32768.0).collect();
                if let Some(d) = det.feed(&f) {
                    decoded = Some(d);
                }
            }
            assert_eq!(decoded, Some('5'), "tone at {rate} Hz must decode as '5'");
        }
    }

    #[test]
    fn protocol_mode_plans_frames_and_synthesizes_no_tone() {
        let plan = plan_dtmf(DtmfMode::Protocol, '7', DEFAULT_DTMF_MS, 8_000).expect("valid digit");
        assert_eq!(
            plan,
            DtmfEmission::Protocol('7'),
            "protocol mode hands the digit to the engine, no PCM",
        );
    }

    #[test]
    fn plan_rejects_invalid_digits_in_both_modes() {
        for mode in [DtmfMode::InBand, DtmfMode::Protocol] {
            assert!(
                matches!(
                    plan_dtmf(mode, 'x', DEFAULT_DTMF_MS, 8_000),
                    Err(StationError::InvalidDigit)
                ),
                "{mode:?} must reject a non-keypad character",
            );
        }
    }

    #[test]
    fn station_enrolls_a_receive_only_bridge_member() {
        let st = Station::with_backend_factory(
            StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        // `add_bridge_member` needs a live `Manager`, and the Station only
        // builds one lazily (on connect, or here, on the inbound listener
        // starting) — exactly the sequence the node daemon runs in practice
        // (it always brings the listener up before wiring the bridge).
        // Loopback, ephemeral port: no traffic leaves 127.0.0.1.
        st.enable_inbound(crate::config::InboundConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..crate::config::InboundConfig::default()
        })
        .expect("inbound listener starts and builds the engine");
        st.set_bridge_config(astar_iax::BridgeConfig {
            mode: astar_iax::BridgeMode::Bridge,
            mix_minus: true,
            include_local_radio: false,
            ..Default::default()
        })
        .unwrap();

        let h = st.add_bridge_member().expect("bridge mode enrolls");
        assert!(h.sample_rate == 8000 || h.sample_rate == 16000);
        st.remove_bridge_member(h.id);
    }
}
