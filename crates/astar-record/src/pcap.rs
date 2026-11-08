// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! A pcap writer and reader for the packet log.
//!
//! The file is classic pcap with nanosecond timestamps (magic `0xA1B23C4D`)
//! and link type 101 (raw IP), so each record starts at an IP header. Every
//! UDP datagram is wrapped in a synthetic IPv4 or IPv6 header plus a UDP
//! header with real checksums, which lets Wireshark dissect it directly.

// Every narrowing cast is range-checked first (packet length against the IP
// length field) or is the deliberate split of a timestamp into sec/nsec.
#![allow(clippy::cast_possible_truncation)]

use std::io::{self, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC_NS: u32 = 0xA1B2_3C4D;
const SNAPLEN: u32 = 65535;
const LINKTYPE_RAW: u32 = 101;
pub(crate) const GLOBAL_HEADER_LEN: usize = 24;
const RECORD_HEADER_LEN: usize = 16;

/// Map an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to plain IPv4.
#[must_use]
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        v4 @ SocketAddr::V4(_) => v4,
    }
}

/// Sum 16-bit big-endian words (odd tail padded with zero), not yet folded.
fn sum_words(data: &[u8], mut acc: u32) -> u32 {
    let (pairs, rest) = data.as_chunks::<2>();
    for c in pairs {
        acc += u32::from(u16::from_be_bytes(*c));
    }
    if let [last] = rest {
        acc += u32::from(*last) << 8;
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc > 0xFFFF {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    acc as u16
}

fn pseudo_sum(src: IpAddr, dst: IpAddr, udp_len: u32) -> u32 {
    let mut acc = 0u32;
    match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            acc = sum_words(&s.octets(), acc);
            acc = sum_words(&d.octets(), acc);
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            acc = sum_words(&s.octets(), acc);
            acc = sum_words(&d.octets(), acc);
        }
        _ => {}
    }
    acc += 17;
    acc += udp_len >> 16;
    acc += udp_len & 0xFFFF;
    acc
}

/// UDP checksum over pseudo-header + `udp` (header with zero checksum + payload).
fn udp_checksum(src: IpAddr, dst: IpAddr, udp: &[u8]) -> u16 {
    let acc = sum_words(udp, pseudo_sum(src, dst, udp.len() as u32));
    let c = !fold(acc);
    if c == 0 { 0xFFFF } else { c }
}

/// Writes UDP datagrams as a nanosecond raw-IP pcap stream.
pub struct PcapWriter<W: Write> {
    out: W,
    scratch: Vec<u8>,
}

impl<W: Write> PcapWriter<W> {
    /// Write the global header and return the writer.
    pub fn new(mut out: W) -> io::Result<Self> {
        let mut h = Vec::with_capacity(GLOBAL_HEADER_LEN);
        h.extend_from_slice(&MAGIC_NS.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&4u16.to_le_bytes());
        h.extend_from_slice(&0i32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&SNAPLEN.to_le_bytes());
        h.extend_from_slice(&LINKTYPE_RAW.to_le_bytes());
        out.write_all(&h)?;
        Ok(Self {
            out,
            scratch: Vec::new(),
        })
    }

