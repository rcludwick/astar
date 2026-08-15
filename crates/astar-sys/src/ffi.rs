// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The C-ABI surface. Every `extern "C"` function is `#[no_mangle]`, wraps its
//! body in `catch_unwind`, and null-checks its handle/out-pointer arguments.
//!
//! cbindgen reads this module (via `lib.rs`) to emit `include/astar.h`.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char, c_float, c_int, c_uint, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::Duration;

use zeroize::Zeroize;

use astar_station::LinkMode;
use astar_station::{
    AnswerPolicy, CallMode, CallStatus, CodecPolicy, DtmfMode, InboundConfig, IncomingAuthPolicy,
    IncomingCallPolicy, NodeConfig, OperatingMode, PortalCredentials, RegisterConfig, Station,
    StationConfig, StationError, StationEvent, VoiceFormat, WgConfigError, WgLinkConfig,
};

// ---------------------------------------------------------------------------
// Opaque handle
// ---------------------------------------------------------------------------

/// Opaque station handle. Heap-allocated by [`iax_station_new`], freed by
/// [`iax_station_free`]. The only cross-boundary heap object; treat it as a
/// pointer you must not dereference, copy-and-free twice, or use after free.
pub struct IaxStation {
    inner: Station,
    /// The caller id of the most recently drained [`IaxEventKind::Incoming`]
    /// event, exposed via [`iax_station_incoming_from`]. Kept here (rather than
    /// embedded in the fixed-size [`IaxEvent`]) so the poll loop stays simple
    /// and bindings get a clean string accessor. Secret-free (a node id).
    last_incoming: Mutex<String>,
    /// The node label of the most recently drained link event, exposed via
    /// [`iax_station_link_event_node`] (iax-1075). Mirrors `last_incoming`.
    last_link_node: Mutex<String>,
}

// ---------------------------------------------------------------------------
// repr(C) types
// ---------------------------------------------------------------------------

/// Station configuration passed to [`iax_station_new`]. All fields are
/// **borrowed** `const char*` (the library copies what it needs; the caller
/// keeps ownership and may free them right after the call returns).
///
/// A NULL pointer means "unset": NULL `input`/`output` selects the system
/// default device; NULL `secret` defaults to the guest secret `"allstar"`; the
/// three `portal_*` fields enable the WT path only when **all three** are
/// non-NULL.
///
/// Note: there is intentionally no `secret`-in-snapshot field anywhere; the
/// `secret` here is an in-param consumed into the station, never echoed back.
#[repr(C)]
pub struct IaxConfig {
    /// Capture device name substring, or NULL for the system default.
    pub input: *const c_char,
    /// Playback device name substring, or NULL for the system default.
    pub output: *const c_char,
    /// `AllStar` portal user (WT path), or NULL.
    pub portal_user: *const c_char,
    /// `AllStar` portal password (WT path), or NULL. Consumed into the station's
    /// `PortalCredentials`; never stored in any out-struct or logged.
    pub portal_pass: *const c_char,
    /// `AllStar` node selector for token minting (WT path), or NULL.
    pub portal_node: *const c_char,
    /// Guest secret, or NULL for the default `"allstar"`.
    pub secret: *const c_char,
    /// Codec negotiation policy for OUTBOUND calls placed by this station
    /// (iax-4348/iax-3e53), as a config string: `"ulaw_only"` (the library
    /// default), `"allow_slin"`, `"prefer_slin"`, or `"prefer_slin16"`
    /// (16 kHz wideband). NULL, empty, or `"default"` selects the library
    /// default. Any other string fails construction ([`iax_station_new`]
    /// returns NULL) rather than silently defaulting.
    ///
    /// Construction-time only — the station's audio pipeline rate is pinned
    /// by this policy when the engine is built, so there is deliberately no
    /// runtime setter; changing the policy means rebuilding the station.
    pub codec_policy: *const c_char,
}

/// Top-level operating mode. Mirrors `astar_station::OperatingMode`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IaxMode {
    /// Dial-out Web-Transceiver client (default).
    Wt = 0,
    /// Inbound IAX2 node (accept calls, bridge to the local handset).
    Node = 1,
}

/// How [`iax_station_send_dtmf`] emits digits on the active call. Mirrors
/// `astar_station::DtmfMode` (iax-7fff).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IaxDtmfMode {
    /// Synthesize the dual-tone waveform into the call's TX audio path
    /// (in-band; default) — for nodes/paths that only decode audio tones.
    InBand = 0,
    /// Send out-of-band IAX2 protocol DTMF frames (`DTMF BEGIN`/`DTMF END`) —
    /// what Asterisk/`AllStar` expects by default. The tone duration does not
    /// apply; the frame pair carries the digit only.
    Protocol = 1,
}

/// How a node answers inbound calls. Mirrors `astar_station::AnswerPolicy`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IaxAnswerPolicy {
    /// Auto-accept the inbound offer and bridge it to the local handset.
    Auto = 0,
    /// Surface the offer as an [`IaxEventKind::Incoming`] event; the operator
    /// then calls [`iax_station_answer`] / [`iax_station_reject`].
    Manual = 1,
}

/// Inbound-authentication policy for a node listener. Mirrors
/// `astar_iax::IncomingAuthPolicy`. Note: per-user credentials for `Required`
/// are not yet configurable across this ABI (a future revision); `Required`
/// with no credentials rejects everyone, so use `Off` for an open dev dial-in.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IaxAuthPolicy {
    /// Every inbound NEW must authenticate (unknown user → REJECT).
    Required = 0,
    /// Challenge only if the peer's username maps to a held credential.
    Optional = 1,
    /// Never challenge (accept anonymous). Permissive — dev dial-in only.
    Off = 2,
}

/// Node-mode configuration passed to [`iax_station_set_node_config`].
///
/// **Secret-free by construction**: there is no password field anywhere here.
/// The registrar password (for `registrar`/`register_user`) is supplied only at
/// runtime through the callback registered via
/// [`iax_station_set_credential_resolver`]. All `const char*` are borrowed for
/// the duration of the call.
#[repr(C)]
pub struct IaxNodeConfig {
    /// Listener bind address `"host:port"`, or NULL for `"0.0.0.0:4569"`.
    pub bind: *const c_char,
    /// Auto vs manual answer.
    pub answer: IaxAnswerPolicy,
    /// Inbound auth policy.
    pub auth: IaxAuthPolicy,
    /// Upstream registrar `"host:port"` to register AS a node, or NULL to only
    /// listen (no registration). Resolve the node→host:port yourself.
    pub registrar: *const c_char,
    /// The node id to register AS (e.g. `"77777"`), required when `registrar`
    /// is non-NULL, otherwise ignored. May be NULL when not registering.
    pub register_user: *const c_char,
    /// Requested registration refresh interval in seconds, or `0` for `60`.
    pub refresh_secs: u32,
}

/// Resolve a registrar/auth username to its secret. Registered via
/// [`iax_station_set_credential_resolver`] and invoked by the library when it
/// needs the password (e.g. to answer a registrar's MD5 challenge).
///
/// Contract: write the NUL-terminated secret for `user` into `out` (a buffer of
/// `cap` bytes) and return [`IAX_OK`]; return any non-zero value to signal "no
/// secret" (the library treats the secret as empty). `user_data` is the opaque
/// pointer passed at registration time. The callback may be invoked from a
/// background thread, so whatever `user_data` points at must be thread-safe.
pub type IaxCredentialResolver = extern "C" fn(
    user: *const c_char,
    out: *mut c_char,
    cap: usize,
    user_data: *mut c_void,
) -> c_int;

/// Call lifecycle status. Mirrors `astar_station::CallStatus`, collapsing
/// `Hangup`/`Failed` into a single [`IaxStatus::Hangup`] (the human-readable
/// reason is available via [`iax_station_next_event`], not this enum).
#[repr(C)]
pub enum IaxStatus {
    /// No call in progress.
    Idle = 0,
    /// NEW sent, awaiting answer.
    Dialing = 1,
    /// Peer answered; media flowing.
    Answered = 2,
    /// Call ended (normal hangup or failed dial).
    Hangup = 3,
}

/// A snapshot of live call state. Caller-allocated; filled by
/// [`iax_station_snapshot`].
#[repr(C)]
pub struct IaxState {
    /// Current lifecycle status.
    pub status: IaxStatus,
    /// Local transmit (PTT) state.
    pub ptt: bool,
    /// Remote-keyed state.
    pub remote_ptt: bool,
    /// TX (transmitted, post-DSP) level in dBFS, `-60.0..=0.0`. Meters only
    /// while keyed; floors to -60 when unkeyed.
    pub tx_db: c_float,
    /// RX (decoded node audio) level in dBFS, `-60.0..=0.0`.
    pub rx_db: c_float,
    /// Continuous mic INPUT level in dBFS, `-60.0..=0.0`, metered even while
    /// unkeyed (post-gain, pre-noise-reduction). Drive VOX from this field, not
    /// `tx_db` (iax-5c30). A plain dBFS float, carrying no sensitive data.
    pub input_db: c_float,
    /// Smoothed round-trip estimate in milliseconds, or `-1` if unknown.
    pub rtt_ms: c_int,
    /// Current top-level operating mode (WT dial-out vs inbound Node).
    pub mode: IaxMode,
    /// Cumulative voice-ts-ladder re-anchors (>80 ms TX-clock drift events;
    /// iax-5530/iax-9e55). A growing value signals choppy TX. A plain `u64`
    /// health counter, credential-free.
    pub tx_reanchors: u64,
    /// Cumulative cpal capture overruns (dropped input buffers — holes in the
    /// captured mic PCM; iax-9e55) on the active call's routed mic. The lead
    /// suspect for choppy TX; `0` when monitor-only. A plain `u64` health
    /// counter, credential-free.
    pub tx_capture_overruns: u64,
    /// Negotiated voice codec of the active call as its IAX2 format bit
    /// (iax-3e53): `0` = none (idle or still negotiating), `4` = G.711 µ-law,
    /// `8` = G.711 A-law, `64` = slin (8 kHz linear), `32768` = slin16
    /// (16 kHz wideband). A plain codec id, credential-free.
    pub negotiated_format: c_uint,
    /// Digits already sent of the active [`iax_station_send_dtmf_string`]
    /// sequence (iax-4b7a); `0` when no sequence is playing. A plain progress
    /// counter, credential-free.
    pub dtmf_played: c_uint,
    /// Total digits of the active sequence; `0` when no sequence is playing.
    /// A plain progress counter, credential-free.
    pub dtmf_total: c_uint,
    /// `true` when M17 voice is available: the `m17` feature is compiled in
    /// AND a working Codec 2 backend was found, probed against the current
    /// [`iax_station_set_codec_dirs`] value (iax-f2b8 Task 5). Gate a UI's
    /// M17 connect affordance on this rather than calling
    /// [`iax_station_connect_m17`] speculatively.
    pub m17_available: bool,
    /// `true` while an M17 session is live (mutually exclusive with an
    /// active IAX2 call — see [`iax_station_connect_m17`]).
    pub m17_active: bool,
    /// `true` when D-Star voice is available: the `dstar` feature is compiled
    /// in AND a `ThumbDV` is attached RIGHT NOW (iax-4c8e). D-Star has no
    /// software vocoder, so unlike [`Self::m17_available`] this tracks
    /// HOTPLUG — it flips within ~500 ms of the dongle being plugged in or
    /// pulled out. Gate a UI's D-Star affordance on this (grey it out when
    /// `false`) rather than calling [`iax_station_connect_dstar`]
    /// speculatively.
    pub dstar_available: bool,
    /// `true` while a D-Star session is live — mutually exclusive with both
    /// an active IAX2 call and an M17 session (see
    /// [`iax_station_connect_dstar`]).
    pub dstar_active: bool,
}

