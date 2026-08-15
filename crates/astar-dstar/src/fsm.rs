// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure client link state machine for a `DExtra` (XRF/XLX) reflector
//! connection (research §2). Mirrors `astar_m17::fsm::SessionFsm`'s
//! shape: no I/O, `now` passed in explicitly as an [`Instant`] for every
//! time-sensitive call, so the whole thing is unit-testable without
//! sleeping.
//!
//! ## Wire layout (research §2, `chazapis/pydv`'s `dv/dextra.py`)
//!
//! - **Connect request**, 11 bytes: 8-byte space-padded callsign, 1 own
//!   (src) module byte, 1 dest module byte, 1 revision byte (`0x00`).
//! - **Connect ACK/NACK**, 14 bytes: 8-byte callsign, 2 module bytes, then
//!   4 bytes `"ACK\0"` or `"NAK\0"`. Only the trailing 4 bytes are checked
//!   here — the reflector echoes callsign/module fields we already know.
//! - **Keepalive**, 9 bytes: 8-byte callsign + a NUL byte. Either side
//!   echoes one back on receipt; a client that hears nothing for 30s
//!   considers the link dead (xlxd `DEXTRA_KEEPALIVE_TIMEOUT=30`).
//! - **Disconnect**, 11 bytes: same shape as connect, but with the dest
//!   module byte blanked to `' '` to signal "unlink."
//! - **Disconnect ACK**: the literal 12-byte string `"DISCONNECTED"`.
//!
//! ## Own vs. dest module
//!
//! [`DextraFsm::new`] takes a single `module` byte: the *dest* module (the
//! reflector module the caller wants to link into, e.g. `b'B'`). The *own*
//! (src) module byte is fixed to `b' '` — a plain client, not itself acting
//! as a repeater/gateway module — matching common non-repeater `DExtra`
//! client behavior. [`DextraFsm::unlink`] blanks only the dest module
//! (research §2's documented disconnect signal); the own module byte never
//! changes.
//!
//! ## `LinkState` gating in [`DextraFsm::on_packet`]
//!
//! Every branch is gated to the states where the packet is actually
//! meaningful, rather than reacting to shape alone — this FSM gets wired to
//! a live, adversarial-ish socket in a later task, so an ACK arriving
//! before `connect()` (or replayed after the link is already up) must not
//! re-fire a state transition:
//!
//! - **ACK/NAK**: only change anything while [`LinkState::Linking`] (the
//!   one state actually waiting on a connect response). An ACK received
//!   while already [`LinkState::Linked`] (a duplicate/retransmitted ACK —
//!   real reflectors can and do resend) is treated exactly like an ACK in
//!   any other non-`Linking` state: ignored, `FsmAction::None`, no clock
//!   refresh, no repeated [`FsmAction::Linked`]. The periodic liveness
//!   signal is the keepalive, not the ACK, so there's no correctness cost
//!   to not refreshing `last_rx` here — it keeps the gating uniform instead
//!   of special-casing "ignored, but still touches state" per source state.
//! - **Keepalive**: answered (and refreshes `last_rx`) only in
//!   [`LinkState::Linking`], [`LinkState::Linked`], or
//!   [`LinkState::Unlinking`] — the states where a session is actively
//!   trying to reach or hold a link. Ignored in [`LinkState::Idle`] (never
//!   connected) and [`LinkState::Failed`] (already dead): answering either
//!   would manufacture outbound traffic for a session nothing is waiting
//!   on.
//! - **`"DISCONNECTED"`**: meaningful in [`LinkState::Unlinking`] (our own
//!   clean `unlink()` was acked → [`LinkState::Idle`], a fully idle and
//!   reusable session) and in [`LinkState::Linked`] (a reflector-initiated
//!   drop we never asked for → [`LinkState::Failed`], not `Idle`, so
//!   `state()` lets a caller tell "we cleanly disconnected" apart from
//!   "the reflector dropped us"; both still return `FsmAction::Unlinked`
//!   so the caller tears the session down either way). Ignored elsewhere.
//! - **`tick` timeout**: unchanged from before this gating pass — it was
//!   already conditioned on `state` being neither `Idle` nor `Failed`,
//!   i.e. only `Linking`/`Linked`/`Unlinking` can time out.

