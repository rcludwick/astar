// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Client-side state machine for one MMDVM/homebrew master link.
//!
//! Where `astar_ysf`'s and `astar_nxdn`'s links are a poll repeated until
//! something answers, DMR's is a **chain**: `RPTL` → `RPTACK` + salt →
//! `RPTK` → `RPTACK` → `RPTC` → `RPTACK` → linked, and only then do the
//! pings start. Four differences follow from that, and each of them is the
//! substance of this module:
//!
//! 1. [`DmrFsm::connect`] sends **one** `RPTL`, not three. A duplicate login
//!    makes the master mint a fresh salt (`hblink.py`'s `RPTL` branch
//!    replaces the peer's entry outright), so the second copy of a triple
//!    would invalidate the digest computed from the first.
//! 2. `RPTACK` means a different thing at every stage — a salt at
//!    [`LinkState::LoggingIn`], a bare acknowledgement afterwards — and the
//!    four bytes are identical either way, so only the state tells them
//!    apart.
//! 3. The salt is the four bytes **verbatim**
//!    (`DMRGateway/DMRNetwork.cpp`: `::memcpy(m_salt, m_buffer + 6U,
//!    sizeof(uint32_t))`), never round-tripped through an integer.
//! 4. `MSTNAK` is diagnosed by the state it arrived in, exactly as
//!    `DMRGateway/DMRNetwork.cpp: writeJSONLinkFailed` does — "wrong
//!    password" and "your ID is not permitted here" are the two things an
//!    operator needs told apart and the wire distinguishes them only by
//!    timing. One `MSTNAK` is not a failure at all: the one that answers our
//!    own `RPTCL`.
//!
//! Holds no clock and does no I/O: `now` is passed in for every
//! time-sensitive operation, as every other astar link FSM does.
//!
//! # Secrets
//!
//! The password is **moved in** at construction and is the FSM's alone from
//! then on: it builds the `RPTK` digest and nothing else. It is never
//! returned, never formatted, never logged, and [`DmrFsm`]'s `Debug` — hand
//! written, because the derived one would print it — shows the state and the
//! ids only. CLAUDE.md: secrets are connect-time in-args and nothing else.
//!
//! It is held rather than dropped after the first `RPTK` because a stalled
//! handshake starts over ([`LOGIN_RETRY`]) and a fresh salt needs a fresh
//! digest; an FSM that had thrown the password away would answer that salt
//! with a digest of nothing and fail authentication for a reason no log
//! would explain.

use std::time::{Duration, Instant};

use crate::wire::{self, CallType, ConfigFields, DataPacket, Packet, RadioId, Timeslot};

/// How often to ping once linked.
///
/// `DroidStar dmr.cpp: setup_connection` — `m_ping_timer->start(5000)`.
/// G4KLX's gateway has no ping timer at all and reuses its 10 s retry timer;
/// 5 s is inside every window the references use and is the softclient's own
/// number (`docs/design/dmr-wire.md` §1).
pub const PING_INTERVAL: Duration = Duration::from_secs(5);
/// How long a stalled handshake waits before starting over with a fresh
/// `RPTL`.
///
/// `DMRGateway/DMRNetwork.cpp`, constructor: `m_retryTimer(1000U, 10U)`.
/// Starting over rather than repeating the stalled step is the reference's
/// behaviour and the only correct one: a salt that went unanswered is stale,
/// and the digest built from it can never be authorised.
pub const LOGIN_RETRY: Duration = Duration::from_secs(10);
/// Silence after which a live link is declared dead.
///
/// `DMRGateway/DMRNetwork.cpp`, same constructor: `m_timeoutTimer(1000U,
/// 60U)` — twelve missed pongs.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the link is in the handshake chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not connected.
    Idle,
    /// `RPTL` sent, waiting for a salt.
    LoggingIn,
    /// `RPTK` sent, waiting for the master to accept the digest.
    Authenticating,
    /// `RPTC` sent, waiting for the master to accept the config.
    Configuring,
    /// Linked. Pings from here.
    Linked,
    /// `RPTCL` sent; nothing more is expected, and the `MSTNAK` that answers
    /// it is not an error.
    Closing,
    /// Refused, dropped or timed out. [`FailureStage`] says which.
    Failed,
}

