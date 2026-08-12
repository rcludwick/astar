// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `M17Session`: a full-transceive M17 reflector client runtime (iax-f2b8
//! Task 3).
//!
//! Unlike [`crate::session::ConsoleSession`] (IAX2/`AllStar`), M17 has no
//! separate call-setup handshake or protocol-level PTT frame: a `CONN`/`ACKN`
//! exchange with a reflector brings the link up, and transmission is carried
//! entirely by voice-stream packets (start of a stream = key-down, the `EOS`
//! bit = key-up). [`M17Session`] owns:
//!
//! - its own [`AudioRouter`] (the `MicMonitor` pattern, see
//!   [`astar_audio::monitor`]) — mutual exclusion with a live IAX2 call is
//!   enforced later, at the `ConsoleSession`/station level (Task 4), not here;
//! - a [`SessionFsm`] (link state + keepalive);
//! - a [`Codec2Voice`] instance (Codec 2 mode 3200, the rate M17 payloads
//!   use);
//! - ONE run-loop thread ("iax-m17") that owns the `UdpSocket` (plain,
//!   `set_read_timeout(50ms)` — no mio, no async) and drives all of the
//!   above.
//!
//! The control-side [`M17Session`] handle talks to the run-loop thread only
//! through a small set of atomics (poll-cheap, per [`M17SnapshotState`]) plus
//! a request flag for PTT — never a shared/locked `AudioRouter` or `Codec2Voice`,
//! so those stay single-threaded (owned entirely by the run-loop) with no
//! cross-thread synchronization on the hot audio path.
//!
//! # RX jitter handling (documented choice)
//!
//! This milestone's RX path is a simple in-order pass-through: packets are
//! decoded and forwarded to [`astar_audio::router::CallAudio::rx_frames`]
//! in arrival order, with no reordering/jitter-smoothing buffer. `receiving`
//! reflects "a stream packet has arrived within the last 400 ms", not
//! anything about buffer health. The loopback tests exercise same-process
//! UDP (negligible jitter/reordering), so this is sufficient to validate the
//! milestone; a `astar_codec::jitter::JitterBuf`-backed reorder stage is
//! the natural follow-on hardening before this ships against a real
//! Internet-routed reflector.

use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioRouter, CallAudio, MicId, MicProfile, OutputId, StreamConfig,
};
use astar_codec::codec2::Codec2Voice;
use astar_m17::{
    BROADCAST, ControlPacket, FsmAction, LinkState, Lsf, SessionFsm, StreamPacket, encode_callsign,
};

use crate::session::{ConsoleError, resolve_device};

/// How long the RX path waits, after the last voice-stream packet, before
/// clearing [`M17SnapshotState::receiving`] back to `false`.
const RX_SILENCE_TIMEOUT: Duration = Duration::from_millis(400);

/// The run-loop thread's socket read timeout: also the cadence at which PTT
/// edges, the FSM keepalive tick, and the RX silence timeout are all
/// re-checked. 50 ms per the Task 3 design brief (a plain read-timeout loop,
/// not mio/async).
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Operator-supplied configuration for an [`M17Session`].
pub struct M17Config {
    /// Reflector hostname or IP address.
    pub host: String,
    /// Reflector UDP port (the M17 IP-framing default is 17000, but this is
    /// caller-supplied — no protocol default is assumed here).
    pub port: u16,
    /// Reflector module letter (e.g. `b'A'`).
    pub module: u8,
    /// This station's callsign (encoded via [`encode_callsign`]; invalid
    /// callsigns fail [`M17Session::connect`] with [`ConsoleError::Device`]).
    pub callsign: String,
    /// Capture device substring; `None` = system default (mirrors
    /// [`resolve_device`]'s contract).
    pub input: Option<String>,
    /// Playback device substring; `None` = system default.
    pub output: Option<String>,
    /// Extra directories to search for a runtime `libcodec2` (ahead of the
    /// hard-coded system paths); see [`astar_codec::open_codec2`].
    pub codec_dirs: Vec<std::path::PathBuf>,
    /// How long to wait, with no packet from the reflector, before declaring
    /// the link [`LinkState::Failed`]. Default (via a fresh [`SessionFsm`])
    /// is 30 s; tests shorten this to avoid a 30-real-second wait.
    pub keepalive_timeout: Duration,
}

/// The audio DSP prefs to seed a freshly-opened M17 router with (iax-f2b8-fix
/// Fix 4): mirrors the standing-pref re-push
/// [`crate::session::ConsoleSession::connect`] does for an IAX2 dial
/// (originally 8 prefs; iax-a4e7 PHASE 1 adds RX compression, a 10-pref
/// re-push). Built by [`crate::session::ConsoleSession::m17_connect`]
/// from its own standing pref cells and applied directly onto the new
/// [`AudioRouter`] in [`M17Session::connect`] — `Station::m17_connect` builds
/// [`M17Config`] without ever seeing those cells, so without this a fresh M17
/// link silently reverted to the router's bare defaults (unity gain, DSP off)
/// no matter what the operator had already dialed in for IAX2/WT calls.
#[derive(Clone)]
pub struct M17Prefs {
    /// TX (mic/input) gain multiplier.
    pub input_gain: f32,
    /// RX (speaker/output) gain multiplier.
    pub output_gain: f32,
    /// Capture noise-reduction toggle.
    pub denoise: bool,
    /// Capture compression toggle.
    pub compress: bool,
    /// Compressor strength (0.0..=1.0).
    pub compress_level: f32,
    /// TX trim (0.0..=2.0; 1.0 = unity), the always-on final gain stage.
    pub tx_trim: f32,
    /// RX/output compression toggle (iax-a4e7 PHASE 1): automatic leveling of
    /// the received audio.
    pub rx_compress: bool,
    /// RX/output compression strength (0.0..=1.0).
    pub rx_compress_level: f32,
    /// VOX pre-roll / look-back length in ms (0 = disabled).
    pub vox_preroll_ms: u32,
    /// Calibrated per-mic profile, if one is set.
    pub calibrated: Option<MicProfile>,
}