use std::time::{Duration, Instant};

/// How long the client waits without hearing anything from the reflector
/// before declaring the link dead (research §2, xlxd
/// `DEXTRA_KEEPALIVE_TIMEOUT=30`).
const LINK_TIMEOUT: Duration = Duration::from_secs(30);

/// Own/src module byte this crate's `DExtra` client always presents: a plain
/// (non-repeater) client (see module docs).
const OWN_MODULE: u8 = b' ';

/// Dest-module byte a disconnect blanks to, signaling "unlink" (research
/// §2).
const BLANK_MODULE: u8 = b' ';

const CALLSIGN_LEN: usize = 8;
const ACK_NAK_LEN: usize = 14;
const KEEPALIVE_LEN: usize = 9;
const DISCONNECTED: &[u8] = b"DISCONNECTED";

/// Current state of the `DExtra` link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected; [`DextraFsm::connect`] hasn't been called (or the
    /// link has cleanly unlinked back down to rest).
    Idle,
    /// Connect request sent, waiting for ACK/NAK.
    Linking,
    /// ACK received; the link is up.
    Linked,
    /// [`DextraFsm::unlink`] sent, waiting for the `"DISCONNECTED"` ack.
    Unlinking,
    /// Rejected (NAK) or timed out.
    Failed,
}

impl LinkState {
    /// A stable lowercase name for this state.
    ///
    /// This is an ABI string, not a debug convenience: it crosses the C-ABI
    /// in [`iax_station_dstar_state`]'s JSON (iax-4c8e) and is matched by
    /// name in front-ends. Renaming a variant is free; changing one of these
    /// strings breaks every consumer, so treat them as fixed.
    ///
    /// [`iax_station_dstar_state`]: https://docs.rs/astar-sys
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LinkState::Idle => "idle",
            LinkState::Linking => "linking",
            LinkState::Linked => "linked",
            LinkState::Unlinking => "unlinking",
            LinkState::Failed => "failed",
        }
    }
}

/// What the caller should do in response to [`DextraFsm::on_packet`] or
/// [`DextraFsm::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsmAction {
    /// Nothing to do.
    None,
    /// Send these bytes to the reflector (e.g. a keepalive echo).
    Send(Vec<u8>),
    /// The link just came up (ACK received).
    Linked,
    /// The link just went down (NAK, or a `"DISCONNECTED"` ack); tear down
    /// the session.
    Unlinked,
    /// The link timed out (no keepalive for 30s); tear down the session.
    Timeout,
}

/// An error constructing a [`DextraFsm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsmError {
    /// Callsign is longer than the wire format's 8-byte field.
    CallsignTooLong,
    /// Callsign contains a non-ASCII byte.
    NonAsciiCallsign,
}

/// Pure client-side state machine for one `DExtra` reflector link.
///
/// Holds no clock and does no I/O: `now` is passed in by the caller for
/// every time-sensitive operation.
#[derive(Debug, Clone)]
pub struct DextraFsm {
    callsign: [u8; CALLSIGN_LEN],
    own_module: u8,
    dest_module: u8,
    state: LinkState,
    last_rx: Option<Instant>,
    keepalive_timeout: Duration,
}

impl DextraFsm {
    /// Creates a new, unlinked session for `callsign` targeting reflector
    /// module `module`, using the default 30s keepalive timeout.
    pub fn new(callsign: &str, module: u8) -> Result<DextraFsm, FsmError> {
        Self::with_keepalive_timeout(callsign, module, LINK_TIMEOUT)
    }

    /// Like [`DextraFsm::new`], but with a caller-supplied keepalive
    /// timeout instead of the default 30s — lets tests shrink the
    /// silence-timeout window without changing the wire protocol.
    pub fn with_keepalive_timeout(
        callsign: &str,
        module: u8,
        keepalive_timeout: Duration,
    ) -> Result<DextraFsm, FsmError> {
        if !callsign.is_ascii() {
            return Err(FsmError::NonAsciiCallsign);
        }
        if callsign.len() > CALLSIGN_LEN {
            return Err(FsmError::CallsignTooLong);
        }
        let mut cs = [b' '; CALLSIGN_LEN];
        cs[..callsign.len()].copy_from_slice(callsign.as_bytes());
        Ok(DextraFsm {
            callsign: cs,
            own_module: OWN_MODULE,
            dest_module: module,
            state: LinkState::Idle,
            last_rx: None,
            keepalive_timeout,
        })
    }

