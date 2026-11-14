// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Replays every trace in `scenarios/` through the `ButtonMachine`.
//!
//! The macOS controller's tests replay the same files through the C ABI, so
//! the Rust machine and the Swift transport are pinned to one set of expected
//! outputs.
//!
//! Schema (exact): `{ "name", "device", "steps": [{ "at_ms", "event", "expect" }] }`.
//! `event.kind` is one of `enable{confirmed}`, `disable`, `confirmed`,
//! `adapter{state: on|off|unauthorized|resetting|unsupported}`,
//! `connected{epoch}`, `gatt_accepted{epoch}`, `gatt_rejected{epoch}`,
//! `subscribed{epoch}`, `unsubscribed{epoch}`, `notify{epoch, bytes}`,
//! `probe_ok{epoch, rssi}`, `probe_failed{epoch}`, `disconnected{epoch}`,
//! `sleep`, `wake`, `lease_lost`, `tick`. `expect.intent` is
//! `none|press|release`; `expect.reason` is required exactly when the intent
//! is release, and is one of `button|malformed|link_lost|adapter_off|disabled|sleep`.
//! `expect.status` is the snake-case `LinkStatus`; `expect.cmds` is the exact
//! set drawn from `probe` and `drop_link`. `pressed`, `raw_pressed` and
//! `signal` (`strong|fair|weak|null`) are optional and checked only when present.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use astar_ble_ptt::{
    AdapterState, ButtonMachine, Cmds, DEVICES, Intent, LinkEvent, LinkStatus, MachineConfig,
    NotifyBytes, Output, ReleaseReason, SignalBand,
};
use serde_json::Value;

/// Every scenario the macOS replay relies on, including the one that pins the
/// conditional latch (an idle loss arms nothing). More may exist; none of these
/// may go missing.
const REQUIRED: [&str; 23] = [
    "idle_disconnect_first_press_keys",
    "press_release_basic",
    "disconnect_mid_press",
    "resubscribe_while_held",
    "stale_notify_after_disconnect",
    "bt_off_mid_press",
    "sleep_mid_press",
    "malformed_value",
    "malformed_while_pressed_releases",
    "unconfirmed_never_presses",
    "rssi_two_misses_release",
    "rssi_reply_resets_misses",
    "idle_probe_misses_never_release",
    "notify_during_settle_latches",
    "notify_before_subscribe_ignored",
    "press_after_linklost_needs_release",
    "reconnect_does_not_clear_latch",
    "lease_lost_latches",
    "disable_mid_press",
    "gatt_rejected_lookalike",
    "unsubscribed_mid_press",
    "double_press_without_release_single_press",
    "adapter_unauthorized_status",
];

fn scenario_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("crates/astar-ble-ptt/scenarios exists")
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .expect("UTF-8 file name")
}

/// Fails when `obj` holds any key outside `allowed`. Without this a misspelt
/// key (`"presssed"`) would silently switch its check off.
fn assert_only_keys(ctx: &str, what: &str, obj: &Value, allowed: &[&str]) {
    let map = obj
        .as_object()
        .unwrap_or_else(|| panic!("{ctx}: {what} must be an object"));
    for key in map.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "{ctx}: unknown {what} key {key:?} (allowed: {allowed:?})"
        );
    }
}

fn epoch(v: &Value) -> u32 {
    u32::try_from(v["epoch"].as_u64().expect("event.epoch")).expect("epoch fits u32")
}

fn adapter(name: &str) -> AdapterState {
    match name {
        "on" => AdapterState::On,
        "off" => AdapterState::Off,
        "unauthorized" => AdapterState::Unauthorized,
        "resetting" => AdapterState::Resetting,
        "unsupported" => AdapterState::Unsupported,
        other => panic!("unknown adapter state {other:?}"),
    }
}