/// A poll-cheap snapshot of an [`M17Session`]'s live state. Backed by atomics
/// on the control side — [`M17Session::state`] never blocks on the run-loop
/// thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct M17SnapshotState {
    /// Current reflector link state.
    pub link: LinkState,
    /// `true` while transmit is keyed (mirrors the last [`M17Session::set_ptt`]
    /// applied by the run-loop; the apply is bounded by one
    /// [`SOCKET_POLL_TIMEOUT`] tick, ~50 ms).
    pub ptt: bool,
    /// `true` while voice-stream packets have arrived from the reflector
    /// within the last 400 ms.
    pub receiving: bool,
    /// Current TX level in dBFS (post-DSP mic peak; see
    /// [`AudioRouter::mic_tx_dbfs`]).
    pub tx_dbfs: f32,
    /// Current RX level in dBFS (post-mix output-bus peak; see
    /// [`AudioRouter::output_rx_dbfs`]).
    pub rx_dbfs: f32,
    /// The mic capture gain CURRENTLY applied on the router's open lane (iax-
    /// f2b8-fix Fix 4): read back from [`AudioRouter::mic_gain`] every
    /// run-loop tick, i.e. round-tripped through the real router state rather
    /// than just echoing back whatever [`M17Session::set_mic_gain`] was last
    /// called with. Test-visible proof the pref actually reached the router.
    pub applied_mic_gain: f32,
    /// The output gain CURRENTLY applied on the router's open bus. Same
    /// round-trip contract as `applied_mic_gain`; see
    /// [`AudioRouter::output_gain`].
    pub applied_output_gain: f32,
    /// The RX/output compression toggle CURRENTLY applied on the router's
    /// open bus (iax-a4e7 PHASE 1). Same round-trip contract as
    /// `applied_mic_gain`; see [`AudioRouter::output_compress`].
    pub applied_rx_compress: bool,
    /// The RX/output compression strength CURRENTLY applied on the router's
    /// open bus (iax-a4e7 PHASE 1). Same round-trip contract as
    /// `applied_mic_gain`; see [`AudioRouter::output_compress_level`].
    pub applied_rx_compress_level: f32,
    /// Current mic INPUT level in dBFS (iax-f2b8-fix Fix 6): post-gain,
    /// pre-`NoiseReducer`, metered CONTINUOUSLY on the router's mic lane
    /// EVEN WHILE UNKEYED — mirrors [`AudioRouter::mic_input_dbfs`] exactly,
    /// so a consumer's VOX can key from silence the same way the IAX2 path
    /// already does via `ConsoleState::input_level_db`.
    pub input_dbfs: f32,
}

/// Atomics shared between the control-side [`M17Session`] and its run-loop
/// thread. `link`/`tx_dbfs`/`rx_dbfs` are written by the run-loop and read by
/// [`M17Session::state`]; `ptt`/`receiving` follow the same direction.
struct SharedState {
    link: AtomicU8,
    ptt: AtomicBool,
    receiving: AtomicBool,
    /// `f32` bits (`f32::to_bits`/`from_bits`) — dBFS TX peak.
    tx_dbfs: AtomicU32,
    /// `f32` bits — dBFS RX peak.
    rx_dbfs: AtomicU32,
    /// `f32` bits — mic gain read back from the router every tick (Fix 4).
    applied_mic_gain: AtomicU32,
    /// `f32` bits — output gain read back from the router every tick (Fix 4).
    applied_output_gain: AtomicU32,
    /// RX/output compression toggle read back from the router every tick
    /// (iax-a4e7 PHASE 1, mirrors Fix 4's `applied_output_gain`).
    applied_rx_compress: AtomicBool,
    /// `f32` bits — RX/output compression strength read back from the router
    /// every tick (iax-a4e7 PHASE 1).
    applied_rx_compress_level: AtomicU32,
    /// `f32` bits — mic INPUT level (dBFS) read back from the router every
    /// tick (Fix 6), continuously, independent of `keyed`.
    input_dbfs: AtomicU32,
}

impl SharedState {
    fn new() -> Self {
        // -60 dBFS is this codebase's "silent" floor (see
        // `astar_audio::peak_to_dbfs`); seed both meters there so a
        // snapshot taken before the run-loop's first meter read reports
        // silence rather than 0 dBFS (full scale). Gains seed at unity (1.0)
        // for the same reason: a snapshot taken before the first tick reports
        // the router's actual pre-tick default, not an arbitrary sentinel.
        Self {
            link: AtomicU8::new(link_to_u8(LinkState::Idle)),
            ptt: AtomicBool::new(false),
            receiving: AtomicBool::new(false),
            tx_dbfs: AtomicU32::new((-60.0f32).to_bits()),
            rx_dbfs: AtomicU32::new((-60.0f32).to_bits()),
            applied_mic_gain: AtomicU32::new(1.0f32.to_bits()),
            applied_output_gain: AtomicU32::new(1.0f32.to_bits()),
            applied_rx_compress: AtomicBool::new(false),
            applied_rx_compress_level: AtomicU32::new(0.90f32.to_bits()),
            input_dbfs: AtomicU32::new((-60.0f32).to_bits()),
        }
    }

    fn snapshot(&self) -> M17SnapshotState {
        M17SnapshotState {
            link: u8_to_link(self.link.load(Ordering::Relaxed)),
            ptt: self.ptt.load(Ordering::Relaxed),
            receiving: self.receiving.load(Ordering::Relaxed),
            tx_dbfs: f32::from_bits(self.tx_dbfs.load(Ordering::Relaxed)),
            rx_dbfs: f32::from_bits(self.rx_dbfs.load(Ordering::Relaxed)),
            applied_mic_gain: f32::from_bits(self.applied_mic_gain.load(Ordering::Relaxed)),
            applied_output_gain: f32::from_bits(self.applied_output_gain.load(Ordering::Relaxed)),
            applied_rx_compress: self.applied_rx_compress.load(Ordering::Relaxed),
            applied_rx_compress_level: f32::from_bits(
                self.applied_rx_compress_level.load(Ordering::Relaxed),
            ),
            input_dbfs: f32::from_bits(self.input_dbfs.load(Ordering::Relaxed)),
        }
    }
}

/// Live TX/RX spectrum bins, shared between the control-side [`M17Session`]
/// and the run-loop thread (iax-f2b8-fix Fix 6): refreshed every run-loop
/// tick from the router's own analyzers ([`AudioRouter::mic_tx_spectrum`]/
/// [`AudioRouter::output_rx_spectrum`]), mirroring the `applied_*_gain`
/// round-trip pattern from Fix 4. A `Mutex` (not atomics), since a spectrum
/// is a fixed-size array, not a scalar; the lock is only ever held for a
/// cheap fixed-size copy, never across a blocking call.
struct SharedSpectrum {
    /// `(bins, count)` — `count` is `0` before the router's mic lane has
    /// ever produced a reading (mirrors [`AudioRouter::mic_tx_spectrum`]'s
    /// own "`None`/`0` if the lane isn't open" contract rather than
    /// reporting stale/zeroed data as if it were real).
    tx: std::sync::Mutex<([f32; astar_audio::SPECTRUM_BINS], usize)>,
    /// Same contract as `tx`, for the router's output bus.
    rx: std::sync::Mutex<([f32; astar_audio::SPECTRUM_BINS], usize)>,
}

impl SharedSpectrum {
    fn new() -> Self {
        Self {
            tx: std::sync::Mutex::new(([0.0; astar_audio::SPECTRUM_BINS], 0)),
            rx: std::sync::Mutex::new(([0.0; astar_audio::SPECTRUM_BINS], 0)),
        }
    }
}

