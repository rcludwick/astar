// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Minimal hand-rolled IPv4+UDP packet codec (iax-35b4).
//!
//! `WireGuard` is L3: with no TUN device the userspace stack must synthesize
//! and parse the inner IP/UDP headers itself. This module builds an IPv4
//! packet (20-byte header, no options, DF set) carrying a UDP datagram with
//! correct header checksums, and parses/validates the reverse direction.
//! No fragmentation, IPv4-only for v1.

use std::net::{Ipv4Addr, SocketAddrV4};

/// Fixed IPv4 header length: 20 bytes (IHL 5, no options).
pub const IPV4_HEADER_LEN: usize = 20;
/// UDP header length: 8 bytes.
pub const UDP_HEADER_LEN: usize = 8;

/// IP protocol number for UDP.
const PROTOCOL_UDP: u8 = 17;
/// TTL for built packets; generous since the tunnel is one hop.
const TTL: u8 = 64;
/// Largest payload that keeps the IPv4 total length within `u16`.
const MAX_PAYLOAD: usize = u16::MAX as usize - IPV4_HEADER_LEN - UDP_HEADER_LEN;

/// Why a buffer failed to build or parse as an IPv4+UDP packet.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    /// Input shorter than the headers it must contain.
    #[error("packet truncated ({0} bytes)")]
    Truncated(usize),
    /// IP version field is not 4.
    #[error("not an IPv4 packet (version {0})")]
    NotIpv4(u8),
    /// IHL other than 5 (options are rejected in v1).
    #[error("unsupported IHL {0} (options not supported)")]
    UnsupportedIhl(u8),
    /// IPv4 total-length field disagrees with the input length.
    #[error("IPv4 total length {total} does not match input length {input}")]
    LengthMismatch {
        /// Value of the total-length header field.
        total: u16,
        /// Actual number of input bytes.
        input: usize,
    },
    /// IPv4 header checksum verification failed.
    #[error("bad IPv4 header checksum")]
    BadIpv4Checksum,
    /// IP protocol is not UDP.
    #[error("not a UDP packet (protocol {0})")]
    NotUdp(u8),
    /// UDP length field disagrees with the space after the IPv4 header.
    #[error("bad UDP length {0}")]
    BadUdpLength(u16),
    /// UDP checksum verification failed (a zero checksum is accepted).
    #[error("bad UDP checksum")]
    BadUdpChecksum,
    /// Payload would overflow the 16-bit IPv4 total length.
    #[error("payload too large ({0} bytes)")]
    PayloadTooLarge(usize),
}

/// A validated IPv4+UDP packet: addresses plus a borrowed payload slice.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedUdp4<'a> {
    /// Inner source address (IPv4 source + UDP source port).
    pub src: SocketAddrV4,
    /// Inner destination address (IPv4 destination + UDP destination port).
    pub dst: SocketAddrV4,
    /// UDP payload, borrowed from the input buffer.
    pub payload: &'a [u8],
}

