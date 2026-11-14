// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! C ABI over `astar-ble-ptt`: the Bluetooth PTT button's device table and its
//! pure `ButtonMachine`, behind an opaque `IaxBleMachine*`.
//!
//! Why the machine lives in the engine rather than in Swift: whether a notify
//! keys the radio (the live-epoch filter, the settle window, the confirm gate,
//! release-to-re-arm, RSSI liveness) is the safety argument. It must be one
//! implementation that the Rust tests, the scenario traces and every client
//! share. The transport (`CoreBluetooth`) stays in the app. It turns callbacks
//! into [`IaxBleEvent`]s and obeys the commands in [`IaxBleOutput`]. Those
//! commands are a probe (an RSSI read) and dropping the link. There is no write
//! command, by construction.
//!
//! This module follows the same conventions as `ffi.rs`: `catch_unwind` at every
//! boundary, NULL guards, caller-allocated out-structs and integer codes. It is
//! a poll model with no callbacks. A machine handle is single-threaded: the
//! caller drives it from one thread (the macOS main actor).
//!
//! The ABI adds no behaviour of its own. It translates in both directions and
//! refuses ([`IAX_ERR_INVALID`]) anything it cannot translate, rather than
//! guessing.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use astar_ble_ptt::{
    AdapterState, BleButtonDevice, ButtonMachine, DEVICES, DeviceKind, GattCharacteristic, Intent,
    LinkEvent, Liveness, MachineConfig, NotifyBytes, Output, Uuid128,
};

use crate::ffi::{IAX_ERR_INVALID, IAX_ERR_NULL, IAX_ERR_PANIC, IAX_OK};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// [`IaxBleEvent::kind`]: the client enabled the button. `confirmed` says
/// whether this Mac already holds a confirm press for the peer.
pub const IAX_BLE_EV_ENABLE: u32 = 1;
/// [`IaxBleEvent::kind`]: the client disabled the button. Releases a held key.
pub const IAX_BLE_EV_DISABLE: u32 = 2;
/// [`IaxBleEvent::kind`]: the confirm press was recorded during setup.
pub const IAX_BLE_EV_CONFIRMED: u32 = 3;
/// [`IaxBleEvent::kind`]: the Bluetooth adapter reported a new state in `adapter`.
pub const IAX_BLE_EV_ADAPTER: u32 = 4;
/// [`IaxBleEvent::kind`]: a connect attempt numbered `epoch` succeeded.
pub const IAX_BLE_EV_CONNECTED: u32 = 5;
/// [`IaxBleEvent::kind`]: the peer on `epoch` exposes the button characteristic with NOTIFY.
pub const IAX_BLE_EV_GATT_ACCEPTED: u32 = 6;
/// [`IaxBleEvent::kind`]: the peer on `epoch` is a lookalike without the button service.
pub const IAX_BLE_EV_GATT_REJECTED: u32 = 7;
/// [`IaxBleEvent::kind`]: notifications are on for `epoch`. Starts the settle window.
pub const IAX_BLE_EV_SUBSCRIBED: u32 = 8;
/// [`IaxBleEvent::kind`]: notifications stopped on `epoch`. Releases a held key.
pub const IAX_BLE_EV_UNSUBSCRIBED: u32 = 9;
/// [`IaxBleEvent::kind`]: a notify on `epoch`, its bytes at `payload` / `payload_len`.
pub const IAX_BLE_EV_NOTIFY: u32 = 10;
/// [`IaxBleEvent::kind`]: an RSSI read on `epoch` answered with `rssi` dBm.
pub const IAX_BLE_EV_PROBE_OK: u32 = 11;
/// [`IaxBleEvent::kind`]: an RSSI read on `epoch` failed. Two in a row while keyed release.
pub const IAX_BLE_EV_PROBE_FAILED: u32 = 12;
/// [`IaxBleEvent::kind`]: the link on `epoch` dropped. Releases a held key.
pub const IAX_BLE_EV_DISCONNECTED: u32 = 13;
/// [`IaxBleEvent::kind`]: the Mac is going to sleep. Releases a held key.
pub const IAX_BLE_EV_SLEEP: u32 = 14;
/// [`IaxBleEvent::kind`]: the Mac woke. Never keys by itself.
pub const IAX_BLE_EV_WAKE: u32 = 15;
/// [`IaxBleEvent::kind`]: the engine refused a lease renew. Latches a button that was down.
pub const IAX_BLE_EV_LEASE_LOST: u32 = 16;
/// [`IaxBleEvent::kind`]: the client's 250 ms timer. Drives probes and renewals.
pub const IAX_BLE_EV_TICK: u32 = 17;

