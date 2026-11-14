// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Offline tests of the Bluetooth PTT C ABI (`src/ble_ptt.rs`), exercised
//! through the `rlib` crate-type by calling the `extern "C"` functions
//! directly. Pure: no Bluetooth, no radio, no network. The machine is fed
//! synthetic events with an injected clock, exactly as the macOS transport
//! feeds it, so nothing here can key anything.

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

use astar_ble_ptt::{
    AdapterState, ButtonMachine, DEVICES, Intent, LinkEvent, LinkStatus, MachineConfig,
    NotifyBytes, Output, ReleaseReason, SignalBand,
};
use astar_sys::*;
use serde_json::Value;

const B02PTT_SERVICE: &str = "89A8591D-BB19-485B-9F59-58492BC33E24";
const B02PTT_CHARACTERISTIC: &str = "894C8042-E841-461C-A5C9-5A73D25DB08E";

/// A device row pre-filled with a non-NUL byte, so a field the ABI forgets to
/// terminate fails `text()` instead of reading as an empty string.
fn filled_device() -> IaxBleDevice {
    IaxBleDevice {
        id: [0x7f; 32],
        display_name: [0x7f; 64],
        name_prefix: [0x7f; 32],
        service: [0x7f; 37],
        characteristic: [0x7f; 37],
        kind: u32::MAX,
        liveness: u32::MAX,
        verified: [0x7f; 128],
    }
}

