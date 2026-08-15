// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! AMBE vocoder — the D-Star payload codec (iax-a9d4).
//!
//! One backend, opt-in via the `ambe-hw` Cargo feature: it talks to a
//! `ThumbDV` USB dongle (`ambe-thumbdv`) that runs the real DVSI AMBE3000
//! chip, over a dedicated worker thread — all serial/USB I/O stays off the
//! calling (audio) thread (iax-239a). D-Star here is hardware-only; there is
//! no software vocoder and no fallback.
//!
//! # Licensing / feature story (read before touching `Cargo.toml`)
//!
//! `ambe-hw` is not in this crate's default feature set — that's what keeps a
//! plain `cargo build -p astar-codec` free of the vocoder driver. The
//! `ambe-thumbdv` dependency is a path dependency on this repo's vendored
//! copy (`vendor/ambe-thumbdv`, MIT/Apache-2.0), so the workspace builds
//! offline with no git dependency. Verify with:
//!
//! ```sh
//! cargo tree -p astar-codec                  # no ambe-thumbdv
//! cargo tree -p astar-codec --features ambe-hw
//! ```
//!
//! Frames are 9 bytes (72 bits) in, 160 PCM samples (20 ms @ 8 kHz) out —
//! the D-Star full-rate AMBE frame.

/// A live AMBE encoder/decoder instance for D-Star's full-rate frame: 160
/// PCM samples (20 ms @ 8 kHz) encode to / decode from 9 bytes.
pub trait AmbeVoice: Send {
    /// Decode one 9-byte channel frame into one 20 ms frame of 16-bit
    /// linear PCM.
    fn decode(&mut self, frame: &[u8; 9]) -> [i16; 160];
    /// Encode one 20 ms frame of 16-bit linear PCM into a 9-byte channel
    /// frame.
    fn encode(&mut self, pcm: &[i16; 160]) -> [u8; 9];
}

/// A streaming AMBE decoder (iax-b3e7 M0): frames can be submitted ahead of
/// when their decoded audio is needed, so a caller can keep several decode
/// requests working concurrently instead of waiting for each one to return
/// before submitting the next. This is what makes real-time D-Star playback
/// possible against the `ThumbDV`: `AmbeVoice::decode`'s stop-and-wait shape
/// measures ~24.5 ms mean / 27.1 ms max per frame against D-Star's 20 ms
/// cadence (cannot sustain a stream), while the same chip driven pipelined
/// measures ~7.45 ms/frame.
///
/// In-flight depth (frames submitted, not yet retrieved via
/// [`poll_decoded`](AmbeStream::poll_decoded)) is bounded at
/// [`AMBE_STREAM_MAX_IN_FLIGHT`]. Once that many are outstanding,
/// `submit_decode` drops the *incoming* frame rather than the oldest: for a
/// hardware backend, everything already submitted may already be written to
/// the device and cannot be recalled, so the newest frame is the only one
/// that can safely be dropped.
pub trait AmbeStream: Send {
    /// Queue a frame for decoding. Never blocks on the device; when the
    /// pipeline is already at [`AMBE_STREAM_MAX_IN_FLIGHT`] this drops the
    /// incoming frame (logged via `tracing::warn!`) rather than submitting
    /// it.
    fn submit_decode(&mut self, frame: [u8; 9]);
    /// Take the next decoded frame, if one has come back yet.
    fn poll_decoded(&mut self) -> Option<[i16; 160]>;
    /// Frames currently in flight: submitted but not yet returned by
    /// `poll_decoded`.
    fn in_flight(&self) -> usize;

    /// Queue one 20 ms frame of PCM for encoding (iax-2f6b TX). Never blocks
    /// on the device; when the encode pipeline is already at
    /// [`AMBE_STREAM_MAX_IN_FLIGHT`] this drops the incoming PCM (logged via
    /// `tracing::warn!`) rather than submitting it — same drop-newest
    /// contract as [`submit_decode`](AmbeStream::submit_decode), for the same
    /// reason: once a frame is written to the device it cannot be recalled,
    /// so only the newest, not-yet-written frame is ever safe to drop. This
    /// is a distinct pipeline from the decode side, with its own bound and
    /// its own in-flight accounting ([`in_flight_encoded`](AmbeStream::in_flight_encoded)):
    /// a caller may have decode and encode requests outstanding at the same
    /// time without either affecting the other's backpressure.
    fn submit_encode(&mut self, pcm: [i16; 160]);
    /// Take the next encoded 9-byte channel frame, if one has come back yet.
    fn poll_encoded(&mut self) -> Option<[u8; 9]>;
    /// Frames currently in flight on the encode side: submitted but not yet
    /// returned by `poll_encoded`. Independent of [`in_flight`](AmbeStream::in_flight),
    /// which tracks the decode side only.
    fn in_flight_encoded(&self) -> usize;
}

/// Bound on outstanding (submitted, not yet retrieved) frames for any
/// [`AmbeStream`] implementation. Matches this crate's measured pipeline
/// cushion for the `ThumbDV` — enough in-flight depth to keep the serial link
/// saturated (~7.45 ms/frame pipelined vs. the 20 ms D-Star cadence)
/// without unbounded growth if a consumer falls behind.
pub const AMBE_STREAM_MAX_IN_FLIGHT: usize = 4;

/// Which concrete implementation backed an [`AmbeVoice`] returned by
/// [`open_ambe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbeBackend {
    /// `ThumbDV` USB hardware dongle (`ambe-hw` feature).
    ///
    /// The only backend. A software AMBE variant existed once; D-Star is
    /// hardware-only now, and this stays an enum so the ABI string below and
    /// the `Option<AmbeBackend>` preference API keep their shape.
    Hardware,
}

impl AmbeBackend {
    /// A stable lowercase name for this backend.
    ///
    /// This is an ABI string, not a debug convenience: it crosses the C-ABI
    /// in the D-Star state JSON (iax-4c8e) and is matched by name in
    /// front-ends, so treat these strings as fixed even if a variant is
    /// renamed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AmbeBackend::Hardware => "thumbdv",
        }
    }
}

/// `IAX_THUMBDV_PORT` env var: pins `ThumbDV` detection to one specific serial
/// device path when several `ThumbDV`-class devices are attached and the scan
/// would otherwise need disambiguating (iax-b3e7 spec §4). Checked BEFORE
/// [`ambe_thumbdv::detect`]'s own scan (via [`detect_thumbdv`]) and before
/// [`open_hw_stream_from_candidate_ports`]'s equivalent scan — mirrors
/// `IAX_CODEC2_PATH`'s precedent in this crate's `codec2.rs`.
///
/// # ON-AIR SAFETY — this override does NOT widen the VID/PID filter
///
/// The pinned path still has to appear in
/// [`ambe_thumbdv::SerialTransport::candidate_ports`]'s FTDI `0x0403`/`0x6015`
/// (or `"ThumbDV"` product-string) scan; see
/// [`thumbdv_candidate_ports_from`]. A path that doesn't match is REFUSED
/// (empty candidate list + a `tracing::error!`), never opened. That filter is
/// what keeps every opener in this crate away from a USB radio interface's
/// serial port — opening one of those asserts RTS, which on the reference
/// `AllScan` UCI150 is the transmitter keying line. So the override selects
/// *which* `ThumbDV`, which is all spec §4 asks for; it can never be used to
/// point this crate at a radio interface (deliberately or by typo).
#[cfg(feature = "ambe-hw")]
fn thumbdv_port_override() -> Option<String> {
    std::env::var("IAX_THUMBDV_PORT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// The candidate serial-port list any `ThumbDV` opener should scan: the
/// VID/PID-filtered [`ambe_thumbdv::SerialTransport::candidate_ports`] scan
/// `detect()` uses internally, narrowed to [`thumbdv_port_override`]'s single
/// pinned path when `IAX_THUMBDV_PORT` is set.
#[cfg(feature = "ambe-hw")]
fn thumbdv_candidate_ports() -> Vec<String> {
    thumbdv_candidate_ports_from(
        thumbdv_port_override().as_deref(),
        &ambe_thumbdv::SerialTransport::candidate_ports(),
    )
}

/// The candidate-list policy itself, with the override and the real VID/PID
/// scan both injected — the hardware-free unit-test seam for
/// [`thumbdv_candidate_ports`].
///
/// The override NARROWS the scan, it never replaces it: a pinned path is
/// returned only if the scan already found it. Anything else yields an empty
/// candidate list, so no opener in this crate ever touches the pinned path.
/// See [`thumbdv_port_override`]'s "ON-AIR SAFETY" section for why that
/// property is load-bearing and must not regress.
#[cfg(feature = "ambe-hw")]
fn thumbdv_candidate_ports_from(pinned: Option<&str>, scanned: &[String]) -> Vec<String> {
    let Some(pinned) = pinned else {
        return scanned.to_vec();
    };
    if scanned.iter().any(|p| p == pinned) {
        return vec![pinned.to_string()];
    }
    tracing::error!(
        "IAX_THUMBDV_PORT={pinned} is not a ThumbDV: no serial port with that path matched the \
         FTDI 0x0403:0x6015 / \"ThumbDV\" scan ({scanned:?}). Refusing to open it — pointing this \
         at a USB radio interface's port would assert RTS and key a transmitter."
    );
    Vec::new()
}

/// Detect and initialize a `ThumbDV`, honoring [`thumbdv_port_override`]
/// first: when `IAX_THUMBDV_PORT` pins one of the scanned candidates, this
/// bypasses [`ambe_thumbdv::detect`]'s own scan and opens/initializes exactly
/// that path (trying both of `detect`'s supported baud rates, same fallback
/// order). Without the override, delegates straight to
/// [`ambe_thumbdv::detect`]. A pinned path the VID/PID scan didn't find is
/// refused outright — see [`thumbdv_candidate_ports_from`].
#[cfg(feature = "ambe-hw")]
fn detect_thumbdv()
-> Result<ambe_thumbdv::ThumbDv<ambe_thumbdv::SerialTransport>, ambe_thumbdv::DeviceError> {
    if thumbdv_port_override().is_none() {
        return ambe_thumbdv::detect();
    }
    for path in thumbdv_candidate_ports() {
        for baud in [460_800u32, 230_400u32] {
            if let Ok(transport) = ambe_thumbdv::SerialTransport::open(&path, baud)
                && let Ok(dev) = ambe_thumbdv::ThumbDv::init_with(transport)
            {
                return Ok(dev);
            }
        }
    }
    Err(ambe_thumbdv::DeviceError::Timeout(
        "IAX_THUMBDV_PORT device did not initialize",
    ))
}

/// `true` when a `ThumbDV`-class serial port is attached — the VID/PID scan
/// only, with NO device open at all (nothing is initialized, no port is
/// held, nothing can be keyed). This is the right probe for a UI
/// availability indicator: it answers "is the dongle plugged in" without
/// contending for a port a live D-Star session may already own, which a
/// real open-based probe cannot do (only one process may hold the device).
#[cfg(feature = "ambe-hw")]
#[must_use]
pub fn thumbdv_present() -> bool {
    !thumbdv_candidate_ports().is_empty()
}

/// Distinguishes *why* `ThumbDV` detection just failed, since
/// [`ambe_thumbdv::detect`] collapses "no dongle" and "dongle busy" into the
/// same `DeviceError::Timeout("no ThumbDV found")` (iax-b3e7 spec §4) —
/// not useful for an operator staring at `dstar-listen`'s output. Classified
/// entirely on this side, with zero changes to the `ambe-thumbdv` crate:
/// [`ambe_thumbdv::SerialTransport::candidate_ports`] plus a trial open of
/// each candidate tells "no candidates" (unplugged) apart from "a candidate
/// exists but won't open because something else already has it" (busy).
#[cfg(feature = "ambe-hw")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbDvFailure {
    /// No serial port matching the `ThumbDV`'s VID/PID (or the
    /// `IAX_THUMBDV_PORT` override) was found at all.
    Unplugged,
    /// A candidate port exists but opening it failed with `EBUSY`
    /// (`io::ErrorKind::ResourceBusy`) or permission-denied — read as
    /// "something else already has it open."
    Busy {
        /// The port path that's busy, named directly in the message so an
        /// operator with several USB-serial devices knows which one.
        port: String,
    },
    /// A candidate port exists but failed to open/initialize for some other
    /// reason (wrong device, a transient I/O error, etc.) — `detect()`'s own
    /// error is the most useful thing to log in this case, but this crate
    /// doesn't have it any more by the time this classification runs (see
    /// [`classify_thumbdv_failure`]'s doc), so this variant renders a
    /// deliberately generic message rather than fabricating specifics.
    Other,
}

#[cfg(feature = "ambe-hw")]
impl ThumbDvFailure {
    /// Human-readable, secret-free message naming the specific failure —
    /// what `dstar-listen` (and any other D-Star connect caller) should
    /// print instead of a generic "no `ThumbDV` found."
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            ThumbDvFailure::Unplugged => {
                "no ThumbDV detected — plug in the dongle and try again".to_string()
            }
            ThumbDvFailure::Busy { port } => {
                format!("ThumbDV at {port} is busy — another process has it open")
            }
            ThumbDvFailure::Other => {
                "ThumbDV detected but failed to initialize (wrong device or a transient error)"
                    .to_string()
            }
        }
    }
}

/// Classify why a `detect_thumbdv()`/`open_hw_stream_from_candidate_ports`
/// call just failed, using the real [`thumbdv_candidate_ports`] list plus a
/// real trial open of each candidate (immediately dropped either way — this
/// must never hold a port a moment longer than needed to observe the
/// `open()` result; the hardware-safety rule in play here is "only one
/// process may hold the device," and a classification probe is not an
/// exception to it).
#[cfg(feature = "ambe-hw")]
#[must_use]
pub fn classify_thumbdv_failure() -> ThumbDvFailure {
    let candidates = thumbdv_candidate_ports();
    classify_thumbdv_failure_with(&candidates, |port| {
        // Deliberately a raw filesystem open, NOT `SerialTransport::open`:
        // `serialport` sets `TIOCEXCL` on every successful open (posix
        // `tty.rs`) and, when TIOCEXCL is already held by someone else,
        // maps the resulting `EBUSY` to its OWN `ErrorKind::NoDevice` (see
        // `serialport`'s `posix/error.rs` — a deliberate remap, not a bug)
        // rather than `Io`. `ambe_thumbdv`'s `map_serialport_error` only
        // preserves `serialport::ErrorKind::Io(..)`; `NoDevice` falls
        // through its `_` arm into a generic `io::Error::other(..)`, so by
        // the time `SerialTransport::open`'s `io::Result` reaches us
        // `.kind()` is `Other` — indistinguishable from any other failure.
        // A plain `OpenOptions::open` still trips the same `TIOCEXCL` lock
        // (it's enforced by the OS against ANY subsequent opener, not just
        // other `serialport` users) but goes through std's own `io::Error`
        // conversion, which maps `EBUSY` to `ResourceBusy` and `EACCES` to
        // `PermissionDenied` directly, with no crate-specific remapping in
        // the way. Confirmed against the real dongle (this spec item's
        // permitted "ONE brief real check"): opening it a second time from
        // this same process while the first handle is still held measures
        // `ErrorKind::ResourceBusy` (`raw_os_error 16`, "Resource busy")
        // through `OpenOptions::open`, vs. `ErrorKind::Other` ("Device or
        // resource busy") through `SerialTransport::open` for the identical
        // contention. The baud rate a real hardware transaction would use
        // is irrelevant here — this only asks whether the OS will let the
        // port open at all, immediately dropping the handle either way.
        trial_open(port)
    })
}