/// Build an IPv4+UDP packet around `payload`.
///
/// Header: IHL 5 (no options), DF set, TTL 64, protocol UDP, correct IPv4
/// header checksum and UDP checksum (pseudo-header included; a computed
/// checksum of `0x0000` is transmitted as `0xFFFF` per RFC 768).
pub fn build_udp4(
    src: SocketAddrV4,
    dst: SocketAddrV4,
    payload: &[u8],
) -> Result<Vec<u8>, PacketError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(PacketError::PayloadTooLarge(payload.len()));
    }
    // Both fit in u16: payload.len() <= MAX_PAYLOAD bounds the totals.
    #[allow(clippy::cast_possible_truncation)]
    let total_len = (IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len()) as u16;
    #[allow(clippy::cast_possible_truncation)]
    let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;

    let mut pkt = Vec::with_capacity(usize::from(total_len));
    pkt.extend_from_slice(&[0x45, 0x00]); // version 4, IHL 5, DSCP/ECN 0
    pkt.extend_from_slice(&total_len.to_be_bytes());
    pkt.extend_from_slice(&[0x00, 0x00]); // identification
    pkt.extend_from_slice(&[0x40, 0x00]); // DF set, fragment offset 0
    pkt.extend_from_slice(&[TTL, PROTOCOL_UDP]);
    pkt.extend_from_slice(&[0x00, 0x00]); // header checksum placeholder
    pkt.extend_from_slice(&src.ip().octets());
    pkt.extend_from_slice(&dst.ip().octets());
    let ip_csum = checksum(&pkt[..IPV4_HEADER_LEN]);
    pkt[10..12].copy_from_slice(&ip_csum.to_be_bytes());

    pkt.extend_from_slice(&src.port().to_be_bytes());
    pkt.extend_from_slice(&dst.port().to_be_bytes());
    pkt.extend_from_slice(&udp_len.to_be_bytes());
    pkt.extend_from_slice(&[0x00, 0x00]); // UDP checksum placeholder
    pkt.extend_from_slice(payload);
    let udp_csum = match !fold(udp_pseudo_sum(
        *src.ip(),
        *dst.ip(),
        &pkt[IPV4_HEADER_LEN..],
    )) {
        // RFC 768: a computed checksum of zero is transmitted as all ones.
        0x0000 => 0xFFFF,
        c => c,
    };
    pkt[26..28].copy_from_slice(&udp_csum.to_be_bytes());
    Ok(pkt)
}

/// Parse and validate an IPv4+UDP packet.
///
/// Rejects non-IPv4, options (IHL != 5), non-UDP, truncated input, length
/// mismatches, and bad checksums. A UDP checksum field of zero means "not
/// computed" and is accepted.
pub fn parse_udp4(packet: &[u8]) -> Result<ParsedUdp4<'_>, PacketError> {
    if packet.len() < IPV4_HEADER_LEN {
        return Err(PacketError::Truncated(packet.len()));
    }
    let version = packet[0] >> 4;
    if version != 4 {
        return Err(PacketError::NotIpv4(version));
    }
    let ihl = packet[0] & 0x0F;
    if ihl != 5 {
        return Err(PacketError::UnsupportedIhl(ihl));
    }
    let total = u16::from_be_bytes([packet[2], packet[3]]);
    if usize::from(total) != packet.len() {
        return Err(PacketError::LengthMismatch {
            total,
            input: packet.len(),
        });
    }
    if fold(sum_words(0, &packet[..IPV4_HEADER_LEN])) != 0xFFFF {
        return Err(PacketError::BadIpv4Checksum);
    }
    let protocol = packet[9];
    if protocol != PROTOCOL_UDP {
        return Err(PacketError::NotUdp(protocol));
    }
    if packet.len() < IPV4_HEADER_LEN + UDP_HEADER_LEN {
        return Err(PacketError::Truncated(packet.len()));
    }
    let src_ip = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let dst_ip = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);

    let udp = &packet[IPV4_HEADER_LEN..];
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]);
    if usize::from(udp_len) != udp.len() {
        return Err(PacketError::BadUdpLength(udp_len));
    }
    let udp_csum = u16::from_be_bytes([udp[6], udp[7]]);
    // A zero checksum means "not computed" — legitimate in IPv4. Otherwise
    // the sum over pseudo-header + datagram (checksum included) must fold
    // to all ones.
    if udp_csum != 0 && fold(udp_pseudo_sum(src_ip, dst_ip, udp)) != 0xFFFF {
        return Err(PacketError::BadUdpChecksum);
    }

    Ok(ParsedUdp4 {
        src: SocketAddrV4::new(src_ip, u16::from_be_bytes([udp[0], udp[1]])),
        dst: SocketAddrV4::new(dst_ip, u16::from_be_bytes([udp[2], udp[3]])),
        payload: &udp[UDP_HEADER_LEN..],
    })
}

/// Accumulate big-endian 16-bit words of `data` onto `sum` (RFC 1071).
/// An odd trailing byte is zero-padded on the right.
fn sum_words(mut sum: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(2);
    for word in chunks.by_ref() {
        sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(u16::from_be_bytes([*last, 0]));
    }
    sum
}

/// Fold a 32-bit one's-complement accumulator down to 16 bits.
fn fold(mut sum: u32) -> u16 {
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    // The loop leaves sum <= 0xFFFF.
    #[allow(clippy::cast_possible_truncation)]
    let folded = sum as u16;
    folded
}

