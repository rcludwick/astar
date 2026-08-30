// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A live `YSFReflector` link: the socket and the thread that drive
//! [`astar_ysf::YsfFsm`] (iax-e8a4, designed in `docs/design/ysf-network.md`).
//!
//! `astar-ysf` is deliberately pure — the FSM decides, and hands back bytes
//! to send — so something has to own a `UdpSocket`, feed it datagrams, and
//! call `tick`. That is this module, and it is the piece the design named as
//! missing: "the protocol crate is built; the vocoder and the session are
//! not."
//!
//! # There is no audio here, and that is the point
//!
//! YSF voice is **AMBE+2**, which on astar means the AMBE-3000 in a dongle
//! and nothing else. No vocoder path exists yet, so this link **receives
//! frames and never sends one**: it polls to hold the link open, reports who
//! is talking from the `YSFD` header, and hands the ninety payload bytes
//! nowhere.
//!
//! That is a smaller thing than it sounds, and more useful than it sounds.
//! The header is not the payload: `DataPacket::source` is the callsign of
//! whoever is transmitting, in clear, with no vocoder involved. So a link
//! can honestly answer "is this reflector alive, and who is on it" long
//! before it can answer "what did they say".
//!
//! What it must not do is imply the second. There is no TX path here at all
//! — not a stubbed one, not one that sends silence — because a transmit
//! function that put nothing on the air would be a worse lie than an absent
//! one, and on a network where other people are listening.
//!
//! # Threading
//!
//! One thread per link, owning the socket. It blocks on `recv_from` with a
//! read timeout so `tick` still runs when the reflector goes quiet — that
//! timeout is what makes the poll cadence and the link timeout work at all.
//! The control side sees an `AtomicU*` snapshot and a mutex-guarded
//! last-heard string, the same arrangement the audio lanes use.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_ysf::{FsmAction, LinkState, YsfFsm};

use crate::session::ConsoleError;

/// How long the socket blocks before the loop runs `tick` anyway.
///
/// Shorter than the 5 s poll interval by enough that a poll is never late by
/// a meaningful fraction of it, and long enough that an idle link is not a
/// busy loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// What the control side can see of a link, without touching the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YsfSnapshot {
    /// The link state's ABI string — `idle`, `linking`, `linked`,
    /// `unlinking`, `failed`. See [`LinkState::as_str`], which documents why
    /// these strings are not a debug convenience.
    pub link_state: &'static str,
    /// Callsign of whoever transmitted most recently, if anyone has.
    /// Read from the `YSFD` header, so it needs no vocoder.
    pub last_heard: Option<String>,
    /// Radio frames received since the link came up. A liveness counter —
    /// a link that is up but silent and one that is receiving look the same
    /// from `link_state` alone.
    pub frames_rx: u64,
    /// Whether a transmission is in progress, from the header's end flag.
    pub receiving: bool,
}

/// A live link to one `YSFReflector`.
///
/// `Debug` reports the snapshot rather than the internals: the thread handle
/// and the socket are not something a caller can act on, and the link state
/// is.
pub struct YsfLink {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct Shared {
    /// `LinkState` as its discriminant index; see `state_from_u32`.
    link_state: AtomicU32,
    last_heard: Mutex<Option<String>>,
    frames_rx: AtomicU64,
    receiving: AtomicBool,
    stop: AtomicBool,
}

/// `LinkState` has no numeric repr of its own — it is a protocol type and
/// does not need one — so the mapping lives here, next to the atomic it
/// exists for, rather than being pushed into the protocol crate.
fn state_index(s: LinkState) -> u32 {
    match s {
        LinkState::Idle => 0,
        LinkState::Linking => 1,
        LinkState::Linked => 2,
        LinkState::Unlinking => 3,
        LinkState::Failed => 4,
    }
}

fn state_str(i: u32) -> &'static str {
    match i {
        1 => LinkState::Linking.as_str(),
        2 => LinkState::Linked.as_str(),
        3 => LinkState::Unlinking.as_str(),
        4 => LinkState::Failed.as_str(),
        _ => LinkState::Idle.as_str(),
    }
}

