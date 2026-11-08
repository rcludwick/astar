// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The UDP side of `EchoLink`: one thread carrying every station astar has
//! called.
//!
//! A call to an `EchoLink` station uses two UDP ports on each end. The
//! control port (5199) carries RTCP: an SDES to connect, the same SDES every
//! 10 s as a keepalive, and a BYE to hang up. The audio port (5198) carries
//! the voice, four GSM frames (80 ms) to a datagram, plus `oNDATA` text:
//! the far station's information text and chat lines.
//!
//! [`EchoLinkLink`] runs one thread, `astar-echolink`, which owns both
//! sockets and a table of [`PeerSlot`]s, one per station called. Each slot
//! has its own [`Qso`] (the handshake and the clocks), its own GSM encoder
//! and decoder, and its audio ends: either the app's voice route
//! ([`PeerAudio::Client`]) or a conference member on astar-server
//! ([`PeerAudio::Member`]).
//!
//! # Who gets an answer
//!
//! astar only calls out. A datagram is matched to a slot by where it came
//! from; one that matches no slot is dropped, and nothing is sent back. In
//! production the match is by IP alone, because a NAT in front of a station
//! may rewrite the source port of its replies; tests match by the full
//! socket address so that a second fake on 127.0.0.1 counts as a stranger.
//!
//! # When the ports are open
//!
//! The sockets are bound when the first station is added and closed when the
//! last one goes, so astar holds no UDP port while it is merely logged in.
//!
//! # Receiving audio
//!
//! A packet whose sequence number is not newer than the last one played
//! (counting around the 16-bit wrap) is dropped. Each packet's four frames
//! go to the voice route in order, stamped on a sender clock built from the
//! sequence number ([`RxTimeline`]), so the station mixer plays them out of
//! the same adaptive jitter buffer an `AllStarLink` call uses. That is what
//! keeps a busy conference, which never pauses, from building up delay.
//!
//! # The packet log
//!
//! Each slot has its own [`CallPacketTap`], so each station is its own call
//! in the log. The tap reads the audio port only, as soon as a datagram is
//! matched to a slot and before anything decides what to do with it, so a
//! repeat the sequence gate drops is still logged. Its filter keeps the voice
//! packets and refuses the port's text. The control port has no tap.
//!
//! # Sending audio
//!
//! While PTT is applied, the capture channel's 160-sample frames are
//! gathered four to a packet. On key-up whatever is left is drained first
//! and a partial packet is filled out with silent frames. While PTT is not
//! applied the capture channel is drained and dropped on every pass, so
//! audio queued while receiving is never sent on the next key-down.
//!
//! # The transmit cap
//!
//! No over runs longer than [`Timings::max_transmit`], 10 minutes. A key
//! held down that long is treated as stuck: the link sends what is already
//! queued, pads the last packet out, and stops, which is all `EchoLink`
//! needs to end an over (the far end hears the audio stop). It stays
//! stopped until the key comes up: for the app that is PTT released, for a
//! conference member on astar-server a pause in the net of
//! `MEMBER_FLUSH`. The call says why in a plain sentence,
//! [`TX_CAP_NOTICE`] for the app and [`member_cap_notice`] for the node.
//! There is no setting that turns it off; it is the `EchoLink` twin of
//! D-Star's time-out timer.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::{CallAudio, RxFrame};
use astar_codec::gsm::{FRAME_SAMPLES, GsmDecoder, GsmEncoder};
use astar_echolink::directory::StationEntry;
use astar_echolink::qso::{Qso, QsoAction, QsoEvent, QsoState};
use astar_echolink::voice::{self, AudioPacket, Datagram, FRAMES_PER_PACKET, GSM_FRAME_LEN};
use astar_echolink::{PeerAddr, Timings};
use astar_record::packet_tap::{CallMeta, CallPacketTap, PacketCallId, PacketNetwork, PacketSlot};
use tracing::{debug, warn};

use super::client::ChatLine;
use super::rx_clock::RxTimeline;
use super::{EchoLinkNetwork, net};
use crate::voice_route::RateBridge;