/// The NUL-terminated UTF-8 text in a fixed `c_char` array. Panics when the
/// array holds no NUL, so an unterminated field is a test failure.
fn text(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field.iter().map(|c| c.to_ne_bytes()[0]).collect();
    CStr::from_bytes_until_nul(&bytes)
        .expect("field must be NUL-terminated")
        .to_str()
        .expect("field must be UTF-8")
        .to_owned()
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn accepts(index: usize, service: &CStr, characteristic: &CStr, properties: u8) -> bool {
    unsafe { iax_bleptt_accepts_gatt(index, service.as_ptr(), characteristic.as_ptr(), properties) }
}

#[test]
fn device_count_is_one_and_matches_the_rust_table() {
    assert_eq!(iax_bleptt_device_count(), 1);
    assert_eq!(iax_bleptt_device_count(), DEVICES.len());
}

#[test]
fn device_row_fields_match_the_rust_table() {
    let mut row = filled_device();
    let rc = unsafe { iax_bleptt_device_at(0, ptr::from_mut(&mut row)) };
    assert_eq!(rc, IAX_OK);
    let rust = &DEVICES[0];
    assert_eq!(text(&row.id), "inrico-b02ptt");
    assert_eq!(text(&row.id), rust.id);
    assert_eq!(text(&row.display_name), "Inrico B02PTT");
    assert_eq!(text(&row.display_name), rust.display_name);
    assert_eq!(text(&row.name_prefix), "B02PTT");
    assert_eq!(text(&row.name_prefix), rust.name_prefix);
    assert_eq!(text(&row.service), B02PTT_SERVICE);
    assert_eq!(text(&row.service), rust.service.to_string());
    assert_eq!(text(&row.characteristic), B02PTT_CHARACTERISTIC);
    assert_eq!(text(&row.characteristic), rust.characteristic.to_string());
    // 36 characters, then the terminator in the last of the 37 slots.
    assert_eq!(row.service[36], 0);
    assert_eq!(row.characteristic[36], 0);
    assert_eq!(row.kind, IAX_BLE_KIND_SPEAKER_MIC);
    assert_eq!(row.liveness, IAX_BLE_LIVENESS_RSSI);
    assert_eq!(text(&row.verified), rust.verified);
}

#[test]
fn device_at_out_of_range_is_invalid_and_leaves_out_untouched() {
    let mut row = filled_device();
    let before = row;
    for index in [1, usize::MAX] {
        let rc = unsafe { iax_bleptt_device_at(index, ptr::from_mut(&mut row)) };
        assert_eq!(rc, IAX_ERR_INVALID, "index {index}");
        assert_eq!(row, before, "index {index}");
    }
}

#[test]
fn device_at_null_out_is_a_null_error() {
    assert_eq!(
        unsafe { iax_bleptt_device_at(0, ptr::null_mut()) },
        IAX_ERR_NULL
    );
}

#[test]
fn match_name_finds_the_b02ptt_by_case_sensitive_prefix() {
    let hit = cstring("B02PTT-6522");
    let lower = cstring("b02ptt-1");
    assert_eq!(unsafe { iax_bleptt_match_name(hit.as_ptr()) }, 0);
    assert_eq!(unsafe { iax_bleptt_match_name(lower.as_ptr()) }, -1);
    assert_eq!(unsafe { iax_bleptt_match_name(ptr::null()) }, -1);
}

#[test]
fn match_name_rejects_non_utf8_instead_of_matching_lossily() {
    // A lossy conversion reads this as "B02PTT-\u{FFFD}", which would match.
    // A C string literal may carry the raw 0xFF byte; it adds the NUL itself.
    let name = c"B02PTT-\xff";
    assert_eq!(unsafe { iax_bleptt_match_name(name.as_ptr()) }, -1);
}

#[test]
fn accepts_gatt_only_with_notify_on_the_right_uuids() {
    let svc = cstring(B02PTT_SERVICE);
    let chr = cstring(B02PTT_CHARACTERISTIC);
    let malformed = cstring("not-a-uuid");
    assert!(accepts(0, &svc, &chr, 0x10)); // NOTIFY
    assert!(accepts(0, &svc, &chr, 0x12)); // READ | NOTIFY
    assert!(!accepts(0, &svc, &chr, 0x02)); // READ only
    assert!(!accepts(0, &svc, &chr, 0x00));
    assert!(!accepts(0, &chr, &svc, 0x10)); // service and characteristic swapped
    assert!(!accepts(1, &svc, &chr, 0x10)); // no such row
    assert!(!accepts(0, &malformed, &chr, 0x10));
    assert!(!accepts(0, &svc, &malformed, 0x10));
}

#[test]
fn accepts_gatt_takes_lowercase_uuids() {
    let svc = cstring(&B02PTT_SERVICE.to_lowercase());
    let chr = cstring(&B02PTT_CHARACTERISTIC.to_lowercase());
    assert!(accepts(0, &svc, &chr, 0x10));
}

#[test]
fn accepts_gatt_null_strings_are_false() {
    let svc = cstring(B02PTT_SERVICE);
    let chr = cstring(B02PTT_CHARACTERISTIC);
    assert!(!unsafe { iax_bleptt_accepts_gatt(0, ptr::null(), chr.as_ptr(), 0x10) });
    assert!(!unsafe { iax_bleptt_accepts_gatt(0, svc.as_ptr(), ptr::null(), 0x10) });
}

#[test]
fn default_config_equals_machine_config_default() {
    let mut cfg = IaxBleConfig {
        settle_ms: u32::MAX,
        probe_keyed_ms: u32::MAX,
        probe_misses_dead: u32::MAX,
        probe_idle_ms: u32::MAX,
        lease_ttl_ms: u32::MAX,
        renew_ms: u32::MAX,
        band_strong_dbm: i16::MAX,
        band_fair_dbm: i16::MAX,
    };
    assert_eq!(
        unsafe { iax_bleptt_default_config(ptr::from_mut(&mut cfg)) },
        IAX_OK
    );
    assert_eq!(
        cfg,
        IaxBleConfig {
            settle_ms: 300,
            probe_keyed_ms: 500,
            probe_misses_dead: 2,
            probe_idle_ms: 5000,
            lease_ttl_ms: 1500,
            renew_ms: 250,
            band_strong_dbm: -65,
            band_fair_dbm: -80,
        }
    );
    let d = MachineConfig::DEFAULT;
    let ms = |x: std::time::Duration| u32::try_from(x.as_millis()).unwrap();
    assert_eq!(
        cfg,
        IaxBleConfig {
            settle_ms: ms(d.settle_after_subscribe),
            probe_keyed_ms: ms(d.probe_interval_keyed),
            probe_misses_dead: u32::from(d.probe_misses_dead),
            probe_idle_ms: ms(d.probe_interval_idle),
            lease_ttl_ms: ms(d.lease_ttl),
            renew_ms: ms(d.renew_period),
            band_strong_dbm: d.band_strong_dbm,
            band_fair_dbm: d.band_fair_dbm,
        }
    );
}

#[test]
fn default_config_null_out_is_a_null_error() {
    assert_eq!(
        unsafe { iax_bleptt_default_config(ptr::null_mut()) },
        IAX_ERR_NULL
    );
}

// --- The machine handle -----------------------------------------------------

/// Frees the handle however the test exits, so a failing assert never leaks.
struct Machine(*mut IaxBleMachine);

impl Drop for Machine {
    fn drop(&mut self) {
        unsafe { iax_bleptt_free(self.0) };
    }
}

fn machine() -> Machine {
    let h = iax_bleptt_new(0);
    assert!(!h.is_null(), "row 0 must build a machine");
    Machine(h)
}

/// Output pre-filled with values the ABI never writes, so a field it forgets
/// to fill shows up as a mismatch rather than a lucky zero.
fn sentinel_output() -> IaxBleOutput {
    IaxBleOutput {
        intent: 0xDEAD,
        release_reason: 0xDEAD,
        cmd_probe: true,
        cmd_drop_link: true,
        status: 0xDEAD,
        signal: 0xDEAD,
        has_rssi: true,
        last_rssi: i16::MIN,
        raw_pressed: true,
        pressed: true,
    }
}

fn event(kind: u32) -> IaxBleEvent {
    IaxBleEvent {
        kind,
        epoch: 0,
        adapter: 0,
        confirmed: false,
        rssi: 0,
        payload: ptr::null(),
        payload_len: 0,
    }
}

fn at_epoch(kind: u32, epoch: u32) -> IaxBleEvent {
    IaxBleEvent {
        epoch,
        ..event(kind)
    }
}

/// A notify whose pointer borrows `payload`; the caller keeps it alive.
fn notify(epoch: u32, payload: &[u8]) -> IaxBleEvent {
    IaxBleEvent {
        epoch,
        payload: payload.as_ptr(),
        payload_len: payload.len(),
        ..event(IAX_BLE_EV_NOTIFY)
    }
}

/// Feed one event and insist the ABI accepted it, so a step that silently
/// returned an error cannot pass on a stale output.
fn feed(h: *mut IaxBleMachine, ev: &IaxBleEvent, now_ms: u64) -> IaxBleOutput {
    let mut out = sentinel_output();
    let rc = unsafe { iax_bleptt_event(h, ptr::from_ref(ev), now_ms, ptr::from_mut(&mut out)) };
    assert_eq!(rc, IAX_OK, "event kind {} at {now_ms} ms", ev.kind);
    out
}

/// Feed one event the ABI must refuse, and return its code and the out-struct.
fn feed_refused(h: *mut IaxBleMachine, ev: &IaxBleEvent) -> (i32, IaxBleOutput) {
    let mut out = sentinel_output();
    let rc = unsafe { iax_bleptt_event(h, ptr::from_ref(ev), 5, ptr::from_mut(&mut out)) };
    (rc, out)
}

fn status(h: *mut IaxBleMachine) -> IaxBleOutput {
    let mut out = sentinel_output();
    assert_eq!(
        unsafe { iax_bleptt_status(h, ptr::from_mut(&mut out)) },
        IAX_OK
    );
    out
}

/// Enable (with `confirmed`), adapter on, then connect, accept GATT and
/// subscribe on epoch 1: the first five steps of the `disconnect_mid_press`
/// scenario. Returns the output of the subscribe.
fn bring_up(h: *mut IaxBleMachine, confirmed: bool) -> IaxBleOutput {
    let enable = IaxBleEvent {
        confirmed,
        ..event(IAX_BLE_EV_ENABLE)
    };
    assert_eq!(feed(h, &enable, 0).status, LinkStatus::Searching as u32);
    let on = IaxBleEvent {
        adapter: IAX_BLE_ADAPTER_ON,
        ..event(IAX_BLE_EV_ADAPTER)
    };
    assert_eq!(feed(h, &on, 1).status, LinkStatus::Searching as u32);
    let connected = feed(h, &at_epoch(IAX_BLE_EV_CONNECTED, 1), 10);
    assert_eq!(connected.status, LinkStatus::Connecting as u32);
    let accepted = feed(h, &at_epoch(IAX_BLE_EV_GATT_ACCEPTED, 1), 20);
    assert_eq!(accepted.status, LinkStatus::Connecting as u32);
    feed(h, &at_epoch(IAX_BLE_EV_SUBSCRIBED, 1), 30)
}

/// `bring_up` confirmed, then a `[01]` notify at 2000 ms (well past the 300 ms
/// settle window), asserting the press.
fn pressed_machine() -> Machine {
    let m = machine();
    bring_up(m.0, true);
    assert!(feed(m.0, &notify(1, &[0x01]), 2000).pressed);
    m
}

#[test]
fn new_rejects_a_row_that_does_not_exist() {
    assert!(iax_bleptt_new(1).is_null());
    assert!(iax_bleptt_new(usize::MAX).is_null());
}

#[test]
fn free_of_null_is_a_no_op() {
    unsafe { iax_bleptt_free(ptr::null_mut()) };
}

#[test]
fn new_machine_reports_disabled_and_unpressed() {
    let m = machine();
    assert_eq!(
        status(m.0),
        IaxBleOutput {
            intent: IAX_BLE_INTENT_NONE,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::Disabled as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: false,
            pressed: false,
        }
    );
}

#[test]
fn machine_round_trip_press_then_link_lost_release() {
    let m = machine();
    let up = bring_up(m.0, true);
    assert_eq!(up.status, LinkStatus::Ready as u32);
    assert_eq!(up.intent, IAX_BLE_INTENT_NONE);

    let press = feed(m.0, &notify(1, &[0x01]), 2000);
    assert_eq!(
        press,
        IaxBleOutput {
            intent: IAX_BLE_INTENT_PRESS,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::Ready as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: true,
            pressed: true,
        }
    );

    let held = status(m.0);
    assert_eq!(
        held.intent, IAX_BLE_INTENT_NONE,
        "status never repeats an intent"
    );
    assert!(held.pressed);

    let lost = feed(m.0, &at_epoch(IAX_BLE_EV_DISCONNECTED, 1), 2100);
    assert_eq!(lost.intent, IAX_BLE_INTENT_RELEASE);
    assert_eq!(lost.release_reason, ReleaseReason::LinkLost as u32);
    assert_eq!(lost.release_reason, 3);
    assert_eq!(lost.status, LinkStatus::Reconnecting as u32);
    assert!(!lost.pressed);
    assert!(!lost.cmd_probe);
    assert!(!lost.cmd_drop_link);
}

#[test]
fn idle_relink_reads_ready_and_the_first_press_keys() {
    // The conditional latch: a drop with the button up leaves
    // nothing to latch, so the relink reads Ready, not ReleaseToArm, and the
    // first press past the settle window keys. A drop while held is
    // machine_round_trip_press_then_link_lost_release's case.
    let m = machine();
    let mut pure = pure_machine();
    assert_eq!(bring_up(m.0, true).status, LinkStatus::Ready as u32);
    for (ev, ms) in [
        (LinkEvent::Enable { confirmed: true }, 0),
        (LinkEvent::Adapter(AdapterState::On), 1),
        (LinkEvent::Connected { epoch: 1 }, 10),
        (LinkEvent::GattAccepted { epoch: 1 }, 20),
        (LinkEvent::Subscribed { epoch: 1 }, 30),
    ] {
        // `handle` is #[must_use]; bring_up already checked the ABI side of
        // these five steps, so the pure twin's outputs are not needed here.
        let _ = pure.handle(ev, at(ms));
    }

    let lost = feed(m.0, &at_epoch(IAX_BLE_EV_DISCONNECTED, 1), 1000);
    assert_eq!(
        lost,
        wire_of(pure.handle(LinkEvent::Disconnected { epoch: 1 }, at(1000))),
        "idle drop: the ABI says what the pure machine says"
    );
    assert_eq!(
        lost.intent, IAX_BLE_INTENT_NONE,
        "nothing was held, so nothing is released"
    );
    assert_eq!(lost.release_reason, 0);
    assert_eq!(lost.status, LinkStatus::Reconnecting as u32);
    assert!(!lost.pressed);

    let connected = feed(m.0, &at_epoch(IAX_BLE_EV_CONNECTED, 2), 1500);
    assert_eq!(
        connected,
        wire_of(pure.handle(LinkEvent::Connected { epoch: 2 }, at(1500))),
        "reconnect"
    );
    assert_eq!(connected.status, LinkStatus::Connecting as u32);
    let accepted = feed(m.0, &at_epoch(IAX_BLE_EV_GATT_ACCEPTED, 2), 1510);
    assert_eq!(
        accepted,
        wire_of(pure.handle(LinkEvent::GattAccepted { epoch: 2 }, at(1510))),
        "regatt"
    );
    assert_eq!(accepted.status, LinkStatus::Connecting as u32);

    let relinked = feed(m.0, &at_epoch(IAX_BLE_EV_SUBSCRIBED, 2), 1520);
    assert_eq!(
        relinked,
        wire_of(pure.handle(LinkEvent::Subscribed { epoch: 2 }, at(1520))),
        "relink: the ABI says what the pure machine says"
    );
    assert_eq!(
        relinked,
        IaxBleOutput {
            intent: IAX_BLE_INTENT_NONE,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::Ready as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: false,
            pressed: false,
        },
        "an idle relink reads Ready and keys nothing by itself"
    );

    let press = feed(m.0, &notify(2, &[0x01]), 2000);
    assert_eq!(
        press.intent, IAX_BLE_INTENT_PRESS,
        "the first press past the settle window keys"
    );
    assert_eq!(press.status, LinkStatus::Ready as u32);
    assert!(press.pressed);
}

#[test]
fn lease_lost_with_the_button_up_stays_ready_through_the_abi() {
    // A lost lease while the button is up latches nothing (conditional latch):
    // no intent, still Ready, and the next press keys.
    let m = machine();
    assert_eq!(bring_up(m.0, true).status, LinkStatus::Ready as u32);
    let out = feed(m.0, &event(IAX_BLE_EV_LEASE_LOST), 500);
    assert_eq!(
        out,
        IaxBleOutput {
            intent: IAX_BLE_INTENT_NONE,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::Ready as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: false,
            pressed: false,
        },
        "lease lost with the button up"
    );
    assert_eq!(
        feed(m.0, &notify(1, &[0x01]), 2000).intent,
        IAX_BLE_INTENT_PRESS,
        "the next press keys"
    );
}

#[test]
fn unconfirmed_enable_never_presses_through_the_abi() {
    let m = machine();
    let up = bring_up(m.0, false);
    assert_eq!(up.status, LinkStatus::NeedsConfirm as u32);
    // The whole struct: raw_pressed true with pressed false is exactly what
    // the client's confirm-press flow reads to confirm an unconfirmed unit.
    assert_eq!(
        feed(m.0, &notify(1, &[0x01]), 2000),
        IaxBleOutput {
            intent: IAX_BLE_INTENT_NONE,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::NeedsConfirm as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: true,
            pressed: false,
        }
    );
}

#[test]
fn confirmed_event_arms_an_unconfirmed_unit_through_the_abi() {
    let m = machine();
    assert_eq!(bring_up(m.0, false).status, LinkStatus::NeedsConfirm as u32);
    let confirmed = feed(m.0, &event(IAX_BLE_EV_CONFIRMED), 40);
    assert_eq!(confirmed.status, LinkStatus::Ready as u32);
    assert_eq!(confirmed.intent, IAX_BLE_INTENT_NONE);
    assert!(!confirmed.pressed);
    assert_eq!(
        feed(m.0, &notify(1, &[0x01]), 2000).intent,
        IAX_BLE_INTENT_PRESS
    );
}

#[test]
fn wake_never_keys_or_releases_through_the_abi() {
    let m = pressed_machine();
    assert_eq!(
        feed(m.0, &event(IAX_BLE_EV_WAKE), 2100),
        IaxBleOutput {
            intent: IAX_BLE_INTENT_NONE,
            release_reason: 0,
            cmd_probe: false,
            cmd_drop_link: false,
            status: LinkStatus::Ready as u32,
            signal: 0,
            has_rssi: false,
            last_rssi: 0,
            raw_pressed: true,
            pressed: true,
        }
    );
}

// --- The pure twin -----------------------------------------------------------
//
// The ABI contract pins the intent, the release reason, the adapter and
// Disable statuses, and `pressed == false` after a release. It does not pin
// whether Sleep or a liveness release takes the link down (Reconnecting versus
// ReleaseToArm) or what `raw_pressed` reads after a forced release; those are
// the astar-ble-ptt machine's own rules. So the release tests below run a pure `ButtonMachine`
// beside the handle on the same events at the same times and insist the ABI
// says exactly what the pure machine says, then hard-code only the pinned
// fields.

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn pure_machine() -> ButtonMachine {
    ButtonMachine::new(&DEVICES[0], MachineConfig::DEFAULT)
}

/// The pure twin of `pressed_machine`: the same six events at the same times.
fn pure_pressed() -> ButtonMachine {
    let mut p = pure_machine();
    let steps = [
        (LinkEvent::Enable { confirmed: true }, 0),
        (LinkEvent::Adapter(AdapterState::On), 1),
        (LinkEvent::Connected { epoch: 1 }, 10),
        (LinkEvent::GattAccepted { epoch: 1 }, 20),
        (LinkEvent::Subscribed { epoch: 1 }, 30),
        (
            LinkEvent::Notify {
                epoch: 1,
                bytes: NotifyBytes::from_slice(&[0x01]),
            },
            2000,
        ),
    ];
    for (ev, ms) in steps {
        // `handle` is #[must_use]; only the final state matters here.
        let _ = p.handle(ev, at(ms));
    }
    assert!(p.output().pressed, "the pure twin must hold the key too");
    p
}

/// The pure crate's `Output`, written onto the wire by this test's own hand.
/// It is deliberately not `output_to_ffi`, so a wrong field there disagrees
/// with this copy instead of agreeing with itself.
fn wire_of(o: Output) -> IaxBleOutput {
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

/// The fields of a release the ABI contract pins whatever the machine does
/// with the link: RELEASE on the wire with this reason number, nothing held.
fn assert_pinned_release(out: &IaxBleOutput, reason: u32, label: &str) {
    assert_eq!(out.intent, IAX_BLE_INTENT_RELEASE, "{label}: intent");
    assert_eq!(out.release_reason, reason, "{label}: release_reason");
    assert!(!out.pressed, "{label}: pressed");
}

#[test]
fn every_release_path_crosses_the_abi() {
    // Swift feeds these numeric kinds directly, so a swapped arm in
    // link_event_from_ffi or adapter_from_ffi would leave a key held on
    // disable, sleep, unsubscribe or Bluetooth going away.
    let adapter = |state: u32| IaxBleEvent {
        adapter: state,
        ..event(IAX_BLE_EV_ADAPTER)
    };
    // (label, wire event, the same event for the pure twin, reason number,
    // the status the contract pins, if it pins one)
    let cases = [
        (
            "disable",
            event(IAX_BLE_EV_DISABLE),
            LinkEvent::Disable,
            5,
            Some(LinkStatus::Disabled),
        ),
        ("sleep", event(IAX_BLE_EV_SLEEP), LinkEvent::Sleep, 6, None),
        (
            "unsubscribed",
            at_epoch(IAX_BLE_EV_UNSUBSCRIBED, 1),
            LinkEvent::Unsubscribed { epoch: 1 },
            3,
            None,
        ),
        (
            "adapter off",
            adapter(IAX_BLE_ADAPTER_OFF),
            LinkEvent::Adapter(AdapterState::Off),
            4,
            Some(LinkStatus::BluetoothOff),
        ),
        (
            "adapter resetting",
            adapter(IAX_BLE_ADAPTER_RESETTING),
            LinkEvent::Adapter(AdapterState::Resetting),
            4,
            Some(LinkStatus::BluetoothOff),
        ),
        (
            "adapter unauthorized",
            adapter(IAX_BLE_ADAPTER_UNAUTHORIZED),
            LinkEvent::Adapter(AdapterState::Unauthorized),
            4,
            Some(LinkStatus::Unauthorized),
        ),
        (
            "adapter unsupported",
            adapter(IAX_BLE_ADAPTER_UNSUPPORTED),
            LinkEvent::Adapter(AdapterState::Unsupported),
            4,
            Some(LinkStatus::Unsupported),
        ),
    ];
    for (label, ev, pure_ev, reason, pinned_status) in cases {
        let m = pressed_machine();
        let mut pure = pure_pressed();
        let out = feed(m.0, &ev, 2100);
        assert_eq!(
            out,
            wire_of(pure.handle(pure_ev, at(2100))),
            "{label}: the ABI says what the pure machine says"
        );
        assert_pinned_release(&out, reason, label);
        if let Some(want) = pinned_status {
            assert_eq!(out.status, want as u32, "{label}: status");
        }
        let after = status(m.0);
        assert_eq!(after, wire_of(pure.output()), "{label}: the release sticks");
        assert_eq!(
            after.intent, IAX_BLE_INTENT_NONE,
            "{label}: status never repeats an intent"
        );
        assert!(!after.pressed, "{label}: still released");
    }

    // A lost lease never repeats a release (the station already dropped the
    // source), but the machine stops holding. The button was down here, so
    // the conditional latch holds it until a release (link untouched, so
    // ReleaseToArm). The button-up case is
    // lease_lost_with_the_button_up_stays_ready_through_the_abi.
    let m = pressed_machine();
    let mut pure = pure_pressed();
    let out = feed(m.0, &event(IAX_BLE_EV_LEASE_LOST), 2100);
    assert_eq!(
        out,
        wire_of(pure.handle(LinkEvent::LeaseLost, at(2100))),
        "lease lost: the ABI says what the pure machine says"
    );
    assert_eq!(out.intent, IAX_BLE_INTENT_NONE, "lease lost: intent");
    assert_eq!(out.release_reason, 0, "lease lost: release_reason");
    assert!(!out.pressed, "lease lost: pressed");
    assert_eq!(
        out.status,
        LinkStatus::ReleaseToArm as u32,
        "lease lost: status"
    );
}

#[test]
fn two_missed_probes_release_and_drop_the_link() {
    // The ABI half of astar-ble-ptt's rssi_two_misses_release, with the misses
    // reported as failed RSSI reads rather than as unanswered probes. The
    // ticks sit 600 ms after the press and 600 ms after the first probe, so
    // neither lands on the 500 ms keyed-probe boundary.
    let m = pressed_machine();
    let mut pure = pure_pressed();

    let tick = feed(m.0, &event(IAX_BLE_EV_TICK), 2600);
    assert_eq!(
        tick,
        wire_of(pure.handle(LinkEvent::Tick, at(2600))),
        "first tick"
    );
    assert!(tick.cmd_probe, "first tick probes the held key");

    let first = feed(m.0, &at_epoch(IAX_BLE_EV_PROBE_FAILED, 1), 2650);
    assert_eq!(
        first,
        wire_of(pure.handle(LinkEvent::ProbeFailed { epoch: 1 }, at(2650))),
        "first miss"
    );
    assert_eq!(first.intent, IAX_BLE_INTENT_NONE, "one miss is not dead");
    assert!(first.pressed);

    let tick = feed(m.0, &event(IAX_BLE_EV_TICK), 3200);
    assert_eq!(
        tick,
        wire_of(pure.handle(LinkEvent::Tick, at(3200))),
        "second tick"
    );
    assert!(tick.cmd_probe, "second tick probes the held key");

    let second = feed(m.0, &at_epoch(IAX_BLE_EV_PROBE_FAILED, 1), 3250);
    assert_eq!(
        second,
        wire_of(pure.handle(LinkEvent::ProbeFailed { epoch: 1 }, at(3250))),
        "second miss"
    );
    assert_pinned_release(&second, 3, "second miss");
    assert!(
        second.cmd_drop_link,
        "second miss: the dead link is dropped"
    );
    assert!(!second.cmd_probe, "second miss: no probe on a dropped link");
}

#[test]
fn keyed_probe_command_and_rssi_reply_cross_the_abi() {
    let m = pressed_machine();
    let tick = feed(m.0, &event(IAX_BLE_EV_TICK), 2600);
    assert!(tick.cmd_probe, "a held key is probed every 500 ms");
    assert!(!tick.cmd_drop_link);
    let reply = IaxBleEvent {
        epoch: 1,
        rssi: -70,
        ..event(IAX_BLE_EV_PROBE_OK)
    };
    let out = feed(m.0, &reply, 2650);
    assert!(out.has_rssi);
    assert_eq!(out.last_rssi, -70);
    assert_eq!(out.signal, SignalBand::Fair as u32);
    assert_eq!(out.signal, 2);
    assert!(out.pressed);
}

#[test]
fn nine_byte_payload_decodes_malformed_and_releases() {
    let m = pressed_machine();
    let long = [0x01, 0, 0, 0, 0, 0, 0, 0, 0];
    let out = feed(m.0, &notify(1, &long), 2100);
    assert_eq!(out.intent, IAX_BLE_INTENT_RELEASE);
    assert_eq!(out.release_reason, ReleaseReason::Malformed as u32);
    assert_eq!(out.release_reason, 2);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm as u32);
}

#[test]
fn payload_len_bounds_the_read() {
    // The byte after `len` is 0xFF. The machine decodes only the first `len`
    // bytes, so a 1-byte [01] in a longer buffer is a press through the ABI.
    // That the ABI copies no more than `len` bytes is pinned by the unit test
    // `notify_reads_no_more_than_len` in src/ble_ptt.rs, not here.
    let m = machine();
    bring_up(m.0, true);
    let buf = [0x01_u8, 0xFF];
    let ev = IaxBleEvent {
        payload_len: 1,
        ..notify(1, &buf)
    };
    let out = feed(m.0, &ev, 2000);
    assert_eq!(out.intent, IAX_BLE_INTENT_PRESS);
    assert!(out.pressed);
}

#[test]
fn null_payload_with_zero_length_is_an_empty_notify() {
    let m = pressed_machine();
    let empty = at_epoch(IAX_BLE_EV_NOTIFY, 1);
    let out = feed(m.0, &empty, 2100);
    assert_eq!(out.intent, IAX_BLE_INTENT_RELEASE);
    assert_eq!(out.release_reason, ReleaseReason::Malformed as u32);
    assert!(!out.pressed);
}

#[test]
fn null_payload_with_a_length_is_invalid_and_changes_nothing() {
    let m = pressed_machine();
    let bad = IaxBleEvent {
        payload_len: 1,
        ..at_epoch(IAX_BLE_EV_NOTIFY, 1)
    };
    let (rc, out) = feed_refused(m.0, &bad);
    assert_eq!(rc, IAX_ERR_INVALID);
    assert_eq!(out, sentinel_output());
    assert!(
        status(m.0).pressed,
        "a refused event never reaches the machine"
    );
}

#[test]
fn unknown_event_kind_is_invalid_and_changes_nothing() {
    let m = machine();
    let before = status(m.0);
    for kind in [0, 18, u32::MAX] {
        let (rc, out) = feed_refused(m.0, &event(kind));
        assert_eq!(rc, IAX_ERR_INVALID, "kind {kind}");
        assert_eq!(out, sentinel_output(), "kind {kind}");
    }
    assert_eq!(status(m.0), before);
}

#[test]
fn unknown_adapter_state_is_invalid_and_changes_nothing() {
    let m = machine();
    let enable = IaxBleEvent {
        confirmed: true,
        ..event(IAX_BLE_EV_ENABLE)
    };
    feed(m.0, &enable, 0);
    let before = status(m.0);
    let bad = IaxBleEvent {
        adapter: 5,
        ..event(IAX_BLE_EV_ADAPTER)
    };
    let (rc, out) = feed_refused(m.0, &bad);
    assert_eq!(rc, IAX_ERR_INVALID);
    assert_eq!(out, sentinel_output());
    assert_eq!(status(m.0), before);
}

#[test]
fn null_handle_event_and_out_are_null_errors() {
    let m = machine();
    let ev = event(IAX_BLE_EV_TICK);
    let mut out = sentinel_output();
    let out_ptr = ptr::from_mut(&mut out);
    unsafe {
        assert_eq!(
            iax_bleptt_event(ptr::null_mut(), ptr::from_ref(&ev), 0, out_ptr),
            IAX_ERR_NULL
        );
        assert_eq!(iax_bleptt_event(m.0, ptr::null(), 0, out_ptr), IAX_ERR_NULL);
        assert_eq!(
            iax_bleptt_event(m.0, ptr::from_ref(&ev), 0, ptr::null_mut()),
            IAX_ERR_NULL
        );
        assert_eq!(iax_bleptt_status(ptr::null_mut(), out_ptr), IAX_ERR_NULL);
        assert_eq!(iax_bleptt_status(m.0, ptr::null_mut()), IAX_ERR_NULL);
    }
    assert_eq!(out, sentinel_output());
}

// --- Scenario replay through the ABI ----------------------------------------

/// The traces astar-ble-ptt's own tests replay. Reading the same files here means
/// a swapped arm in the event, adapter, status, reason or signal mapping
/// fails on a trace the pure crate already passes.
const SCENARIOS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../astar-ble-ptt/scenarios");

fn wire_kind(kind: &str) -> u32 {
    match kind {
        "enable" => IAX_BLE_EV_ENABLE,
        "disable" => IAX_BLE_EV_DISABLE,
        "confirmed" => IAX_BLE_EV_CONFIRMED,
        "adapter" => IAX_BLE_EV_ADAPTER,
        "connected" => IAX_BLE_EV_CONNECTED,
        "gatt_accepted" => IAX_BLE_EV_GATT_ACCEPTED,
        "gatt_rejected" => IAX_BLE_EV_GATT_REJECTED,
        "subscribed" => IAX_BLE_EV_SUBSCRIBED,
        "unsubscribed" => IAX_BLE_EV_UNSUBSCRIBED,
        "notify" => IAX_BLE_EV_NOTIFY,
        "probe_ok" => IAX_BLE_EV_PROBE_OK,
        "probe_failed" => IAX_BLE_EV_PROBE_FAILED,
        "disconnected" => IAX_BLE_EV_DISCONNECTED,
        "sleep" => IAX_BLE_EV_SLEEP,
        "wake" => IAX_BLE_EV_WAKE,
        "lease_lost" => IAX_BLE_EV_LEASE_LOST,
        "tick" => IAX_BLE_EV_TICK,
        other => panic!("scenario event kind {other:?} is not in the schema"),
    }
}

fn wire_adapter(state: &str) -> u32 {
    match state {
        "on" => IAX_BLE_ADAPTER_ON,
        "off" => IAX_BLE_ADAPTER_OFF,
        "unauthorized" => IAX_BLE_ADAPTER_UNAUTHORIZED,
        "resetting" => IAX_BLE_ADAPTER_RESETTING,
        "unsupported" => IAX_BLE_ADAPTER_UNSUPPORTED,
        other => panic!("scenario adapter state {other:?} is not in the schema"),
    }
}

fn wire_intent(intent: &str) -> u32 {
    match intent {
        "none" => IAX_BLE_INTENT_NONE,
        "press" => IAX_BLE_INTENT_PRESS,
        "release" => IAX_BLE_INTENT_RELEASE,
        other => panic!("scenario intent {other:?} is not in the schema"),
    }
}

/// 0 when the step names no reason, which the schema allows only off RELEASE.
fn wire_reason(reason: Option<&str>) -> u32 {
    let reason = match reason {
        None => return 0,
        Some("button") => ReleaseReason::Button,
        Some("malformed") => ReleaseReason::Malformed,
        Some("link_lost") => ReleaseReason::LinkLost,
        Some("adapter_off") => ReleaseReason::AdapterOff,
        Some("disabled") => ReleaseReason::Disabled,
        Some("sleep") => ReleaseReason::Sleep,
        Some(other) => panic!("scenario reason {other:?} is not in the schema"),
    };
    reason as u32
}

fn wire_status(status: &str) -> u32 {
    let status = match status {
        "disabled" => LinkStatus::Disabled,
        "bluetooth_off" => LinkStatus::BluetoothOff,
        "unauthorized" => LinkStatus::Unauthorized,
        "unsupported" => LinkStatus::Unsupported,
        "searching" => LinkStatus::Searching,
        "connecting" => LinkStatus::Connecting,
        "not_supported_device" => LinkStatus::NotSupportedDevice,
        "needs_confirm" => LinkStatus::NeedsConfirm,
        "ready" => LinkStatus::Ready,
        "release_to_arm" => LinkStatus::ReleaseToArm,
        "reconnecting" => LinkStatus::Reconnecting,
        other => panic!("scenario status {other:?} is not in the schema"),
    };
    status as u32
}

fn wire_signal(signal: &Value) -> u32 {
    let band = match signal.as_str() {
        None if signal.is_null() => return 0,
        Some("strong") => SignalBand::Strong,
        Some("fair") => SignalBand::Fair,
        Some("weak") => SignalBand::Weak,
        _ => panic!("scenario signal {signal} is not in the schema"),
    };
    band as u32
}

fn json_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string in {v}"))
}

