// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Property tests for the `ButtonMachine` invariants, over random event traces
//! with monotone time:
//!
//! * **I1** (S1): every Press passes an independently written gate.
//! * **I2**: never two Press without a Release between them.
//! * **I3** (S2): after any loss event the key is not held, and a Release came
//!   in that same step if it was.
//! * **I4** (S3): after a forced release, and after any loss event while the
//!   button was down, there is no Press until a live `[release]`.
//! * **I5**: events tagged with a dead epoch change nothing; the same trace
//!   with them removed gives the same outputs.
//!
//! `Reference` is deliberately not machine.rs. It re-derives which epoch is
//! live, which gates are open and whether the button is latched from the
//! rules in the machine's module docs, so a mistake has to be made twice to
//! slip through. Its latch model matters for I1: a latch that arrives with
//! neither a Release intent nor a loss event (a `[01]` inside the settle
//! window, `Confirmed` or `Enable` while held) is invisible to I4, so only
//! I1's `!latched` gate catches a machine that forgets one. A machine that
//! latches too much (every loss, whatever the button's state) is safe and
//! passes all five; the transitions tests `idle_disconnect_first_press_keys`
//! and `lease_lost_with_button_up_does_not_latch` catch that one.
#![allow(clippy::too_many_lines)]

use std::time::Duration;