/// How long the thread blocks on the audio socket each pass.
const READ_TIMEOUT: Duration = Duration::from_millis(50);
/// Receive buffer. Every datagram we accept is far shorter; a longer one is
/// cut here and then classified like any other.
const RECV_BUF: usize = 4096;
/// Most control datagrams read in one pass, so a flood cannot hold the
/// thread away from its clocks.
const CONTROL_BURST: usize = 64;
/// Chat lines kept per station; older ones are dropped first.
const MAX_CHAT_LINES: usize = 256;
/// A conference member's partial packet goes out padded once the conference
/// has sent nothing for this long.
const MEMBER_FLUSH: Duration = Duration::from_millis(100);
/// A pause after an unexpected socket error, so a socket that keeps failing
/// cannot spin the thread.
const ERROR_PAUSE: Duration = Duration::from_millis(10);

/// What the app's call says once the transmit cap has stopped an over.
pub const TX_CAP_NOTICE: &str = "EchoLink stopped transmitting after 10 minutes of continuous \
                                 talk. Release PTT to talk again.";

/// What the node's leg says once the transmit cap has stopped sending the
/// net to `call`.
#[must_use]
pub fn member_cap_notice(call: &str) -> String {
    format!(
        "Stopped sending the net to {call} after 10 minutes of continuous talk; it resumes \
         after the next pause."
    )
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where one station's contact stands, as the outside reads it.
pub(crate) type PeerState = QsoState;

/// What the link thread reports about one station, read from outside.
pub(crate) struct PeerShared {
    pub(crate) state: Mutex<PeerState>,
    /// The far end is talking (audio arrived within `talk_hold`).
    pub(crate) talking: AtomicBool,
    /// PTT as actually applied by the link thread, not as requested.
    pub(crate) ptt: AtomicBool,
    /// The far station's information text, once it has sent one.
    pub(crate) info: Mutex<Option<String>>,
    pub(crate) chat: Mutex<Vec<ChatLine>>,
    /// Why the link stopped transmitting on its own, until the next over.
    pub(crate) notice: Mutex<Option<String>>,
    /// How many times the transmit cap has fired on this station.
    pub(crate) caps: AtomicU32,
    /// This station's call in the packet log.
    pub(crate) packet_call: PacketCallId,
}

impl PeerShared {
    fn new(packet_call: PacketCallId) -> Self {
        Self {
            state: Mutex::new(QsoState::Connecting),
            talking: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            info: Mutex::new(None),
            chat: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
            caps: AtomicU32::new(0),
            packet_call,
        }
    }

    fn push_chat(&self, line: ChatLine) {
        let mut chat = lock(&self.chat);
        if chat.len() >= MAX_CHAT_LINES {
            chat.remove(0);
        }
        chat.push(line);
    }
}

/// Where a station's audio comes from and goes to.
#[allow(
    clippy::large_enum_variant,
    reason = "one per station, built once and never moved on the audio path"
)]
pub(crate) enum PeerAudio {
    /// The app's one call: the voice route's channel ends and the PTT request.
    Client {
        call_audio: CallAudio,
        ptt: Arc<AtomicBool>,
    },
    /// A conference member on astar-server, at the member's rate.
    Member {
        to_conference: Sender<RxFrame>,
        from_conference: Receiver<Vec<i16>>,
        /// 8 kHz to the member's rate, when they differ.
        rx_bridge: Option<RateBridge>,
        /// The member's rate to 8 kHz, when they differ.
        tx_bridge: Option<RateBridge>,
    },
}

/// The transmit side of one slot.
struct TxState {
    keyed: bool,
    seq: u16,
    pending: Vec<[i16; FRAME_SAMPLES]>,
    /// When a conference member last handed us a frame; cleared once the
    /// partial packet after it has been flushed.
    last_frame: Option<Instant>,
    /// When a conference member last handed us a frame, kept across the
    /// flush: what tells a continuing over from a new one.
    last_heard: Option<Instant>,
    cap: TxCap,
}

/// What [`TxCap::check`] decided for one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapVerdict {
    /// Send as usual.
    Send,
    /// The over just ran past the cap: stop now.
    Stop,
    /// The cap stopped this over earlier and the key is still down.
    Held,
}

/// The transmit cap's clock and latch for one station. Pure: the caller
/// hands in the clock, so a test can drive it with made-up instants.
#[derive(Debug, Default)]
struct TxCap {
    /// When the current over started.
    since: Option<Instant>,
    /// The cap stopped the current over; nothing goes out until the key
    /// comes up.
    tripped: bool,
}

