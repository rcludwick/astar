// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One test per row of the `ButtonMachine` transition table (machine.rs module
//! docs), each covering both sides of its boundary, plus the named tests the
//! mutation checklist relies on.
#![allow(clippy::too_many_lines)]

use std::time::Duration;

use astar_ble_ptt::{
    AdapterState, ButtonMachine, Cmds, DEVICES, Intent, LinkEvent, LinkStatus, MachineConfig,
    NotifyBytes, Output, ReleaseReason, SignalBand,
};

const LINK_LOST: Intent = Intent::Release(ReleaseReason::LinkLost);
const BUTTON: Intent = Intent::Release(ReleaseReason::Button);
const PROBE: Cmds = Cmds {
    probe: true,
    drop_link: false,
};
const DROP: Cmds = Cmds {
    probe: false,
    drop_link: true,
};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn machine() -> ButtonMachine {
    ButtonMachine::new(&DEVICES[0], MachineConfig::DEFAULT)
}

fn notify(epoch: u32, payload: &[u8]) -> LinkEvent {
    LinkEvent::Notify {
        epoch,
        bytes: NotifyBytes::from_slice(payload),
    }
}

fn feed(m: &mut ButtonMachine, steps: &[(LinkEvent, u64)]) -> Output {
    let mut last = m.output();
    for &(event, at) in steps {
        last = m.handle(event, ms(at));
    }
    last
}

/// Links `epoch`: connect at `at`, GATT accepted at `at + 10`, subscribed at `at + 20`.
fn relink(m: &mut ButtonMachine, epoch: u32, at: u64) -> Output {
    feed(
        m,
        &[
            (LinkEvent::Connected { epoch }, at),
            (LinkEvent::GattAccepted { epoch }, at + 10),
            (LinkEvent::Subscribed { epoch }, at + 20),
        ],
    )
}

/// Enabled with `confirmed`, adapter on, epoch 1 linked and subscribed at 30 ms.
fn linked(confirmed: bool) -> ButtonMachine {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
        ],
    );
    let _ = relink(&mut m, 1, 10);
    m
}

fn ready() -> ButtonMachine {
    let m = linked(true);
    assert_eq!(m.output().status, LinkStatus::Ready);
    m
}

/// `ready()` with the button pressed at 1000 ms.
fn pressed() -> ButtonMachine {
    let mut m = ready();
    assert_eq!(m.handle(notify(1, &[1]), ms(1000)).intent, Intent::Press);
    m
}

#[test]
fn fresh_machine_is_disabled_and_idle() {
    assert_eq!(
        machine().output(),
        Output {
            intent: Intent::None,
            cmds: Cmds::default(),
            status: LinkStatus::Disabled,
            signal: None,
            last_rssi: None,
            raw_pressed: false,
            pressed: false,
        }
    );
}

#[test]
fn default_config_is_pinned() {
    let expected = MachineConfig {
        settle_after_subscribe: ms(300),
        probe_interval_keyed: ms(500),
        probe_misses_dead: 2,
        probe_interval_idle: ms(5000),
        lease_ttl: ms(1500),
        renew_period: ms(250),
        band_strong_dbm: -65,
        band_fair_dbm: -80,
    };
    assert_eq!(MachineConfig::DEFAULT, expected);
    assert_eq!(MachineConfig::default(), expected);
    assert_eq!(*machine().config(), expected);
}

#[test]
fn custom_config_is_honoured() {
    let config = MachineConfig {
        settle_after_subscribe: ms(100),
        ..MachineConfig::DEFAULT
    };
    let mut m = ButtonMachine::new(&DEVICES[0], config);
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
        ],
    );
    let _ = relink(&mut m, 1, 10);
    assert_eq!(m.config().settle_after_subscribe, ms(100));
    assert_eq!(m.handle(notify(1, &[1]), ms(130)).intent, Intent::Press);
}

// Row: Notify [01], gates open, past settle -> Press.
// Row: Notify [01] inside the settle window -> latched.
#[test]
fn notify_during_settle_latches() {
    // Subscribed at 30 ms: 329 ms is 299 ms into the window, 330 ms is 300 ms.
    let mut m = ready();
    let out = m.handle(notify(1, &[1]), ms(329));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert!(out.raw_pressed);
    assert!(!out.pressed);
    // Still latched after the window: only a live [00] clears it.
    assert_eq!(m.handle(notify(1, &[1]), ms(400)).intent, Intent::None);

    let mut m = ready();
    let out = m.handle(notify(1, &[1]), ms(330));
    assert_eq!(out.intent, Intent::Press);
    assert_eq!(out.status, LinkStatus::Ready);
    assert!(out.pressed);
    assert!(out.raw_pressed);
}