/// [`IaxBleEvent::adapter`]: powered on and authorised.
pub const IAX_BLE_ADAPTER_ON: u32 = 0;
/// [`IaxBleEvent::adapter`]: powered off.
pub const IAX_BLE_ADAPTER_OFF: u32 = 1;
/// [`IaxBleEvent::adapter`]: the user has not granted Bluetooth access.
pub const IAX_BLE_ADAPTER_UNAUTHORIZED: u32 = 2;
/// [`IaxBleEvent::adapter`]: the system is resetting the adapter.
pub const IAX_BLE_ADAPTER_RESETTING: u32 = 3;
/// [`IaxBleEvent::adapter`]: this Mac has no LE adapter.
pub const IAX_BLE_ADAPTER_UNSUPPORTED: u32 = 4;

/// [`IaxBleOutput::intent`]: nothing to tell the station this step.
pub const IAX_BLE_INTENT_NONE: u32 = 0;
/// [`IaxBleOutput::intent`]: lease-press the Bluetooth source now.
pub const IAX_BLE_INTENT_PRESS: u32 = 1;
/// [`IaxBleOutput::intent`]: release the Bluetooth source now; see `release_reason`.
pub const IAX_BLE_INTENT_RELEASE: u32 = 2;

/// [`IaxBleDevice::kind`]: a speaker-mic, so the client offers its audio helper.
pub const IAX_BLE_KIND_SPEAKER_MIC: u32 = 0;
/// [`IaxBleDevice::kind`]: a button with no audio of its own.
pub const IAX_BLE_KIND_BUTTON_ONLY: u32 = 1;
/// [`IaxBleDevice::liveness`]: the link is not probed while keyed.
pub const IAX_BLE_LIVENESS_NONE: u32 = 0;
/// [`IaxBleDevice::liveness`]: the link is probed by RSSI reads while keyed.
pub const IAX_BLE_LIVENESS_RSSI: u32 = 1;

// ---------------------------------------------------------------------------
// Device table
// ---------------------------------------------------------------------------

/// One row of the supported-button table, filled by [`iax_bleptt_device_at`].
///
/// Strings are fixed, NUL-terminated UTF-8 arrays so the client allocates and
/// frees nothing, and so Swift reads the UUIDs from here instead of
/// hard-coding them: the Rust table is the only place a UUID is written.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IaxBleDevice {
    /// Stable config key (`inrico-b02ptt`). Never shown to the user.
    pub id: [c_char; 32],
    /// Name for the UI (`Inrico B02PTT`).
    pub display_name: [c_char; 64],
    /// Advertised-name prefix, matched case-sensitively; the per-unit suffix is ignored.
    pub name_prefix: [c_char; 32],
    /// GATT service holding the button: uppercase, hyphenated, 36 characters.
    pub service: [c_char; 37],
    /// The button characteristic (must carry NOTIFY), same form as `service`.
    pub characteristic: [c_char; 37],
    /// `IAX_BLE_KIND_*`.
    pub kind: u32,
    /// `IAX_BLE_LIVENESS_*`.
    pub liveness: u32,
    /// Provenance of the row (the listen-only spike that verified it). Kept for
    /// maintainers; clients must not display it.
    pub verified: [c_char; 128],
}