/// One non-blocking trial open of a serial device, used only by
/// [`classify_thumbdv_failure`] to observe the `open()` result and
/// immediately close again.
///
/// `O_NONBLOCK | O_NOCTTY` matter: a POSIX `open()` on a terminal device
/// without them blocks in the kernel's `tty_port_block_til_ready` until
/// carrier (DCD) is asserted. This runs on `DstarSession::connect`'s FAILURE
/// path, so a dongle whose driver never raises DCD would turn the spec §4
/// error message into an indefinite hang — exactly the "a wedged transfer
/// must surface as an error, never a UI hang" property iax-239a established.
/// `EBUSY`/`EACCES` still surface unchanged, which is all the classification
/// reads.
#[cfg(feature = "ambe-hw")]
fn trial_open(port: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The libc values for the two unix families this crate targets,
        // inlined rather than taking a `libc` dependency on a codec crate
        // for two integers. Apple/BSD: O_NONBLOCK 0x0004, O_NOCTTY 0x20000.
        // Linux (asm-generic): O_NONBLOCK 0o4000, O_NOCTTY 0o400.
        #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd"))]
        const FLAGS: i32 = 0x0004 | 0x0002_0000;
        #[cfg(not(any(target_vendor = "apple", target_os = "freebsd", target_os = "netbsd")))]
        const FLAGS: i32 = 0o4000 | 0o400;

        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FLAGS)
            .open(port)
            .map(drop)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(port)
            .map(drop)
    }
}

/// The classification logic itself, with the candidate list and the "try to
/// open this port" outcome both injected — the hardware-free unit-test seam
/// for [`classify_thumbdv_failure`].
#[cfg(feature = "ambe-hw")]
fn classify_thumbdv_failure_with(
    candidates: &[String],
    mut try_open: impl FnMut(&str) -> std::io::Result<()>,
) -> ThumbDvFailure {
    if candidates.is_empty() {
        return ThumbDvFailure::Unplugged;
    }
    for port in candidates {
        if let Err(e) = try_open(port)
            && is_busy_or_permission_denied(&e)
        {
            return ThumbDvFailure::Busy { port: port.clone() };
        }
        // Either it opened fine (oddly, given the caller just had a
        // detect/open failure to diagnose) or it failed for some other
        // reason — either way, not busy; keep looking at the rest.
    }
    ThumbDvFailure::Other
}

#[cfg(feature = "ambe-hw")]
fn is_busy_or_permission_denied(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ResourceBusy | std::io::ErrorKind::PermissionDenied
    )
}

/// Open an AMBE voice instance.
///
/// `prefer` selects a backend; `None` (no opinion) takes whatever this build
/// offers. There is exactly one backend — [`AmbeBackend::Hardware`] — so both
/// `None` and `Some(Hardware)` try [`ambe_thumbdv::detect`] (a bounded serial
/// probe) and return `None` when no dongle answers. Nothing is ever silently
/// substituted for the backend the caller asked for: an operator
/// transmitting/decoding through a vocoder they did not choose, with no
/// indication anything changed, would be a much worse failure mode than a
/// visible connect error. `astar-station`'s D-Star path relies on exactly
/// this: it is hardware-only (no preference knob) and requests
/// `Some(Hardware)` unconditionally.
///
/// Returns `None` if no backend is available (including: `ambe-hw` isn't
/// compiled in, or it is but no dongle is detected).
#[must_use]
pub fn open_ambe(prefer: Option<AmbeBackend>) -> Option<(Box<dyn AmbeVoice>, AmbeBackend)> {
    // `detect_thumbdv()` honors `IAX_THUMBDV_PORT` before falling back to
    // `ambe_thumbdv::detect()`'s own bounded serial probing (never hangs —
    // see ambe-thumbdv's device.rs).
    //
    // On failure there is nothing to fall back to: `None`, outright. A
    // caller must NEVER silently receive some other vocoder in place of the
    // backend it asked for (see this function's doc comment).
    #[cfg(feature = "ambe-hw")]
    {
        // `Hardware` is the only backend, so `None` (no opinion) and
        // `Some(Hardware)` take the same path.
        let _ = prefer;
        detect_thumbdv().ok().map(|dv| {
            (
                Box::new(open_hw_with(dv)) as Box<dyn AmbeVoice>,
                AmbeBackend::Hardware,
            )
        })
    }

    // Without `ambe-hw` compiled in there is no backend at all.
    #[cfg(not(feature = "ambe-hw"))]
    {
        let _ = prefer;
        None
    }
}

/// Open an AMBE decoder as a streaming [`AmbeStream`] pipeline (iax-b3e7 M0):
/// `astar-console`'s `DstarSession` entry point, analogous to
/// [`open_ambe`] but returning the non-blocking `submit_decode`/
/// `poll_decoded` interface a real-time D-Star session needs to keep the
/// `ThumbDV` fed ahead of its 20 ms cadence, instead of the stop-and-wait
/// [`AmbeVoice`].
///
/// Backend selection mirrors [`open_ambe`] exactly — see its doc comment.
///
/// The hardware path cannot reuse [`ambe_thumbdv::detect`]: it hands back an
/// already-initialized [`ambe_thumbdv::ThumbDv`], which has no way to yield
/// its transport back out for the pipelined worker to drive directly (see
/// [`open_hw_stream_with`]'s doc comment). So this scans
/// [`thumbdv_candidate_ports`] itself (`IAX_THUMBDV_PORT` override, else the
/// same VID/PID-filtered scan `detect()` uses internally, so this never
/// risks opening an unrelated serial device) — and opens the first
/// candidate that both connects and passes [`open_hw_stream_with`]'s own
/// init handshake, at either of `detect`'s two supported baud rates (§6
/// fallback). This does not itself distinguish "no dongle" from "dongle
/// busy" — it only answers "is a working pipelined stream available now";
/// call [`classify_thumbdv_failure`] on `None` to tell those apart for an
/// error message.
///
/// Returns `None` if no backend is available.
#[must_use]
pub fn open_ambe_stream(prefer: Option<AmbeBackend>) -> Option<(Box<dyn AmbeStream>, AmbeBackend)> {
    #[cfg(feature = "ambe-hw")]
    {
        // `Hardware` is the only backend, so `None` (no opinion) and
        // `Some(Hardware)` take the same path.
        let _ = prefer;
        open_hw_stream_from_candidate_ports().map(|stream| (stream, AmbeBackend::Hardware))
    }

    #[cfg(not(feature = "ambe-hw"))]
    {
        let _ = prefer;
        None
    }
}

/// Scans [`thumbdv_candidate_ports`] and returns a pipelined [`AmbeStream`]
/// for the first candidate that opens and passes [`open_hw_stream_with`]'s
/// init handshake, trying both of `detect`'s supported baud rates per
/// candidate. `None` if no candidate is present or every candidate fails
/// (unplugged, busy, wrong device) — see [`open_ambe_stream`]'s doc for why
/// finer-grained classification isn't this function's job.
#[cfg(feature = "ambe-hw")]
fn open_hw_stream_from_candidate_ports() -> Option<Box<dyn AmbeStream>> {
    for path in thumbdv_candidate_ports() {
        for baud in [460_800u32, 230_400u32] {
            let Ok(transport) = ambe_thumbdv::SerialTransport::open(&path, baud) else {
                continue;
            };
            if let Ok(stream) = open_hw_stream_with(transport) {
                return Some(stream);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Hardware backend: a ThumbDV USB dongle, driven from a dedicated worker
// thread (iax-239a rule: all serial/USB I/O stays off the calling —
// audio — thread; a wedged transfer must surface as silence, never a
// hang).
// ---------------------------------------------------------------------

/// One request/reply round trip handed to the worker thread.
#[cfg(feature = "ambe-hw")]
enum HwReq {
    Decode([u8; 9], std::sync::mpsc::SyncSender<[i16; 160]>),
    // Boxed to keep the two variants close in size (clippy::large_enum_variant):
    // a bare `[i16; 160]` here would make `Encode` ~15x the size of `Decode`.
    Encode(Box<[i16; 160]>, std::sync::mpsc::SyncSender<[u8; 9]>),
}

/// `AmbeVoice` backed by a `ThumbDV` dongle. Owns only a channel to the
/// worker thread that actually talks to the device — no serial/USB I/O
/// ever runs on the caller's (audio) thread.
///
/// `pub(crate)` (not private) only so its return type matches
/// [`open_hw_with`]'s `pub(crate)` visibility (the `private_interfaces`
/// lint requires that); it is not part of this crate's public API —
/// external callers reach it only through [`open_ambe`]'s
/// `Box<dyn AmbeVoice>`.
#[cfg(feature = "ambe-hw")]
pub(crate) struct HwAmbe {
    tx: std::sync::mpsc::Sender<HwReq>,
}

/// Bounded wait for a worker reply: long enough for a healthy round trip
/// (a real `ThumbDV` transaction completes in low single-digit ms), short
/// enough that a wedged device never stalls the audio pipeline.
#[cfg(feature = "ambe-hw")]
const HW_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(feature = "ambe-hw")]
impl AmbeVoice for HwAmbe {
    fn decode(&mut self, frame: &[u8; 9]) -> [i16; 160] {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(HwReq::Decode(*frame, reply_tx)).is_err() {
            // Worker thread is gone (channel closed): silence, not a panic.
            return [0i16; 160];
        }
        reply_rx.recv_timeout(HW_REPLY_TIMEOUT).unwrap_or_else(|_| {
            tracing::warn!(
                "ambe-hw: worker did not reply within {HW_REPLY_TIMEOUT:?}, returning silence"
            );
            [0i16; 160]
        })
    }

    fn encode(&mut self, pcm: &[i16; 160]) -> [u8; 9] {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        if self
            .tx
            .send(HwReq::Encode(Box::new(*pcm), reply_tx))
            .is_err()
        {
            return [0u8; 9];
        }
        reply_rx.recv_timeout(HW_REPLY_TIMEOUT).unwrap_or_else(|_| {
            tracing::warn!(
                "ambe-hw: worker did not reply within {HW_REPLY_TIMEOUT:?}, returning a zero frame"
            );
            [0u8; 9]
        })
    }
}

/// Spawn the worker thread owning `dv` and return an `HwAmbe` that talks
/// to it. Exposed at `pub(crate)` so tests can drive it directly with a
/// `MockTransport`-backed `ThumbDv`; production callers reach it only
/// through [`open_ambe`].
///
/// The worker loops on `rx.recv()` (blocks the worker thread, never the
/// caller) until every `Sender` — held only by the returned `HwAmbe` — is
/// dropped, at which point `recv()` returns `Err` and the thread exits.
/// Each `DeviceError` from the dongle is logged via `tracing::warn!` and
/// answered with silence/a zero frame rather than propagated: the codec
/// layer has no listener recovery story, so a bad frame is better than a
/// stuck pipeline.
#[cfg(feature = "ambe-hw")]
pub(crate) fn open_hw_with<T: ambe_thumbdv::Transport + Send + 'static>(
    dv: ambe_thumbdv::ThumbDv<T>,
) -> HwAmbe {
    let (tx, rx) = std::sync::mpsc::channel::<HwReq>();
    std::thread::spawn(move || {
        let mut dv = dv;
        while let Ok(req) = rx.recv() {
            match req {
                HwReq::Decode(frame, reply) => {
                    let pcm = dv.decode_frame(&frame).unwrap_or_else(|e| {
                        tracing::warn!("ambe-hw: decode_frame failed: {e}");
                        [0i16; 160]
                    });
                    // Drop the reply if the caller already gave up on it
                    // (recv_timeout fired first) — nothing to do about that.
                    let _ = reply.send(pcm);
                }
                HwReq::Encode(pcm, reply) => {
                    let frame = dv.encode_frame(&pcm).unwrap_or_else(|e| {
                        tracing::warn!("ambe-hw: encode_frame failed: {e}");
                        [0u8; 9]
                    });
                    let _ = reply.send(frame);
                }
            }
        }
    });
    HwAmbe { tx }
}

// ---------------------------------------------------------------------
// Hardware backend, pipelined path: `AmbeStream` for the ThumbDV.
//
// `HwAmbe` above is stop-and-wait by construction: `ThumbDv::decode_frame`
// fuses "write the request" and "block until the matching response parses"
// into one call, so only one request can ever be on the wire at a time —
// exactly the ~24.5 ms/frame shape this M0 exists to fix. Reaching the
// measured ~7.45 ms/frame pipelined figure requires writing frame N+1
// before frame N's response arrives, which needs direct access to the
// serial transport — but `ThumbDv<T>` never hands its transport back out
// once constructed (no `transport_mut`/`into_transport`).
//
// So this path takes the raw `Transport` directly and replays
// `ThumbDv::init_with`'s init cookbook itself (`hw_stream_init`, below)
// using only `ambe_thumbdv`'s already-`pub` packet builders/`Deframer`/
// `parse_response` — no changes needed upstream. The worker then writes
// queued requests and reads responses independently on the same thread
// (one thread owning the one `&mut Transport` — no locking needed, and no
// second thread contending for the one serial port).
// ---------------------------------------------------------------------

/// How long the pipelined worker blocks on a single `recv_some` call. Short
/// enough that a newly-submitted frame's write doesn't wait long behind an
/// in-progress read attempt.
#[cfg(feature = "ambe-hw")]
const STREAM_READ_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// How long the pipelined worker blocks on the request channel when nothing
/// is outstanding, so it idles instead of busy-spinning the thread.
#[cfg(feature = "ambe-hw")]
const STREAM_IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

/// Send one request and block (up to a 300 ms deadline, mirroring
/// `ambe_thumbdv`'s own per-transaction bound) until a full response packet
/// parses. Used only during [`hw_stream_init`]'s one-time handshake — the
/// steady-state worker loop never waits like this for a decode response.
#[cfg(feature = "ambe-hw")]
fn hw_stream_transact<T: ambe_thumbdv::Transport>(
    transport: &mut T,
    req: &[u8],
) -> Result<ambe_thumbdv::Response, ambe_thumbdv::DeviceError> {
    use ambe_thumbdv::{Deframer, DeviceError, parse_response};

    transport.send(req).map_err(DeviceError::Io)?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    let mut deframer = Deframer::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(DeviceError::Timeout("300 ms deadline expired"));
        }
        let mut buf = [0u8; 1024];
        match transport.recv_some(&mut buf, remaining) {
            Ok(0) => continue,
            Ok(n) => deframer.push(&buf[..n]),
            Err(e) => return Err(DeviceError::Io(e)),
        }
        if let Some(pkt) = deframer.next_packet() {
            return parse_response(&pkt).map_err(|e| DeviceError::Protocol(e.to_string()));
        }
    }
}