// Row: Notify [01] while latched / pressed / unconfirmed -> nothing, raw_pressed set.
#[test]
fn row_press_ignored_when_latched_pressed_unconfirmed_or_adapter_unknown() {
    let mut latched = ready();
    let _ = latched.handle(notify(1, &[1]), ms(100));
    let mut held = pressed();
    let mut unconfirmed = linked(false);
    let mut adapter_unknown = machine();
    let _ = feed(
        &mut adapter_unknown,
        &[(LinkEvent::Enable { confirmed: true }, 0)],
    );
    let _ = relink(&mut adapter_unknown, 1, 10);

    for (name, m, still_pressed) in [
        ("latched", &mut latched, false),
        ("pressed", &mut held, true),
        ("unconfirmed", &mut unconfirmed, false),
        ("adapter unknown", &mut adapter_unknown, false),
    ] {
        let out = m.handle(notify(1, &[1]), ms(2000));
        assert_eq!(out.intent, Intent::None, "{name}");
        assert!(out.raw_pressed, "{name}");
        assert_eq!(out.pressed, still_pressed, "{name}");
    }
}

// Row: Notify [00] on the live epoch -> Release(Button) every time, latch cleared.
#[test]
fn row_release_byte_always_releases_and_clears_the_latch() {
    let mut m = ready();
    for at in [500, 600] {
        let out = m.handle(notify(1, &[0]), ms(at));
        assert_eq!(out.intent, BUTTON);
        assert!(!out.raw_pressed);
    }

    let mut m = pressed();
    let out = m.handle(notify(1, &[0]), ms(1400));
    assert_eq!(out.intent, BUTTON);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::Ready);

    let mut m = ready();
    let _ = m.handle(notify(1, &[1]), ms(100));
    assert_eq!(m.handle(notify(1, &[0]), ms(500)).status, LinkStatus::Ready);
    assert_eq!(m.handle(notify(1, &[1]), ms(600)).intent, Intent::Press);
}

// Row: Notify other on the live epoch -> latched, Release(Malformed).
#[test]
fn row_malformed_latches_and_releases() {
    for payload in [&[][..], &[2][..], &[1, 0][..], &[1; 9][..]] {
        let mut m = ready();
        let out = m.handle(notify(1, payload), ms(500));
        assert_eq!(
            out.intent,
            Intent::Release(ReleaseReason::Malformed),
            "{payload:?}"
        );
        assert_eq!(out.status, LinkStatus::ReleaseToArm, "{payload:?}");
        assert_eq!(
            m.handle(notify(1, &[1]), ms(600)).intent,
            Intent::None,
            "{payload:?}"
        );
    }
}

#[test]
fn malformed_while_pressed_releases() {
    let mut m = pressed();
    let out = m.handle(notify(1, &[1, 0]), ms(1100));
    assert_eq!(out.intent, Intent::Release(ReleaseReason::Malformed));
    assert!(!out.pressed);
    assert!(!out.raw_pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    let out = m.handle(notify(1, &[1]), ms(1200));
    assert_eq!(out.intent, Intent::None);
    assert!(!out.pressed);
}

// Row: any epoch-tagged event that is not the live epoch, or arrives with the link down -> dropped.
#[test]
fn row_stale_or_down_events_are_dropped() {
    let mut m = pressed();
    let before = m.output();
    for event in [
        notify(2, &[0]),
        notify(0, &[1]),
        LinkEvent::Disconnected { epoch: 2 },
        LinkEvent::Unsubscribed { epoch: 7 },
        LinkEvent::GattRejected { epoch: 2 },
        LinkEvent::GattAccepted { epoch: 2 },
        LinkEvent::Subscribed { epoch: 2 },
        LinkEvent::ProbeFailed { epoch: 2 },
        LinkEvent::ProbeOk {
            epoch: 2,
            rssi: -50,
        },
    ] {
        assert_eq!(m.handle(event, ms(1100)), before, "{event:?}");
    }

    let mut m = ready();
    let _ = m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(500));
    let before = m.output();
    for event in [
        notify(1, &[1]),
        notify(1, &[0]),
        LinkEvent::Subscribed { epoch: 1 },
        LinkEvent::Unsubscribed { epoch: 1 },
        LinkEvent::ProbeOk {
            epoch: 1,
            rssi: -50,
        },
        LinkEvent::ProbeFailed { epoch: 1 },
        LinkEvent::GattAccepted { epoch: 1 },
    ] {
        assert_eq!(m.handle(event, ms(600)), before, "{event:?}");
    }
}