fn event(ctx: &str, v: &Value) -> LinkEvent {
    let kind = v["kind"].as_str().expect("event.kind");
    let fields: &[&str] = match kind {
        "enable" => &["kind", "confirmed"],
        "adapter" => &["kind", "state"],
        "notify" => &["kind", "epoch", "bytes"],
        "probe_ok" => &["kind", "epoch", "rssi"],
        "connected" | "gatt_accepted" | "gatt_rejected" | "subscribed" | "unsubscribed"
        | "probe_failed" | "disconnected" => &["kind", "epoch"],
        _ => &["kind"],
    };
    assert_only_keys(ctx, "event", v, fields);
    match kind {
        "enable" => LinkEvent::Enable {
            confirmed: v["confirmed"].as_bool().expect("enable.confirmed"),
        },
        "disable" => LinkEvent::Disable,
        "confirmed" => LinkEvent::Confirmed,
        "adapter" => LinkEvent::Adapter(adapter(v["state"].as_str().expect("adapter.state"))),
        "connected" => LinkEvent::Connected { epoch: epoch(v) },
        "gatt_accepted" => LinkEvent::GattAccepted { epoch: epoch(v) },
        "gatt_rejected" => LinkEvent::GattRejected { epoch: epoch(v) },
        "subscribed" => LinkEvent::Subscribed { epoch: epoch(v) },
        "unsubscribed" => LinkEvent::Unsubscribed { epoch: epoch(v) },
        "notify" => {
            let bytes: Vec<u8> = v["bytes"]
                .as_array()
                .expect("notify.bytes")
                .iter()
                .map(|b| u8::try_from(b.as_u64().expect("byte")).expect("byte fits u8"))
                .collect();
            LinkEvent::Notify {
                epoch: epoch(v),
                bytes: NotifyBytes::from_slice(&bytes),
            }
        }
        "probe_ok" => LinkEvent::ProbeOk {
            epoch: epoch(v),
            rssi: i16::try_from(v["rssi"].as_i64().expect("probe_ok.rssi")).expect("rssi fits i16"),
        },
        "probe_failed" => LinkEvent::ProbeFailed { epoch: epoch(v) },
        "disconnected" => LinkEvent::Disconnected { epoch: epoch(v) },
        "sleep" => LinkEvent::Sleep,
        "wake" => LinkEvent::Wake,
        "lease_lost" => LinkEvent::LeaseLost,
        "tick" => LinkEvent::Tick,
        other => panic!("unknown event kind {other:?}"),
    }
}

fn reason(name: &str) -> ReleaseReason {
    match name {
        "button" => ReleaseReason::Button,
        "malformed" => ReleaseReason::Malformed,
        "link_lost" => ReleaseReason::LinkLost,
        "adapter_off" => ReleaseReason::AdapterOff,
        "disabled" => ReleaseReason::Disabled,
        "sleep" => ReleaseReason::Sleep,
        other => panic!("unknown release reason {other:?}"),
    }
}

fn status(name: &str) -> LinkStatus {
    match name {
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
        other => panic!("unknown status {other:?}"),
    }
}

fn signal(v: &Value) -> Option<SignalBand> {
    match v.as_str() {
        None => {
            assert!(v.is_null(), "signal must be a string or null, got {v}");
            None
        }
        Some("strong") => Some(SignalBand::Strong),
        Some("fair") => Some(SignalBand::Fair),
        Some("weak") => Some(SignalBand::Weak),
        Some(other) => panic!("unknown signal {other:?}"),
    }
}

fn cmds(v: &Value) -> Cmds {
    let names: Vec<&str> = v
        .as_array()
        .expect("expect.cmds is an array")
        .iter()
        .map(|c| c.as_str().expect("cmd name"))
        .collect();
    let set: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(set.len(), names.len(), "duplicate cmd in {names:?}");
    for name in &set {
        assert!(
            matches!(*name, "probe" | "drop_link"),
            "unknown cmd {name:?}"
        );
    }
    Cmds {
        probe: set.contains("probe"),
        drop_link: set.contains("drop_link"),
    }
}

fn check(ctx: &str, expect: &Value, out: &Output) {
    assert_only_keys(
        ctx,
        "expect",
        expect,
        &[
            "intent",
            "reason",
            "status",
            "cmds",
            "pressed",
            "raw_pressed",
            "signal",
        ],
    );
    let kind = expect["intent"].as_str().expect("expect.intent");
    if kind != "release" {
        assert!(
            expect.get("reason").is_none(),
            "{ctx}: a reason without a release"
        );
    }
    let intent = match kind {
        "none" => Intent::None,
        "press" => Intent::Press,
        "release" => Intent::Release(reason(
            expect["reason"].as_str().expect("a release needs a reason"),
        )),
        other => panic!("{ctx}: unknown intent {other:?}"),
    };
    assert_eq!(out.intent, intent, "{ctx}: intent");
    assert_eq!(
        out.status,
        status(expect["status"].as_str().expect("expect.status")),
        "{ctx}: status"
    );
    assert_eq!(out.cmds, cmds(&expect["cmds"]), "{ctx}: cmds");
    if let Some(p) = expect.get("pressed") {
        assert_eq!(
            out.pressed,
            p.as_bool().expect("pressed is a bool"),
            "{ctx}: pressed"
        );
    }
    if let Some(p) = expect.get("raw_pressed") {
        assert_eq!(
            out.raw_pressed,
            p.as_bool().expect("raw_pressed is a bool"),
            "{ctx}: raw_pressed"
        );
    }
    if let Some(s) = expect.get("signal") {
        assert_eq!(out.signal, signal(s), "{ctx}: signal");
    }
}

