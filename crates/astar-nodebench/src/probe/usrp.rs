// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The ASL3 node's "radio": a USRP endpoint in `chan_usrp`'s wire format.
//!
//! The node's `rxchannel = USRP/127.0.0.1:<bench port>:<node port>` makes
//! `chan_usrp` bind `<node port>` and send to `<bench port>`; the bench binds
//! `<bench port>` and sends to `<node port>`.
//!
//! # Wire format (from ASL3 `app_rpt` `channels/chan_usrp.c`)
//!
//! A 32-byte header, then (for voice) 160 samples of 8 kHz signed-linear:
//!
//! | offset | field | encoding |
//! |---|---|---|
//! | 0 | `eye` | the four bytes `USRP` |
//! | 4 | `seq` | u32, **big-endian** (`htonl(txseq++)`; read with `ntohl`) |
//! | 8 | `memory` | u32, always 0 |
//! | 12 | `keyup` | u32, **big-endian** (`htonl(1)` on every voice frame; 0 on the unkey packet) |
//! | 16 | `talkgroup` | u32, always 0 |
//! | 20 | `type` | u32: 0 voice, 1 DTMF, 2 text. **Host order**: `chan_usrp` compares it raw, never `ntohl` (so 2 is `02 00 00 00` on the little-endian arm64/amd64 hosts the bench runs on) |
//! | 24 | `mpxid` | u32, 0 |
//! | 28 | `reserved` | u32, 0 |
//! | 32 | samples | 160 × i16, **host order** (`memcpy` of an `ast_format_slin` frame: little-endian on every host the bench runs on) |
//!
//! Behaviour that matters to the bench:
//!
//! * **Node → bench.** While the node transmits, every 20 ms voice frame goes
//!   out with `keyup = 1`. When it unkeys, `chan_usrp` sends one header-only
//!   (32-byte) packet, all zero but `eye` and `seq` — `keyup = 0`.
//! * **Bench → node.** `chan_usrp` ignores `keyup` on receive. It queues any
//!   packet whose payload is exactly 320 bytes, raises RX-key on the node as
//!   soon as the queue is non-empty, and drops RX-key a few frame-times after
//!   the queue runs dry (`MAX_RXKEY_TIME`). So "PTT" from the bench *is* a
//!   stream of voice frames; [`UsrpProbe::transmit`] still sets `keyup = 1`
//!   and sends the unkey packet, as `chan_usrp` itself does, for any other
//!   USRP peer. It drains the queue one frame per node write, so frames must
//!   be paced at 20 ms or the overload threshold flushes them.
//! * A `seq` that is not the previous + 1 makes the node log "Possible data
//!   loss"; seq 0 is exempt.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{RxFrame, TxReport, frames_of, sleep_until};

/// Header length.
pub const HEADER_LEN: usize = 32;
/// Samples in one voice frame (20 ms at 8 kHz).
pub const VOICE_SAMPLES: usize = 160;
/// The sample rate `chan_usrp` speaks.
pub const SAMPLE_RATE: u32 = 8000;
const FRAME: Duration = Duration::from_millis(20);

/// USRP packet `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsrpType {
    Voice,
    Dtmf,
    Text,
    Other(u32),
}

impl UsrpType {
    fn code(self) -> u32 {
        match self {
            Self::Voice => 0,
            Self::Dtmf => 1,
            Self::Text => 2,
            Self::Other(c) => c,
        }
    }
    fn from_code(c: u32) -> Self {
        match c {
            0 => Self::Voice,
            1 => Self::Dtmf,
            2 => Self::Text,
            o => Self::Other(o),
        }
    }
}

/// One decoded USRP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsrpPacket {
    pub seq: u32,
    pub memory: u32,
    pub keyup: bool,
    pub talkgroup: u32,
    pub kind: UsrpType,
    pub mpxid: u32,
    pub reserved: u32,
    /// Voice samples (only when the payload is exactly one voice frame).
    pub audio: Vec<i16>,
    /// The raw payload after the header (text / DTMF / odd-sized voice).
    pub payload: Vec<u8>,
}

impl UsrpPacket {
    /// A keyed voice frame.
    #[must_use]
    pub fn voice(seq: u32, audio: Vec<i16>) -> Self {
        Self {
            seq,
            memory: 0,
            keyup: true,
            talkgroup: 0,
            kind: UsrpType::Voice,
            mpxid: 0,
            reserved: 0,
            audio,
            payload: Vec::new(),
        }
    }