#[test]
fn stale_notify_after_disconnect() {
    let mut m = pressed();
    assert_eq!(
        m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(1100))
            .intent,
        LINK_LOST
    );
    let out = m.handle(notify(1, &[1]), ms(1200));
    assert_eq!(out.intent, Intent::None);
    assert!(!out.pressed);
    assert!(!out.raw_pressed);
    assert_eq!(relink(&mut m, 2, 1300).status, LinkStatus::ReleaseToArm);
    // A release byte from the dead link must not re-arm the new one.
    let out = m.handle(notify(1, &[0]), ms(1700));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(2, &[1]), ms(1800)).intent, Intent::None);
}

// Row: Disconnected / Unsubscribed on the live epoch -> link down, latched if held, Release(LinkLost) if pressed.
#[test]
fn row_disconnect_and_unsubscribe_release_link_lost() {
    for event in [
        LinkEvent::Disconnected { epoch: 1 },
        LinkEvent::Unsubscribed { epoch: 1 },
    ] {
        let mut m = pressed();
        let _ = m.handle(
            LinkEvent::ProbeOk {
                epoch: 1,
                rssi: -50,
            },
            ms(1050),
        );
        let out = m.handle(event, ms(1100));
        assert_eq!(out.intent, LINK_LOST, "{event:?}");
        assert_eq!(out.status, LinkStatus::Reconnecting, "{event:?}");
        assert!(!out.pressed, "{event:?}");
        assert_eq!(out.last_rssi, None, "{event:?}");
        assert_eq!(out.signal, None, "{event:?}");
        // Held at the loss: latched across the relink.
        assert_eq!(
            relink(&mut m, 2, 1200).status,
            LinkStatus::ReleaseToArm,
            "{event:?}"
        );

        let mut m = ready();
        let out = m.handle(event, ms(500));
        assert_eq!(out.intent, Intent::None, "{event:?}");
        assert_eq!(out.status, LinkStatus::Reconnecting, "{event:?}");
        // Up at the loss: not latched, so the relink is Ready.
        assert_eq!(
            relink(&mut m, 2, 600).status,
            LinkStatus::Ready,
            "{event:?}"
        );
    }
}

// Row: Adapter(not On) -> link down, Release(AdapterOff) if pressed.
#[test]
fn row_adapter_not_on_releases() {
    for (state, status) in [
        (AdapterState::Off, LinkStatus::BluetoothOff),
        (AdapterState::Resetting, LinkStatus::BluetoothOff),
        (AdapterState::Unauthorized, LinkStatus::Unauthorized),
        (AdapterState::Unsupported, LinkStatus::Unsupported),
    ] {
        let mut m = pressed();
        let out = m.handle(LinkEvent::Adapter(state), ms(1100));
        assert_eq!(
            out.intent,
            Intent::Release(ReleaseReason::AdapterOff),
            "{state:?}"
        );
        assert_eq!(out.status, status, "{state:?}");
        assert!(!out.pressed, "{state:?}");
        let out = m.handle(LinkEvent::Adapter(AdapterState::On), ms(1200));
        assert_eq!(out.status, LinkStatus::Reconnecting, "{state:?}");
        let before = m.output();
        assert_eq!(m.handle(notify(1, &[1]), ms(1300)), before, "{state:?}");
    }
}

#[test]
fn adapter_on_while_pressed_changes_nothing() {
    let mut m = pressed();
    let before = m.output();
    assert_eq!(
        m.handle(LinkEvent::Adapter(AdapterState::On), ms(1100)),
        before
    );
}

// Row: Disable / Sleep -> link down, Release(reason) if pressed.
#[test]
fn row_disable_and_sleep_release() {
    for (event, reason, status) in [
        (
            LinkEvent::Disable,
            ReleaseReason::Disabled,
            LinkStatus::Disabled,
        ),
        (
            LinkEvent::Sleep,
            ReleaseReason::Sleep,
            LinkStatus::Reconnecting,
        ),
    ] {
        let mut m = pressed();
        let out = m.handle(event, ms(1100));
        assert_eq!(out.intent, Intent::Release(reason), "{event:?}");
        assert_eq!(out.status, status, "{event:?}");
        assert!(!out.pressed, "{event:?}");
        let before = m.output();
        assert_eq!(m.handle(notify(1, &[1]), ms(1200)), before, "{event:?}");
    }
}

