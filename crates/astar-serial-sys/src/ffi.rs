// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The flat C-ABI. Every `extern "C"` fn is `#[unsafe(no_mangle)]`, wraps its
//! body in `catch_unwind`, and null-checks its handle/out-pointer arguments.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, c_char, c_float, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use astar_ptt::{
    BridgeConfig, KeyLine, PttAgent, PttBackend, RadioLine, RxKeyMode, Uci150Serial, Uci150Usb,
};

/// Success.
pub const IAX_OK: c_int = 0;
/// A Rust panic was caught at the boundary.
pub const IAX_ERR_PANIC: c_int = -1;
/// A NULL handle or out-pointer argument.
pub const IAX_ERR_NULL: c_int = -2;
/// The serial port could not be opened (bad path, busy, permissions).
pub const IAX_ERR_OPEN: c_int = -3;
/// A serial read/write failed during a tick (e.g. device unplugged).
pub const IAX_ERR_SERIAL: c_int = -4;
/// Autodetect found no matching device.
pub const IAX_ERR_NO_DEVICE: c_int = -5;
/// A caller buffer was too small.
pub const IAX_ERR_BUFFER: c_int = -6;

/// Map an `IAX_ERR_*` code (or [`IAX_OK`]) to a `'static`, NUL-terminated,
/// no-credential C string. The pointer is owned by the library; never free it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_serial_error_text(code: c_int) -> *const c_char {
    let s: &'static [u8] = match code {
        IAX_OK => b"ok\0",
        IAX_ERR_PANIC => b"internal panic\0",
        IAX_ERR_NULL => b"null pointer argument\0",
        IAX_ERR_OPEN => b"serial open failed\0",
        IAX_ERR_SERIAL => b"serial i/o error\0",
        IAX_ERR_NO_DEVICE => b"no serial device found\0",
        IAX_ERR_BUFFER => b"buffer too small\0",
        _ => b"unknown error\0",
    };
    s.as_ptr().cast::<c_char>()
}

/// Operator-key INPUT line.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum IaxKeyLine {
    Cts = 0,
    Dcd = 1,
    Dsr = 2,
    Ri = 3,
}

/// Radio-key OUTPUT line.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum IaxRadioLine {
    Rts = 0,
    Dtr = 1,
}

/// What drives the radio key while receiving.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum IaxRxKeyMode {
    RemotePtt = 0,
    RxActivity = 1,
}

/// Which transport reaches the radio interface's modem-control lines.
///
/// `Usb` is the raw-USB backend (no tty, no dext): sandbox/MAS- and iOS-eligible,
/// the only path that works inside the App Store sandbox, and what astar's own
/// clients select. `Tty` is the OS serial tty (`/dev/cu.*`), which on macOS needs
/// the `CH34x` dext.
///
/// `Tty` is the C enum's **zero value**, so a zero-initialized [`IaxSerialConfig`]
/// selects the tty path. That is an ABI fact, not a recommendation: opening a USB
/// radio interface's tty asserts RTS, and RTS is the radio-key line. Callers should
/// set this field explicitly rather than relying on zero-init.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum IaxSerialTransport {
    Tty = 0,
    Usb = 1,
}

/// Serial client configuration. The only string is `port_path`; carries no credentials.
#[repr(C)]
pub struct IaxSerialConfig {
    /// Serial device path; NULL = autodetect. Ignored when `transport` is `Usb`.
    pub port_path: *const c_char,
    /// Which transport reaches the modem-control lines. Set it explicitly:
    /// `Usb` for the raw-USB / sandbox-eligible backend, `Tty` for the OS
    /// serial device. Zero-init lands on `Tty` — see [`IaxSerialTransport`].
    pub transport: IaxSerialTransport,
    /// Operator-key input line.
    pub key_line: IaxKeyLine,
    /// `true`: input asserted == keyed.
    pub key_active_high: bool,
    /// Radio-key output line.
    pub radio_line: IaxRadioLine,
    /// `true`: assert output == key the radio.
    pub radio_active_high: bool,
    /// Key de-glitch window in ms; 0 = no debounce.
    pub cts_debounce_ms: u32,
    /// What drives the radio key while receiving.
    pub rx_mode: IaxRxKeyMode,
    /// `RxActivity`: level (dBFS) strictly above this counts as active.
    pub rx_floor_db: c_float,
    /// `RxActivity`: keep the radio keyed this long (ms) after audio stops.
    pub rx_hang_ms: u32,
}

