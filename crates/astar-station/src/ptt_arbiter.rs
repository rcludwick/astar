// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The PTT arbiter: the one place that decides whether the station is on air
//! when several sources can key it — the on-screen button and Space, VOX, a
//! USB radio interface, a Bluetooth LE button, and `Station::set_ptt` callers.
//!
//! It is pure. It owns no lock, no thread and no clock: every method that
//! needs the time takes `now`. That lets the tests drive the lease and the
//! transmit time-out to the millisecond, and lets `Station` enforce both from
//! its own watchdog thread, independent of any UI thread.
//!
//! The rules, each pinned by a test in `tests/ptt_arbiter.rs`:
//!
//! 1. On air is the OR of the held, unlatched sources. Only the OR's edges
//!    reach the console, so one source letting go never cuts off another.
//! 2. A press on a latched source is ignored. A press the console refuses is
//!    rolled back and latched, so a button held through dialing does not key
//!    when the call answers. `Legacy` is rolled back without a latch: a
//!    `set_ptt` caller (astar-server's `/key`) refused while no call is up
//!    keys on its next call without an `/unkey` first.
//! 3. A release clears the source's hold and its latch. It is idempotent and
//!    never refused: letting go must always work.
//! 4. `stop_all` releases every source and latches every held one, `Legacy`
//!    included, so a stuck wireless source cannot hold the radio past the
//!    universal stop.
//! 5. `call_ended`, and `observe_call` seeing the call go or change, release
//!    every source and latch every held one except `Legacy`, so a button held
//!    through a hangup never keys the next call. A held `Legacy` is released
//!    without a latch: the hang-up already ended its over, and the next
//!    call's `/key` keys.
//! 6. A leased source unkeys itself at its deadline unless renewed. Renew
//!    never presses: a client that lost track of the button can only make
//!    things quieter.
//! 7. One transmit time-out times continuous on-air time of the OR, from its
//!    rising edge, like a radio's TOT. Handing over between sources does not
//!    reset it; only the OR falling does. It has no off state.
//! 8. When it fires, everything stops and every held source is latched,
//!    `Legacy` included. Each re-arms only by releasing (`set_ptt(false)`
//!    for `Legacy`).

use std::time::{Duration, Instant};

/// The transmit time-out a new arbiter starts with. Two minutes, so a client
/// that never pushes the setting (the node daemon, the CLI, Python) is still
/// bounded.
pub const DEFAULT_TX_TIMEOUT: Duration = Duration::from_secs(120);

/// The shortest transmit time-out [`PttArbiter::set_tx_timeout`] accepts. One
/// minute is the lowest stop on the Mac's slider; zero in particular is
/// refused, so no stored or bridged value can ever mean "off".
pub const MIN_TX_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest transmit time-out accepted. Ten minutes keeps the bound on a
/// source that stops reporting short, whatever the setting.
pub const MAX_TX_TIMEOUT: Duration = Duration::from_secs(600);

/// Everything that can key the station. The discriminants are the C ABI's
/// `IAX_PTT_*` values and the bit positions in every [`PttSnapshot`] mask, so
/// they never change.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PttSource {
    /// The on-screen PTT button and Space together: one person's hand, so one
    /// source for keying and for latching.
    Manual = 0,
    /// The VOX gate. Its hang time is part of its hold.
    Vox = 1,
    /// A USB radio interface's PTT line.
    Serial = 2,
    /// A Bluetooth LE PTT button. The only source that keys through a lease,
    /// because it is the only one whose releases can be lost in the air.
    Bluetooth = 3,
    /// `Station::set_ptt` callers: the node daemon's `/key`, the CLI, Python
    /// and the Iced client.
    Legacy = 4,
}

impl PttSource {
    /// Every source in discriminant order, for building and walking masks.
    pub const ALL: [PttSource; 5] = [
        PttSource::Manual,
        PttSource::Vox,
        PttSource::Serial,
        PttSource::Bluetooth,
        PttSource::Legacy,
    ];