    /// Current [`LinkState`].
    #[must_use]
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// This session's own 8-byte space-padded callsign field, exactly as
    /// used on the wire in every connect/keepalive/unlink packet
    /// (iax-2f6b): the single source of truth for the padding logic
    /// [`DextraFsm::new`] already applies, so a TX layer building its own RF
    /// header's `my` field doesn't need to duplicate it.
    #[must_use]
    pub fn callsign(&self) -> [u8; CALLSIGN_LEN] {
        self.callsign
    }

    /// Begins connecting: moves to [`LinkState::Linking`] and returns the
    /// 11-byte connect request to send.
    #[must_use]
    pub fn connect(&mut self, now: Instant) -> Vec<u8> {
        self.state = LinkState::Linking;
        self.last_rx = Some(now);
        self.connect_packet(self.dest_module)
    }

    /// Unlinks: moves to [`LinkState::Unlinking`] and returns the 11-byte
    /// disconnect request (dest module blanked to `' '`) to send.
    #[must_use]
    pub fn unlink(&mut self, now: Instant) -> Vec<u8> {
        self.state = LinkState::Unlinking;
        self.last_rx = Some(now);
        self.connect_packet(BLANK_MODULE)
    }

    fn connect_packet(&self, dest_module: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity(11);
        buf.extend_from_slice(&self.callsign);
        buf.push(self.own_module);
        buf.push(dest_module);
        buf.push(0x00);
        buf
    }

    fn keepalive_packet(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(KEEPALIVE_LEN);
        buf.extend_from_slice(&self.callsign);
        buf.push(0x00);
        buf
    }

    /// Feeds a received packet into the session, advancing the state
    /// machine and returning the resulting [`FsmAction`]. Every branch is
    /// gated to the [`LinkState`]s where it's meaningful — see the module
    /// docs' "`LinkState` gating" section for the full rationale per
    /// packet type.
    #[must_use]
    pub fn on_packet(&mut self, buf: &[u8], now: Instant) -> FsmAction {
        if buf.len() == KEEPALIVE_LEN && buf[CALLSIGN_LEN] == 0x00 {
            if matches!(
                self.state,
                LinkState::Linking | LinkState::Linked | LinkState::Unlinking
            ) {
                self.last_rx = Some(now);
                return FsmAction::Send(self.keepalive_packet());
            }
            return FsmAction::None;
        }

        if buf.len() == ACK_NAK_LEN {
            if buf.ends_with(b"ACK\0") {
                if self.state == LinkState::Linking {
                    self.last_rx = Some(now);
                    self.state = LinkState::Linked;
                    return FsmAction::Linked;
                }
                return FsmAction::None;
            }
            if buf.ends_with(b"NAK\0") {
                if self.state == LinkState::Linking {
                    self.last_rx = Some(now);
                    self.state = LinkState::Failed;
                    return FsmAction::Unlinked;
                }
                return FsmAction::None;
            }
        }

        if buf == DISCONNECTED {
            return match self.state {
                LinkState::Unlinking => {
                    self.last_rx = Some(now);
                    self.state = LinkState::Idle;
                    FsmAction::Unlinked
                }
                LinkState::Linked => {
                    self.last_rx = Some(now);
                    self.state = LinkState::Failed;
                    FsmAction::Unlinked
                }
                LinkState::Idle | LinkState::Linking | LinkState::Failed => FsmAction::None,
            };
        }

        FsmAction::None
    }

