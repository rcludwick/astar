// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! pcap/pcapng replay harness for IAX2 (UDP/4569).
//!
//! A [`ReplayFixture`] is a list of [`RecordedFrame`]s extracted from a
//! capture file. Each recorded frame carries the captured UDP payload (the
//! IAX2 frame bytes), the source and destination socket addresses, and a
//! timestamp relative to the first packet in the capture.
//!
//! [`play`] iterates the fixture and asserts the round-trip invariant
//! `encode(parse(bytes)) == bytes` for every payload. The byte-equality
//! check is the load-bearing assertion: it pins our parser/encoder to be
//! interoperable with whatever upstream Asterisk wrote on the wire.
//!
//! # Supported captures
//!
//! - Both classic libpcap (`.pcap`) and pcapng (`.pcapng`) containers, via
//!   `pcap-parser`'s `create_reader`.
//! - Link-layer types: Ethernet (`DLT_EN10MB`), raw IP (`DLT_RAW`,
//!   `DLT_IPV4`, `DLT_IPV6`), BSD loopback (`DLT_NULL`), and OpenBSD
//!   loopback (`DLT_LOOP`). Other link types are skipped with a warning.
//! - Network layers: IPv4 and IPv6. IPv6 extension headers are not
//!   followed — UDP must be the immediate next-header. Captures that
//!   exercise extension headers will produce zero parsed frames.
//! - Transport: UDP only.
//! - Fragmentation: IPv4 fragments (MF set or non-zero offset) are skipped
//!   with a warning. IAX2 frames fit comfortably under typical MTUs, so
//!   real captures should not hit this in practice.

use std::fs::File;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use astar_iax_core::{Frame, ParseError, encode, parse};
use pcap_parser::pcapng::Block;
use pcap_parser::{Linktype, PcapBlockOwned, PcapError, create_reader};

/// IAX2 well-known UDP port (`IAX_DEFAULT_PORTNO` in `iax2.h`).
pub const IAX2_PORT: u16 = 4569;

/// A single UDP datagram extracted from a capture, carrying an IAX2 frame
/// in `bytes`.
#[derive(Debug, Clone)]
pub struct RecordedFrame {
    /// Capture timestamp relative to the first packet in the fixture.
    /// `Duration::ZERO` for the first frame.
    pub timestamp: Duration,
    /// Source `IP:port` from the IP + UDP headers.
    pub src_addr: SocketAddr,
    /// Destination `IP:port` from the IP + UDP headers.
    pub dst_addr: SocketAddr,
    /// UDP payload (the raw IAX2 frame bytes).
    pub bytes: Vec<u8>,
}

/// All IAX2 datagrams from a single capture file, in capture order.
#[derive(Debug, Clone)]
pub struct ReplayFixture {
    /// Display name (file stem) for diagnostic output.
    pub name: String,
    /// Optional companion `.md` file path describing the scenario; loaded
    /// only to confirm presence — full assertion-matching is future work.
    pub companion_md: Option<PathBuf>,
    /// Extracted IAX2 datagrams in capture order.
    pub frames: Vec<RecordedFrame>,
}

/// Errors raised while loading or replaying a fixture.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("I/O error reading capture: {0}")]
    Io(#[from] std::io::Error),
    #[error("pcap-parser failed: {0}")]
    Pcap(String),
    #[error("frame #{index} ({len} bytes from {src} -> {dst}) failed to parse: {source}")]
    Parse {
        index: usize,
        len: usize,
        src: SocketAddr,
        dst: SocketAddr,
        #[source]
        source: ParseError,
    },
    #[error(
        "frame #{index} ({src} -> {dst}) did not round-trip: original {orig_len} B, re-encoded {reenc_len} B"
    )]
    RoundTrip {
        index: usize,
        src: SocketAddr,
        dst: SocketAddr,
        orig_len: usize,
        reenc_len: usize,
    },
}

/// Which invariants `play` should enforce.
#[derive(Debug, Clone)]
pub enum ReplayAssertion {
    /// Every frame must parse without error and re-encode to the original
    /// bytes.
    ParseRoundTrip,
    /// Every frame must parse without error. No round-trip assertion.
    /// Use for fixtures captured from real peers that may carry
    /// vendor-extension IEs or subclasses our encoder does not model.
    ParseOnly,
    /// Reserved for future typed-pattern matching against a `.md`
    /// companion. Not yet implemented; treated as [`Self::ParseRoundTrip`]
    /// today.
    Match,
}