    /// This source's bit in a [`PttSnapshot`] mask: `1 << discriminant`.
    #[must_use]
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// The source with this C ABI value, or `None`. A binding passing an
    /// unknown value is refused, never mapped to a guess.
    #[must_use]
    pub fn from_raw(raw: u32) -> Option<PttSource> {
        match raw {
            0 => Some(PttSource::Manual),
            1 => Some(PttSource::Vox),
            2 => Some(PttSource::Serial),
            3 => Some(PttSource::Bluetooth),
            4 => Some(PttSource::Legacy),
            _ => None,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Why the arbiter forced sources off, for the UI's banner and announcement.
/// The values are the C ABI's `IAX_PTT_FORCED_*` (`0` there means none yet).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedReason {
    /// A leased source was not renewed before its deadline: its client went
    /// quiet, so the engine let go for it.
    LeaseExpired = 1,
    /// The radio was on air for the whole transmit time-out.
    TxTimeout = 2,
}

/// The last forced stop. `seq` increments once per forced stop so a poller
/// can tell a new one from the one it already showed; `sources` is the mask
/// it released. `stop_all`, `call_ended` and `legacy_off` are requests, not
/// forced stops, and never change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ForcedRecord {
    /// Forced stops so far (wrapping).
    pub seq: u32,
    /// Mask of the sources the last forced stop released.
    pub sources: u32,
    /// Why; `None` until the first forced stop.
    pub reason: Option<ForcedReason>,
}

/// What the caller must now tell `ConsoleSession::set_ptt`. Only OR edges
/// reach the console, so a caller forwards `Unkey` and nothing else. Rising
/// edges come back as [`PressOutcome::KeyNow`] instead, because a key can be
/// refused and must then be rolled back; `Key` is never returned by this
/// arbiter and exists so the edge type reads both ways.
#[must_use = "an Unkey edge must be forwarded to the console"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEdge {
    /// The OR did not change.
    None,
    /// The OR rose.
    Key,
    /// The OR fell: unkey the console now.
    Unkey,
}

/// The result of a press.
#[must_use = "KeyNow must key the console, and a failed key must be rolled back"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressOutcome {
    /// The source is latched, or it is `Bluetooth` pressed without a lease
    /// (it keys only through `lease_press`): nothing recorded, nothing keyed.
    Ignored,
    /// Recorded; the radio was already on air through another source (or this one).
    Held,
    /// Recorded and this raised the OR: the caller must key the console now,
    /// and call `rollback_press(src)` if that fails.
    KeyNow,
}

/// [`PttArbiter::lease_renew`] on a source that holds no live lease. Renew
/// never presses, so this is the answer for idle, released, latched,
/// plain-pressed and already-expired sources alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotLeased;

/// [`PttArbiter::set_tx_timeout`] outside 60..=600 s. The old value stays, so
/// a bad value can never leave the station without a time-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxTimeoutOutOfRange;

/// The arbiter's state for a snapshot, credential-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PttSnapshot {
    /// Mask of held sources.
    pub sources: u32,
    /// Mask of latched sources (each needs a release before it keys again).
    pub latched: u32,
    /// The last forced stop.
    pub forced: ForcedRecord,
    /// The transmit time-out in force.
    pub tx_timeout: Duration,
}

/// A held source; `lease` is set only through [`PttArbiter::lease_press`].
#[derive(Debug, Clone, Copy)]
struct Hold {
    lease: Option<Lease>,
}

#[derive(Debug, Clone, Copy)]
struct Lease {
    ttl: Duration,
    deadline: Instant,
}

/// The mask of every source for which `pred` holds. Bits are distinct, so the
/// sum is the OR.
fn mask_of(pred: impl Fn(PttSource) -> bool) -> u32 {
    PttSource::ALL
        .into_iter()
        .filter(|&src| pred(src))
        .map(PttSource::bit)
        .sum()
}

/// Combines every PTT source into one on-air decision. See the module docs
/// for the rules; `Station` owns one behind a mutex and forwards its edges.
#[derive(Debug)]
pub struct PttArbiter {
    held: [Option<Hold>; 5],
    latched: [bool; 5],
    on_air_since: Option<Instant>,
    tx_timeout: Duration,
    forced: ForcedRecord,
    last_answered_seq: Option<u64>,
}

impl PttArbiter {
    /// An idle arbiter with the default two-minute transmit time-out.
    #[must_use]
    pub fn new() -> PttArbiter {
        PttArbiter {
            held: [None; 5],
            latched: [false; 5],
            on_air_since: None,
            tx_timeout: DEFAULT_TX_TIMEOUT,
            forced: ForcedRecord {
                seq: 0,
                sources: 0,
                reason: None,
            },
            last_answered_seq: None,
        }
    }

    /// Press `src` with no lease. `Bluetooth` is `Ignored` here and changes
    /// nothing: Bluetooth keys only through `lease_press`, so the engine
    /// always holds its deadline and a stalled client is bounded by the
    /// 1.5 s lease, not by the 2-minute transmit time-out. Every other source
    /// follows `press_inner`.
    pub fn press(&mut self, src: PttSource, now: Instant) -> PressOutcome {
        if src == PttSource::Bluetooth {
            return PressOutcome::Ignored;
        }
        self.press_inner(src, now)
    }