pub(crate) fn key_line(c: IaxKeyLine) -> KeyLine {
    match c {
        IaxKeyLine::Cts => KeyLine::Cts,
        IaxKeyLine::Dcd => KeyLine::Dcd,
        IaxKeyLine::Dsr => KeyLine::Dsr,
        IaxKeyLine::Ri => KeyLine::Ri,
    }
}

pub(crate) fn radio_line(c: IaxRadioLine) -> RadioLine {
    match c {
        IaxRadioLine::Rts => RadioLine::Rts,
        IaxRadioLine::Dtr => RadioLine::Dtr,
    }
}

pub(crate) fn bridge_config(cfg: &IaxSerialConfig) -> BridgeConfig {
    BridgeConfig {
        cts_keyed_high: cfg.key_active_high,
        rts_key_high: cfg.radio_active_high,
        cts_debounce: Duration::from_millis(u64::from(cfg.cts_debounce_ms)),
        rx_mode: match cfg.rx_mode {
            IaxRxKeyMode::RemotePtt => RxKeyMode::RemotePtt,
            IaxRxKeyMode::RxActivity => RxKeyMode::RxActivity,
        },
        rx_floor_db: cfg.rx_floor_db,
        rx_hang: Duration::from_millis(u64::from(cfg.rx_hang_ms)),
    }
}

/// Test-only accessor (the conversion is `pub(crate)`).
#[doc(hidden)]
#[must_use]
pub fn test_bridge_config(cfg: &IaxSerialConfig) -> BridgeConfig {
    bridge_config(cfg)
}

/// The backend choice resolved from a config, before any I/O. Keeps the
/// transport routing (which backend, and for tty whether to autodetect) pure and
/// testable; [`iax_serial_open`] just executes the plan.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BackendPlan {
    /// tty backend; `None` = autodetect, `Some(path)` = explicit device path.
    Tty(Option<String>),
    /// Raw-USB backend (enumerates internally; `port_path` is ignored).
    Usb,
}

/// Decide which backend to build from `cfg` without opening anything. `Err` only
/// when a non-NULL `port_path` is not valid UTF-8.
///
/// # Safety
/// `cfg.port_path` must be NULL or point to a valid NUL-terminated C string.
unsafe fn backend_plan(cfg: &IaxSerialConfig) -> Result<BackendPlan, ()> {
    match cfg.transport {
        IaxSerialTransport::Usb => Ok(BackendPlan::Usb),
        IaxSerialTransport::Tty if cfg.port_path.is_null() => Ok(BackendPlan::Tty(None)),
        IaxSerialTransport::Tty => {
            let s = unsafe { CStr::from_ptr(cfg.port_path) }
                .to_str()
                .map_err(|_| ())?;
            Ok(BackendPlan::Tty(Some(s.to_string())))
        }
    }
}

/// Test-only accessor: `(is_usb, tty_path)`. `None` mirrors a `backend_plan`
/// `Err` (non-UTF-8 path).
///
/// # Safety
/// As [`backend_plan`]: `cfg.port_path` must be NULL or a valid C string.
#[doc(hidden)]
#[must_use]
pub unsafe fn test_backend_plan(cfg: &IaxSerialConfig) -> Option<(bool, Option<String>)> {
    match unsafe { backend_plan(cfg) } {
        Ok(BackendPlan::Usb) => Some((true, None)),
        Ok(BackendPlan::Tty(p)) => Some((false, p)),
        Err(()) => None,
    }
}

/// Opaque serial-client handle (a `Box<IaxSerial>` behind a raw pointer).
///
/// All hardware I/O runs on an internal worker thread ([`PttAgent`], iax-239a):
/// [`iax_serial_ptt_tick`] is a non-blocking mailbox exchange, so a wedged USB
/// transfer (device re-enumerated mid-flight) can never park the caller —
/// the tick reports `IAX_ERR_SERIAL` once the worker's heartbeat goes stale.
///
/// **Not thread-safe.** Use one `IaxSerial*` from a single thread at a time:
/// concurrent calls on the same handle are undefined behavior. (This differs
/// from `IaxStation*`, which is internally synchronized.)
pub struct IaxSerial {
    agent: PttAgent,
}

