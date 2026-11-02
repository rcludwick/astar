// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Registration against a registrar that requires call tokens, the way
//! `chan_iax2` does (nodebench findings 2 and 8).
//!
//! The fake registrar keeps chan_iax2's rules:
//!
//! - a REGREQ with an empty CALLTOKEN IE is answered with CALLTOKEN and
//!   allocates nothing;
//! - a REGREQ carrying the token allocates a fresh call number every time it
//!   arrives (chan_iax2 never matches a `dest_call = 0` REGREQ to an existing
//!   call), and that call expects `oseqno` 0: anything else gets a VNAK and
//!   the call number stays allocated;
//! - the MD5 REGREQ to the allocated call must carry `oseqno` 1.
//!
//! Against ASL3 the old client sent its token REGREQ with `oseqno` 3 (the
//! empty REGREQs before it had used 0-2), took the VNAK as a request to
//! resend everything in flight, and looped: about 2,000 call numbers in 2 s.

#![allow(
    clippy::too_many_lines,
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::single_match
)]

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astar_iax::{Registrar, RegistrationEvent};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::Secret;
use astar_iax_core::subclass::{FrameType, IaxCommand};

const TOKEN: &[u8] = b"1760000000?0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct Counters {
    allocated: AtomicUsize,
    vnaks: AtomicUsize,
    token_regreq_oseqnos: std::sync::Mutex<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Behave like chan_iax2 after `drop_first` REGREQs are lost.
    Normal { drop_first: usize },
    /// VNAK every token REGREQ, whatever its sequence number.
    AlwaysVnak,
}

fn send(sock: &UdpSocket, to: SocketAddr, f: FullFrame<'_>) {
    let mut b = Vec::new();
    encode(&Frame::Full(Box::new(f)), &mut b).unwrap();
    sock.send_to(&b, to).unwrap();
}

fn frame(
    source_call: u16,
    dest_call: u16,
    oseqno: u8,
    iseqno: u8,
    cmd: IaxCommand,
    ies: Ies<'_>,
) -> FullFrame<'_> {
    FullFrame {
        source_call,
        dest_call,
        retransmission: false,
        timestamp: 0,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies,
        payload: &[],
    }
}

fn spawn_registrar(
    mode: Mode,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut seen_regreqs = 0usize;
        let mut next_callno: u16 = 100;
        while !stop.load(Ordering::Relaxed) {
            let (n, client) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if !matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq)) {
                continue; // ACKs and the like
            }
            seen_regreqs += 1;
            if let Mode::Normal { drop_first } = mode
                && seen_regreqs <= drop_first
            {
                continue; // the registrar is not up yet
            }
            let mut ie_bytes = Vec::new();
            ff.ies.encode(&mut ie_bytes).unwrap();
            let ies = Ies::parse(&ie_bytes).unwrap();
            let client_call = ff.source_call;
            if ff.dest_call == 0 {
                match ies.calltoken {
                    Some([]) | None => {
                        let reply = Ies {
                            calltoken: Some(TOKEN),
                            ..Ies::empty()
                        };
                        send(
                            &sock,
                            client,
                            frame(1, client_call, 0, 1, IaxCommand::CallToken, reply),
                        );
                    }
                    Some(t) if t == TOKEN => {
                        counters
                            .token_regreq_oseqnos
                            .lock()
                            .unwrap()
                            .push(ff.oseqno);
                        counters.allocated.fetch_add(1, Ordering::Relaxed);
                        next_callno = next_callno.wrapping_add(1) % 0x7fff;
                        let ours = next_callno;
                        if matches!(mode, Mode::AlwaysVnak) || ff.oseqno != 0 {
                            counters.vnaks.fetch_add(1, Ordering::Relaxed);
                            send(
                                &sock,
                                client,
                                frame(ours, client_call, 0, 0, IaxCommand::Vnak, Ies::empty()),
                            );
                        } else {
                            let reply = Ies {
                                authmethods: Some(2),
                                challenge: Some("c0ffee"),
                                ..Ies::empty()
                            };
                            send(
                                &sock,
                                client,
                                frame(ours, client_call, 0, 1, IaxCommand::RegAuth, reply),
                            );
                        }
                    }
                    Some(_) => {}
                }
            } else if ies.md5_result.is_some() {
                if ff.oseqno != 1 {
                    counters.vnaks.fetch_add(1, Ordering::Relaxed);
                    send(
                        &sock,
                        client,
                        frame(
                            ff.dest_call,
                            client_call,
                            1,
                            1,
                            IaxCommand::Vnak,
                            Ies::empty(),
                        ),
                    );
                    continue;
                }
                let reply = Ies {
                    refresh: Some(60),
                    ..Ies::empty()
                };
                send(
                    &sock,
                    client,
                    frame(ff.dest_call, client_call, 1, 2, IaxCommand::RegAck, reply),
                );
            }
        }
    });
    (addr, h)
}

fn run(mode: Mode, window: Duration) -> (Option<RegistrationEvent>, Arc<Counters>) {
    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Counters::default());
    let (addr, h) = spawn_registrar(mode, Arc::clone(&stop), Arc::clone(&counters));
    let (registration, events) = Registrar::new(addr, "1998", Arc::new(Secret::new("s".into())))
        .register()
        .expect("spawn registration");
    let deadline = Instant::now() + window;
    let mut outcome = None;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(ev @ (RegistrationEvent::Registered { .. } | RegistrationEvent::Failed(_))) => {
                outcome = Some(ev);
                break;
            }
            _ => {}
        }
    }
    drop(registration);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    (outcome, counters)
}

#[test]
fn token_regreq_restarts_numbering_and_registers_with_one_call_number() {
    let (outcome, c) = run(Mode::Normal { drop_first: 0 }, Duration::from_secs(5));
    assert!(
        matches!(outcome, Some(RegistrationEvent::Registered { .. })),
        "registered, got {outcome:?}"
    );
    assert_eq!(
        c.token_regreq_oseqnos.lock().unwrap().first().copied(),
        Some(0),
        "the token REGREQ restarts at oseqno 0, as chan_iax2's resend_with_token does"
    );
    assert_eq!(c.vnaks.load(Ordering::Relaxed), 0, "no VNAK");
    assert_eq!(c.allocated.load(Ordering::Relaxed), 1, "one call number");
}

/// The rig's start-up order: astar-server's REGREQs go unanswered while the
/// node boots, then the node answers a retransmission.
#[test]
fn registrar_that_was_late_still_gets_a_zero_oseqno_token_regreq() {
    let (outcome, c) = run(Mode::Normal { drop_first: 2 }, Duration::from_secs(10));
    assert!(
        matches!(outcome, Some(RegistrationEvent::Registered { .. })),
        "registered, got {outcome:?}; token REGREQ oseqnos {:?}",
        c.token_regreq_oseqnos.lock().unwrap()
    );
    assert_eq!(c.vnaks.load(Ordering::Relaxed), 0, "no VNAK");
    assert_eq!(c.allocated.load(Ordering::Relaxed), 1, "one call number");
}

/// A VNAK from a call we never learned must not be answered by resending
/// everything in flight: that is the loop that ate 2048 call numbers.
#[test]
fn vnak_on_the_token_regreq_does_not_storm() {
    let (outcome, c) = run(Mode::AlwaysVnak, Duration::from_secs(3));
    let allocated = c.allocated.load(Ordering::Relaxed);
    assert!(
        allocated <= 3,
        "{allocated} call numbers allocated in 3 s (the storm took ~2,000 in 2 s); outcome {outcome:?}"
    );
}