impl TxCap {
    /// `keying` is whether the source wants to send on this pass. A pass
    /// with the key up ends the over and re-arms the cap.
    fn check(&mut self, keying: bool, now: Instant, cap: Duration) -> CapVerdict {
        if !keying {
            self.since = None;
            self.tripped = false;
            return CapVerdict::Send;
        }
        if self.tripped {
            return CapVerdict::Held;
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) >= cap {
            self.since = None;
            self.tripped = true;
            return CapVerdict::Stop;
        }
        CapVerdict::Send
    }
}

/// One station in the link's table.
pub(crate) struct PeerSlot {
    entry: StationEntry,
    addr: PeerAddr,
    qso: Qso,
    enc: GsmEncoder,
    dec: GsmDecoder,
    last_seq: Option<u16>,
    /// The sender clock the app's receive lane is played out against.
    rx_clock: RxTimeline,
    audio: PeerAudio,
    shared: Arc<PeerShared>,
    tx: TxState,
    info_sent: bool,
    /// What [`Qso::start`] asked for (the first SDES), sent once the slot is
    /// in the table and the sockets are open.
    first: Vec<QsoAction>,
    /// Copies this station's voice packets to the packet log. Dropped with
    /// the slot, which is what ends the call there.
    packet_tap: CallPacketTap,
}

enum Command {
    /// Add a station; the reply says whether the sockets could be opened.
    Add(Box<PeerSlot>, Sender<io::Result<()>>),
    /// Hang up on a station, by callsign.
    Remove(String),
    /// Send a chat line to every connected station.
    Chat(String),
    Shutdown,
}

/// The `EchoLink` UDP link: one thread, two sockets, any number of stations.
pub(crate) struct EchoLinkLink {
    commands: Sender<Command>,
    local: Arc<Mutex<Option<PeerAddr>>>,
    peer_addr: Arc<dyn Fn(&StationEntry) -> PeerAddr + Send + Sync>,
    my_call: String,
    timings: Timings,
    /// Where each station's packet tap offers its packets.
    packet_slot: PacketSlot,
    /// A tap's local address until the sockets are open.
    audio_bind: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl EchoLinkLink {
    /// Start the link thread for `my_call`. No socket is opened until the
    /// first station is added.
    pub(crate) fn start(
        my_call: &str,
        location: &str,
        net: &EchoLinkNetwork,
        packet_slot: PacketSlot,
    ) -> io::Result<Self> {
        let (commands, inbox) = mpsc::channel();
        let local = Arc::new(Mutex::new(None));
        let run = Run {
            my_call: my_call.to_owned(),
            location: location.to_owned(),
            audio_bind: net.audio_bind,
            control_bind: net.control_bind,
            match_by_ip: net.match_by_ip,
            max_transmit: net.timings.max_transmit,
            sockets: None,
            local: Arc::clone(&local),
            peers: Vec::new(),
        };
        let thread = thread::Builder::new()
            .name("astar-echolink".into())
            .spawn(move || run.run(&inbox))?;
        Ok(Self {
            commands,
            local,
            peer_addr: Arc::clone(&net.peer_addr),
            my_call: my_call.to_owned(),
            timings: net.timings.clone(),
            packet_slot,
            audio_bind: net.audio_bind,
            thread: Some(thread),
        })
    }

    /// Call a station: the first SDES goes out as soon as the sockets are
    /// open. Fails only when the sockets cannot be bound.
    pub(crate) fn add(&self, entry: StationEntry, audio: PeerAudio) -> io::Result<Arc<PeerShared>> {
        let addr = (self.peer_addr)(&entry);
        let (qso, first) = Qso::start(&self.my_call, self.timings.clone(), Instant::now());
        let packet_tap = CallPacketTap::new(
            self.packet_slot.clone(),
            voice::is_media,
            CallMeta {
                network: PacketNetwork::EchoLink,
                codec: None,
                label: format!("EchoLink {}", entry.call),
                local: self.audio_bind,
                peer: addr.audio,
            },
        );
        let shared = Arc::new(PeerShared::new(packet_tap.id()));
        let slot = PeerSlot {
            entry,
            addr,
            qso,
            enc: GsmEncoder::new(),
            dec: GsmDecoder::new(),
            last_seq: None,
            rx_clock: RxTimeline::new(Instant::now()),
            audio,
            shared: Arc::clone(&shared),
            tx: TxState {
                keyed: false,
                seq: 0,
                pending: Vec::with_capacity(FRAMES_PER_PACKET),
                last_frame: None,
                last_heard: None,
                cap: TxCap::default(),
            },
            info_sent: false,
            first,
            packet_tap,
        };
        let (reply, answer) = mpsc::channel();
        let stopped = || io::Error::other("echolink: the link thread has stopped");
        self.commands
            .send(Command::Add(Box::new(slot), reply))
            .map_err(|_| stopped())?;
        answer.recv().map_err(|_| stopped())??;
        Ok(shared)
    }

    /// Hang up on the station with this callsign.
    pub(crate) fn remove(&self, call: &str) {
        let _ = self.commands.send(Command::Remove(call.to_owned()));
    }

    /// Send a chat line to every connected station.
    pub(crate) fn chat(&self, text: &str) {
        let _ = self.commands.send(Command::Chat(text.to_owned()));
    }

    /// The bound audio and control addresses; `None` while no socket is open.
    pub(crate) fn local_addrs(&self) -> Option<PeerAddr> {
        *lock(&self.local)
    }
}

impl Drop for EchoLinkLink {
    /// Hang up on everyone (a BYE each) and wait for the thread, which is at
    /// most one pass away (`READ_TIMEOUT`).
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Sockets {
    audio: UdpSocket,
    control: UdpSocket,
}

impl Sockets {
    /// Bind both ports. The audio socket blocks for up to `READ_TIMEOUT` and
    /// paces the thread; the control socket is drained without blocking.
    fn open(audio: SocketAddr, control: SocketAddr) -> io::Result<Self> {
        let audio = net::bind_udp(audio)?;
        let control = net::bind_udp(control)?;
        audio.set_read_timeout(Some(READ_TIMEOUT))?;
        control.set_nonblocking(true)?;
        Ok(Self { audio, control })
    }

