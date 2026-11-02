// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! A directory server and a station to test against, on 127.0.0.1 only.
//!
//! [`FakeDirectory`] listens on an ephemeral TCP port and answers the way an
//! `EchoLink` directory does: a login line gets `OK` (or whatever reply the
//! test picked), and the list request `s` gets a plain `@@@` list of the
//! stations it was started with. [`FakeStation`] binds two ephemeral UDP
//! ports, answers each SDES with its own, and by default echoes every audio
//! packet straight back, the way `*ECHOTEST*` does.
//!
//! Both fakes bind `127.0.0.1:0` and nothing else, and every datagram a fake
//! sends goes to a loopback address (it panics otherwise). Every request and
//! datagram they receive is stamped with [`Instant::now`] on arrival, so a test
//! can compare the order of events across several fakes.
//!
//! The directory keeps every request it receives, password included. That is
//! the point of a test fake, and it is why this module exists only under the
//! `loopback-fakes` feature, which only dev-dependencies turn on.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_codec::gsm::{FRAME_SAMPLES, GsmEncoder};

use crate::PeerAddr;
use crate::directory::{LIST_REQUEST, StationEntry, StationStatus, kind_of};
use crate::rtcp::{self, Rtcp};
use crate::voice::{self, AudioPacket, Datagram, FRAMES_PER_PACKET, GSM_FRAME_LEN};

/// How often the fakes' threads look at their stop flag.
const POLL: Duration = Duration::from_millis(10);
/// Longest the directory waits for a client to finish its request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// Largest request the directory reads; a login line is far shorter.
const MAX_REQUEST: usize = 64 * 1024;
/// The clock time every listed station shows in its status tag.
const LISTED_AT: &str = "12:00";
/// One cycle of a 1 kHz sine at 8 kHz sampling, peak 8000.
const TONE_1KHZ: [i16; 8] = [0, 5657, 8000, 5657, 0, -5657, -8000, -5657];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Panic unless `addr` is loopback: a fake never sends anywhere else.
fn assert_loopback(addr: SocketAddr) {
    assert!(
        addr.ip().is_loopback(),
        "echolink fake: refusing to send to non-loopback {addr}"
    );
}

/// What the fake directory says to a login (or logoff) line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeLogin {
    /// `OK`.
    Ok,
    /// `INCORRECT PASSWORD`.
    BadPassword,
    /// This text, as the whole reply.
    Refuse(String),
    /// Reads the request, then says nothing and holds the connection open.
    Silent,
}

struct DirectoryShared {
    reply: Mutex<FakeLogin>,
    refuse: AtomicBool,
    stations: Vec<StationEntry>,
    accepted: Mutex<Vec<Instant>>,
    lines: Mutex<Vec<(Instant, Vec<u8>)>>,
    stop: AtomicBool,
}

/// A directory server on `127.0.0.1:<ephemeral>`. Stops when dropped.
pub struct FakeDirectory {
    addr: SocketAddr,
    shared: Arc<DirectoryShared>,
    thread: Option<JoinHandle<()>>,
}

impl FakeDirectory {
    /// Start listening. `reply` answers login lines; `stations` is the list.
    ///
    /// # Panics
    /// If 127.0.0.1 cannot be bound.
    #[must_use]
    pub fn start(reply: FakeLogin, stations: Vec<StationEntry>) -> FakeDirectory {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1:0");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(DirectoryShared {
            reply: Mutex::new(reply),
            refuse: AtomicBool::new(false),
            stations,
            accepted: Mutex::new(Vec::new()),
            lines: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("fake-echolink-directory".into())
                .spawn(move || accept_loop(&listener, &shared))
                .expect("spawn fake directory")
        };
        FakeDirectory {
            addr,
            shared,
            thread: Some(thread),
        }
    }

    /// Where the directory listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every request received, in order, password included.
    #[must_use]
    pub fn lines(&self) -> Vec<Vec<u8>> {
        lock(&self.shared.lines)
            .iter()
            .map(|(_, l)| l.clone())
            .collect()
    }

    /// Every request received, each with the moment it finished arriving.
    #[must_use]
    pub fn line_times(&self) -> Vec<(Instant, Vec<u8>)> {
        lock(&self.shared.lines).clone()
    }

    /// Change the reply to later login lines.
    pub fn set_reply(&self, reply: FakeLogin) {
        *lock(&self.shared.reply) = reply;
    }

    /// While on, every connection is accepted and closed at once, unread.
    pub fn set_refuse_connections(&self, refuse: bool) {
        self.shared.refuse.store(refuse, Ordering::SeqCst);
    }

    /// Connections accepted so far, refused ones included.
    #[must_use]
    pub fn exchanges(&self) -> usize {
        lock(&self.shared.accepted).len()
    }

    /// When each connection was accepted.
    #[must_use]
    pub fn exchange_times(&self) -> Vec<Instant> {
        lock(&self.shared.accepted).clone()
    }
}