use astar_ble_ptt::{
    AdapterState, ButtonMachine, DEVICES, Intent, LinkEvent, MachineConfig, NotifyBytes, Output,
    ReleaseReason,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// The settle window, restated so the reference does not read it from the
/// code under test.
const SETTLE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Byte {
    Press,
    Release,
    Other,
}

/// Reads a B02PTT payload without the crate's decoder.
fn classify(bytes: &NotifyBytes) -> Byte {
    if bytes.len != 1 {
        return Byte::Other;
    }
    match bytes.bytes[0] {
        0x01 => Byte::Press,
        0x00 => Byte::Release,
        _ => Byte::Other,
    }
}

/// What the reference believes about the link, before each event.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default)]
struct Reference {
    enabled: bool,
    confirmed: bool,
    adapter_on: bool,
    max_epoch: u32,
    live: Option<u32>,
    gatt: bool,
    subscribed_at: Option<Duration>,
    /// The last live, subscribed notify was `[01]` and nothing has forced the
    /// button's state to unknown since.
    raw_pressed: bool,
    /// S3's latch, re-derived from the module-doc rules: set by a `[01]`
    /// inside the settle window and by a malformed payload; set by every loss
    /// (each go-down, `LeaseLost`, `Confirmed`, `Enable`, a repeat subscribe)
    /// only while `raw_pressed`; cleared only by a live, subscribed `[00]`.
    latched: bool,
}

impl Reference {
    /// A loss: the button's state is unknown from here on. Latches only if
    /// it was down; never clears a latch.
    fn force(&mut self) {
        if self.raw_pressed {
            self.latched = true;
        }
        self.raw_pressed = false;
    }

    fn is_live(&self, epoch: u32) -> bool {
        self.live == Some(epoch)
    }

    fn subscribed_live(&self, epoch: u32) -> bool {
        self.is_live(epoch) && self.subscribed_at.is_some()
    }

    /// S1: the only conditions under which a `[01]` may key.
    fn press_allowed(&self, epoch: u32, now: Duration) -> bool {
        self.enabled
            && self.confirmed
            && self.adapter_on
            && self.is_live(epoch)
            && self.gatt
            && self.subscribed_at.is_some_and(|at| now >= at + SETTLE)
            && !self.latched
    }

    /// S2: the events that must end a press in the step that reports them.
    fn is_loss(&self, event: LinkEvent) -> bool {
        match event {
            LinkEvent::Disable | LinkEvent::Sleep | LinkEvent::LeaseLost => true,
            LinkEvent::Adapter(state) => state != AdapterState::On,
            LinkEvent::Disconnected { epoch }
            | LinkEvent::Unsubscribed { epoch }
            | LinkEvent::GattRejected { epoch } => self.is_live(epoch),
            LinkEvent::Subscribed { epoch } => self.subscribed_live(epoch),
            LinkEvent::Connected { epoch } => {
                self.enabled && epoch > self.max_epoch && self.live.is_some()
            }
            LinkEvent::Notify { epoch, bytes } => {
                self.subscribed_live(epoch) && classify(&bytes) == Byte::Other
            }
            _ => false,
        }
    }

    fn apply(&mut self, event: LinkEvent, now: Duration, out: &Output) {
        match event {
            LinkEvent::Enable { confirmed } => {
                // Re-enabling makes a held button's state unknown.
                self.force();
                self.enabled = true;
                self.confirmed = confirmed;
            }
            LinkEvent::Disable => {
                self.force();
                self.enabled = false;
                self.live = None;
            }
            LinkEvent::Confirmed => {
                self.confirmed = true;
                // Held through the confirm press: it must come up first.
                if self.raw_pressed {
                    self.latched = true;
                }
            }
            LinkEvent::LeaseLost => {
                // The station dropped the key: latch if the button is down.
                // The button's state is still known, so it is kept.
                if self.raw_pressed {
                    self.latched = true;
                }
            }
            LinkEvent::Adapter(state) => {
                self.adapter_on = state == AdapterState::On;
                if !self.adapter_on {
                    self.force();
                    self.live = None;
                }
            }
            LinkEvent::Connected { epoch } => {
                if epoch > self.max_epoch {
                    // Every epoch seen counts, adopted or not.
                    self.max_epoch = epoch;
                    if self.enabled {
                        if self.live.is_some() {
                            // Replacing a live link is a loss.
                            self.force();
                        }
                        self.live = Some(epoch);
                        self.gatt = false;
                        self.subscribed_at = None;
                    }
                }
            }
            LinkEvent::GattAccepted { epoch } => {
                if self.is_live(epoch) {
                    self.gatt = true;
                }
            }
            LinkEvent::GattRejected { epoch }
            | LinkEvent::Unsubscribed { epoch }
            | LinkEvent::Disconnected { epoch } => {
                if self.is_live(epoch) {
                    self.force();
                    self.live = None;
                }
            }
            LinkEvent::Subscribed { epoch } => {
                if self.is_live(epoch) && self.gatt {
                    // A (re)subscribe makes the held state unknown again.
                    self.force();
                    self.subscribed_at = Some(now);
                }
            }
            LinkEvent::Notify { epoch, bytes } => {
                if self.subscribed_live(epoch) {
                    match classify(&bytes) {
                        Byte::Press => {
                            self.raw_pressed = true;
                            if self.subscribed_at.is_some_and(|at| now < at + SETTLE) {
                                self.latched = true;
                            }
                        }
                        Byte::Release => {
                            self.raw_pressed = false;
                            self.latched = false;
                        }
                        Byte::Other => {
                            self.raw_pressed = false;
                            self.latched = true;
                        }
                    }
                }
            }
            LinkEvent::Sleep => {
                self.force();
                self.live = None;
            }
            _ => {}
        }
        // Two missed probes: the machine declares the link dead and the
        // transport drops it. (A GattRejected also sets drop_link; forcing
        // twice is harmless: the first force already cleared raw_pressed.)
        if out.cmds.drop_link {
            self.force();
            self.live = None;
        }
    }
}

struct Step {
    event: LinkEvent,
    now: Duration,
    before: Reference,
    prev: Output,
    out: Output,
}

fn walk(
    trace: &[(LinkEvent, Duration)],
    check: &mut dyn FnMut(&Step) -> Result<(), TestCaseError>,
) -> Result<(), TestCaseError> {
    let mut machine = ButtonMachine::new(&DEVICES[0], MachineConfig::DEFAULT);
    let mut reference = Reference::default();
    for &(event, now) in trace {
        let prev = machine.output();
        let out = machine.handle(event, now);
        check(&Step {
            event,
            now,
            before: reference,
            prev,
            out,
        })?;
        reference.apply(event, now, &out);
    }
    Ok(())
}

fn payload() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => Just(vec![0x01]),
        4 => Just(vec![0x00]),
        1 => Just(vec![]),
        1 => Just(vec![0x02]),
        1 => Just(vec![0x01, 0x00]),
        1 => Just(vec![0x01; 9]),
    ]
}