/// [`MachineConfig::DEFAULT`] in whole milliseconds, filled by
/// [`iax_bleptt_default_config`]. Read-only for the client: it exists so the
/// lease TTL and renew period the client uses come from the same numbers the
/// machine was tested with.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IaxBleConfig {
    /// Notifies inside this window after a subscribe latch instead of keying.
    pub settle_ms: u32,
    /// RSSI probe period while the button holds the key.
    pub probe_keyed_ms: u32,
    /// Consecutive missed probes that mean the link is dead.
    pub probe_misses_dead: u32,
    /// RSSI probe period while idle (signal display only; misses never release).
    pub probe_idle_ms: u32,
    /// Lease TTL the client passes to `iax_station_ptt_lease_press`.
    pub lease_ttl_ms: u32,
    /// How often the client renews the lease while pressed.
    pub renew_ms: u32,
    /// RSSI at or above this is "strong".
    pub band_strong_dbm: i16,
    /// RSSI at or above this (and below strong) is "fair"; below is "weak".
    pub band_fair_dbm: i16,
}

/// Copy `text` into a fixed C array, truncating to leave room for the NUL and
/// zero-filling the rest, so the array is always terminated.
fn fill_fixed<const N: usize>(dst: &mut [c_char; N], text: &str) {
    let bytes = text.as_bytes();
    let copy = bytes.len().min(N.saturating_sub(1));
    for (slot, &byte) in dst.iter_mut().zip(&bytes[..copy]) {
        *slot = c_char::from_ne_bytes([byte]);
    }
    dst[copy..].fill(0);
}

fn kind_to_ffi(kind: DeviceKind) -> u32 {
    match kind {
        DeviceKind::SpeakerMic => IAX_BLE_KIND_SPEAKER_MIC,
        DeviceKind::ButtonOnly => IAX_BLE_KIND_BUTTON_ONLY,
    }
}

fn liveness_to_ffi(liveness: Liveness) -> u32 {
    match liveness {
        Liveness::None => IAX_BLE_LIVENESS_NONE,
        Liveness::Rssi => IAX_BLE_LIVENESS_RSSI,
    }
}

fn device_row(device: &BleButtonDevice) -> IaxBleDevice {
    let mut row = IaxBleDevice {
        id: [0; 32],
        display_name: [0; 64],
        name_prefix: [0; 32],
        service: [0; 37],
        characteristic: [0; 37],
        kind: kind_to_ffi(device.kind),
        liveness: liveness_to_ffi(device.liveness),
        verified: [0; 128],
    };
    fill_fixed(&mut row.id, device.id);
    fill_fixed(&mut row.display_name, device.display_name);
    fill_fixed(&mut row.name_prefix, device.name_prefix);
    fill_fixed(&mut row.service, &device.service.to_string());
    fill_fixed(&mut row.characteristic, &device.characteristic.to_string());
    fill_fixed(&mut row.verified, device.verified);
    row
}

/// Whole milliseconds, saturating: the defaults are all well inside `u32`.
fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// Parse a `const char*` UUID (either case). NULL, non-UTF-8 or malformed → `None`.
unsafe fn uuid_arg(p: *const c_char) -> Option<Uuid128> {
    if p.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    Uuid128::parse(text)
}

/// Number of rows in the supported-button table. Rows are indexed
/// `0..count`; the index is what [`iax_bleptt_new`] and
/// [`iax_bleptt_accepts_gatt`] take.
#[unsafe(no_mangle)]
pub extern "C" fn iax_bleptt_device_count() -> usize {
    DEVICES.len()
}

