// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The [`ButtonMachine`]: every decision about whether a Bluetooth button may
//! key, as a pure function of link events and caller-supplied time.
//!
//! # Why it is pure
//!
//! A wireless button is the one keying source that can vanish mid-over. The
//! rules below must hold however it vanishes, and they are only worth
//! something if tests prove them. So they live here, with no adapter, no
//! clock and no threads. The macOS transport turns `CoreBluetooth` callbacks
//! into [`LinkEvent`]s and carries out [`Cmds`]; it decides nothing. The
//! traces in `scenarios/` drive this machine from Rust and, through
//! `astar-sys`, from Swift.
//!
//! The machine has no transmit time-out, no lease and no call-end logic. Those
//! live in the station's PTT arbiter, enforced by the engine's watchdog, so
//! there is one place to get them right.
//!
//! # Invariants
//!
//! * **S1, no phantom key.** [`Intent::Press`] comes only from a `[press]`
//!   notify on the live epoch, after subscribe and past the settle window,
//!   while enabled, confirmed on this Mac, with the adapter on and the machine
//!   neither latched nor already pressed.
//! * **S2, every loss unkeys in the same step.** A release byte, a malformed
//!   payload, disconnect, unsubscribe, the adapter leaving On, disable, sleep,
//!   a lost lease and two missed RSSI probes each end the press in the step
//!   that reports them.
//! * **S3, release-to-re-arm.** A loss while the button is down latches, and
//!   only a live `[release]` clears the latch. A reconnect never does, and a
//!   `[press]` inside the settle window after subscribe latches too, so coming
//!   back never keys by itself.
//! * **S8, nothing writes to the device.** [`Cmds`] holds an RSSI read and a
//!   link drop, and nothing else.
//!
//! # Transitions
//!
//! | Event | Condition | Effect |
//! |---|---|---|
//! | `Notify [press]` | live, subscribed, past settle, enabled, confirmed, adapter On, not latched, not pressed | pressed; `Press` |
//! | `Notify [press]` | inside the settle window | latched |
//! | `Notify [press]` | latched, pressed or unconfirmed | nothing (`raw_pressed` still set, for the self-test) |
//! | `Notify [release]` | live | latch and press cleared; `Release(Button)` every time |
//! | `Notify` anything else | live | latched; `Release(Malformed)` |
//! | any epoch-tagged event | not the live epoch, or link down | dropped |
//! | `Connected` | enabled, epoch newer than every epoch seen | adopted; a live link it replaces goes down (latched if the button was down; `Release(LinkLost)` if pressed) |
//! | `Disconnected` / `Unsubscribed` | live | link down; latched if the button was down; `Release(LinkLost)` if pressed |
//! | `GattRejected` | live | link down; latched if the button was down; `NotSupportedDevice`; cmd `drop_link` |
//! | `Subscribed` again | live | latched if the button was down; `Release(LinkLost)` if pressed; the settle window restarts |
//! | `Adapter` not On | — | link down; latched if the button was down; `Release(AdapterOff)` if pressed |
//! | `Disable` / `Sleep` | — | link down; latched if the button was down; `Release(Disabled)` / `Release(Sleep)` if pressed |
//! | `Enable` | — | latched if the button was down; `Release(Disabled)` if pressed |
//! | `Confirmed` | — | latched if the button was down |
//! | `LeaseLost` | — | press dropped, latched if the button was down, no intent (the station already unkeyed) |
//! | `Tick` | pressed, RSSI liveness, 500 ms since the last probe | cmd `probe`; one still unanswered counts a miss |
//! | `Tick` / `ProbeFailed` | pressed, second consecutive miss | link down; latched; `Release(LinkLost)`; cmd `drop_link` |
//! | `ProbeOk` | live | misses reset; signal band updated |
//! | `Tick` | linked, not pressed, 5 s since the last probe | cmd `probe` (status only; idle misses never release) |
//! | `Wake` | — | nothing; the next `Subscribed` restarts the settle window |
//!
//! "The button was down" means `raw_pressed`: the last live notify was
//! `[press]`. Every loss latches on exactly that condition and never clears a
//! latch already set. A loss with the button up arms nothing, so after an idle
//! drop and relink the status is `Ready` and the first press past the settle
//! window keys. A loss with the button down latches, and the button must
//! report `[release]` on a live link before a press keys again. The device
//! sends nothing on subscribe, so a button held across the gap stays silent
//! until it is let go, and its `[release]` keys nothing.