/// Internet checksum (RFC 1071) over `data`: one's-complement sum of
/// big-endian 16-bit words (odd tail zero-padded), then complemented.
fn checksum(data: &[u8]) -> u16 {
    !fold(sum_words(0, data))
}

/// One's-complement sum of the UDP pseudo-header plus the whole UDP
/// datagram `udp` (header + payload), unfolded and uncomplemented.
fn udp_pseudo_sum(src: Ipv4Addr, dst: Ipv4Addr, udp: &[u8]) -> u32 {
    let mut sum = sum_words(0, &src.octets());
    sum = sum_words(sum, &dst.octets());
    sum += u32::from(PROTOCOL_UDP); // zero byte + protocol
    // Callers bound the datagram to an IPv4 total length, so this fits.
    #[allow(clippy::cast_possible_truncation)]
    let udp_len = udp.len() as u32; // pseudo-header UDP length
    sum += udp_len;
    sum_words(sum, udp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(ip: [u8; 4], port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::from(ip), port)
    }

    /// Hand-computed reference packet: 10.0.0.1:4569 -> 10.0.0.2:4569, "hi".
    ///
    /// IPv4 header sum: 0x4500+0x001E+0x0000+0x4000+0x4011+0x0A00+0x0001
    /// +0x0A00+0x0002 = 0xD932, checksum = !0xD932 = 0x26CD. UDP pseudo+
    /// header+payload sum: 0x141E+0x23BC+0x6869 = 0xA043, checksum = 0x5FBC.
    const REFERENCE: [u8; 30] = [
        0x45, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x26, 0xCD, 0x0A, 0x00, 0x00,
        0x01, 0x0A, 0x00, 0x00, 0x02, 0x11, 0xD9, 0x11, 0xD9, 0x00, 0x0A, 0x5F, 0xBC, 0x68, 0x69,
    ];

    fn reference_src() -> SocketAddrV4 {
        addr([10, 0, 0, 1], 4569)
    }

    fn reference_dst() -> SocketAddrV4 {
        addr([10, 0, 0, 2], 4569)
    }

    #[test]
    fn build_matches_hand_computed_vector() {
        let pkt = build_udp4(reference_src(), reference_dst(), b"hi").unwrap();
        assert_eq!(pkt, REFERENCE);
    }

    #[test]
    fn parse_accepts_hand_computed_vector() {
        let parsed = parse_udp4(&REFERENCE).unwrap();
        assert_eq!(parsed.src, reference_src());
        assert_eq!(parsed.dst, reference_dst());
        assert_eq!(parsed.payload, b"hi");
    }

    #[test]
    fn round_trip_preserves_fields_and_payload() {
        let src = addr([192, 168, 7, 4], 41000);
        let dst = addr([10, 200, 0, 9], 4569);
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01]; // odd length
        let pkt = build_udp4(src, dst, &payload).unwrap();
        let parsed = parse_udp4(&pkt).unwrap();
        assert_eq!(parsed.src, src);
        assert_eq!(parsed.dst, dst);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn round_trip_empty_payload() {
        let pkt = build_udp4(reference_src(), reference_dst(), &[]).unwrap();
        assert_eq!(pkt.len(), IPV4_HEADER_LEN + UDP_HEADER_LEN);
        let parsed = parse_udp4(&pkt).unwrap();
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn zero_udp_checksum_transmits_as_ffff() {
        // Payload chosen so the pseudo-header sum folds to 0xFFFF: the
        // computed checksum is 0x0000, which must go on the wire as 0xFFFF.
        let pkt = build_udp4(reference_src(), reference_dst(), &[0xC8, 0x25]).unwrap();
        assert_eq!(&pkt[26..28], &[0xFF, 0xFF]);
        let parsed = parse_udp4(&pkt).unwrap();
        assert_eq!(parsed.payload, &[0xC8, 0x25]);
    }

    #[test]
    fn parse_accepts_zero_udp_checksum() {
        // A zero UDP checksum means "not computed" in IPv4 and must pass.
        let mut pkt = REFERENCE;
        pkt[26] = 0;
        pkt[27] = 0;
        let parsed = parse_udp4(&pkt).unwrap();
        assert_eq!(parsed.payload, b"hi");
    }

    #[test]
    fn parse_rejects_wrong_version() {
        let mut pkt = REFERENCE;
        pkt[0] = 0x65;
        assert_eq!(parse_udp4(&pkt), Err(PacketError::NotIpv4(6)));
    }

    #[test]
    fn parse_rejects_options_ihl() {
        let mut pkt = REFERENCE;
        pkt[0] = 0x46;
        assert_eq!(parse_udp4(&pkt), Err(PacketError::UnsupportedIhl(6)));
        pkt[0] = 0x44;
        assert_eq!(parse_udp4(&pkt), Err(PacketError::UnsupportedIhl(4)));
    }

    #[test]
    fn parse_rejects_non_udp_protocol() {
        // Change protocol to TCP and refresh the IPv4 header checksum so the
        // protocol check itself is what fires.
        let mut pkt = REFERENCE;
        pkt[9] = 6;
        pkt[10] = 0;
        pkt[11] = 0;
        let csum = checksum(&pkt[..IPV4_HEADER_LEN]);
        pkt[10..12].copy_from_slice(&csum.to_be_bytes());
        assert_eq!(parse_udp4(&pkt), Err(PacketError::NotUdp(6)));
    }

    #[test]
    fn parse_rejects_bad_ipv4_checksum() {
        let mut pkt = REFERENCE;
        pkt[8] ^= 0x01; // perturb TTL without touching the checksum
        assert_eq!(parse_udp4(&pkt), Err(PacketError::BadIpv4Checksum));
    }

    #[test]
    fn parse_rejects_bad_udp_checksum() {
        let mut pkt = REFERENCE;
        pkt[29] ^= 0x01; // flip a payload bit
        assert_eq!(parse_udp4(&pkt), Err(PacketError::BadUdpChecksum));
    }

    #[test]
    fn parse_rejects_total_length_mismatch() {
        let mut pkt = REFERENCE.to_vec();
        pkt.push(0x00); // trailing garbage byte
        assert_eq!(
            parse_udp4(&pkt),
            Err(PacketError::LengthMismatch {
                total: 30,
                input: 31
            })
        );
    }

    #[test]
    fn parse_rejects_bad_udp_length() {
        let mut pkt = REFERENCE;
        pkt[25] = 12; // claims 12 bytes; 10 are present
        assert_eq!(parse_udp4(&pkt), Err(PacketError::BadUdpLength(12)));
        pkt[25] = 4; // shorter than the UDP header itself
        assert_eq!(parse_udp4(&pkt), Err(PacketError::BadUdpLength(4)));
    }

    #[test]
    fn parse_rejects_every_truncation_without_panicking() {
        for len in 0..REFERENCE.len() {
            assert!(
                parse_udp4(&REFERENCE[..len]).is_err(),
                "prefix of {len} bytes must not parse"
            );
        }
    }

    #[test]
    fn build_rejects_oversized_payload() {
        let payload = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            build_udp4(reference_src(), reference_dst(), &payload),
            Err(PacketError::PayloadTooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn build_round_trips_max_payload() {
        let payload = vec![0xA5u8; MAX_PAYLOAD];
        let pkt = build_udp4(reference_src(), reference_dst(), &payload).unwrap();
        assert_eq!(pkt.len(), u16::MAX as usize);
        let parsed = parse_udp4(&pkt).unwrap();
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn checksum_folds_carries() {
        // 0xFFFF + 0xFFFF = 0x1FFFE folds to 0xFFFF; complement is 0x0000.
        assert_eq!(checksum(&[0xFF, 0xFF, 0xFF, 0xFF]), 0x0000);
        // Sum with a carry out of bit 16: 0x8000 + 0x8001 = 0x10001 -> 0x0002.
        assert_eq!(checksum(&[0x80, 0x00, 0x80, 0x01]), !0x0002);
    }

    #[test]
    fn checksum_pads_odd_length_with_zero() {
        // A trailing lone byte acts as the high octet of a zero-padded word.
        assert_eq!(checksum(&[0x12]), checksum(&[0x12, 0x00]));
    }
}