    /// The header-only unkey packet `chan_usrp` sends when TX drops.
    #[must_use]
    pub fn unkey(seq: u32) -> Self {
        Self {
            keyup: false,
            ..Self::voice(seq, Vec::new())
        }
    }

    /// Encode to wire bytes (see the module table for byte order).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.audio.len() * 2 + self.payload.len());
        out.extend_from_slice(b"USRP");
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.memory.to_be_bytes());
        out.extend_from_slice(&u32::from(self.keyup).to_be_bytes());
        out.extend_from_slice(&self.talkgroup.to_be_bytes());
        out.extend_from_slice(&self.kind.code().to_le_bytes());
        out.extend_from_slice(&self.mpxid.to_be_bytes());
        out.extend_from_slice(&self.reserved.to_be_bytes());
        for s in &self.audio {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode wire bytes. `None` if shorter than the header or the eye is not
    /// `USRP` (`chan_usrp` drops those too).
    #[must_use]
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN || &buf[0..4] != b"USRP" {
            return None;
        }
        let be = |o: usize| u32::from_be_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let le = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        // `type` is host order in chan_usrp; accept a network-order sender too.
        let (host, net) = (le(20), be(20));
        let type_code = if host > 2 && net <= 2 { net } else { host };
        let kind = UsrpType::from_code(type_code);
        let body = &buf[HEADER_LEN..];
        let (audio, payload) = if kind == UsrpType::Voice && body.len() == VOICE_SAMPLES * 2 {
            let a = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect();
            (a, Vec::new())
        } else {
            (Vec::new(), body.to_vec())
        };
        Some(Self {
            seq: be(4),
            memory: be(8),
            keyup: be(12) != 0,
            talkgroup: be(16),
            kind,
            mpxid: be(24),
            reserved: be(28),
            audio,
            payload,
        })
    }
}

/// A USRP endpoint on a UDP socket. Cheap to share across threads: transmit
/// and record each work on their own clone of the socket.
pub struct UsrpProbe {
    sock: UdpSocket,
    remote: Mutex<SocketAddr>,
    seq: Arc<AtomicU32>,
}

impl UsrpProbe {
    /// Bind `local` and send to `remote` (both should be 127.0.0.1).
    ///
    /// # Errors
    /// Binding the socket.
    pub fn bind(local: SocketAddr, remote: SocketAddr) -> io::Result<Self> {
        let sock = UdpSocket::bind(local)?;
        Ok(Self {
            sock,
            remote: Mutex::new(remote),
            // chan_usrp exempts seq 0 from its continuity check; start at 1 so
            // our very first frame is checked like the rest.
            seq: Arc::new(AtomicU32::new(1)),
        })
    }

    /// The bound address.
    ///
    /// # Errors
    /// Reading the socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    /// Where frames go.
    #[must_use]
    pub fn remote(&self) -> SocketAddr {
        *self.remote.lock().expect("remote lock")
    }

    /// Change where frames go (for tests that bind both ends on port 0).
    pub fn set_remote(&self, remote: SocketAddr) {
        *self.remote.lock().expect("remote lock") = remote;
    }

    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Send one packet now.
    ///
    /// # Errors
    /// The UDP send.
    pub fn send(&self, pkt: &UsrpPacket) -> io::Result<()> {
        self.sock.send_to(&pkt.encode(), self.remote()).map(|_| ())
    }

    /// Key up and send `audio` (8 kHz) as 20 ms voice frames paced on an
    /// absolute 20 ms schedule, zero-padding the last frame, then send the
    /// unkey packet. Blocks for the duration.
    ///
    /// # Errors
    /// Any UDP send.
    pub fn transmit(&self, audio: &[i16]) -> io::Result<TxReport> {
        let frames = frames_of(audio, VOICE_SAMPLES);
        let started = Instant::now();
        for (i, f) in frames.iter().enumerate() {
            sleep_until(started + FRAME * u32::try_from(i).unwrap_or(u32::MAX));
            self.send(&UsrpPacket::voice(self.next_seq(), f.clone()))?;
        }
        sleep_until(started + FRAME * u32::try_from(frames.len()).unwrap_or(u32::MAX));
        self.send(&UsrpPacket::unkey(self.next_seq()))?;
        Ok(TxReport {
            started,
            finished: Instant::now(),
            frames: frames.len(),
        })
    }

    /// Receive for `duration`, returning every valid USRP packet with its
    /// arrival time: voice frames (`keyed`, 160 samples, `seq`) and unkey
    /// packets (not keyed, no samples). Packets from any source but the
    /// remote are ignored.
    ///
    /// # Errors
    /// Socket errors other than the read timeout.
    pub fn record(&self, duration: Duration) -> io::Result<Vec<RxFrame>> {
        record_on(&self.sock, Some(self.remote()), duration)
    }