impl LinkState {
    /// A stable lowercase name.
    ///
    /// This is an ABI string, not a debug convenience: it is what a link
    /// state is called when it crosses to a front-end. Renaming a variant is
    /// free; changing one of these strings is not.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            LinkState::Idle => "idle",
            LinkState::LoggingIn => "logging_in",
            LinkState::Authenticating => "authenticating",
            LinkState::Configuring => "configuring",
            LinkState::Linked => "linked",
            LinkState::Closing => "closing",
            LinkState::Failed => "failed",
        }
    }
}

/// Where a link failed — the diagnosis an operator is owed.
///
/// `DMRGateway/DMRNetwork.cpp: writeJSONLinkFailed` makes exactly this
/// distinction, for exactly this reason: on the wire the difference between
/// "your ID is not permitted here" and "your password is wrong" is nothing
/// but which packet the `MSTNAK` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    /// `MSTNAK` to `RPTL`: this ID is not permitted on this master.
    Login,
    /// `MSTNAK` to `RPTK`: the password is wrong. The common case.
    Auth,
    /// `MSTNAK` to `RPTC`: the config was rejected — often this ID is
    /// already connected somewhere else.
    Config,
    /// `MSTNAK` while running: the session was dropped.
    Session,
    /// `MSTCL`: the master closed.
    Closed,
    /// Nothing heard for [`LINK_TIMEOUT`].
    Timeout,
}

impl FailureStage {
    /// A stable lowercase name. An ABI string, like [`LinkState::as_str`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureStage::Login => "login",
            FailureStage::Auth => "auth",
            FailureStage::Config => "config",
            FailureStage::Session => "session",
            FailureStage::Closed => "closed",
            FailureStage::Timeout => "timeout",
        }
    }
}

/// What the caller should do in response to [`DmrFsm::on_packet`] or
/// [`DmrFsm::tick`].
#[derive(Debug, PartialEq, Eq)]
pub enum FsmAction {
    /// Nothing to do.
    None,
    /// Send these bytes to the master.
    Send(Vec<u8>),
    /// The handshake completed.
    Linked,
    /// A frame for us arrived.
    Data(Box<DataPacket>),
    /// The link is over, and this is why.
    Failed(FailureStage),
}

/// Pure client-side state machine for one homebrew master link.
///
/// `Debug` is written by hand: the derived one would print the password.
pub struct DmrFsm {
    id: RadioId,
    talkgroup: u32,
    slot: Timeslot,
    fields: ConfigFields,
    /// Taken at construction and never shown to anybody. It lives as long
    /// as the FSM because a stalled handshake starts the chain over and
    /// needs it again — what is pinned is that it never leaves. See the
    /// module's "Secrets" note.
    password: String,
    state: LinkState,
    /// When the current handshake step was sent, for [`LOGIN_RETRY`].
    stage_since: Option<Instant>,
    last_rx: Option<Instant>,
    last_ping: Option<Instant>,
    ping_interval: Duration,
    link_timeout: Duration,
}

