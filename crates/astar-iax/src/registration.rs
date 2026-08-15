// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! High-level registration API. Spawns one blocking `mio` thread per
//! `Registration`, drives `RegFsm` + `Reliability`, surfaces lifecycle
//! events over a `std::sync::mpsc` channel.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_iax_core::frame::parse_lenient;
use astar_iax_core::session::auth::{Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::TimerKind;
use astar_iax_core::session::reg::{
    RegAction, RegAppCommand, RegAppEvent, RegEvent, RegFailReason, RegFsm, RegState,
    RegisterOptions,
};
use astar_iax_core::session::reliability::{Reliability, ReliabilityConfig, RxOutcome};

use crate::transport::{LinkSocket, NetStack, OsNetStack};

/// Public-facing event surface for a single registration.
#[derive(Debug, Clone)]
pub enum RegistrationEvent {
    Registering,
    Registered {
        refresh: Duration,
        apparent_addr: Option<SocketAddr>,
    },
    Refreshing,
    Refreshed,
    Failed(RegFailReason),
    Released,
}

impl From<RegAppEvent> for RegistrationEvent {
    fn from(e: RegAppEvent) -> Self {
        match e {
            RegAppEvent::Registering => Self::Registering,
            RegAppEvent::Registered {
                refresh,
                apparent_addr,
            } => Self::Registered {
                refresh,
                apparent_addr,
            },
            RegAppEvent::Refreshing => Self::Refreshing,
            RegAppEvent::Refreshed => Self::Refreshed,
            RegAppEvent::Failed(r) => Self::Failed(r),
            RegAppEvent::Released => Self::Released,
        }
    }
}

/// Builder for a registration. Does not own any thread until `register()`.
pub struct Registrar {
    #[allow(clippy::struct_field_names)]
    registrar: SocketAddr,
    username: String,
    password: Arc<Secret>,
    options: RegisterOptions,
    /// Transport seam (iax-b6f5): the socket factory the runtime binds from.
    /// [`OsNetStack`] (plain OS UDP) by default — byte-identical behavior.
    net: Arc<dyn NetStack>,
}

impl Registrar {
    #[must_use]
    pub fn new(registrar: SocketAddr, username: impl Into<String>, password: Arc<Secret>) -> Self {
        Self {
            registrar,
            username: username.into(),
            password,
            options: RegisterOptions::default(),
            net: Arc::new(OsNetStack),
        }
    }

    #[must_use]
    pub fn with_options(mut self, options: RegisterOptions) -> Self {
        self.options = options;
        self
    }

    /// Transport seam (iax-927a): bind the runtime's socket from `net` instead
    /// of the OS default — e.g. [`crate::Manager::net_stack`] so the
    /// registration exchange rides the Manager's `WireGuard` tunnel.
    #[must_use]
    pub fn with_net(mut self, net: Arc<dyn NetStack>) -> Self {
        self.net = net;
        self
    }

    /// Spawn the blocking mio thread. Returns the handle and an event receiver.
    pub fn register(self) -> io::Result<(Registration, mpsc::Receiver<RegistrationEvent>)> {
        use mio::{Poll, Token, Waker};
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel::<RuntimeCommand>();
        let registrar = self.registrar;
        let creds = Credentials {
            username: self.username,
            password: self.password,
            allowed_methods: astar_iax_core::session::auth::AuthMethods::MD5,
        };
        let options = self.options;
        let net = self.net;
        // Pre-create the Poll and Waker on the calling thread so we can hand
        // back a clone of the waker that wakes the runtime out of poll() when
        // a command arrives.
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
        let waker_for_caller = Arc::clone(&waker);
        let handle = thread::Builder::new()
            .name(format!("iax-reg-{registrar}"))
            .spawn(move || {
                run_loop(
                    poll, waker, net, registrar, creds, options, event_tx, cmd_rx,
                );
            })?;
        Ok((
            Registration {
                cmd_tx,
                waker: waker_for_caller,
                handle: Some(handle),
            },
            event_rx,
        ))
    }
}

#[derive(Debug)]
enum RuntimeCommand {
    Deregister,
    Shutdown,
}

pub struct Registration {
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    waker: Arc<mio::Waker>,
    handle: Option<JoinHandle<()>>,
}

impl Registration {
    /// Initiate REGREL and block until the thread exits (either Closed or Failed).
    pub fn deregister(mut self) -> io::Result<()> {
        let _ = self.cmd_tx.send(RuntimeCommand::Deregister);
        let _ = self.waker.wake();
        if let Some(h) = self.handle.take() {
            h.join().map_err(|_| io::Error::other("thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(RuntimeCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Per-registration thread loop. mio UDP socket + std mpsc command channel +
/// vector-of-deadlines timer wheel. Single-thread, no async.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value
)]
fn run_loop(
    mut poll: mio::Poll,
    waker: Arc<mio::Waker>,
    net: Arc<dyn NetStack>,
    registrar: SocketAddr,
    creds: Credentials,
    options: RegisterOptions,
    event_tx: mpsc::Sender<RegistrationEvent>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
) {
    use mio::{Events, Token};

    const SOCK: Token = Token(0);

    let mut sock = match net.bind("0.0.0.0:0".parse().unwrap()) {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.send(RegistrationEvent::Failed(RegFailReason::NetworkError(
                e.kind(),
            )));
            return;
        }
    };
    if let Err(e) = sock.register(poll.registry(), SOCK, waker) {
        let _ = event_tx.send(RegistrationEvent::Failed(RegFailReason::NetworkError(
            e.kind(),
        )));
        return;
    }

    let our_call = CallNo::new(1).unwrap();
    let mut fsm = RegFsm::new(creds, our_call, options);
    let mut rel = Reliability::new(our_call, ReliabilityConfig::default());
    let mut timers: Vec<(Instant, TimerKind)> = Vec::new();
    let mut buf = [0u8; 4096];

    // Kick: send StartRegister to the FSM.
    let actions = fsm.handle(RegEvent::App(RegAppCommand::StartRegister {
        now: Instant::now(),
    }));
    dispatch_actions(
        actions,
        sock.as_ref(),
        registrar,
        &mut rel,
        &mut timers,
        &event_tx,
    );

    let mut events = Events::with_capacity(8);
    loop {
        // Compute poll timeout = min(next timer, 100ms safety).
        let now = Instant::now();
        let next_timer = timers.iter().map(|(at, _)| *at).min();
        let timeout = match next_timer {
            Some(t) if t > now => t - now,
            Some(_) => Duration::from_millis(0),
            None => Duration::from_millis(100),
        };

        if let Err(e) = poll.poll(&mut events, Some(timeout)) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let _ = event_tx.send(RegistrationEvent::Failed(RegFailReason::NetworkError(
                e.kind(),
            )));
            return;
        }

        // Drain commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RuntimeCommand::Deregister => {
                    let actions = fsm.handle(RegEvent::App(RegAppCommand::Deregister {
                        now: Instant::now(),
                    }));
                    dispatch_actions(
                        actions,
                        sock.as_ref(),
                        registrar,
                        &mut rel,
                        &mut timers,
                        &event_tx,
                    );
                }
                RuntimeCommand::Shutdown => return,
            }
        }

        // Drain UDP. Attempt a non-blocking drain on ANY wakeup (not just when
        // the socket token fired): behaviorally identical for OS UDP (an empty
        // socket returns `WouldBlock` immediately), and required by a
        // waker-driven [`LinkSocket`] impl whose readiness arrives via the
        // waker, not a token (iax-b6f5).
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    let bytes = buf[..n].to_vec();
                    let Ok(frame) = parse_lenient(&bytes) else {
                        continue;
                    };
                    match rel.on_frame_in(frame, Instant::now()) {
                        RxOutcome::Deliver { frame, send_ack } => {
                            if let Some(ack) = send_ack {
                                let _ = sock.send_to(&ack, registrar);
                            }
                            let actions = fsm.handle(RegEvent::Frame {
                                frame,
                                now: Instant::now(),
                            });
                            dispatch_actions(
                                actions,
                                sock.as_ref(),
                                registrar,
                                &mut rel,
                                &mut timers,
                                &event_tx,
                            );
                        }
                        RxOutcome::Consumed => {}
                        RxOutcome::Duplicate { resend_ack } => {
                            if let Some(b) = resend_ack {
                                let _ = sock.send_to(&b, registrar);
                            }
                        }
                        RxOutcome::Vnak(iseqno) => {
                            // RFC 5456 §6.9.3: the peer wants a resend
                            // from `iseqno`. iax-3b9d (mirroring
                            // iax-a307): answer it — this used to map to
                            // DeliveryFailed and killed the
                            // registration on a VNAK.
                            for bytes in rel.resend_from(iseqno) {
                                let _ = sock.send_to(&bytes, registrar);
                            }
                        }
                        RxOutcome::GaveUp { oseqno } => {
                            let actions = fsm.handle(RegEvent::DeliveryFailed { oseqno });
                            dispatch_actions(
                                actions,
                                sock.as_ref(),
                                registrar,
                                &mut rel,
                                &mut timers,
                                &event_tx,
                            );
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    let _ = event_tx.send(RegistrationEvent::Failed(RegFailReason::NetworkError(
                        e.kind(),
                    )));
                    return;
                }
            }
        }

        // Drain expired timers.
        let now = Instant::now();
        let mut fired = Vec::new();
        timers.retain(|(at, kind)| {
            if *at <= now {
                fired.push(*kind);
                false
            } else {
                true
            }
        });
        for kind in fired {
            use rand::Rng;
            let salt: u32 = rand::thread_rng().r#gen();
            let actions = fsm.handle(RegEvent::Timer {
                kind,
                now,
                jitter_salt: salt,
            });
            dispatch_actions(
                actions,
                sock.as_ref(),
                registrar,
                &mut rel,
                &mut timers,
                &event_tx,
            );
        }

        // Reliability tick.
        let tick = rel.tick(now);
        for bytes in tick.retransmit {
            let _ = sock.send_to(&bytes, registrar);
        }
        for oseqno in tick.gave_up {
            let actions = fsm.handle(RegEvent::DeliveryFailed { oseqno });
            dispatch_actions(
                actions,
                sock.as_ref(),
                registrar,
                &mut rel,
                &mut timers,
                &event_tx,
            );
        }

        // Exit if FSM reached a terminal state.
        if matches!(fsm.state(), RegState::Closed | RegState::Failed(_)) {
            return;
        }
    }
}