use core::time::Duration;

use crate::devices::{BleButtonDevice, Decode, Decoded, Liveness, decode};

/// Identifies one connect attempt. The client allocates a strictly increasing
/// value per attempt and never uses 0, so an event from a dead link can always
/// be told apart from the live one and dropped.
pub type Epoch = u32;

/// The adapter's state as the platform reports it. Anything but `On` drops the
/// link: no notification can be trusted without a working adapter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdapterState {
    /// Powered on and usable.
    On,
    /// Powered off.
    Off,
    /// The app is not allowed to use Bluetooth.
    Unauthorized,
    /// Restarting; treated as off until it reports again.
    Resetting,
    /// No usable LE adapter.
    Unsupported,
}

/// A notify payload copied into a fixed buffer, so a [`LinkEvent`] stays
/// `Copy` and crosses the C ABI without an allocation.
///
/// `len` is the payload's TRUE length. A payload longer than the buffer keeps
/// its length, so it is still seen and decodes as [`Decoded::Malformed`]
/// instead of being truncated into something that looks valid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotifyBytes {
    /// The payload's length as received; it may exceed `bytes.len()`.
    pub len: usize,
    /// The first eight bytes; the rest are zero.
    pub bytes: [u8; 8],
}

impl NotifyBytes {
    /// Copies up to eight bytes and records the full length.
    #[must_use]
    pub fn from_slice(payload: &[u8]) -> NotifyBytes {
        let mut bytes = [0u8; 8];
        let n = payload.len().min(bytes.len());
        bytes[..n].copy_from_slice(&payload[..n]);
        NotifyBytes {
            len: payload.len(),
            bytes,
        }
    }

    /// Malformed whenever `len > 8`; otherwise `decode(rule, &bytes[..len])`.
    #[must_use]
    pub fn decoded(&self, rule: Decode) -> Decoded {
        match self.bytes.get(..self.len) {
            Some(payload) => decode(rule, payload),
            None => Decoded::Malformed,
        }
    }
}