impl YsfLink {
    /// Resolve `host`, bind a local socket, and start polling.
    ///
    /// `options` is the YCS room request (`set_options`); `None` for a plain
    /// `YSFReflector`.
    ///
    /// # Errors
    /// [`ConsoleError::Ysf`] if the callsign is not a valid YSF callsign, if
    /// `host` does not resolve, or if the socket cannot be bound.
    pub fn connect(
        host: &str,
        callsign: &str,
        options: Option<String>,
    ) -> Result<YsfLink, ConsoleError> {
        let mut fsm =
            YsfFsm::new(callsign).map_err(|e| ConsoleError::Ysf(format!("callsign: {e}")))?;
        fsm.set_options(options);

        let addr: SocketAddr = host
            .to_socket_addrs()
            .map_err(|e| ConsoleError::Ysf(format!("resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| ConsoleError::Ysf(format!("resolve {host}: no addresses")))?;

        // Bind to the unspecified address on an ephemeral port, matching the
        // family of whatever we resolved to.
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).map_err(|e| ConsoleError::Ysf(format!("bind: {e}")))?;
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|e| ConsoleError::Ysf(format!("socket timeout: {e}")))?;

        let shared = Arc::new(Shared {
            link_state: AtomicU32::new(state_index(LinkState::Idle)),
            last_heard: Mutex::new(None),
            frames_rx: AtomicU64::new(0),
            receiving: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });

        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("astar-ysf-link".into())
                .spawn(move || run(&socket, addr, fsm, &shared))
                .map_err(|e| ConsoleError::Ysf(format!("thread: {e}")))?
        };

        Ok(YsfLink {
            shared,
            thread: Some(thread),
        })
    }

    /// Current link state, cheap enough to poll.
    #[must_use]
    pub fn snapshot(&self) -> YsfSnapshot {
        YsfSnapshot {
            link_state: state_str(self.shared.link_state.load(Ordering::Relaxed)),
            last_heard: self.shared.last_heard.lock().map_or(None, |g| g.clone()),
            frames_rx: self.shared.frames_rx.load(Ordering::Relaxed),
            receiving: self.shared.receiving.load(Ordering::Relaxed),
        }
    }

    /// Send the unlink and stop the thread. Consumes the link.
    pub fn disconnect(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl std::fmt::Debug for YsfLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YsfLink")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for YsfLink {
    /// Dropping a link unlinks it. A `YSFReflector` would otherwise keep the
    /// client until its own timeout expires, and a caller who dropped the
    /// handle has plainly finished with it.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The link thread's body. The socket is owned by the spawned closure for
/// the thread's lifetime and borrowed here.
fn run(socket: &UdpSocket, addr: SocketAddr, mut fsm: YsfFsm, shared: &Arc<Shared>) {
    let publish = |fsm: &YsfFsm| {
        shared
            .link_state
            .store(state_index(fsm.state()), Ordering::Relaxed);
    };

    for datagram in fsm.connect(Instant::now()) {
        if socket.send_to(&datagram, addr).is_err() {
            shared
                .link_state
                .store(state_index(LinkState::Failed), Ordering::Relaxed);
            return;
        }
    }
    publish(&fsm);

    let mut buf = [0_u8; astar_ysf::reflector::MAX_DATAGRAM];
    while !shared.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == addr => {
                let action = fsm.on_packet(&buf[..n], Instant::now());
                if !handle(&action, socket, addr, shared) {
                    break;
                }
            }
            // A datagram from somewhere else on our ephemeral port. Ignore
            // it rather than feeding the FSM a stranger's bytes.
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => break,
        }
        let action = fsm.tick(Instant::now());
        if !handle(&action, socket, addr, shared) {
            break;
        }
        publish(&fsm);
    }

    // Best effort: the reflector drops us on its own timeout anyway, and a
    // failed send here is not worth reporting to a caller that has gone.
    for datagram in fsm.unlink(Instant::now()) {
        let _ = socket.send_to(&datagram, addr);
    }
    shared
        .link_state
        .store(state_index(LinkState::Idle), Ordering::Relaxed);
}