/// Fill `out` with row `index`. Returns [`IAX_ERR_INVALID`] for an index past
/// the end (leaving `out` untouched), [`IAX_ERR_NULL`] for a NULL `out`, or
/// [`IAX_ERR_PANIC`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_device_at(index: usize, out: *mut IaxBleDevice) -> c_int {
    if out.is_null() {
        return IAX_ERR_NULL;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let Some(device) = DEVICES.get(index) else {
            return IAX_ERR_INVALID;
        };
        unsafe { out.write(device_row(device)) };
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// The row whose advertised-name prefix `name` carries, matched
/// case-sensitively, or `-1`. NULL, non-UTF-8 and a caught panic are also
/// `-1`. Non-UTF-8 is refused rather than converted lossily, because a lossy
/// `"B02PTT-\u{FFFD}"` would match a name no B02PTT advertises.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_match_name(name: *const c_char) -> c_int {
    if name.is_null() {
        return -1;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(text) = unsafe { CStr::from_ptr(name) }.to_str() else {
            return -1;
        };
        astar_ble_ptt::match_advertised_name(Some(text))
            .and_then(|found| DEVICES.iter().position(|d| std::ptr::eq(d, found)))
            .and_then(|index| c_int::try_from(index).ok())
            .unwrap_or(-1)
    }))
    .unwrap_or(-1)
}

/// True only when `service` is row `index`'s button service and
/// `characteristic` is its button characteristic with the NOTIFY bit in
/// `properties`. UUIDs are accepted in either case. The client calls this once
/// per discovered characteristic and accepts the peer if any call is true.
/// A bad index, NULL, non-UTF-8, a malformed UUID or a caught panic is `false`,
/// so a lookalike device can never be accepted by accident.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_accepts_gatt(
    index: usize,
    service: *const c_char,
    characteristic: *const c_char,
    properties: u8,
) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(device) = DEVICES.get(index) else {
            return false;
        };
        let (Some(service), Some(uuid)) = (unsafe { uuid_arg(service) }, unsafe {
            uuid_arg(characteristic)
        }) else {
            return false;
        };
        astar_ble_ptt::accepts_gatt(device, service, &[GattCharacteristic { uuid, properties }])
    }))
    .unwrap_or(false)
}

/// Fill `out` with [`MachineConfig::DEFAULT`], the configuration every
/// [`iax_bleptt_new`] machine runs. Returns [`IAX_ERR_NULL`] for a NULL `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_default_config(out: *mut IaxBleConfig) -> c_int {
    if out.is_null() {
        return IAX_ERR_NULL;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let d = MachineConfig::DEFAULT;
        let cfg = IaxBleConfig {
            settle_ms: millis(d.settle_after_subscribe),
            probe_keyed_ms: millis(d.probe_interval_keyed),
            probe_misses_dead: u32::from(d.probe_misses_dead),
            probe_idle_ms: millis(d.probe_interval_idle),
            lease_ttl_ms: millis(d.lease_ttl),
            renew_ms: millis(d.renew_period),
            band_strong_dbm: d.band_strong_dbm,
            band_fair_dbm: d.band_fair_dbm,
        };
        unsafe { out.write(cfg) };
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

// ---------------------------------------------------------------------------
// Machine handle
// ---------------------------------------------------------------------------

/// Opaque Bluetooth PTT button machine. Heap-allocated by [`iax_bleptt_new`],
/// freed by [`iax_bleptt_free`]. Single-threaded: drive it from one thread.
/// It holds no secret and no I/O; it is the pure `ButtonMachine` and nothing
/// else.
pub struct IaxBleMachine {
    inner: ButtonMachine,
}

/// One transport observation, passed to [`iax_bleptt_event`]. Only the fields
/// its `kind` names are read: `epoch` for link events, `adapter` for
/// [`IAX_BLE_EV_ADAPTER`], `confirmed` for [`IAX_BLE_EV_ENABLE`], `rssi` for
/// [`IAX_BLE_EV_PROBE_OK`], and `payload` / `payload_len` for
/// [`IAX_BLE_EV_NOTIFY`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IaxBleEvent {
    /// `IAX_BLE_EV_*`. Anything else is refused with [`IAX_ERR_INVALID`].
    pub kind: u32,
    /// Connect-attempt number allocated by the client, strictly increasing; 0 is never used.
    pub epoch: u32,
    /// `IAX_BLE_ADAPTER_*`. Anything else is refused with [`IAX_ERR_INVALID`].
    pub adapter: u32,
    /// Whether this Mac already holds a confirm press for the peer.
    pub confirmed: bool,
    /// RSSI in dBm from a successful probe.
    pub rssi: i16,
    /// Notify bytes, borrowed for the call only. May be NULL only when `payload_len` is 0.
    pub payload: *const u8,
    /// The notify's true length. Up to 8 bytes are copied; anything longer
    /// keeps its length and decodes Malformed, so a long payload can never be
    /// truncated into a valid press.
    pub payload_len: usize,
}