    /// Advances time to `now`, declaring the link [`LinkState::Failed`] if
    /// the configured keepalive timeout ([`LINK_TIMEOUT`] by default; see
    /// [`DextraFsm::with_keepalive_timeout`]) has elapsed with no received
    /// packet since the last [`DextraFsm::connect`], [`DextraFsm::unlink`],
    /// or [`DextraFsm::on_packet`] call.
    #[must_use]
    pub fn tick(&mut self, now: Instant) -> FsmAction {
        let Some(last) = self.last_rx else {
            return FsmAction::None;
        };
        if !matches!(self.state, LinkState::Failed | LinkState::Idle)
            && now.duration_since(last) >= self.keepalive_timeout
        {
            self.state = LinkState::Failed;
            return FsmAction::Timeout;
        }
        FsmAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack_packet(callsign: [u8; 8], own: u8, dest: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ACK_NAK_LEN);
        buf.extend_from_slice(&callsign);
        buf.push(own);
        buf.push(dest);
        buf.extend_from_slice(b"ACK\0");
        buf
    }

    fn nak_packet(callsign: [u8; 8], own: u8, dest: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ACK_NAK_LEN);
        buf.extend_from_slice(&callsign);
        buf.push(own);
        buf.push(dest);
        buf.extend_from_slice(b"NAK\0");
        buf
    }

    #[test]
    fn connect_produces_exact_11_byte_wire_packet() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let bytes = fsm.connect(now);
        assert_eq!(fsm.state(), LinkState::Linking);
        assert_eq!(bytes.len(), 11);
        assert_eq!(&bytes[0..8], b"N0CALL  "); // 8-byte space-padded
        assert_eq!(bytes[8], b' '); // own module: plain client
        assert_eq!(bytes[9], b'B'); // dest module
        assert_eq!(bytes[10], 0x00); // revision
    }

    #[test]
    fn connect_pads_short_callsign_and_keeps_full_length_callsign() {
        let mut fsm = DextraFsm::new("AJ7HR", b'A').expect("valid callsign");
        let bytes = fsm.connect(Instant::now());
        assert_eq!(&bytes[0..8], b"AJ7HR   ");

        let mut fsm2 = DextraFsm::new("12345678", b'A').expect("exactly 8 bytes");
        let bytes2 = fsm2.connect(Instant::now());
        assert_eq!(&bytes2[0..8], b"12345678");
    }

    #[test]
    fn new_rejects_callsign_longer_than_8_bytes() {
        let err = DextraFsm::new("123456789", b'A').unwrap_err();
        assert_eq!(err, FsmError::CallsignTooLong);
    }

    #[test]
    fn new_rejects_non_ascii_callsign() {
        let err = DextraFsm::new("N0CÄLL", b'A').unwrap_err();
        assert_eq!(err, FsmError::NonAsciiCallsign);
    }

    #[test]
    fn ack_moves_to_linked() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);

        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let action = fsm.on_packet(&ack, now);
        assert_eq!(action, FsmAction::Linked);
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn nak_moves_to_failed() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);

        let nak = nak_packet(*b"XRF757 G", b' ', b'B');
        let action = fsm.on_packet(&nak, now);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn keepalive_rx_is_answered_with_our_own_keepalive() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);

        let mut keepalive = b"XRF757 G".to_vec();
        keepalive.push(0x00);
        assert_eq!(keepalive.len(), 9);

        let action = fsm.on_packet(&keepalive, now);
        let mut expected = b"N0CALL  ".to_vec();
        expected.push(0x00);
        assert_eq!(action, FsmAction::Send(expected));
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn thirty_one_seconds_of_silence_times_out_the_link() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);
        assert_eq!(fsm.state(), LinkState::Linked);

        let later = now + Duration::from_secs(31);
        let action = fsm.tick(later);
        assert_eq!(action, FsmAction::Timeout);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn under_thirty_seconds_does_not_time_out() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);

        let later = now + Duration::from_secs(29);
        let action = fsm.tick(later);
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn with_keepalive_timeout_shortens_the_silence_window() {
        let mut fsm = DextraFsm::with_keepalive_timeout("N0CALL", b'B', Duration::from_millis(200))
            .expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);
        assert_eq!(fsm.state(), LinkState::Linked);

        let later = now + Duration::from_millis(250);
        let action = fsm.tick(later);
        assert_eq!(action, FsmAction::Timeout);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn unlink_blanks_dest_module_and_keeps_own_module() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);

        let bytes = fsm.unlink(now);
        assert_eq!(fsm.state(), LinkState::Unlinking);
        assert_eq!(bytes.len(), 11);
        assert_eq!(&bytes[0..8], b"N0CALL  ");
        assert_eq!(bytes[8], b' '); // own module unchanged
        assert_eq!(bytes[9], b' '); // dest module blanked
        assert_eq!(bytes[10], 0x00);
    }

    #[test]
    fn disconnected_ack_moves_to_idle() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);
        let _ = fsm.unlink(now);

        let action = fsm.on_packet(b"DISCONNECTED", now);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    // --- LinkState gating (task-4 review fix) ---

    #[test]
    fn ack_while_idle_is_ignored() {
        // No connect() call at all: an ACK arriving out of nowhere must
        // not jump straight to Linked.
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        assert_eq!(fsm.state(), LinkState::Idle);

        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let action = fsm.on_packet(&ack, Instant::now());
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn nak_while_idle_is_ignored() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        assert_eq!(fsm.state(), LinkState::Idle);

        let nak = nak_packet(*b"XRF757 G", b' ', b'B');
        let action = fsm.on_packet(&nak, Instant::now());
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn duplicate_ack_while_linked_does_not_refire() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let first = fsm.on_packet(&ack, now);
        assert_eq!(first, FsmAction::Linked);
        assert_eq!(fsm.state(), LinkState::Linked);

        // A retransmitted ACK for the same, already-up link: must not
        // re-fire FsmAction::Linked a second time.
        let second = fsm.on_packet(&ack, now);
        assert_eq!(second, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Linked);
    }

    #[test]
    fn keepalive_while_idle_is_ignored() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        assert_eq!(fsm.state(), LinkState::Idle);

        let mut keepalive = b"XRF757 G".to_vec();
        keepalive.push(0x00);
        let action = fsm.on_packet(&keepalive, Instant::now());
        assert_eq!(
            action,
            FsmAction::None,
            "must not manufacture outbound traffic for a session that was never established"
        );
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn keepalive_while_failed_is_ignored() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let nak = nak_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&nak, now);
        assert_eq!(fsm.state(), LinkState::Failed);

        let mut keepalive = b"XRF757 G".to_vec();
        keepalive.push(0x00);
        let action = fsm.on_packet(&keepalive, now);
        assert_eq!(
            action,
            FsmAction::None,
            "must not manufacture outbound traffic for an already-dead session"
        );
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn disconnected_while_idle_is_ignored() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        assert_eq!(fsm.state(), LinkState::Idle);

        let action = fsm.on_packet(b"DISCONNECTED", Instant::now());
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn reflector_initiated_disconnected_while_linked_fails_the_link() {
        // "DISCONNECTED" arriving while Linked (we never called unlink())
        // is a reflector-initiated drop, not our own clean unlink: lands
        // in Failed, not Idle, so a caller can tell the two apart via
        // state(), while still getting FsmAction::Unlinked either way.
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let ack = ack_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&ack, now);
        assert_eq!(fsm.state(), LinkState::Linked);

        let action = fsm.on_packet(b"DISCONNECTED", now);
        assert_eq!(action, FsmAction::Unlinked);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn idle_link_never_times_out() {
        let fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        assert_eq!(fsm.state(), LinkState::Idle);
        // No connect() called yet: last_rx is None, tick must be a no-op
        // forever, not spuriously time out an unconnected session.
        let mut fsm = fsm;
        let action = fsm.tick(Instant::now() + Duration::from_secs(3600));
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Idle);
    }

    #[test]
    fn failed_link_ticks_stay_quiet() {
        let mut fsm = DextraFsm::new("N0CALL", b'B').expect("valid callsign");
        let now = Instant::now();
        let _ = fsm.connect(now);
        let nak = nak_packet(*b"XRF757 G", b' ', b'B');
        let _ = fsm.on_packet(&nak, now);
        assert_eq!(fsm.state(), LinkState::Failed);

        let action = fsm.tick(now + Duration::from_secs(3600));
        assert_eq!(action, FsmAction::None);
        assert_eq!(fsm.state(), LinkState::Failed);
    }
}