    /// Write one datagram; returns the bytes written to the stream.
    pub fn write_packet(
        &mut self,
        ts_ns: u64,
        src: SocketAddr,
        dst: SocketAddr,
        payload: &[u8],
    ) -> io::Result<u64> {
        let src = canonical(src);
        let dst = canonical(dst);
        let udp_len = 8 + payload.len();
        let bad = |m: &str| io::Error::new(ErrorKind::InvalidInput, m.to_string());
        let pkt = &mut self.scratch;
        pkt.clear();
        match (src.ip(), dst.ip()) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                let total = 20 + udp_len;
                if total > usize::from(u16::MAX) {
                    return Err(bad("datagram too large for IPv4"));
                }
                pkt.extend_from_slice(&[0x45, 0]);
                pkt.extend_from_slice(&(total as u16).to_be_bytes());
                pkt.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]);
                pkt.extend_from_slice(&s.octets());
                pkt.extend_from_slice(&d.octets());
                let ck = !fold(sum_words(&pkt[..20], 0));
                pkt[10..12].copy_from_slice(&ck.to_be_bytes());
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                if udp_len > usize::from(u16::MAX) {
                    return Err(bad("datagram too large for IPv6"));
                }
                pkt.extend_from_slice(&[0x60, 0, 0, 0]);
                pkt.extend_from_slice(&(udp_len as u16).to_be_bytes());
                pkt.extend_from_slice(&[17, 64]);
                pkt.extend_from_slice(&s.octets());
                pkt.extend_from_slice(&d.octets());
            }
            _ => return Err(bad("source and destination address families differ")),
        }
        let udp_at = pkt.len();
        pkt.extend_from_slice(&src.port().to_be_bytes());
        pkt.extend_from_slice(&dst.port().to_be_bytes());
        pkt.extend_from_slice(&(udp_len as u16).to_be_bytes());
        pkt.extend_from_slice(&[0, 0]);
        pkt.extend_from_slice(payload);
        let ck = udp_checksum(src.ip(), dst.ip(), &pkt[udp_at..]);
        pkt[udp_at + 6..udp_at + 8].copy_from_slice(&ck.to_be_bytes());

        let mut rec = [0u8; RECORD_HEADER_LEN];
        rec[0..4].copy_from_slice(&((ts_ns / 1_000_000_000) as u32).to_le_bytes());
        rec[4..8].copy_from_slice(&((ts_ns % 1_000_000_000) as u32).to_le_bytes());
        rec[8..12].copy_from_slice(&(pkt.len() as u32).to_le_bytes());
        rec[12..16].copy_from_slice(&(pkt.len() as u32).to_le_bytes());
        self.out.write_all(&rec)?;
        self.out.write_all(pkt)?;
        Ok((RECORD_HEADER_LEN + pkt.len()) as u64)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// One packet read back from a pcap file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcapRecord {
    pub ts_ns: u64,
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub payload: Vec<u8>,
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Parse a whole pcap file written by [`PcapWriter`], validating the magic,
/// link type, every length and every checksum.
pub fn read_pcap(bytes: &[u8]) -> Result<Vec<PcapRecord>, String> {
    if bytes.len() < GLOBAL_HEADER_LEN {
        return Err("shorter than the pcap global header".into());
    }
    if le32(&bytes[0..4]) != MAGIC_NS {
        return Err("bad pcap magic".into());
    }
    if bytes[4..6] != 2u16.to_le_bytes() || bytes[6..8] != 4u16.to_le_bytes() {
        return Err("unsupported pcap version".into());
    }
    if le32(&bytes[20..24]) != LINKTYPE_RAW {
        return Err("link type is not raw IP (101)".into());
    }
    let mut out = Vec::new();
    let mut pos = GLOBAL_HEADER_LEN;
    while pos < bytes.len() {
        let hdr = bytes
            .get(pos..pos + RECORD_HEADER_LEN)
            .ok_or("truncated record header")?;
        let (sec, nsec) = (le32(&hdr[0..4]), le32(&hdr[4..8]));
        let (incl, orig) = (le32(&hdr[8..12]) as usize, le32(&hdr[12..16]));
        if incl as u32 != orig {
            return Err("incl_len and orig_len differ".into());
        }
        if nsec >= 1_000_000_000 {
            return Err("nanosecond field out of range".into());
        }
        pos += RECORD_HEADER_LEN;
        let pkt = bytes.get(pos..pos + incl).ok_or("truncated record body")?;
        pos += incl;
        let ts_ns = u64::from(sec) * 1_000_000_000 + u64::from(nsec);
        out.push(parse_packet(ts_ns, pkt)?);
    }
    Ok(out)
}