fn adapter() -> impl Strategy<Value = AdapterState> {
    prop_oneof![
        4 => Just(AdapterState::On),
        1 => Just(AdapterState::Off),
        1 => Just(AdapterState::Unauthorized),
        1 => Just(AdapterState::Resetting),
        1 => Just(AdapterState::Unsupported),
    ]
}

fn event() -> impl Strategy<Value = LinkEvent> {
    prop_oneof![
        1 => any::<bool>().prop_map(|confirmed| LinkEvent::Enable { confirmed }),
        1 => Just(LinkEvent::Disable),
        1 => Just(LinkEvent::Confirmed),
        2 => adapter().prop_map(LinkEvent::Adapter),
        2 => (0u32..=4).prop_map(|epoch| LinkEvent::Connected { epoch }),
        2 => (0u32..=4).prop_map(|epoch| LinkEvent::GattAccepted { epoch }),
        1 => (0u32..=4).prop_map(|epoch| LinkEvent::GattRejected { epoch }),
        2 => (0u32..=4).prop_map(|epoch| LinkEvent::Subscribed { epoch }),
        1 => (0u32..=4).prop_map(|epoch| LinkEvent::Unsubscribed { epoch }),
        10 => (0u32..=4, payload()).prop_map(|(epoch, p)| LinkEvent::Notify {
            epoch,
            bytes: NotifyBytes::from_slice(&p),
        }),
        2 => (0u32..=4, -100i16..=-40).prop_map(|(epoch, rssi)| LinkEvent::ProbeOk { epoch, rssi }),
        2 => (0u32..=4).prop_map(|epoch| LinkEvent::ProbeFailed { epoch }),
        1 => (0u32..=4).prop_map(|epoch| LinkEvent::Disconnected { epoch }),
        1 => Just(LinkEvent::Sleep),
        1 => Just(LinkEvent::Wake),
        1 => Just(LinkEvent::LeaseLost),
        5 => Just(LinkEvent::Tick),
    ]
}

/// Gaps that land on both sides of the 300 ms, 500 ms and 5 s boundaries.
fn gap_ms() -> impl Strategy<Value = u64> {
    prop_oneof![0u64..=60, 240u64..=560, 4_900u64..=5_100]
}

/// A linked, confirmed, subscribed start, then random events.
fn traces() -> impl Strategy<Value = Vec<(LinkEvent, Duration)>> {
    prop::collection::vec((event(), gap_ms()), 0..80).prop_map(|steps| {
        let mut trace = vec![
            (LinkEvent::Enable { confirmed: true }, Duration::ZERO),
            (
                LinkEvent::Adapter(AdapterState::On),
                Duration::from_millis(1),
            ),
            (LinkEvent::Connected { epoch: 1 }, Duration::from_millis(10)),
            (
                LinkEvent::GattAccepted { epoch: 1 },
                Duration::from_millis(20),
            ),
            (
                LinkEvent::Subscribed { epoch: 1 },
                Duration::from_millis(30),
            ),
        ];
        let mut now = Duration::from_millis(30);
        for (event, gap) in steps {
            now += Duration::from_millis(gap);
            trace.push((event, now));
        }
        trace
    })
}