// Row: LeaseLost -> press dropped, latched if held, no intent.
#[test]
fn row_lease_lost_latches_without_intent() {
    let mut m = pressed();
    let out = m.handle(LinkEvent::LeaseLost, ms(1010));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.cmds, Cmds::default());
    assert!(!out.pressed);
    assert!(out.raw_pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(1, &[1]), ms(1100)).intent, Intent::None);
    assert_eq!(m.handle(notify(1, &[0]), ms(1200)).intent, BUTTON);
    assert_eq!(m.handle(notify(1, &[1]), ms(1300)).intent, Intent::Press);
}

#[test]
fn lease_lost_with_button_up_does_not_latch() {
    let mut m = ready();
    let out = m.handle(LinkEvent::LeaseLost, ms(500));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.cmds, Cmds::default());
    assert!(!out.raw_pressed);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::Ready);
    // The button was up, so the next press keys without a release first.
    let out = m.handle(notify(1, &[1]), ms(600));
    assert_eq!(out.intent, Intent::Press);
    assert!(out.pressed);
}

// Row: Tick while pressed, probe due -> cmd Probe (500 ms boundary).
#[test]
fn row_keyed_probe_due_every_500ms() {
    let mut m = pressed();
    assert_eq!(m.handle(LinkEvent::Tick, ms(1499)).cmds, Cmds::default());
    assert_eq!(m.handle(LinkEvent::Tick, ms(1500)).cmds, PROBE);
    assert_eq!(m.handle(LinkEvent::Tick, ms(1999)).cmds, Cmds::default());
    let out = m.handle(LinkEvent::Tick, ms(2000));
    assert_eq!(out.cmds, PROBE);
    assert!(out.pressed);
}

// Row: Tick / ProbeFailed while pressed, misses reach 2 -> Release(LinkLost), DropLink.
#[test]
fn rssi_two_misses_release() {
    let mut m = pressed();
    assert_eq!(m.handle(LinkEvent::Tick, ms(1500)).cmds, PROBE);
    // First miss: the 1500 ms probe is still unanswered. The key stays.
    let out = m.handle(LinkEvent::Tick, ms(2000));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.cmds, PROBE);
    assert!(out.pressed);
    // Second consecutive miss releases and drops the link.
    let out = m.handle(LinkEvent::Tick, ms(2500));
    assert_eq!(out.intent, LINK_LOST);
    assert_eq!(out.cmds, DROP);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::Reconnecting);
    let before = m.output();
    assert_eq!(m.handle(notify(1, &[1]), ms(2600)), before);
}

#[test]
fn rssi_two_failed_probes_release() {
    let mut m = pressed();
    assert_eq!(m.handle(LinkEvent::Tick, ms(1500)).cmds, PROBE);
    let out = m.handle(LinkEvent::ProbeFailed { epoch: 1 }, ms(1600));
    assert_eq!(out.intent, Intent::None);
    assert!(out.pressed);
    assert_eq!(m.handle(LinkEvent::Tick, ms(2000)).cmds, PROBE);
    let out = m.handle(LinkEvent::ProbeFailed { epoch: 1 }, ms(2100));
    assert_eq!(out.intent, LINK_LOST);
    assert_eq!(out.cmds, DROP);
    assert_eq!(out.status, LinkStatus::Reconnecting);
}

// Row: ProbeOk on the live epoch -> misses reset, signal band updated.
#[test]
fn rssi_reply_resets_misses() {
    let mut m = pressed();
    assert_eq!(m.handle(LinkEvent::Tick, ms(1500)).cmds, PROBE);
    assert_eq!(m.handle(LinkEvent::Tick, ms(2000)).cmds, PROBE); // one miss
    let out = m.handle(
        LinkEvent::ProbeOk {
            epoch: 1,
            rssi: -60,
        },
        ms(2100),
    );
    assert_eq!(out.signal, Some(SignalBand::Strong));
    let out = m.handle(LinkEvent::Tick, ms(2500));
    assert_eq!(out.cmds, PROBE);
    assert!(out.pressed);
    // Without the reset this would be the third miss and would have released already.
    let out = m.handle(LinkEvent::Tick, ms(3000));
    assert_eq!(out.cmds, PROBE);
    assert!(out.pressed);
    assert_eq!(out.intent, Intent::None);
}

