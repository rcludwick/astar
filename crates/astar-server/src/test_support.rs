// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Helpers shared by this crate's `#[cfg(test)]` modules.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use astar_m17::{BROADCAST, ControlPacket, Lsf, StreamPacket, encode_callsign};

/// `[packet_log]` with debug mode on, over `dir`.
pub(crate) fn packet_log_cfg(dir: &Path) -> crate::config::PacketLogCfg {
    crate::config::PacketLogCfg {
        debug_mode: true,
        dir: dir.to_path_buf(),
        ..crate::config::PacketLogCfg::default()
    }
}

/// A closed packet log as the logger would have left it: a pcap of `body`
/// and its companion. Returns the pcap's name.
pub(crate) fn seed_packet_log(dir: &Path, stem: &str, body: &[u8]) -> String {
    let name = format!("{stem}.pcap");
    std::fs::write(dir.join(&name), body).unwrap();
    std::fs::write(
        dir.join(format!("{stem}.jitter.jsonl")),
        "{\"astar_packet_log\":1}\n{\"end\":\"hangup\"}\n",
    )
    .unwrap();
    name
}

/// A loopback UDP address that is open but will never answer, plus the
/// socket that makes it so.
///
/// Link tests need a peer to dial that leaves the call in `Connecting` for
/// the length of an assertion. Two addresses that look obvious are both
/// wrong:
///
/// * **`127.0.0.1:4569` is the well-known IAX2 port.** On any machine where
///   an astar node (or any other IAX2 daemon) happens to be running, that
///   peer *answers* the NEW — so the dial completes, the link is torn down
///   again, and the assertion that a link is in the roster fails at random.
///   Worse, `cargo test` would be poking a live daemon.
/// * **A closed port is not silent either.** UDP to a closed localhost port
///   draws an ICMP "port unreachable" that the OS surfaces as
///   `ECONNREFUSED` on the dialling socket's next `recv_from`, which the
///   call run-loop treats as fatal and exits within milliseconds.
///
/// So: bind an ephemeral port and never read it. Nothing else on the
/// machine can own it while we hold it, the port stays open (no ICMP), and
/// nothing is listening to reply — the call sits in `NewSent` for its full
/// retry budget, far longer than any test.
///
/// **Bind the socket to a named `_guard`, not to `_`.** A bare `_` pattern
/// drops the value immediately, which closes the port and reintroduces the
/// ICMP refusal this helper exists to avoid.
pub(crate) fn dead_port() -> (UdpSocket, String) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind a silent loopback sink");
    let addr = sock
        .local_addr()
        .expect("the sink's own address")
        .to_string();
    (sock, addr)
}

/// `count` stream packets of a 1 kHz tone from `callsign`, encoded with the
/// static Codec 2 the test build links, ending with EOS. Test-only use of
/// `astar_codec`: the node's own M17 code never names it. Shared by the
/// controller's M17 tests and the Discord adapter's tests.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn far_talker_packets(callsign: &str, count: u16) -> Vec<Vec<u8>> {
    let (mut codec, _) = astar_codec::codec2::open_codec2(&[])
        .expect("the codec2-static dev-dependency always provides a codec");
    let mut phase = 0.0_f32;
    let mut frame = || {
        let mut pcm = [0_i16; 160];
        for s in &mut pcm {
            *s = (phase.sin() * 8000.0) as i16;
            phase += 2.0 * std::f32::consts::PI * 1000.0 / 8000.0;
        }
        codec.encode(&pcm)
    };
    (0..count)
        .map(|k| {
            let mut payload = [0u8; 16];
            payload[..8].copy_from_slice(&frame());
            payload[8..].copy_from_slice(&frame());
            let frame_number = if k + 1 == count {
                k | StreamPacket::EOS_BIT
            } else {
                k
            };
            StreamPacket {
                stream_id: 0x4D17,
                lsf: Lsf {
                    dst: BROADCAST,
                    src: encode_callsign(callsign).expect("a valid base-40 callsign"),
                    type_field: Lsf::TYPE_VOICE_3200_STREAM,
                    meta: [0; 14],
                },
                frame_number,
                payload,
            }
            .to_bytes()
            .to_vec()
        })
        .collect()
}

/// Poll `pred` every 10 ms until it holds or `timeout_ms` passes, then ask
/// once more. The house style for waits (iax-3f7a): a deadline loop, never a
/// single fixed-timeout read that flakes on a loaded machine.
pub(crate) fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    pred()
}

/// How a [`RecordingReflector`] answers a CONN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FakeReply {
    /// ACKN, then a PING every 100 ms until the client sends DISC.
    Ackn,
    /// NACK: the link is refused.
    Nack,
    /// Nothing, ever. The monitor stays "linking".
    Silent,
}

/// A raw-UDP fake M17 reflector on `127.0.0.1:0` that records every datagram
/// it receives, in arrival order.
///
/// The node's M17 monitor must never send anything but CONN, PONG and DISC,
/// and proving that needs every byte it sent. `astar_m17::Reflector` relays
/// and reaps, which would hide exactly that. This only ever sends to the
/// loopback peer that wrote to it.
pub(crate) struct RecordingReflector {
    /// Where to point a `[m17.reflectors]` entry.
    pub(crate) addr: SocketAddr,
    log: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecordingReflector {
    /// Bind `127.0.0.1:0` and start recording, answering every CONN with `reply`.
    pub(crate) fn start(reply: FakeReply) -> RecordingReflector {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind the recording reflector");
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("set the recorder's read timeout");
        let addr = socket.local_addr().expect("the recorder's own address");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_log = Arc::clone(&log);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("test-m17-recorder".into())
            .spawn(move || record(&socket, reply, &thread_log, &thread_stop))
            .expect("spawn the recording reflector");
        RecordingReflector {
            addr,
            log,
            stop,
            thread: Some(thread),
        }
    }

    /// Everything received so far, in order.
    pub(crate) fn datagrams(&self) -> Vec<Vec<u8>> {
        self.log
            .lock()
            .expect("recorder log mutex poisoned")
            .clone()
    }

    /// Stop the thread and join it.
    pub(crate) fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RecordingReflector {
    fn drop(&mut self) {
        self.halt();
    }
}

/// The recorder's loop: log every datagram, answer CONN per `reply`, PING a
/// linked peer every 100 ms, forget it on DISC.
fn record(socket: &UdpSocket, reply: FakeReply, log: &Mutex<Vec<Vec<u8>>>, stop: &AtomicBool) {
    let ping = ControlPacket::Ping {
        callsign: encode_callsign("M17-TEST").expect("a valid base-40 name"),
    }
    .to_bytes();
    let mut linked: Option<SocketAddr> = None;
    let mut next_ping = Instant::now();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if let Ok((n, src)) = socket.recv_from(&mut buf) {
            log.lock()
                .expect("recorder log mutex poisoned")
                .push(buf[..n].to_vec());
            match (ControlPacket::parse(&buf[..n]), reply) {
                (Some(ControlPacket::Conn { .. }), FakeReply::Ackn) => {
                    let _ = socket.send_to(b"ACKN", src);
                    linked = Some(src);
                }
                (Some(ControlPacket::Conn { .. }), FakeReply::Nack) => {
                    let _ = socket.send_to(b"NACK", src);
                }
                (Some(ControlPacket::Disc { .. }), _) => linked = None,
                _ => {}
            }
        }
        if let Some(peer) = linked
            && Instant::now() >= next_ping
        {
            let _ = socket.send_to(&ping, peer);
            next_ping = Instant::now() + Duration::from_millis(100);
        }
    }
}