    fn local(&self) -> io::Result<PeerAddr> {
        Ok(PeerAddr {
            audio: self.audio.local_addr()?,
            control: self.control.local_addr()?,
        })
    }
}

/// The link thread's own state.
struct Run {
    my_call: String,
    location: String,
    audio_bind: SocketAddr,
    control_bind: SocketAddr,
    match_by_ip: bool,
    max_transmit: Duration,
    sockets: Option<Sockets>,
    local: Arc<Mutex<Option<PeerAddr>>>,
    peers: Vec<PeerSlot>,
}

impl Run {
    fn run(mut self, inbox: &Receiver<Command>) {
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            if !self.take_commands(inbox) {
                break;
            }
            let Some(sockets) = self.sockets.as_ref() else {
                continue;
            };
            let now = Instant::now();
            for slot in &mut self.peers {
                transmit(sockets, slot, now, self.max_transmit);
            }
            self.read_control(&mut buf);
            self.read_audio(&mut buf);
            let now = Instant::now();
            if let Some(sockets) = self.sockets.as_ref() {
                for slot in &mut self.peers {
                    let actions = slot.qso.tick(now);
                    apply(sockets, slot, actions, &self.location);
                }
            }
            self.reap();
        }
        self.hang_up_all();
    }

    /// Handle waiting commands. With no socket open there is nothing else to
    /// do, so this sleeps until the next one arrives.
    /// Returns false on `Shutdown` or when every handle is gone.
    fn take_commands(&mut self, inbox: &Receiver<Command>) -> bool {
        if self.sockets.is_none() {
            let Ok(cmd) = inbox.recv() else {
                return false;
            };
            if !self.command(cmd) {
                return false;
            }
        }
        loop {
            match inbox.try_recv() {
                Ok(cmd) => {
                    if !self.command(cmd) {
                        return false;
                    }
                }
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn command(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Add(slot, reply) => {
                let _ = reply.send(self.add(*slot));
            }
            Command::Remove(call) => {
                if let (Some(sockets), Some(slot)) = (
                    self.sockets.as_ref(),
                    self.peers.iter_mut().find(|s| s.entry.call == call),
                ) {
                    hang_up(sockets, slot, &self.location);
                }
                self.reap();
            }
            Command::Chat(text) => self.chat(&text),
            Command::Shutdown => return false,
        }
        true
    }

    fn add(&mut self, mut slot: PeerSlot) -> io::Result<()> {
        if self.sockets.is_none() {
            let sockets = Sockets::open(self.audio_bind, self.control_bind)?;
            *lock(&self.local) = Some(sockets.local()?);
            debug!(local = ?sockets.local().ok(), "echolink: UDP ports open");
            self.sockets = Some(sockets);
        }
        if let Some(local) = *lock(&self.local) {
            slot.packet_tap.set_local(local.audio);
        }
        if let Some(sockets) = self.sockets.as_ref() {
            let first = std::mem::take(&mut slot.first);
            apply(sockets, &mut slot, first, &self.location);
        }
        debug!(call = %slot.entry.call, "echolink: calling");
        self.peers.push(slot);
        Ok(())
    }

    fn chat(&mut self, text: &str) {
        let Some(sockets) = self.sockets.as_ref() else {
            return;
        };
        // One line: a CR or LF would end the datagram's text early.
        let text = text.replace(['\r', '\n'], " ");
        let datagram = voice::chat_datagram(&self.my_call, &text);
        // What the far end will read, after the length cap.
        let sent = match voice::classify(&datagram) {
            Datagram::Chat { text, .. } => text,
            _ => text,
        };
        for slot in &mut self.peers {
            if *lock(&slot.shared.state) != QsoState::Connected {
                continue;
            }
            send(&sockets.audio, &datagram, slot.addr.audio);
            slot.shared.push_chat(ChatLine {
                from: self.my_call.clone(),
                text: sent.clone(),
                mine: true,
            });
        }
    }

    /// Read every waiting control datagram, up to `CONTROL_BURST`.
    fn read_control(&mut self, buf: &mut [u8]) {
        for _ in 0..CONTROL_BURST {
            let Some(sockets) = self.sockets.as_ref() else {
                return;
            };
            let (n, from) = match sockets.control.recv_from(buf) {
                Ok(got) => got,
                Err(e) if is_quiet(&e) => return,
                Err(e) => {
                    debug!(error = %e, "echolink: control socket error");
                    return;
                }
            };
            let Some(slot) = find(&mut self.peers, from, self.match_by_ip, |a| a.control) else {
                debug!(%from, "echolink: control datagram from a station we did not call");
                continue;
            };
            let actions = slot.qso.on_control(&buf[..n], Instant::now());
            apply(sockets, slot, actions, &self.location);
        }
    }

    /// Wait up to `READ_TIMEOUT` for one audio-port datagram and handle it.
    fn read_audio(&mut self, buf: &mut [u8]) {
        let Some(sockets) = self.sockets.as_ref() else {
            return;
        };
        let (n, from) = match sockets.audio.recv_from(buf) {
            Ok(got) => got,
            Err(e) if is_quiet(&e) => return,
            Err(e) => {
                debug!(error = %e, "echolink: audio socket error");
                thread::sleep(ERROR_PAUSE);
                return;
            }
        };
        let at = Instant::now();
        let Some(slot) = find(&mut self.peers, from, self.match_by_ip, |a| a.audio) else {
            debug!(%from, "echolink: audio datagram from a station we did not call");
            return;
        };
        slot.packet_tap.capture_at(at, from, &buf[..n]);
        match voice::classify(&buf[..n]) {
            Datagram::Audio(packet) => {
                if *lock(&slot.shared.state) != QsoState::Connected {
                    return;
                }
                let actions = slot.qso.on_audio(Instant::now());
                apply(sockets, slot, actions, &self.location);
                receive(slot, &packet, Instant::now());
            }
            Datagram::Info(text) => *lock(&slot.shared.info) = Some(text),
            Datagram::Chat { from, text } => {
                let from = if from.trim().is_empty() {
                    slot.entry.call.clone()
                } else {
                    from
                };
                slot.shared.push_chat(ChatLine {
                    from,
                    text,
                    mine: false,
                });
            }
            Datagram::Unsupported => {
                debug!(
                    len = n,
                    "echolink: dropped an unsupported audio-port datagram"
                );
            }
        }
    }

    /// Drop closed stations; with none left, close the sockets.
    fn reap(&mut self) {
        self.peers.retain(|slot| {
            let open = !matches!(slot.qso.state(), QsoState::Closed(_));
            if !open {
                debug!(call = %slot.entry.call, state = ?slot.qso.state(), "echolink: contact ended");
            }
            open
        });
        if self.peers.is_empty() && self.sockets.is_some() {
            self.sockets = None;
            *lock(&self.local) = None;
            debug!("echolink: UDP ports closed");
        }
    }

    fn hang_up_all(&mut self) {
        if let Some(sockets) = self.sockets.as_ref() {
            for slot in &mut self.peers {
                hang_up(sockets, slot, &self.location);
            }
        }
        self.reap();
    }
}

/// A read that found nothing: the timeout, a non-blocking socket with
/// nothing waiting, or a signal.
fn is_quiet(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// The slot a datagram from `from` belongs to, if any.
fn find(
    peers: &mut [PeerSlot],
    from: SocketAddr,
    match_by_ip: bool,
    port: fn(&PeerAddr) -> SocketAddr,
) -> Option<&mut PeerSlot> {
    peers.iter_mut().find(|slot| {
        let want = port(&slot.addr);
        if match_by_ip {
            want.ip() == from.ip()
        } else {
            want == from
        }
    })
}

fn send(sock: &UdpSocket, bytes: &[u8], to: SocketAddr) {
    if let Err(e) = net::send_to(sock, bytes, to) {
        warn!(%to, error = %e, "echolink: send failed");
    }
}

/// Carry out what a station's [`Qso`] asked for.
fn apply(sockets: &Sockets, slot: &mut PeerSlot, actions: Vec<QsoAction>, location: &str) {
    for action in actions {
        match action {
            QsoAction::SendControl(bytes) => send(&sockets.control, &bytes, slot.addr.control),
            QsoAction::Event(QsoEvent::Connected { callsign }) => {
                debug!(call = %slot.entry.call, answered_as = %callsign, "echolink: connected");
                *lock(&slot.shared.state) = QsoState::Connected;
                if !slot.info_sent {
                    slot.info_sent = true;
                    send(
                        &sockets.audio,
                        &voice::info_datagram(location),
                        slot.addr.audio,
                    );
                }
            }
            QsoAction::Event(QsoEvent::Closed(reason)) => {
                *lock(&slot.shared.state) = QsoState::Closed(reason);
                slot.shared.talking.store(false, Ordering::Relaxed);
                slot.shared.ptt.store(false, Ordering::Relaxed);
            }
            QsoAction::Event(QsoEvent::TalkStart) => {
                slot.shared.talking.store(true, Ordering::Relaxed);
            }
            QsoAction::Event(QsoEvent::TalkStop) => {
                slot.shared.talking.store(false, Ordering::Relaxed);
                // A new transmission may restart its sequence numbers, so
                // the gate starts afresh rather than muting it, and so does
                // the receive clock.
                slot.last_seq = None;
                slot.rx_clock.end_over();
            }
        }
    }
}

/// End the contact from our side. A transmission in progress is finished
/// first: what the mic already queued goes out, the last partial packet
/// padded with silence, and only then the BYE.
fn hang_up(sockets: &Sockets, slot: &mut PeerSlot, location: &str) {
    if slot.tx.keyed && *lock(&slot.shared.state) == QsoState::Connected {
        if let PeerAudio::Client { call_audio, .. } = &slot.audio {
            let frames: Vec<Vec<i16>> = call_audio.tx_frames.try_iter().collect();
            for frame in frames {
                if let Ok(pcm) = <[i16; FRAME_SAMPLES]>::try_from(frame.as_slice()) {
                    queue_frame(sockets, slot, &pcm);
                }
            }
        }
        flush_partial(sockets, slot);
        slot.tx.keyed = false;
    }
    let actions = slot.qso.hang_up();
    apply(sockets, slot, actions, location);
}

/// Play one received packet, unless it is older than the last one played.
///
/// The app's frames go out stamped on the sender's clock (see
/// [`RxTimeline`]), so the station mixer plays them out of its jitter
/// buffer. A conference member's go out bare: the conference mixes on its
/// own tick and ignores a sender clock.
fn receive(slot: &mut PeerSlot, packet: &AudioPacket, now: Instant) {
    if slot
        .last_seq
        .is_some_and(|last| !voice::seq_is_newer(packet.seq, last))
    {
        return;
    }
    slot.last_seq = Some(packet.seq);
    let pcm: Vec<Vec<i16>> = packet
        .frames
        .iter()
        .map(|frame| slot.dec.decode(frame).to_vec())
        .collect();
    match &mut slot.audio {
        PeerAudio::Client { call_audio, .. } => {
            for frame in slot.rx_clock.stamp(packet.seq, now, pcm) {
                let _ = call_audio.rx_frames.send(frame);
            }
        }
        PeerAudio::Member {
            to_conference,
            rx_bridge,
            ..
        } => {
            for pcm in pcm {
                let pcm = match rx_bridge {
                    Some(bridge) => match bridge.convert(&pcm) {
                        Some(pcm) => pcm,
                        None => continue,
                    },
                    None => pcm,
                };
                let _ = to_conference.send(RxFrame::from(pcm));
            }
        }
    }
}

/// One pass of the transmit side for one station.
fn transmit(sockets: &Sockets, slot: &mut PeerSlot, now: Instant, max_transmit: Duration) {
    let connected = *lock(&slot.shared.state) == QsoState::Connected;
    match &slot.audio {
        PeerAudio::Client { call_audio, ptt } => {
            let request = connected && ptt.load(Ordering::Acquire);
            let verdict = slot.tx.cap.check(request, now, max_transmit);
            if verdict == CapVerdict::Stop {
                warn!(
                    call = %slot.entry.call,
                    "echolink: transmission reached the {max_transmit:?} transmit cap — unkeying \
                     (release PTT to re-arm)"
                );
                capped(&slot.shared, TX_CAP_NOTICE.to_owned());
            }
            let want = request && verdict == CapVerdict::Send;
            if want && !slot.tx.keyed {
                slot.tx.keyed = true;
                slot.tx.pending.clear();
                slot.shared.ptt.store(true, Ordering::Relaxed);
                *lock(&slot.shared.notice) = None;
            }
            if !slot.tx.keyed {
                // Not transmitting: what the mic queued is never sent.
                while call_audio.tx_frames.try_recv().is_ok() {}
                return;
            }
            let frames: Vec<Vec<i16>> = call_audio.tx_frames.try_iter().collect();
            for frame in frames {
                if let Ok(pcm) = <[i16; FRAME_SAMPLES]>::try_from(frame.as_slice()) {
                    queue_frame(sockets, slot, &pcm);
                }
            }
            if !want {
                if connected {
                    flush_partial(sockets, slot);
                }
                slot.tx.pending.clear();
                slot.tx.keyed = false;
                slot.shared.ptt.store(false, Ordering::Relaxed);
            }
        }
        PeerAudio::Member {
            from_conference, ..
        } => {
            let frames: Vec<Vec<i16>> = from_conference.try_iter().collect();
            if !connected {
                return;
            }
            if !frames.is_empty() {
                slot.tx.last_heard = Some(now);
            }
            // The net counts as keyed until it has been quiet for
            // MEMBER_FLUSH, the same pause that ends a packet.
            let keying = slot
                .tx
                .last_heard
                .is_some_and(|t| now.saturating_duration_since(t) < MEMBER_FLUSH);
            match slot.tx.cap.check(keying, now, max_transmit) {
                CapVerdict::Send => {}
                CapVerdict::Stop => {
                    warn!(
                        call = %slot.entry.call,
                        "echolink: the net has been sending to this station for the \
                         {max_transmit:?} transmit cap — stopping until it pauses"
                    );
                    flush_partial(sockets, slot);
                    slot.tx.last_frame = None;
                    capped(&slot.shared, member_cap_notice(&slot.entry.call));
                    return;
                }
                CapVerdict::Held => return,
            }
            if !frames.is_empty() {
                *lock(&slot.shared.notice) = None;
            }
            for frame in frames {
                let pcm = match &mut slot.audio {
                    PeerAudio::Member {
                        tx_bridge: Some(bridge),
                        ..
                    } => match bridge.convert(&frame) {
                        Some(pcm) => pcm,
                        None => continue,
                    },
                    _ => frame,
                };
                if let Ok(pcm) = <[i16; FRAME_SAMPLES]>::try_from(pcm.as_slice()) {
                    slot.tx.last_frame = Some(now);
                    queue_frame(sockets, slot, &pcm);
                }
            }
            if slot
                .tx
                .last_frame
                .is_some_and(|t| now.saturating_duration_since(t) >= MEMBER_FLUSH)
            {
                flush_partial(sockets, slot);
                slot.tx.last_frame = None;
            }
        }
    }
}

/// Record that the transmit cap stopped an over, and why.
fn capped(shared: &PeerShared, notice: String) {
    *lock(&shared.notice) = Some(notice);
    shared.caps.fetch_add(1, Ordering::Relaxed);
}

/// Add a frame; four make a packet, which goes out at once.
fn queue_frame(sockets: &Sockets, slot: &mut PeerSlot, pcm: &[i16; FRAME_SAMPLES]) {
    slot.tx.pending.push(*pcm);
    if slot.tx.pending.len() == FRAMES_PER_PACKET {
        send_packet(sockets, slot);
    }
}

/// Fill a partial packet out with silent frames and send it.
fn flush_partial(sockets: &Sockets, slot: &mut PeerSlot) {
    if slot.tx.pending.is_empty() {
        return;
    }
    slot.tx
        .pending
        .resize(FRAMES_PER_PACKET, [0; FRAME_SAMPLES]);
    send_packet(sockets, slot);
}

fn send_packet(sockets: &Sockets, slot: &mut PeerSlot) {
    let mut frames = [[0u8; GSM_FRAME_LEN]; FRAMES_PER_PACKET];
    for (out, pcm) in frames.iter_mut().zip(slot.tx.pending.drain(..)) {
        *out = slot.enc.encode(&pcm);
    }
    slot.tx.seq = slot.tx.seq.wrapping_add(1);
    let packet = AudioPacket {
        seq: slot.tx.seq,
        frames,
    }
    .encode();
    send(&sockets.audio, &packet, slot.addr.audio);
}

#[cfg(test)]
mod cap_tests {
    //! The transmit cap against made-up instants, so ten minutes take no
    //! time at all.
    use super::*;

    const CAP: Duration = Duration::from_secs(600);

    #[test]
    fn an_over_shorter_than_the_cap_is_never_stopped() {
        let t0 = Instant::now();
        let mut cap = TxCap::default();
        assert_eq!(cap.check(true, t0, CAP), CapVerdict::Send);
        assert_eq!(
            cap.check(true, t0 + CAP.saturating_sub(Duration::from_millis(1)), CAP),
            CapVerdict::Send
        );
    }

    #[test]
    fn ten_minutes_of_keying_stops_the_over_and_holds_it() {
        let t0 = Instant::now();
        let mut cap = TxCap::default();
        assert_eq!(cap.check(true, t0, CAP), CapVerdict::Send);
        assert_eq!(cap.check(true, t0 + CAP, CAP), CapVerdict::Stop);
        // The key is still down: nothing goes out, however long it stays.
        assert_eq!(cap.check(true, t0 + CAP * 2, CAP), CapVerdict::Held);
        assert_eq!(cap.check(true, t0 + CAP * 5, CAP), CapVerdict::Held);
    }

    #[test]
    fn releasing_the_key_re_arms_and_restarts_the_clock() {
        let t0 = Instant::now();
        let mut cap = TxCap::default();
        cap.check(true, t0, CAP);
        assert_eq!(cap.check(true, t0 + CAP, CAP), CapVerdict::Stop);
        assert_eq!(cap.check(false, t0 + CAP, CAP), CapVerdict::Send);
        let t1 = t0 + CAP + Duration::from_secs(1);
        assert_eq!(cap.check(true, t1, CAP), CapVerdict::Send);
        assert_eq!(
            cap.check(true, t1 + CAP.saturating_sub(Duration::from_secs(1)), CAP),
            CapVerdict::Send
        );
        assert_eq!(cap.check(true, t1 + CAP, CAP), CapVerdict::Stop);
    }

    #[test]
    fn a_short_pause_starts_a_new_over() {
        let t0 = Instant::now();
        let mut cap = TxCap::default();
        cap.check(true, t0, CAP);
        cap.check(false, t0 + CAP / 2, CAP);
        // A fresh over: the first half does not count against it.
        assert_eq!(cap.check(true, t0 + CAP / 2, CAP), CapVerdict::Send);
        assert_eq!(cap.check(true, t0 + CAP, CAP), CapVerdict::Send);
    }

    #[test]
    fn the_sentences_name_ten_minutes() {
        assert!(TX_CAP_NOTICE.contains("10 minutes of continuous talk"));
        assert!(member_cap_notice("W1AW").contains("W1AW"));
        assert!(member_cap_notice("W1AW").contains("10 minutes of continuous talk"));
    }
}