#[test]
fn row_probe_ok_sets_signal_band() {
    let mut m = ready();
    assert_eq!(m.output().signal, None);
    for (rssi, band) in [
        (-40, SignalBand::Strong),
        (-65, SignalBand::Strong),
        (-66, SignalBand::Fair),
        (-80, SignalBand::Fair),
        (-81, SignalBand::Weak),
        (-100, SignalBand::Weak),
    ] {
        let out = m.handle(LinkEvent::ProbeOk { epoch: 1, rssi }, ms(500));
        assert_eq!(out.signal, Some(band), "{rssi}");
        assert_eq!(out.last_rssi, Some(rssi), "{rssi}");
        assert_eq!(out.intent, Intent::None, "{rssi}");
    }
}

// Row: Tick while linked and not pressed, idle probe due -> cmd Probe (status only).
#[test]
fn idle_probe_due_every_five_seconds() {
    let mut m = ready();
    assert_eq!(m.handle(LinkEvent::Tick, ms(100)).cmds, PROBE);
    assert_eq!(m.handle(LinkEvent::Tick, ms(5099)).cmds, Cmds::default());
    assert_eq!(m.handle(LinkEvent::Tick, ms(5100)).cmds, PROBE);
}

#[test]
fn idle_misses_never_release() {
    let mut m = ready();
    assert_eq!(m.handle(LinkEvent::Tick, ms(100)).cmds, PROBE);
    assert_eq!(m.handle(LinkEvent::Tick, ms(5100)).cmds, PROBE);
    assert_eq!(m.handle(LinkEvent::Tick, ms(10_100)).cmds, PROBE);
    for at in [10_200, 10_300] {
        let out = m.handle(LinkEvent::ProbeFailed { epoch: 1 }, ms(at));
        assert_eq!(out.intent, Intent::None);
        assert_eq!(out.cmds, Cmds::default());
        assert_eq!(out.status, LinkStatus::Ready);
    }
    let out = m.handle(LinkEvent::Tick, ms(15_100));
    assert_eq!(out.cmds, PROBE);
    assert_eq!(out.status, LinkStatus::Ready);
    assert_eq!(m.handle(notify(1, &[1]), ms(15_200)).intent, Intent::Press);
}

#[test]
fn tick_before_subscribe_never_probes() {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
        ],
    );
    assert_eq!(m.handle(LinkEvent::Tick, ms(5)).cmds, Cmds::default());
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Connected { epoch: 1 }, 10),
            (LinkEvent::GattAccepted { epoch: 1 }, 20),
        ],
    );
    assert_eq!(m.handle(LinkEvent::Tick, ms(25)).cmds, Cmds::default());
}

// Row: Wake -> no keying.
#[test]
fn row_wake_never_keys() {
    let mut m = pressed();
    let before = m.output();
    assert_eq!(m.handle(LinkEvent::Wake, ms(1100)), before);

    let mut m = ready();
    let _ = m.handle(LinkEvent::Sleep, ms(500));
    let out = m.handle(LinkEvent::Wake, ms(600));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::Reconnecting);
    assert!(!out.pressed);
}

#[test]
fn notify_before_subscribe_ignored() {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
            (LinkEvent::Connected { epoch: 1 }, 10),
            (LinkEvent::GattAccepted { epoch: 1 }, 20),
        ],
    );
    let before = m.output();
    assert_eq!(m.handle(notify(1, &[1]), ms(25)), before);
    assert!(!m.output().raw_pressed);
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(30)).status,
        LinkStatus::Ready
    );
    assert_eq!(m.handle(notify(1, &[1]), ms(330)).intent, Intent::Press);
}

#[test]
fn subscribed_without_gatt_is_ignored() {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
            (LinkEvent::Connected { epoch: 1 }, 10),
        ],
    );
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(20)).status,
        LinkStatus::Connecting
    );
    let out = m.handle(notify(1, &[1]), ms(2000));
    assert_eq!(out.intent, Intent::None);
    assert!(!out.raw_pressed);
    let _ = m.handle(LinkEvent::GattAccepted { epoch: 1 }, ms(2010));
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(2020))
            .status,
        LinkStatus::Ready
    );
    assert_eq!(m.handle(notify(1, &[1]), ms(2320)).intent, Intent::Press);
}

#[test]
fn resubscribe_while_held_releases() {
    let mut m = pressed();
    let out = m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(1100));
    assert_eq!(out.intent, LINK_LOST);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(1, &[1]), ms(1500)).intent, Intent::None);
    assert_eq!(m.handle(notify(1, &[0]), ms(1600)).intent, BUTTON);
    assert_eq!(m.handle(notify(1, &[1]), ms(1700)).intent, Intent::Press);
}