/// Open the serial port (explicit `port_path` or autodetect when NULL), clear
/// the radio line, and build the keying bridge. Returns NULL on any failure
/// (NULL config, no device, port busy, bad path, non-UTF-8 path).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_serial_open(cfg: *const IaxSerialConfig) -> *mut IaxSerial {
    if cfg.is_null() {
        return std::ptr::null_mut();
    }
    let cfg = unsafe { &*cfg };
    catch_unwind(AssertUnwindSafe(|| {
        let key = key_line(cfg.key_line);
        let radio = radio_line(cfg.radio_line);
        // SAFETY: caller guarantees `port_path` is NULL or a valid C string.
        let Ok(plan) = (unsafe { backend_plan(cfg) }) else {
            return std::ptr::null_mut();
        };
        let backend: Box<dyn PttBackend> = match plan {
            BackendPlan::Tty(path) => {
                // Explicit path, or autodetect the first WCH tty device.
                let Some(path) = path.or_else(Uci150Serial::autodetect) else {
                    return std::ptr::null_mut();
                };
                match Uci150Serial::open_with(&path, key, radio) {
                    Ok(b) => Box::new(b),
                    Err(_) => return std::ptr::null_mut(),
                }
            }
            BackendPlan::Usb => match Uci150Usb::open_with(key, radio) {
                Ok(b) => Box::new(b),
                Err(_) => return std::ptr::null_mut(),
            },
        };
        let Some(agent) = PttAgent::new(backend, bridge_config(cfg)) else {
            return std::ptr::null_mut();
        };
        Box::into_raw(Box::new(IaxSerial { agent }))
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Close the handle: stop the worker (its fail-safe drops the radio line on
/// the way out), then free. Never blocks indefinitely — a worker wedged in a
/// dead hardware transfer is detached after a short grace period (its device
/// is gone, so no radio line is left to unkey). NULL is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_serial_close(s: *mut IaxSerial) {
    if s.is_null() {
        return;
    }
    // Guard the teardown: `PttAgent`'s `Drop` signals + reaps the worker, so a
    // panic here must not unwind across the C boundary (UB). Mirrors
    // `iax_station_free`.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(s) });
    }));
}

/// One keying tick: NON-BLOCKING (iax-239a). Publishes `remote_keyed`/`rx_db`
/// (from the consumer's `AstarStation` snapshot) to the worker thread — which
/// does the actual line I/O on its own ~20 ms cadence — and pops the next
/// pending debounced key edge. On return, `*out_changed` is `true` when the
/// call PTT should change, and `*out_set_ptt` holds the new value (only
/// meaningful when changed). Returns `IAX_OK` or a negative `IAX_ERR_*`:
/// `IAX_ERR_SERIAL` when the worker died (persistent backend failure) or
/// stalled (wedged hardware transfer) — tear the handle down and re-open once
/// the device is back.
///
/// Call from a single thread per handle; see [`IaxSerial`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_serial_ptt_tick(
    s: *mut IaxSerial,
    remote_keyed: bool,
    rx_db: c_float,
    out_set_ptt: *mut bool,
    out_changed: *mut bool,
) -> c_int {
    if s.is_null() || out_set_ptt.is_null() || out_changed.is_null() {
        return IAX_ERR_NULL;
    }
    let serial = unsafe { &mut *s };
    catch_unwind(AssertUnwindSafe(|| {
        match serial.agent.tick(remote_keyed, rx_db) {
            Ok(edge) => {
                unsafe {
                    *out_changed = edge.is_some();
                    *out_set_ptt = edge.unwrap_or(false);
                }
                IAX_OK
            }
            Err(_) => IAX_ERR_SERIAL,
        }
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Autodetect a serial port (first WCH USB device) and write its path into
/// `out` (NUL-terminated, `cap` bytes). Returns `IAX_OK`, `IAX_ERR_NO_DEVICE`,
/// `IAX_ERR_BUFFER` (path + NUL would not fit), or `IAX_ERR_NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_serial_autodetect(out: *mut c_char, cap: usize) -> c_int {
    if out.is_null() {
        return IAX_ERR_NULL;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let Some(path) = Uci150Serial::autodetect() else {
            return IAX_ERR_NO_DEVICE;
        };
        let bytes = path.as_bytes();
        if bytes.len() + 1 > cap {
            return IAX_ERR_BUFFER;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len());
            *out.add(bytes.len()) = 0;
        }
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}