/// Live-adjustable audio DSP prefs the control side can push at any time
/// (iax-f2b8-fix Fix 4): the run-loop re-applies all of them onto its
/// `AudioRouter` every poll tick, the same "control side stores an atomic;
/// the run-loop applies it on its next ~50 ms poll" pattern [`M17Session::set_ptt`]
/// already uses. `profile`/`profile_gen` follow a dirty-flag instead (a
/// `MicProfile` clone every 50 ms is needless work for something that changes
/// rarely): the run-loop only re-applies it when `profile_gen` has moved past
/// what it last applied.
struct SharedPrefs {
    /// `f32` bits — mic capture gain.
    mic_gain: AtomicU32,
    /// `f32` bits — output gain.
    output_gain: AtomicU32,
    denoise: AtomicBool,
    compress: AtomicBool,
    /// `f32` bits — compressor strength (0.0..=1.0).
    compress_level: AtomicU32,
    /// `f32` bits — TX trim (0.0..=2.0; 1.0 = unity).
    tx_trim: AtomicU32,
    /// RX/output compression toggle (iax-a4e7 PHASE 1).
    rx_compress: AtomicBool,
    /// `f32` bits — RX/output compression strength (0.0..=1.0).
    rx_compress_level: AtomicU32,
    preroll_ms: AtomicU32,
    profile: std::sync::Mutex<Option<MicProfile>>,
    /// Bumped on every [`M17Session::set_calibrated`] call; the run-loop
    /// tracks the last generation it applied and re-applies only on change.
    profile_gen: AtomicU32,
    /// `f32` bits — spectrum peak-hold decay, dB/SECOND (iax-f2b8-fix Fix 6).
    /// Unlike the other fields here, this has NO corresponding [`M17Prefs`]
    /// field: [`crate::session::ConsoleSession::set_spectrum_decay`] is
    /// itself a "live-only" setter with no standing/persisted cell (applies
    /// only to CURRENTLY live analyzers, never re-pushed at connect time —
    /// see its own doc comment), so this seeds at the analyzers' own
    /// built-in default and only ever changes via
    /// [`M17Session::set_spectrum_decay`].
    spectrum_decay: AtomicU32,
}

impl SharedPrefs {
    fn new(prefs: &M17Prefs) -> Self {
        Self {
            mic_gain: AtomicU32::new(prefs.input_gain.to_bits()),
            output_gain: AtomicU32::new(prefs.output_gain.to_bits()),
            denoise: AtomicBool::new(prefs.denoise),
            compress: AtomicBool::new(prefs.compress),
            compress_level: AtomicU32::new(prefs.compress_level.to_bits()),
            tx_trim: AtomicU32::new(prefs.tx_trim.to_bits()),
            rx_compress: AtomicBool::new(prefs.rx_compress),
            rx_compress_level: AtomicU32::new(prefs.rx_compress_level.to_bits()),
            preroll_ms: AtomicU32::new(prefs.vox_preroll_ms),
            profile: std::sync::Mutex::new(prefs.calibrated.clone()),
            // Starts at 1 (not 0, the run-loop's initial `last_applied_profile_gen`):
            // the initial profile is applied directly, synchronously, in
            // `M17Session::connect` before the run-loop thread ever starts
            // (so it's live from the very first TX frame, not just "eventually,
            // once the first tick lands"); this generation exists purely to
            // signal LIVE changes via `set_calibrated` afterward.
            profile_gen: AtomicU32::new(1),
            spectrum_decay: AtomicU32::new(
                astar_audio::spectrum::DEFAULT_DECAY_DB_PER_SEC.to_bits(),
            ),
        }
    }

    /// Apply every pref onto `router`'s open mic lane / output bus. Called
    /// once synchronously at connect time (before the run-loop thread starts)
    /// and again every run-loop tick thereafter — cheap (a handful of atomic
    /// loads plus the router's own atomic stores), so no dirty-flag tracking
    /// is needed for anything but the heavier `profile` clone.
    fn apply(&self, router: &AudioRouter, mic: &MicId, out: &OutputId, last_applied_gen: &mut u32) {
        router.set_mic_gain(mic, f32::from_bits(self.mic_gain.load(Ordering::Relaxed)));
        router.set_output_gain(
            out,
            f32::from_bits(self.output_gain.load(Ordering::Relaxed)),
        );
        router.set_mic_denoise(mic, self.denoise.load(Ordering::Relaxed));
        router.set_mic_compress(mic, self.compress.load(Ordering::Relaxed));
        router.set_mic_compress_level(
            mic,
            f32::from_bits(self.compress_level.load(Ordering::Relaxed)),
        );
        router.set_mic_tx_trim(mic, f32::from_bits(self.tx_trim.load(Ordering::Relaxed)));
        router.set_output_compress(out, self.rx_compress.load(Ordering::Relaxed));
        router.set_output_compress_level(
            out,
            f32::from_bits(self.rx_compress_level.load(Ordering::Relaxed)),
        );
        router.set_mic_preroll_ms(mic, self.preroll_ms.load(Ordering::Relaxed));
        let decay = f32::from_bits(self.spectrum_decay.load(Ordering::Relaxed));
        router.set_mic_spectrum_decay(mic, decay);
        router.set_output_spectrum_decay(out, decay);

        let current_gen = self.profile_gen.load(Ordering::Relaxed);
        if current_gen != *last_applied_gen {
            let profile = self.profile.lock().expect("profile mutex").clone();
            router.set_mic_profile(mic, profile);
            *last_applied_gen = current_gen;
        }
    }
}

fn link_to_u8(s: LinkState) -> u8 {
    match s {
        LinkState::Idle => 0,
        LinkState::Connecting => 1,
        LinkState::Linked => 2,
        LinkState::Failed => 3,
    }
}