/// What the machine decided this step, filled by [`iax_bleptt_event`] and
/// [`iax_bleptt_status`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IaxBleOutput {
    /// `IAX_BLE_INTENT_*`: what to tell the station now. Always NONE from [`iax_bleptt_status`].
    pub intent: u32,
    /// `ReleaseReason` (1 button, 2 malformed, 3 link lost, 4 adapter off,
    /// 5 disabled, 6 sleep) when `intent` is RELEASE, else 0.
    pub release_reason: u32,
    /// Read the peer's RSSI now and report it as [`IAX_BLE_EV_PROBE_OK`] or
    /// [`IAX_BLE_EV_PROBE_FAILED`].
    pub cmd_probe: bool,
    /// Cancel the peer connection now.
    pub cmd_drop_link: bool,
    /// `LinkStatus` (0 disabled … 10 reconnecting), the only source of the UI's status line.
    pub status: u32,
    /// 0 when no RSSI has been read, else `SignalBand` (1 strong, 2 fair, 3 weak).
    pub signal: u32,
    /// Whether `last_rssi` holds a reading.
    pub has_rssi: bool,
    /// Last RSSI in dBm; 0 when `has_rssi` is false.
    pub last_rssi: i16,
    /// The last live notify was a press, whatever the gates said. For the self-test only.
    pub raw_pressed: bool,
    /// The machine holds the key; the client renews the lease while this is true.
    pub pressed: bool,
}

fn adapter_from_ffi(raw: u32) -> Option<AdapterState> {
    match raw {
        IAX_BLE_ADAPTER_ON => Some(AdapterState::On),
        IAX_BLE_ADAPTER_OFF => Some(AdapterState::Off),
        IAX_BLE_ADAPTER_UNAUTHORIZED => Some(AdapterState::Unauthorized),
        IAX_BLE_ADAPTER_RESETTING => Some(AdapterState::Resetting),
        IAX_BLE_ADAPTER_UNSUPPORTED => Some(AdapterState::Unsupported),
        _ => None,
    }
}

/// Copy at most 8 bytes of the notify into `NotifyBytes`, keeping the TRUE
/// length so the machine decodes a long payload as Malformed. Reads no more
/// than 8 bytes whatever `len` says. NULL with a non-zero length is refused.
///
/// # Safety
/// A non-NULL `payload` must be readable for `len` bytes.
unsafe fn notify_from_ffi(payload: *const u8, len: usize) -> Option<NotifyBytes> {
    let mut bytes = [0_u8; 8];
    if payload.is_null() {
        return (len == 0).then_some(NotifyBytes { len: 0, bytes });
    }
    let copied = len.min(bytes.len());
    let head = unsafe { std::slice::from_raw_parts(payload, copied) };
    bytes[..copied].copy_from_slice(head);
    Some(NotifyBytes { len, bytes })
}