fn parse_packet(ts_ns: u64, pkt: &[u8]) -> Result<PcapRecord, String> {
    let first = *pkt.first().ok_or("empty packet")?;
    let (src, dst, udp) = match first >> 4 {
        4 => {
            if pkt.len() < 20 || first != 0x45 {
                return Err("bad IPv4 header".into());
            }
            if usize::from(u16::from_be_bytes([pkt[2], pkt[3]])) != pkt.len() {
                return Err("IPv4 total length mismatch".into());
            }
            if pkt[9] != 17 {
                return Err("IPv4 protocol is not UDP".into());
            }
            if fold(sum_words(&pkt[..20], 0)) != 0xFFFF {
                return Err("IPv4 header checksum fails".into());
            }
            let s = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
            let d = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
            (IpAddr::V4(s), IpAddr::V4(d), &pkt[20..])
        }
        6 => {
            if pkt.len() < 40 {
                return Err("bad IPv6 header".into());
            }
            if usize::from(u16::from_be_bytes([pkt[4], pkt[5]])) != pkt.len() - 40 {
                return Err("IPv6 payload length mismatch".into());
            }
            if pkt[6] != 17 {
                return Err("IPv6 next header is not UDP".into());
            }
            let a: [u8; 16] = pkt[8..24].try_into().expect("16 bytes");
            let b: [u8; 16] = pkt[24..40].try_into().expect("16 bytes");
            (
                IpAddr::V6(Ipv6Addr::from(a)),
                IpAddr::V6(Ipv6Addr::from(b)),
                &pkt[40..],
            )
        }
        _ => return Err("not an IP packet".into()),
    };
    if udp.len() < 8 {
        return Err("truncated UDP header".into());
    }
    if usize::from(u16::from_be_bytes([udp[4], udp[5]])) != udp.len() {
        return Err("UDP length mismatch".into());
    }
    let stored = u16::from_be_bytes([udp[6], udp[7]]);
    let mut zeroed = udp.to_vec();
    zeroed[6] = 0;
    zeroed[7] = 0;
    if stored != udp_checksum(src, dst, &zeroed) {
        return Err("UDP checksum fails".into());
    }
    let sp = u16::from_be_bytes([udp[0], udp[1]]);
    let dp = u16::from_be_bytes([udp[2], udp[3]]);
    Ok(PcapRecord {
        ts_ns,
        src: SocketAddr::new(src, sp),
        dst: SocketAddr::new(dst, dp),
        payload: udp[8..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn write_all(pkts: &[(u64, &str, &str, &[u8])]) -> Vec<u8> {
        let mut w = PcapWriter::new(Vec::new()).unwrap();
        for (t, s, d, p) in pkts {
            w.write_packet(*t, a(s), a(d), p).unwrap();
        }
        w.flush().unwrap();
        w.into_inner()
    }

    #[test]
    fn round_trip_keeps_count_order_timestamps_and_addresses() {
        let pkts: Vec<(u64, &str, &str, &[u8])> = vec![
            (
                1_700_000_000_000_000_001,
                "192.0.2.1:4569",
                "192.0.2.2:4569",
                b"one",
            ),
            (
                1_700_000_000_999_999_999,
                "192.0.2.2:4569",
                "192.0.2.1:4569",
                b"two!",
            ),
            (
                1_700_000_001_000_000_000,
                "198.51.100.1:1",
                "198.51.100.2:65535",
                &[0xFF; 301],
            ),
            (
                1_700_000_002_123_456_789,
                "[2001:db8::1]:5198",
                "[2001:db8::2]:5199",
                b"six",
            ),
            (
                1_700_000_003_000_000_007,
                "[2001:db8::2]:5199",
                "[2001:db8::1]:5198",
                &[7; 1000],
            ),
        ];
        let got = read_pcap(&write_all(&pkts)).unwrap();
        assert_eq!(got.len(), 5);
        for (g, (t, s, d, p)) in got.iter().zip(&pkts) {
            assert_eq!(g.ts_ns, *t);
            assert_eq!(g.src, a(s));
            assert_eq!(g.dst, a(d));
            assert_eq!(g.payload, *p);
        }
    }

    #[test]
    fn the_global_header_is_nanosecond_raw_ip() {
        let f = write_all(&[]);
        assert_eq!(f.len(), 24);
        assert_eq!(&f[0..4], &[0x4D, 0x3C, 0xB2, 0xA1]);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 2);
        assert_eq!(u16::from_le_bytes([f[6], f[7]]), 4);
        assert_eq!(le32(&f[16..20]), 65535);
        assert_eq!(le32(&f[20..24]), 101);
    }

    #[test]
    fn ipv4_and_udp_checksums_verify() {
        let f = write_all(&[(5, "192.0.2.1:4569", "192.0.2.2:4569", b"odd")]);
        let pkt = &f[24 + 16..];
        assert_eq!(fold(sum_words(&pkt[..20], 0)), 0xFFFF);
        let acc = sum_words(
            &pkt[20..],
            pseudo_sum(a("192.0.2.1:1").ip(), a("192.0.2.2:1").ip(), 11),
        );
        assert_eq!(fold(acc), 0xFFFF);
        // A flipped payload bit must be caught by the reader.
        let mut bad = f.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(read_pcap(&bad).is_err());
    }

    #[test]
    fn ipv6_udp_checksum_is_never_zero() {
        // A normal packet carries a non-zero checksum that verifies.
        let f = write_all(&[(5, "[2001:db8::1]:1", "[2001:db8::2]:2", b"x")]);
        let pkt = &f[24 + 16..];
        assert_ne!(u16::from_be_bytes([pkt[46], pkt[47]]), 0);
        let acc = sum_words(
            &pkt[40..],
            pseudo_sum(a("[2001:db8::1]:1").ip(), a("[2001:db8::2]:2").ip(), 9),
        );
        assert_eq!(fold(acc), 0xFFFF);
        // Search two-byte payloads for one whose true checksum is zero: it
        // has to go out as 0xFFFF, because zero means no checksum.
        let (s, d) = (a("[2001:db8::1]:1").ip(), a("[2001:db8::2]:2").ip());
        for n in 0..=u16::MAX {
            let mut udp = vec![0, 1, 0, 2, 0, 10, 0, 0];
            udp.extend_from_slice(&n.to_be_bytes());
            let raw = !fold(sum_words(&udp, pseudo_sum(s, d, 10)));
            if raw == 0 {
                assert_eq!(udp_checksum(s, d, &udp), 0xFFFF);
                return;
            }
        }
        panic!("no payload produced a zero checksum");
    }

    #[test]
    fn a_v4_mapped_source_is_written_as_plain_ipv4() {
        let f = write_all(&[(1, "[::ffff:203.0.113.7]:5198", "192.0.2.1:5198", b"hi")]);
        assert_eq!(f[24 + 16] >> 4, 4);
        let got = read_pcap(&f).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].src, a("203.0.113.7:5198"));
    }

    #[test]
    fn mixed_families_are_refused_not_mangled() {
        let mut w = PcapWriter::new(Vec::new()).unwrap();
        let e = w
            .write_packet(1, a("[2001:db8::1]:1"), a("192.0.2.1:1"), b"x")
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidInput);
        assert_eq!(w.into_inner().len(), 24);
    }

    #[test]
    fn an_empty_payload_is_a_valid_packet() {
        let got = read_pcap(&write_all(&[(9, "192.0.2.1:1", "192.0.2.2:2", b"")])).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, Vec::<u8>::new());
    }

    #[test]
    fn a_truncated_last_record_is_reported() {
        let f = write_all(&[
            (1, "192.0.2.1:1", "192.0.2.2:2", b"abcdef"),
            (2, "192.0.2.1:1", "192.0.2.2:2", b"ghijkl"),
        ]);
        assert!(read_pcap(&f[..f.len() - 3]).is_err());
        assert!(read_pcap(&f[..24 + 16 + 34 + 5]).is_err());
    }

    #[test]
    fn tshark_reads_what_we_write() {
        if std::process::Command::new("tshark")
            .arg("-v")
            .output()
            .is_err()
        {
            return;
        }
        // IAX2 mini frame (F bit clear) and full frame (F bit set).
        let mini = [0x12, 0x34, 0x00, 0x01, 0xAA, 0xBB];
        let mut full = vec![0u8; 12];
        full[0] = 0x80 | 0x12;
        full[1] = 0x34;
        let t1 = 1_700_000_000_123_456_789u64;
        let t2 = 1_700_000_001_000_000_042u64;
        let f = write_all(&[
            (t1, "192.0.2.1:4569", "192.0.2.2:4569", &mini),
            (t2, "192.0.2.1:4569", "192.0.2.2:4569", &full),
        ]);
        let dir = std::env::temp_dir().join(format!("astar-pcap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.pcap");
        std::fs::write(&path, &f).unwrap();
        let out = std::process::Command::new("tshark")
            .args(["-r"])
            .arg(&path)
            .args([
                "-T",
                "fields",
                "-e",
                "_ws.col.Protocol",
                "-e",
                "frame.time_epoch",
            ])
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "tshark said: {text} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(lines[0], "IAX2\t1700000000.123456789");
        assert_eq!(lines[1], "IAX2\t1700000001.000000042");
    }
}