impl Drop for FakeDirectory {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<DirectoryShared>) {
    let mut handlers = Vec::new();
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                lock(&shared.accepted).push(Instant::now());
                if shared.refuse.load(Ordering::SeqCst) {
                    drop(stream);
                    continue;
                }
                let shared = Arc::clone(shared);
                handlers.push(thread::spawn(move || {
                    let _ = serve(stream, &shared);
                }));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => thread::sleep(POLL),
        }
    }
    for h in handlers {
        let _ = h.join();
    }
}

/// Read one request to EOF, record it, and answer it.
fn serve(mut stream: TcpStream, shared: &DirectoryShared) -> io::Result<()> {
    // An accepted socket inherits the listener's non-blocking mode on some platforms.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(POLL))?;
    let started = Instant::now();
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if shared.stop.load(Ordering::SeqCst) || started.elapsed() > REQUEST_DEADLINE {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                if request.len() >= MAX_REQUEST {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    lock(&shared.lines).push((Instant::now(), request.clone()));
    let reply = lock(&shared.reply).clone();
    if reply == FakeLogin::Silent {
        while !shared.stop.load(Ordering::SeqCst) {
            thread::sleep(POLL);
        }
        return Ok(());
    }
    if request == LIST_REQUEST {
        stream.write_all(&list_body(&shared.stations))?;
    } else if request.first() == Some(&b'l') {
        let text: Vec<u8> = match reply {
            FakeLogin::Ok => b"OK".to_vec(),
            FakeLogin::BadPassword => b"INCORRECT PASSWORD\r".to_vec(),
            FakeLogin::Refuse(t) => format!("{t}\r").into_bytes(),
            FakeLogin::Silent => unreachable!("answered above"),
        };
        stream.write_all(&text)?;
    }
    Ok(())
}

/// A plain list: `@@@`, the count, four lines per station, `+++`.
fn list_body(stations: &[StationEntry]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out = format!("@@@\n{}\n", stations.len());
    for s in stations {
        let tag = match s.status {
            StationStatus::Online => format!(" [ON {LISTED_AT}]"),
            StationStatus::Busy => format!(" [BUSY {LISTED_AT}]"),
            StationStatus::Unknown => String::new(),
        };
        let _ = write!(
            out,
            "{}\n{}{}\n{}\n{}\n",
            s.call, s.description, tag, s.node, s.ip
        );
    }
    out.push_str("+++\n");
    out.into_bytes()
}

/// What arrived on which port, and when.
#[derive(Debug, Clone)]
struct Arrival {
    at: Instant,
    from: SocketAddr,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Sdes,
    Bye,
    /// Any other control-port datagram, well formed or not.
    OtherControl,
    Audio,
    Chat(String),
    Info(String),
    /// Anything else on the audio port.
    OtherAudio,
}

struct StationShared {
    callsign: String,
    answer: AtomicBool,
    /// While on, SDES is answered from `spare` instead of the control port.
    answer_from_spare: AtomicBool,
    /// A third socket on 127.0.0.1, standing in for a NAT that rewrites the
    /// source port of the station's replies.
    spare: UdpSocket,
    echo: AtomicBool,
    stop: AtomicBool,
    arrivals: Mutex<Vec<Arrival>>,
    audio_packets: AtomicUsize,
    seq: Mutex<u16>,
}

/// A station on two ephemeral UDP ports of 127.0.0.1. Stops when dropped.
pub struct FakeStation {
    audio: UdpSocket,
    control: UdpSocket,
    shared: Arc<StationShared>,
    threads: Vec<JoinHandle<()>>,
}

impl FakeStation {
    /// Bind the audio and control ports and start answering.
    ///
    /// # Panics
    /// If 127.0.0.1 cannot be bound.
    #[must_use]
    pub fn start(callsign: &str) -> FakeStation {
        let bind = || {
            let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1:0");
            s.set_read_timeout(Some(POLL)).expect("read timeout");
            s
        };
        let (audio, control) = (bind(), bind());
        let shared = Arc::new(StationShared {
            callsign: callsign.to_owned(),
            answer: AtomicBool::new(true),
            answer_from_spare: AtomicBool::new(false),
            spare: bind(),
            echo: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            arrivals: Mutex::new(Vec::new()),
            audio_packets: AtomicUsize::new(0),
            seq: Mutex::new(0),
        });
        let spawn = |name: &str, sock: &UdpSocket, run: fn(&UdpSocket, &StationShared)| {
            let sock = sock.try_clone().expect("clone socket");
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name(name.into())
                .spawn(move || run(&sock, &shared))
                .expect("spawn fake station")
        };
        let threads = vec![
            spawn("fake-echolink-audio", &audio, audio_loop),
            spawn("fake-echolink-control", &control, control_loop),
        ];
        FakeStation {
            audio,
            control,
            shared,
            threads,
        }
    }

    /// The station's audio and control addresses.
    #[must_use]
    pub fn addr(&self) -> PeerAddr {
        PeerAddr {
            audio: self.audio.local_addr().expect("local addr"),
            control: self.control.local_addr().expect("local addr"),
        }
    }

    /// The directory entry for this station, at 127.0.0.1, online.
    #[must_use]
    pub fn entry(&self, node: u32) -> StationEntry {
        let call = self.shared.callsign.to_uppercase();
        StationEntry {
            kind: kind_of(&call),
            call,
            description: "Fake station".to_owned(),
            node,
            ip: Ipv4Addr::LOCALHOST,
            status: StationStatus::Online,
        }
    }

    /// While off, SDES is still counted but no longer answered.
    pub fn set_answer(&self, answer: bool) {
        self.shared.answer.store(answer, Ordering::SeqCst);
    }

    /// While on, each SDES is answered from a third port of 127.0.0.1 rather
    /// than the control port, the way a port-rewriting NAT makes a real
    /// station's replies look. Off by default.
    pub fn set_answer_from_another_port(&self, on: bool) {
        self.shared.answer_from_spare.store(on, Ordering::SeqCst);
    }

    /// The third port that [`FakeStation::set_answer_from_another_port`]
    /// answers from.
    #[must_use]
    pub fn another_port_addr(&self) -> SocketAddr {
        self.shared.spare.local_addr().expect("local addr")
    }

    /// While on (the default), every audio packet goes straight back to its
    /// sender. While off, audio is counted and nothing is sent back.
    pub fn set_echo(&self, echo: bool) {
        self.shared.echo.store(echo, Ordering::SeqCst);
    }

    /// Talk: send `frames` GSM frames of a 1 kHz tone to `to.audio`, four to
    /// a 144-byte packet, as fast as the socket takes them. A last partial
    /// packet is filled out with silence.
    ///
    /// # Panics
    /// If `to.audio` is not loopback.
    pub fn send_audio(&self, to: PeerAddr, frames: usize) {
        assert_loopback(to.audio);
        let mut enc = GsmEncoder::new();
        let mut phase = 0usize;
        let mut tone = || {
            let mut pcm = [0i16; FRAME_SAMPLES];
            for s in &mut pcm {
                *s = TONE_1KHZ[phase % TONE_1KHZ.len()];
                phase += 1;
            }
            pcm
        };
        let mut sent = 0;
        while sent < frames {
            let mut packet = [[0u8; GSM_FRAME_LEN]; FRAMES_PER_PACKET];
            for f in &mut packet {
                *f = if sent < frames {
                    sent += 1;
                    enc.encode(&tone())
                } else {
                    enc.encode(&[0; FRAME_SAMPLES])
                };
            }
            let seq = {
                let mut s = lock(&self.shared.seq);
                *s = s.wrapping_add(1);
                *s
            };
            let bytes = AudioPacket {
                seq,
                frames: packet,
            }
            .encode();
            let _ = self.audio.send_to(&bytes, to.audio);
        }
    }

    /// Send a BYE to whoever last sent this station an SDES.
    pub fn send_bye(&self) {
        if let Some(to) = self.last_from(|k| *k == Kind::Sdes) {
            send(&self.control, &rtcp::bye(), to);
        }
    }

    /// Send an SDES from the control port to any loopback address, called or
    /// not: an uninvited station knocking.
    ///
    /// # Panics
    /// If `to` is not loopback.
    pub fn send_sdes_to(&self, to: SocketAddr) {
        send(&self.control, &rtcp::sdes(&self.shared.callsign, ""), to);
    }

    /// Send any bytes from the control port to a loopback address.
    ///
    /// # Panics
    /// If `to` is not loopback.
    pub fn send_raw_control(&self, to: SocketAddr, bytes: &[u8]) {
        send(&self.control, bytes, to);
    }

    /// Send any bytes from the audio port to a loopback address.
    ///
    /// # Panics
    /// If `to` is not loopback.
    pub fn send_raw_audio(&self, to: SocketAddr, bytes: &[u8]) {
        send(&self.audio, bytes, to);
    }

    /// Send a chat line to the station's peer.
    pub fn send_chat(&self, text: &str) {
        if let Some(to) = self.audio_peer() {
            send(
                &self.audio,
                &voice::chat_datagram(&self.shared.callsign, text),
                to,
            );
        }
    }

    /// Send station information text to the station's peer.
    pub fn send_info(&self, text: &str) {
        if let Some(to) = self.audio_peer() {
            send(&self.audio, &voice::info_datagram(text), to);
        }
    }

    /// Audio packets received, echoed or not.
    #[must_use]
    pub fn audio_packets(&self) -> usize {
        self.shared.audio_packets.load(Ordering::SeqCst)
    }

    /// The text of every chat line received, in order.
    #[must_use]
    pub fn chats(&self) -> Vec<String> {
        lock(&self.shared.arrivals)
            .iter()
            .filter_map(|a| match &a.kind {
                Kind::Chat(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// The text of every station information datagram received, in order.
    #[must_use]
    pub fn infos(&self) -> Vec<String> {
        lock(&self.shared.arrivals)
            .iter()
            .filter_map(|a| match &a.kind {
                Kind::Info(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// SDES datagrams received, from anyone.
    #[must_use]
    pub fn sdes_count(&self) -> usize {
        lock(&self.shared.arrivals)
            .iter()
            .filter(|a| a.kind == Kind::Sdes)
            .count()
    }

    /// Control-port datagrams received from `addr`, of any kind.
    #[must_use]
    pub fn controls_from(&self, addr: SocketAddr) -> usize {
        lock(&self.shared.arrivals)
            .iter()
            .filter(|a| {
                a.from == addr && matches!(a.kind, Kind::Sdes | Kind::Bye | Kind::OtherControl)
            })
            .count()
    }

    /// When each BYE arrived.
    #[must_use]
    pub fn bye_times(&self) -> Vec<Instant> {
        lock(&self.shared.arrivals)
            .iter()
            .filter(|a| a.kind == Kind::Bye)
            .map(|a| a.at)
            .collect()
    }

    /// The sender of the most recent arrival of a kind.
    fn last_from(&self, want: impl Fn(&Kind) -> bool) -> Option<SocketAddr> {
        lock(&self.shared.arrivals)
            .iter()
            .rev()
            .find(|a| want(&a.kind))
            .map(|a| a.from)
    }

    /// Where the peer receives audio-port datagrams: the last address that
    /// sent this station one, or else the port below its control port, as on
    /// the real network (5198 below 5199).
    fn audio_peer(&self) -> Option<SocketAddr> {
        self.last_from(|k| {
            matches!(
                k,
                Kind::Audio | Kind::Chat(_) | Kind::Info(_) | Kind::OtherAudio
            )
        })
        .or_else(|| {
            self.last_from(|k| *k == Kind::Sdes).map(|mut c| {
                c.set_port(c.port().wrapping_sub(1));
                c
            })
        })
    }
}

impl Drop for FakeStation {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn send(sock: &UdpSocket, bytes: &[u8], to: SocketAddr) {
    assert_loopback(to);
    let _ = sock.send_to(bytes, to);
}

/// Receive until stopped, stamping each datagram, and hand it to `on`.
fn receive(sock: &UdpSocket, shared: &StationShared, mut on: impl FnMut(&[u8], SocketAddr)) {
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Ordering::SeqCst) {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => on(&buf[..n], from),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            // A port-unreachable from an earlier send can surface here; keep going.
            Err(_) => {}
        }
    }
}

fn record(shared: &StationShared, at: Instant, from: SocketAddr, kind: Kind) {
    lock(&shared.arrivals).push(Arrival { at, from, kind });
}

fn control_loop(sock: &UdpSocket, shared: &StationShared) {
    receive(sock, shared, |bytes, from| {
        let at = Instant::now();
        let kind = match rtcp::parse(bytes) {
            Ok(Rtcp::Sdes { .. }) => Kind::Sdes,
            Ok(Rtcp::Bye) => Kind::Bye,
            _ => Kind::OtherControl,
        };
        record(shared, at, from, kind.clone());
        if kind == Kind::Sdes && shared.answer.load(Ordering::SeqCst) {
            let reply = rtcp::sdes(&shared.callsign, "");
            if shared.answer_from_spare.load(Ordering::SeqCst) {
                send(&shared.spare, &reply, from);
            } else {
                send(sock, &reply, from);
            }
        }
    });
}

fn audio_loop(sock: &UdpSocket, shared: &StationShared) {
    receive(sock, shared, |bytes, from| {
        let at = Instant::now();
        let kind = match voice::classify(bytes) {
            Datagram::Audio(_) => Kind::Audio,
            Datagram::Chat { text, .. } => Kind::Chat(text),
            Datagram::Info(t) => Kind::Info(t),
            Datagram::Unsupported => Kind::OtherAudio,
        };
        record(shared, at, from, kind.clone());
        if kind == Kind::Audio {
            shared.audio_packets.fetch_add(1, Ordering::SeqCst);
            if shared.echo.load(Ordering::SeqCst) {
                send(sock, bytes, from);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{ListParser, LoginReply, Status, login_line, parse_login_reply};

    const WAIT: Duration = Duration::from_secs(2);

    /// What a client does: connect, write, half-close, read to EOF.
    fn exchange(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut s = TcpStream::connect_timeout(&addr, WAIT).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        s.write_all(request).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        out
    }

    fn client() -> UdpSocket {
        let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        s
    }

    fn until(mut ok: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < WAIT, "timed out waiting");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn fake_directory_records_the_login_line_and_serves_the_list() {
        let station = FakeStation::start("*ECHOTEST*");
        let entry = station.entry(9999);
        let dir = FakeDirectory::start(FakeLogin::Ok, vec![entry.clone()]);
        assert!(dir.addr().ip().is_loopback());

        let line = login_line("KX7ABC", "s3cret-pw", Status::Online, (8, 30), "Denver");
        let reply = exchange(dir.addr(), &line);
        assert_eq!(parse_login_reply(&reply, true), Some(LoginReply::Ok));

        let mut list = ListParser::new();
        list.push(&exchange(dir.addr(), LIST_REQUEST)).unwrap();
        assert_eq!(list.finish().unwrap().stations, vec![entry]);

        assert_eq!(dir.lines(), vec![line.to_vec(), LIST_REQUEST.to_vec()]);
        assert_eq!(dir.exchanges(), 2);

        dir.set_reply(FakeLogin::BadPassword);
        assert_eq!(
            parse_login_reply(&exchange(dir.addr(), &line), true),
            Some(LoginReply::BadPassword)
        );
        dir.set_reply(FakeLogin::Refuse("NOPE".into()));
        assert_eq!(
            parse_login_reply(&exchange(dir.addr(), &line), true),
            Some(LoginReply::Refused("NOPE".into()))
        );

        dir.set_refuse_connections(true);
        // Accepted and closed at once: the client sees no reply at all.
        let mut s = TcpStream::connect_timeout(&dir.addr(), WAIT).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        assert_eq!(out, [] as [u8; 0]);
        until(|| dir.exchanges() == 5);
        assert_eq!(dir.lines().len(), 4, "a refused connection is not read");

        dir.set_refuse_connections(false);
        dir.set_reply(FakeLogin::Silent);
        let mut s = TcpStream::connect_timeout(&dir.addr(), WAIT).unwrap();
        s.write_all(&line).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut buf = [0u8; 8];
        let e = s.read(&mut buf).unwrap_err();
        assert!(matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        until(|| dir.lines().len() == 5);
    }

    #[test]
    fn fake_station_answers_sdes_and_echoes_audio() {
        let station = FakeStation::start("*ECHOTEST*");
        let peer = station.addr();
        assert!(peer.audio.ip().is_loopback() && peer.control.ip().is_loopback());
        assert_ne!(peer.audio, peer.control);
        let entry = station.entry(9999);
        assert_eq!(entry.ip, Ipv4Addr::LOCALHOST);
        assert_eq!(entry.kind, crate::directory::StationKind::Conference);

        let (control, audio) = (client(), client());
        control
            .send_to(&rtcp::sdes("KX7ABC", "Rob"), peer.control)
            .unwrap();
        let mut buf = [0u8; 2048];
        let (n, from) = control.recv_from(&mut buf).unwrap();
        assert_eq!(from, peer.control);
        assert_eq!(
            rtcp::parse(&buf[..n]).unwrap(),
            Rtcp::Sdes {
                callsign: "*ECHOTEST*".into(),
                name: String::new()
            }
        );
        assert_eq!(station.controls_from(control.local_addr().unwrap()), 1);

        let pkt = AudioPacket {
            seq: 7,
            frames: [[0xD0; 33]; 4],
        }
        .encode();
        audio.send_to(&pkt, peer.audio).unwrap();
        let (n, from) = audio.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&pkt[..], peer.audio));
        assert_eq!(station.audio_packets(), 1);

        audio
            .send_to(&voice::chat_datagram("KX7ABC", "hello"), peer.audio)
            .unwrap();
        until(|| station.chats() == vec!["hello".to_owned()]);

        station.send_chat("hi back");
        let (n, _) = audio.recv_from(&mut buf).unwrap();
        assert_eq!(
            voice::classify(&buf[..n]),
            Datagram::Chat {
                from: "*ECHOTEST*".into(),
                text: "hi back".into()
            }
        );
        station.send_info("Echo test server");
        let (n, _) = audio.recv_from(&mut buf).unwrap();
        assert_eq!(
            voice::classify(&buf[..n]),
            Datagram::Info("Echo test server".into())
        );

        // Ten frames are three packets, the last filled out with silence.
        station.send_audio(
            PeerAddr {
                audio: audio.local_addr().unwrap(),
                control: control.local_addr().unwrap(),
            },
            10,
        );
        let mut seqs = Vec::new();
        for _ in 0..3 {
            let (n, _) = audio.recv_from(&mut buf).unwrap();
            assert_eq!(n, voice::AUDIO_PACKET_LEN);
            let Datagram::Audio(p) = voice::classify(&buf[..n]) else {
                panic!("not audio")
            };
            seqs.push(p.seq);
        }
        assert!(seqs.windows(2).all(|w| voice::seq_is_newer(w[1], w[0])));

        station.send_bye();
        let (n, _) = control.recv_from(&mut buf).unwrap();
        assert_eq!(rtcp::parse(&buf[..n]).unwrap(), Rtcp::Bye);

        station.set_answer(false);
        control
            .send_to(&rtcp::sdes("KX7ABC", "Rob"), peer.control)
            .unwrap();
        assert!(
            control.recv_from(&mut buf).is_err(),
            "SDES answered while off"
        );
        until(|| station.controls_from(control.local_addr().unwrap()) == 2);
    }

    #[test]
    fn fake_station_answers_from_another_port_and_records_infos() {
        let station = FakeStation::start("W1AW");
        station.set_answer_from_another_port(true);
        let (control, audio) = (client(), client());
        control
            .send_to(&rtcp::sdes("KX7ABC", ""), station.addr().control)
            .unwrap();
        let mut buf = [0u8; 2048];
        let (n, from) = control.recv_from(&mut buf).unwrap();
        assert!(matches!(rtcp::parse(&buf[..n]), Ok(Rtcp::Sdes { .. })));
        assert_eq!(from, station.another_port_addr());
        assert_ne!(from, station.addr().control);
        assert_eq!(station.sdes_count(), 1);

        audio
            .send_to(&voice::info_datagram("Denver"), station.addr().audio)
            .unwrap();
        until(|| station.infos() == vec!["Denver".to_owned()]);

        station.send_sdes_to(control.local_addr().unwrap());
        let (n, from) = control.recv_from(&mut buf).unwrap();
        assert!(matches!(rtcp::parse(&buf[..n]), Ok(Rtcp::Sdes { .. })));
        assert_eq!(from, station.addr().control);
        station.send_raw_control(control.local_addr().unwrap(), b"\xff");
        assert_eq!(control.recv_from(&mut buf).unwrap().0, 1);
        station.send_raw_audio(audio.local_addr().unwrap(), b"xyz");
        let (n, from) = audio.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"xyz"[..], station.addr().audio));
    }

    #[test]
    fn fake_station_with_echo_off_counts_but_sends_nothing_back() {
        let station = FakeStation::start("W1AW");
        station.set_echo(false);
        let audio = client();
        let pkt = AudioPacket {
            seq: 1,
            frames: [[0xD0; 33]; 4],
        }
        .encode();
        for _ in 0..3 {
            audio.send_to(&pkt, station.addr().audio).unwrap();
        }
        until(|| station.audio_packets() == 3);
        let mut buf = [0u8; 2048];
        assert!(audio.recv_from(&mut buf).is_err(), "audio came back");
    }

    #[test]
    fn fake_stamps_are_in_arrival_order() {
        let dir = FakeDirectory::start(FakeLogin::Ok, Vec::new());
        let station = FakeStation::start("W1AW");
        let control = client();

        let before = Instant::now();
        control
            .send_to(&rtcp::bye(), station.addr().control)
            .unwrap();
        until(|| station.bye_times().len() == 1);
        let line = login_line("KX7ABC", "pw", Status::Online, (0, 0), "");
        exchange(dir.addr(), &line);
        control
            .send_to(&rtcp::bye(), station.addr().control)
            .unwrap();
        until(|| station.bye_times().len() == 2);
        exchange(dir.addr(), LIST_REQUEST);

        let byes = station.bye_times();
        let lines = dir.line_times();
        let accepted = dir.exchange_times();
        assert_eq!(lines.len(), 2);
        assert!(before <= byes[0]);
        assert!(byes[0] < lines[0].0, "first BYE before the login line");
        assert!(lines[0].0 < byes[1], "login line before the second BYE");
        assert!(byes[1] < lines[1].0, "second BYE before the list request");
        assert_eq!(lines[0].1, line.to_vec());
        assert!(accepted[0] <= lines[0].0 && accepted[1] <= lines[1].0);
        assert!(accepted[0] < accepted[1]);
    }
}