/// Translate one [`IaxBleEvent`]. `None` for an unknown kind or adapter
/// state, or a NULL payload with a length, so the caller refuses it.
///
/// # Safety
/// As [`notify_from_ffi`] for `ev.payload`.
unsafe fn link_event_from_ffi(ev: &IaxBleEvent) -> Option<LinkEvent> {
    let epoch = ev.epoch;
    Some(match ev.kind {
        IAX_BLE_EV_ENABLE => LinkEvent::Enable {
            confirmed: ev.confirmed,
        },
        IAX_BLE_EV_DISABLE => LinkEvent::Disable,
        IAX_BLE_EV_CONFIRMED => LinkEvent::Confirmed,
        IAX_BLE_EV_ADAPTER => LinkEvent::Adapter(adapter_from_ffi(ev.adapter)?),
        IAX_BLE_EV_CONNECTED => LinkEvent::Connected { epoch },
        IAX_BLE_EV_GATT_ACCEPTED => LinkEvent::GattAccepted { epoch },
        IAX_BLE_EV_GATT_REJECTED => LinkEvent::GattRejected { epoch },
        IAX_BLE_EV_SUBSCRIBED => LinkEvent::Subscribed { epoch },
        IAX_BLE_EV_UNSUBSCRIBED => LinkEvent::Unsubscribed { epoch },
        IAX_BLE_EV_NOTIFY => LinkEvent::Notify {
            epoch,
            bytes: unsafe { notify_from_ffi(ev.payload, ev.payload_len) }?,
        },
        IAX_BLE_EV_PROBE_OK => LinkEvent::ProbeOk {
            epoch,
            rssi: ev.rssi,
        },
        IAX_BLE_EV_PROBE_FAILED => LinkEvent::ProbeFailed { epoch },
        IAX_BLE_EV_DISCONNECTED => LinkEvent::Disconnected { epoch },
        IAX_BLE_EV_SLEEP => LinkEvent::Sleep,
        IAX_BLE_EV_WAKE => LinkEvent::Wake,
        IAX_BLE_EV_LEASE_LOST => LinkEvent::LeaseLost,
        IAX_BLE_EV_TICK => LinkEvent::Tick,
        _ => return None,
    })
}

fn output_to_ffi(o: Output) -> IaxBleOutput {
    let (intent, release_reason) = match o.intent {
        Intent::None => (IAX_BLE_INTENT_NONE, 0),
        Intent::Press => (IAX_BLE_INTENT_PRESS, 0),
        Intent::Release(reason) => (IAX_BLE_INTENT_RELEASE, reason as u32),
    };
    IaxBleOutput {
        intent,
        release_reason,
        cmd_probe: o.cmds.probe,
        cmd_drop_link: o.cmds.drop_link,
        status: o.status as u32,
        signal: o.signal.map_or(0, |band| band as u32),
        has_rssi: o.last_rssi.is_some(),
        last_rssi: o.last_rssi.unwrap_or(0),
        raw_pressed: o.raw_pressed,
        pressed: o.pressed,
    }
}