#[test]
fn resubscribe_restarts_settle() {
    let mut m = ready();
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(1000))
            .intent,
        Intent::None
    );
    let out = m.handle(notify(1, &[1]), ms(1299));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);

    let mut m = ready();
    let _ = m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(1000));
    assert_eq!(m.handle(notify(1, &[1]), ms(1300)).intent, Intent::Press);
}

#[test]
fn press_after_linklost_needs_release() {
    let mut m = pressed();
    assert_eq!(
        m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(1100))
            .intent,
        LINK_LOST
    );
    assert_eq!(relink(&mut m, 2, 1200).status, LinkStatus::ReleaseToArm);
    let out = m.handle(notify(2, &[1]), ms(2000));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    let out = m.handle(notify(2, &[0]), ms(2100));
    assert_eq!(out.intent, BUTTON);
    assert_eq!(out.status, LinkStatus::Ready);
    assert_eq!(m.handle(notify(2, &[1]), ms(2200)).intent, Intent::Press);
}

#[test]
fn reconnect_does_not_clear_latch() {
    let mut m = pressed();
    let _ = m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(1100));
    assert_eq!(relink(&mut m, 2, 1200).status, LinkStatus::ReleaseToArm);
    assert_eq!(
        m.handle(LinkEvent::Disconnected { epoch: 2 }, ms(1300))
            .intent,
        Intent::None
    );
    assert_eq!(relink(&mut m, 3, 1400).status, LinkStatus::ReleaseToArm);
    let out = m.handle(notify(3, &[1]), ms(2000));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
}

/// Every way the link goes away and comes back: the loss events, then
/// whatever brings the link back (nothing, for a plain drop).
const LOSSES: [&[LinkEvent]; 5] = [
    &[LinkEvent::Disconnected { epoch: 1 }],
    &[LinkEvent::Unsubscribed { epoch: 1 }],
    &[LinkEvent::Sleep, LinkEvent::Wake],
    &[
        LinkEvent::Adapter(AdapterState::Off),
        LinkEvent::Adapter(AdapterState::On),
    ],
    &[LinkEvent::Disable, LinkEvent::Enable { confirmed: true }],
];

// The conditional latch: a loss with the button up arms no latch, so after
// the relink the first press past the settle window keys.
#[test]
fn idle_disconnect_first_press_keys() {
    for loss in LOSSES {
        let mut m = ready();
        for &event in loss {
            assert_eq!(m.handle(event, ms(500)).intent, Intent::None, "{loss:?}");
        }
        assert!(!m.output().raw_pressed, "{loss:?}");
        assert_eq!(relink(&mut m, 2, 600).status, LinkStatus::Ready, "{loss:?}");
        // Subscribed at 620 ms; 920 ms is exactly the end of the settle window.
        let out = m.handle(notify(2, &[1]), ms(920));
        assert_eq!(out.intent, Intent::Press, "{loss:?}");
        assert!(out.pressed, "{loss:?}");
        assert_eq!(out.status, LinkStatus::Ready, "{loss:?}");
        let out = m.handle(notify(2, &[0]), ms(1000));
        assert_eq!(out.intent, BUTTON, "{loss:?}");
        assert!(!out.pressed, "{loss:?}");
    }
}

// A loss while the button is down latches, whichever way the link went, and
// the latch survives the relink until a live [00].
#[test]
fn loss_while_held_latches_until_release() {
    let reasons = [
        ReleaseReason::LinkLost,
        ReleaseReason::LinkLost,
        ReleaseReason::Sleep,
        ReleaseReason::AdapterOff,
        ReleaseReason::Disabled,
    ];
    for (loss, reason) in LOSSES.into_iter().zip(reasons) {
        let mut m = pressed();
        let (first, rest) = loss.split_first().expect("a loss has an event");
        assert_eq!(
            m.handle(*first, ms(1100)).intent,
            Intent::Release(reason),
            "{loss:?}"
        );
        for &event in rest {
            assert_eq!(m.handle(event, ms(1110)).intent, Intent::None, "{loss:?}");
        }
        assert!(!m.output().pressed, "{loss:?}");
        assert!(!m.output().raw_pressed, "{loss:?}");
        assert_eq!(
            relink(&mut m, 2, 1200).status,
            LinkStatus::ReleaseToArm,
            "{loss:?}"
        );
        // Subscribed at 1220 ms; 1520 ms is past the settle window, so only
        // the latch can stop this press.
        let out = m.handle(notify(2, &[1]), ms(1520));
        assert_eq!(out.intent, Intent::None, "{loss:?}");
        assert!(!out.pressed, "{loss:?}");
        assert_eq!(out.status, LinkStatus::ReleaseToArm, "{loss:?}");
        let out = m.handle(notify(2, &[0]), ms(1600));
        assert_eq!(out.intent, BUTTON, "{loss:?}");
        assert_eq!(out.status, LinkStatus::Ready, "{loss:?}");
        assert_eq!(
            m.handle(notify(2, &[1]), ms(1700)).intent,
            Intent::Press,
            "{loss:?}"
        );
    }
}

