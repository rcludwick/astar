// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A seeded UDP relay that makes a loopback link behave like a bad network.
//!
//! The relay binds `127.0.0.1:0` and forwards every packet between one client
//! (the first peer to write to it) and one `target`. Packets going from the
//! client to the target pass through a schedule built from a [`Profile`] and a
//! seed: each is delayed by some milliseconds, and some voice packets are
//! dropped. Replies from the target go back to the client untouched.
//!
//! The same seed gives the same schedule: [`plan`] is a pure function of
//! `(profile, seed, n)`, where `n` counts the client's packets from zero.
//!
//! # Safety
//!
//! The nodebench rig is sealed to the loopback interface, so [`Relay::start`]
//! refuses any target that is not a loopback address.
//!
//! # What counts as voice
//!
//! IAX2 full frames set the top bit of their first 16-bit word (big-endian);
//! mini frames, which carry voice, leave it clear. Meta frames start with a
//! zero 16-bit word, which also has the top bit clear, so a first word of
//! exactly zero is treated as not voice. Only voice is ever dropped, so link
//! signalling is not what a run measures.
//!
//! The relay copies the network, not the audio damage: it never rewrites a
//! payload or a time-stamp.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A named delay-and-loss schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// No delay, no loss.
    Clean,
    /// The shape measured on 2026-11-18.
    Net1118,
    /// `Net1118` with every delay doubled, plus a 400 ms blackout.
    Harsh,
}

impl Profile {
    /// The name used on the command line and in results.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Profile::Clean => "clean",
            Profile::Net1118 => "net-1118",
            Profile::Harsh => "harsh",
        }
    }

    /// Every profile, in order of increasing damage.
    #[must_use]
    pub fn all() -> [Profile; 3] {
        [Profile::Clean, Profile::Net1118, Profile::Harsh]
    }
}

/// What the schedule does to one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Pass it on after this many extra milliseconds.
    Forward { delay_ms: u32 },
    /// Lose it.
    Drop,
}

/// One voice packet in this many is lost under `Net1118` and `Harsh`.
const LOSS_ONE_IN: u64 = 400;
/// `Harsh` loses every voice packet in this window, in milliseconds after the
/// first voice packet.
const BLACKOUT_MS: std::ops::Range<u64> = 10_000..10_400;