/// Everything the transport reports, in the order it happened. Every event
/// about a link carries that link's [`Epoch`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkEvent {
    /// The config's PTT hardware is this device. (Re)sets whether the unit is
    /// confirmed on this Mac.
    Enable {
        /// The unit's identifier is bound to a confirm press on this Mac.
        confirmed: bool,
    },
    /// The device is no longer the config's PTT hardware.
    Disable,
    /// The user's confirm press bound this unit to this Mac.
    Confirmed,
    /// The adapter changed state.
    Adapter(AdapterState),
    /// A connection completed. Adopted only while enabled, and only if
    /// `epoch` is newer than every epoch seen so far, adopted or not: the
    /// client's epochs only grow, so an older one is always a dead attempt.
    Connected {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// Service discovery found the button service with a NOTIFY characteristic.
    GattAccepted {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// Service discovery did not find it: a lookalike.
    GattRejected {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// Notifications are enabled (the CCCD write completed).
    Subscribed {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// Notifications stopped.
    Unsubscribed {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// A notification arrived on the button characteristic.
    Notify {
        /// The connect attempt.
        epoch: Epoch,
        /// The payload.
        bytes: NotifyBytes,
    },
    /// An RSSI read answered.
    ProbeOk {
        /// The connect attempt.
        epoch: Epoch,
        /// Signal strength in `dBm`.
        rssi: i16,
    },
    /// An RSSI read failed.
    ProbeFailed {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// The link dropped.
    Disconnected {
        /// The connect attempt.
        epoch: Epoch,
    },
    /// The computer is going to sleep; the transport cancels the link.
    Sleep,
    /// The computer woke; the transport reconnects.
    Wake,
    /// The station refused the press or a renewal: it has already released or
    /// latched the Bluetooth source.
    LeaseLost,
    /// The client's periodic timer (every `renew_period`).
    Tick,
}

/// Why the machine released. The numbering is fixed: it crosses the C ABI.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReleaseReason {
    /// The button was let go (a live `[release]`).
    Button = 1,
    /// A payload that is neither press nor release.
    Malformed = 2,
    /// Disconnect, unsubscribe, a rejected or replaced link, a resubscribe, or
    /// two missed RSSI probes.
    LinkLost = 3,
    /// The adapter left the On state.
    AdapterOff = 4,
    /// The device was disabled or re-enabled.
    Disabled = 5,
    /// The computer went to sleep.
    Sleep = 6,
}

/// What the caller must now tell the station.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    /// Nothing to do.
    None,
    /// Lease-press the Bluetooth source.
    Press,
    /// Release the Bluetooth source. Never refused, so it is always sent.
    Release(ReleaseReason),
}

/// The only meanings the UI may show for the link. The numbering is fixed: it
/// crosses the C ABI.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkStatus {
    /// Not enabled.
    Disabled = 0,
    /// The adapter is off or resetting.
    BluetoothOff = 1,
    /// The app is not allowed to use Bluetooth.
    Unauthorized = 2,
    /// No usable LE adapter.
    Unsupported = 3,
    /// Enabled, never linked in this enable cycle.
    Searching = 4,
    /// Linked, not yet subscribed.
    Connecting = 5,
    /// The last peer did not expose the button service.
    NotSupportedDevice = 6,
    /// Subscribed, but this unit is not confirmed on this Mac.
    NeedsConfirm = 7,
    /// Subscribed and confirmed: a press keys.
    Ready = 8,
    /// Subscribed and confirmed, but latched: release the button first.
    ReleaseToArm = 9,
    /// The link was up in this enable cycle and is down now.
    Reconnecting = 10,
}

/// Signal strength for the status chip only. It never affects keying.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignalBand {
    /// At or above `band_strong_dbm`.
    Strong = 1,
    /// At or above `band_fair_dbm`, below `band_strong_dbm`.
    Fair = 2,
    /// Below `band_fair_dbm`.
    Weak = 3,
}

/// What the transport must do after this step. Nothing else can ever be
/// asked of it: there is no write command (S8).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cmds {
    /// Read the link's RSSI (`CBPeripheral.readRSSI`), and answer with
    /// `ProbeOk` or `ProbeFailed`.
    pub probe: bool,
    /// Cancel the connection: the link is judged dead or is a lookalike.
    pub drop_link: bool,
}

/// The machine's answer to one event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Output {
    /// What to tell the station.
    pub intent: Intent,
    /// What the transport must do.
    pub cmds: Cmds,
    /// What the UI shows.
    pub status: LinkStatus,
    /// Signal band from the last RSSI reply on the live link.
    pub signal: Option<SignalBand>,
    /// The last RSSI reply on the live link, in `dBm`.
    pub last_rssi: Option<i16>,
    /// The last live notify was `[press]`, whatever the gates said. Drives the
    /// self-test and the confirm press; never keys.
    pub raw_pressed: bool,
    /// The machine holds the key. The client renews the station's lease while
    /// this is true.
    pub pressed: bool,
}

/// The machine's timings and thresholds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachineConfig {
    /// A `[press]` this soon after subscribing latches instead of keying: some
    /// firmware reports its current state when notifications are enabled.
    pub settle_after_subscribe: Duration,
    /// RSSI probe period while pressed.
    pub probe_interval_keyed: Duration,
    /// Consecutive missed probes that mean the link is dead.
    pub probe_misses_dead: u8,
    /// RSSI probe period while linked and idle (status chip only).
    pub probe_interval_idle: Duration,
    /// The lease the client asks the station for on a press.
    pub lease_ttl: Duration,
    /// How often the client ticks the machine and renews the lease.
    pub renew_period: Duration,
    /// RSSI at or above this (`dBm`) is `Strong`.
    pub band_strong_dbm: i16,
    /// RSSI at or above this (`dBm`), and below `band_strong_dbm`, is `Fair`; below is `Weak`.
    pub band_fair_dbm: i16,
}

