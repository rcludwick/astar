// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use astar_audio::AudioBackend;
use astar_console::{
    CallStatus, ConsoleConfig, ConsoleSession, ConsoleState, LinkConnectSpec, OperatingMode,
    RegisterOutcome,
};
use astar_iax::{CallMode, LinkEvent, LinkMode, LinkRoster};
use astar_iax_core::session::auth::Secret;

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

/// An IAX2 station: owns the single-call session, audio, and the ASL3 connect
/// recipe. PTT-source-agnostic: callers drive keying via [`Station::set_ptt`];
/// hardware PTT lives in `astar-ptt` / `astar-serial-sys`. Internally
/// thread-safe (the session sits behind an
/// `Arc<Mutex<>>`), so every method takes `&self`.
///
/// Vendor-neutral: [`Station::connect`] is the generic IAX2 path (works against
/// stock Asterisk); [`Station::connect_wt`] is one `AllStar` convenience method.
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
    /// Mic monitor (iax-2377): opens the input device WITHOUT a call so a
    /// front-end can preview/characterize the mic. `Some` only while monitoring.
    /// Never started while a call is active (the device would be double-opened);
    /// `Drop` (on `monitor_stop`) releases the device.
    monitor: Mutex<Option<astar_audio::MicMonitor>>,
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
}