/// Mirrors `ThumbDv`'s private `check_status`: a `Status` response with a
/// nonzero status is an error; a `Status` for the wrong field is a protocol
/// error; anything else is unexpected.
#[cfg(feature = "ambe-hw")]
fn hw_stream_check_status(
    resp: &ambe_thumbdv::Response,
    expected_field: u8,
) -> Result<(), ambe_thumbdv::DeviceError> {
    use ambe_thumbdv::{DeviceError, Response};

    match resp {
        Response::Status { field, status } => {
            if *status != 0 {
                return Err(DeviceError::Status {
                    field: *field,
                    status: *status,
                });
            }
            if *field != expected_field {
                return Err(DeviceError::Protocol(format!(
                    "expected field 0x{expected_field:02X}, got 0x{field:02X}"
                )));
            }
            Ok(())
        }
        _ => Err(DeviceError::Protocol("expected Status response".into())),
    }
}

/// Replays `ThumbDv::init_with`'s initialization cookbook (§8.1) directly
/// against a raw [`ambe_thumbdv::Transport`], byte-for-byte identical to
/// what `ThumbDv::init_with` does — this duplicates the *algorithm*, not the
/// wire format, using only `ambe_thumbdv`'s `pub` packet builders, and
/// exists solely because `ThumbDv<T>` cannot yield its transport back out
/// for the pipelined worker to drive directly (see the module comment
/// above).
#[cfg(feature = "ambe-hw")]
fn hw_stream_init<T: ambe_thumbdv::Transport>(
    transport: &mut T,
) -> Result<(), ambe_thumbdv::DeviceError> {
    use ambe_thumbdv::{
        DeviceError, Response, dcmode_off, ecmode_off, gain_zero, init_encdec, prodid_query,
        ratep_dstar, reset,
    };
    // Not re-exported at the crate root (see `ambe_thumbdv::lib`'s `pub use`
    // list) — the existing `hw_tests::scripted_init` helper reaches it the
    // same way.
    use ambe_thumbdv::packet::verstring_query;

    // Step 1: drain any stale bytes left over from a previous session.
    let mut discard = [0u8; 1024];
    let _ = transport.recv_some(&mut discard, std::time::Duration::from_millis(50));

    // Step 2: reset, await Ready.
    match hw_stream_transact(transport, &reset())? {
        Response::Ready => {}
        _ => return Err(DeviceError::Protocol("expected Ready after reset".into())),
    }

    // Step 3: product ID, must be an AMBE3000.
    let Response::ProdId(prodid) = hw_stream_transact(transport, &prodid_query())? else {
        return Err(DeviceError::Protocol("expected ProdId".into()));
    };
    if !prodid.starts_with("AMBE3000") {
        return Err(DeviceError::WrongDevice(format!(
            "expected AMBE3000*, got {prodid}"
        )));
    }

    // Step 4: version string (queried for parity with the cookbook; not
    // otherwise used by the streaming path).
    match hw_stream_transact(transport, &verstring_query())? {
        Response::Version(_) => {}
        _ => return Err(DeviceError::Protocol("expected Version".into())),
    }

    // Steps 5-9: D-STAR rate params, encoder/decoder init, EC/DC off, gain
    // zero.
    hw_stream_check_status(&hw_stream_transact(transport, &ratep_dstar())?, 0x0A)?;
    hw_stream_check_status(&hw_stream_transact(transport, &init_encdec())?, 0x0B)?;
    hw_stream_check_status(&hw_stream_transact(transport, &ecmode_off())?, 0x05)?;
    hw_stream_check_status(&hw_stream_transact(transport, &dcmode_off())?, 0x06)?;
    hw_stream_check_status(&hw_stream_transact(transport, &gain_zero())?, 0x4B)?;

    Ok(())
}

/// Cap on a side's surplus-response counter. There can never be more
/// responses owed-but-unmatchable than that side's own in-flight bound, and
/// capping keeps a permanently-erroring device (unplugged mid-QSO: every read
/// fails) from growing the counter without limit and then swallowing good
/// frames if it ever comes back.
#[cfg(feature = "ambe-hw")]
const HW_STREAM_MAX_PENDING_DISCARD: usize = AMBE_STREAM_MAX_IN_FLIGHT;

/// The AMBE frame substituted for an encode request the device never
/// answered: D-Star's NULL (silence) codeword, as every reference
/// implementation transmits it (`MMDVMHost`'s `DSTAR_NULL_FRAME_SYNC_BYTES`
/// minus its trailing slow-data bytes; the same constant is re-exported as
/// `astar_dstar::NULL_AMBE`, duplicated here because this crate does not
/// depend on that one).
///
/// NOT all-zero: an all-zero payload is not silence to an AMBE decoder, it is
/// a frame whose voice parameters are all zero, which typically renders as a
/// click or noise burst — and one of these can land in the MIDDLE of a live
/// transmission, on the air.
#[cfg(feature = "ambe-hw")]
const NULL_AMBE_FRAME: [u8; 9] = [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8];

/// One direction's (decode's or encode's) outstanding-request accounting.
/// Decode and encode requests share one worker thread and one serial link
/// (§5a of the iax-2f6b scout: the `ThumbDV` is a single USB device, only one
/// thread may hold it), but a `Speech` response and a `Channel` response are
/// structurally distinct packet types on the wire (`RawPacket::ptype`), so
/// each direction gets its own `outstanding`/`pending_discard` bookkeeping —
/// a lost or late response on one side can never shift the other side's
/// queue, and the two pipelines' [`AMBE_STREAM_MAX_IN_FLIGHT`] bounds are
/// enforced independently.
#[cfg(feature = "ambe-hw")]
struct PipelineSide<Resp> {
    /// Submission timestamps, oldest first — DVSI responses carry no
    /// sequence number, but the chip answers requests OF ONE KIND in the
    /// order they were written, so "oldest outstanding of this kind" is
    /// always the correct match once the response's kind is known.
    outstanding: std::collections::VecDeque<std::time::Instant>,
    /// Responses this side's device still owes for requests already answered
    /// with a substituted value (see [`PipelineSide::substitute`]).
    pending_discard: usize,
    /// How long THIS side will wait for the surplus responses it is owed
    /// before writing them off (see [`PipelineSide::settle_debt`]).
    ///
    /// Per side, not per worker (iax-2f6b review): with one shared deadline,
    /// a decode-side deadline set minutes ago — and never cleared, because
    /// every path that leaves the idle branch leaves it set — expires
    /// instantly the next time the queues drain and wipes the ENCODE side's
    /// freshly-recorded debt with it. The encode side then delivers the next
    /// real response into the wrong request's slot and every subsequent
    /// on-air frame is shifted by one: exactly the permanent desync
    /// `pending_discard` exists to prevent.
    flush_deadline: Option<std::time::Instant>,
    resp_tx: std::sync::mpsc::Sender<Resp>,
}

#[cfg(feature = "ambe-hw")]
impl<Resp> PipelineSide<Resp> {
    fn new(resp_tx: std::sync::mpsc::Sender<Resp>) -> Self {
        PipelineSide {
            outstanding: std::collections::VecDeque::new(),
            pending_discard: 0,
            flush_deadline: None,
            resp_tx,
        }
    }

    /// Answer the oldest outstanding request on this side with a substituted
    /// value (a per-request timeout, or a transport read error) and record
    /// that the device *still owes* a real response for it — see
    /// `pending_discard`'s doc comment and [`hw_stream_deliver_to_side`].
    fn substitute(&mut self, value: Resp) {
        self.outstanding.pop_front();
        self.pending_discard = (self.pending_discard + 1).min(HW_STREAM_MAX_PENDING_DISCARD);
        // Fresh debt gets a fresh clock: any deadline still recorded here
        // belongs to an older debt and must not be allowed to expire this one
        // the instant the pipeline next goes idle.
        self.flush_deadline = None;
        let _ = self.resp_tx.send(value);
    }

    /// Idle-flush step for ONE side: `true` once this side owes nothing —
    /// either because every surplus response has been accounted for, or
    /// because its own deadline passed and the device is never going to send
    /// them. Only ever touches this side's counters.
    fn settle_debt(&mut self, now: std::time::Instant) -> bool {
        if self.pending_discard == 0 {
            self.flush_deadline = None;
            return true;
        }
        let deadline = *self
            .flush_deadline
            .get_or_insert_with(|| now + HW_REPLY_TIMEOUT);
        if now >= deadline {
            self.pending_discard = 0;
            self.flush_deadline = None;
            return true;
        }
        false
    }
}

/// Write one decode request to the device and push its submission time onto
/// the decode side's outstanding queue, or — if the write itself fails —
/// send one silence frame immediately rather than tracking a request that
/// was never actually sent (no `pending_discard` bump: nothing was written,
/// so the device owes nothing for it).
#[cfg(feature = "ambe-hw")]
fn hw_stream_write_decode<T: ambe_thumbdv::Transport>(
    transport: &mut T,
    side: &mut PipelineSide<[i16; 160]>,
    frame: [u8; 9],
) {
    match transport.send(&ambe_thumbdv::channel_in(&frame)) {
        Ok(()) => side.outstanding.push_back(std::time::Instant::now()),
        Err(e) => {
            tracing::warn!("ambe-hw stream: decode write failed: {e}, substituting silence");
            let _ = side.resp_tx.send([0i16; 160]);
        }
    }
}

/// Write one encode request to the device and push its submission time onto
/// the encode side's outstanding queue, mirroring
/// [`hw_stream_write_decode`] for the encode direction (iax-2f6b).
#[cfg(feature = "ambe-hw")]
fn hw_stream_write_encode<T: ambe_thumbdv::Transport>(
    transport: &mut T,
    side: &mut PipelineSide<[u8; 9]>,
    pcm: &[i16; 160],
) {
    match transport.send(&ambe_thumbdv::speech_in(pcm)) {
        Ok(()) => side.outstanding.push_back(std::time::Instant::now()),
        Err(e) => {
            tracing::warn!("ambe-hw stream: encode write failed: {e}, substituting a null frame");
            let _ = side.resp_tx.send(NULL_AMBE_FRAME);
        }
    }
}

/// Deliver — or deliberately discard — one framed response packet. The
/// response's own parsed type identifies which side it belongs to: a
/// `Speech` response can only ever be answering a `channel_in` (decode)
/// request, and a `Channel` response can only ever be answering a
/// `speech_in` (encode) request — i.e. the AMBE3000 answers a DECODE request
/// (channel bits in) with a SPEECH-shaped frame (PCM out), and an ENCODE
/// request (speech in) with a CHANNEL-shaped one (AMBE bits out). See
/// `hw_decode_round_trips_through_worker`/`hw_encode_round_trips_through_worker`
/// for the scripted wire bytes that pin exactly this, and
/// [`hw_stream_deliver_to_side`] for the matching rules once the side is
/// known.
///
/// Neither side is touched for a response that identifies as neither kind (a
/// stray control packet, e.g. an unsolicited `Status`/`Ready`, or a parse
/// error): such a packet can never BE the real answer either side's device
/// still owes (an owed answer is always Speech- or Channel-shaped, matching
/// what was actually requested), so discarding it without touching either
/// side's `outstanding`/`pending_discard` is always correct.
#[cfg(feature = "ambe-hw")]
fn hw_stream_deliver_packet(
    pkt: &ambe_thumbdv::RawPacket,
    decode_side: &mut PipelineSide<[i16; 160]>,
    encode_side: &mut PipelineSide<[u8; 9]>,
) {
    use ambe_thumbdv::{Response, parse_response};

    match parse_response(pkt) {
        Ok(Response::Speech(pcm)) => hw_stream_deliver_to_side(decode_side, pcm, "decode"),
        Ok(Response::Channel(frame)) => hw_stream_deliver_to_side(encode_side, frame, "encode"),
        Ok(other) => {
            tracing::warn!(
                "ambe-hw stream: unexpected response {other:?} (neither Speech nor Channel), \
                 discarding (whichever request it might have answered will time out into a \
                 substituted value)"
            );
        }
        Err(e) => {
            tracing::warn!("ambe-hw stream: response parse error: {e}, discarding the packet");
        }
    }
}

/// Match one already-identified response value **positionally** against
/// `side`'s `outstanding` queue (DVSI responses carry no sequence number;
/// the chip answers requests of one kind in order over the one serial link,
/// so "oldest outstanding of this kind" is always correct).
///
/// Two cases never reach an `outstanding` slot:
///
/// - `pending_discard > 0`: the device owes this side responses for requests
///   already answered with a substituted value. Delivering one now would
///   hand it to the NEXT request's slot and shift every following frame of
///   THIS side by one for the life of the session — the permanent desync
///   spec §1 says a lost response must not cause. Discard and decrement.
/// - nothing outstanding on this side: a stale/unsolicited packet, discarded
///   so it cannot survive into the next transmission.
#[cfg(feature = "ambe-hw")]
fn hw_stream_deliver_to_side<Resp>(side: &mut PipelineSide<Resp>, value: Resp, kind: &str) {
    if side.pending_discard > 0 {
        side.pending_discard -= 1;
        tracing::debug!(
            "ambe-hw stream: discarding a surplus {kind} response the device still owed"
        );
        return;
    }
    if side.outstanding.is_empty() {
        tracing::warn!(
            "ambe-hw stream: {kind} response arrived with nothing outstanding, discarding"
        );
        return;
    }
    side.outstanding.pop_front();
    let _ = side.resp_tx.send(value);
}

/// One request handed to the pipelined worker over its single request
/// channel — decode and encode share one worker thread and one serial link
/// (§5a: exactly one process/thread may hold the `ThumbDV`), so both kinds of
/// request travel the same channel and are told apart by this tag.
///
/// `Encode`'s payload is boxed for the same reason [`HwReq::Encode`]'s is: a
/// bare `[i16; 160]` (320 bytes) would make this enum ~35x the size of its
/// `Decode` variant (`clippy::large_enum_variant`).
#[cfg(feature = "ambe-hw")]
enum StreamReq {
    Decode([u8; 9]),
    Encode(Box<[i16; 160]>),
}