fn json_u32(v: &Value, key: &str) -> u32 {
    let raw = v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} must be an integer in {v}"));
    u32::try_from(raw).unwrap()
}

/// The notify bytes of a scenario event (empty for every other kind).
fn scenario_payload(ev: &Value) -> Vec<u8> {
    ev.get("bytes").map_or_else(Vec::new, |bytes| {
        bytes
            .as_array()
            .expect("bytes must be an array")
            .iter()
            .map(|b| u8::try_from(b.as_u64().expect("byte must be an integer")).unwrap())
            .collect()
    })
}

/// Translate one scenario event to the wire. `payload` must outlive the call.
fn scenario_event(ev: &Value, payload: &[u8]) -> IaxBleEvent {
    let kind = wire_kind(json_str(ev, "kind"));
    IaxBleEvent {
        kind,
        epoch: if ev.get("epoch").is_some() {
            json_u32(ev, "epoch")
        } else {
            0
        },
        adapter: if kind == IAX_BLE_EV_ADAPTER {
            wire_adapter(json_str(ev, "state"))
        } else {
            0
        },
        confirmed: kind == IAX_BLE_EV_ENABLE
            && ev["confirmed"].as_bool().expect("confirmed must be a bool"),
        rssi: ev.get("rssi").map_or(0, |r| {
            i16::try_from(r.as_i64().expect("rssi must be an integer")).unwrap()
        }),
        payload: payload.as_ptr(),
        payload_len: payload.len(),
    }
}