/// The kind of a drained lifecycle event (see [`iax_station_next_event`]).
#[repr(C)]
pub enum IaxEventKind {
    /// No event was queued.
    None = 0,
    /// The peer answered; media is flowing.
    Answered = 1,
    /// The remote end keyed/unkeyed (see [`IaxEvent::remote_ptt`]).
    RemotePtt = 2,
    /// The call ended (normal hangup or failed dial).
    Hangup = 3,
    /// The operating mode changed (Wt <-> Node). The new mode is not carried
    /// across this ABI yet; read it from a status query.
    ModeChanged = 4,
    /// An inbound call is ringing (Node/Manual); read the caller id from a
    /// status query.
    Incoming = 5,
    /// Outbound node registration succeeded (Phase 7). Secret-free.
    Registered = 6,
    /// Outbound node registration failed (Phase 7). The reason string is not
    /// carried across the ABI here; it is secret-free.
    RegisterFailed = 7,
}

/// A drained lifecycle event. Caller-allocated; filled by
/// [`iax_station_next_event`]. The hangup *reason* string is not carried here
/// (it is secret-free but variable-length); callers that want it read it from a
/// status-text buffer in a future revision — the kind alone suffices for the
/// poll loop.
#[repr(C)]
pub struct IaxEvent {
    /// What happened.
    pub kind: IaxEventKind,
    /// For [`IaxEventKind::RemotePtt`]: the new remote-keyed state.
    pub remote_ptt: bool,
}

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// Success.
pub const IAX_OK: c_int = 0;
/// A Rust panic was caught at the boundary (should never happen; a bug).
pub const IAX_ERR_PANIC: c_int = -1;
/// A required handle or out-pointer argument was NULL.
pub const IAX_ERR_NULL: c_int = -2;
/// Operation needs an active call but none exists.
pub const IAX_ERR_NOT_CONNECTED: c_int = -3;
/// `connect`/`connect_wt` while a call is already live.
pub const IAX_ERR_ALREADY_CONNECTED: c_int = -4;
/// Portal/token-mint problem (no portal config, login failure, etc.).
pub const IAX_ERR_PORTAL: c_int = -5;
/// Node resolution (DNS) failed.
pub const IAX_ERR_RESOLVE: c_int = -6;
/// Audio enumeration / device error.
pub const IAX_ERR_AUDIO: c_int = -7;
/// Underlying IAX client error.
pub const IAX_ERR_IAX: c_int = -8;
/// Serial-PTT wiring error.
pub const IAX_ERR_SERIAL: c_int = -9;
/// A `const char*` argument was not valid UTF-8.
pub const IAX_ERR_UTF8: c_int = -10;
/// Operation not supported in the current mode (e.g. switching to an
/// unimplemented mode).
pub const IAX_ERR_UNSUPPORTED: c_int = -11;
/// Operation not valid in the current operating mode (e.g. `connect` while in
/// Node mode).
pub const IAX_ERR_MODE_MISMATCH: c_int = -12;
/// Inbound listener failed to bind (port in use, permission denied, etc.).
pub const IAX_ERR_LISTEN: c_int = -13;
/// Node is at its maximum inbound call capacity; the offer was rejected.
pub const IAX_ERR_AT_CAPACITY: c_int = -14;
/// `iax_station_send_dtmf` was given a character that is not a valid DTMF
/// key (`0-9`, `*`, `#`, `A-D`).
pub const IAX_ERR_INVALID_DIGIT: c_int = -15;
/// A link-layer operation failed (no such link, dial failure, bad address).
pub const IAX_ERR_LINK: c_int = -16;
/// `iax_station_send_dtmf_string` while a previous sequence is still playing
/// (iax-4b7a). Cancel it or wait for it to finish.
pub const IAX_ERR_DTMF_BUSY: c_int = -17;
/// M17 error (iax-f2b8 Task 4/5) — an invalid module/callsign, a link/device
/// failure, or the `m17` feature isn't compiled in. Returned by
/// [`iax_station_connect_m17`].
pub const IAX_ERR_M17: c_int = -18;
/// D-Star error (iax-a9d4 Task 6 built RX; iax-2f6b added TX; iax-4c8e
/// exposed both here) — an invalid module/callsign, a link failure, no
/// `ThumbDV` attached, or the `dstar` feature isn't compiled in. D-Star is
/// full-transceive: a live session keys/unkeys like any other call, so this
/// is not returned for a PTT/key attempt. Returned by
/// [`iax_station_connect_dstar`].
pub const IAX_ERR_DSTAR: c_int = -19;

/// Number of log-spaced dBFS bins [`iax_station_mic_spectrum`] writes when
/// monitoring (iax-e73e). Size the `out` array to (at least) this; a larger
/// buffer is fine (the extra entries are left untouched). A literal here so
/// cbindgen can emit it as a `#define`; a `const` assert below keeps it pinned
/// to the engine's [`astar_station::SPECTRUM_BINS`].
pub const IAX_SPECTRUM_BINS: usize = 256;

const _: () = assert!(
    IAX_SPECTRUM_BINS == astar_station::SPECTRUM_BINS,
    "IAX_SPECTRUM_BINS must match astar_audio::SPECTRUM_BINS"
);

// ---------------------------------------------------------------------------
// Internal helpers (not exported to C)
// ---------------------------------------------------------------------------

/// Borrow a `const char*` as `Option<String>`. NULL → `None`.
unsafe fn opt_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

/// Borrow a required `const char*` as `&str`, erroring on NULL or non-UTF-8.
/// (Used where a lossy conversion could silently corrupt a node/name.)
unsafe fn req_str<'a>(p: *const c_char) -> Result<&'a str, c_int> {
    if p.is_null() {
        return Err(IAX_ERR_NULL);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| IAX_ERR_UTF8)
}

fn err_code(e: &StationError) -> c_int {
    match e {
        StationError::NotConnected | StationError::NoPendingCall => IAX_ERR_NOT_CONNECTED,
        StationError::AlreadyConnected => IAX_ERR_ALREADY_CONNECTED,
        StationError::Portal(_) => IAX_ERR_PORTAL,
        StationError::Resolve(_) => IAX_ERR_RESOLVE,
        StationError::Audio(_) => IAX_ERR_AUDIO,
        StationError::Iax(_) => IAX_ERR_IAX,
        StationError::Serial(_) => IAX_ERR_SERIAL,
        StationError::Unsupported => IAX_ERR_UNSUPPORTED,
        StationError::Listen(_) => IAX_ERR_LISTEN,
        StationError::AtCapacity => IAX_ERR_AT_CAPACITY,
        StationError::InvalidDigit => IAX_ERR_INVALID_DIGIT,
        StationError::Link(_) => IAX_ERR_LINK,
        StationError::DtmfBusy => IAX_ERR_DTMF_BUSY,
        StationError::M17(_) => IAX_ERR_M17,
        StationError::Dstar(_) => IAX_ERR_DSTAR,
    }
}

fn fill_state(s: &astar_station::ConsoleState) -> IaxState {
    let status = match &s.status {
        CallStatus::Idle => IaxStatus::Idle,
        CallStatus::Dialing => IaxStatus::Dialing,
        CallStatus::Answered => IaxStatus::Answered,
        CallStatus::Hangup { .. } | CallStatus::Failed { .. } => IaxStatus::Hangup,
    };
    IaxState {
        status,
        ptt: s.ptt,
        remote_ptt: s.remote_ptt,
        tx_db: s.tx_level_db,
        rx_db: s.rx_level_db,
        input_db: s.input_level_db,
        rtt_ms: s
            .rtt_ms
            .map_or(-1, |v| c_int::try_from(v).unwrap_or(c_int::MAX)),
        mode: mode_to_ffi(s.mode),
        tx_reanchors: s.tx_reanchors,
        tx_capture_overruns: s.tx_capture_overruns,
        negotiated_format: s.negotiated_format.map_or(0, VoiceFormat::as_u32),
        dtmf_played: s.dtmf_played,
        dtmf_total: s.dtmf_total,
        m17_available: s.m17_available,
        m17_active: s.m17_active,
        dstar_available: s.dstar_available,
        dstar_active: s.dstar_active,
    }
}

/// Map the station's operating mode to the C-ABI enum.
fn mode_to_ffi(m: OperatingMode) -> IaxMode {
    match m {
        OperatingMode::Wt => IaxMode::Wt,
        OperatingMode::Node => IaxMode::Node,
    }
}

/// Map a `Result<(), StationError>` to an integer code.
fn result_code(r: Result<(), StationError>) -> c_int {
    match r {
        Ok(()) => IAX_OK,
        Err(e) => err_code(&e),
    }
}

/// Write `text` into a caller buffer of `len` bytes, NUL-terminated and
/// truncate-safe. Returns the number of bytes (excluding the NUL) that the full
/// text *would* need, so the caller can detect truncation. `buf` may be NULL
/// only if `len == 0` (a sizing query).
unsafe fn fill_buf(text: &str, buf: *mut c_char, len: usize) -> c_int {
    let bytes = text.as_bytes();
    let needed = bytes.len();
    // Saturate the "needed" report to c_int::MAX; a list that long is absurd
    // but must never wrap into a negative (error-looking) value.
    let needed_c = c_int::try_from(needed).unwrap_or(c_int::MAX);
    if len == 0 || buf.is_null() {
        return needed_c;
    }
    // Reserve one byte for the trailing NUL.
    let copy = needed.min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), copy);
        *buf.add(copy) = 0;
    }
    needed_c
}