/// Replays one parsed scenario; returns the number of steps checked.
fn replay_doc(doc: &Value) -> usize {
    let name = doc["name"].as_str().expect("scenario name");
    let device_id = doc["device"].as_str().expect("scenario device");
    let device = DEVICES
        .iter()
        .find(|d| d.id == device_id)
        .unwrap_or_else(|| panic!("{name}: unknown device {device_id:?}"));
    let mut machine = ButtonMachine::new(device, MachineConfig::DEFAULT);
    let steps = doc["steps"].as_array().expect("scenario steps");
    assert!(!steps.is_empty(), "{name}: no steps");
    let mut last_at = 0;
    for (i, step) in steps.iter().enumerate() {
        let at = step["at_ms"].as_u64().expect("at_ms");
        assert!(at >= last_at, "{name} step {i}: time went backwards");
        last_at = at;
        let ctx = format!("{name} step {i}");
        assert_only_keys(&ctx, "step", step, &["at_ms", "event", "expect"]);
        let out = machine.handle(event(&ctx, &step["event"]), Duration::from_millis(at));
        check(&ctx, &step["expect"], &out);
    }
    steps.len()
}

#[test]
fn scenario_set_is_complete() {
    let names: BTreeSet<String> = scenario_files()
        .iter()
        .map(|p| stem(p).to_owned())
        .collect();
    assert!(
        names.len() >= 20,
        "at least 20 scenarios, found {}",
        names.len()
    );
    for required in REQUIRED {
        assert!(
            names.contains(required),
            "missing scenarios/{required}.json"
        );
    }
}

#[test]
fn every_scenario_replays() {
    let files = scenario_files();
    assert!(
        files.len() >= 20,
        "at least 20 scenarios, found {}",
        files.len()
    );
    let mut steps = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).expect("readable scenario");
        let doc: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: not JSON: {e}", path.display()));
        assert_eq!(
            doc["name"].as_str(),
            Some(stem(path)),
            "name must match the file name"
        );
        steps += replay_doc(&doc);
    }
    assert!(
        steps >= 150,
        "only {steps} steps across {} scenarios",
        files.len()
    );
}

#[test]
fn replay_fails_on_a_wrong_expectation() {
    // Proves the harness can fail: a first step that claims Ready must panic.
    let doc = serde_json::json!({
        "name": "wrong",
        "device": "inrico-b02ptt",
        "steps": [{
            "at_ms": 0,
            "event": { "kind": "enable", "confirmed": true },
            "expect": { "intent": "none", "status": "ready", "cmds": [] }
        }]
    });
    assert!(std::panic::catch_unwind(|| replay_doc(&doc)).is_err());
}

#[test]
fn replay_fails_on_a_misspelt_key() {
    // A correct expectation plus a misspelt optional key must panic, not
    // silently skip the check that key was meant to switch on.
    let good = serde_json::json!({
        "name": "good",
        "device": "inrico-b02ptt",
        "steps": [{
            "at_ms": 0,
            "event": { "kind": "enable", "confirmed": true },
            "expect": { "intent": "none", "status": "searching", "cmds": [] }
        }]
    });
    assert_eq!(replay_doc(&good), 1);
    let mut misspelt_expect = good.clone();
    misspelt_expect["steps"][0]["expect"]["presssed"] = serde_json::json!(true);
    assert!(std::panic::catch_unwind(|| replay_doc(&misspelt_expect)).is_err());
    let mut stray_event_field = good;
    stray_event_field["steps"][0]["event"]["epoch"] = serde_json::json!(1);
    assert!(std::panic::catch_unwind(|| replay_doc(&stray_event_field)).is_err());
}