/// Assert one step's output against its `expect`, exactly. Optional keys the
/// step leaves out are not checked, as the schema says.
fn check_step(out: &IaxBleOutput, ex: &Value, ctx: &str) {
    assert_eq!(
        out.intent,
        wire_intent(json_str(ex, "intent")),
        "{ctx}: intent"
    );
    assert_eq!(
        out.release_reason,
        wire_reason(ex.get("reason").and_then(Value::as_str)),
        "{ctx}: reason"
    );
    assert_eq!(
        out.status,
        wire_status(json_str(ex, "status")),
        "{ctx}: status"
    );
    let mut want_cmds: Vec<&str> = ex["cmds"]
        .as_array()
        .expect("cmds must be an array")
        .iter()
        .map(|c| c.as_str().expect("cmd must be a string"))
        .collect();
    want_cmds.sort_unstable();
    let mut got_cmds = Vec::new();
    if out.cmd_drop_link {
        got_cmds.push("drop_link");
    }
    if out.cmd_probe {
        got_cmds.push("probe");
    }
    assert_eq!(got_cmds, want_cmds, "{ctx}: cmds");
    if let Some(pressed) = ex.get("pressed") {
        assert_eq!(Some(out.pressed), pressed.as_bool(), "{ctx}: pressed");
    }
    if let Some(raw) = ex.get("raw_pressed") {
        assert_eq!(Some(out.raw_pressed), raw.as_bool(), "{ctx}: raw_pressed");
    }
    if let Some(signal) = ex.get("signal") {
        assert_eq!(out.signal, wire_signal(signal), "{ctx}: signal");
    }
}

fn replay(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    let name = json_str(&doc, "name").to_owned();
    assert_eq!(doc["device"], "inrico-b02ptt", "{name}: device");
    let m = machine();
    let steps = doc["steps"].as_array().expect("steps must be an array");
    for (i, step) in steps.iter().enumerate() {
        let at = step["at_ms"].as_u64().expect("at_ms must be an integer");
        let ev = &step["event"];
        let payload = scenario_payload(ev);
        let wire = scenario_event(ev, &payload);
        let ctx = format!("{name} step {i} ({} at {at} ms)", json_str(ev, "kind"));
        check_step(&feed(m.0, &wire, at), &step["expect"], &ctx);
    }
}

#[test]
fn scenarios_replay_identically_through_the_abi() {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(SCENARIOS)
        .unwrap_or_else(|e| panic!("{SCENARIOS}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 20,
        "{} scenarios in {SCENARIOS}, want at least 20",
        paths.len()
    );
    for path in &paths {
        replay(path);
    }
}
