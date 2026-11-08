// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A raw far talker on the in-process D-Star reflector.

// Each test binary compiles its own copy and uses only part of it.
#![allow(dead_code)]

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use astar_codec::ambe::{AmbeBackend, open_ambe};
use astar_codec::vocoder_trust::VocoderTrust;
use astar_dstar::{DsvtPacket, ReflectorHandle, RfHeader};
use astar_netrec::config::{NetCfg, Target};

use crate::common::{net, tone_frame, wait_for};

/// D-Star voice frames run seq 0 to 20 and wrap.
const SUPERFRAME: u16 = 21;

pub fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback")
}

pub fn dstar_net(addr: SocketAddr) -> NetCfg {
    net(Target::Dstar {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        module: 'A',
        reflector: Some("XRF757".to_string()),
    })
}

/// Wait for the session under test to be the module's one client.
pub fn recorder_linked(handle: &ReflectorHandle) {
    assert!(
        wait_for(|| handle.client_count() == 1, Duration::from_millis(1_500)),
        "the recorder linked"
    );
}

fn field8(callsign: &str) -> [u8; 8] {
    let mut buf = [b' '; 8];
    buf[..callsign.len()].copy_from_slice(callsign.as_bytes());
    buf
}

/// A linked raw socket. The reflector relays what it sends to every other
/// client on the module, which is how the recorder hears it.
pub struct Talker {
    sock: UdpSocket,
    reflector: SocketAddr,
}

impl Talker {
    pub fn link(reflector: SocketAddr, callsign: &str) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw talker");
        sock.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout");
        let mut request = field8(callsign).to_vec();
        request.extend_from_slice(&[b' ', b'A', 0x00]);
        sock.send_to(&request, reflector)
            .expect("send link request");
        let mut buf = [0u8; 128];
        let (n, _) = sock.recv_from(&mut buf).expect("link ACK");
        assert!(buf[..n].ends_with(b"ACK\0"), "the talker linked");
        Self { sock, reflector }
    }

    fn send(&self, packet: &DsvtPacket) {
        self.sock
            .send_to(&packet.encode(), self.reflector)
            .expect("send");
    }

    /// Talk as `callsign` for `secs` seconds of a 1 kHz tone, encoded by the
    /// software vocoder and paced to real time against the clock.
    pub fn talk(&self, stream_id: u16, callsign: &str, secs: u16) {
        let (mut vocoder, backend) = open_ambe(Some(AmbeBackend::Software), &VocoderTrust::none())
            .expect("the software vocoder");
        assert_eq!(backend, AmbeBackend::Software);
        self.send(&DsvtPacket::Header {
            stream_id,
            header: RfHeader {
                flags: [0x00, 0x00, 0x00],
                rpt2: *b"XRF757 G",
                rpt1: *b"XRF757 A",
                ur: *b"CQCQCQ  ",
                my: field8(callsign),
                suffix: *b"    ",
            },
        });
        let frames = secs * 50;
        let began = Instant::now();
        let mut n = 0u32;
        for k in 0..frames {
            self.send(&DsvtPacket::Voice {
                stream_id,
                seq: u8::try_from(k % SUPERFRAME).expect("a superframe index"),
                end: k + 1 == frames,
                ambe: vocoder.encode(&tone_frame(&mut n)),
                slow_data: [0x00, 0x00, 0x00],
            });
            let due = began + Duration::from_millis(20 * (u64::from(k) + 1));
            thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    }
}