    /// [`Self::record`], but from any source. Over podman's port forwarding
    /// (the Mac's gvproxy) the node's transmissions arrive from a forwarder
    /// port, not from the address the bench sends to.
    ///
    /// # Errors
    /// Socket errors other than the read timeout.
    pub fn record_any(&self, duration: Duration) -> io::Result<Vec<RxFrame>> {
        record_on(&self.sock, None, duration)
    }

    /// [`Self::record_any`] on a background thread.
    ///
    /// # Errors
    /// Cloning the socket.
    pub fn record_any_in_background(
        &self,
        duration: Duration,
    ) -> io::Result<JoinHandle<io::Result<Vec<RxFrame>>>> {
        let me = self.try_clone()?;
        std::thread::Builder::new()
            .name("usrp-rx".into())
            .spawn(move || me.record_any(duration))
    }

    /// [`Self::transmit`] on a background thread.
    ///
    /// # Errors
    /// Cloning the socket.
    pub fn transmit_in_background(
        &self,
        audio: Vec<i16>,
    ) -> io::Result<JoinHandle<io::Result<TxReport>>> {
        let me = self.try_clone()?;
        std::thread::Builder::new()
            .name("usrp-tx".into())
            .spawn(move || me.transmit(&audio))
    }

    /// [`Self::record`] on a background thread.
    ///
    /// # Errors
    /// Cloning the socket.
    pub fn record_in_background(
        &self,
        duration: Duration,
    ) -> io::Result<JoinHandle<io::Result<Vec<RxFrame>>>> {
        let me = self.try_clone()?;
        std::thread::Builder::new()
            .name("usrp-rx".into())
            .spawn(move || me.record(duration))
    }

    /// A second handle on the same socket and sequence counter.
    ///
    /// # Errors
    /// Cloning the socket.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            sock: self.sock.try_clone()?,
            remote: Mutex::new(self.remote()),
            seq: Arc::clone(&self.seq),
        })
    }
}