#[test]
fn connected_replacing_live_link_releases() {
    let mut m = pressed();
    let out = m.handle(LinkEvent::Connected { epoch: 2 }, ms(1100));
    assert_eq!(out.intent, LINK_LOST);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::Connecting);
    let before = m.output();
    assert_eq!(m.handle(notify(1, &[0]), ms(1200)), before);
    // Held when it was replaced: the new link starts latched.
    let _ = m.handle(LinkEvent::GattAccepted { epoch: 2 }, ms(1210));
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 2 }, ms(1220))
            .status,
        LinkStatus::ReleaseToArm
    );

    // Replaced with the button up: the new link is Ready.
    let mut m = ready();
    let out = m.handle(LinkEvent::Connected { epoch: 2 }, ms(500));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.status, LinkStatus::Connecting);
    let _ = m.handle(LinkEvent::GattAccepted { epoch: 2 }, ms(510));
    assert_eq!(
        m.handle(LinkEvent::Subscribed { epoch: 2 }, ms(520)).status,
        LinkStatus::Ready
    );
}

#[test]
fn connected_with_old_or_zero_epoch_is_ignored() {
    let mut m = ready();
    let _ = m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(500));
    let before = m.output();
    assert_eq!(m.handle(LinkEvent::Connected { epoch: 1 }, ms(600)), before);
    assert_eq!(m.handle(LinkEvent::Connected { epoch: 0 }, ms(700)), before);
    assert_eq!(
        m.handle(LinkEvent::Connected { epoch: 2 }, ms(800)).status,
        LinkStatus::Connecting
    );

    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
        ],
    );
    assert_eq!(
        m.handle(LinkEvent::Connected { epoch: 0 }, ms(2)).status,
        LinkStatus::Searching
    );
}

#[test]
fn connected_while_disabled_is_ignored() {
    let mut m = machine();
    assert_eq!(
        m.handle(LinkEvent::Connected { epoch: 5 }, ms(0)).status,
        LinkStatus::Disabled
    );
    let out = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 1),
            (LinkEvent::Adapter(AdapterState::On), 2),
        ],
    );
    assert_eq!(out.status, LinkStatus::Searching);
    // Epoch 5 was seen, though not adopted: epochs up to 5 are stale now.
    let before = m.output();
    assert_eq!(m.handle(LinkEvent::Connected { epoch: 1 }, ms(3)), before);
    assert_eq!(m.handle(LinkEvent::Connected { epoch: 5 }, ms(4)), before);
    assert_eq!(
        m.handle(LinkEvent::Connected { epoch: 6 }, ms(5)).status,
        LinkStatus::Connecting
    );
}

#[test]
fn gatt_rejected_drops_link_and_reports_not_supported() {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
            (LinkEvent::Connected { epoch: 1 }, 10),
        ],
    );
    let out = m.handle(LinkEvent::GattRejected { epoch: 1 }, ms(20));
    assert_eq!(out.intent, Intent::None);
    assert_eq!(out.cmds, DROP);
    assert_eq!(out.status, LinkStatus::NotSupportedDevice);
    let before = m.output();
    assert_eq!(m.handle(LinkEvent::Subscribed { epoch: 1 }, ms(30)), before);
    assert_eq!(
        m.handle(LinkEvent::Connected { epoch: 2 }, ms(100)).status,
        LinkStatus::Connecting
    );
}

#[test]
fn enable_clears_not_supported_device() {
    let mut m = machine();
    let _ = feed(
        &mut m,
        &[
            (LinkEvent::Enable { confirmed: true }, 0),
            (LinkEvent::Adapter(AdapterState::On), 1),
            (LinkEvent::Connected { epoch: 1 }, 10),
            (LinkEvent::GattRejected { epoch: 1 }, 20),
        ],
    );
    assert_eq!(m.output().status, LinkStatus::NotSupportedDevice);
    assert_eq!(
        m.handle(LinkEvent::Enable { confirmed: true }, ms(30))
            .status,
        LinkStatus::Reconnecting
    );
    // The rejected link went down with the button up, so nothing latched.
    assert_eq!(relink(&mut m, 2, 40).status, LinkStatus::Ready);
}