/// Per-fixture summary of what `play` saw.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayStats {
    /// Number of recorded UDP/4569 datagrams in the fixture.
    pub frames_total: usize,
    /// Number of frames that parsed cleanly and round-tripped.
    pub frames_parsed: usize,
    /// Subset of `frames_parsed` that were mini frames (high bit of byte
    /// 0 clear).
    pub mini_frames: usize,
    /// Subset of `frames_parsed` that were full frames.
    pub full_frames: usize,
}

impl ReplayFixture {
    /// Load every UDP/4569 datagram from a `.pcap` or `.pcapng` file.
    ///
    /// Non-UDP, non-IPv4/v6, fragmented, and non-port-4569 packets are
    /// silently filtered out (with a warning via `tracing` for the cases
    /// we explicitly skip).
    pub fn from_pcap(path: impl AsRef<Path>) -> Result<Self, ReplayError> {
        let path = path.as_ref();
        let name = path.file_stem().map_or_else(
            || path.display().to_string(),
            |s| s.to_string_lossy().into_owned(),
        );

        let companion_md = {
            let candidate = path.with_extension("md");
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        };

        let file = File::open(path)?;
        let mut reader = create_reader(65_536, file).map_err(format_pcap_err)?;

        // Per-interface link types (pcapng can declare many). For legacy
        // pcap there is exactly one interface; we record it under index 0.
        let mut link_types: Vec<Linktype> = Vec::new();
        let mut frames: Vec<RecordedFrame> = Vec::new();
        // Set on the first captured packet; subsequent timestamps are
        // recorded relative to it.
        let mut t0: Option<Duration> = None;
        // Resolution defaults: pcap legacy is microseconds; pcapng IDB
        // default is microseconds (if_tsresol == 6).
        let mut tsresols: Vec<u8> = Vec::new();

        loop {
            match reader.next() {
                Ok((offset, block)) => {
                    handle_block(&block, &mut link_types, &mut tsresols, &mut frames, &mut t0);
                    reader.consume(offset);
                }
                Err(PcapError::Eof) => break,
                Err(PcapError::Incomplete(_)) => {
                    reader.refill().map_err(format_pcap_err)?;
                }
                Err(e) => return Err(format_pcap_err(e)),
            }
        }

        Ok(Self {
            name,
            companion_md,
            frames,
        })
    }
}

fn format_pcap_err<E: std::fmt::Debug>(e: E) -> ReplayError {
    ReplayError::Pcap(format!("{e:?}"))
}

fn handle_block(
    block: &PcapBlockOwned<'_>,
    link_types: &mut Vec<Linktype>,
    tsresols: &mut Vec<u8>,
    frames: &mut Vec<RecordedFrame>,
    t0: &mut Option<Duration>,
) {
    match block {
        // Classic pcap: header carries the single linktype.
        PcapBlockOwned::LegacyHeader(hdr) => {
            link_types.clear();
            tsresols.clear();
            link_types.push(hdr.network);
            // Legacy pcap timestamps are microseconds (resolution 10^-6).
            tsresols.push(6);
        }
        PcapBlockOwned::Legacy(pkt) => {
            let Some(&linktype) = link_types.first() else {
                return;
            };
            // Legacy timestamps: ts_sec + ts_usec.
            let captured = Duration::new(u64::from(pkt.ts_sec), pkt.ts_usec.saturating_mul(1000));
            ingest_packet(linktype, pkt.data, captured, frames, t0);
        }
        PcapBlockOwned::NG(Block::SectionHeader(_)) => {
            // New section: clear per-section state (interfaces are
            // section-scoped per the pcapng spec).
            link_types.clear();
            tsresols.clear();
        }
        PcapBlockOwned::NG(Block::InterfaceDescription(idb)) => {
            link_types.push(idb.linktype);
            // if_tsresol is 6 (microseconds) by default in pcapng.
            let tsresol = if idb.if_tsresol == 0 {
                6
            } else {
                idb.if_tsresol
            };
            tsresols.push(tsresol);
        }
        PcapBlockOwned::NG(Block::EnhancedPacket(epb)) => {
            let if_id = epb.if_id as usize;
            let Some(&linktype) = link_types.get(if_id) else {
                tracing::warn!(if_id, "EPB references unknown interface; skipping");
                return;
            };
            let tsresol = tsresols.get(if_id).copied().unwrap_or(6);
            let captured = pcapng_ts(epb.ts_high, epb.ts_low, tsresol);
            ingest_packet(linktype, epb.data, captured, frames, t0);
        }
        PcapBlockOwned::NG(Block::SimplePacket(spb)) => {
            // SPB inherits the first interface's linktype and has no
            // timestamp of its own.
            let Some(&linktype) = link_types.first() else {
                return;
            };
            let captured = t0.unwrap_or(Duration::ZERO);
            ingest_packet(linktype, spb.data, captured, frames, t0);
        }
        // Name resolution, statistics, custom blocks, etc. — nothing to
        // do.
        PcapBlockOwned::NG(_) => {}
    }
}