impl MachineConfig {
    /// The pinned defaults. The hardware listen-only check may retune the
    /// settle window and the bands; a test pins each value so any change is
    /// deliberate.
    pub const DEFAULT: MachineConfig = MachineConfig {
        settle_after_subscribe: Duration::from_millis(300),
        probe_interval_keyed: Duration::from_millis(500),
        probe_misses_dead: 2,
        probe_interval_idle: Duration::from_secs(5),
        lease_ttl: Duration::from_millis(1500),
        renew_period: Duration::from_millis(250),
        band_strong_dbm: -65,
        band_fair_dbm: -80,
    };
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The link as the machine sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    Down,
    Up {
        epoch: Epoch,
        gatt: bool,
        subscribed_at: Option<Duration>,
    },
}

/// Turns [`LinkEvent`]s into press and release intents for one device.
///
/// Feed it every event in order, with `now` from one monotonic clock. Act on
/// every [`Output`]: send its intent to the station and its commands to the
/// transport.
// Each flag is an independent fact about the link or the button; folding them
// into an enum would hide which combinations the tests cover.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct ButtonMachine {
    device: &'static BleButtonDevice,
    config: MachineConfig,
    enabled: bool,
    confirmed: bool,
    adapter: Option<AdapterState>,
    link: Link,
    max_epoch: Epoch,
    ever_linked: bool,
    rejected: bool,
    pressed: bool,
    latched: bool,
    raw_pressed: bool,
    probe_sent_at: Option<Duration>,
    last_probe_at: Option<Duration>,
    missed: u8,
    last_rssi: Option<i16>,
}

impl ButtonMachine {
    /// A disabled machine for `device`. The adapter state starts unknown,
    /// which counts as not yet reported, never as off.
    #[must_use]
    pub fn new(device: &'static BleButtonDevice, config: MachineConfig) -> ButtonMachine {
        ButtonMachine {
            device,
            config,
            enabled: false,
            confirmed: false,
            adapter: None,
            link: Link::Down,
            max_epoch: 0,
            ever_linked: false,
            rejected: false,
            pressed: false,
            latched: false,
            raw_pressed: false,
            probe_sent_at: None,
            last_probe_at: None,
            missed: 0,
            last_rssi: None,
        }
    }

    /// Applies one event. `now` is a monotonic offset from an arbitrary
    /// origin, supplied by the caller; the machine never reads a clock.
    #[must_use = "the intent must reach the station and the commands the transport"]
    pub fn handle(&mut self, event: LinkEvent, now: Duration) -> Output {
        let mut cmds = Cmds::default();
        let intent = match event {
            LinkEvent::Enable { confirmed } => self.on_enable(confirmed),
            LinkEvent::Disable => {
                self.enabled = false;
                self.go_down(ReleaseReason::Disabled)
            }
            LinkEvent::Confirmed => {
                self.confirmed = true;
                self.latch_if_button_down();
                Intent::None
            }
            LinkEvent::Adapter(state) => {
                self.adapter = Some(state);
                if state == AdapterState::On {
                    Intent::None
                } else {
                    self.go_down(ReleaseReason::AdapterOff)
                }
            }
            LinkEvent::Connected { epoch } => self.on_connected(epoch),
            LinkEvent::GattAccepted { epoch } => {
                if let Link::Up {
                    epoch: live, gatt, ..
                } = &mut self.link
                    && *live == epoch
                {
                    *gatt = true;
                }
                Intent::None
            }
            LinkEvent::GattRejected { epoch } => self.on_gatt_rejected(epoch, &mut cmds),
            LinkEvent::Subscribed { epoch } => self.on_subscribed(epoch, now),
            LinkEvent::Unsubscribed { epoch } | LinkEvent::Disconnected { epoch } => {
                if self.is_live(epoch) {
                    self.go_down(ReleaseReason::LinkLost)
                } else {
                    Intent::None
                }
            }
            LinkEvent::Notify { epoch, bytes } => self.on_notify(epoch, bytes, now),
            LinkEvent::ProbeOk { epoch, rssi } => {
                if self.is_live(epoch) {
                    self.missed = 0;
                    self.probe_sent_at = None;
                    self.last_rssi = Some(rssi);
                }
                Intent::None
            }
            LinkEvent::ProbeFailed { epoch } => self.on_probe_failed(epoch, &mut cmds),
            LinkEvent::Sleep => self.go_down(ReleaseReason::Sleep),
            LinkEvent::Wake => Intent::None,
            LinkEvent::LeaseLost => {
                self.latch_if_button_down();
                self.pressed = false;
                Intent::None
            }
            LinkEvent::Tick => self.on_tick(now, &mut cmds),
        };
        self.snapshot(intent, cmds)
    }