// ---------------------------------------------------------------------------
// Lifecycle: new / free
// ---------------------------------------------------------------------------

/// Create a station. Returns a non-NULL `IaxStation*` on success, or NULL if
/// `cfg` is NULL or a panic was caught. The returned handle must be released
/// with [`iax_station_free`]. The `cfg` pointer and its strings are borrowed
/// for the duration of the call only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_new(cfg: *const IaxConfig) -> *mut IaxStation {
    catch_unwind(AssertUnwindSafe(|| {
        if cfg.is_null() {
            return std::ptr::null_mut();
        }
        let cfg = unsafe { &*cfg };
        let portal = match (
            unsafe { opt_str(cfg.portal_user) },
            unsafe { opt_str(cfg.portal_pass) },
            unsafe { opt_str(cfg.portal_node) },
        ) {
            (Some(user), Some(password), Some(node)) => Some(PortalCredentials {
                user,
                password,
                node,
            }),
            _ => None,
        };
        // Parse the codec-policy config string (iax-3e53): NULL/empty/"default"
        // = the library default; anything else must be a documented policy
        // string (iax-4348's `CodecPolicy::from_str`) or construction fails
        // (NULL), never a silent fallback.
        let codec_policy = match unsafe { opt_str(cfg.codec_policy) }.as_deref() {
            None | Some("" | "default") => CodecPolicy::default(),
            Some(s) => match s.parse::<CodecPolicy>() {
                Ok(p) => p,
                Err(_) => return std::ptr::null_mut(),
            },
        };
        let sc = StationConfig {
            input: unsafe { opt_str(cfg.input) },
            output: unsafe { opt_str(cfg.output) },
            portal,
            secret: unsafe { opt_str(cfg.secret) }.unwrap_or_else(|| "allstar".to_string()),
            codec_policy,
            ..StationConfig::default()
        };
        Box::into_raw(Box::new(IaxStation {
            inner: Station::new(sc),
            last_incoming: Mutex::new(String::new()),
            last_link_node: Mutex::new(String::new()),
        }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Free a station created by [`iax_station_new`]. NULL is a no-op. Tears down
/// any active call (via `Station`'s `Drop`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_free(st: *mut IaxStation) {
    if st.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(st) });
    }));
}

// ---------------------------------------------------------------------------
// Connect / disconnect
// ---------------------------------------------------------------------------

