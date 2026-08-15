// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Emit a synthetic libpcap fixture exercising the replay harness.
//!
//! Generates `fixtures/synthetic.pcap` containing a small sequence of
//! Ethernet/IPv4/UDP packets carrying IAX2 frames built by
//! `astar_iax_core::encode`. The output is committed alongside the
//! fixtures README so the replay test has something to chew on before
//! real captures land.
//!
//! Re-run with:
//! ```text
//! cargo run -p astar-conformance --example gen_synthetic_pcap
//! ```

#![allow(clippy::too_many_lines, clippy::similar_names)]

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use astar_iax_core::frame::{FullFrame, MiniFrame};
use astar_iax_core::ie::Ies;
use astar_iax_core::{
    ControlSubclass, Frame, FrameType, IaxCommand, Subclass, VoiceFormat, encode,
};

/// `DLT_EN10MB`
const LINKTYPE_ETHERNET: u32 = 1;

fn main() -> std::io::Result<()> {
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("synthetic.pcap");
    let mut file = File::create(&out_path)?;

    write_pcap_header(&mut file)?;

    // Synthetic conversation:
    //
    //  1. client -> server  IAX/NEW with USERNAME + empty CALLTOKEN
    //  2. server -> client  IAX/AUTHREQ with CHALLENGE + AUTHMETHODS=MD5
    //  3. client -> server  IAX/AUTHREP with MD5_RESULT
    //  4. server -> client  IAX/ACCEPT with FORMAT=ULAW
    //  5. server -> client  CONTROL/RINGING
    //  6. client -> server  voice mini frame (ULAW, 8 bytes)
    //  7. client -> server  voice mini frame (ULAW, 8 bytes)
    //
    // Each captured packet uses fixed addresses so the replay test sees
    // realistic SocketAddrs.

    let client: [u8; 4] = [192, 0, 2, 10];
    let server: [u8; 4] = [192, 0, 2, 20];
    let client_port = 32_768u16;
    let server_port = 4_569u16;

    let mut t = 0u32;
    let mut tick = |delta: u32| {
        t += delta;
        t
    };

    let new = full(
        1,
        0,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        Ies {
            username: Some("rob"),
            calltoken: Some(&[]),
            ..Ies::empty()
        },
    );
    write_packet(
        &mut file,
        tick(0),
        client,
        client_port,
        server,
        server_port,
        &new,
    )?;

    let challenge = "12345678";
    let authreq = full(
        5,
        1,
        20,
        0,
        1,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::AuthReq),
        Ies {
            authmethods: Some(2),
            challenge: Some(challenge),
            ..Ies::empty()
        },
    );
    write_packet(
        &mut file,
        tick(20_000),
        server,
        server_port,
        client,
        client_port,
        &authreq,
    )?;

    let md5_result = "00112233445566778899aabbccddeeff";
    let authrep = full(
        1,
        5,
        40,
        1,
        2,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::AuthRep),
        Ies {
            md5_result: Some(md5_result),
            ..Ies::empty()
        },
    );
    write_packet(
        &mut file,
        tick(20_000),
        client,
        client_port,
        server,
        server_port,
        &authrep,
    )?;

    let accept = full(
        5,
        1,
        60,
        2,
        3,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Accept),
        Ies {
            format: Some(VoiceFormat::G711U.as_u32()),
            ..Ies::empty()
        },
    );
    write_packet(
        &mut file,
        tick(20_000),
        server,
        server_port,
        client,
        client_port,
        &accept,
    )?;

    let ringing = full(
        5,
        1,
        80,
        3,
        3,
        FrameType::Control,
        Subclass::Control(ControlSubclass::Ringing),
        Ies::empty(),
    );
    write_packet(
        &mut file,
        tick(20_000),
        server,
        server_port,
        client,
        client_port,
        &ringing,
    )?;

    let mini_payload_a = [0xffu8; 8];
    let mini_a = mini(1, 100, &mini_payload_a);
    write_packet(
        &mut file,
        tick(20_000),
        client,
        client_port,
        server,
        server_port,
        &mini_a,
    )?;

    let mini_payload_b = [0x7fu8; 8];
    let mini_b = mini(1, 120, &mini_payload_b);
    write_packet(
        &mut file,
        tick(20_000),
        client,
        client_port,
        server,
        server_port,
        &mini_b,
    )?;

    eprintln!("wrote {}", out_path.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn full(
    source_call: u16,
    dest_call: u16,
    timestamp: u32,
    oseqno: u8,
    iseqno: u8,
    frame_type: FrameType,
    subclass: Subclass,
    ies: Ies<'_>,
) -> Vec<u8> {
    let f = FullFrame {
        source_call,
        dest_call,
        retransmission: false,
        timestamp,
        oseqno,
        iseqno,
        frame_type,
        subclass,
        ies,
        payload: &[],
    };
    let frame = Frame::Full(Box::new(f));
    let mut out = Vec::new();
    encode(&frame, &mut out).expect("test frame must encode");
    out
}

fn mini(source_call: u16, timestamp: u16, payload: &[u8]) -> Vec<u8> {
    let m = MiniFrame {
        source_call,
        timestamp,
        payload,
    };
    let frame = Frame::Mini(m);
    let mut out = Vec::new();
    encode(&frame, &mut out).expect("test frame must encode");
    out
}

// --- pcap file writers --------------------------------------------------

fn write_pcap_header(out: &mut impl Write) -> std::io::Result<()> {
    // Classic libpcap, little-endian, microsecond timestamps. Layout per
    // pcap-savefile(5):
    //   magic      u32 = 0xa1b2c3d4
    //   version    u16 major=2, u16 minor=4
    //   thiszone   i32 = 0
    //   sigfigs    u32 = 0
    //   snaplen    u32 = 65535
    //   network    u32 = LINKTYPE_ETHERNET
    let mut hdr = Vec::with_capacity(24);
    hdr.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    hdr.extend_from_slice(&2u16.to_le_bytes());
    hdr.extend_from_slice(&4u16.to_le_bytes());
    hdr.extend_from_slice(&0i32.to_le_bytes());
    hdr.extend_from_slice(&0u32.to_le_bytes());
    hdr.extend_from_slice(&65_535u32.to_le_bytes());
    hdr.extend_from_slice(&LINKTYPE_ETHERNET.to_le_bytes());
    out.write_all(&hdr)
}

#[allow(clippy::too_many_arguments)]
fn write_packet(
    out: &mut impl Write,
    ts_us_total: u32,
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    iax_payload: &[u8],
) -> std::io::Result<()> {
    let ts_sec = ts_us_total / 1_000_000;
    let ts_usec = ts_us_total % 1_000_000;

    let frame = build_ethernet_ipv4_udp(src_ip, src_port, dst_ip, dst_port, iax_payload);

    // Per-packet header: ts_sec, ts_usec, incl_len, orig_len.
    let mut hdr = Vec::with_capacity(16);
    hdr.extend_from_slice(&ts_sec.to_le_bytes());
    hdr.extend_from_slice(&ts_usec.to_le_bytes());
    let len = u32::try_from(frame.len()).expect("synthetic frames fit in u32");
    hdr.extend_from_slice(&len.to_le_bytes());
    hdr.extend_from_slice(&len.to_le_bytes());
    out.write_all(&hdr)?;
    out.write_all(&frame)
}

fn build_ethernet_ipv4_udp(
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14 + 20 + 8 + payload.len());

    // Ethernet header: dst MAC, src MAC, ethertype=IPv4.
    buf.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
    buf.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    buf.extend_from_slice(&0x0800u16.to_be_bytes());

    // IPv4 header (no options, header len = 20).
    let udp_len = u16::try_from(8 + payload.len()).expect("synthetic payload < 64 KiB");
    let total_len = 20 + udp_len;
    let ip_start = buf.len();
    buf.push(0x45); // version=4, IHL=5
    buf.push(0x00); // DSCP/ECN
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&0x0000u16.to_be_bytes()); // id
    buf.extend_from_slice(&0x4000u16.to_be_bytes()); // DF, no offset
    buf.push(64); // TTL
    buf.push(17); // proto = UDP
    let csum_pos = buf.len();
    buf.extend_from_slice(&[0u8, 0u8]); // checksum placeholder
    buf.extend_from_slice(&src_ip);
    buf.extend_from_slice(&dst_ip);
    let csum = ipv4_checksum(&buf[ip_start..]);
    let [hi, lo] = csum.to_be_bytes();
    buf[csum_pos] = hi;
    buf[csum_pos + 1] = lo;

    // UDP header. Checksum=0 is legal for IPv4 UDP and saves us pseudo-
    // header math; pcap-parser doesn't validate it.
    buf.extend_from_slice(&src_port.to_be_bytes());
    buf.extend_from_slice(&dst_port.to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&[0u8, 0u8]);
    buf.extend_from_slice(payload);

    buf
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < header.len() {
        sum += u32::from(u16::from_be_bytes([header[i], header[i + 1]]));
        i += 2;
    }
    if i < header.len() {
        sum += u32::from(header[i]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum & 0xffff).unwrap_or(0)
}