fn u8_to_link(v: u8) -> LinkState {
    match v {
        1 => LinkState::Connecting,
        2 => LinkState::Linked,
        3 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

/// A full-transceive M17 reflector client: connects, keys/unkeys transmit,
/// and decodes received voice — see the module docs for the architecture.
pub struct M17Session {
    /// `Some` until [`M17Session::disconnect`] (or `Drop`) joins it.
    thread: Option<JoinHandle<()>>,
    /// Set to request the run-loop thread send `DISC` and exit. The 50 ms
    /// socket read timeout bounds how long a join can take.
    shutdown: Arc<AtomicBool>,
    /// The last [`M17Session::set_ptt`] request; the run-loop applies it (and
    /// does TX stream bookkeeping) on its next poll.
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
    /// Live-adjustable audio DSP prefs (iax-f2b8-fix Fix 4); see
    /// [`SharedPrefs`].
    prefs: Arc<SharedPrefs>,
    /// Live TX/RX spectrum bins (iax-f2b8-fix Fix 6); see [`SharedSpectrum`].
    spectrum: Arc<SharedSpectrum>,
}

impl M17Session {
    /// Connect to an M17 reflector: resolves the configured audio devices,
    /// opens this session's own [`AudioRouter`] call (mirrors the
    /// `MicMonitor` pattern — no sharing with any IAX2 call), opens a Codec 2
    /// instance, binds a UDP socket, and starts the "iax-m17" run-loop
    /// thread, which sends the initial `CONN`.
    ///
    /// `make_backend` is called exactly once, synchronously, before this
    /// returns (mirrors [`crate::session::ConsoleSession::connect`]'s
    /// backend-factory contract). `prefs` (iax-f2b8-fix Fix 4) is applied
    /// onto the fresh router SYNCHRONOUSLY, before this returns — a fresh M17
    /// link starts with the operator's already-configured volume/DSP prefs
    /// live from the very first frame, not just "eventually, once the
    /// run-loop's first poll tick lands".
    ///
    /// # Errors
    /// [`ConsoleError::Device`] for an invalid callsign, unresolvable audio
    /// device, or missing Codec 2 backend; [`ConsoleError::Audio`] if opening
    /// the call's audio streams fails; [`ConsoleError::Resolve`] if
    /// `cfg.host`/`cfg.port` don't resolve or the socket can't be bound.
    // `cfg` is taken by value per the Task 3 interface contract (matches
    // `ConsoleSession::connect`'s own `ConsoleConfig`-by-value shape); every
    // field is read out (cloned/copied/borrowed) rather than moved, which is
    // why clippy would otherwise suggest a reference here.
    #[allow(clippy::needless_pass_by_value)]
    pub fn connect(
        cfg: M17Config,
        prefs: M17Prefs,
        make_backend: &dyn Fn() -> Box<dyn AudioBackend>,
    ) -> Result<M17Session, ConsoleError> {
        let callsign = encode_callsign(&cfg.callsign).ok_or_else(|| {
            ConsoleError::Device(format!("invalid M17 callsign {:?}", cfg.callsign))
        })?;

        // Resolve devices against the backend BEFORE it moves into the
        // router (mirrors ConsoleSession::connect's iax-be48 idiom, reusing
        // the same public `resolve_device` helper).
        let backend = make_backend();
        let in_id = resolve_device(
            backend.as_ref(),
            cfg.input.as_deref(),
            astar_audio::Direction::Input,
        )?;
        let out_id = resolve_device(
            backend.as_ref(),
            cfg.output.as_deref(),
            astar_audio::Direction::Output,
        )?;

        let mut router = AudioRouter::new(backend);
        let mic = MicId::new(&in_id);
        let out = OutputId::new(&out_id);
        // 8 kHz mono 20 ms: the StreamConfig::default() rate Codec 2 mode
        // 3200 (and thus M17) is built around.
        let config = StreamConfig::default();
        let call_audio = router
            .open_call(&mic, &out, config)
            .map_err(ConsoleError::Audio)?;
        // WARNING (per astar_audio::AudioRouter::open_call): open_call
        // keys the gate immediately. Un-key right away; M17Session::set_ptt
        // is the only thing allowed to key it from here on.
        router.set_gate(&mic, false);

        // Fix 4: apply the operator's already-configured volume/DSP prefs
        // onto this fresh router BEFORE the run-loop thread ever starts —
        // see the SharedPrefs::apply doc comment for why the profile
        // generation counter starts at 1 (matching the run-loop's initial
        // `last_applied_profile_gen`, so this synchronous apply isn't
        // redundantly repeated on the very first tick).
        let shared_prefs = Arc::new(SharedPrefs::new(&prefs));
        let mut last_applied_profile_gen = 1;
        shared_prefs.apply(&router, &mic, &out, &mut last_applied_profile_gen);

        let (codec, _backend) =
            astar_codec::codec2::open_codec2(&cfg.codec_dirs).ok_or_else(|| {
                ConsoleError::Device(
                    "codec2 unavailable: no runtime libcodec2 found and codec2-static not enabled"
                        .to_string(),
                )
            })?;

        let socket = connect_udp_socket(&cfg.host, cfg.port)?;

        let fsm = SessionFsm::with_keepalive_timeout(callsign, cfg.module, cfg.keepalive_timeout);

        let shutdown = Arc::new(AtomicBool::new(false));
        let ptt_request = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState::new());
        let spectrum = Arc::new(SharedSpectrum::new());

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_ptt_request = Arc::clone(&ptt_request);
        let thread_shared = Arc::clone(&shared);
        let thread_prefs = Arc::clone(&shared_prefs);
        let thread_spectrum = Arc::clone(&spectrum);
        let handle = std::thread::Builder::new()
            .name("iax-m17".to_string())
            .spawn(move || {
                run_loop(RunLoopParams {
                    socket,
                    fsm,
                    router,
                    mic,
                    out,
                    call_audio,
                    codec,
                    callsign,
                    shutdown: thread_shutdown,
                    ptt_request: thread_ptt_request,
                    shared: thread_shared,
                    prefs: thread_prefs,
                    last_applied_profile_gen,
                    spectrum: thread_spectrum,
                });
            })
            .map_err(|e| ConsoleError::Resolve {
                node: cfg.host.clone(),
                source: e,
            })?;

        Ok(M17Session {
            thread: Some(handle),
            shutdown,
            ptt_request,
            shared,
            prefs: shared_prefs,
            spectrum,
        })
    }

    /// Engage/release transmit. M17 carries PTT purely via stream
    /// start/`EOS` (there is no protocol PTT frame): the run-loop starts a
    /// fresh random `StreamID` on the next key-down edge it observes, and
    /// flushes a final `EOS`-marked packet on key-up. Applied by the
    /// run-loop on its next poll (bounded by [`SOCKET_POLL_TIMEOUT`], ~50 ms)
    /// — this call itself never blocks.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt_request.store(on, Ordering::Relaxed);
    }

    // --- live DSP pref passthroughs (iax-f2b8-fix Fix 4) --------------------
    //
    // Every setter below just stores an atomic; the run-loop applies it onto
    // the router on its next poll (bounded by `SOCKET_POLL_TIMEOUT`, ~50 ms) —
    // exactly `set_ptt`'s own contract, applied to the DSP prefs instead of
    // the keying edge. `&self` (not `&mut self`, unlike `set_ptt`): mirrors
    // `ConsoleSession`'s own pref setters, which are `&self` so they can be
    // called from a shared reference.

    /// Set the mic (TX/input) capture gain on the live M17 router lane.
    pub fn set_mic_gain(&self, g: f32) {
        self.prefs.mic_gain.store(g.to_bits(), Ordering::Relaxed);
    }

    /// Set the output (RX/speaker) gain on the live M17 router bus.
    pub fn set_output_gain(&self, g: f32) {
        self.prefs.output_gain.store(g.to_bits(), Ordering::Relaxed);
    }

    /// Toggle capture noise reduction on the live M17 router lane.
    pub fn set_denoise(&self, on: bool) {
        self.prefs.denoise.store(on, Ordering::Relaxed);
    }

    /// Toggle capture compression on the live M17 router lane.
    pub fn set_compress(&self, on: bool) {
        self.prefs.compress.store(on, Ordering::Relaxed);
    }

    /// Set the capture compression strength on the live M17 router lane.
    pub fn set_compression_level(&self, level: f32) {
        self.prefs
            .compress_level
            .store(level.to_bits(), Ordering::Relaxed);
    }

    /// Set the TX trim on the live M17 router lane.
    pub fn set_tx_trim(&self, g: f32) {
        self.prefs.tx_trim.store(g.to_bits(), Ordering::Relaxed);
    }

    /// Toggle RX/output compression on the live M17 router bus (iax-a4e7
    /// PHASE 1): automatic leveling of the received audio.
    pub fn set_rx_compress(&self, on: bool) {
        self.prefs.rx_compress.store(on, Ordering::Relaxed);
    }

    /// Set the RX/output compression strength on the live M17 router bus
    /// (iax-a4e7 PHASE 1).
    pub fn set_rx_compression_level(&self, level: f32) {
        self.prefs
            .rx_compress_level
            .store(level.to_bits(), Ordering::Relaxed);
    }

    /// Set the VOX pre-roll length (ms) on the live M17 router lane.
    pub fn set_vox_preroll_ms(&self, ms: u32) {
        self.prefs.preroll_ms.store(ms, Ordering::Relaxed);
    }

    /// Push a calibrated per-mic profile onto the live M17 router lane.
    pub fn set_calibrated(&self, profile: Option<MicProfile>) {
        *self.prefs.profile.lock().expect("profile mutex") = profile;
        self.prefs.profile_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Set the live spectrum peak-hold decay (dB/SECOND) on the M17 router's
    /// TX (mic lane) + RX (output bus) analyzers (iax-f2b8-fix Fix 6).
    /// "Live-only" — see [`SharedPrefs::spectrum_decay`]'s doc comment for
    /// why there's no connect-time seed from a persisted
    /// `ConsoleSession` cell (there isn't one).
    pub fn set_spectrum_decay(&self, db_per_sec: f32) {
        self.prefs
            .spectrum_decay
            .store(db_per_sec.to_bits(), Ordering::Relaxed);
    }

    /// Copy the live TX spectrum (iax-f2b8-fix Fix 6) into `out` and return
    /// the number of log-binned dBFS bins written (`0` before the router's
    /// mic lane has produced its first reading). Refreshed every run-loop
    /// tick from [`AudioRouter::mic_tx_spectrum`] — the SAME log-binned,
    /// peak-held dBFS values the IAX2 path's `Manager::tx_spectrum` produces.
    #[must_use]
    pub fn tx_spectrum(&self, out: &mut [f32]) -> usize {
        let (bins, count) = *self.spectrum.tx.lock().expect("tx spectrum mutex");
        let n = count.min(out.len());
        out[..n].copy_from_slice(&bins[..n]);
        n
    }

    /// Copy the live RX spectrum (iax-f2b8-fix Fix 6) into `out` and return
    /// the number of bins written (`0` before the router's output bus has
    /// produced its first reading). Refreshed every run-loop tick from
    /// [`AudioRouter::output_rx_spectrum`].
    #[must_use]
    pub fn rx_spectrum(&self, out: &mut [f32]) -> usize {
        let (bins, count) = *self.spectrum.rx.lock().expect("rx spectrum mutex");
        let n = count.min(out.len());
        out[..n].copy_from_slice(&bins[..n]);
        n
    }

    /// A poll-cheap snapshot of the session's current state.
    #[must_use]
    pub fn state(&self) -> M17SnapshotState {
        self.shared.snapshot()
    }

    /// Disconnect: requests the run-loop send `DISC` and exit, then joins the
    /// thread. The 50 ms socket read timeout bounds the join.
    pub fn disconnect(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Resolves `host:port` and returns a connected, read-timeout-armed
/// [`UdpSocket`] ready for the run-loop — the [`M17Session::connect`] half of
/// the iax-m17-localhost fix, split out to keep `connect` under Clippy's
/// line-count lint.
///
/// `to_socket_addrs` can yield MULTIPLE candidates in resolution order —
/// notably `"localhost"` on macOS, which resolves to
/// `[[::1]:port, 127.0.0.1:port]`. The old code took only the first
/// candidate and bound a v4-only `"0.0.0.0:0"` socket regardless of its
/// family, so an IPv6-first resolution could never connect (an `AF_INET`
/// socket can't `connect()` to an `AF_INET6` peer). This tries every
/// candidate, binding a socket that matches ITS family, and uses the first
/// one that connects.
///
/// NOTE: UDP `connect()` succeeding proves only that the local
/// bind/family/routing-table lookup for that peer succeeded — it's
/// connectionless, so this is not proof the peer is reachable or even
/// listening. That's fine here: family mismatch (the actual bug) is exactly
/// what binding a matching-family socket screens for; genuine
/// unreachability still surfaces later as the FSM never reaching `Linked`.
///
/// # Errors
/// [`ConsoleError::Resolve`] if `host`/`port` resolve to no addresses, or
/// no candidate can be bound+connected, or the chosen socket's read timeout
/// can't be set.
fn connect_udp_socket(host: &str, port: u16) -> Result<UdpSocket, ConsoleError> {
    let resolve_err = || ConsoleError::Resolve {
        node: host.to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {host}:{port}"),
        ),
    };
    let candidates: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    let mut connected = None;
    for candidate in &candidates {
        let bind_addr = if candidate.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let Ok(candidate_socket) = UdpSocket::bind(bind_addr) else {
            continue;
        };
        if candidate_socket.connect(candidate).is_ok() {
            connected = Some(candidate_socket);
            break;
        }
    }
    let socket = connected.ok_or_else(resolve_err)?;
    socket
        .set_read_timeout(Some(SOCKET_POLL_TIMEOUT))
        .map_err(|e| ConsoleError::Resolve {
            node: host.to_string(),
            source: e,
        })?;
    Ok(socket)
}

impl Drop for M17Session {
    fn drop(&mut self) {
        // Defensive: a session dropped without an explicit `disconnect()`
        // call still shuts its thread down cleanly (same DISC-then-join path)
        // rather than leaking it. A no-op after `disconnect()` already ran
        // (thread is `None` by then).
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything [`run_loop`] needs, bundled to keep the spawn call's arity sane
/// (`clippy::too_many_arguments`).
struct RunLoopParams {
    socket: UdpSocket,
    fsm: SessionFsm,
    router: AudioRouter,
    mic: MicId,
    out: OutputId,
    call_audio: CallAudio,
    codec: Box<dyn Codec2Voice>,
    callsign: [u8; 6],
    shutdown: Arc<AtomicBool>,
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
    prefs: Arc<SharedPrefs>,
    /// The profile generation already applied (synchronously, at connect
    /// time, before this thread started) — seeds the run-loop's own tracking
    /// var so it doesn't redundantly re-clone+re-apply the SAME profile on
    /// its very first tick.
    last_applied_profile_gen: u32,
    spectrum: Arc<SharedSpectrum>,
}

/// Per-transmission TX bookkeeping: the current random `StreamID`, the
/// running frame counter (low 15 bits; restarts at 0 on every key-down), and
/// any half-paired 160-sample frame awaiting its partner before a full
/// 54-byte packet can be built.
struct TxState {
    stream_id: u16,
    frame_no: u16,
    pending: Option<[i16; 160]>,
}

impl TxState {
    fn new() -> Self {
        Self {
            stream_id: 0,
            frame_no: 0,
            pending: None,
        }
    }

    /// Key-down edge: fresh random `StreamID`, counter restarts at 0. Also
    /// defensively drains and discards anything already sitting in
    /// `call_audio.tx_frames` before resetting — a safety net against any
    /// stale audio left over from BEFORE this key-down: the connect-time
    /// window between `open_call` (which keys the mic lane's gate
    /// immediately) and the first `set_gate(false)`, or a residual frame
    /// that slipped in on a prior unkey race. This discard can never eat a
    /// LEGITIMATE frame: the mic lane's gate is still `false` when this
    /// runs (the caller only flips it to `true` afterward — see
    /// `apply_ptt_edge`), and `MicLane::write` returns before ever reaching
    /// the channel while unkeyed, so the channel can only contain leftovers
    /// at this point, never freshly-produced audio.
    fn key_down(&mut self, call_audio: &CallAudio) {
        while call_audio.tx_frames.try_recv().is_ok() {}
        self.stream_id = rand::random();
        self.frame_no = 0;
        self.pending = None;
    }

    /// Consume the current frame number for a just-completed pair and
    /// advance the counter, masking off [`StreamPacket::EOS_BIT`] so the
    /// counter itself can never collide with it.
    fn next_frame_no(&mut self) -> u16 {
        let n = self.frame_no;
        self.frame_no = self.frame_no.wrapping_add(1) & !StreamPacket::EOS_BIT;
        n
    }
}

/// The "iax-m17" run-loop: the ONE thread that owns the socket, the
/// [`AudioRouter`], and the [`Codec2Voice`] instance for this session. Single
/// poll cadence (the socket's 50 ms read timeout) drives everything: PTT
/// edges, TX framing, RX decode, the FSM's keepalive tick, and meter
/// refresh.
fn run_loop(p: RunLoopParams) {
    let RunLoopParams {
        socket,
        mut fsm,
        router,
        mic,
        out,
        call_audio,
        mut codec,
        callsign,
        shutdown,
        ptt_request,
        shared,
        prefs,
        mut last_applied_profile_gen,
        spectrum,
    } = p;
    // `router` outlives the whole loop (dropping it at the end closes the
    // audio streams); only `set_gate`/`mic_tx_dbfs`/`output_rx_dbfs` (all
    // `&self`) are needed after `open_call`, so no `mut` binding is required.

    let now = Instant::now();
    let conn_bytes = fsm.connect(now);
    let _ = socket.send(&conn_bytes);
    shared
        .link
        .store(link_to_u8(fsm.state()), Ordering::Relaxed);

    let mut keyed = false;
    let mut tx = TxState::new();
    let mut last_rx_voice: Option<Instant> = None;
    let mut buf = [0u8; 2_048];
    // Fix 6: reused every tick, never reallocated.
    let mut tx_spectrum_buf = [0.0f32; astar_audio::SPECTRUM_BINS];
    let mut rx_spectrum_buf = [0.0f32; astar_audio::SPECTRUM_BINS];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            send_disc_flushing_eos_if_keyed(
                &socket,
                &router,
                &mic,
                callsign,
                codec.as_mut(),
                &mut tx,
                &shared,
                &call_audio,
                keyed,
            );
            break;
        }

        // 0. Re-apply live audio DSP prefs (iax-f2b8-fix Fix 4): cheap even
        //    when nothing changed (a handful of atomic loads/stores), so no
        //    dirty-flag tracking beyond the profile's own generation counter.
        prefs.apply(&router, &mic, &out, &mut last_applied_profile_gen);

        // 1. Apply a pending PTT edge (set_ptt only requests; this is where
        //    it actually takes effect).
        let want_key = ptt_request.load(Ordering::Relaxed);
        if want_key != keyed {
            keyed = apply_ptt_edge(
                &socket,
                &router,
                &mic,
                callsign,
                codec.as_mut(),
                &mut tx,
                &shared,
                &call_audio,
                want_key,
            );
        }

        // 2. Drain any ready TX frames, pairing two 160-sample frames per
        //    54-byte stream packet.
        if keyed {
            drain_tx_frames(&socket, callsign, codec.as_mut(), &mut tx, &call_audio);
        }

        // 3. Socket poll (bounded by SOCKET_POLL_TIMEOUT): react to whatever
        //    the FSM says about a received packet.
        poll_socket(
            &socket,
            &mut buf,
            &mut fsm,
            codec.as_mut(),
            &call_audio,
            &shared,
            &mut last_rx_voice,
        );

        // 4. Keepalive tick (answers PING with PONG via FsmAction::Send;
        //    declares Failed after the configured silence window).
        if let FsmAction::Send(bytes) = fsm.tick(Instant::now()) {
            let _ = socket.send(&bytes);
        }
        shared
            .link
            .store(link_to_u8(fsm.state()), Ordering::Relaxed);

        // 5. RX silence timeout: no voice-stream packet in 400 ms clears
        //    `receiving`.
        if let Some(t) = last_rx_voice
            && t.elapsed() >= RX_SILENCE_TIMEOUT
        {
            shared.receiving.store(false, Ordering::Relaxed);
            last_rx_voice = None;
        }

        // 6. Meters + spectrum (iax-f2b8-fix Fix 6 adds input level + TX/RX
        //    spectrum readback to Fix 4's gain readback; extracted to keep
        //    this function under clippy's line-count limit).
        refresh_meters(
            &router,
            &mic,
            &out,
            &shared,
            &spectrum,
            &mut tx_spectrum_buf,
            &mut rx_spectrum_buf,
        );
    }
    // `router` (and thus the audio streams) and `socket` drop here.
}

/// Run-loop step 6: refresh every poll-cheap meter/analyzer the control side
/// can read (dBFS levels, applied gains, TX/RX spectrum) straight off the
/// router's own accessors — nothing is computed here, only copied. See the
/// individual `SharedState`/`SharedSpectrum` field docs for what each one
/// proves (iax-f2b8-fix Fix 4's gain readback, Fix 6's input level +
/// spectrum).
#[allow(clippy::too_many_arguments)]
fn refresh_meters(
    router: &AudioRouter,
    mic: &MicId,
    out: &OutputId,
    shared: &SharedState,
    spectrum: &SharedSpectrum,
    tx_spectrum_buf: &mut [f32; astar_audio::SPECTRUM_BINS],
    rx_spectrum_buf: &mut [f32; astar_audio::SPECTRUM_BINS],
) {
    if let Some(db) = router.mic_tx_dbfs(mic) {
        shared.tx_dbfs.store(db.to_bits(), Ordering::Relaxed);
    }
    if let Some(db) = router.output_rx_dbfs(out) {
        shared.rx_dbfs.store(db.to_bits(), Ordering::Relaxed);
    }
    // Read the gains back OFF the router itself (not just echoing the pref
    // atomics) so a snapshot proves the value actually reached the router,
    // not just that `set_*_gain` was called (iax-f2b8-fix Fix 4).
    if let Some(g) = router.mic_gain(mic) {
        shared
            .applied_mic_gain
            .store(g.to_bits(), Ordering::Relaxed);
    }
    if let Some(g) = router.output_gain(out) {
        shared
            .applied_output_gain
            .store(g.to_bits(), Ordering::Relaxed);
    }
    // RX/output compression readback (iax-a4e7 PHASE 1), same round-trip
    // contract as the gain readback above.
    if let Some(on) = router.output_compress(out) {
        shared.applied_rx_compress.store(on, Ordering::Relaxed);
    }
    if let Some(level) = router.output_compress_level(out) {
        shared
            .applied_rx_compress_level
            .store(level.to_bits(), Ordering::Relaxed);
    }
    // Mic INPUT level (iax-f2b8-fix Fix 6): continuous, independent of
    // `keyed` — see `AudioRouter::mic_input_dbfs`'s own doc for why (VOX must
    // be able to key from silence).
    if let Some(db) = router.mic_input_dbfs(mic) {
        shared.input_dbfs.store(db.to_bits(), Ordering::Relaxed);
    }
    // TX/RX spectrum (iax-f2b8-fix Fix 6): fixed-size buffers reused every
    // tick (no per-tick allocation); `mic_tx_spectrum`/`output_rx_spectrum`
    // always write exactly `SPECTRUM_BINS` once the lane/bus is open (see
    // `SpectrumAnalyzer::copy_into`), so `n` is effectively always
    // `SPECTRUM_BINS` here — checked defensively anyway since it's the
    // router's contract, not this file's.
    if let Some(n) = router.mic_tx_spectrum(mic, tx_spectrum_buf) {
        let mut g = spectrum.tx.lock().expect("tx spectrum mutex");
        g.0[..n].copy_from_slice(&tx_spectrum_buf[..n]);
        g.1 = n;
    }
    if let Some(n) = router.output_rx_spectrum(out, rx_spectrum_buf) {
        let mut g = spectrum.rx.lock().expect("rx spectrum mutex");
        g.0[..n].copy_from_slice(&rx_spectrum_buf[..n]);
        g.1 = n;
    }
}

/// Run-loop step 1: apply a pending PTT edge. M17 carries PTT purely via
/// stream start/`EOS` (there is no protocol PTT frame).
///
/// Key-down: drains/discards any stale `call_audio.tx_frames` entries WHILE
/// the gate is still closed (see [`TxState::key_down`] for why that
/// ordering is what makes the discard safe), THEN opens the gate under the
/// fresh random `StreamID`.
///
/// Key-up: closes the gate FIRST (so the mic lane stops enqueuing anything
/// more), THEN drains whatever it already queued into ordinary packets
/// ([`drain_tx_frames`]) — up to one [`SOCKET_POLL_TIMEOUT`] tick's worth of
/// audio the run-loop hadn't gotten to yet — and only THEN flushes the
/// final `EOS`-marked packet (the one pending half-frame, if any, from that
/// drain, zero-padded, or an all-zero payload if nothing was left). Getting
/// this order backwards is exactly what clips the tail of a transmission
/// and/or leaks stale audio into the START of the next one under a new
/// `StreamID`.
///
/// Returns the newly-applied keyed state (mirrors `want_key`; only called
/// when it differs from the previous poll's).
#[allow(clippy::too_many_arguments)]
fn apply_ptt_edge(
    socket: &UdpSocket,
    router: &AudioRouter,
    mic: &MicId,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    shared: &SharedState,
    call_audio: &CallAudio,
    want_key: bool,
) -> bool {
    if want_key {
        tx.key_down(call_audio);
        router.set_gate(mic, true);
    } else {
        router.set_gate(mic, false);
        drain_tx_frames(socket, callsign, codec, tx, call_audio);
        send_voice_packet(
            socket,
            callsign,
            codec,
            tx.stream_id,
            tx.frame_no,
            true,
            tx.pending.take().as_ref(),
            None,
        );
    }
    shared.ptt.store(want_key, Ordering::Relaxed);
    want_key
}

/// Run-loop shutdown branch (iax-f2b8-fix Fix 5): if still `keyed`, flush the
/// SAME EOS-marked packet a normal unkey would — reusing [`apply_ptt_edge`]'s
/// unkey path — BEFORE sending `DISC`. Without this, disconnecting while
/// keyed left the far end's stream open (no EOS bit ever seen) until ITS OWN
/// silence timeout closed it out.
#[allow(clippy::too_many_arguments)]
fn send_disc_flushing_eos_if_keyed(
    socket: &UdpSocket,
    router: &AudioRouter,
    mic: &MicId,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    shared: &SharedState,
    call_audio: &CallAudio,
    keyed: bool,
) {
    if keyed {
        let _ = apply_ptt_edge(
            socket, router, mic, callsign, codec, tx, shared, call_audio, false,
        );
    }
    let disc = ControlPacket::Disc {
        callsign: Some(callsign),
    }
    .to_bytes();
    let _ = socket.send(&disc);
}

/// Run-loop step 2: drain any TX frames ready in `call_audio`, pairing two
/// 160-sample frames per 54-byte voice-stream packet. Only called while
/// keyed; the mic lane's gate stops forwarding frames the instant it's
/// unkeyed, so this simply drains whatever was already buffered and returns
/// (an inner `loop`/`break`, not a `while` on an outer flag, since nothing
/// here needs to re-check the keyed state itself).
fn drain_tx_frames(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    call_audio: &CallAudio,
) {
    while let Ok(frame) = call_audio.tx_frames.try_recv() {
        let Some(pcm) = frame_to_array(&frame) else {
            continue; // defensive: StreamConfig::default() guarantees len 160
        };
        if let Some(first) = tx.pending.take() {
            let frame_no = tx.next_frame_no();
            send_voice_packet(
                socket,
                callsign,
                codec,
                tx.stream_id,
                frame_no,
                false,
                Some(&first),
                Some(&pcm),
            );
        } else {
            tx.pending = Some(pcm);
        }
    }
}

/// Run-loop step 3: poll the socket (bounded by [`SOCKET_POLL_TIMEOUT`]) and
/// react to whatever the FSM says about a received packet: reply to a
/// keepalive `PING`, decode+forward a voice-stream packet (bumping
/// `last_rx_voice`/`receiving`), or do nothing for anything else (including
/// a fresh `Unlinked`, which the run-loop's own `fsm.state()` read picks up
/// regardless of which path set it).
fn poll_socket(
    socket: &UdpSocket,
    buf: &mut [u8],
    fsm: &mut SessionFsm,
    codec: &mut dyn Codec2Voice,
    call_audio: &CallAudio,
    shared: &SharedState,
    last_rx_voice: &mut Option<Instant>,
) {
    match socket.recv(buf) {
        Ok(n) => {
            let action = fsm.on_packet(&buf[..n], Instant::now());
            match action {
                FsmAction::Send(bytes) => {
                    let _ = socket.send(&bytes);
                }
                FsmAction::Voice(pkt) => {
                    *last_rx_voice = Some(Instant::now());
                    shared.receiving.store(true, Ordering::Relaxed);
                    decode_and_forward(codec, &pkt, call_audio);
                }
                FsmAction::Unlinked | FsmAction::None => {}
            }
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(_) => {}
    }
}

/// Decode one received [`StreamPacket`]'s 16-byte Codec 2 payload (two 8-byte
/// mode-3200 chunks) into 2×160 PCM samples and forward them to
/// `call_audio.rx_frames`. A pure in-order pass-through — see the module
/// docs' "RX jitter handling" section for why no reorder buffer is used yet.
fn decode_and_forward(codec: &mut dyn Codec2Voice, pkt: &StreamPacket, call_audio: &CallAudio) {
    let mut bits_a = [0u8; 8];
    bits_a.copy_from_slice(&pkt.payload[0..8]);
    let mut bits_b = [0u8; 8];
    bits_b.copy_from_slice(&pkt.payload[8..16]);
    let pcm_a = codec.decode(&bits_a);
    let pcm_b = codec.decode(&bits_b);
    let _ = call_audio.rx_frames.send(pcm_a.to_vec());
    let _ = call_audio.rx_frames.send(pcm_b.to_vec());
}

/// Build and send one voice-stream packet. `a`/`b` are the two 160-sample
/// halves; `None` for a half means "send zero bytes for this half" —
/// literal silence, not a Codec 2-encoded silent frame — which is exactly
/// what the key-up flush needs for a fully- or partially-empty final packet
/// (see [`M17Session`]'s module docs and the Task 3 design brief: "zero-pad
/// the second half if only one frame is pending; if zero frames pending,
/// send an EOS packet with all-zero payload").
#[allow(clippy::too_many_arguments)]
fn send_voice_packet(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    stream_id: u16,
    frame_number: u16,
    eos: bool,
    a: Option<&[i16; 160]>,
    b: Option<&[i16; 160]>,
) {
    let bits_a = a.map_or([0u8; 8], |pcm| codec.encode(pcm));
    let bits_b = b.map_or([0u8; 8], |pcm| codec.encode(pcm));
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(&bits_a);
    payload[8..16].copy_from_slice(&bits_b);
    let mut fnum = frame_number & !StreamPacket::EOS_BIT;
    if eos {
        fnum |= StreamPacket::EOS_BIT;
    }
    let pkt = StreamPacket {
        stream_id,
        lsf: Lsf {
            // BROADCAST dst: mirrors astar_m17's own FSM test fixtures —
            // voice frames relayed through a reflector module go to every
            // listener on that module, not a single peer address.
            dst: BROADCAST,
            src: callsign,
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number: fnum,
        payload,
    };
    let _ = socket.send(&pkt.to_bytes());
}

/// Convert one TX frame (`StreamConfig::default()` guarantees 160 i16
/// samples per frame while keyed) into the fixed-size array Codec 2 wants.
/// Returns `None` on an unexpected length (defensive only — should not
/// happen given the router's frame chunking).
fn frame_to_array(v: &[i16]) -> Option<[i16; 160]> {
    if v.len() != 160 {
        return None;
    }
    let mut a = [0i16; 160];
    a.copy_from_slice(v);
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// Build a [`CallAudio`] by hand — no `AudioRouter`/`MicLane` involved —
    /// so a test can push raw frames straight onto the exact channel
    /// `TxState`/`apply_ptt_edge`/`drain_tx_frames` read from. Returns the
    /// `CallAudio` plus the `Sender` half of `tx_frames` (the test's stand-in
    /// for "the mic lane already queued this") and the `Receiver` half of
    /// `rx_frames` (unused by the TX-side tests below, but part of the real
    /// struct).
    fn fake_call_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<Vec<i16>>) {
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        (call_audio, tx_tx, rx_rx)
    }

    #[test]
    fn key_down_discards_stale_frames_left_in_the_channel() {
        // Simulates the connect-time gate window (open_call keys the mic
        // lane immediately; M17Session::connect un-keys right after) or any
        // other leftover-frame race: something pushed frames onto
        // `tx_frames` before this key-down ever ran.
        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![1_i16; 160]).unwrap();
        push.send(vec![2_i16; 160]).unwrap();

        let mut tx = TxState::new();
        tx.key_down(&call_audio);

        assert!(
            call_audio.tx_frames.try_recv().is_err(),
            "key_down must drain/discard anything already queued before a fresh transmission starts"
        );
        assert!(tx.pending.is_none());
    }

    #[test]
    fn unkey_drains_queued_frames_before_the_eos_flush() {
        // Reproduces the reviewed race directly: three frames (a full pair
        // plus one odd frame) are sitting in `tx_frames` — audio the mic
        // lane queued in the ~50ms since the run-loop's last drain — when
        // the unkey edge is observed. The fix must send them as an ordinary
        // (non-EOS) packet BEFORE the EOS-flushed final packet, not drop
        // them and not leak them into the next transmission.
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        recv_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let recv_addr = recv_sock.local_addr().expect("recv addr");
        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.connect(recv_addr).expect("connect send socket");

        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![10_i16; 160]).unwrap();
        push.send(vec![20_i16; 160]).unwrap();
        push.send(vec![30_i16; 160]).unwrap();

        // A router with no mic ever opened: `set_gate` on an unopened mic is
        // a documented no-op, so this is a valid (if inert) stand-in — the
        // gate side effect itself isn't what this test is checking.
        let router = AudioRouter::new(Box::new(astar_audio::NullBackend::new()));
        let mic = MicId::new("unopened-test-mic");
        let (mut codec, _backend) = astar_codec::codec2::open_codec2(&[])
            .expect("a codec must be available under this crate's dev-dependency codec2-static");
        let mut tx = TxState::new();
        tx.stream_id = 0xBEEF;
        tx.frame_no = 5;
        let shared = SharedState::new();

        let keyed = apply_ptt_edge(
            &send_sock,
            &router,
            &mic,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            false, // unkey edge
        );
        assert!(!keyed);

        // First packet: the drained pair (frames 10+20), NOT EOS-marked.
        let mut buf = [0u8; 128];
        let (n1, _) = recv_sock
            .recv_from(&mut buf)
            .expect("the drained pair must be sent as an ordinary packet");
        let p1 = StreamPacket::parse(&buf[..n1]).expect("valid stream packet");
        assert_eq!(p1.stream_id, 0xBEEF);
        assert!(
            !p1.is_last(),
            "the drained pair must not itself carry the EOS bit"
        );

        // Second packet: the EOS flush carrying the odd leftover frame (30),
        // zero-padded in its second half.
        let (n2, _) = recv_sock
            .recv_from(&mut buf)
            .expect("the EOS flush must follow the drained pair");
        let p2 = StreamPacket::parse(&buf[..n2]).expect("valid stream packet");
        assert_eq!(p2.stream_id, 0xBEEF);
        assert!(p2.is_last(), "the final flushed packet must carry EOS");

        // Nothing left over: the tail wasn't clipped (all 3 queued frames
        // went out) and nothing leaks into a future transmission.
        assert!(
            call_audio.tx_frames.try_recv().is_err(),
            "every queued frame must have been drained, none left for the next transmission"
        );
        assert!(tx.pending.is_none());
    }
}