/// Manual / non-WT connect (the vendor-neutral generic IAX2 path). `dest`,
/// `calling`, and `name` are required `const char*`; `secret` may be NULL (the
/// configured guest secret is used). The `secret` is consumed immediately and
/// never stored or echoed. Returns [`IAX_OK`] or an `IAX_ERR_*` code.
///
/// NOTE: this performs blocking network I/O (DNS resolve + dial); call it off
/// any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_connect(
    st: *mut IaxStation,
    dest: *const c_char,
    calling: *const c_char,
    secret: *const c_char,
    name: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let dest = match unsafe { req_str(dest) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let calling = match unsafe { req_str(calling) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let name = match unsafe { req_str(name) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        // secret defaults to the configured guest secret when NULL.
        let secret_owned = unsafe { opt_str(secret) };
        let secret = secret_owned.as_deref().unwrap_or("allstar");
        result_code(station.inner.connect(dest, calling, secret, name))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Web-Transceiver connect (the `AllStar` convenience path): mints a token from
/// the configured portal credentials, resolves `dest_node`, and dials. Requires
/// the three `portal_*` fields to have been set in [`IaxConfig`]; otherwise
/// returns [`IAX_ERR_PORTAL`]. Returns [`IAX_OK`] or an `IAX_ERR_*` code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_connect_wt(
    st: *mut IaxStation,
    dest_node: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let dest = match unsafe { req_str(dest_node) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        result_code(station.inner.connect_wt(dest))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Web-Transceiver connect to an EXPLICIT address (astar's manual-address path):
/// mints a token from the configured portal credentials exactly like
/// [`iax_station_connect_wt`], but dials `addr` (`host:port`, or a bare `host`
/// defaulting to the IAX2 port 4569; `host` may be an IP literal or a DNS name)
/// instead of the registrar-resolved node address. Use this for the
/// NAT-hairpin / localhost / LAN case where the registrar's public IP is
/// unreachable.
///
/// `addr` must be non-NULL ([`IAX_ERR_NULL`]) and non-empty/parseable
/// ([`IAX_ERR_RESOLVE`]); for the registrar-resolved path use
/// [`iax_station_connect_wt`]. Requires the `portal_*` config fields
/// ([`IAX_ERR_PORTAL`] otherwise). Returns [`IAX_OK`] or an `IAX_ERR_*` code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_connect_wt_addr(
    st: *mut IaxStation,
    dest_node: *const c_char,
    addr: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let dest = match unsafe { req_str(dest_node) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let addr = match unsafe { req_str(addr) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        // An empty address is rejected by `connect_wt_at` (→ `IAX_ERR_RESOLVE`),
        // so callers fall back to `iax_station_connect_wt` for the no-override path.
        result_code(station.inner.connect_wt_at(dest, addr))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Validate the configured Web-Transceiver credentials WITHOUT placing a call:
/// run the portal login + WT-token mint from the held portal credentials, then
/// **discard** the minted token (it is never returned, stored, or logged). This
/// is the front-end "Test credentials" path; it opens **no** IAX call and **no**
/// UDP call socket.
///
/// Takes **no secret argument** — it reuses the portal credentials supplied via
/// the three `portal_*` fields of [`IaxConfig`] at [`iax_station_new`]. Requires
/// those fields to have been set; otherwise returns [`IAX_ERR_PORTAL`]. Returns
/// [`IAX_OK`] on success, or an `IAX_ERR_*` code — [`IAX_ERR_PORTAL`] for a
/// login/token/network failure (the category is not carried across this ABI; the
/// message from [`iax_error_text`] is generic and secret-free).
///
/// NOTE: this performs blocking network I/O (HTTPS to the portal); call it off
/// any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_mint_token(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        result_code(station.inner.test_mint_token())
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Tear down any active call. Idempotent (no-op while idle). Returns
/// [`IAX_OK`], [`IAX_ERR_NULL`], or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_disconnect(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.disconnect();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Operating mode + node
// ---------------------------------------------------------------------------

fn mode_from_ffi(m: IaxMode) -> OperatingMode {
    match m {
        IaxMode::Wt => OperatingMode::Wt,
        IaxMode::Node => OperatingMode::Node,
    }
}

fn answer_from_ffi(a: IaxAnswerPolicy) -> AnswerPolicy {
    match a {
        IaxAnswerPolicy::Auto => AnswerPolicy::Auto,
        IaxAnswerPolicy::Manual => AnswerPolicy::Manual,
    }
}

fn auth_from_ffi(a: IaxAuthPolicy) -> IncomingAuthPolicy {
    match a {
        IaxAuthPolicy::Required => IncomingAuthPolicy::Required,
        IaxAuthPolicy::Optional => IncomingAuthPolicy::Optional,
        IaxAuthPolicy::Off => IncomingAuthPolicy::Off,
    }
}

/// Switch the operating mode (WT dial-out ↔ inbound Node). Entering Node mode
/// starts the listener (and fires registration if a registrar was configured
/// via [`iax_station_set_node_config`] and a resolver is set); leaving it tears
/// the node down and deregisters. Returns [`IAX_OK`] or an `IAX_ERR_*` code
/// ([`IAX_ERR_UNSUPPORTED`] for unsupported switches).
///
/// NOTE: this performs blocking work (device + socket setup, registration);
/// call it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_mode(st: *mut IaxStation, mode: IaxMode) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        result_code(station.inner.set_mode(mode_from_ffi(mode)))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Write the current operating mode into `out`. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`/`out`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_mode(st: *mut IaxStation, out: *mut IaxMode) -> c_int {
    if st.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let m = mode_to_ffi(station.inner.mode());
        unsafe {
            *out = m;
        }
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Configure Node mode (listener bind, answer/auth policy, optional
/// register-as-node). Takes effect on the next switch to Node mode. The config
/// is **secret-free**; the registrar password is supplied only through the
/// resolver set by [`iax_station_set_credential_resolver`]. `cfg` and its
/// strings are borrowed for the duration of the call. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`/`cfg`, or `registrar` set without
/// `register_user`), [`IAX_ERR_RESOLVE`] (unparseable `bind`/`registrar`
/// address), [`IAX_ERR_UTF8`], or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_node_config(
    st: *mut IaxStation,
    cfg: *const IaxNodeConfig,
) -> c_int {
    if st.is_null() || cfg.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    let cfg = unsafe { &*cfg };
    catch_unwind(AssertUnwindSafe(|| {
        let bind = match unsafe { opt_str(cfg.bind) } {
            Some(s) => match s.parse() {
                Ok(addr) => addr,
                Err(_) => return IAX_ERR_RESOLVE,
            },
            None => NodeConfig::default().bind,
        };
        let register = if cfg.registrar.is_null() {
            None
        } else {
            let peer_s = match unsafe { req_str(cfg.registrar) } {
                Ok(s) => s,
                Err(c) => return c,
            };
            let Ok(peer) = peer_s.parse() else {
                return IAX_ERR_RESOLVE;
            };
            let username = match unsafe { req_str(cfg.register_user) } {
                Ok(s) => s.to_string(),
                Err(c) => return c,
            };
            let refresh = Duration::from_secs(if cfg.refresh_secs == 0 {
                60
            } else {
                u64::from(cfg.refresh_secs)
            });
            Some(RegisterConfig {
                peer,
                username,
                refresh,
            })
        };
        let policy = IncomingCallPolicy {
            auth: auth_from_ffi(cfg.auth),
            ..IncomingCallPolicy::default()
        };
        station.inner.set_node_config(NodeConfig {
            bind,
            policy,
            answer: answer_from_ffi(cfg.answer),
            register,
            max_calls: 20,
            ..NodeConfig::default()
        });
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Start the inbound IAX2 listener with the given node configuration.
///
/// Binds the UDP listener to `cfg.bind` (defaulting to `0.0.0.0:4569` when
/// `bind` is NULL), and configures the auth/answer policy from `cfg`. The
/// station's operating mode is **not** changed — the listener runs
/// independently of the WT/Node mode flag. Call this to bring up the
/// always-on inbound path without switching modes.
///
/// The registrar credential for any future authentication challenge is
/// supplied only via the resolver registered with
/// [`iax_station_set_credential_resolver`] — no credential is passed here.
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st` or `cfg`),
/// [`IAX_ERR_RESOLVE`] (unparseable `bind`), [`IAX_ERR_LISTEN`] (bind
/// failed — port in use, permission denied, etc.), [`IAX_ERR_UTF8`], or
/// [`IAX_ERR_PANIC`].
///
/// NOTE: binds a UDP socket; call it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_enable_inbound(
    st: *mut IaxStation,
    cfg: *const IaxNodeConfig,
) -> c_int {
    if st.is_null() || cfg.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    let cfg = unsafe { &*cfg };
    catch_unwind(AssertUnwindSafe(|| {
        let bind = match unsafe { opt_str(cfg.bind) } {
            Some(s) => match s.parse() {
                Ok(addr) => addr,
                Err(_) => return IAX_ERR_RESOLVE,
            },
            None => InboundConfig::default().bind,
        };
        let policy = IncomingCallPolicy {
            auth: auth_from_ffi(cfg.auth),
            ..IncomingCallPolicy::default()
        };
        let ic = InboundConfig {
            bind,
            policy,
            answer: answer_from_ffi(cfg.answer),
            max_calls: 20,
            ..InboundConfig::default()
        };
        result_code(station.inner.enable_inbound(ic))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Stop the inbound listener started by [`iax_station_enable_inbound`] (or
/// by switching to Node mode). Idempotent — a no-op if the listener is not
/// running. The station's operating mode is **not** changed.
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_disable_inbound(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.disable_inbound();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Start outbound node registration using `cfg.registrar` / `cfg.register_user`
/// / `cfg.refresh_secs`. The registrar credential is resolved on demand via the
/// callback registered with [`iax_station_set_credential_resolver`] — no
/// credential is passed directly here.
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`, `cfg`, or `registrar`/
/// `register_user` not set), [`IAX_ERR_RESOLVE`] (unparseable `registrar`),
/// [`IAX_ERR_UTF8`], or [`IAX_ERR_PANIC`].
///
/// NOTE: opens a UDP socket; call it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_register(
    st: *mut IaxStation,
    cfg: *const IaxNodeConfig,
) -> c_int {
    if st.is_null() || cfg.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    let cfg = unsafe { &*cfg };
    catch_unwind(AssertUnwindSafe(|| {
        if cfg.registrar.is_null() {
            return IAX_ERR_NULL;
        }
        let peer_s = match unsafe { req_str(cfg.registrar) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let Ok(peer) = peer_s.parse() else {
            return IAX_ERR_RESOLVE;
        };
        let username = match unsafe { req_str(cfg.register_user) } {
            Ok(s) => s.to_string(),
            Err(c) => return c,
        };
        let refresh = Duration::from_secs(if cfg.refresh_secs == 0 {
            60
        } else {
            u64::from(cfg.refresh_secs)
        });
        let rc = RegisterConfig {
            peer,
            username,
            refresh,
        };
        result_code(station.inner.register(rc))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Stop outbound node registration. Sends REGREL to the registrar and joins the
/// registration thread. Idempotent — a no-op when not currently registered.
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_deregister(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.deregister();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Opaque C resolver + its `user_data`, wrapped so the closure stored in the
/// station is `Send + Sync`. SAFETY: the C side promises `user_data` (and the
/// function it points into) is safe to invoke from a background thread — see
/// the [`IaxCredentialResolver`] contract.
struct ResolverCtx {
    f: IaxCredentialResolver,
    data: *mut c_void,
}
// SAFETY: upheld by the IaxCredentialResolver contract (caller-thread-safe).
unsafe impl Send for ResolverCtx {}
unsafe impl Sync for ResolverCtx {}

impl ResolverCtx {
    /// Invoke the C resolver for `user` and return its secret (empty on any
    /// failure). Called on whatever thread the library resolves credentials on.
    fn resolve(&self, user: &str) -> String {
        let c_user = CString::new(user).unwrap_or_default();
        // Generous fixed buffer; node/registrar secrets are short.
        let mut buf = [0u8; 512];
        let rc = (self.f)(
            c_user.as_ptr(),
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len(),
            self.data,
        );
        let secret = if rc == IAX_OK {
            CStr::from_bytes_until_nul(&buf)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        // Scrub the transient secret bytes. `zeroize` is a volatile write the
        // optimizer may not elide, unlike `fill(0)` on a soon-dropped buffer.
        buf.zeroize();
        secret
    }
}

/// Register the credential resolver used to obtain secrets at runtime (e.g. the
/// registrar password for [`iax_station_set_node_config`]'s `registrar`). This
/// is the **only** channel for a secret across this ABI — secrets never appear
/// in any config struct, snapshot, event, or log. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_credential_resolver(
    st: *mut IaxStation,
    resolver: IaxCredentialResolver,
    user_data: *mut c_void,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let ctx = ResolverCtx {
            f: resolver,
            data: user_data,
        };
        // `move |user| ctx.resolve(user)` captures `ctx` as a whole (a method
        // call on it), so the Send+Sync wrapper applies — unlike disjoint
        // field capture, which would expose the bare `*mut c_void`.
        station
            .inner
            .set_secret_resolver(Box::new(move |user: &str| ctx.resolve(user)));
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Answer the pending inbound offer (Node/Manual). Returns
/// [`IAX_ERR_NOT_CONNECTED`] when not in Node mode or no offer is pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_answer(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| result_code(station.inner.answer()))).unwrap_or(IAX_ERR_PANIC)
}

/// Reject the pending inbound offer (Node/Manual). Returns
/// [`IAX_ERR_NOT_CONNECTED`] when not in Node mode or no offer is pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_reject(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| result_code(station.inner.reject()))).unwrap_or(IAX_ERR_PANIC)
}

/// Write the caller id of the most recent [`IaxEventKind::Incoming`] event into
/// the caller buffer `buf` of `len` bytes (NUL-terminated, truncate-safe).
/// Returns the byte length the full id needs (excluding the NUL) so the caller
/// can detect truncation, or a negative `IAX_ERR_*`. Pass `len == 0` to query
/// the size. The id is a node identifier — secret-free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_incoming_from(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let from = station
            .last_incoming
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        unsafe { fill_buf(&from, buf, len) }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// PTT + gains
// ---------------------------------------------------------------------------

/// Set local transmit (PTT). Returns [`IAX_ERR_NOT_CONNECTED`] while idle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_ptt(st: *mut IaxStation, on: bool) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| result_code(station.inner.set_ptt(on))))
        .unwrap_or(IAX_ERR_PANIC)
}

/// Send a single DTMF digit to the active call's peer (iax-0e9b). How the
/// digit is emitted follows the mode set via [`iax_station_set_dtmf_mode`]
/// (iax-7fff): by default one complete, fixed-duration in-band tone (~250 ms);
/// in [`IaxDtmfMode::Protocol`] one out-of-band `DTMF BEGIN`/`DTMF END` frame
/// pair. `digit` is an ASCII byte and must be one of the 16 DTMF keys
/// (`'0'..='9'`, `'*'`, `'#'`, `'A'..='D'`; iax-47ae); any other byte returns
/// [`IAX_ERR_INVALID_DIGIT`] without touching the call. Returns
/// [`IAX_ERR_NOT_CONNECTED`] while idle, [`IAX_ERR_NULL`] for a NULL handle.
///
/// Input command only: nothing is stored, returned, or logged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_send_dtmf(st: *mut IaxStation, digit: c_char) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        // c_char may be signed; reinterpret the raw byte (no sign loss) — a
        // non-ASCII byte (high bit set) is never a dialer key.
        let byte = digit.to_ne_bytes()[0];
        if !byte.is_ascii() {
            return IAX_ERR_INVALID_DIGIT;
        }
        result_code(station.inner.send_dtmf(char::from(byte)))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Send a multi-digit DTMF command to the active call as one engine-timed
/// sequence (iax-4b7a): ~250 ms tone + ~100 ms gap per digit, honoring the
/// mode set via [`iax_station_set_dtmf_mode`]. Validation is all-or-nothing:
/// `digits` must be a NUL-terminated string of the 16 DTMF keys only, or
/// [`IAX_ERR_INVALID_DIGIT`] is returned and nothing is sent (an empty string
/// is rejected the same way). The queue advances on [`iax_station_snapshot`]
/// polls; progress is [`IaxState::dtmf_played`] / [`IaxState::dtmf_total`].
/// Returns [`IAX_ERR_NOT_CONNECTED`] while idle (nothing queued),
/// [`IAX_ERR_DTMF_BUSY`] while a previous sequence is still playing,
/// [`IAX_ERR_NULL`] for a NULL handle or string.
///
/// Input command only: nothing is stored beyond the queue, returned, or logged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_send_dtmf_string(
    st: *mut IaxStation,
    digits: *const c_char,
) -> c_int {
    if st.is_null() || digits.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        // Non-UTF-8 bytes can never be dialer keys; reject them the same way
        // the per-digit validation would.
        let Ok(s) = unsafe { std::ffi::CStr::from_ptr(digits) }.to_str() else {
            return IAX_ERR_INVALID_DIGIT;
        };
        result_code(station.inner.send_dtmf_string(s))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Drop the un-played remainder of an [`iax_station_send_dtmf_string`]
/// command (iax-4b7a). The digit currently sounding finishes its tone. Safe
/// to call when nothing is playing. Returns [`IAX_OK`] or [`IAX_ERR_NULL`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_cancel_dtmf(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.cancel_dtmf();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

fn dtmf_mode_from_ffi(m: IaxDtmfMode) -> DtmfMode {
    match m {
        IaxDtmfMode::InBand => DtmfMode::InBand,
        IaxDtmfMode::Protocol => DtmfMode::Protocol,
    }
}

/// Select how [`iax_station_send_dtmf`] emits digits (iax-7fff): an in-band
/// tone in the TX audio path ([`IaxDtmfMode::InBand`], the default) or
/// out-of-band IAX2 protocol frames ([`IaxDtmfMode::Protocol`]). The mode is
/// stored on the station and applies to the digits sent after the change;
/// setting it while idle is fine. Returns [`IAX_OK`], [`IAX_ERR_NULL`] for a
/// NULL handle, or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_dtmf_mode(
    st: *mut IaxStation,
    mode: IaxDtmfMode,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_dtmf_mode(dtmf_mode_from_ffi(mode));
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the input (TX/mic) gain multiplier (clamped `[0.0, 2.0]`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_input_gain(st: *mut IaxStation, gain: c_float) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_input_gain(gain);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the output (RX/speaker) gain multiplier (clamped `[0.0, 4.0]`:
/// 100%-400% headroom for boosting a quiet station, iax-a4e7).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_output_gain(st: *mut IaxStation, gain: c_float) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_output_gain(gain);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Toggle RX/output compression on the live/next call (iax-a4e7 PHASE 1):
/// automatic leveling of the RECEIVED audio, reusing the mic-path compressor
/// (makeup gain included) on the output bus, applied BEFORE the output gain
/// multiply so the 100%-400% output-gain range amplifies the already-leveled
/// signal. Shared across networks (output is listener-side). Takes effect
/// immediately on an active call's output bus. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_rx_compression(st: *mut IaxStation, on: bool) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_rx_compression(on);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the RX/output compression strength (`level` clamped to `0.0..=1.0`):
/// `0.0` = light, `1.0` = most aggressive, default `0.90`. Takes effect
/// immediately when RX compression is enabled (iax-a4e7 PHASE 1). Returns
/// [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_rx_compression_level(
    st: *mut IaxStation,
    level: c_float,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_rx_compression_level(level);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Toggle mic voice compression on the live/next call. Takes effect immediately
/// on an active call's capture lane. Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL
/// `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_compression(st: *mut IaxStation, on: bool) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_compression(on);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the mic voice-compression strength (`level` clamped to `0.0..=1.0`):
/// `0.0` = light, `1.0` = most aggressive, default `0.90`. Takes effect
/// immediately when compression is enabled. Returns [`IAX_OK`], [`IAX_ERR_NULL`]
/// (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_compression_level(
    st: *mut IaxStation,
    level: c_float,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_compression_level(level);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the TX trim gain (0.0..=2.0 clamped, default 1.0): the final output
/// stage after compression. Attenuates a hot mic that compression makeup gain
/// would otherwise keep loud; values above 1.0 boost (clamped at full scale).
/// Takes effect immediately on the live/next call. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_tx_trim(st: *mut IaxStation, gain: c_float) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_tx_trim(gain);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the live spectrum peak-hold decay in dB/SECOND (`db_per_sec` clamped to
/// `1.0..=500.0`, default `100.0`). Drives the fall-rate of the peak-held
/// spectrum bars shared by the mic monitor, the live-call TX, and the live-call
/// RX analyzers (iax-8616) — a single call scrubs every visible spectrum at
/// once. Applies to the analyzers that are currently live (the mic monitor if
/// monitoring, the active call's TX/RX if a call is up). Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_spectrum_decay(
    st: *mut IaxStation,
    db_per_sec: c_float,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_spectrum_decay(db_per_sec);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the VOX pre-roll / look-back length in milliseconds (`ms` clamped to
/// `0..=250`, `0` = disabled, the default). When software VOX keys the call,
/// the engine flushes this much buffered mic audio ahead of the live stream so
/// the speech onset is not clipped. Takes effect immediately on the active
/// routed mic. Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_vox_preroll_ms(st: *mut IaxStation, ms: c_uint) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_vox_preroll_ms(ms);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Toggle mic noise reduction (denoise) on the live/next call. Takes effect
/// immediately on an active call's capture lane. Returns [`IAX_OK`],
/// [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_noise_reduction(st: *mut IaxStation, on: bool) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.set_noise_reduction(on);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Monitor mode (mic capture without a call)
// ---------------------------------------------------------------------------

/// Start monitor mode (iax-2377): open the capture device and run the mic lane
/// WITHOUT a call so a front-end can preview / characterize the mic before
/// dialing. `input` is a capture-device name substring, or NULL for the system
/// default. Idempotent and call-safe: a no-op if a call is already active (the
/// device is already open) or if a monitor is already running. Stop it with
/// [`iax_station_monitor_stop`]. Returns [`IAX_OK`], [`IAX_ERR_AUDIO`] (device
/// resolve/open failed), [`IAX_ERR_NULL`] (NULL `st`), or [`IAX_ERR_PANIC`].
///
/// NOTE: opens an audio device (blocking); call it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_monitor_start(
    st: *mut IaxStation,
    input: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let input = unsafe { opt_str(input) };
        result_code(station.inner.monitor_start(input.as_deref()))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Stop monitor mode and release the capture device. Idempotent (no-op if not
/// monitoring). Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_monitor_stop(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.monitor_stop();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Copy the live voice-band mic spectrum (iax-e73e) into the caller-allocated
/// `out` array (capacity `cap` floats) and return the number of bins written
/// (`0` while not monitoring), or a negative `IAX_ERR_*`. Each value is a
/// peak-held dBFS magnitude (`-120.0..=0.0`); the bins are log-spaced over the
/// voice band (~100 Hz..3.9 kHz). Poll-only (no callback); the front-end polls
/// ~20 Hz. The values are plain dBFS floats and carry no sensitive data. `out`
/// may be NULL only when `cap == 0` (a no-op that returns 0).
///
/// Returns [`IAX_ERR_NULL`] (NULL `st`, or NULL `out` with `cap > 0`) or
/// [`IAX_ERR_PANIC`] on the boundary panic path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_mic_spectrum(
    st: *mut IaxStation,
    out: *mut c_float,
    cap: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    if out.is_null() && cap > 0 {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        if cap == 0 {
            return 0;
        }
        // Clamp to the most bins we could ever write so a caller that over-states
        // `cap` cannot make us materialize an out-of-bounds slice; the engine
        // never emits more than `IAX_SPECTRUM_BINS`.
        let n_slice = cap.min(IAX_SPECTRUM_BINS);
        let slice = unsafe { std::slice::from_raw_parts_mut(out, n_slice) };
        let n = station.inner.mic_spectrum(slice);
        c_int::try_from(n).unwrap_or(c_int::MAX)
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Copy the live-call **TX** spectrum (iax-2b09) — the audio you are sending on
/// the active network call — into the caller-allocated `out` array (capacity
/// `cap` floats), returning the number of bins written (`0` with no active
/// call), or a negative `IAX_ERR_*`. Same bin contract as
/// [`iax_station_mic_spectrum`] (peak-held dBFS, log-spaced voice band,
/// `IAX_SPECTRUM_BINS` max). Poll-only; carries no sensitive data. `out` may be
/// NULL only when `cap == 0`.
///
/// Returns [`IAX_ERR_NULL`] (NULL `st`, or NULL `out` with `cap > 0`) or
/// [`IAX_ERR_PANIC`] on the boundary panic path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_tx_spectrum(
    st: *mut IaxStation,
    out: *mut c_float,
    cap: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    if out.is_null() && cap > 0 {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        if cap == 0 {
            return 0;
        }
        let n_slice = cap.min(IAX_SPECTRUM_BINS);
        let slice = unsafe { std::slice::from_raw_parts_mut(out, n_slice) };
        let n = station.inner.tx_spectrum(slice);
        c_int::try_from(n).unwrap_or(c_int::MAX)
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Copy the live-call **RX** spectrum (iax-2b09) — the audio received from the
/// far end on the active network call — into the caller-allocated `out` array
/// (capacity `cap` floats), returning the number of bins written (`0` with no
/// active call), or a negative `IAX_ERR_*`. Same bin contract as
/// [`iax_station_mic_spectrum`]. Poll-only; carries no sensitive data. `out` may
/// be NULL only when `cap == 0`.
///
/// Returns [`IAX_ERR_NULL`] (NULL `st`, or NULL `out` with `cap > 0`) or
/// [`IAX_ERR_PANIC`] on the boundary panic path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_rx_spectrum(
    st: *mut IaxStation,
    out: *mut c_float,
    cap: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    if out.is_null() && cap > 0 {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        if cap == 0 {
            return 0;
        }
        let n_slice = cap.min(IAX_SPECTRUM_BINS);
        let slice = unsafe { std::slice::from_raw_parts_mut(out, n_slice) };
        let n = station.inner.rx_spectrum(slice);
        c_int::try_from(n).unwrap_or(c_int::MAX)
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Mic characterization + profile (iax-5fb6 / iax-2095)
// ---------------------------------------------------------------------------

/// Characterize the monitored mic (iax-5fb6) and write the resulting
/// [`crate::IaxStation`]-side `MicProfile` as JSON into the caller buffer `buf`
/// of `len` bytes (NUL-terminated, truncate-safe). Returns the byte length the
/// full JSON needs (excluding the NUL) so the caller can size/retry, or a
/// negative `IAX_ERR_*`. Pass `len == 0` to query the size.
///
/// Requires monitor mode to be running (see [`iax_station_monitor_start`]); call
/// after a few seconds of monitored silence. When not monitoring, writes an
/// empty string and returns 0. `harmonic_comb` toggles harmonic-aware notch
/// detection — **pass `false` for the default flat detector**; `true` enables a
/// learned-fundamental notch comb that catches rolled-off upper harmonics
/// (iax-5fb6; gated here so the comb stays off until validated).
///
/// The JSON carries plain DSP numbers only (high-pass, notch frequencies/Q,
/// noise floor, gate threshold) — no credential fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_characterize(
    st: *mut IaxStation,
    harmonic_comb: bool,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let json = match station.inner.characterize(harmonic_comb) {
            Some(profile) => serde_json::to_string(&profile).unwrap_or_default(),
            None => String::new(),
        };
        unsafe { fill_buf(&json, buf, len) }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Apply (or clear) a calibrated per-mic profile (iax-2095). `json` is a
/// `MicProfile` JSON string as produced by [`iax_station_characterize`] (or
/// persisted by the front-end), or NULL to CLEAR the profile back to the generic
/// noise reducer. A recalled profile rebuilds the live call's noise-reduction
/// comb (and seeds the next call). The JSON carries plain DSP numbers only
/// (high-pass, notch frequencies/Q, noise floor, gate threshold) — no credential
/// fields.
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`), [`IAX_ERR_UTF8`] (non-UTF-8
/// `json`), [`IAX_ERR_AUDIO`] (malformed/invalid profile JSON), or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_mic_profile(
    st: *mut IaxStation,
    json: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        // NULL json clears the profile.
        if json.is_null() {
            station.inner.set_mic_profile(None);
            return IAX_OK;
        }
        let s = match unsafe { req_str(json) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        match serde_json::from_str::<astar_station::MicProfile>(s) {
            Ok(profile) => {
                station.inner.set_mic_profile(Some(profile));
                IAX_OK
            }
            Err(_) => IAX_ERR_AUDIO,
        }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Poll: snapshot + next_event
// ---------------------------------------------------------------------------

/// Fill the caller-allocated `out` with the latest call state. Cheap; poll it.
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`/`out`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_snapshot(st: *mut IaxStation, out: *mut IaxState) -> c_int {
    if st.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let snap = station.inner.snapshot();
        unsafe {
            *out = fill_state(&snap);
        }
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Drain one queued lifecycle event into the caller-allocated `out`. When no
/// event is queued, `out.kind` is set to [`IaxEventKind::None`]. Returns
/// [`IAX_OK`], [`IAX_ERR_NULL`], or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_next_event(st: *mut IaxStation, out: *mut IaxEvent) -> c_int {
    if st.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let ev = station.inner.next_event();
        // Stash the incoming caller id so a Manual-answer caller can read it
        // via `iax_station_incoming_from` after seeing the edge.
        if let Some(StationEvent::IncomingCall { from }) = &ev
            && let Ok(mut slot) = station.last_incoming.lock()
        {
            slot.clear();
            slot.push_str(from);
        }
        let filled = match ev {
            None => IaxEvent {
                kind: IaxEventKind::None,
                remote_ptt: false,
            },
            Some(StationEvent::Answered) => IaxEvent {
                kind: IaxEventKind::Answered,
                remote_ptt: false,
            },
            Some(StationEvent::RemotePtt(on)) => IaxEvent {
                kind: IaxEventKind::RemotePtt,
                remote_ptt: on,
            },
            // The reason string is intentionally not carried across the ABI
            // here; only the edge kind is exposed (it is secret-free).
            Some(StationEvent::Hangup { .. }) => IaxEvent {
                kind: IaxEventKind::Hangup,
                remote_ptt: false,
            },
            // The new mode is not carried across the ABI here; only the edge
            // kind is exposed. Callers read the live mode from a status query.
            Some(StationEvent::ModeChanged(_)) => IaxEvent {
                kind: IaxEventKind::ModeChanged,
                remote_ptt: false,
            },
            // The caller id is not carried across the ABI yet; only the edge
            // kind is exposed. Callers read it from a status query.
            Some(StationEvent::IncomingCall { .. }) => IaxEvent {
                kind: IaxEventKind::Incoming,
                remote_ptt: false,
            },
            Some(StationEvent::Registered) => IaxEvent {
                kind: IaxEventKind::Registered,
                remote_ptt: false,
            },
            // The reason string is intentionally not carried across the ABI
            // here; only the edge kind is exposed (it is secret-free).
            Some(StationEvent::RegisterFailed { .. }) => IaxEvent {
                kind: IaxEventKind::RegisterFailed,
                remote_ptt: false,
            },
        };
        unsafe {
            *out = filled;
        }
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// Enumerate input device names, newline-joined, into the caller buffer `buf`
/// of `len` bytes (NUL-terminated, truncate-safe). Returns the number of bytes
/// the full list needs (excluding the NUL) so the caller can detect truncation,
/// or a negative `IAX_ERR_*` code. Pass `len == 0` to query the required size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_list_inputs(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    unsafe { list_devices_dir(st, buf, len, true) }
}

/// Enumerate output device names, newline-joined, into the caller buffer.
/// Same contract as [`iax_station_list_inputs`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_list_outputs(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    unsafe { list_devices_dir(st, buf, len, false) }
}

unsafe fn list_devices_dir(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
    inputs: bool,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| match station.inner.list_devices() {
        Ok((ins, outs)) => {
            let list = if inputs { ins } else { outs };
            let joined = list.join("\n");
            unsafe { fill_buf(&joined, buf, len) }
        }
        Err(e) => err_code(&e),
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set the capture/playback devices applied to the next connect. NULL selects
/// the system default for that direction. Returns [`IAX_OK`], [`IAX_ERR_NULL`]
/// (NULL `st`), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_devices(
    st: *mut IaxStation,
    input: *const c_char,
    output: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let input = unsafe { opt_str(input) };
        let output = unsafe { opt_str(output) };
        station.inner.set_devices(input, output);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Push-to-talk
//
// PTT *source* is the consumer's concern (a UI button, a spacebar handler, a
// serial device, a HID footswitch, …). This C-ABI exposes only the universal
// `iax_station_set_ptt(st, on)` keying hook plus the snapshot levels; a hardware
// source such as the UCI150 serial PTT lives in a separate, optional, cross-
// platform library that drives keying by calling `iax_station_set_ptt` (iax-0c79).
// Keeping it out of the core keeps this surface portable to every platform,
// including iOS where serial PTT cannot exist.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Error text
// ---------------------------------------------------------------------------

/// Map an `IAX_ERR_*` code (or [`IAX_OK`]) to a `'static`, NUL-terminated,
/// human-readable C string. The returned pointer is owned by the library and
/// must **never** be freed by the caller. The strings are generic and
/// secret-free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_error_text(code: c_int) -> *const c_char {
    let s: &'static [u8] = match code {
        IAX_OK => b"ok\0",
        IAX_ERR_PANIC => b"internal panic\0",
        IAX_ERR_NULL => b"null pointer argument\0",
        IAX_ERR_NOT_CONNECTED => b"no active call\0",
        IAX_ERR_ALREADY_CONNECTED => b"a call is already in progress\0",
        IAX_ERR_PORTAL => b"portal authentication error\0",
        IAX_ERR_RESOLVE => b"node resolution failed\0",
        IAX_ERR_AUDIO => b"audio error\0",
        IAX_ERR_IAX => b"iax error\0",
        IAX_ERR_SERIAL => b"serial error\0",
        IAX_ERR_UTF8 => b"argument was not valid utf-8\0",
        IAX_ERR_UNSUPPORTED => b"operation not supported in this mode\0",
        IAX_ERR_MODE_MISMATCH => b"operation not valid in the current mode\0",
        IAX_ERR_LISTEN => b"listener bind failed\0",
        IAX_ERR_AT_CAPACITY => b"node at capacity\0",
        IAX_ERR_INVALID_DIGIT => b"not a valid DTMF digit\0",
        IAX_ERR_LINK => b"link operation failed\0",
        IAX_ERR_M17 => b"m17 error\0",
        IAX_ERR_DSTAR => b"dstar error\0",
        _ => b"unknown error\0",
    };
    s.as_ptr().cast::<c_char>()
}

#[cfg(test)]
mod fill_state_tests {
    //! Offline coverage of the snapshot mapping (iax-3e53): the negotiated
    //! codec crosses the ABI as its IAX2 format bit, `0` = none.
    use super::fill_state;
    use astar_station::{ConsoleState, VoiceFormat};

    #[test]
    fn negotiated_format_maps_to_the_iax2_format_bit() {
        let mut s = ConsoleState::default();
        assert_eq!(fill_state(&s).negotiated_format, 0, "idle → 0");
        s.negotiated_format = Some(VoiceFormat::G711U);
        assert_eq!(fill_state(&s).negotiated_format, 4, "µ-law → bit 2");
        s.negotiated_format = Some(VoiceFormat::Slin16);
        assert_eq!(fill_state(&s).negotiated_format, 32768, "slin16 → bit 15");
    }
}

// ---------------------------------------------------------------------------
// Link surface (iax-1075): node-to-node links + roster + lifecycle events.
// Same conventions as the rest of the ABI: catch_unwind at the boundary,
// NULL-checked handles, negative IAX_ERR_* codes, JSON for structured reads.
// ---------------------------------------------------------------------------

/// Link mode over the C ABI. Mirrors `astar_iax::LinkMode`.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum IaxLinkMode {
    /// Mic routed + RX mixed: transmit to and hear the peer.
    Transceive = 0,
    /// Hear the peer (and relay onward in conference mode); never transmit.
    Monitor = 1,
    /// Hear the peer on the local speaker only; never relayed, never sent to.
    LocalMonitor = 2,
}

fn link_mode(m: IaxLinkMode) -> LinkMode {
    match m {
        IaxLinkMode::Transceive => LinkMode::Transceive,
        IaxLinkMode::Monitor => LinkMode::Monitor,
        IaxLinkMode::LocalMonitor => LinkMode::LocalMonitor,
    }
}

/// What a drained link event was. `None` means "no event pending".
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IaxLinkEventKind {
    /// No event was pending.
    None = 0,
    /// The link's call reached Active.
    Connected = 1,
    /// The link's call ended / dropped.
    Disconnected = 2,
    /// The link's local PTT changed (see `keyed`).
    Keyed = 3,
}

/// One drained link lifecycle event. The node label is NOT embedded (variable
/// length): read it via [`iax_station_link_event_node`] immediately after a
/// successful drain, before the next [`iax_station_link_next_event`] call.
#[repr(C)]
pub struct IaxLinkEvent {
    /// What happened ([`IaxLinkEventKind::None`] = nothing pending).
    pub kind: IaxLinkEventKind,
    /// The link's opaque call id (matches the roster's `call`).
    pub call: u64,
    /// For [`IaxLinkEventKind::Keyed`]: the new keyed state.
    pub keyed: bool,
}

/// Connect a node link: resolve `node` via the `AllStar` registrar and dial it
/// in `mode`. `secret` is consumed at dial time and never stored. Returns
/// [`IAX_OK`] or a negative `IAX_ERR_*` ([`IAX_ERR_RESOLVE`] /
/// [`IAX_ERR_LINK`] / [`IAX_ERR_AUDIO`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_connect(
    st: *mut IaxStation,
    node: *const c_char,
    mode: IaxLinkMode,
    caller_id: *const c_char,
    secret: *const c_char,
    permanent: bool,
) -> c_int {
    unsafe {
        link_connect_impl(
            st,
            node,
            std::ptr::null(),
            mode,
            caller_id,
            secret,
            permanent,
        )
    }
}

/// [`iax_station_link_connect`] with an explicit `host:port` address instead
/// of registrar resolution (private nodes / test rigs).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_connect_at(
    st: *mut IaxStation,
    node: *const c_char,
    addr: *const c_char,
    mode: IaxLinkMode,
    caller_id: *const c_char,
    secret: *const c_char,
    permanent: bool,
) -> c_int {
    if addr.is_null() {
        return IAX_ERR_NULL;
    }
    unsafe { link_connect_impl(st, node, addr, mode, caller_id, secret, permanent) }
}

unsafe fn link_connect_impl(
    st: *mut IaxStation,
    node: *const c_char,
    addr: *const c_char,
    mode: IaxLinkMode,
    caller_id: *const c_char,
    secret: *const c_char,
    permanent: bool,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    let node = match unsafe { req_str(node) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let caller_id = match unsafe { req_str(caller_id) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let secret = match unsafe { req_str(secret) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let addr = if addr.is_null() {
        None
    } else {
        match unsafe { req_str(addr) } {
            Ok(s) => Some(s),
            Err(c) => return c,
        }
    };
    catch_unwind(AssertUnwindSafe(|| {
        let r = match addr {
            Some(a) => station.inner.link_connect_at(
                node,
                a,
                link_mode(mode),
                caller_id,
                secret,
                CallMode::Standard,
                permanent,
            ),
            None => station.inner.link_connect(
                node,
                link_mode(mode),
                caller_id,
                secret,
                CallMode::Standard,
                permanent,
            ),
        };
        match r {
            Ok(_) => IAX_OK,
            Err(e) => err_code(&e),
        }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Tear a link down by node label. Returns [`IAX_OK`] or [`IAX_ERR_LINK`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_disconnect(
    st: *mut IaxStation,
    node: *const c_char,
) -> c_int {
    unsafe { link_node_op(st, node, Station::link_disconnect) }
}

/// Change a link's mode by node label (Transceive routes the default mic so
/// the link is immediately key-able). Returns [`IAX_OK`] or [`IAX_ERR_LINK`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_set_mode(
    st: *mut IaxStation,
    node: *const c_char,
    mode: IaxLinkMode,
) -> c_int {
    let mode = link_mode(mode);
    unsafe {
        link_node_op(st, node, move |station, node| {
            station.link_set_mode(node, mode)
        })
    }
}

/// Key / unkey a link by node label (refused for non-transmit modes).
/// Returns [`IAX_OK`] or [`IAX_ERR_LINK`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_key(
    st: *mut IaxStation,
    node: *const c_char,
    on: bool,
) -> c_int {
    unsafe { link_node_op(st, node, move |station, node| station.link_key(node, on)) }
}

/// Shared shape of the by-node link ops: NULL checks, UTF-8, `catch_unwind`.
unsafe fn link_node_op(
    st: *mut IaxStation,
    node: *const c_char,
    op: impl FnOnce(&Station, &str) -> Result<(), astar_station::StationError>,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    let node = match unsafe { req_str(node) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    catch_unwind(AssertUnwindSafe(|| match op(&station.inner, node) {
        Ok(()) => IAX_OK,
        Err(e) => err_code(&e),
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Write the live link roster as JSON (`{"links":[{"call":..,"node":"..",
/// "mode":"..","state":"..","keyed":..}]}`) into the caller buffer.
/// Same contract as [`iax_station_list_inputs`]: NUL-terminated + truncate-
/// safe; returns the byte length the full JSON needs (a `len == 0` call is a
/// sizing query), or a negative `IAX_ERR_*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_roster_json(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let roster = station.inner.link_roster();
        match serde_json::to_string(&roster) {
            Ok(json) => unsafe { fill_buf(&json, buf, len) },
            Err(_) => IAX_ERR_IAX,
        }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Drain the next pending link lifecycle event. Returns 1 and fills `out`
/// when an event was pending (read the node label via
/// [`iax_station_link_event_node`] before the next drain), 0 when none, or a
/// negative `IAX_ERR_*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_next_event(
    st: *mut IaxStation,
    out: *mut IaxLinkEvent,
) -> c_int {
    if st.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let Some(ev) = station.inner.next_link_event() else {
            unsafe {
                (*out).kind = IaxLinkEventKind::None;
                (*out).call = 0;
                (*out).keyed = false;
            }
            return 0;
        };
        let (kind, node, call, keyed) = match ev {
            astar_station::LinkEvent::Connected { node, call } => {
                (IaxLinkEventKind::Connected, node, call, false)
            }
            astar_station::LinkEvent::Disconnected { node, call, .. } => {
                (IaxLinkEventKind::Disconnected, node, call, false)
            }
            astar_station::LinkEvent::Keyed { node, call, keyed } => {
                (IaxLinkEventKind::Keyed, node, call, keyed)
            }
        };
        *station.last_link_node.lock().expect("link node mutex") = node;
        unsafe {
            (*out).kind = kind;
            (*out).call = call;
            (*out).keyed = keyed;
        }
        1
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Node label of the most recently drained link event (NUL-terminated into
/// the caller buffer; same contract as [`iax_station_incoming_from`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_link_event_node(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let node = station.last_link_node.lock().expect("link node mutex");
        unsafe { fill_buf(&node, buf, len) }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// WireGuard link transport (iax-912e)
// ---------------------------------------------------------------------------

/// The fixed private-key *reference* this ABI hands to the engine. The caller's
/// key string is wrapped Rust-side in a one-shot resolver answering only for
/// this reference (the house secret rule); engine diagnostics name the
/// reference, never the material.
const WG_FFI_KEY_REF: &str = "wireguard-private-key";

/// `WireGuard` link-transport configuration passed to
/// [`iax_station_set_wireguard`]. All fields are **borrowed** `const char*`
/// (the library copies what it needs; the caller keeps ownership and may free
/// them right after the call returns).
///
/// `endpoint`, `peer_public_key`, `tunnel_ip`, and `private_key` are required
/// (NULL → [`IAX_ERR_NULL`]). `allowed_ips` and `also_bind_udp` are optional
/// (NULL = none); `keepalive_secs` `0` selects the library default (25 s).
///
/// The `private_key` is an **in-param** consumed into the engine at tunnel
/// build time via a one-shot resolver; it is never stored in any
/// config/snapshot/event/log type, never echoed back, and never appears in an
/// error string.
#[repr(C)]
pub struct IaxWireguardConfig {
    /// Peer (`WireGuard` server) endpoint `"host:port"` — an IP literal or a
    /// DNS name. Required.
    pub endpoint: *const c_char,
    /// The peer's public key, base64 (32 bytes encoded). Required.
    pub peer_public_key: *const c_char,
    /// This station's tunnel address in IPv4 CIDR form (e.g. `"10.99.0.2/32"`).
    /// Required; the tunnel-inner network is IPv4-only.
    pub tunnel_ip: *const c_char,
    /// Comma-separated allowed-IPs CIDRs for the peer (advisory in the
    /// userspace stack), or NULL for none. Whitespace around entries is
    /// tolerated.
    pub allowed_ips: *const c_char,
    /// Persistent keepalive interval in seconds, or `0` for the default (25).
    pub keepalive_secs: u16,
    /// This station's `WireGuard` private key, base64 (32 bytes encoded).
    /// Required. Consumed; never stored, echoed, or logged.
    pub private_key: *const c_char,
    /// Optional plain (non-tunnel) OS UDP listener address `"host:port"` the
    /// engine binds ALONGSIDE the tunnel listener for direct/LAN peers, or
    /// NULL for none.
    pub also_bind_udp: *const c_char,
}

/// Map a `WireGuard` config-validation error to the established codes: an
/// unresolvable endpoint is [`IAX_ERR_RESOLVE`] (the address-resolution code
/// [`iax_station_set_node_config`] also uses); a bad key/CIDR is
/// [`IAX_ERR_LINK`] (invalid link-transport config). The messages are dropped
/// at this boundary — only the code crosses.
fn wg_err_code(e: &WgConfigError) -> c_int {
    match e {
        WgConfigError::Endpoint(_) => IAX_ERR_RESOLVE,
        WgConfigError::Key(_) | WgConfigError::Address(_) | WgConfigError::AllowedIp(_) => {
            IAX_ERR_LINK
        }
    }
}

/// Route the whole engine — outgoing dials, the inbound listener, and outbound
/// registration — through one shared userspace `WireGuard` tunnel (iax-912e).
/// Call BEFORE connect/enable-inbound: the transport is immutable while a
/// session is up (switching = disconnect/reconnect, then set again). Never
/// calling this is byte-identical to plain OS UDP.
///
/// A NULL `cfg` clears the transport back to plain OS UDP (the inverse
/// operation, same immutability rule). `cfg` and its strings are borrowed for
/// the duration of the call; the `private_key` is consumed via a one-shot
/// resolver and never stored (see [`IaxWireguardConfig`]).
///
/// Returns [`IAX_OK`], [`IAX_ERR_NULL`] (NULL `st`, or a required field NULL),
/// [`IAX_ERR_UTF8`], [`IAX_ERR_RESOLVE`] (unresolvable `endpoint` /
/// unparseable `also_bind_udp`), [`IAX_ERR_LINK`] (bad key/CIDR),
/// [`IAX_ERR_ALREADY_CONNECTED`] (a call is pooled), or [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_wireguard(
    st: *mut IaxStation,
    cfg: *const IaxWireguardConfig,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        if cfg.is_null() {
            return result_code(station.inner.clear_wireguard());
        }
        let cfg = unsafe { &*cfg };
        let endpoint = match unsafe { req_str(cfg.endpoint) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let peer_public_key = match unsafe { req_str(cfg.peer_public_key) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let tunnel_ip = match unsafe { req_str(cfg.tunnel_ip) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let private_key = match unsafe { req_str(cfg.private_key) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let allowed_ips: Vec<String> = if cfg.allowed_ips.is_null() {
            Vec::new()
        } else {
            match unsafe { req_str(cfg.allowed_ips) } {
                Ok(s) => s
                    .split(',')
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string)
                    .collect(),
                Err(c) => return c,
            }
        };
        let also_bind_udp = if cfg.also_bind_udp.is_null() {
            None
        } else {
            match unsafe { req_str(cfg.also_bind_udp) } {
                Ok(s) => match s.parse() {
                    Ok(a) => Some(a),
                    Err(_) => return IAX_ERR_RESOLVE,
                },
                Err(c) => return c,
            }
        };
        let keepalive = if cfg.keepalive_secs == 0 {
            25
        } else {
            cfg.keepalive_secs
        };
        let wg = match WgLinkConfig::new(
            WG_FFI_KEY_REF,
            tunnel_ip,
            peer_public_key,
            endpoint,
            &allowed_ips,
            keepalive,
        ) {
            Ok(c) => c.with_also_bind_udp(also_bind_udp),
            Err(e) => return wg_err_code(&e),
        };
        result_code(station.inner.set_wireguard(wg, private_key))
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// M17 (iax-f2b8 Task 5)
// ---------------------------------------------------------------------------

/// Connect to an M17 reflector (iax-f2b8 Task 4/5): resolves `host`/`port`
/// and opens a full-transceive session on `module`, mutually exclusive with
/// an active IAX2 call. See [`astar_station::Station::m17_connect`] for
/// the full contract (module case-folding, codec-dir search, exclusivity).
///
/// `host` and `callsign` are required (NULL/non-UTF-8 → [`IAX_ERR_NULL`] /
/// [`IAX_ERR_UTF8`]). `module` must be a single ASCII byte; a non-ASCII byte
/// is rejected here with [`IAX_ERR_M17`] before it ever reaches the station
/// — the remaining `A`-`Z` validation (and an empty `callsign`) is caught by
/// the station and also maps to [`IAX_ERR_M17`]. Returns [`IAX_OK`],
/// [`IAX_ERR_ALREADY_CONNECTED`] (an IAX2 call is live), [`IAX_ERR_M17`], or
/// [`IAX_ERR_PANIC`].
///
/// NOTE: this performs blocking work (device resolve, socket bind, session
/// spawn); call it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_connect_m17(
    st: *mut IaxStation,
    host: *const c_char,
    port: u16,
    module: c_char,
    callsign: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let host = match unsafe { req_str(host) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let callsign = match unsafe { req_str(callsign) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        // c_char may be signed; reinterpret the raw byte (no sign loss) — a
        // non-ASCII byte is never a valid module letter. The remaining A-Z
        // check is the station's job (`Station::m17_connect`).
        let byte = module.to_ne_bytes()[0];
        if !byte.is_ascii() {
            return IAX_ERR_M17;
        }
        result_code(
            station
                .inner
                .m17_connect(host, port, char::from(byte), callsign),
        )
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Disconnect the live M17 session, if any (iax-f2b8 Task 4/5). Idempotent —
/// a no-op while idle. Returns [`IAX_OK`], [`IAX_ERR_NULL`], or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_m17_disconnect(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.m17_disconnect();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Set extra directories to search for a runtime `libcodec2`, ahead of the
/// hard-coded system paths (iax-f2b8 Task 4/5). `dirs` is a single
/// NUL-terminated string of `':'`-separated filesystem paths (e.g.
/// `"/opt/app/lib:/usr/local/lib"`); NULL or empty clears the list back to
/// the system search paths only. Call before [`iax_station_connect_m17`] —
/// it does not affect a session already in progress. Returns [`IAX_OK`],
/// [`IAX_ERR_UTF8`] (non-UTF-8 `dirs`), [`IAX_ERR_NULL`] (NULL `st`), or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_set_codec_dirs(
    st: *mut IaxStation,
    dirs: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        // NULL means "clear" here (unlike `req_str`'s required-argument
        // contract), so this validates UTF-8 by hand rather than delegating.
        let dirs = if dirs.is_null() {
            ""
        } else {
            match unsafe { CStr::from_ptr(dirs) }.to_str() {
                Ok(s) => s,
                Err(_) => return IAX_ERR_UTF8,
            }
        };
        let paths = dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        station.inner.set_codec_dirs(paths);
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// D-Star (iax-4c8e)
// ---------------------------------------------------------------------------

/// Connect to a D-Star `DExtra` reflector (iax-a9d4 built RX, iax-2f6b added
/// TX): resolves `host`/`port` and opens a full-transceive session on
/// `module`, mutually exclusive with both an active IAX2 call and a live M17
/// session. See [`astar_station::Station::dstar_connect`] for the full
/// contract.
///
/// D-Star is HARDWARE-ONLY: the vocoder is a DVSI `ThumbDV`, with no software
/// fallback, so this fails when no dongle is attached. Poll
/// [`IaxSnapshot::dstar_available`] and offer the affordance only when it is
/// `true`, rather than calling this speculatively.
///
/// `host` and `callsign` are required (NULL/non-UTF-8 → [`IAX_ERR_NULL`] /
/// [`IAX_ERR_UTF8`]). `module` must be a single ASCII byte; a non-ASCII byte
/// is rejected here with [`IAX_ERR_DSTAR`] before it reaches the station —
/// the remaining `A`-`Z` validation (and an empty `callsign`) is caught by
/// the station and also maps to [`IAX_ERR_DSTAR`]. Returns [`IAX_OK`],
/// [`IAX_ERR_ALREADY_CONNECTED`] (an IAX2 call or M17 session is live),
/// [`IAX_ERR_DSTAR`], or [`IAX_ERR_PANIC`].
///
/// NOTE: this performs blocking work — a serial-port scan plus, per candidate
/// port and baud rate, an open and an eight-transaction dongle init, then a
/// socket bind and session spawn. It can take on the order of a second. Call
/// it off any UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_connect_dstar(
    st: *mut IaxStation,
    host: *const c_char,
    port: u16,
    module: c_char,
    callsign: *const c_char,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let host = match unsafe { req_str(host) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        let callsign = match unsafe { req_str(callsign) } {
            Ok(s) => s,
            Err(c) => return c,
        };
        // c_char may be signed; reinterpret the raw byte (no sign loss) — a
        // non-ASCII byte is never a valid module letter. The remaining A-Z
        // check is the station's job (`Station::dstar_connect`).
        let byte = module.to_ne_bytes()[0];
        if !byte.is_ascii() {
            return IAX_ERR_DSTAR;
        }
        result_code(
            station
                .inner
                .dstar_connect(host, port, char::from(byte), callsign),
        )
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Disconnect the live D-Star session, if any (iax-4c8e). Idempotent — a
/// no-op while idle. Returns [`IAX_OK`], [`IAX_ERR_NULL`], or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_dstar_disconnect(st: *mut IaxStation) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        station.inner.dstar_disconnect();
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Write the live D-Star session's state as JSON into the caller buffer `buf`
/// of `len` bytes (NUL-terminated, truncate-safe; same contract as
/// [`iax_station_list_inputs`] — returns the byte length the full JSON needs,
/// excluding the NUL, so a `len == 0` call is a sizing query).
///
/// Carries the D-Star-shaped state that has no equivalent in
/// [`IaxSnapshot`]'s network-agnostic fields:
///
/// ```json
/// {"link":"linked","talker":"AJ7HR","slow_text":"hi","backend":"thumbdv",
///  "tx_capable":true,"ptt":false,"tx_db":-60.0,"rx_db":-31.2,"input_db":-45.0}
/// ```
///
/// `link` is one of `idle`/`linking`/`linked`/`unlinking`/`failed`; `backend`
/// is `thumbdv` (or `soft`, which D-Star never uses today). `talker` and
/// `slow_text` are `null` until a header / a complete slow-data message has
/// been received, and then PERSIST past end-of-transmission — they are
/// "most recently heard", not "currently transmitting". Every field is
/// credential-free: callsigns and levels only.
///
/// Writes `{}` when no session is active or the `dstar` feature isn't
/// compiled in. Returns [`IAX_ERR_NULL`] if `st` is NULL, or
/// [`IAX_ERR_PANIC`].
///
/// The three level fields duplicate [`IaxSnapshot`]'s `tx_db`/`rx_db`/
/// `input_db`, which mirror the active session whatever its network — read
/// them from the snapshot on a metering tick and call this only when the UI
/// needs the D-Star-specific fields.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_station_dstar_state(
    st: *mut IaxStation,
    buf: *mut c_char,
    len: usize,
) -> c_int {
    if st.is_null() {
        return IAX_ERR_NULL;
    }
    let station = unsafe { &*st };
    catch_unwind(AssertUnwindSafe(|| {
        let json = dstar_state_json(station);
        unsafe { fill_buf(&json, buf, len) }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Render the live D-Star session's state as JSON, or `"{}"` when there is no
/// session (or no `dstar` feature). Split out of
/// [`iax_station_dstar_state`] so it is reachable from tests without an FFI
/// buffer dance.
fn dstar_state_json(station: &IaxStation) -> String {
    #[cfg(feature = "dstar")]
    {
        let Some(s) = station.inner.dstar_state() else {
            return "{}".to_string();
        };
        // Built through serde_json rather than `format!` so a callsign or
        // slow-data message containing a quote or backslash cannot break out
        // of the string and corrupt the document. Slow data is attacker-
        // supplied — it arrives from whoever is transmitting on the
        // reflector.
        serde_json::json!({
            "link": s.link.as_str(),
            "talker": s.talker,
            "slow_text": s.slow_text,
            "backend": s.backend.map(astar_station::AmbeBackend::as_str),
            "tx_capable": s.tx_capable,
            "ptt": s.ptt,
            "tx_db": s.tx_dbfs,
            "rx_db": s.rx_dbfs,
            "input_db": s.input_dbfs,
        })
        .to_string()
    }
    #[cfg(not(feature = "dstar"))]
    {
        let _ = station;
        "{}".to_string()
    }
}

#[cfg(test)]
mod resolver_bridge_tests {
    //! Offline coverage of the secret bridge: a C resolver callback's output
    //! must surface as the resolved secret, and a non-zero return must yield an
    //! empty secret (never garbage from the buffer).
    use super::{IAX_OK, ResolverCtx};
    use std::ffi::{c_char, c_int, c_void};

    /// A resolver that writes a fixed secret regardless of `user`.
    extern "C" fn good(
        _user: *const c_char,
        out: *mut c_char,
        cap: usize,
        _data: *mut c_void,
    ) -> c_int {
        let secret = b"be04-node-secret";
        assert!(cap > secret.len());
        unsafe {
            std::ptr::copy_nonoverlapping(secret.as_ptr(), out.cast::<u8>(), secret.len());
            *out.add(secret.len()) = 0;
        }
        IAX_OK
    }

    /// A resolver that signals "no secret" by returning non-zero. It also dirties
    /// `out` to prove the bridge does not read it on failure.
    extern "C" fn deny(
        _user: *const c_char,
        out: *mut c_char,
        cap: usize,
        _data: *mut c_void,
    ) -> c_int {
        // Dirty `out` (non-zero, no NUL) to prove the bridge ignores it on
        // failure. `1` is representable whether c_char is i8 or u8.
        unsafe {
            for i in 0..cap {
                *out.add(i) = 1;
            }
        }
        -1
    }

    #[test]
    fn good_resolver_delivers_secret() {
        let ctx = ResolverCtx {
            f: good,
            data: std::ptr::null_mut(),
        };
        assert_eq!(ctx.resolve("77777"), "be04-node-secret");
    }

    #[test]
    fn denied_resolver_yields_empty_not_garbage() {
        let ctx = ResolverCtx {
            f: deny,
            data: std::ptr::null_mut(),
        };
        assert_eq!(ctx.resolve("77777"), "");
    }
}