fn dispatch_actions(
    actions: smallvec::SmallVec<[RegAction; 4]>,
    sock: &dyn LinkSocket,
    registrar: SocketAddr,
    rel: &mut Reliability,
    timers: &mut Vec<(Instant, TimerKind)>,
    event_tx: &mpsc::Sender<RegistrationEvent>,
) {
    let now = Instant::now();
    for action in actions {
        match action {
            RegAction::SendReliable(frame) => {
                let bytes = rel.enqueue(frame, now);
                let _ = sock.send_to(&bytes, registrar);
            }
            RegAction::SetPeerCall(peer) => {
                rel.set_peer_call(peer);
            }
            RegAction::ResetReliability => {
                // Refresh round (iax-177d): brand-new transaction — fresh
                // seqnos and dest_call=0 until the registrar re-challenges.
                rel.reset_transaction();
            }
            RegAction::SetTimer(kind, dur) => {
                timers.retain(|(_, k)| *k != kind);
                timers.push((now + dur, kind));
            }
            RegAction::CancelTimer(kind) => {
                timers.retain(|(_, k)| *k != kind);
            }
            RegAction::AppEvent(e) => {
                let _ = event_tx.send(RegistrationEvent::from(e));
            }
            RegAction::LogInvalid { reason } => {
                tracing::debug!(target: "astar_iax::registration", reason);
            }
        }
    }
}