/// Create a machine for row `device_index`, running
/// [`MachineConfig::DEFAULT`]. It starts Disabled and unpressed. Returns NULL
/// for an index past the end or a caught panic. Release it with
/// [`iax_bleptt_free`].
#[unsafe(no_mangle)]
pub extern "C" fn iax_bleptt_new(device_index: usize) -> *mut IaxBleMachine {
    catch_unwind(AssertUnwindSafe(|| {
        DEVICES
            .get(device_index)
            .map_or(std::ptr::null_mut(), |device| {
                Box::into_raw(Box::new(IaxBleMachine {
                    inner: ButtonMachine::new(device, MachineConfig::DEFAULT),
                }))
            })
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Feed one event at `now_ms` (a monotonic offset from any origin the client
/// keeps fixed for the machine's life) and write the decision to `out`.
///
/// Returns [`IAX_ERR_INVALID`] for an unknown `kind` or `adapter`, or a NULL
/// `payload` with a non-zero `payload_len`. The machine and `out` are then
/// untouched. Returns [`IAX_ERR_NULL`] for a NULL `h`, `event` or `out`, and
/// [`IAX_ERR_PANIC`] for a caught panic. The client treats a panic as a
/// release, because it can no longer trust the machine's state.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_event(
    h: *mut IaxBleMachine,
    event: *const IaxBleEvent,
    now_ms: u64,
    out: *mut IaxBleOutput,
) -> c_int {
    if h.is_null() || event.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let machine = unsafe { &mut *h };
    let event = unsafe { &*event };
    catch_unwind(AssertUnwindSafe(|| {
        let Some(link) = (unsafe { link_event_from_ffi(event) }) else {
            return IAX_ERR_INVALID;
        };
        let output = machine.inner.handle(link, Duration::from_millis(now_ms));
        unsafe { out.write(output_to_ffi(output)) };
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Write the machine's current state to `out` with `intent` NONE and no
/// commands, for a UI that needs the status without feeding an event.
/// Returns [`IAX_ERR_NULL`] for a NULL `h` or `out`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_status(h: *mut IaxBleMachine, out: *mut IaxBleOutput) -> c_int {
    if h.is_null() || out.is_null() {
        return IAX_ERR_NULL;
    }
    let machine = unsafe { &*h };
    catch_unwind(AssertUnwindSafe(|| {
        unsafe { out.write(output_to_ffi(machine.inner.output())) };
        IAX_OK
    }))
    .unwrap_or(IAX_ERR_PANIC)
}

/// Free a machine from [`iax_bleptt_new`]. NULL is a no-op. Freeing does not
/// release anything at the station. The client releases the Bluetooth source
/// before it frees the machine, and the engine's 1.5 s lease covers a client
/// that does not.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iax_bleptt_free(h: *mut IaxBleMachine) {
    if h.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(h) });
    }));
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use std::ffi::c_char;

    use super::{NotifyBytes, fill_fixed, notify_from_ffi};

    #[test]
    fn notify_keeps_the_true_length_of_a_long_payload() {
        // Through the machine a truncated [01, 0 x 7] would still decode
        // Malformed, so only this direct check pins that `len` is the true 9.
        let buf = [0x01_u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        assert_eq!(
            unsafe { notify_from_ffi(buf.as_ptr(), 9) },
            Some(NotifyBytes {
                len: 9,
                bytes: [
                    buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]
                ],
            })
        );
    }

    #[test]
    fn notify_null_is_empty_only_without_a_length() {
        assert_eq!(
            unsafe { notify_from_ffi(ptr::null(), 0) },
            Some(NotifyBytes {
                len: 0,
                bytes: [0; 8]
            })
        );
        assert_eq!(unsafe { notify_from_ffi(ptr::null(), 1) }, None);
    }

    #[test]
    fn notify_reads_no_more_than_len() {
        // Nine readable bytes, so an over-read stays in bounds and shows up as
        // 0xFF in the copy instead of as undefined behaviour.
        let buf = [0x01_u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            unsafe { notify_from_ffi(buf.as_ptr(), 1) },
            Some(NotifyBytes {
                len: 1,
                bytes: [0x01, 0, 0, 0, 0, 0, 0, 0]
            })
        );
    }

    #[test]
    fn fill_fixed_always_leaves_a_nul() {
        // Too long: truncated to N - 1 bytes plus the NUL, so Swift never
        // reads past the array.
        // `from_ne_bytes`, not `as`, so pedantic clippy sees no sign cast.
        let c = |byte: u8| c_char::from_ne_bytes([byte]);
        let mut dst = [c(0x7f); 4];
        fill_fixed(&mut dst, "abcdef");
        assert_eq!(dst, [c(b'a'), c(b'b'), c(b'c'), 0]);
        // Exact fit: N - 1 bytes of text, then the NUL.
        let mut d2 = [c(0x7f); 4];
        fill_fixed(&mut d2, "abc");
        assert_eq!(d2, [c(b'a'), c(b'b'), c(b'c'), 0]);
    }
}