/// Write one already-dequeued request, dispatching to the decode or encode
/// side by its tag.
#[cfg(feature = "ambe-hw")]
fn hw_stream_write_req<T: ambe_thumbdv::Transport>(
    transport: &mut T,
    decode_side: &mut PipelineSide<[i16; 160]>,
    encode_side: &mut PipelineSide<[u8; 9]>,
    req: StreamReq,
) {
    match req {
        StreamReq::Decode(frame) => hw_stream_write_decode(transport, decode_side, frame),
        StreamReq::Encode(pcm) => hw_stream_write_encode(transport, encode_side, &pcm),
    }
}

/// The pipelined worker: writes requests as they arrive and reads responses
/// independently, matching them against the correct side's `outstanding`
/// queue by the response's own parsed type (see [`hw_stream_deliver_packet`]).
/// Decode and encode requests share this one thread and one `Transport`
/// (§5a), each with its own bookkeeping (`PipelineSide`) so a lost or late
/// response on one side never shifts the other's queue.
///
/// A device error or a per-request timeout answers exactly the oldest
/// outstanding entry of the affected side(s) with one substituted value and
/// remembers that the device still owes a real response for it
/// (`pending_discard`), so that response is discarded when it finally shows
/// up instead of being delivered against a later request's slot. A lost
/// response therefore costs one frame of audio, not a permanently
/// desynchronized stream.
///
/// Exits as soon as the request channel disconnects (every `HwAmbeStream`
/// clone dropped), even with requests still outstanding: nothing is left to
/// receive their responses either (both `resp_tx`s' `Receiver`s are owned by
/// the same handle), so there is nothing to flush.
#[cfg(feature = "ambe-hw")]
#[allow(clippy::too_many_lines)]
fn hw_stream_worker<T: ambe_thumbdv::Transport>(
    mut transport: T,
    req_rx: &std::sync::mpsc::Receiver<StreamReq>,
    decode_resp_tx: &std::sync::mpsc::Sender<[i16; 160]>,
    encode_resp_tx: &std::sync::mpsc::Sender<[u8; 9]>,
) {
    use ambe_thumbdv::{Deframer, Response, parse_response};
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};

    let mut decode_side = PipelineSide::new(decode_resp_tx.clone());
    let mut encode_side = PipelineSide::new(encode_resp_tx.clone());
    let mut deframer = Deframer::new();

    loop {
        // 1. Drain and write everything currently queued, without blocking.
        let mut disconnected = false;
        loop {
            match req_rx.try_recv() {
                Ok(req) => {
                    hw_stream_write_req(&mut transport, &mut decode_side, &mut encode_side, req);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            return;
        }

        // 2. Deliver every packet the deframer ALREADY holds before going
        //    anywhere near another blocking read: one serial read routinely
        //    returns bytes spanning several 326-byte Speech packets (the
        //    1024-byte buffer below holds ~3.1 of them), and making packets
        //    2 and 3 each wait out a fresh `recv_some` poll would stall the
        //    consumer for no reason.
        while let Some(pkt) = deframer.next_packet() {
            hw_stream_deliver_packet(&pkt, &mut decode_side, &mut encode_side);
        }

        // 3. Nothing outstanding on either side.
        if decode_side.outstanding.is_empty() && encode_side.outstanding.is_empty() {
            if decode_side.pending_discard > 0 || encode_side.pending_discard > 0 {
                // At least one side's device owes responses that nothing can
                // be matched against any more. Drain and discard them HERE,
                // while the pipeline is idle and bounded by `flush_deadline`,
                // so they can never leak into the next transmission's slots
                // (the next transmission may be a different talker, minutes
                // later, or the other direction entirely).
                let mut buf = [0u8; 1024];
                match transport.recv_some(&mut buf, STREAM_READ_POLL) {
                    Ok(0) => {}
                    Ok(n) => deframer.push(&buf[..n]),
                    Err(_) => {
                        // The link itself is broken, so nothing is going to
                        // discharge either side's debt: write both off (a
                        // transport error is genuinely shared state, unlike
                        // a per-request timeout).
                        decode_side.pending_discard = 0;
                        encode_side.pending_discard = 0;
                    }
                }
                while decode_side.pending_discard > 0 || encode_side.pending_discard > 0 {
                    let Some(pkt) = deframer.next_packet() else {
                        break;
                    };
                    // Route the flushed packet by its own parsed type, same
                    // as the main delivery path — a Speech-shaped packet can
                    // only ever be discharging decode's debt, a
                    // Channel-shaped one only encode's. Anything else
                    // matches neither owed shape and is dropped in place.
                    match parse_response(&pkt) {
                        Ok(Response::Speech(_)) if decode_side.pending_discard > 0 => {
                            decode_side.pending_discard -= 1;
                        }
                        Ok(Response::Channel(_)) if encode_side.pending_discard > 0 => {
                            encode_side.pending_discard -= 1;
                        }
                        _ => {}
                    }
                }
                // Each side settles on ITS OWN clock — one side's expired
                // deadline may never write off the other's live debt.
                let now = std::time::Instant::now();
                let decode_done = decode_side.settle_debt(now);
                let encode_done = encode_side.settle_debt(now);
                if decode_done && encode_done {
                    // Nothing is owed either way; reset the framing state too
                    // so no partial packet survives into the next
                    // transmission.
                    deframer = Deframer::new();
                }
                continue;
            }
            // Idle-wait for the next submission instead of busy-spinning the
            // worker thread on repeated empty reads.
            match req_rx.recv_timeout(STREAM_IDLE_WAIT) {
                Ok(req) => {
                    hw_stream_write_req(&mut transport, &mut decode_side, &mut encode_side, req);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        // 4. Something is outstanding on at least one side: try to read. A
        //    short poll keeps this loop iteration bounded so newly-submitted
        //    frames get written promptly on the next pass; whatever is read
        //    is framed and delivered by step 2 on that same next pass.
        let mut buf = [0u8; 1024];
        match transport.recv_some(&mut buf, STREAM_READ_POLL) {
            Ok(0) => {}
            Ok(n) => {
                deframer.push(&buf[..n]);
                continue;
            }
            Err(e) => {
                // Deliberately does NOT substitute here (iax-2f6b review): a
                // read error is attributable to at most one of the two
                // transactions in flight, but nothing in it says which. The
                // old behaviour substituted on BOTH sides, so one transient
                // USB hiccup put an all-zero AMBE frame ON THE AIR for an
                // encode request the device was about to answer correctly —
                // and then swallowed that real answer as `pending_discard`
                // debt when it arrived. Step 5 below substitutes for whichever
                // side has actually blown HW_REPLY_TIMEOUT, which keeps the
                // cost with the direction that really lost a response. The
                // sleep is what keeps a persistently failing transport from
                // hot-spinning this thread (`recv_some` returns its error
                // immediately, without waiting out its poll).
                tracing::warn!(
                    "ambe-hw stream: read failed: {e}, leaving it to the per-request timeout to \
                     decide which side (if any) lost a response"
                );
                std::thread::sleep(STREAM_READ_POLL);
            }
        }

        // 5. Read poll came back empty: age out either side's oldest
        //    outstanding request if it has waited longer than a healthy
        //    round trip. Independent per side — in the half-duplex steady
        //    state at most one side is ever non-empty, but nothing here
        //    assumes that.
        if decode_side
            .outstanding
            .front()
            .is_some_and(|t| t.elapsed() > HW_REPLY_TIMEOUT)
        {
            tracing::warn!("ambe-hw stream: decode response timeout, substituting silence");
            decode_side.substitute([0i16; 160]);
        }
        if encode_side
            .outstanding
            .front()
            .is_some_and(|t| t.elapsed() > HW_REPLY_TIMEOUT)
        {
            tracing::warn!("ambe-hw stream: encode response timeout, substituting a null frame");
            encode_side.substitute(NULL_AMBE_FRAME);
        }
    }
}

/// `AmbeStream` backed by a `ThumbDV` dongle's pipelined worker. Like
/// `HwAmbe`, owns only channels to the worker thread — no serial/USB I/O
/// ever runs on the caller's (audio) thread.
///
/// `in_flight` is tracked entirely on this handle, incremented the moment a
/// frame is accepted onto the request channel and decremented the moment
/// `poll_decoded` returns one back — not by asking the worker — so the
/// [`AMBE_STREAM_MAX_IN_FLIGHT`] check in `submit_decode` is synchronous and
/// race-free with respect to the worker's own progress.
#[cfg(feature = "ambe-hw")]
pub(crate) struct HwAmbeStream {
    req_tx: std::sync::mpsc::Sender<StreamReq>,
    resp_rx_decode: std::sync::mpsc::Receiver<[i16; 160]>,
    resp_rx_encode: std::sync::mpsc::Receiver<[u8; 9]>,
    in_flight: usize,
    in_flight_encode: usize,
}

#[cfg(feature = "ambe-hw")]
impl AmbeStream for HwAmbeStream {
    fn submit_decode(&mut self, frame: [u8; 9]) {
        if self.in_flight >= AMBE_STREAM_MAX_IN_FLIGHT {
            tracing::warn!(
                "ambe-hw stream: pipeline full ({AMBE_STREAM_MAX_IN_FLIGHT} in flight), dropping newest frame"
            );
            return;
        }
        // If the worker is gone the frame simply never gets a reply; no
        // point tracking it as in-flight (poll_decoded would wait forever).
        if self.req_tx.send(StreamReq::Decode(frame)).is_ok() {
            self.in_flight += 1;
        }
    }

    fn poll_decoded(&mut self) -> Option<[i16; 160]> {
        match self.resp_rx_decode.try_recv() {
            Ok(pcm) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                Some(pcm)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker thread is GONE — it returned, or it panicked
                // somewhere inside the transport/deframer. Nothing will ever
                // answer the outstanding requests, so zero the counter:
                // otherwise `in_flight()` stays pinned above zero forever and
                // any `while in_flight() > 0` drain spins on a `None` that
                // can never change (iax-239a: a wedged or dead device must
                // surface as an error, never a hang).
                if self.in_flight > 0 {
                    tracing::warn!(
                        "ambe-hw stream: worker gone, abandoning {} in-flight decode frame(s)",
                        self.in_flight
                    );
                    self.in_flight = 0;
                }
                None
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn submit_encode(&mut self, pcm: [i16; 160]) {
        if self.in_flight_encode >= AMBE_STREAM_MAX_IN_FLIGHT {
            tracing::warn!(
                "ambe-hw stream: encode pipeline full ({AMBE_STREAM_MAX_IN_FLIGHT} in flight), dropping newest frame"
            );
            return;
        }
        if self.req_tx.send(StreamReq::Encode(Box::new(pcm))).is_ok() {
            self.in_flight_encode += 1;
        } else {
            // The worker is gone (panicked, or the port was yanked). Nothing
            // will ever answer, so the frame is simply lost — say so rather
            // than silently swallowing every mic frame handed to us from here
            // on (iax-239a: a dead device surfaces as an error, never as
            // silence).
            tracing::warn!(
                "ambe-hw stream: worker gone, discarding a mic frame that can never be encoded"
            );
        }
    }

    fn poll_encoded(&mut self) -> Option<[u8; 9]> {
        match self.resp_rx_encode.try_recv() {
            Ok(frame) => {
                self.in_flight_encode = self.in_flight_encode.saturating_sub(1);
                Some(frame)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if self.in_flight_encode > 0 {
                    tracing::warn!(
                        "ambe-hw stream: worker gone, abandoning {} in-flight encode frame(s)",
                        self.in_flight_encode
                    );
                    self.in_flight_encode = 0;
                }
                None
            }
        }
    }

    fn in_flight_encoded(&self) -> usize {
        self.in_flight_encode
    }
}

/// Initialize `transport` and spawn the pipelined worker, returning both
/// the [`HwAmbeStream`] handle and the worker's `JoinHandle` — the handle is
/// `pub(crate)` (needed by tests, which verify clean shutdown by joining
/// it); production callers go through [`open_hw_stream_with`], which
/// discards it (matching [`open_hw_with`]'s fire-and-forget worker thread).
#[cfg(feature = "ambe-hw")]
pub(crate) fn open_hw_stream_with_handle<T: ambe_thumbdv::Transport + Send + 'static>(
    mut transport: T,
) -> Result<(HwAmbeStream, std::thread::JoinHandle<()>), ambe_thumbdv::DeviceError> {
    hw_stream_init(&mut transport)?;
    let (req_tx, req_rx) = std::sync::mpsc::channel::<StreamReq>();
    let (decode_resp_tx, resp_rx_decode) = std::sync::mpsc::channel::<[i16; 160]>();
    let (encode_resp_tx, resp_rx_encode) = std::sync::mpsc::channel::<[u8; 9]>();
    let handle = std::thread::spawn(move || {
        hw_stream_worker(transport, &req_rx, &decode_resp_tx, &encode_resp_tx);
    });
    Ok((
        HwAmbeStream {
            req_tx,
            resp_rx_decode,
            resp_rx_encode,
            in_flight: 0,
            in_flight_encode: 0,
        },
        handle,
    ))
}

/// Open a pipelined [`AmbeStream`] against a raw `ThumbDV` transport.
///
/// Unlike [`open_hw_with`] (which takes an already-initialized
/// [`ambe_thumbdv::ThumbDv`]), this takes the raw transport directly and
/// performs its own copy of the init cookbook ([`hw_stream_init`]) before
/// starting the pipelined worker — see this module's "Hardware backend,
/// pipelined path" comment for why that duplication is necessary and
/// sanctioned (zero changes needed in `ambe-thumbdv` itself).
///
/// # Errors
/// Returns the [`ambe_thumbdv::DeviceError`] from the init handshake if it
/// fails (wrong device, protocol error, timeout, I/O error).
#[cfg(feature = "ambe-hw")]
pub fn open_hw_stream_with<T: ambe_thumbdv::Transport + Send + 'static>(
    transport: T,
) -> Result<Box<dyn AmbeStream>, ambe_thumbdv::DeviceError> {
    let (stream, _worker) = open_hw_stream_with_handle(transport)?;
    Ok(Box::new(stream))
}

// ---------------------------------------------------------------------
// Shared test scaffolding for every `ambe-hw` test module below. Two
// process-global exclusions live here rather than inside one submodule,
// because `hw_tests`, `hw_hardware_tests` and `thumbdv_failure_tests` all
// compile into the SAME test binary and `cargo test` runs a binary's tests
// on N threads by default:
//
// - `hardware_lock()`: only one process (let alone one thread) may hold the
//   real ThumbDV. Every test that opens it — or that deliberately holds the
//   port to make something else fail — takes this for its whole body.
// - `env_lock()`: `IAX_THUMBDV_PORT` is process-global mutable state.
// ---------------------------------------------------------------------

#[cfg(all(test, feature = "ambe-hw"))]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    /// How long to let a just-dropped `HwAmbeStream`'s worker actually close
    /// the serial fd before the next test may open it. Dropping the handle
    /// only closes `req_tx`; the worker notices on its next loop pass, up to
    /// `STREAM_IDLE_WAIT` (20 ms) later, and only then drops the transport.
    /// Without this settle the next test's open races a port that is still
    /// held and fails spuriously.
    const PORT_SETTLE: Duration = Duration::from_millis(40);

    /// Held for the whole body of any test that touches the real dongle. Its
    /// `Drop` settles first (see [`PORT_SETTLE`]) so the port is genuinely
    /// closed by the time the next waiter wakes.
    pub(crate) struct HardwareGuard(Option<MutexGuard<'static, ()>>);

    impl Drop for HardwareGuard {
        fn drop(&mut self) {
            std::thread::sleep(PORT_SETTLE);
            drop(self.0.take());
        }
    }

    /// Serializes every hardware-touching test in this binary.
    /// `unwrap_or_else(PoisonError::into_inner)` recovers from a prior test
    /// panicking while holding it — mutual exclusion is all that is wanted
    /// here, and there is no data to be poisoned.
    pub(crate) fn hardware_lock() -> HardwareGuard {
        static LOCK: Mutex<()> = Mutex::new(());
        HardwareGuard(Some(LOCK.lock().unwrap_or_else(PoisonError::into_inner)))
    }

    /// Serializes every test that sets/reads `IAX_THUMBDV_PORT` (mirrors
    /// `codec2.rs`'s `IAX_CODEC2_PATH` precedent).
    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `true` (and, on stderr, why) when a test may actually touch hardware:
    /// the single `IAX_THUMBDV_TESTS=1` gate spec §5 requires, so a machine
    /// with no dongle — and plain CI — stays green.
    pub(crate) fn hardware_opted_in() -> bool {
        if std::env::var("IAX_THUMBDV_TESTS").ok().as_deref() == Some("1") {
            return true;
        }
        eprintln!(
            "skipping hardware ThumbDV test (set IAX_THUMBDV_TESTS=1 with a real dongle attached)"
        );
        false
    }

    /// Runs `f` with `IAX_THUMBDV_PORT` pinned to a path the VID/PID scan can
    /// never return, so any code under test takes its "no candidate ports"
    /// path and physically opens NOTHING. This is what lets the strict
    /// no-fallback tests assert against real entry points without going
    /// anywhere near the dongle.
    pub(crate) fn with_no_thumbdv<R>(f: impl FnOnce() -> R) -> R {
        let _guard = env_lock();
        // SAFETY: serialized by `env_lock`; no other thread reads or writes
        // `IAX_THUMBDV_PORT` while the guard is held.
        unsafe {
            std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-NOSUCHDEVICE");
        }
        let out = f();
        // SAFETY: same serialization as the `set_var` above.
        unsafe {
            std::env::remove_var("IAX_THUMBDV_PORT");
        }
        out
    }
}

// ---------------------------------------------------------------------
// Hardware backend tests: ThumbDV worker thread, driven via MockTransport.
// ---------------------------------------------------------------------

#[cfg(all(test, feature = "ambe-hw"))]
mod hw_tests {
    use super::test_support::with_no_thumbdv;
    use super::{
        AmbeBackend, AmbeStream, AmbeVoice, NULL_AMBE_FRAME, open_ambe, open_ambe_stream,
        open_hw_stream_with, open_hw_stream_with_handle, open_hw_with,
    };
    use ambe_thumbdv::{
        MockTransport, ThumbDv, channel_in, dcmode_off, ecmode_off, gain_zero, init_encdec,
        prodid_query, ratep_dstar, reset, speech_in,
    };
    use std::time::{Duration, Instant};

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    /// One complete Speech response packet with every sample set to `value`
    /// — matches the wire format `hw_decode_round_trips_through_worker`
    /// already pins (`61 01 42 02 00 A0` header + 160 big-endian samples).
    fn speech_response(value: i16) -> Vec<u8> {
        let mut r = hex("61 01 42 02 00 A0");
        for _ in 0..160 {
            r.extend_from_slice(&value.to_be_bytes());
        }
        r
    }

    /// One complete Channel response packet carrying `frame` — matches the
    /// wire format `hw_encode_round_trips_through_worker` already pins
    /// (`61 00 0B 01 01 48` header + 9 raw bytes).
    fn channel_response(frame: [u8; 9]) -> Vec<u8> {
        let mut r = hex("61 00 0B 01 01 48");
        r.extend_from_slice(&frame);
        r
    }

    /// Busy-poll `poll_decoded` (it's non-blocking and the worker replies on
    /// its own thread) up to `timeout`, matching this file's existing
    /// "no sleep-then-assert" convention for latency-sensitive waits.
    fn poll_until<S: AmbeStream + ?Sized>(stream: &mut S, timeout: Duration) -> Option<[i16; 160]> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pcm) = stream.poll_decoded() {
                return Some(pcm);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Same as [`poll_until`] but for the encode side's `poll_encoded`.
    fn poll_encoded_until<S: AmbeStream + ?Sized>(
        stream: &mut S,
        timeout: Duration,
    ) -> Option<[u8; 9]> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = stream.poll_encoded() {
                return Some(frame);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Join a worker thread with a bound, so a hung/leaked worker fails the
    /// test loudly instead of hanging the whole suite.
    fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) -> bool {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = handle.join();
            let _ = done_tx.send(result.is_ok());
        });
        done_rx.recv_timeout(timeout) == Ok(true)
    }

    /// Scripts the `ThumbDv::init_with` cookbook exchange (§8.1) on a
    /// `MockTransport` — mirrors ambe-thumbdv's own `device.rs` test
    /// fixture (`scripted_init`), which isn't exported for reuse here.
    fn scripted_init() -> MockTransport {
        let mut m = MockTransport::new();
        m.expect(reset(), vec![hex("61 00 01 00 39")]);
        let mut prod = hex("61 00 0B 00 30");
        prod.extend_from_slice(b"AMBE3000R\0");
        m.expect(prodid_query(), vec![prod]);
        let mut ver = hex("61 00 07 00 31");
        ver.extend_from_slice(b"V120A\0");
        m.expect(ambe_thumbdv::packet::verstring_query(), vec![ver]);
        m.expect(ratep_dstar(), vec![hex("61 00 02 00 0A 00")]);
        m.expect(init_encdec(), vec![hex("61 00 02 00 0B 00")]);
        m.expect(ecmode_off(), vec![hex("61 00 02 00 05 00")]);
        m.expect(dcmode_off(), vec![hex("61 00 02 00 06 00")]);
        m.expect(gain_zero(), vec![hex("61 00 02 00 4B 00")]);
        m
    }

    #[test]
    fn hw_decode_round_trips_through_worker() {
        let mut mock = scripted_init();
        let frame = [0u8; 9];
        let mut resp = hex("61 01 42 02 00 A0");
        for _ in 0..160 {
            resp.extend_from_slice(&1234i16.to_be_bytes());
        }
        mock.expect(channel_in(&frame), vec![resp]);

        let dv = ThumbDv::init_with(mock).unwrap();
        let mut hw = open_hw_with(dv);
        let out = hw.decode(&frame);
        assert_eq!(out.len(), 160);
        assert_eq!(out, [1234i16; 160]);
    }

    #[test]
    fn hw_encode_round_trips_through_worker() {
        let mut mock = scripted_init();
        let pcm = [777i16; 160];
        let mut resp = hex("61 00 0B 01 01 48");
        resp.extend_from_slice(&[0x5Au8; 9]);
        mock.expect(speech_in(&pcm), vec![resp]);

        let dv = ThumbDv::init_with(mock).unwrap();
        let mut hw = open_hw_with(dv);
        let out = hw.encode(&pcm);
        assert_eq!(out, [0x5Au8; 9]);
    }

    #[test]
    fn hw_worker_error_returns_silence_not_hang() {
        let mut mock = scripted_init();
        let frame = [0u8; 9];
        // Respond to the channel_in request with a control Ready packet
        // instead of the expected Speech response: decode_frame() surfaces
        // this as DeviceError::Protocol, which the worker must translate
        // into silence rather than propagating a hang or a panic to the
        // audio thread.
        mock.expect(channel_in(&frame), vec![hex("61 00 01 00 39")]);

        let dv = ThumbDv::init_with(mock).unwrap();
        let mut hw = open_hw_with(dv);
        let t0 = std::time::Instant::now();
        let out = hw.decode(&frame);
        assert_eq!(out, [0i16; 160]);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "decode() must not hang on a worker error"
        );
    }

    #[test]
    fn open_ambe_strict_hardware_never_silently_substitutes_soft() {
        // Regression test (whole-branch review finding 3, IMPORTANT):
        // `open_ambe` used to fall through to `Soft` on `detect()` failure
        // even for an explicit `Some(Hardware)` request — silently handing
        // an operator who asked for hardware a different backend, with no
        // indication anything changed. This must never happen.
        //
        // `with_no_thumbdv` pins `IAX_THUMBDV_PORT` at a path the VID/PID
        // scan can never return, so this runs the strict-failure path
        // deterministically and — critically — opens NO serial device at
        // all, whether or not a real dongle happens to be attached to the
        // machine running the suite (this repo's rule: never open the
        // ThumbDV outside a task that says to). Without that pin the
        // assertion went vacuous on a dongle-equipped machine, since
        // `Some(Hardware)` also satisfies `assert_ne!(.., Some(Soft))`.
        let result = with_no_thumbdv(|| open_ambe(Some(AmbeBackend::Hardware)).map(|(_, b)| b));
        assert_eq!(
            result, None,
            "an explicit Hardware preference with no hardware available must return None, never \
             a silently substituted Soft backend"
        );
    }

    #[test]
    fn open_ambe_stream_strict_hardware_never_silently_substitutes_soft() {
        // Streaming counterpart of `open_ambe_strict_hardware_never_silently_
        // substitutes_soft` above: same contract, same reasoning, just
        // exercised through `open_ambe_stream`'s own candidate-port scan
        // instead of `ambe_thumbdv::detect()`.
        let result =
            with_no_thumbdv(|| open_ambe_stream(Some(AmbeBackend::Hardware)).map(|(_, b)| b));
        assert_eq!(
            result, None,
            "an explicit Hardware preference with no hardware available must return None, never \
             a silently substituted Soft backend"
        );
    }

    // -------------------------------------------------------------------
    // Pipelined AmbeStream tests (iax-b3e7 M0).
    // -------------------------------------------------------------------

    #[test]
    fn hw_stream_fifo_orders_responses_under_a_full_pipeline() {
        // Four requests fill the pipeline to its bound before any response
        // is available -- MockTransport only ever serves the *last* send's
        // response queue (ties recv_some to the most recent send), so
        // expectations 0-2 get no responses and all four real packets are
        // stacked, in submission order, on expectation 3. Positional
        // matching must still hand them back in the order they were
        // submitted: 100, 101, 102, 103.
        let mut mock = scripted_init();
        let frames: [[u8; 9]; 4] = core::array::from_fn(|i| [u8::try_from(i).unwrap(); 9]);
        mock.expect(channel_in(&frames[0]), vec![]);
        mock.expect(channel_in(&frames[1]), vec![]);
        mock.expect(channel_in(&frames[2]), vec![]);
        mock.expect(
            channel_in(&frames[3]),
            vec![
                speech_response(100),
                speech_response(101),
                speech_response(102),
                speech_response(103),
            ],
        );

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        for f in frames {
            stream.submit_decode(f);
        }
        assert_eq!(
            stream.in_flight(),
            4,
            "all four submissions must be accepted"
        );

        for (i, expected) in [100i16, 101, 102, 103].into_iter().enumerate() {
            let pcm = poll_until(&mut stream, Duration::from_secs(2))
                .unwrap_or_else(|| panic!("frame {i} never decoded"));
            assert_eq!(
                pcm, [expected; 160],
                "frame {i} out of order: expected all-{expected}, positional FIFO matching broke"
            );
        }
        assert_eq!(stream.in_flight(), 0);

        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker did not shut down cleanly"
        );
    }

    #[test]
    fn hw_stream_drops_newest_frame_when_pipeline_full() {
        let mut mock = scripted_init();
        let frames: [[u8; 9]; 4] = core::array::from_fn(|i| [u8::try_from(i).unwrap(); 9]);
        for f in &frames {
            mock.expect(channel_in(f), vec![]);
        }
        // Deliberately no 5th expectation: if submit_decode incorrectly
        // wrote the dropped frame anyway, the worker's send() call would
        // panic here ("unexpected send() after all expectations consumed")
        // instead of this test silently passing.

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        for f in frames {
            stream.submit_decode(f);
        }
        assert_eq!(stream.in_flight(), 4);

        stream.submit_decode([0xFFu8; 9]);
        assert_eq!(
            stream.in_flight(),
            4,
            "submit_decode must drop the newest frame once the pipeline is full, not grow past it"
        );

        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker did not shut down cleanly (a stray write would have panicked the mock first)"
        );
    }

    #[test]
    fn hw_stream_in_flight_tracks_submissions_and_polls() {
        let mut mock = scripted_init();
        let frame = [3u8; 9];
        mock.expect(channel_in(&frame), vec![speech_response(42)]);

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        assert_eq!(stream.in_flight(), 0);
        stream.submit_decode(frame);
        assert_eq!(
            stream.in_flight(),
            1,
            "submit must increment in_flight immediately"
        );

        let pcm = poll_until(&mut stream, Duration::from_secs(2)).expect("decode must arrive");
        assert_eq!(pcm, [42i16; 160]);
        assert_eq!(
            stream.in_flight(),
            0,
            "poll_decoded must decrement in_flight on delivery"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    // -------------------------------------------------------------------
    // Encode-direction mirrors of the decode tests above (iax-2f6b): same
    // FIFO/drop/in-flight contracts, driven through `submit_encode`/
    // `poll_encoded` instead, against `speech_in`/`Response::Channel` on the
    // wire instead of `channel_in`/`Response::Speech`.
    // -------------------------------------------------------------------

    #[test]
    fn hw_stream_encode_fifo_orders_responses_under_a_full_pipeline() {
        let mut mock = scripted_init();
        let pcms: [[i16; 160]; 4] = core::array::from_fn(|i| [i16::try_from(i).unwrap(); 160]);
        mock.expect(speech_in(&pcms[0]), vec![]);
        mock.expect(speech_in(&pcms[1]), vec![]);
        mock.expect(speech_in(&pcms[2]), vec![]);
        mock.expect(
            speech_in(&pcms[3]),
            vec![
                channel_response([100u8; 9]),
                channel_response([101u8; 9]),
                channel_response([102u8; 9]),
                channel_response([103u8; 9]),
            ],
        );

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        for pcm in pcms {
            stream.submit_encode(pcm);
        }
        assert_eq!(
            stream.in_flight_encoded(),
            4,
            "all four submissions must be accepted"
        );

        for (i, expected) in [100u8, 101, 102, 103].into_iter().enumerate() {
            let frame = poll_encoded_until(&mut stream, Duration::from_secs(2))
                .unwrap_or_else(|| panic!("frame {i} never encoded"));
            assert_eq!(
                frame, [expected; 9],
                "frame {i} out of order: expected all-{expected}, positional FIFO matching broke"
            );
        }
        assert_eq!(stream.in_flight_encoded(), 0);

        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker did not shut down cleanly"
        );
    }

    #[test]
    fn hw_stream_drops_newest_encode_frame_when_pipeline_full() {
        let mut mock = scripted_init();
        let pcms: [[i16; 160]; 4] = core::array::from_fn(|i| [i16::try_from(i).unwrap(); 160]);
        for pcm in &pcms {
            mock.expect(speech_in(pcm), vec![]);
        }
        // Deliberately no 5th expectation: if submit_encode incorrectly
        // wrote the dropped frame anyway, the worker's send() call would
        // panic here instead of this test silently passing.

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        for pcm in pcms {
            stream.submit_encode(pcm);
        }
        assert_eq!(stream.in_flight_encoded(), 4);

        stream.submit_encode([0x7FFFi16; 160]);
        assert_eq!(
            stream.in_flight_encoded(),
            4,
            "submit_encode must drop the newest frame once the pipeline is full, not grow past it"
        );

        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker did not shut down cleanly (a stray write would have panicked the mock first)"
        );
    }

    #[test]
    fn hw_stream_encode_in_flight_tracks_submissions_and_polls() {
        let mut mock = scripted_init();
        let pcm = [3i16; 160];
        mock.expect(speech_in(&pcm), vec![channel_response([42u8; 9])]);

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        assert_eq!(stream.in_flight_encoded(), 0);
        stream.submit_encode(pcm);
        assert_eq!(
            stream.in_flight_encoded(),
            1,
            "submit must increment in_flight_encoded immediately"
        );

        let frame =
            poll_encoded_until(&mut stream, Duration::from_secs(2)).expect("encode must arrive");
        assert_eq!(frame, [42u8; 9]);
        assert_eq!(
            stream.in_flight_encoded(),
            0,
            "poll_encoded must decrement in_flight_encoded on delivery"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_encode_response_timeout_yields_a_null_frame_for_the_oldest_outstanding_request() {
        let mut mock = scripted_init();
        let pcm = [5i16; 160];
        mock.expect(speech_in(&pcm), vec![]); // never answered: must time out

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        stream.submit_encode(pcm);
        assert_eq!(stream.in_flight_encoded(), 1);

        let t0 = Instant::now();
        let out = poll_encoded_until(&mut stream, Duration::from_millis(500))
            .expect("a lost response must eventually manufacture a filler frame, not hang forever");
        assert_eq!(
            out, NULL_AMBE_FRAME,
            "the substituted frame goes ON THE AIR: it must be D-Star's null codeword, not \
             all-zero (which is a click, not silence)"
        );
        assert_eq!(stream.in_flight_encoded(), 0);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "timeout substitution took too long: {:?}",
            t0.elapsed()
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_worker_shuts_down_cleanly_with_both_queues_outstanding() {
        let mut mock = scripted_init();
        let frame = [9u8; 9];
        let pcm = [3i16; 160];
        mock.expect(channel_in(&frame), vec![]); // never answered
        mock.expect(speech_in(&pcm), vec![]); // never answered

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        stream.submit_decode(frame);
        stream.submit_encode(pcm);
        assert_eq!(stream.in_flight(), 1);
        assert_eq!(stream.in_flight_encoded(), 1);

        // Drop while both requests are still outstanding -- the worker must
        // exit promptly rather than spin forever waiting for replies nobody
        // is listening for anymore.
        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker must shut down cleanly even with both queues outstanding"
        );
    }

    // -------------------------------------------------------------------
    // A transport whose response stream is scripted INDEPENDENTLY of send
    // ordering. `MockTransport` cannot express this: it only ever serves the
    // response queue attached to the MOST RECENT `send`, so it can never
    // model a device that answers late (after the worker already gave up on
    // a request), nor a read that returns several packets at once. Both are
    // the mechanisms behind the desync bug this section pins down.
    // -------------------------------------------------------------------

    struct ScriptedTransport {
        /// Served one-per-`recv_some`, in order, to satisfy
        /// `hw_stream_init`'s stop-and-wait cookbook.
        init: std::collections::VecDeque<Vec<u8>>,
        /// `(due after init finished, bytes)` — the steady-state script.
        chunks: std::collections::VecDeque<(Duration, Vec<u8>)>,
        /// Bytes handed out across several `recv_some` calls when a chunk is
        /// bigger than the caller's buffer.
        carry: std::collections::VecDeque<u8>,
        sends: usize,
        start: Option<Instant>,
    }

    impl ScriptedTransport {
        fn new(chunks: Vec<(Duration, Vec<u8>)>) -> Self {
            let mut init = std::collections::VecDeque::new();
            let mut prod = hex("61 00 0B 00 30");
            prod.extend_from_slice(b"AMBE3000R\0");
            let mut ver = hex("61 00 07 00 31");
            ver.extend_from_slice(b"V120A\0");
            init.push_back(hex("61 00 01 00 39")); // Ready, after reset
            init.push_back(prod);
            init.push_back(ver);
            for field in ["0A", "0B", "05", "06", "4B"] {
                init.push_back(hex(&format!("61 00 02 00 {field} 00")));
            }
            ScriptedTransport {
                init,
                chunks: chunks.into(),
                carry: std::collections::VecDeque::new(),
                sends: 0,
                start: None,
            }
        }
    }

    impl ambe_thumbdv::Transport for ScriptedTransport {
        fn send(&mut self, _bytes: &[u8]) -> std::io::Result<()> {
            self.sends += 1;
            Ok(())
        }

        fn recv_some(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
            // `hw_stream_init` drains stale bytes BEFORE its first send:
            // answering that drain would shift the whole cookbook by one.
            if self.sends == 0 {
                std::thread::sleep(timeout.min(Duration::from_millis(1)));
                return Ok(0);
            }
            if let Some(resp) = self.init.pop_front() {
                let n = resp.len().min(buf.len());
                buf[..n].copy_from_slice(&resp[..n]);
                return Ok(n);
            }
            let start = *self.start.get_or_insert_with(Instant::now);
            let deadline = Instant::now() + timeout;
            loop {
                while self
                    .chunks
                    .front()
                    .is_some_and(|(at, _)| start.elapsed() >= *at)
                {
                    let (_, bytes) = self.chunks.pop_front().expect("checked above");
                    self.carry.extend(bytes);
                }
                if !self.carry.is_empty() {
                    let n = self.carry.len().min(buf.len());
                    for slot in buf.iter_mut().take(n) {
                        *slot = self.carry.pop_front().expect("checked above");
                    }
                    return Ok(n);
                }
                if Instant::now() >= deadline {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn hw_stream_unexpected_response_type_costs_one_silence_frame_not_desync() {
        // An out-of-band control packet (the chip can emit one unsolicited)
        // must NOT consume an outstanding slot: consuming one would hand the
        // NEXT request's real response to this request and shift the whole
        // stream by one frame permanently. Discarding the stray packet
        // instead costs at most one frame — whichever request ends up
        // without an answer times out into silence — and the response count
        // stays 1:1 with the request count either way.
        let frames = 2usize;
        let mut bytes = hex("61 00 01 00 39"); // stray Ready
        bytes.extend(speech_response(77));
        let transport = ScriptedTransport::new(vec![(Duration::from_millis(10), bytes)]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([1u8; 9]);
        stream.submit_decode([2u8; 9]);

        let first = poll_until(&mut stream, Duration::from_secs(2)).expect("first response");
        assert_eq!(
            first, [77i16; 160],
            "the one real Speech response must reach the OLDEST outstanding request, not be \
             thrown away because a stray control packet arrived ahead of it"
        );
        let second = poll_until(&mut stream, Duration::from_secs(2)).expect("second response");
        assert_eq!(
            second, [0i16; 160],
            "the request left without an answer must time out into exactly one silence frame"
        );
        assert_eq!(
            stream.in_flight(),
            0,
            "responses must stay 1:1 with the {frames} submitted requests"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_a_late_response_after_a_timeout_never_shifts_the_following_frames() {
        // THE DESYNC REGRESSION GUARD (spec §1: "a lost response costs one
        // frame of audio instead of permanently desynchronizing the stream").
        //
        // Frame A is submitted and the device stalls past HW_REPLY_TIMEOUT,
        // so the worker substitutes one silence frame for it. The device then
        // answers A anyway, LATE — while nothing is outstanding. Frame B is
        // submitted afterwards and the device answers it normally.
        //
        // Before the fix the worker had no memory that A's response was still
        // owed: A's late 111 was delivered against B's slot and every
        // subsequent frame stayed shifted by one for the life of the session
        // (possibly across a talker change, minutes later). B must get 222.
        let transport = ScriptedTransport::new(vec![
            (Duration::from_millis(150), speech_response(111)), // A's, far too late
            (Duration::from_millis(400), speech_response(222)), // B's, on time
        ]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([0xAAu8; 9]);
        let substituted =
            poll_until(&mut stream, Duration::from_secs(2)).expect("A must time out into silence");
        assert_eq!(substituted, [0i16; 160]);

        // Let A's late response actually arrive and be reconciled while the
        // pipeline is idle, then start "the next transmission".
        std::thread::sleep(Duration::from_millis(250));
        stream.submit_decode([0xBBu8; 9]);

        let b = poll_until(&mut stream, Duration::from_secs(2)).expect("B must decode");
        assert_eq!(
            b, [222i16; 160],
            "B got the PREVIOUS request's late response — the stream is permanently offset by \
             one frame, which is exactly what spec §1 forbids"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_interleaved_decode_and_encode_route_by_response_type_not_submission_order() {
        // THE CORE iax-2f6b GUARANTEE: decode and encode requests share one
        // worker/one transport, and their responses must stay unambiguous
        // even when both are outstanding at once. Submit a decode request
        // FIRST and an encode request SECOND, but script the ENCODE side's
        // (Channel-shaped) response to arrive first and the DECODE side's
        // (Speech-shaped) response second — the opposite of submission
        // order. Positional-per-kind matching (via the response's own wire
        // type, not "whichever request is next overall") must still route
        // each response to its own queue correctly.
        let mut bytes = channel_response([0x77u8; 9]); // encode's answer, arrives first
        bytes.extend(speech_response(4242)); // decode's answer, arrives second
        let transport = ScriptedTransport::new(vec![(Duration::from_millis(10), bytes)]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([1u8; 9]);
        stream.submit_encode([2i16; 160]);
        assert_eq!(stream.in_flight(), 1);
        assert_eq!(stream.in_flight_encoded(), 1);

        let encoded =
            poll_encoded_until(&mut stream, Duration::from_secs(2)).expect("encode response");
        assert_eq!(
            encoded, [0x77u8; 9],
            "the Channel-typed response must reach the encode queue even though the decode \
             request was submitted first"
        );
        let decoded = poll_until(&mut stream, Duration::from_secs(2)).expect("decode response");
        assert_eq!(
            decoded, [4242i16; 160],
            "the Speech-typed response must reach the decode queue"
        );
        assert_eq!(stream.in_flight(), 0);
        assert_eq!(stream.in_flight_encoded(), 0);

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    /// A transport that fails ONE `recv_some` — the transient USB read error
    /// a busy bus produces — and otherwise replays a time-based script like
    /// [`ScriptedTransport`].
    struct FlakyReadTransport {
        inner: ScriptedTransport,
        fail_at: Duration,
        failed: bool,
    }

    impl FlakyReadTransport {
        fn new(fail_at: Duration, chunks: Vec<(Duration, Vec<u8>)>) -> Self {
            FlakyReadTransport {
                inner: ScriptedTransport::new(chunks),
                fail_at,
                failed: false,
            }
        }
    }

    impl ambe_thumbdv::Transport for FlakyReadTransport {
        fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.inner.send(bytes)
        }

        fn recv_some(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
            if !self.failed
                && self
                    .inner
                    .start
                    .is_some_and(|start| start.elapsed() >= self.fail_at)
            {
                self.failed = true;
                return Err(std::io::Error::other("transient USB read error"));
            }
            self.inner.recv_some(buf, timeout)
        }
    }

    #[test]
    fn hw_stream_one_read_error_does_not_corrupt_both_directions() {
        // iax-2f6b review: a read error is attributable to at most ONE of the
        // two transactions in flight, and nothing in it says which. The old
        // worker substituted for BOTH — so a single transient USB hiccup put
        // an all-zero AMBE frame ON THE AIR for an encode request the device
        // was about to answer correctly, and then swallowed that real answer
        // as `pending_discard` debt when it arrived ~5 ms later. The innocent
        // direction lost a frame and picked up a spurious debt on the
        // strength of the other direction's error.
        //
        // Both requests are outstanding when the error lands, and both real
        // responses arrive well inside HW_REPLY_TIMEOUT: both must be
        // delivered intact.
        let transport = FlakyReadTransport::new(
            Duration::from_millis(10),
            vec![
                (Duration::from_millis(40), speech_response(123)),
                (Duration::from_millis(50), channel_response([0x77u8; 9])),
            ],
        );

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([0xAAu8; 9]);
        stream.submit_encode([0xBBi16; 160]);

        let decoded = poll_until(&mut stream, Duration::from_secs(2)).expect("decode must arrive");
        assert_eq!(
            decoded, [123i16; 160],
            "a read error attributable to at most one transaction must not substitute silence for \
             a decode the device answered correctly"
        );
        let encoded =
            poll_encoded_until(&mut stream, Duration::from_secs(2)).expect("encode must arrive");
        assert_eq!(
            encoded, [0x77u8; 9],
            "the encode direction was innocent: substituting a filler frame for it puts fabricated \
             audio ON THE AIR and desyncs the queue when the real answer lands"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_an_expired_decode_deadline_never_writes_off_the_encode_sides_debt() {
        // iax-2f6b review: `flush_deadline` used to be ONE worker-level
        // value shared by two independent debt counters, and it was only ever
        // cleared inside the idle branch — so a decode-side deadline set
        // minutes ago survived, expired, and the next time both queues
        // drained it zeroed BOTH sides' `pending_discard`. The encode side's
        // genuine debt vanished, its owed straggler was then delivered
        // against the NEXT request's slot, and every following on-air frame
        // was shifted by one for the rest of the transmission.
        //
        // The interleaving: decode A times out (decode debt + a deadline);
        // encode B is submitted immediately, keeping the queues non-empty
        // until that deadline is stale; B times out (fresh ENCODE debt); B's
        // real response then arrives while encode C is outstanding. C must
        // never receive B's answer.
        let transport = ScriptedTransport::new(vec![
            // B's real Channel response, far too late for B — and landing
            // while C is outstanding, which is the whole point.
            (Duration::from_millis(240), channel_response([0x11u8; 9])),
            (Duration::from_millis(270), channel_response([0x22u8; 9])),
        ]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([0xAAu8; 9]); // A: never answered
        let substituted_a =
            poll_until(&mut stream, Duration::from_secs(2)).expect("A must time out into silence");
        assert_eq!(substituted_a, [0i16; 160]);

        stream.submit_encode([0x0Bi16; 160]); // B: answered far too late
        let substituted_b = poll_encoded_until(&mut stream, Duration::from_secs(2))
            .expect("B must time out into a filler frame");
        assert_eq!(substituted_b, NULL_AMBE_FRAME);

        stream.submit_encode([0x0Ci16; 160]); // C
        let c = poll_encoded_until(&mut stream, Duration::from_secs(2)).expect("C must answer");
        assert_ne!(
            c, [0x11u8; 9],
            "C received B's owed straggler: the encode side's debt was written off by the DECODE \
             side's stale deadline, and the on-air stream is now permanently offset by one frame"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_a_lost_decode_response_never_desyncs_the_encode_queue() {
        // Cross-queue variant of `hw_stream_a_late_response_after_a_timeout_
        // never_shifts_the_following_frames`: decode request A times out and
        // is substituted with silence; A's real (Speech-shaped) response
        // then arrives LATE, while an encode request B is outstanding on the
        // OTHER queue. The late Speech response must be recognized as
        // decode-shaped and discarded as A's owed straggler — it must never
        // be mistaken for B's Channel-shaped answer — and B's real response
        // must still reach the encode queue intact.
        let transport = ScriptedTransport::new(vec![
            (Duration::from_millis(150), speech_response(111)), // A's, far too late
            (Duration::from_millis(400), channel_response([0x55u8; 9])), // B's, on time
        ]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        stream.submit_decode([0xAAu8; 9]); // A
        let substituted =
            poll_until(&mut stream, Duration::from_secs(2)).expect("A must time out into silence");
        assert_eq!(substituted, [0i16; 160]);

        // Let A's late response actually arrive and be reconciled while the
        // pipeline is idle, then submit B on the OTHER (encode) queue.
        std::thread::sleep(Duration::from_millis(250));
        stream.submit_encode([0xBBi16; 160]); // B

        let b = poll_encoded_until(&mut stream, Duration::from_secs(2)).expect("B must encode");
        assert_eq!(
            b, [0x55u8; 9],
            "B's real Channel response must reach the encode queue, not be swallowed as A's \
             owed decode straggler (or vice versa)"
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_delivers_every_packet_from_a_single_read() {
        // One serial read routinely spans several 326-byte Speech packets.
        // The worker must drain the deframer completely rather than
        // extracting one packet and then blocking on another `recv_some`
        // that has nothing left to read (5 ms of avoidable stall per extra
        // packet, right when the run loop is draining a stream's tail).
        let mut bytes = Vec::new();
        for v in [10i16, 11, 12, 13] {
            bytes.extend(speech_response(v));
        }
        let transport = ScriptedTransport::new(vec![(Duration::from_millis(5), bytes)]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        for f in 0..4u8 {
            stream.submit_decode([f; 9]);
        }
        for expected in [10i16, 11, 12, 13] {
            let pcm = poll_until(&mut stream, Duration::from_secs(2))
                .unwrap_or_else(|| panic!("frame {expected} never decoded"));
            assert_eq!(pcm, [expected; 160], "positional FIFO order must hold");
        }
        assert_eq!(stream.in_flight(), 0);

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_delivers_every_encode_packet_from_a_single_read() {
        // Encode-direction mirror of `hw_stream_delivers_every_packet_from_
        // a_single_read`: one serial read can span several Channel response
        // packets too.
        let mut bytes = Vec::new();
        for v in [20u8, 21, 22, 23] {
            bytes.extend(channel_response([v; 9]));
        }
        let transport = ScriptedTransport::new(vec![(Duration::from_millis(5), bytes)]);

        let (mut stream, handle) = open_hw_stream_with_handle(transport).unwrap();
        for f in 0..4u8 {
            stream.submit_encode([i16::from(f); 160]);
        }
        for expected in [20u8, 21, 22, 23] {
            let frame = poll_encoded_until(&mut stream, Duration::from_secs(2))
                .unwrap_or_else(|| panic!("frame {expected} never encoded"));
            assert_eq!(frame, [expected; 9], "positional FIFO order must hold");
        }
        assert_eq!(stream.in_flight_encoded(), 0);

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_response_timeout_yields_silence_for_the_oldest_outstanding_request() {
        let mut mock = scripted_init();
        let frame = [5u8; 9];
        mock.expect(channel_in(&frame), vec![]); // never answered: must time out

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        stream.submit_decode(frame);
        assert_eq!(stream.in_flight(), 1);

        let t0 = Instant::now();
        let out = poll_until(&mut stream, Duration::from_millis(500)).expect(
            "a lost response must eventually manufacture a silence frame, not hang forever",
        );
        assert_eq!(out, [0i16; 160]);
        assert_eq!(stream.in_flight(), 0);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "timeout substitution took too long: {:?}",
            t0.elapsed()
        );

        drop(stream);
        assert!(join_with_timeout(handle, Duration::from_secs(1)));
    }

    #[test]
    fn hw_stream_worker_shuts_down_cleanly_with_a_request_still_outstanding() {
        let mut mock = scripted_init();
        let frame = [9u8; 9];
        mock.expect(channel_in(&frame), vec![]); // never answered

        let (mut stream, handle) = open_hw_stream_with_handle(mock).unwrap();
        stream.submit_decode(frame);
        assert_eq!(stream.in_flight(), 1);

        // Drop while the request is still outstanding (no response, and the
        // 100 ms per-request timeout hasn't fired yet) -- the worker must
        // exit promptly rather than spin forever waiting for a reply nobody
        // is listening for anymore.
        drop(stream);
        assert!(
            join_with_timeout(handle, Duration::from_secs(1)),
            "worker must shut down cleanly even with a request still outstanding"
        );
    }

    #[test]
    fn open_hw_stream_with_round_trips_through_the_public_entry_point() {
        // Exercises the public `Box<dyn AmbeStream>`-returning wrapper
        // directly, not just the pub(crate) handle-returning constructor
        // the other tests use.
        let mut mock = scripted_init();
        let frame = [4u8; 9];
        mock.expect(channel_in(&frame), vec![speech_response(9)]);

        let mut stream = open_hw_stream_with(mock).unwrap();
        stream.submit_decode(frame);
        let pcm = poll_until(stream.as_mut(), Duration::from_secs(2)).expect("decode must arrive");
        assert_eq!(pcm, [9i16; 160]);
    }
}

// ---------------------------------------------------------------------
// Real-hardware validation (iax-b3e7 spec §5). Every test here is a no-op
// unless `IAX_THUMBDV_TESTS=1` is set in the environment AND a real ThumbDV
// is attached: normal `cargo test --features ambe-hw` runs on a machine
// with no dongle (or plain CI) must stay green. The env-var check is the
// FIRST statement in each `#[test]` fn (no `#[ignore]` — this codebase
// doesn't use it, see `mint_token.rs`'s `IAX_PORTAL_LIVE` precedent), so
// `cargo test` always runs the function; it just does nothing observable
// without the opt-in.
//
// These tests physically open `/dev/cu.usbserial-DK0EOQVS` (or whatever
// `IAX_THUMBDV_PORT`/the VID·PID scan finds) and must not be run
// concurrently with anything else touching the port — that's the caller's
// responsibility (only one process may hold the dongle).
// ---------------------------------------------------------------------

#[cfg(all(test, feature = "ambe-hw"))]
mod hw_hardware_tests {
    use super::test_support::{hardware_lock, hardware_opted_in};
    use super::{AmbeBackend, open_ambe_stream};
    use std::time::{Duration, Instant};

    #[test]
    fn hardware_detect_finds_the_real_dongle_and_reports_its_prodid() {
        if !hardware_opted_in() {
            return;
        }
        let _hw = hardware_lock();
        // `super::detect_thumbdv()`, not `ambe_thumbdv::detect()` directly:
        // the crate's own entry point is the one that honors
        // `IAX_THUMBDV_PORT`, so on the multi-USB-serial machine spec §4
        // added the override for, this test probes the SAME device the rest
        // of the suite does instead of falling back to a raw VID/PID scan.
        let dv = super::detect_thumbdv()
            .expect("a real ThumbDV must be detected when IAX_THUMBDV_TESTS=1 is set");
        assert_eq!(
            dv.prodid(),
            "AMBE3000F",
            "measured hardware fact (M0 spec): this chip reports AMBE3000F, got {:?}",
            dv.prodid()
        );
    }

    #[test]
    fn hardware_open_ambe_stream_strict_hardware_returns_a_hardware_backed_stream() {
        if !hardware_opted_in() {
            return;
        }
        let _hw = hardware_lock();
        let (_stream, backend) = open_ambe_stream(Some(AmbeBackend::Hardware)).expect(
            "strict Hardware open must succeed when IAX_THUMBDV_TESTS=1 and a dongle is attached",
        );
        assert_eq!(
            backend,
            AmbeBackend::Hardware,
            "D-Star's open path must never silently substitute another backend"
        );
    }

    /// THROUGHPUT REGRESSION GUARD — this is the empirical proof M0 exists
    /// for. The stop-and-wait `AmbeVoice::decode` path this milestone
    /// replaces measured 24.5 ms mean / 27.1 ms max per frame against
    /// D-Star's 20 ms cadence (cannot sustain a stream); the pipelined path
    /// measured 7.45 ms/frame with 50 in flight. This test keeps exactly
    /// [`super::AMBE_STREAM_MAX_IN_FLIGHT`] frames in flight throughout (a
    /// naive submit-all-then-drain loop would submit far faster than the
    /// chip replies and drop almost every frame — a test-shape bug, not a
    /// throughput measurement) and busy-polls `poll_decoded` rather than
    /// sleeping between checks, since a fixed sleep would pad the very
    /// per-frame latency being measured.
    #[test]
    fn hardware_ambe_stream_sustains_100_frames_under_20ms_each() {
        // A LITERAL 4, deliberately not `super::AMBE_STREAM_MAX_IN_FLIGHT`:
        // deriving the test's pipeline depth from the constant under test
        // means a regression of that constant to 1 reshapes the measurement
        // into stop-and-wait (~16 ms/frame — still under budget) instead of
        // failing. The assert below pins the two together so the constant
        // can't drift silently either.
        const IN_FLIGHT: usize = 4;
        const N: usize = 100;
        const BUDGET: Duration = Duration::from_millis(20);
        /// A stall bound on any single frame. Pipelined delivery is bursty
        /// by nature (several responses can land in one read), so this is
        /// deliberately looser than the mean budget — it exists to catch a
        /// pipeline that periodically wedges, which a mean cannot see.
        const MAX_BUDGET: Duration = Duration::from_millis(60);

        if !hardware_opted_in() {
            return;
        }
        assert_eq!(
            super::AMBE_STREAM_MAX_IN_FLIGHT,
            IN_FLIGHT,
            "this guard measures a pipeline {IN_FLIGHT} deep; the shipped bound changed"
        );
        let _hw = hardware_lock();

        // NOT `[0u8; 9]`: an all-zero channel frame decodes to digital
        // silence, which would make the non-silence assertion below vacuous.
        // These are the same bytes the console/station end-to-end tests feed.
        let frame = [0xA5u8, 0x3C, 0x91, 0x77, 0x2E, 0xC4, 0x58, 0xDA, 0x0F];

        let (mut stream, backend) =
            open_ambe_stream(Some(AmbeBackend::Hardware)).expect("dongle required for this test");
        assert_eq!(backend, AmbeBackend::Hardware);

        // Prime the pipeline (mirrors DstarSession's own priming, spec item
        // 2) before starting the clock, so warm-up isn't counted in the
        // measured mean.
        for _ in 0..IN_FLIGHT {
            stream.submit_decode(frame);
        }

        let hang_guard = Instant::now() + Duration::from_secs(10);
        let mut submitted = IN_FLIGHT;
        let mut decoded = 0usize;
        let mut nonsilent = 0usize;
        let mut max_gap = Duration::ZERO;
        let t0 = Instant::now();
        let mut last = t0;
        while decoded < N {
            assert!(
                Instant::now() < hang_guard,
                "pipeline stalled: only {decoded}/{N} frames decoded within the hang guard"
            );
            if let Some(pcm) = stream.poll_decoded() {
                decoded += 1;
                max_gap = max_gap.max(last.elapsed());
                last = Instant::now();
                if pcm.iter().any(|s| *s != 0) {
                    nonsilent += 1;
                }
                if submitted < N {
                    stream.submit_decode(frame);
                    submitted += 1;
                }
            } else {
                std::thread::yield_now();
            }
        }
        let elapsed = t0.elapsed();
        let mean_per_frame = elapsed / u32::try_from(N).unwrap();

        // Without this the guard passes vacuously against a wedged device:
        // `hw_stream_write_decode` substitutes a silence frame the instant
        // `transport.send` fails, so a chip that decodes NOTHING reports a
        // sub-millisecond "throughput".
        assert!(
            nonsilent > 0,
            "every one of the {N} decoded frames was digital silence — the device is not actually \
             decoding, so this measurement is meaningless"
        );
        assert!(
            mean_per_frame < BUDGET,
            "mean {mean_per_frame:?}/frame over {N} frames must sustain under D-Star's \
             {BUDGET:?} cadence budget (measured baseline for the fixed pipelined path: \
             7.45ms/frame; the bug this milestone fixes was 24.5ms mean stop-and-wait) — got \
             {mean_per_frame:?}"
        );
        assert!(
            max_gap < MAX_BUDGET,
            "worst single-frame gap {max_gap:?} exceeded {MAX_BUDGET:?}: the pipeline wedges \
             periodically even though the mean ({mean_per_frame:?}) looks healthy"
        );
        eprintln!(
            "hardware_ambe_stream_sustains_100_frames_under_20ms_each: mean {mean_per_frame:?}/frame \
             over {N} frames ({elapsed:?} total, max gap {max_gap:?}, {nonsilent}/{N} non-silent), \
             budget {BUDGET:?}"
        );
    }

    /// The ENCODE twin of the guard above (iax-2f6b): the TX direction has
    /// the tightest real-time margin in the system — a measured ~15.9 ms per
    /// frame against the same 20 ms budget, roughly 4 ms of headroom, versus
    /// decode's 7.3 ms.
    ///
    /// Without this, a change to `hw_stream_write_encode`/`hw_stream_write_req`
    /// that adds a round trip or a retry could push encode to 40 ms/frame —
    /// halving the frames a live transmission gets on the air, since mic
    /// frames arrive every 20 ms and `MAX_PENDING_TX_FRAMES` then starts
    /// discarding them — while the whole suite, INCLUDING the D-Star TX
    /// round trip (which allows 400 ms per frame), stayed green.
    #[test]
    fn hardware_ambe_stream_encodes_100_frames_under_20ms_each() {
        const IN_FLIGHT: usize = 4;
        const N: usize = 100;
        const BUDGET: Duration = Duration::from_millis(20);
        /// Same reasoning as the decode guard's: pipelined delivery is
        /// bursty, so the per-frame stall bound is looser than the mean.
        const MAX_BUDGET: Duration = Duration::from_millis(60);

        if !hardware_opted_in() {
            return;
        }
        assert_eq!(
            super::AMBE_STREAM_MAX_IN_FLIGHT,
            IN_FLIGHT,
            "this guard measures a pipeline {IN_FLIGHT} deep; the shipped bound changed"
        );
        let _hw = hardware_lock();

        // A real 400 Hz tone, not silence: an all-zero PCM frame is exactly
        // the input a broken encode path would also produce, which would make
        // the non-null assertion below vacuous.
        let mut pcm = [0i16; 160];
        for (i, s) in pcm.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let v = (0.6
                * f32::from(i16::MAX)
                * (std::f32::consts::TAU * 400.0 * i as f32 / 8_000.0).sin())
                as i16;
            *s = v;
        }

        let (mut stream, backend) =
            open_ambe_stream(Some(AmbeBackend::Hardware)).expect("dongle required for this test");
        assert_eq!(backend, AmbeBackend::Hardware);

        for _ in 0..IN_FLIGHT {
            stream.submit_encode(pcm);
        }

        let hang_guard = Instant::now() + Duration::from_secs(10);
        let mut submitted = IN_FLIGHT;
        let mut encoded = 0usize;
        let mut nontrivial = 0usize;
        let mut max_gap = Duration::ZERO;
        let t0 = Instant::now();
        let mut last = t0;
        while encoded < N {
            assert!(
                Instant::now() < hang_guard,
                "encode pipeline stalled: only {encoded}/{N} frames encoded within the hang guard"
            );
            if let Some(frame) = stream.poll_encoded() {
                encoded += 1;
                max_gap = max_gap.max(last.elapsed());
                last = Instant::now();
                // Neither all-zero (a write failure's substitute) nor the
                // null codeword (a response timeout's substitute).
                if frame != [0u8; 9] && frame != super::NULL_AMBE_FRAME {
                    nontrivial += 1;
                }
                if submitted < N {
                    stream.submit_encode(pcm);
                    submitted += 1;
                }
            } else {
                std::thread::yield_now();
            }
        }
        let elapsed = t0.elapsed();
        let mean_per_frame = elapsed / u32::try_from(N).unwrap();

        // Vacuity guard, exactly as on the decode side: `hw_stream_write_encode`
        // substitutes a filler frame the instant `transport.send` fails, so a
        // chip that encodes NOTHING would otherwise report a sub-millisecond
        // "throughput".
        assert!(
            nontrivial > 0,
            "every one of the {N} encoded frames was a substituted filler — the device is not \
             actually encoding, so this measurement is meaningless"
        );
        assert!(
            mean_per_frame < BUDGET,
            "mean {mean_per_frame:?}/frame over {N} encodes must sustain under D-Star's \
             {BUDGET:?} cadence budget (measured baseline: ~15.9ms/frame — the tightest margin \
             in the system) — got {mean_per_frame:?}"
        );
        assert!(
            max_gap < MAX_BUDGET,
            "worst single-frame encode gap {max_gap:?} exceeded {MAX_BUDGET:?}: the pipeline \
             wedges periodically even though the mean ({mean_per_frame:?}) looks healthy"
        );
        eprintln!(
            "hardware_ambe_stream_encodes_100_frames_under_20ms_each: mean {mean_per_frame:?}/frame \
             over {N} frames ({elapsed:?} total, max gap {max_gap:?}, {nontrivial}/{N} \
             non-substituted), budget {BUDGET:?}"
        );
    }
}

// ---------------------------------------------------------------------
// ThumbDV busy/absent classification tests (iax-b3e7 spec §4). Hardware-free
// seam: `classify_thumbdv_failure_with` takes the candidate list and the
// "try to open this port" outcome as plain arguments, so every branch is
// exercisable without a real dongle (or even the `MockTransport`/
// `ThumbDv`/`DVSI`-packet machinery `hw_tests` above needs — this logic
// never speaks the wire protocol at all, it only reasons about `io::Error`
// kinds).
// ---------------------------------------------------------------------

#[cfg(all(test, feature = "ambe-hw"))]
mod thumbdv_failure_tests {
    use super::test_support::{env_lock, hardware_lock, hardware_opted_in};
    use super::{
        ThumbDvFailure, classify_thumbdv_failure, classify_thumbdv_failure_with,
        thumbdv_candidate_ports, thumbdv_candidate_ports_from, thumbdv_port_override,
    };
    use std::io;

    #[test]
    fn no_candidates_means_unplugged() {
        let got = classify_thumbdv_failure_with(&[], |_| Ok(()));
        assert_eq!(got, ThumbDvFailure::Unplugged);
    }

    #[test]
    fn candidate_busy_names_the_port() {
        let port = "/dev/cu.usbserial-DK0EOQVS".to_string();
        let got = classify_thumbdv_failure_with(std::slice::from_ref(&port), |p| {
            assert_eq!(p, port, "must trial-open the candidate it was given");
            Err(io::Error::from(io::ErrorKind::ResourceBusy))
        });
        assert_eq!(got, ThumbDvFailure::Busy { port: port.clone() });
        assert_eq!(
            got.message(),
            format!("ThumbDV at {port} is busy — another process has it open")
        );
    }

    #[test]
    fn candidate_permission_denied_also_counts_as_busy() {
        // Some setups (e.g. a udev rule that hasn't granted access yet)
        // surface a held-by-someone-else port as permission-denied rather
        // than EBUSY -- both read the same to an operator: "I can't have
        // this port, something else does."
        let port = "/dev/cu.usbserial-XYZ".to_string();
        let got = classify_thumbdv_failure_with(std::slice::from_ref(&port), |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(got, ThumbDvFailure::Busy { port });
    }

    #[test]
    fn candidate_present_but_other_error_is_not_busy() {
        // A candidate that fails to open for some unrelated reason (e.g. it
        // briefly vanished, or a transient I/O error) is neither
        // "unplugged" (a candidate DID show up) nor "busy" (nothing said
        // someone else has it) -- `Other` is the honest answer.
        let got = classify_thumbdv_failure_with(&["/dev/cu.usbserial-GHOST".to_string()], |_| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert_eq!(got, ThumbDvFailure::Other);
    }

    #[test]
    fn first_candidate_free_second_busy_still_finds_the_busy_one() {
        // Real machines can have more than one FTDI VID/PID-matching serial
        // device attached; the busy one might not be first in the scan.
        let free = "/dev/cu.usbserial-FREE".to_string();
        let busy = "/dev/cu.usbserial-BUSY".to_string();
        let candidates = [free, busy.clone()];
        let got = classify_thumbdv_failure_with(&candidates, |p| {
            if p == busy {
                Err(io::Error::from(io::ErrorKind::ResourceBusy))
            } else {
                Ok(())
            }
        });
        assert_eq!(got, ThumbDvFailure::Busy { port: busy });
    }

    #[test]
    fn every_candidate_opens_fine_is_other_not_busy_or_unplugged() {
        // Pathological (detect() just failed, yet every candidate opens
        // clean here) -- can't be "unplugged" (a candidate exists) or "busy"
        // (nothing refused to open); `Other` is the only honest answer.
        let got = classify_thumbdv_failure_with(&["/dev/cu.usbserial-OK".to_string()], |_| Ok(()));
        assert_eq!(got, ThumbDvFailure::Other);
    }

    #[test]
    fn unplugged_message_matches_the_spec_wording() {
        assert_eq!(
            ThumbDvFailure::Unplugged.message(),
            "no ThumbDV detected — plug in the dongle and try again"
        );
    }

    // ---- IAX_THUMBDV_PORT: narrows the VID/PID scan, never widens it ----
    //
    // ON-AIR SAFETY. The FTDI 0x0403:0x6015 / "ThumbDV" filter in
    // `SerialTransport::candidate_ports()` is the only thing keeping every
    // opener in this crate away from a USB radio interface's serial port
    // (opening one asserts RTS, which on the reference AllScan UCI150 is the
    // transmitter keying line). The env override must therefore be able to
    // SELECT among scanned candidates and nothing else; the tests below pin
    // that. They use the injected `thumbdv_candidate_ports_from` seam, so
    // they assert the policy without depending on what is plugged into the
    // machine running them.

    #[test]
    fn env_override_narrows_the_scan_to_the_pinned_candidate() {
        let scanned = [
            "/dev/cu.usbserial-AAA".to_string(),
            "/dev/cu.usbserial-BBB".to_string(),
        ];
        assert_eq!(
            thumbdv_candidate_ports_from(Some("/dev/cu.usbserial-BBB"), &scanned),
            vec!["/dev/cu.usbserial-BBB".to_string()],
            "the override must pick WHICH scanned ThumbDV to use — spec §4's whole purpose"
        );
    }

    #[test]
    fn env_override_naming_an_unscanned_port_yields_no_candidates() {
        // The hazard case: an operator with several USB-serial devices sets
        // IAX_THUMBDV_PORT to the wrong one — the UCI150's WCH CH343 port,
        // which the VID/PID scan deliberately excludes. Every opener in this
        // crate walks this candidate list (including
        // `classify_thumbdv_failure`'s trial open, which runs on the FAILURE
        // path), so a non-empty result here means a radio interface's tty
        // gets opened and its transmitter keyed. It must be empty.
        let scanned = ["/dev/cu.usbserial-DK0EOQVS".to_string()];
        assert!(
            thumbdv_candidate_ports_from(Some("/dev/cu.wchusbserial5B210098201"), &scanned)
                .is_empty(),
            "IAX_THUMBDV_PORT must NEVER be able to name a port the FTDI VID/PID scan rejected"
        );
    }

    #[test]
    fn no_env_override_returns_the_scan_verbatim() {
        let scanned = ["/dev/cu.usbserial-DK0EOQVS".to_string()];
        assert_eq!(
            thumbdv_candidate_ports_from(None, &scanned),
            scanned.to_vec(),
            "with no override the VID/PID scan is used exactly as-is"
        );
    }

    #[test]
    fn env_override_is_read_from_the_environment() {
        let _guard = env_lock();
        // SAFETY: serialized by `env_lock`; no other thread reads or writes
        // `IAX_THUMBDV_PORT` while the guard is held.
        unsafe {
            std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-PINNED");
        }
        let override_val = thumbdv_port_override();
        // A path no VID/PID scan can return, so this also proves the
        // end-to-end refusal (and physically opens nothing).
        let candidates = thumbdv_candidate_ports();
        // SAFETY: same serialization as the `set_var` above.
        unsafe {
            std::env::remove_var("IAX_THUMBDV_PORT");
        }

        assert_eq!(override_val, Some("/dev/cu.usbserial-PINNED".to_string()));
        assert!(
            candidates.is_empty(),
            "a pinned path the scan never produced must be refused, got {candidates:?}"
        );
    }

    #[test]
    fn empty_env_override_is_treated_as_unset() {
        let _guard = env_lock();
        // SAFETY: serialized by `env_lock`.
        unsafe {
            std::env::set_var("IAX_THUMBDV_PORT", "");
        }
        let override_val = thumbdv_port_override();
        // SAFETY: same serialization as the `set_var` above.
        unsafe {
            std::env::remove_var("IAX_THUMBDV_PORT");
        }
        assert_eq!(
            override_val, None,
            "an empty IAX_THUMBDV_PORT must not pin to an empty path"
        );
    }

    #[test]
    fn classify_thumbdv_failure_is_callable_and_does_not_panic() {
        // Smoke test of the real (non-injected) entry point. It walks
        // `thumbdv_candidate_ports()` and TRIAL-OPENS each match, i.e. it
        // physically opens the real dongle when one is attached — so it sits
        // behind the same `IAX_THUMBDV_TESTS` gate spec §5 requires of every
        // hardware-touching test, and takes the hardware lock. Full branch
        // coverage of the logic itself comes from the
        // `classify_thumbdv_failure_with` seam above, with no I/O at all.
        if !hardware_opted_in() {
            return;
        }
        let _hw = hardware_lock();
        let _ = classify_thumbdv_failure();
    }

    #[test]
    fn hardware_busy_classification_names_the_held_port() {
        // The ONE brief real check this task's instructions allow: hold the
        // real ThumbDV ourselves (standing in for "another process already
        // has it"), confirm `classify_thumbdv_failure` reports it as `Busy`
        // and names the exact port, then release it immediately -- the
        // `drop` happens BEFORE any assertion that could panic, so the port
        // is left closed and free even if this test fails.
        if !hardware_opted_in() {
            return;
        }
        let _hw = hardware_lock();
        let candidates = thumbdv_candidate_ports();
        assert!(
            !candidates.is_empty(),
            "IAX_THUMBDV_TESTS=1 requires a ThumbDV attached"
        );
        let port = candidates[0].clone();
        let held = ambe_thumbdv::SerialTransport::open(&port, 460_800)
            .expect("port must be free for this check to mean anything");

        let failure = classify_thumbdv_failure();
        drop(held); // release BEFORE asserting, so a failed assertion never leaves it held

        assert_eq!(failure, ThumbDvFailure::Busy { port: port.clone() });
        assert_eq!(
            failure.message(),
            format!("ThumbDV at {port} is busy — another process has it open")
        );
    }
}