/// An epoch-tagged event that is never `Connected`.
fn stale_event(kind: u8, epoch: u32, payload: &[u8]) -> LinkEvent {
    match kind {
        0 => LinkEvent::GattAccepted { epoch },
        1 => LinkEvent::GattRejected { epoch },
        2 => LinkEvent::Subscribed { epoch },
        3 => LinkEvent::Unsubscribed { epoch },
        4 => LinkEvent::Notify {
            epoch,
            bytes: NotifyBytes::from_slice(payload),
        },
        5 => LinkEvent::ProbeOk { epoch, rssi: -50 },
        6 => LinkEvent::ProbeFailed { epoch },
        _ => LinkEvent::Disconnected { epoch },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn i1_every_press_passes_the_reference_gate(trace in traces()) {
        walk(&trace, &mut |s: &Step| {
            if s.out.intent == Intent::Press {
                let LinkEvent::Notify { epoch, bytes } = s.event else {
                    return Err(TestCaseError::fail(format!("Press from {:?}", s.event)));
                };
                prop_assert_eq!(classify(&bytes), Byte::Press);
                prop_assert!(
                    s.before.press_allowed(epoch, s.now),
                    "gate closed for {:?} at {:?}: {:?}", s.event, s.now, s.before
                );
                prop_assert!(!s.prev.pressed);
            }
            Ok(())
        })?;
    }

    #[test]
    fn i2_never_two_presses_without_a_release(trace in traces()) {
        let mut open = false;
        walk(&trace, &mut |s: &Step| {
            match s.out.intent {
                Intent::Press => {
                    prop_assert!(!open, "second Press at {:?}", s.now);
                    open = true;
                }
                Intent::Release(_) => open = false,
                Intent::None => {}
            }
            Ok(())
        })?;
    }

    #[test]
    fn i3_every_loss_unkeys_in_the_same_step(trace in traces()) {
        walk(&trace, &mut |s: &Step| {
            if s.before.is_loss(s.event) {
                prop_assert!(!s.out.pressed, "{:?} left the key held", s.event);
                if s.event == LinkEvent::LeaseLost {
                    prop_assert_eq!(s.out.intent, Intent::None);
                } else if s.prev.pressed {
                    prop_assert!(
                        matches!(s.out.intent, Intent::Release(_)),
                        "{:?} while pressed gave {:?}", s.event, s.out.intent
                    );
                }
            }
            Ok(())
        })?;
    }

    #[test]
    fn i4_no_press_after_a_forced_release_until_a_live_release(trace in traces()) {
        let mut needs_release = false;
        walk(&trace, &mut |s: &Step| {
            if let LinkEvent::Notify { epoch, bytes } = s.event
                && s.before.subscribed_live(epoch)
                && classify(&bytes) == Byte::Release
            {
                needs_release = false;
            }
            if needs_release {
                prop_assert_ne!(s.out.intent, Intent::Press, "re-keyed at {:?}", s.now);
            }
            // Every loss latches a button that was down, and only that one.
            // A malformed payload always releases with `Malformed`, so the
            // first arm covers it whatever the button's state.
            let forced = matches!(s.out.intent, Intent::Release(r) if r != ReleaseReason::Button)
                || (s.before.is_loss(s.event) && s.before.raw_pressed);
            if forced {
                needs_release = true;
            }
            Ok(())
        })?;
    }

    #[test]
    fn i5_stale_epoch_events_change_nothing(
        trace in traces(),
        inserts in prop::collection::vec((0usize..90, 0u8..8, 0u32..=4, payload()), 0..20),
    ) {
        // Pass 1: the trace alone, remembering the live epoch before each step.
        let mut alone = Vec::with_capacity(trace.len());
        let mut live_before = Vec::with_capacity(trace.len());
        walk(&trace, &mut |s: &Step| {
            alone.push(s.out);
            live_before.push(s.before.live);
            Ok(())
        })?;

        // Pass 2: the same trace with dead-epoch events slipped in before chosen steps.
        let mut machine = ButtonMachine::new(&DEVICES[0], MachineConfig::DEFAULT);
        for (i, &(event, now)) in trace.iter().enumerate() {
            for (_, kind, candidate, payload) in inserts.iter().filter(|ins| ins.0 == i) {
                let epoch = if live_before[i] == Some(*candidate) { 0 } else { *candidate };
                let before = machine.output();
                let out = machine.handle(stale_event(*kind, epoch, payload), now);
                prop_assert_eq!(out, before, "stale kind {} epoch {} changed the machine", kind, epoch);
            }
            prop_assert_eq!(machine.handle(event, now), alone[i]);
        }
    }
}