impl std::fmt::Debug for DmrFsm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmrFsm")
            .field("id", &self.id)
            .field("talkgroup", &self.talkgroup)
            .field("slot", &self.slot)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl DmrFsm {
    /// A new, unlinked session for `talkgroup` on `slot`.
    ///
    /// `password` is moved in and used for the `RPTK` digest and nothing
    /// else. It is never returned, never formatted, never logged.
    #[must_use]
    pub fn new(
        id: RadioId,
        talkgroup: u32,
        slot: Timeslot,
        fields: ConfigFields,
        password: String,
    ) -> DmrFsm {
        Self::with_timing(
            id,
            talkgroup,
            slot,
            fields,
            password,
            PING_INTERVAL,
            LINK_TIMEOUT,
        )
    }

    /// Like [`DmrFsm::new`], but with caller-supplied timings — lets tests
    /// shrink the ping and silence windows without touching the wire.
    #[must_use]
    pub fn with_timing(
        id: RadioId,
        talkgroup: u32,
        slot: Timeslot,
        fields: ConfigFields,
        password: String,
        ping_interval: Duration,
        link_timeout: Duration,
    ) -> DmrFsm {
        DmrFsm {
            id,
            talkgroup,
            slot,
            fields,
            password,
            state: LinkState::Idle,
            stage_since: None,
            last_rx: None,
            last_ping: None,
            ping_interval,
            link_timeout,
        }
    }

    /// Current [`LinkState`].
    #[must_use]
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// The radio ID this link identifies itself with.
    #[must_use]
    pub fn radio_id(&self) -> RadioId {
        self.id
    }

    /// The talkgroup this link is listening to.
    #[must_use]
    pub fn talkgroup(&self) -> u32 {
        self.talkgroup
    }

    /// The timeslot this link is listening on.
    #[must_use]
    pub fn slot(&self) -> Timeslot {
        self.slot
    }

    /// Opens the link: the datagrams to send, in order.
    ///
    /// One `RPTL`, and one only — see the module doc's first difference.
    pub fn connect(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::LoggingIn;
        self.stage_since = Some(now);
        self.last_rx = Some(now);
        self.last_ping = None;
        vec![wire::login(self.id).to_vec()]
    }

    /// Closes the link: the datagrams to send, in order.
    ///
    /// Expect an `MSTNAK` back; [`DmrFsm::on_packet`] will not read it as a
    /// failure while the state is [`LinkState::Closing`].
    pub fn close(&mut self, _now: Instant) -> Vec<Vec<u8>> {
        self.state = LinkState::Closing;
        self.stage_since = None;
        self.last_ping = None;
        vec![wire::close(self.id).to_vec()]
    }

    /// Feeds one received datagram in.
    pub fn on_packet(&mut self, datagram: &[u8], now: Instant) -> FsmAction {
        let Some(packet) = wire::parse(datagram) else {
            return FsmAction::None;
        };
        // Only a packet this protocol has a reading for counts as the far
        // end being alive. `Unknown` covers RPTSBKN, an RPTO echo, a
        // trunking command and plain noise, and treating those as proof of
        // life would let a stranger's junk hold a dead link open past
        // LINK_TIMEOUT for as long as they cared to keep sending it.
        let alive = matches!(
            packet,
            Packet::Ack { .. } | Packet::Pong { .. } | Packet::Nak { .. } | Packet::Data(_)
        );
        if alive && !matches!(self.state, LinkState::Idle | LinkState::Failed) {
            self.last_rx = Some(now);
        }
        match packet {
            Packet::Ack { id, salt } => self.on_ack(id, salt, now),
            Packet::Nak { .. } => self.on_nak(),
            Packet::MasterClosing { .. } => {
                if matches!(
                    self.state,
                    LinkState::LoggingIn
                        | LinkState::Authenticating
                        | LinkState::Configuring
                        | LinkState::Linked
                ) {
                    self.state = LinkState::Failed;
                    return FsmAction::Failed(FailureStage::Closed);
                }
                FsmAction::None
            }
            Packet::Data(data) => self.on_data(data),
            // A pong's whole effect is the `last_rx` refresh above — that is
            // what a keepalive is for. `RPTSBKN`, an `RPTO` echo and the
            // trunking commands land here too, and astar answers none of
            // them (`docs/design/dmr-wire.md` §1, §10).
            Packet::Pong { .. } | Packet::Unknown(_) => FsmAction::None,
        }
    }

    /// `RPTACK` — a salt, or a step of the chain acknowledged. Which one it
    /// is, the state says.
    ///
    /// After `RPTK` and after `RPTC` the four bytes are the master echoing
    /// **our** id back (`hblink.py` joins `RPTACK` with `_peer_id`), so an
    /// ack naming somebody else is not ours and must not advance the chain:
    /// one master serves many peers on one port, and a fan-out mistake at
    /// the far end would otherwise walk us into `Linked` on another
    /// station's acknowledgement.
    fn on_ack(&mut self, id: u32, salt: Option<[u8; 4]>, now: Instant) -> FsmAction {
        match self.state {
            LinkState::LoggingIn => {
                let Some(salt) = salt else {
                    // A bare RPTACK with no four bytes cannot be a salt, and
                    // guessing one would send a digest that can never match.
                    return FsmAction::None;
                };
                self.state = LinkState::Authenticating;
                self.stage_since = Some(now);
                FsmAction::Send(wire::auth(self.id, salt, &self.password).to_vec())
            }
            LinkState::Authenticating if id == self.id.get() => {
                self.state = LinkState::Configuring;
                self.stage_since = Some(now);
                FsmAction::Send(wire::config(self.id, &self.fields).to_vec())
            }
            LinkState::Configuring if id == self.id.get() => {
                self.state = LinkState::Linked;
                self.stage_since = None;
                self.last_ping = Some(now);
                FsmAction::Linked
            }
            _ => FsmAction::None,
        }
    }

    /// `MSTNAK` — the diagnosis is the stage it arrived at.
    fn on_nak(&mut self) -> FsmAction {
        let stage = match self.state {
            LinkState::LoggingIn => FailureStage::Login,
            LinkState::Authenticating => FailureStage::Auth,
            LinkState::Configuring => FailureStage::Config,
            LinkState::Linked => FailureStage::Session,
            // A NAK answering our own goodbye, or arriving at a link that
            // has already ended. hblink.py's master NAKs every RPTCL it
            // deletes a peer for; that is a receipt, not a refusal.
            _ => return FsmAction::None,
        };
        self.state = LinkState::Failed;
        FsmAction::Failed(stage)
    }

    /// A `DMRD` — ours, or somebody else's room.
    ///
    /// The master relays what its peers send; the client decides which room
    /// it is listening to. A homebrew master has no per-peer talkgroup to
    /// filter on, so this test lives here and nowhere else.
    fn on_data(&mut self, data: Box<DataPacket>) -> FsmAction {
        if self.state != LinkState::Linked {
            return FsmAction::None;
        }
        let ours = match data.call_type {
            CallType::Group => data.dst_id == self.talkgroup && data.slot == self.slot,
            // A private call addressed to this radio is ours whichever slot
            // carried it: the slot is which room we are listening in, and a
            // call to our own ID is not addressed to a room.
            CallType::Private => data.dst_id == self.id.get(),
        };
        if ours {
            FsmAction::Data(data)
        } else {
            FsmAction::None
        }
    }

    /// Drives the clock: keeps a live link alive, restarts a stalled
    /// handshake, or gives up.
    pub fn tick(&mut self, now: Instant) -> FsmAction {
        match self.state {
            LinkState::LoggingIn | LinkState::Authenticating | LinkState::Configuring => {
                if self
                    .stage_since
                    .is_some_and(|since| now.duration_since(since) >= LOGIN_RETRY)
                {
                    // Start the chain over rather than repeating the step:
                    // the salt behind a stalled RPTK is stale.
                    let login = self.connect(now);
                    return login
                        .into_iter()
                        .next()
                        .map_or(FsmAction::None, FsmAction::Send);
                }
                FsmAction::None
            }
            LinkState::Linked => {
                if self
                    .last_rx
                    .is_some_and(|last| now.duration_since(last) >= self.link_timeout)
                {
                    self.state = LinkState::Failed;
                    return FsmAction::Failed(FailureStage::Timeout);
                }
                let due = self
                    .last_ping
                    .is_none_or(|last| now.duration_since(last) >= self.ping_interval);
                if due {
                    self.last_ping = Some(now);
                    return FsmAction::Send(wire::ping(self.id).to_vec());
                }
                FsmAction::None
            }
            // Idle, Closing and Failed have no clock: nothing is expected
            // and nothing is owed.
            _ => FsmAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{self, ConfigFields, RadioId, Timeslot};

    const TG: u32 = 31_313;
    const PASSWORD: &str = "passw0rd";

    fn fsm() -> DmrFsm {
        DmrFsm::new(
            RadioId::new(3_153_591).expect("id"),
            TG,
            Timeslot::Ts2,
            ConfigFields::softclient("KC0ABC", "astar test"),
            PASSWORD.to_string(),
        )
    }

    fn ack(payload: [u8; 4]) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..6].copy_from_slice(b"RPTACK");
        b[6..].copy_from_slice(&payload);
        b
    }

    fn nak(id: RadioId) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..6].copy_from_slice(b"MSTNAK");
        b[6..].copy_from_slice(&id.to_be_bytes());
        b
    }

    /// Walk a fresh FSM all the way to `Linked` against scripted replies.
    fn linked(now: Instant) -> DmrFsm {
        let mut f = fsm();
        f.connect(now);
        assert_eq!(
            f.on_packet(&ack([0x0A, 0x7E, 0xD4, 0x98]), now),
            FsmAction::Send(wire::auth(f.radio_id(), [0x0A, 0x7E, 0xD4, 0x98], PASSWORD).to_vec())
        );
        let echo = ack(f.radio_id().to_be_bytes());
        let FsmAction::Send(cfg) = f.on_packet(&echo, now) else {
            panic!("config")
        };
        assert_eq!(&cfg[..4], b"RPTC");
        assert_eq!(f.on_packet(&echo, now), FsmAction::Linked);
        f
    }

    #[test]
    fn connect_sends_one_login_and_nothing_else() {
        // Unlike YSF and NXDN there is no triple-poll here: the homebrew
        // handshake is a strict request/response chain, and a duplicate RPTL
        // restarts the master's salt.
        let mut f = fsm();
        let out = f.connect(Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..4], b"RPTL");
        assert_eq!(f.state(), LinkState::LoggingIn);
    }

    #[test]
    fn the_handshake_walks_login_auth_config_linked() {
        let f = linked(Instant::now());
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn the_salt_is_taken_from_the_ack_verbatim() {
        // DMRGateway: ::memcpy(m_salt, m_buffer + 6U, sizeof(uint32_t)).
        // Round-tripping it through a u32 would work by accident on a
        // big-endian host and silently fail everywhere else.
        let now = Instant::now();
        let mut f = fsm();
        f.connect(now);
        let salt = [0x00, 0x00, 0x00, 0x2A];
        let FsmAction::Send(auth) = f.on_packet(&ack(salt), now) else {
            panic!("auth")
        };
        assert_eq!(&auth[8..], &wire::auth_digest(salt, PASSWORD));
    }

    #[test]
    fn a_nak_fails_the_link_and_says_which_stage() {
        // "The password is wrong" and "your ID is not permitted here" are the
        // two things an operator actually needs told apart, and the only
        // difference on the wire is WHEN the NAK arrived.
        let now = Instant::now();
        let refusal = nak(RadioId::new(3_153_591).expect("id"));

        let mut f = fsm();
        f.connect(now);
        assert_eq!(
            f.on_packet(&refusal, now),
            FsmAction::Failed(FailureStage::Login)
        );

        let mut f = fsm();
        f.connect(now);
        f.on_packet(&ack([1, 2, 3, 4]), now);
        assert_eq!(
            f.on_packet(&refusal, now),
            FsmAction::Failed(FailureStage::Auth)
        );
        assert_eq!(f.state(), LinkState::Failed);
    }

    #[test]
    fn a_nak_answering_our_own_goodbye_is_not_a_failure() {
        // docs/design/dmr-wire.md sec.1: hblink.py's master deletes the peer
        // and NAKs its RPTCL. Reading that as an error would end every clean
        // disconnection with a red light.
        let now = Instant::now();
        let mut f = linked(now);
        f.close(now);
        assert_eq!(f.on_packet(&nak(f.radio_id()), now), FsmAction::None);
        assert_eq!(f.state(), LinkState::Closing);
    }

    #[test]
    fn an_ack_that_is_truncated_or_names_another_station_does_not_advance_the_chain() {
        // One master serves many peers on one port. A short RPTACK carries no
        // id at all, and one carrying somebody else's is their receipt, not
        // ours; either walking us on a step would put the whole handshake out
        // of phase with the master's idea of it.
        let now = Instant::now();
        let mut f = fsm();
        f.connect(now);
        f.on_packet(&ack([0x0A, 0x7E, 0xD4, 0x98]), now);
        assert_eq!(f.state(), LinkState::Authenticating);

        assert_eq!(f.on_packet(&ack([1, 2, 3, 4])[..8], now), FsmAction::None);
        assert_eq!(f.state(), LinkState::Authenticating);

        let someone_else = ack(RadioId::new(3_153_592).expect("id").to_be_bytes());
        assert_eq!(f.on_packet(&someone_else, now), FsmAction::None);
        assert_eq!(f.state(), LinkState::Authenticating);

        // Configuring is the same story, one step along.
        let ours = ack(f.radio_id().to_be_bytes());
        assert!(matches!(f.on_packet(&ours, now), FsmAction::Send(_)));
        assert_eq!(f.state(), LinkState::Configuring);
        assert_eq!(f.on_packet(&someone_else, now), FsmAction::None);
        assert_eq!(f.state(), LinkState::Configuring);
        assert_eq!(f.on_packet(&ours, now), FsmAction::Linked);
    }

    #[test]
    fn junk_does_not_hold_a_dead_link_open() {
        // A datagram this protocol has no reading for is not proof the master
        // is alive. If it were, anybody who could reach the socket could keep
        // a link that died sixty seconds ago showing as up.
        let t0 = Instant::now();
        let mut f = linked(t0);
        for offset in [10, 20, 30, 40, 50, 59] {
            assert_eq!(
                f.on_packet(b"RPTSBKN   ", t0 + Duration::from_secs(offset)),
                FsmAction::None
            );
        }
        assert_eq!(
            f.tick(t0 + Duration::from_secs(60)),
            FsmAction::Failed(FailureStage::Timeout)
        );
    }

    #[test]
    fn a_master_close_fails_the_link_as_closed() {
        let now = Instant::now();
        let mut f = linked(now);
        let mut cl = [0u8; 9];
        cl[..5].copy_from_slice(b"MSTCL");
        cl[5..].copy_from_slice(&f.radio_id().to_be_bytes());
        assert_eq!(
            f.on_packet(&cl, now),
            FsmAction::Failed(FailureStage::Closed)
        );
    }

    #[test]
    fn a_ping_is_due_every_five_seconds_once_linked() {
        // DroidStar dmr.cpp: setup_connection -- m_ping_timer->start(5000).
        let t0 = Instant::now();
        let mut f = linked(t0);
        assert_eq!(f.tick(t0 + Duration::from_secs(4)), FsmAction::None);
        let FsmAction::Send(p) = f.tick(t0 + Duration::from_secs(5)) else {
            panic!("ping")
        };
        assert_eq!(&p[..7], b"RPTPING");
    }

    #[test]
    fn nothing_is_pinged_before_the_link_is_up() {
        // A ping during the handshake is a packet the master answers with
        // MSTNAK, which would fail a login that was about to succeed.
        let t0 = Instant::now();
        let mut f = fsm();
        f.connect(t0);
        assert_eq!(f.tick(t0 + Duration::from_secs(6)), FsmAction::None);
    }

    #[test]
    fn a_stalled_handshake_retries_the_login_rather_than_waiting_forever() {
        // DMRGateway/DMRNetwork.cpp: m_retryTimer(1000U, 10U) -- clock()
        // writes a fresh RPTL and goes back to WAITING_LOGIN whenever the
        // chain stalls, because a stale salt cannot be authorised anyway.
        let t0 = Instant::now();
        let mut f = fsm();
        f.connect(t0);
        f.on_packet(&ack([1, 2, 3, 4]), t0);
        assert_eq!(f.state(), LinkState::Authenticating);
        assert_eq!(f.tick(t0 + Duration::from_secs(9)), FsmAction::None);
        let FsmAction::Send(again) = f.tick(t0 + LOGIN_RETRY) else {
            panic!("a fresh login")
        };
        assert_eq!(&again[..4], b"RPTL");
        assert_eq!(f.state(), LinkState::LoggingIn);
    }

    #[test]
    fn a_retried_handshake_still_signs_the_new_salt_with_the_real_password() {
        // The reason the password outlives the first RPTK: a stalled chain
        // starts over, and an FSM that had dropped it would answer the fresh
        // salt with a digest of nothing.
        let t0 = Instant::now();
        let mut f = fsm();
        f.connect(t0);
        f.on_packet(&ack([1, 2, 3, 4]), t0);
        f.tick(t0 + LOGIN_RETRY);
        let salt = [9, 9, 9, 9];
        let FsmAction::Send(auth) = f.on_packet(&ack(salt), t0 + LOGIN_RETRY) else {
            panic!("auth")
        };
        assert_eq!(&auth[8..], &wire::auth_digest(salt, PASSWORD));
    }

    #[test]
    fn sixty_seconds_without_a_pong_fails_the_link() {
        // DMRGateway/DMRNetwork.cpp: m_timeoutTimer(1000U, 60U).
        let t0 = Instant::now();
        let mut f = linked(t0);
        assert_eq!(
            f.tick(t0 + Duration::from_secs(60)),
            FsmAction::Failed(FailureStage::Timeout)
        );
        assert_eq!(f.state(), LinkState::Failed);
    }

    #[test]
    fn a_pong_restarts_the_timeout() {
        // The pong at t0+50 moves the deadline that would otherwise have
        // fallen at t0+60. A ping is due at t0+100 as well, so it is drained
        // first and what is left to assert is the absence of a timeout.
        let t0 = Instant::now();
        let mut f = linked(t0);
        let mut pong = [0u8; 11];
        pong[..7].copy_from_slice(b"MSTPONG");
        pong[7..].copy_from_slice(&f.radio_id().to_be_bytes());
        f.on_packet(&pong, t0 + Duration::from_secs(50));
        let due = f.tick(t0 + Duration::from_secs(100));
        assert!(
            matches!(due, FsmAction::Send(_)),
            "a ping, not a timeout: {due:?}"
        );
        assert_eq!(f.tick(t0 + Duration::from_secs(100)), FsmAction::None);
        assert_eq!(f.state(), LinkState::Linked);
    }

    #[test]
    fn a_frame_for_our_talkgroup_and_slot_is_delivered_and_anything_else_is_not() {
        // The master relays what its peers send; the client is what decides
        // which room it is listening to. A frame for TG 91 on TS1 arriving
        // while we are on TG 31313 TS2 is not our audio.
        let now = Instant::now();
        let mut f = linked(now);
        let mine = wire::DataPacket {
            seq: 0,
            src_id: 4242,
            dst_id: TG,
            peer_id: 4242,
            slot: Timeslot::Ts2,
            call_type: wire::CallType::Group,
            frame_type: wire::FrameType::VoiceSync,
            stream_id: [1, 2, 3, 4],
            burst: [0u8; wire::BURST_LEN],
            ber: 0,
            rssi: 0,
        };
        match f.on_packet(&wire::data(&mine).expect("ids in range"), now) {
            FsmAction::Data(d) => assert_eq!(d.src_id, 4242),
            other => panic!("expected data, got {other:?}"),
        }

        let mut wrong_tg = mine.clone();
        wrong_tg.dst_id = 91;
        assert_eq!(
            f.on_packet(&wire::data(&wrong_tg).expect("ids in range"), now),
            FsmAction::None
        );

        let mut wrong_slot = mine.clone();
        wrong_slot.slot = Timeslot::Ts1;
        assert_eq!(
            f.on_packet(&wire::data(&wrong_slot).expect("ids in range"), now),
            FsmAction::None
        );

        let mut private = mine.clone();
        private.call_type = wire::CallType::Private;
        private.dst_id = f.radio_id().get();
        match f.on_packet(&wire::data(&private).expect("ids in range"), now) {
            FsmAction::Data(d) => assert_eq!(d.call_type, wire::CallType::Private),
            other => panic!("a private call addressed to us is ours: {other:?}"),
        }
    }

    #[test]
    fn a_frame_arriving_before_the_link_is_up_is_not_delivered() {
        // Not pedantry: a DMRD reaching a half-finished handshake is either
        // another peer's traffic the master fanned out early or a stranger
        // spraying the port. Neither is our audio.
        let now = Instant::now();
        let mut f = fsm();
        f.connect(now);
        let frame = wire::DataPacket {
            seq: 0,
            src_id: 4242,
            dst_id: TG,
            peer_id: 4242,
            slot: Timeslot::Ts2,
            call_type: wire::CallType::Group,
            frame_type: wire::FrameType::VoiceSync,
            stream_id: [1, 2, 3, 4],
            burst: [0u8; wire::BURST_LEN],
            ber: 0,
            rssi: 0,
        };
        assert_eq!(
            f.on_packet(&wire::data(&frame).expect("ids in range"), now),
            FsmAction::None
        );
    }

    #[test]
    fn close_sends_rptcl_and_leaves_closing() {
        let now = Instant::now();
        let mut f = linked(now);
        let out = f.close(now);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..5], b"RPTCL");
        assert_eq!(f.state(), LinkState::Closing);
    }

    #[test]
    fn the_password_never_appears_in_debug_output() {
        // CLAUDE.md: secrets are connect-time in-args only -- never in an
        // error, a snapshot or a log. `{:?}` on a live FSM is the easiest way
        // for one to escape, so it is pinned rather than trusted.
        let f = linked(Instant::now());
        let rendered = format!("{f:?}");
        assert!(
            !rendered.contains(PASSWORD),
            "the password reached Debug output"
        );
    }
}