    /// The current state, with `intent == Intent::None` and no commands.
    #[must_use]
    pub fn output(&self) -> Output {
        self.snapshot(Intent::None, Cmds::default())
    }

    /// The timings this machine runs on; the client reads `lease_ttl` and
    /// `renew_period` from here.
    #[must_use]
    pub fn config(&self) -> &MachineConfig {
        &self.config
    }

    fn snapshot(&self, intent: Intent, cmds: Cmds) -> Output {
        Output {
            intent,
            cmds,
            status: self.status(),
            signal: self.signal(),
            last_rssi: self.last_rssi,
            raw_pressed: self.raw_pressed,
            pressed: self.pressed,
        }
    }

    fn is_live(&self, epoch: Epoch) -> bool {
        matches!(self.link, Link::Up { epoch: live, .. } if live == epoch)
    }

    /// The button is believed down: it must come up before it may key again.
    fn latch_if_button_down(&mut self) {
        if self.raw_pressed {
            self.latched = true;
        }
    }

    /// Ends any press and forgets the button's state, without latching.
    fn end_press(&mut self, reason: ReleaseReason) -> Intent {
        let was_pressed = self.pressed;
        self.pressed = false;
        self.raw_pressed = false;
        if was_pressed {
            Intent::Release(reason)
        } else {
            Intent::None
        }
    }

    /// A loss: ends any press, latching only if the button was down. A latch
    /// already set stays set; only a live `[release]` clears it.
    fn force(&mut self, reason: ReleaseReason) -> Intent {
        self.latch_if_button_down();
        self.end_press(reason)
    }

    /// A loss that also forgets the link: every later event tagged with its
    /// epoch is dropped.
    fn go_down(&mut self, reason: ReleaseReason) -> Intent {
        let intent = self.force(reason);
        self.link = Link::Down;
        self.probe_sent_at = None;
        self.last_probe_at = None;
        self.missed = 0;
        self.last_rssi = None;
        intent
    }

    fn on_enable(&mut self, confirmed: bool) -> Intent {
        // Re-enabling resets the confirmation: a key held across it must not survive.
        let intent = self.force(ReleaseReason::Disabled);
        if !self.enabled {
            self.ever_linked = false;
        }
        self.enabled = true;
        self.confirmed = confirmed;
        self.rejected = false;
        intent
    }

    fn on_connected(&mut self, epoch: Epoch) -> Intent {
        if epoch <= self.max_epoch {
            return Intent::None;
        }
        // Recorded even while disabled: an epoch seen once is never new again.
        self.max_epoch = epoch;
        if !self.enabled {
            return Intent::None;
        }
        // A new connection replaces any live one, which is a loss like any
        // other. With no live link there is nothing to lose.
        let intent = if matches!(self.link, Link::Up { .. }) {
            self.go_down(ReleaseReason::LinkLost)
        } else {
            Intent::None
        };
        self.link = Link::Up {
            epoch,
            gatt: false,
            subscribed_at: None,
        };
        self.ever_linked = true;
        self.rejected = false;
        intent
    }

    fn on_gatt_rejected(&mut self, epoch: Epoch, cmds: &mut Cmds) -> Intent {
        if !self.is_live(epoch) {
            return Intent::None;
        }
        self.rejected = true;
        cmds.drop_link = true;
        self.go_down(ReleaseReason::LinkLost)
    }

    fn on_subscribed(&mut self, epoch: Epoch, now: Duration) -> Intent {
        let Link::Up {
            epoch: live,
            gatt: true,
            ..
        } = self.link
        else {
            return Intent::None;
        };
        if live != epoch {
            return Intent::None;
        }
        // A repeat subscribe means the device re-sent its CCCD state: what it
        // held is unknown again, so release and restart the settle window.
        let intent = self.force(ReleaseReason::LinkLost);
        self.link = Link::Up {
            epoch,
            gatt: true,
            subscribed_at: Some(now),
        };
        self.probe_sent_at = None;
        self.last_probe_at = None;
        self.missed = 0;
        intent
    }