fn record_on(
    sock: &UdpSocket,
    only_from: Option<SocketAddr>,
    duration: Duration,
) -> io::Result<Vec<RxFrame>> {
    let deadline = Instant::now() + duration;
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        sock.set_read_timeout(Some((deadline - now).max(Duration::from_millis(1))))?;
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let at = Instant::now();
                if only_from.is_some_and(|r| r != src) {
                    continue;
                }
                if let Some(p) = UsrpPacket::decode(&buf[..n])
                    && p.kind == UsrpType::Voice
                {
                    out.push(RxFrame {
                        at,
                        keyed: p.keyup,
                        seq: Some(p.seq),
                        ts_ms: None,
                        samples: p.audio,
                    });
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio;

    fn lo() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// Two probes on 127.0.0.1 pointed at each other.
    fn pair() -> (UsrpProbe, UsrpProbe) {
        let a = UsrpProbe::bind(lo(), lo()).unwrap();
        let b = UsrpProbe::bind(lo(), a.local_addr().unwrap()).unwrap();
        a.set_remote(b.local_addr().unwrap());
        (a, b)
    }

    #[test]
    fn header_layout_matches_chan_usrp() {
        let p = UsrpPacket::voice(0x0102_0304, vec![0x1234; VOICE_SAMPLES]);
        let b = p.encode();
        assert_eq!(b.len(), HEADER_LEN + 320);
        assert_eq!(&b[0..4], b"USRP");
        assert_eq!(&b[4..8], &[1, 2, 3, 4], "seq is htonl");
        assert_eq!(&b[12..16], &[0, 0, 0, 1], "keyup is htonl(1)");
        assert_eq!(&b[20..24], &[0, 0, 0, 0], "voice type 0");
        assert_eq!(&b[32..34], &[0x34, 0x12], "slin is little-endian");
        assert_eq!(UsrpPacket::decode(&b).unwrap(), p);
    }

    #[test]
    fn unkey_is_a_bare_header_with_keyup_zero() {
        let b = UsrpPacket::unkey(7).encode();
        assert_eq!(b.len(), HEADER_LEN);
        assert_eq!(&b[12..16], &[0, 0, 0, 0]);
        let p = UsrpPacket::decode(&b).unwrap();
        assert!(!p.keyup);
        assert_eq!(p.audio, [] as [i16; 0]);
        assert_eq!(p.seq, 7);
    }

    #[test]
    fn decodes_a_hand_built_chan_usrp_frame() {
        // Bytes laid out as chan_usrp's usrp_xwrite builds them.
        let mut b = Vec::new();
        b.extend_from_slice(b"USRP");
        b.extend_from_slice(&42u32.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&[0; 16]);
        for i in 0..160i16 {
            b.extend_from_slice(&(i * 100 - 8000).to_le_bytes());
        }
        let p = UsrpPacket::decode(&b).unwrap();
        assert_eq!(p.seq, 42);
        assert!(p.keyup);
        assert_eq!(p.kind, UsrpType::Voice);
        assert_eq!(p.audio[0], -8000);
        assert_eq!(p.audio[159], 159 * 100 - 8000);
    }

    #[test]
    fn text_type_is_host_order_and_either_order_decodes() {
        let mut p = UsrpPacket::unkey(1);
        p.kind = UsrpType::Text;
        p.payload = b"hi\0".to_vec();
        let b = p.encode();
        assert_eq!(&b[20..24], &[2, 0, 0, 0]);
        assert_eq!(UsrpPacket::decode(&b).unwrap().kind, UsrpType::Text);
        let mut be = b.clone();
        be[20..24].copy_from_slice(&2u32.to_be_bytes());
        let d = UsrpPacket::decode(&be).unwrap();
        assert_eq!(d.kind, UsrpType::Text);
        assert_eq!(d.payload, b"hi\0");
    }

    #[test]
    fn junk_is_rejected() {
        assert!(UsrpPacket::decode(b"USRP").is_none());
        assert!(UsrpPacket::decode(&[0u8; 352]).is_none());
    }

    #[test]
    fn loopback_tone_survives_with_pacing_sequence_and_unkey() {
        let (a, b) = pair();
        let rec = b.record_in_background(Duration::from_millis(1500)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t = audio::tone(1000.0, -10.0, Duration::from_millis(1000), SAMPLE_RATE);
        let tx = a.transmit(&t).unwrap();
        assert_eq!(tx.frames, 50);
        let elapsed = tx.finished - tx.started;
        assert!(
            elapsed >= Duration::from_millis(1000) && elapsed < Duration::from_millis(1300),
            "paced at 20 ms: {elapsed:?}"
        );

        let got = rec.join().unwrap().unwrap();
        let voice: Vec<_> = got.iter().filter(|f| !f.samples.is_empty()).collect();
        assert_eq!(voice.len(), 50);
        assert!(voice.iter().all(|f| f.keyed));
        let last = got.last().unwrap();
        assert!(!last.keyed && last.samples.is_empty(), "ends with unkey");

        let seqs: Vec<u32> = got.iter().filter_map(|f| f.seq).collect();
        let loss = audio::loss_from_sequence(&seqs);
        assert_eq!(loss.lost, 0);
        assert_eq!(loss.received, 51);

        // Arrival cadence is ~20 ms, with no stall worth reporting on loopback.
        let t0 = voice[0].at;
        let arrivals: Vec<Duration> = voice.iter().map(|f| f.at - t0).collect();
        assert_eq!(
            audio::arrival_gaps(&arrivals, Duration::from_millis(80)),
            [] as [(usize, std::time::Duration); 0]
        );

        let pcm = crate::probe::concat(&got);
        assert_eq!(pcm, t);
        let r = audio::detect_tone(&pcm, SAMPLE_RATE, None).unwrap();
        assert!((r.freq_hz - 1000.0).abs() < 2.0);
        assert!((r.level_dbfs + 10.0).abs() < 0.5);
    }

    #[test]
    fn sequence_continues_across_transmissions_and_clones() {
        let (a, b) = pair();
        let rec = b.record_in_background(Duration::from_millis(500)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        a.transmit(&[0; 320]).unwrap();
        a.transmit_in_background(vec![0; 160])
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
        let seqs: Vec<u32> = rec
            .join()
            .unwrap()
            .unwrap()
            .iter()
            .filter_map(|f| f.seq)
            .collect();
        // 2 voice + unkey, then 1 voice + unkey: one contiguous run from 1.
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn packets_from_strangers_are_ignored() {
        let (_a, b) = pair();
        let stranger = UdpSocket::bind(lo()).unwrap();
        let rec = b.record_in_background(Duration::from_millis(200)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        stranger
            .send_to(
                &UsrpPacket::voice(1, vec![1; 160]).encode(),
                b.local_addr().unwrap(),
            )
            .unwrap();
        assert_eq!(
            rec.join().unwrap().unwrap(),
            [] as [crate::probe::RxFrame; 0]
        );
    }
}