    /// The press rules shared by `press` and `lease_press`. A latched source
    /// is ignored (rule 2). Pressing a source that already holds is `Held`
    /// and changes nothing. A press that raises the OR starts the transmit
    /// time-out's clock.
    fn press_inner(&mut self, src: PttSource, now: Instant) -> PressOutcome {
        if self.latched[src.index()] {
            return PressOutcome::Ignored;
        }
        if self.held[src.index()].is_some() {
            return PressOutcome::Held;
        }
        let was_on = self.is_on_air();
        self.held[src.index()] = Some(Hold { lease: None });
        if was_on {
            return PressOutcome::Held;
        }
        self.on_air_since = Some(now);
        PressOutcome::KeyNow
    }

    /// A press that also arms `deadline = now + ttl` (re-arms it if `src`
    /// already holds). The source unkeys itself at the deadline unless
    /// renewed, so a client that stalls can never leave the radio keyed.
    pub fn lease_press(&mut self, src: PttSource, ttl: Duration, now: Instant) -> PressOutcome {
        let outcome = self.press_inner(src, now);
        if outcome != PressOutcome::Ignored {
            let deadline = now.checked_add(ttl).unwrap_or(now);
            self.held[src.index()] = Some(Hold {
                lease: Some(Lease { ttl, deadline }),
            });
        }
        outcome
    }

    /// Undo a `KeyNow` whose console key failed: un-hold `src`, latch it until
    /// it releases, and stop the clock if nothing else is on air. Without the
    /// latch, a button held through dialing would key the moment the call
    /// answered. `Legacy` is not latched: a `set_ptt(true)` is one request,
    /// not a held button, so a refused `/key` leaves nothing held or latched.
    pub fn rollback_press(&mut self, src: PttSource) {
        self.held[src.index()] = None;
        if src != PttSource::Legacy {
            self.latched[src.index()] = true;
        }
        if !self.is_on_air() {
            self.on_air_since = None;
        }
    }

    /// Release `src`: clear its hold and its latch. Idempotent and never
    /// refused. `Unkey` only when this was the last source on air.
    pub fn release(&mut self, src: PttSource) -> PttEdge {
        let was_on = self.is_on_air();
        self.held[src.index()] = None;
        self.latched[src.index()] = false;
        self.edge_after(was_on)
    }

    /// Extend a held, leased `src` to `now + ttl`. Anything else — idle,
    /// released, latched, plain-pressed, or a lease already at its deadline —
    /// is `NotLeased` and changes nothing. Renew never presses.
    pub fn lease_renew(&mut self, src: PttSource, now: Instant) -> Result<(), NotLeased> {
        let Some(Hold { lease: Some(lease) }) = &mut self.held[src.index()] else {
            return Err(NotLeased);
        };
        if now >= lease.deadline {
            return Err(NotLeased);
        }
        lease.deadline = now.checked_add(lease.ttl).unwrap_or(now);
        Ok(())
    }

    /// Release every source and latch every held one, `Legacy` included: the
    /// universal stop. Not a forced stop, so `forced` is untouched.
    pub fn stop_all(&mut self) -> PttEdge {
        self.release_all(true)
    }

    /// `set_ptt(false)`: `stop_all`, then clear `Legacy`'s own latch, because
    /// `false` is the only release a Legacy caller has.
    pub fn legacy_off(&mut self) -> PttEdge {
        let edge = self.stop_all();
        self.latched[PttSource::Legacy.index()] = false;
        edge
    }

    /// The call is over: release every source and latch every held one except
    /// `Legacy`, so nothing held through a hangup keys the next call. A held
    /// `Legacy` is released without a latch, so the next call's `/key` keys.
    pub fn call_ended(&mut self) -> PttEdge {
        self.release_all(false)
    }

    /// Latches every held source except `Legacy` (released, never latched)
    /// when the call it was pressed on is gone: `!answered`, or
    /// `answered_seq` changed from the last observed value. The first
    /// observation only records the seq.
    ///
    /// An unanswered observation latches and also forgets the seq, so the
    /// first answered observation after it only records the new seq: every
    /// source held at the unanswered observation is already latched, and a
    /// source pressed (and accepted by the console) after the answer belongs
    /// to the current call, which must not read as "a different call".
    pub fn observe_call(&mut self, answered: bool, answered_seq: u64) -> PttEdge {
        if !answered {
            self.last_answered_seq = None;
            return self.release_all(false);
        }
        let previous = self.last_answered_seq.replace(answered_seq);
        if previous.is_some_and(|seq| seq != answered_seq) {
            self.release_all(false)
        } else {
            PttEdge::None
        }
    }