/// Apply one action. Returns false when the loop should stop.
fn handle(action: &FsmAction, socket: &UdpSocket, addr: SocketAddr, shared: &Arc<Shared>) -> bool {
    match action {
        FsmAction::None | FsmAction::Linked => true,
        FsmAction::Send(bytes) => socket.send_to(bytes, addr).is_ok(),
        FsmAction::Data(packet) => {
            shared.frames_rx.fetch_add(1, Ordering::Relaxed);
            shared.receiving.store(!packet.end, Ordering::Relaxed);
            // The source callsign is in the header, in clear — no vocoder
            // involved. `to_trimmed_string` drops the space padding the
            // ten-byte wire field requires.
            let source = packet.source.to_trimmed_string();
            if !source.is_empty()
                && let Ok(mut slot) = shared.last_heard.lock()
            {
                *slot = Some(source);
            }
            true
        }
        FsmAction::Unlinked | FsmAction::Timeout => {
            shared.receiving.store(false, Ordering::Relaxed);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_ysf::Reflector;

    /// Everything here binds `127.0.0.1` and talks to a reflector this test
    /// started. Nothing reaches a real network — see CLAUDE.md's on-air
    /// safety rule, which this crate is squarely inside.
    fn loopback() -> (astar_ysf::ReflectorHandle, SocketAddr) {
        let r = Reflector::bind("127.0.0.1:0".parse().expect("v4")).expect("bind reflector");
        let addr = r.local_addr();
        (r.run(), addr)
    }

    fn wait_for(link: &YsfLink, want: &str, within: Duration) -> YsfSnapshot {
        let deadline = Instant::now() + within;
        loop {
            let snap = link.snapshot();
            if snap.link_state == want || Instant::now() > deadline {
                return snap;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The whole point of the link: polls go out, the reflector's poll comes
    /// back, and that round trip is the acknowledgement. Nothing about this
    /// needs a vocoder.
    #[test]
    fn linking_to_a_reflector_reaches_linked() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");

        let snap = wait_for(&link, "linked", Duration::from_secs(5));
        assert_eq!(snap.link_state, "linked", "link never came up: {snap:?}");
        assert_eq!(snap.frames_rx, 0, "a poll is not a radio frame");
        assert!(snap.last_heard.is_none(), "nobody has transmitted");

        link.disconnect();
        reflector.shutdown();
    }

    /// A callsign the wire format cannot carry is refused before a socket is
    /// bound — the error names the problem rather than surfacing as a failed
    /// link a caller has to diagnose.
    #[test]
    fn an_impossible_callsign_is_refused_at_connect() {
        let err = YsfLink::connect("127.0.0.1:1", "WAY-TOO-LONG-FOR-YSF", None)
            .expect_err("should refuse");
        let text = err.to_string();
        assert!(text.contains("ysf:"), "{text}");
        assert!(text.contains("callsign"), "{text}");
    }

    /// An unresolvable host fails at connect, not later and not silently.
    #[test]
    fn an_unresolvable_host_is_refused_at_connect() {
        let err = YsfLink::connect("no-such-host.invalid:42000", "N0CALL", None)
            .expect_err("should refuse");
        assert!(err.to_string().contains("resolve"), "{err}");
    }

    /// Dropping the handle unlinks and joins the thread. A link that outlived
    /// its handle would hold a socket and keep polling a reflector nobody is
    /// listening to.
    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let (reflector, addr) = loopback();
        {
            let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
            let _ = wait_for(&link, "linked", Duration::from_secs(5));
        }
        // The reflector drops a client on its own timeout too, so this is
        // asserting the thread joined rather than the reflector noticing.
        reflector.shutdown();
    }

    /// The snapshot is readable immediately, before the link has settled —
    /// a UI polls it from the first frame it draws.
    #[test]
    fn snapshot_is_readable_before_the_link_comes_up() {
        let (reflector, addr) = loopback();
        let link = YsfLink::connect(&addr.to_string(), "N0CALL", None).expect("connect");
        let snap = link.snapshot();
        assert!(
            ["idle", "linking", "linked"].contains(&snap.link_state),
            "unexpected early state {snap:?}"
        );
        link.disconnect();
        reflector.shutdown();
    }
}