#[test]
fn unconfirmed_never_presses() {
    let mut m = linked(false);
    assert_eq!(m.output().status, LinkStatus::NeedsConfirm);
    for at in [500, 1000, 5000] {
        let out = m.handle(notify(1, &[1]), ms(at));
        assert_eq!(out.intent, Intent::None, "{at}");
        assert!(out.raw_pressed, "{at}");
        assert!(!out.pressed, "{at}");
        assert_eq!(
            m.handle(notify(1, &[0]), ms(at + 100)).intent,
            BUTTON,
            "{at}"
        );
    }
}

#[test]
fn confirm_while_held_needs_release() {
    let mut m = linked(false);
    let _ = m.handle(notify(1, &[1]), ms(500));
    let out = m.handle(LinkEvent::Confirmed, ms(600));
    assert_eq!(out.intent, Intent::None);
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(1, &[1]), ms(700)).intent, Intent::None);
    let out = m.handle(notify(1, &[0]), ms(800));
    assert_eq!(out.intent, BUTTON);
    assert_eq!(out.status, LinkStatus::Ready);
    assert_eq!(m.handle(notify(1, &[1]), ms(900)).intent, Intent::Press);
}

#[test]
fn confirmed_with_button_up_is_ready() {
    let mut m = linked(false);
    assert_eq!(
        m.handle(LinkEvent::Confirmed, ms(500)).status,
        LinkStatus::Ready
    );
    assert_eq!(m.handle(notify(1, &[1]), ms(600)).intent, Intent::Press);
}

#[test]
fn raw_press_lost_while_unconfirmed_latches() {
    let mut m = linked(false);
    let _ = m.handle(notify(1, &[1]), ms(500));
    let _ = m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(600));
    let _ = m.handle(LinkEvent::Confirmed, ms(700));
    assert_eq!(relink(&mut m, 2, 800).status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(2, &[1]), ms(1200)).intent, Intent::None);
}

#[test]
fn enable_while_pressed_releases() {
    let mut m = pressed();
    let out = m.handle(LinkEvent::Enable { confirmed: true }, ms(1100));
    assert_eq!(out.intent, Intent::Release(ReleaseReason::Disabled));
    assert!(!out.pressed);
    assert_eq!(out.status, LinkStatus::ReleaseToArm);
    assert_eq!(m.handle(notify(1, &[1]), ms(1200)).intent, Intent::None);
    assert_eq!(m.handle(notify(1, &[0]), ms(1300)).intent, BUTTON);
    assert_eq!(m.handle(notify(1, &[1]), ms(1400)).intent, Intent::Press);
}

#[test]
fn reenable_after_disable_is_searching() {
    let mut m = ready();
    assert_eq!(
        m.handle(LinkEvent::Disable, ms(500)).status,
        LinkStatus::Disabled
    );
    assert_eq!(
        m.handle(LinkEvent::Enable { confirmed: true }, ms(600))
            .status,
        LinkStatus::Searching
    );
}

#[test]
fn enable_while_enabled_keeps_reconnecting() {
    let mut m = ready();
    let _ = m.handle(LinkEvent::Disconnected { epoch: 1 }, ms(500));
    assert_eq!(
        m.handle(LinkEvent::Enable { confirmed: true }, ms(600))
            .status,
        LinkStatus::Reconnecting
    );
}

#[test]
fn transport_never_writes_characteristics() {
    // The machine can only ever ask for an RSSI read or a link drop. `Cmds`
    // has exactly these two flags: a write command cannot be added without the
    // size check and the exhaustive destructuring below failing.
    assert_eq!(std::mem::size_of::<Cmds>(), 2);
    let mut m = pressed();
    let mut seen = Vec::new();
    let mut at = 1500;
    for event in [
        LinkEvent::Tick,
        LinkEvent::ProbeFailed { epoch: 1 },
        LinkEvent::Tick,
        notify(1, &[9]),
        LinkEvent::GattRejected { epoch: 1 },
        LinkEvent::Adapter(AdapterState::Off),
        LinkEvent::Sleep,
        LinkEvent::Wake,
        LinkEvent::Disable,
        LinkEvent::LeaseLost,
    ] {
        let Cmds { probe, drop_link } = m.handle(event, ms(at)).cmds;
        seen.push((probe, drop_link));
        at += 500;
    }
    assert_eq!(
        seen,
        vec![
            (true, false),
            (false, false),
            (true, false),
            (false, false),
            (false, true),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
        ]
    );
}
