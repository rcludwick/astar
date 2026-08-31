// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! iax-177d: re-register supervision. A failed registration must (a) clear
//! `is_registered` — the old sticky flag reported `registered: true` for
//! hours while the node was out of the `AllStar` directory — and (b) retry
//! automatically with backoff until it succeeds or `deregister()` is called.
//!
//! The fake registrar REJECTS the first registration outright, then completes
//! the full MD5 handshake for the supervised retry (~5 s later).

#![allow(clippy::too_many_lines, clippy::manual_assert)]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand};
use astar_station::{RegisterConfig, Station, StationConfig, StationEvent};

fn send_frame(sock: &UdpSocket, to: std::net::SocketAddr, frame: &Frame<'_>) {
    let mut bytes = Vec::new();
    encode(frame, &mut bytes).expect("encode");
    sock.send_to(&bytes, to).unwrap();
}

#[test]
fn failed_registration_clears_flag_and_supervision_reregisters() {
    let registrar_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    registrar_sock
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let registrar_addr = registrar_sock.local_addr().unwrap();

    let fake_server = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // ── Attempt 1: REJECT the REGREQ outright → fast Failed. ──
        let (client_call, client) = loop {
            let Ok((n, src)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            if let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n])
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                break (ff.source_call, src);
            }
        };
        send_frame(
            &registrar_sock,
            client,
            &Frame::Full(Box::new(FullFrame {
                source_call: 5001,
                dest_call: client_call,
                retransmission: false,
                timestamp: 0,
                oseqno: 0,
                iseqno: 1,
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::RegRej),
                ies: Ies {
                    cause: Some("Registration Refused"),
                    causecode: Some(29),
                    ..Ies::empty()
                },
                payload: &[],
            })),
        );

        // ── Attempt 2 (the supervised retry, ~5 s later): full handshake. ──
        // The retry runs on a fresh socket, so match a REGREQ from a NEW
        // source address (dedupes attempt-1 retransmits/ACKs).
        let deadline = Instant::now() + Duration::from_secs(12);
        let (client_call, client) = loop {
            if Instant::now() > deadline {
                panic!("supervision never re-registered after the reject");
            }
            let Ok((n, src)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            if src == client {
                continue; // stragglers from attempt 1
            }
            if let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n])
                && matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq))
            {
                break (ff.source_call, src);
            }
        };
        send_frame(
            &registrar_sock,
            client,
            &Frame::Full(Box::new(FullFrame {
                source_call: 5002,
                dest_call: client_call,
                retransmission: false,
                timestamp: 0,
                oseqno: 0,
                iseqno: 1,
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::RegAuth),
                ies: Ies {
                    authmethods: Some(2),
                    challenge: Some("c0ffee"),
                    ..Ies::empty()
                },
                payload: &[],
            })),
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if Instant::now() > deadline {
                panic!("no REGREQ(+MD5) on the supervised retry");
            }
            let Ok((n, _)) = registrar_sock.recv_from(&mut buf) else {
                continue;
            };
            let Ok(Frame::Full(ff)) = parse_lenient(&buf[..n]) else {
                continue;
            };
            if matches!(ff.subclass, Subclass::Iax(IaxCommand::RegReq)) && ff.dest_call == 5002 {
                let mut b = Vec::new();
                ff.ies.encode(&mut b).unwrap();
                if Ies::parse(&b).unwrap().md5_result.is_some() {
                    break;
                }
            }
        }
        send_frame(
            &registrar_sock,
            client,
            &Frame::Full(Box::new(FullFrame {
                source_call: 5002,
                dest_call: client_call,
                retransmission: false,
                timestamp: 0,
                oseqno: 1,
                iseqno: 2,
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::RegAck),
                ies: Ies {
                    refresh: Some(60),
                    ..Ies::empty()
                },
                payload: &[],
            })),
        );
    });

    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    station.set_secret_resolver(Box::new(|_u| "hunter2".to_string()));
    station
        .register(RegisterConfig {
            fallbacks: Vec::new(),
            peer: registrar_addr,
            username: "77777".to_string(),
            refresh: Duration::from_secs(60),
        })
        .expect("register spawns");

    // Phase 1: the reject must surface as RegisterFailed AND clear the flag.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got_failed = false;
    while Instant::now() < deadline && !got_failed {
        match station.next_event() {
            Some(StationEvent::RegisterFailed { .. }) => got_failed = true,
            Some(StationEvent::Registered) => panic!("reject must not register"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(got_failed, "no RegisterFailed after the reject");
    assert!(
        !station.is_registered(),
        "is_registered must clear on failure (the sticky-flag bug)"
    );

    // Phase 2: supervision retries (~5 s backoff) and the retry succeeds.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut got_registered = false;
    while Instant::now() < deadline && !got_registered {
        match station.next_event() {
            Some(StationEvent::Registered) => got_registered = true,
            Some(StationEvent::RegisterFailed { .. }) | None => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {}
        }
    }
    assert!(
        got_registered,
        "supervision did not re-register after the failure"
    );
    assert!(station.is_registered(), "flag set again after recovery");

    let _ = fake_server.join();
}

/// `RegisterConfig::resolve` and the candidate list — the enabling half of
/// registrar failover.
///
/// The operational failure this exists for: `register.allstarlink.org`
/// resolves to three hosts that do NOT all answer a given source address, and
/// which of them answers changes over time. A config pinning one address
/// therefore fails silently — REGREQs go out, nothing comes back — and one
/// node stayed unregistered for nine days that way.
mod registrar_candidates {
    use astar_station::RegisterConfig;
    use std::time::Duration;

    #[test]
    fn a_literal_address_yields_exactly_one_candidate() {
        let cfg = RegisterConfig::resolve("127.0.0.1:4569", "69586", Duration::from_secs(60))
            .expect("literal address resolves");
        assert_eq!(cfg.peer, "127.0.0.1:4569".parse().unwrap());
        assert!(
            cfg.fallbacks.is_empty(),
            "a literal has nothing to fall back to"
        );
        assert_eq!(cfg.candidates().len(), 1);
    }

    /// `localhost` is the one name that resolves the same everywhere, and on
    /// a dual-stack host it resolves to more than one address — which is the
    /// shape that matters, without depending on the internet.
    #[test]
    fn a_hostname_yields_every_address_it_has() {
        let cfg = RegisterConfig::resolve("localhost:4569", "69586", Duration::from_secs(60))
            .expect("localhost resolves");
        let candidates = cfg.candidates();
        assert!(!candidates.is_empty(), "never empty");
        assert_eq!(candidates[0], cfg.peer, "peer is first");
        assert_eq!(candidates.len(), 1 + cfg.fallbacks.len());
        assert!(
            candidates.iter().all(|a| a.port() == 4569),
            "the port carries to every candidate: {candidates:?}"
        );
    }

    #[test]
    fn a_name_that_does_not_resolve_is_an_error_not_a_silent_default() {
        assert!(
            RegisterConfig::resolve(
                "no-such-registrar.invalid:4569",
                "69586",
                Duration::from_secs(60)
            )
            .is_err()
        );
    }

    /// `candidates()` is what the supervisor rotates through, so its order is
    /// load-bearing: the configured peer must be tried first.
    #[test]
    fn candidates_puts_the_configured_peer_first() {
        let cfg = RegisterConfig {
            peer: "10.0.0.1:4569".parse().unwrap(),
            fallbacks: vec![
                "10.0.0.2:4569".parse().unwrap(),
                "10.0.0.3:4569".parse().unwrap(),
            ],
            username: "69586".into(),
            refresh: Duration::from_secs(60),
        };
        let candidates = cfg.candidates();
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], cfg.peer);
        assert_eq!(candidates[1], cfg.fallbacks[0]);
        assert_eq!(candidates[2], cfg.fallbacks[1]);
    }
}