/// Supervision state for automatic re-registration (iax-177d). Without it a
/// single failed refresh round killed the registration thread permanently and
/// the node silently fell out of the `AllStar` directory.
#[derive(Default)]
struct RegSupervise {
    cfg: Option<RegisterConfig>,
    retry: Option<(std::time::Instant, std::time::Duration)>,
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
    /// Create a station with the real `CpalBackend`.
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
            link_events_buf: Mutex::new(std::collections::VecDeque::new()),
            reg_supervise: Mutex::new(RegSupervise::default()),
            dtmf_mode: Mutex::new(DtmfMode::default()),
            dtmf_queue: Mutex::new(None),
            codec_dirs: Mutex::new(Vec::new()),
            #[cfg(feature = "m17")]
            codec2_probe_cache: Mutex::new(None),
        };
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
            link_events_buf: Mutex::new(std::collections::VecDeque::new()),
            reg_supervise: Mutex::new(RegSupervise::default()),
            dtmf_mode: Mutex::new(DtmfMode::default()),
            dtmf_queue: Mutex::new(None),
            codec_dirs: Mutex::new(Vec::new()),
            #[cfg(feature = "m17")]
            codec2_probe_cache: Mutex::new(None),
        };
        if configured_mode == OperatingMode::Node {
            let _ = station.set_mode(OperatingMode::Node);
        }
        station
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
                input,
                output,
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
                .m17_connect(backend, cfg)
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

    /// Connect to a D-Star `DExtra` reflector, full-transceive (iax-a9d4
    /// Task 6 built RX; iax-2f6b adds TX): resolves `host`/`port`, validates
    /// `module` (uppercased; must land on `A`-`Z`) and a non-empty
    /// `callsign`, and opens the session over the station's currently
    /// selected input+output devices (mirrors [`Station::m17_connect`]'s
    /// device resolution exactly).
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
    /// The `ThumbDV` probe/init handshake and the audio-device open both run
    /// with the session mutex NOT held: the session is constructed first and
    /// then installed via `ConsoleSession::dstar_adopt`. A slow or flaky
    /// dongle therefore delays only the caller of this method; concurrent
    /// [`Station::snapshot`]/`dstar_state`/[`Station::set_ptt`] polls — which
    /// all take that same mutex — are unaffected, per the `AstarStation`
    /// poll-and-snapshot contract.
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
                output,
                input,
                // The station-level signature stays primitive-args only (see
                // this method's doc), so the destination reflector's callsign
                // — which fills the TX header's RPT1/RPT2 — is derived from
                // `host` (`xrf757.…` → `XRF757`); connecting by bare IP
                // transmits with those fields blank. See
                // `astar_console::dstar`'s `tx_repeater_fields`.
                reflector_callsign: None,
            };
            // Refuse early and cheaply when something else already owns the
            // session: the lock is held for a few instructions here, not for
            // the whole ThumbDV probe.
            self.session
                .lock()
                .unwrap()
                .dstar_can_connect()
                .map_err(map_console_err)?;

            // The slow part, deliberately OFF the session mutex: a
            // candidate-port scan, then per candidate and per baud rate an
            // open plus an eight-transaction init cookbook (each bounded at
            // 300 ms), plus the audio-output open — seconds, on a flaky
            // dongle. Holding the mutex across that blocked every
            // snapshot/state poll for the whole window.
            let backend = std::cell::RefCell::new(Some((self.make_backend)()));
            let session = astar_console::DstarSession::connect(cfg, &move || {
                backend
                    .borrow_mut()
                    .take()
                    .expect("dstar backend factory called exactly once")
            })
            .map_err(map_console_err)?;

            // Re-takes the lock only to install (and to re-check exclusion,
            // since the state could have changed while it was released).
            self.session
                .lock()
                .unwrap()
                .dstar_adopt(session)
                .map_err(map_console_err)
        }
        #[cfg(not(feature = "dstar"))]
        {
            let _ = (host, port);
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
            astar_console::dstar_available()
        }
        #[cfg(not(feature = "dstar"))]
        {
            false
        }
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
            cfg.policy,
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
        }
        self.do_register(&attempt)
    }

    /// Resolve the secret and start the registration thread, WITHOUT touching
    /// the supervision state (shared by [`Station::register`] and the retry
    /// path in [`Station::next_event`]).
    fn do_register(&self, cfg: &RegisterConfig) -> Result<(), StationError> {
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
            .start_register(cfg.peer, cfg.username.clone(), cfg.refresh, secret)
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
                        crate::config::clone_policy(&cfg.policy),
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
        snap
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

    /// Set the input (TX/mic) gain multiplier (clamped `[0.0, 2.0]`).
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

    /// Set the TX trim gain (`0.0..=2.0`, clamped; default `1.0` = unity) on
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

    /// Start monitor mode (iax-2377): open the input device and run the mic lane
    /// WITHOUT a call (no µ-law encode, no TX), so a front-end can preview /
    /// characterize the mic before dialing. `input` is the capture-device
    /// substring (`None`/empty = system default).
    ///
    /// Idempotent and call-safe: a **no-op** if a call is already active (the
    /// device is already open on the call's mic lane — never double-open it) or
    /// if a monitor is already running. Stop it with [`Station::monitor_stop`].
    ///
    /// # Errors
    /// [`StationError::Audio`] if the device can't be resolved or opened.
    pub fn monitor_start(&self, input: Option<&str>) -> Result<(), StationError> {
        // No-op when a call is live: the device is already open on the call's
        // mic lane; opening it again would contend with CoreAudio.
        if self.session.lock().unwrap().is_active() {
            return Ok(());
        }
        let mut slot = self.monitor.lock().unwrap();
        if slot.is_some() {
            return Ok(()); // already monitoring — idempotent
        }
        let backend = (self.make_backend)();
        let device =
            astar_console::resolve_device(backend.as_ref(), input, astar_audio::Direction::Input)
                .map_err(|e| StationError::Audio(e.to_string()))?;
        let backend = (self.make_backend)();
        let monitor =
            astar_audio::MicMonitor::start(backend, &device, astar_audio::StreamConfig::default())
                .map_err(|e| StationError::Audio(e.to_string()))?;
        *slot = Some(monitor);
        Ok(())
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
            .and_then(astar_audio::MicMonitor::input_dbfs)
    }

    /// Copy the live voice-band mic spectrum (iax-e73e) into `out` and return the
    /// number of log-binned dBFS values written (`0` if not monitoring). Poll
    /// this ~20 Hz to draw the spectrum. The bins are log-spaced over the voice
    /// band (~100 Hz..3.9 kHz) and peak-held so a steady whine stays visible.
    pub fn mic_spectrum(&self, out: &mut [f32]) -> usize {
        match self.monitor.lock().unwrap().as_ref() {
            Some(mon) => mon.spectrum_into(out),
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
        if let Some(mon) = self.monitor.lock().unwrap().as_ref() {
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
        self.monitor
            .lock()
            .unwrap()
            .as_ref()
            .map(|mon| mon.characterize(astar_audio::CharacterizeOpts { harmonic_comb }))
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
                    // Success ends the supervision backoff (iax-177d).
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

impl Drop for Station {
    fn drop(&mut self) {
        // Hang up any live call. `disconnect()` itself now also clears any
        // live/failed M17 session (iax-f2b8-fix Fix 1), so the explicit call
        // below is a defensive, idempotent no-op kept for clarity at the
        // Drop site rather than relied upon.
        self.disconnect();
        self.m17_disconnect();
        self.dstar_disconnect();
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
        C::M17(m) => StationError::M17(m),
        C::Dstar(m) => StationError::Dstar(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