/// Decode a pcapng EPB timestamp (`ts_high:ts_low`, interpreted in
/// `tsresol` units) into a `Duration` since the Unix epoch.
fn pcapng_ts(ts_high: u32, ts_low: u32, tsresol: u8) -> Duration {
    let raw = (u64::from(ts_high) << 32) | u64::from(ts_low);
    // tsresol < 128 means "negative power of ten"; >= 128 means
    // "negative power of two" — we use the bit-7 convention from the
    // pcapng spec.
    if tsresol & 0x80 == 0 {
        // 10^-tsresol seconds.
        let exp = u32::from(tsresol.min(18));
        let denom = 10u64.pow(exp);
        let secs = raw / denom;
        let frac = raw % denom;
        // Scale fractional portion to nanoseconds. `frac < denom == 10^exp`,
        // so the scaled value is always `< 10^9` and fits a u32 — the
        // `try_from` cannot truncate. L3 (iax-f755): assert that invariant
        // loudly instead of silently clamping to `u32::MAX`, which would
        // mask a bug (e.g. a future change to `frac`/`exp`) rather than
        // surface it. Sub-nanosecond precision is dropped for `exp > 9`,
        // which is fine for capture timestamps.
        let nanos: u32 = if exp <= 9 {
            let scale = 10u64.pow(9 - exp);
            u32::try_from(frac * scale).expect("frac*scale < 10^9 fits u32")
        } else {
            let div = 10u64.pow(exp - 9);
            u32::try_from(frac / div).expect("frac/div < 10^9 fits u32")
        };
        Duration::new(secs, nanos)
    } else {
        // 2^-(tsresol & 0x7f) seconds. Rarely used in practice.
        let exp = (tsresol & 0x7f).min(63);
        let denom = 1u64 << exp;
        let secs = raw / denom;
        let frac = raw % denom;
        // `frac < denom`, so `scaled < 10^9` and fits a u32 — see L3 note above.
        let scaled = (u128::from(frac) * 1_000_000_000u128) / u128::from(denom);
        let nanos = u32::try_from(scaled).expect("scaled < 10^9 fits u32");
        Duration::new(secs, nanos)
    }
}

fn ingest_packet(
    linktype: Linktype,
    data: &[u8],
    captured: Duration,
    frames: &mut Vec<RecordedFrame>,
    t0: &mut Option<Duration>,
) {
    let Some(ip_payload) = strip_link_layer(linktype, data) else {
        return;
    };
    let Some((src_ip, dst_ip, udp)) = strip_ip_layer(ip_payload) else {
        return;
    };
    let Some((src_port, dst_port, payload)) = strip_udp(udp) else {
        return;
    };
    if src_port != IAX2_PORT && dst_port != IAX2_PORT {
        return;
    }
    let relative = match *t0 {
        None => {
            *t0 = Some(captured);
            Duration::ZERO
        }
        Some(start) => captured.saturating_sub(start),
    };
    frames.push(RecordedFrame {
        timestamp: relative,
        src_addr: SocketAddr::new(src_ip, src_port),
        dst_addr: SocketAddr::new(dst_ip, dst_port),
        bytes: payload.to_vec(),
    });
}