/// `SplitMix64` finaliser over `(seed, n, stream)`: a stateless generator, so a
/// packet's fate never depends on the ones before it.
fn mix(seed: u64, n: u64, stream: u64) -> u64 {
    let mut z = seed
        .wrapping_add(n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(stream.wrapping_mul(0xD1B5_4A32_D192_ED03))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform in `lo..hi` (milliseconds), from a hash word.
fn uniform(word: u64, lo: u32, hi: u32) -> u32 {
    lo + u32::try_from(word % u64::from(hi - lo)).unwrap_or(0)
}

/// The pure schedule: what happens to client-to-target packet `n`, arriving
/// `at_ms` milliseconds after the first voice packet, given whether it is
/// voice. The relay measures `at_ms` itself; callers of this function pick it.
#[must_use]
pub fn plan(profile: Profile, seed: u64, n: u64, at_ms: u64, voice: bool) -> Fate {
    if profile == Profile::Clean {
        return Fate::Forward { delay_ms: 0 };
    }
    if voice && (mix(seed, n, 1).is_multiple_of(LOSS_ONE_IN)) {
        return Fate::Drop;
    }
    if voice && profile == Profile::Harsh && BLACKOUT_MS.contains(&at_ms) {
        return Fate::Drop;
    }
    let bucket = mix(seed, n, 2) % 100;
    let word = mix(seed, n, 3);
    let base = match bucket {
        0..=89 => uniform(word, 0, 5),
        90..=98 => uniform(word, 5, 25),
        _ => uniform(word, 25, 130),
    };
    let delay_ms = if profile == Profile::Harsh {
        base * 2
    } else {
        base
    };
    Fate::Forward { delay_ms }
}

/// True for an IAX2 mini frame: first big-endian word has its top bit clear
/// and is not zero (a zero word is a meta frame).
fn is_voice(packet: &[u8]) -> bool {
    packet.len() >= 2 && packet[0] & 0x80 == 0 && (packet[0] | packet[1]) != 0
}

/// What a finished run did.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayReport {
    pub forwarded: u64,
    pub dropped: u64,
    pub delay_ms_p50: f64,
    pub delay_ms_p99: f64,
    pub delay_ms_max: f64,
    pub seed: u64,
    pub profile: Profile,
}

/// What the client-facing thread learned.
struct Tally {
    forwarded: u64,
    dropped: u64,
    /// Effective delay of each forwarded packet in microseconds: release
    /// time minus arrival time, after the keep-in-order clamp.
    delays_us: Vec<u64>,
}

/// A running relay: three threads and two sockets.
pub struct Relay {
    local: SocketAddr,
    stop: Arc<AtomicBool>,
    inbound: JoinHandle<Tally>,
    outbound: JoinHandle<()>,
    replies: JoinHandle<()>,
    /// Set once everything has been flushed, so the replies thread can end.
    replies_stop: Arc<AtomicBool>,
    seed: u64,
    profile: Profile,
}

impl Relay {
    /// Binds `127.0.0.1:0` and starts relaying to `target`, which must be a
    /// loopback address.
    pub fn start(target: SocketAddr, profile: Profile, seed: u64) -> Result<Relay, String> {
        if !target.ip().is_loopback() {
            return Err(format!(
                "relay target {target} is not on loopback; nodebench stays on 127.0.0.1"
            ));
        }
        let io_err = |what: &str, e: io::Error| format!("relay {what}: {e}");
        let front = UdpSocket::bind("127.0.0.1:0").map_err(|e| io_err("bind", e))?;
        let back = UdpSocket::bind("127.0.0.1:0").map_err(|e| io_err("bind", e))?;
        back.connect(target).map_err(|e| io_err("connect", e))?;
        let tick = Some(Duration::from_millis(20));
        front
            .set_read_timeout(tick)
            .map_err(|e| io_err("timeout", e))?;
        back.set_read_timeout(tick)
            .map_err(|e| io_err("timeout", e))?;
        let local = front.local_addr().map_err(|e| io_err("addr", e))?;
        let front = Arc::new(front);
        let back = Arc::new(back);
        let stop = Arc::new(AtomicBool::new(false));
        let replies_stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        let (client_tx, client_rx) = mpsc::channel::<SocketAddr>();

        let inbound = {
            let (front, stop) = (front.clone(), stop.clone());
            std::thread::spawn(move || inbound_loop(&front, &stop, profile, seed, &tx, &client_tx))
        };
        let outbound = {
            let back = back.clone();
            std::thread::spawn(move || outbound_loop(&back, &rx))
        };
        let replies = {
            let replies_stop = replies_stop.clone();
            std::thread::spawn(move || replies_loop(&front, &back, &replies_stop, &client_rx))
        };
        Ok(Relay {
            local,
            stop,
            inbound,
            outbound,
            replies,
            replies_stop,
            seed,
            profile,
        })
    }

    /// The address a client sends to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Stops the relay (releasing anything still delayed) and reports.
    #[must_use]
    pub fn stop(self) -> RelayReport {
        self.stop.store(true, Ordering::SeqCst);
        let mut tally = self.inbound.join().unwrap_or(Tally {
            forwarded: 0,
            dropped: 0,
            delays_us: Vec::new(),
        });
        // The outbound thread ends once the queue is flushed; only then may
        // the replies thread stop, so replies to the flushed tail still get
        // back to the client.
        let _ = self.outbound.join();
        self.replies_stop.store(true, Ordering::SeqCst);
        let _ = self.replies.join();
        tally.delays_us.sort_unstable();
        let pick = |per_mille: usize| -> f64 {
            let last = tally.delays_us.len().saturating_sub(1);
            let i = (last * per_mille).div_ceil(1000).min(last);
            #[allow(clippy::cast_precision_loss)]
            tally.delays_us.get(i).map_or(0.0, |d| *d as f64 / 1000.0)
        };
        RelayReport {
            forwarded: tally.forwarded,
            dropped: tally.dropped,
            delay_ms_p50: pick(500),
            delay_ms_p99: pick(990),
            delay_ms_max: pick(1000),
            seed: self.seed,
            profile: self.profile,
        }
    }
}

/// True for the "nothing arrived in time" outcomes of a socket read with a
/// time-out. Anything else is a real error.
fn is_quiet(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// How long after the stop flag the inbound side keeps reading what the client
/// already sent, so every packet is counted.
const DRAIN: Duration = Duration::from_millis(100);

/// Reads the client's packets, applies the schedule, and queues the survivors
/// with release times that never run backwards. After the stop flag it keeps
/// reading until the socket is quiet (or [`DRAIN`] passes), so
/// `forwarded + dropped` is everything the client sent.
fn inbound_loop(
    front: &UdpSocket,
    stop: &AtomicBool,
    profile: Profile,
    seed: u64,
    queue: &Sender<(Instant, Vec<u8>)>,
    client_tx: &Sender<SocketAddr>,
) -> Tally {
    let mut tally = Tally {
        forwarded: 0,
        dropped: 0,
        delays_us: Vec::new(),
    };
    let mut client: Option<SocketAddr> = None;
    let mut first_voice: Option<Instant> = None;
    let mut last_release = Instant::now();
    let mut n = 0u64;
    let mut buf = [0u8; 2048];
    let mut drain_until: Option<Instant> = None;
    let mut reported_error = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            let limit = *drain_until.get_or_insert_with(|| Instant::now() + DRAIN);
            if Instant::now() >= limit {
                break;
            }
        }
        let (len, from) = match front.recv_from(&mut buf) {
            Ok(got) => got,
            Err(e) if is_quiet(&e) => {
                if drain_until.is_some() {
                    break; // stopped and nothing left to read
                }
                continue;
            }
            Err(e) => {
                if !reported_error {
                    reported_error = true;
                    eprintln!("relay: client socket read failed: {e}");
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        match client {
            None => {
                client = Some(from);
                let _ = client_tx.send(from);
            }
            Some(c) if c != from => continue,
            Some(_) => {}
        }
        let now = Instant::now();
        let voice = is_voice(&buf[..len]);
        let at_ms = if voice {
            u64::try_from(
                now.duration_since(*first_voice.get_or_insert(now))
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        } else {
            0
        };
        match plan(profile, seed, n, at_ms, voice) {
            Fate::Drop => tally.dropped += 1,
            Fate::Forward { delay_ms } => {
                let release = last_release.max(now + Duration::from_millis(u64::from(delay_ms)));
                last_release = release;
                tally.forwarded += 1;
                tally
                    .delays_us
                    .push(u64::try_from((release - now).as_micros()).unwrap_or(u64::MAX));
                let _ = queue.send((release, buf[..len].to_vec()));
            }
        }
        n += 1;
    }
    tally
}

/// Sends queued packets to the target at their release times, in order. Ends
/// when the inbound side has gone and the queue is empty.
fn outbound_loop(back: &UdpSocket, queue: &Receiver<(Instant, Vec<u8>)>) {
    while let Ok((release, packet)) = queue.recv() {
        let now = Instant::now();
        if release > now {
            std::thread::sleep(release - now);
        }
        let _ = back.send(&packet);
    }
}

/// Carries the target's replies back to the client, undelayed.
fn replies_loop(
    front: &UdpSocket,
    back: &UdpSocket,
    stop: &AtomicBool,
    client_rx: &Receiver<SocketAddr>,
) {
    let mut client: Option<SocketAddr> = None;
    let mut buf = [0u8; 2048];
    // After the stop flag (set once the outbound queue is flushed), wait for
    // a few quiet read ticks so the target's answers to the last packets are
    // still carried back.
    let mut quiet_after_stop = 0;
    let mut reported_error = false;
    loop {
        if client.is_none() {
            client = client_rx.try_recv().ok();
        }
        match back.recv(&mut buf) {
            Ok(len) => {
                quiet_after_stop = 0;
                if let Some(c) = client.or_else(|| client_rx.try_recv().ok()) {
                    client = Some(c);
                    let _ = front.send_to(&buf[..len], c);
                }
            }
            Err(e) => {
                if !is_quiet(&e) {
                    if !reported_error {
                        reported_error = true;
                        eprintln!("relay: target socket read failed: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if stop.load(Ordering::SeqCst) {
                    quiet_after_stop += 1;
                    if quiet_after_stop > 3 {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fates(profile: Profile, seed: u64, count: u64) -> Vec<Fate> {
        (0..count)
            .map(|n| plan(profile, seed, n, n * 20, true))
            .collect()
    }

    #[test]
    fn same_seed_same_plan() {
        let a = fates(Profile::Net1118, 7, 5000);
        assert_eq!(a, fates(Profile::Net1118, 7, 5000));
        assert_ne!(a, fates(Profile::Net1118, 8, 5000));
    }

    #[test]
    fn clean_changes_nothing() {
        assert!(
            fates(Profile::Clean, 3, 5000)
                .iter()
                .all(|f| *f == Fate::Forward { delay_ms: 0 })
        );
    }

    #[test]
    fn net1118_shape() {
        let mut delays = Vec::new();
        let mut dropped = 0;
        for f in fates(Profile::Net1118, 11, 100_000) {
            match f {
                Fate::Forward { delay_ms } => delays.push(delay_ms),
                Fate::Drop => dropped += 1,
            }
        }
        delays.sort_unstable();
        let p50 = f64::from(delays[delays.len() / 2]);
        let p99 = f64::from(delays[delays.len() * 99 / 100]);
        assert!((1.5..=3.5).contains(&p50), "p50 {p50}");
        assert!((18.0..=30.0).contains(&p99), "p99 {p99}");
        assert!(*delays.last().unwrap() <= 130);
        assert!((200..=300).contains(&dropped), "dropped {dropped}");
    }

    #[test]
    fn only_voice_is_dropped() {
        for p in Profile::all() {
            for n in 0..20_000 {
                // 10_100 ms is inside the harsh blackout.
                assert_ne!(plan(p, 5, n, 10_100, false), Fate::Drop, "{p:?} {n}");
            }
        }
    }

    #[test]
    fn harsh_blackout() {
        // A packet number the loss rule leaves alone, so only the blackout
        // can drop it.
        let n = (0..1000u64)
            .find(|n| plan(Profile::Net1118, 9, *n, 0, true) != Fate::Drop)
            .unwrap();
        let at = |ms| plan(Profile::Harsh, 9, n, ms, true);
        assert_ne!(at(9_999), Fate::Drop, "just before the window");
        assert_eq!(at(10_000), Fate::Drop, "first millisecond");
        assert_eq!(at(10_399), Fate::Drop, "last millisecond");
        // The window is 10.0 s up to but not including 10.4 s, 400 ms long.
        assert_ne!(at(10_400), Fate::Drop, "10_400 is outside");
        for ms in 10_000..10_400 {
            assert_eq!(plan(Profile::Harsh, 9, ms, ms, true), Fate::Drop);
        }
        // Net1118 has no blackout.
        assert_ne!(plan(Profile::Net1118, 9, n, 10_100, true), Fate::Drop);
    }

    #[test]
    fn harsh_doubles_the_delays() {
        let mut net = Vec::new();
        let mut harsh = Vec::new();
        for n in 0..100_000 {
            // at_ms 0 keeps the blackout out of it: the loss rule is shared.
            let a = plan(Profile::Net1118, 11, n, 0, true);
            let b = plan(Profile::Harsh, 11, n, 0, true);
            match (a, b) {
                (Fate::Forward { delay_ms: x }, Fate::Forward { delay_ms: y }) => {
                    assert_eq!(y, 2 * x, "packet {n}");
                    net.push(x);
                    harsh.push(y);
                }
                (Fate::Drop, Fate::Drop) => {}
                other => panic!("loss differs at {n}: {other:?}"),
            }
        }
        net.sort_unstable();
        harsh.sort_unstable();
        assert!(*harsh.last().unwrap() <= 260);
        let ratio = f64::from(harsh[harsh.len() / 2]) / f64::from(net[net.len() / 2]);
        assert!((1.8..=2.2).contains(&ratio), "p50 ratio {ratio}");
    }

    #[test]
    fn a_zero_first_word_is_not_voice() {
        assert!(is_voice(&[0x01, 0x02, 9]));
        assert!(!is_voice(&[0x80, 0x01, 9]), "full frame");
        assert!(!is_voice(&[0x00, 0x00, 9]), "meta frame");
        assert!(!is_voice(&[0x01]), "too short");
    }

    /// A loopback peer that records what it receives (number, arrival time)
    /// and echoes each packet straight back.
    struct Echo {
        addr: SocketAddr,
        got: Arc<Mutex<Vec<(u32, Instant)>>>,
        done: Arc<AtomicBool>,
        handle: JoinHandle<()>,
    }

    impl Echo {
        fn start() -> Echo {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let addr = sock.local_addr().unwrap();
            let got: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
            let done = Arc::new(AtomicBool::new(false));
            let handle = {
                let (got, done) = (got.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut buf = [0u8; 64];
                    loop {
                        match sock.recv_from(&mut buf) {
                            Ok((len, from)) => {
                                let k = u32::from_be_bytes(buf[2..6].try_into().unwrap());
                                got.lock().unwrap().push((k, Instant::now()));
                                let _ = sock.send_to(&buf[..len], from);
                            }
                            Err(_) if done.load(Ordering::SeqCst) => break,
                            Err(_) => {}
                        }
                    }
                })
            };
            Echo {
                addr,
                got,
                done,
                handle,
            }
        }

        /// Ends the peer once its socket has gone quiet; returns what it saw.
        fn finish(self) -> Vec<(u32, Instant)> {
            self.done.store(true, Ordering::SeqCst);
            self.handle.join().unwrap();
            self.got.lock().unwrap().clone()
        }
    }

    fn numbered(k: u32) -> Vec<u8> {
        let mut p = vec![0x01, 0x02];
        p.extend_from_slice(&k.to_be_bytes());
        p
    }

    fn client_to(relay: &Relay) -> UdpSocket {
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        client.connect(relay.local_addr()).unwrap();
        client
    }

    #[test]
    fn relays_both_ways_in_order() {
        let echo = Echo::start();
        let run_seed = 21;
        let relay = Relay::start(echo.addr, Profile::Net1118, run_seed).unwrap();
        let client = client_to(&relay);
        for k in 0..500u32 {
            client.send(&numbered(k)).unwrap();
            std::thread::sleep(Duration::from_micros(300));
        }
        let expected: Vec<u32> = (0..500u32)
            .filter(|k| plan(Profile::Net1118, run_seed, u64::from(*k), 0, true) != Fate::Drop)
            .collect();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut echoed = 0;
        let mut buf = [0u8; 64];
        while Instant::now() < deadline && echoed < expected.len() {
            if client.recv(&mut buf).is_ok() {
                echoed += 1;
            }
        }
        let report = relay.stop();
        let order: Vec<u32> = echo.finish().iter().map(|(k, _)| *k).collect();

        assert_eq!(order, expected, "arrival order");
        assert_eq!(echoed, expected.len(), "replies came back");
        assert_eq!(report.forwarded + report.dropped, 500);
        assert_eq!(report.forwarded, expected.len() as u64);
        assert_eq!(report.seed, run_seed);
        assert_eq!(report.profile, Profile::Net1118);
    }

    #[test]
    fn stop_counts_every_packet_and_reports_effective_delay() {
        let echo = Echo::start();
        let run_seed = 33;
        let relay = Relay::start(echo.addr, Profile::Net1118, run_seed).unwrap();
        let client = client_to(&relay);
        // Sent 1 ms apart and stopped at once: the keep-in-order clamp
        // stretches delays, and some packets are still unread at stop().
        let count = 400u32;
        for k in 0..count {
            client.send(&numbered(k)).unwrap();
            // The last 100 go out back to back, so some are still unread by
            // the relay when stop() is called.
            if k < count - 100 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let report = relay.stop();
        let mut echoed = 0u64;
        let mut buf = [0u8; 64];
        while client.recv(&mut buf).is_ok() {
            echoed += 1;
        }
        let order: Vec<u32> = echo.finish().iter().map(|(k, _)| *k).collect();

        let planned: Vec<u32> = (0..count)
            .filter_map(
                |k| match plan(Profile::Net1118, run_seed, u64::from(k), 0, true) {
                    Fate::Forward { delay_ms } => Some(delay_ms),
                    Fate::Drop => None,
                },
            )
            .collect();
        assert_eq!(report.forwarded + report.dropped, u64::from(count));
        assert_eq!(report.forwarded, planned.len() as u64);
        assert_eq!(echoed, report.forwarded, "replies to the flushed tail");
        let mut sorted = planned.clone();
        sorted.sort_unstable();
        let planned_p50 = f64::from(sorted[sorted.len() / 2]);
        assert!(
            report.delay_ms_p50 > planned_p50,
            "effective p50 {} should exceed planned p50 {planned_p50}: packets 1 ms apart queue behind a slow one",
            report.delay_ms_p50
        );
        let expected_order: Vec<u32> = (0..count)
            .filter(|k| plan(Profile::Net1118, run_seed, u64::from(*k), 0, true) != Fate::Drop)
            .collect();
        assert_eq!(order, expected_order, "order kept, nothing lost at stop");
        let planned_max = f64::from(*planned.iter().max().unwrap());
        assert!(
            report.delay_ms_max >= planned_max,
            "effective max {} < planned max {planned_max}",
            report.delay_ms_max
        );
        assert!(report.delay_ms_p50 <= report.delay_ms_p99);
        assert!(report.delay_ms_p99 <= report.delay_ms_max);
    }

    #[test]
    fn replies_bypass_the_schedule() {
        // A seed whose first packet the schedule holds back 100 ms or more,
        // so a delay on the way back would stand out.
        let (run_seed, held) = (0..200_000u64)
            .find_map(|seed| match plan(Profile::Harsh, seed, 0, 0, true) {
                Fate::Forward { delay_ms } if delay_ms >= 100 => Some((seed, delay_ms)),
                _ => None,
            })
            .unwrap();
        let echo = Echo::start();
        let relay = Relay::start(echo.addr, Profile::Harsh, run_seed).unwrap();
        let client = client_to(&relay);
        let sent = Instant::now();
        client.send(&numbered(0)).unwrap();
        let mut buf = [0u8; 64];
        let mut back = None;
        while sent.elapsed() < Duration::from_secs(2) && back.is_none() {
            back = client.recv(&mut buf).ok().map(|_| Instant::now());
        }
        let back = back.expect("a reply");
        let _ = relay.stop();
        let seen = echo.finish();
        let at_peer = seen[0].1;
        let forward_leg = at_peer - sent;
        let reply_leg = back - at_peer;
        assert!(
            forward_leg >= Duration::from_millis(u64::from(held) - 5),
            "forward leg {forward_leg:?} should carry the {held} ms delay"
        );
        assert!(
            reply_leg < Duration::from_millis(25),
            "reply leg {reply_leg:?} must not be delayed"
        );
    }

    #[test]
    fn stop_waits_for_the_last_reply() {
        // One packet the schedule holds back 120 ms or more, stopped at once:
        // stop() must flush it and still carry the reply home.
        let (run_seed, _) = (0..500_000u64)
            .find_map(|seed| match plan(Profile::Harsh, seed, 0, 0, true) {
                Fate::Forward { delay_ms } if delay_ms >= 120 => Some((seed, delay_ms)),
                _ => None,
            })
            .unwrap();
        let echo = Echo::start();
        let relay = Relay::start(echo.addr, Profile::Harsh, run_seed).unwrap();
        let client = client_to(&relay);
        client.send(&numbered(0)).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let report = relay.stop();
        let mut buf = [0u8; 64];
        assert!(client.recv(&mut buf).is_ok(), "the reply was lost");
        assert_eq!(echo.finish().len(), 1);
        assert_eq!(report.forwarded, 1);
    }

    #[test]
    fn only_timeouts_are_quiet() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut] {
            assert!(is_quiet(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            assert!(!is_quiet(&io::Error::from(kind)), "{kind:?}");
        }
    }

    #[test]
    fn refuses_a_target_off_the_machine() {
        let err = Relay::start("192.0.2.1:4569".parse().unwrap(), Profile::Clean, 1)
            .err()
            .expect("must refuse");
        assert!(err.contains("192.0.2.1:4569"), "{err}");
    }
}