    fn on_notify(&mut self, epoch: Epoch, bytes: NotifyBytes, now: Duration) -> Intent {
        let Link::Up {
            epoch: live,
            subscribed_at: Some(subscribed_at),
            ..
        } = self.link
        else {
            return Intent::None;
        };
        if live != epoch {
            return Intent::None;
        }
        match bytes.decoded(self.device.decode) {
            Decoded::Press => self.on_press_byte(now.saturating_sub(subscribed_at), now),
            Decoded::Release => {
                self.latched = false;
                self.raw_pressed = false;
                self.pressed = false;
                Intent::Release(ReleaseReason::Button)
            }
            Decoded::Malformed => {
                self.latched = true;
                self.raw_pressed = false;
                self.pressed = false;
                Intent::Release(ReleaseReason::Malformed)
            }
        }
    }

    fn on_press_byte(&mut self, since_subscribe: Duration, now: Duration) -> Intent {
        self.raw_pressed = true;
        if since_subscribe < self.config.settle_after_subscribe {
            self.latched = true;
            return Intent::None;
        }
        if !self.may_press() {
            return Intent::None;
        }
        self.pressed = true;
        self.missed = 0;
        self.probe_sent_at = None;
        self.last_probe_at = Some(now);
        Intent::Press
    }

    fn may_press(&self) -> bool {
        self.enabled
            && self.confirmed
            && self.adapter == Some(AdapterState::On)
            && !self.latched
            && !self.pressed
    }

    fn on_probe_failed(&mut self, epoch: Epoch, cmds: &mut Cmds) -> Intent {
        if !self.is_live(epoch) {
            return Intent::None;
        }
        self.probe_sent_at = None;
        // Idle misses only affect the status chip; they never change keying.
        if self.pressed {
            self.count_miss(cmds)
        } else {
            Intent::None
        }
    }

    fn count_miss(&mut self, cmds: &mut Cmds) -> Intent {
        self.missed = self.missed.saturating_add(1);
        if self.missed < self.config.probe_misses_dead {
            return Intent::None;
        }
        cmds.drop_link = true;
        self.go_down(ReleaseReason::LinkLost)
    }

    fn on_tick(&mut self, now: Duration, cmds: &mut Cmds) -> Intent {
        let subscribed = matches!(
            self.link,
            Link::Up {
                subscribed_at: Some(_),
                ..
            }
        );
        if !subscribed || self.device.liveness != Liveness::Rssi {
            return Intent::None;
        }
        let interval = if self.pressed {
            self.config.probe_interval_keyed
        } else {
            self.config.probe_interval_idle
        };
        if self
            .last_probe_at
            .is_some_and(|at| now.saturating_sub(at) < interval)
        {
            return Intent::None;
        }
        if self.pressed && self.probe_sent_at.is_some() {
            let intent = self.count_miss(cmds);
            if cmds.drop_link {
                return intent;
            }
        }
        cmds.probe = true;
        self.probe_sent_at = Some(now);
        self.last_probe_at = Some(now);
        Intent::None
    }

    fn status(&self) -> LinkStatus {
        if !self.enabled {
            return LinkStatus::Disabled;
        }
        match self.adapter {
            Some(AdapterState::Off | AdapterState::Resetting) => return LinkStatus::BluetoothOff,
            Some(AdapterState::Unauthorized) => return LinkStatus::Unauthorized,
            Some(AdapterState::Unsupported) => return LinkStatus::Unsupported,
            Some(AdapterState::On) | None => {}
        }
        match self.link {
            Link::Down if self.rejected => LinkStatus::NotSupportedDevice,
            Link::Down if self.ever_linked => LinkStatus::Reconnecting,
            Link::Down => LinkStatus::Searching,
            Link::Up {
                subscribed_at: None,
                ..
            } => LinkStatus::Connecting,
            Link::Up { .. } if !self.confirmed => LinkStatus::NeedsConfirm,
            Link::Up { .. } if self.latched => LinkStatus::ReleaseToArm,
            Link::Up { .. } => LinkStatus::Ready,
        }
    }

    fn signal(&self) -> Option<SignalBand> {
        let rssi = self.last_rssi?;
        Some(if rssi >= self.config.band_strong_dbm {
            SignalBand::Strong
        } else if rssi >= self.config.band_fair_dbm {
            SignalBand::Fair
        } else {
            SignalBand::Weak
        })
    }
}