    /// Applies the transmit time-out (first) and lease expiry. `now - on_air_since >= tx_timeout`
    /// fires; a lease fires at `now >= deadline`.
    pub fn tick(&mut self, now: Instant) -> PttEdge {
        let timed_out = self
            .on_air_since
            .is_some_and(|since| now.saturating_duration_since(since) >= self.tx_timeout);
        if timed_out {
            let sources = self.held_mask();
            self.record_forced(sources, ForcedReason::TxTimeout);
            return self.release_all(true);
        }
        let expired = mask_of(
            |src| matches!(self.held[src.index()], Some(Hold { lease: Some(lease) }) if now >= lease.deadline),
        );
        if expired == 0 {
            return PttEdge::None;
        }
        let was_on = self.is_on_air();
        for src in PttSource::ALL {
            if expired & src.bit() != 0 {
                self.held[src.index()] = None;
                self.latched[src.index()] = true;
            }
        }
        self.record_forced(expired, ForcedReason::LeaseExpired);
        self.edge_after(was_on)
    }

    /// Set the transmit time-out: 60..=600 s inclusive, else
    /// `TxTimeoutOutOfRange` and the old value stays. Applies to a running
    /// clock at the next `tick`; it never restarts it.
    pub fn set_tx_timeout(&mut self, timeout: Duration) -> Result<(), TxTimeoutOutOfRange> {
        if (MIN_TX_TIMEOUT..=MAX_TX_TIMEOUT).contains(&timeout) {
            self.tx_timeout = timeout;
            Ok(())
        } else {
            Err(TxTimeoutOutOfRange)
        }
    }

    /// Test hook (feature `test-hooks`): sets the time-out without the 60..=600 s
    /// check so a loopback test can watch it fire in milliseconds. The caller
    /// (`Station::set_tx_timeout_for_test`) bounds it to 1 ms..=600 s, so even a
    /// test can never disable it.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn set_tx_timeout_unchecked(&mut self, timeout: Duration) {
        self.tx_timeout = timeout;
    }

    /// The transmit time-out in force.
    #[must_use]
    pub fn tx_timeout(&self) -> Duration {
        self.tx_timeout
    }

    /// Whether any held source is unlatched: the OR the console follows.
    #[must_use]
    pub fn is_on_air(&self) -> bool {
        PttSource::ALL
            .into_iter()
            .any(|src| self.held[src.index()].is_some() && !self.latched[src.index()])
    }

    /// Masks, the last forced stop and the time-out, for the station snapshot.
    #[must_use]
    pub fn snapshot(&self) -> PttSnapshot {
        PttSnapshot {
            sources: self.held_mask(),
            latched: mask_of(|src| self.latched[src.index()]),
            forced: self.forced,
            tx_timeout: self.tx_timeout,
        }
    }

    fn held_mask(&self) -> u32 {
        mask_of(|src| self.held[src.index()].is_some())
    }

    /// Release every source and latch every held one, except that a held
    /// `Legacy` is latched only when `latch_legacy`: the transmit time-out
    /// and `stop_all` pass `true`, `call_ended` and `observe_call` `false`
    /// (rules 4, 5 and 8).
    fn release_all(&mut self, latch_legacy: bool) -> PttEdge {
        let was_on = self.is_on_air();
        for src in PttSource::ALL {
            let was_held = self.held[src.index()].take().is_some();
            if was_held && (latch_legacy || src != PttSource::Legacy) {
                self.latched[src.index()] = true;
            }
        }
        self.edge_after(was_on)
    }

    /// `Unkey` if the OR just fell, which is also the only place the transmit
    /// time-out's clock stops.
    fn edge_after(&mut self, was_on: bool) -> PttEdge {
        if was_on && !self.is_on_air() {
            self.on_air_since = None;
            PttEdge::Unkey
        } else {
            PttEdge::None
        }
    }

    fn record_forced(&mut self, sources: u32, reason: ForcedReason) {
        self.forced = ForcedRecord {
            seq: self.forced.seq.wrapping_add(1),
            sources,
            reason: Some(reason),
        };
    }
}

impl Default for PttArbiter {
    fn default() -> Self {
        Self::new()
    }
}