/// Strip the link-layer header, returning the IP payload.
fn strip_link_layer(linktype: Linktype, data: &[u8]) -> Option<&[u8]> {
    match linktype {
        Linktype::ETHERNET => {
            if data.len() < 14 {
                return None;
            }
            let ethertype = u16::from_be_bytes([data[12], data[13]]);
            match ethertype {
                0x0800 | 0x86dd => Some(&data[14..]),
                0x8100 => {
                    // 802.1Q VLAN tag; skip 4 bytes and re-read ethertype.
                    if data.len() < 18 {
                        return None;
                    }
                    let inner = u16::from_be_bytes([data[16], data[17]]);
                    match inner {
                        0x0800 | 0x86dd => Some(&data[18..]),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        Linktype::NULL | Linktype::LOOP => {
            // BSD loopback / OpenBSD loopback: 4-byte family header. NULL
            // is host-endian per the linktype spec; we accept both.
            if data.len() < 4 {
                return None;
            }
            // BSD/OpenBSD loopback prepends a 4-byte address family.
            // The family number itself varies by OS (AF_INET = 2 every
            // where; AF_INET6 is 24/28/30/10 depending on flavour), so
            // rather than enumerate them we just skip the header and
            // let the IP-version nibble in the payload do the
            // dispatching downstream.
            Some(&data[4..])
        }
        Linktype::RAW | Linktype::IPV4 | Linktype::IPV6 => Some(data),
        Linktype::LINUX_SLL => {
            // 16-byte cooked-mode header: packet_type(2) + arphrd(2) +
            // addr_len(2) + addr(8) + protocol(2).
            if data.len() < 16 {
                return None;
            }
            let ethertype = u16::from_be_bytes([data[14], data[15]]);
            match ethertype {
                0x0800 | 0x86dd => Some(&data[16..]),
                _ => None,
            }
        }
        Linktype(276) => {
            // LINUX_SLL2: 20-byte cooked-mode v2 header (not yet a named
            // constant in pcap-parser). protocol(2) + reserved(2) +
            // ifindex(4) + arphrd(2) + packet_type(1) + addr_len(1) +
            // addr(8).
            if data.len() < 20 {
                return None;
            }
            let ethertype = u16::from_be_bytes([data[0], data[1]]);
            match ethertype {
                0x0800 | 0x86dd => Some(&data[20..]),
                _ => None,
            }
        }
        other => {
            tracing::warn!(?other, "unsupported pcap linktype; skipping packet");
            None
        }
    }
}

/// Strip the IP header, returning `(src, dst, transport_payload)`. Returns
/// `None` for non-IP, non-UDP, or fragmented packets.
fn strip_ip_layer(data: &[u8]) -> Option<(IpAddr, IpAddr, &[u8])> {
    if data.is_empty() {
        return None;
    }
    match data[0] >> 4 {
        4 => strip_ipv4(data),
        6 => strip_ipv6(data),
        _ => None,
    }
}

fn strip_ipv4(data: &[u8]) -> Option<(IpAddr, IpAddr, &[u8])> {
    if data.len() < 20 {
        return None;
    }
    let ihl = (data[0] & 0x0f) as usize * 4;
    if ihl < 20 || data.len() < ihl {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([data[2], data[3]]));
    if total_len > data.len() || total_len < ihl {
        return None;
    }
    let frag_field = u16::from_be_bytes([data[6], data[7]]);
    let mf = frag_field & 0x2000 != 0;
    let offset = frag_field & 0x1fff;
    if mf || offset != 0 {
        tracing::warn!("IPv4 fragment in capture; skipping");
        return None;
    }
    let proto = data[9];
    if proto != 17 {
        return None;
    }
    let src = Ipv4Addr::new(data[12], data[13], data[14], data[15]);
    let dst = Ipv4Addr::new(data[16], data[17], data[18], data[19]);
    Some((IpAddr::V4(src), IpAddr::V4(dst), &data[ihl..total_len]))
}

fn strip_ipv6(data: &[u8]) -> Option<(IpAddr, IpAddr, &[u8])> {
    if data.len() < 40 {
        return None;
    }
    let payload_len = usize::from(u16::from_be_bytes([data[4], data[5]]));
    let next_header = data[6];
    if next_header != 17 {
        // No support for extension headers — captures that need them
        // will simply produce zero frames for that packet.
        return None;
    }
    let total = 40 + payload_len;
    if total > data.len() {
        return None;
    }
    let mut src = [0u8; 16];
    src.copy_from_slice(&data[8..24]);
    let mut dst = [0u8; 16];
    dst.copy_from_slice(&data[24..40]);
    Some((
        IpAddr::V6(Ipv6Addr::from(src)),
        IpAddr::V6(Ipv6Addr::from(dst)),
        &data[40..total],
    ))
}

fn strip_udp(data: &[u8]) -> Option<(u16, u16, &[u8])> {
    if data.len() < 8 {
        return None;
    }
    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let length = usize::from(u16::from_be_bytes([data[4], data[5]]));
    if length < 8 || length > data.len() {
        return None;
    }
    Some((src_port, dst_port, &data[8..length]))
}

/// Walk every recorded frame in `fixture`, parse it, re-encode it, and
/// assert byte equality with the original payload.
pub fn play(
    fixture: &ReplayFixture,
    assertion: &ReplayAssertion,
) -> Result<ReplayStats, ReplayError> {
    let strict = !matches!(assertion, ReplayAssertion::ParseOnly);
    let mut stats = ReplayStats {
        frames_total: fixture.frames.len(),
        ..ReplayStats::default()
    };
    for (index, rec) in fixture.frames.iter().enumerate() {
        let frame_result = if strict {
            parse(&rec.bytes)
        } else {
            astar_iax_core::frame::parse_lenient(&rec.bytes)
        };
        let frame = frame_result.map_err(|e| ReplayError::Parse {
            index,
            len: rec.bytes.len(),
            src: rec.src_addr,
            dst: rec.dst_addr,
            source: e,
        })?;
        if strict {
            let mut buf = Vec::with_capacity(rec.bytes.len());
            // `frame` was just parsed from `rec.bytes`, so every IE
            // payload is bounded by the wire's u8 length prefix and
            // re-encoding is infallible.
            encode(&frame, &mut buf).expect("re-encode of parsed wire frame must fit");
            if buf != rec.bytes {
                return Err(ReplayError::RoundTrip {
                    index,
                    src: rec.src_addr,
                    dst: rec.dst_addr,
                    orig_len: rec.bytes.len(),
                    reenc_len: buf.len(),
                });
            }
        }
        stats.frames_parsed += 1;
        match frame {
            Frame::Full(_) => stats.full_frames += 1,
            Frame::Mini(_) => stats.mini_frames += 1,
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_udp_rejects_short() {
        assert!(strip_udp(&[0u8; 4]).is_none());
    }

    #[test]
    fn strip_udp_extracts_ports_and_payload() {
        // src=4569 dst=12345 len=10 csum=0 payload=AA BB
        let pkt = [0x11, 0xd9, 0x30, 0x39, 0x00, 0x0a, 0x00, 0x00, 0xaa, 0xbb];
        let (src, dst, payload) = strip_udp(&pkt).unwrap();
        assert_eq!(src, IAX2_PORT);
        assert_eq!(dst, 12_345);
        assert_eq!(payload, &[0xaa, 0xbb]);
    }

    #[test]
    fn strip_ipv4_drops_fragments() {
        let mut pkt = vec![
            0x45, 0x00, 0x00, 0x1c, 0x00, 0x00, 0x20, 0x00, // MF=1
            0x40, 0x11, 0x00, 0x00, 0x7f, 0x00, 0x00, 0x01, 0x7f, 0x00, 0x00, 0x02,
        ];
        pkt.extend_from_slice(&[0u8; 8]);
        assert!(strip_ipv4(&pkt).is_none());
    }

    #[test]
    fn pcapng_ts_microseconds() {
        // 1 second, 500_000 µs => 1.5 s.
        let raw: u64 = 1_500_000;
        let ts_high = u32::try_from(raw >> 32).unwrap();
        let ts_low = u32::try_from(raw & 0xffff_ffff).unwrap();
        let d = pcapng_ts(ts_high, ts_low, 6);
        assert_eq!(d, Duration::new(1, 500_000_000));
    }

    #[test]
    fn pcapng_ts_nanoseconds() {
        // 2 s, 250 ns at resolution 10^-9.
        let raw: u64 = 2 * 1_000_000_000 + 250;
        let ts_high = u32::try_from(raw >> 32).unwrap();
        let ts_low = u32::try_from(raw & 0xffff_ffff).unwrap();
        let d = pcapng_ts(ts_high, ts_low, 9);
        assert_eq!(d, Duration::new(2, 250));
    }
}
