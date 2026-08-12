// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 reflector control-plane packets: `CONN`/`ACKN`/`NACK`/`PING`/`PONG`/`DISC`.

/// A parsed control-plane packet exchanged with an M17 reflector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPacket {
    /// Client's request to link to a reflector module: `"CONN"` + 6-byte
    /// callsign + 1 ASCII module letter (11 bytes).
    Conn {
        /// Requesting station's base-40 callsign.
        callsign: [u8; 6],
        /// ASCII module letter (e.g. `b'A'`).
        module: u8,
    },
    /// Reflector's link acceptance: `"ACKN"` only (4 bytes).
    Ackn,
    /// Reflector's link rejection: `"NACK"` only (4 bytes).
    Nack,
    /// Keepalive from the reflector: `"PING"` + 6-byte callsign (10 bytes).
    Ping {
        /// Reflector's base-40 callsign.
        callsign: [u8; 6],
    },
    /// Keepalive reply from the client: `"PONG"` + 6-byte callsign (10 bytes).
    Pong {
        /// Replying station's base-40 callsign.
        callsign: [u8; 6],
    },
    /// Disconnect notice. Client-initiated disconnect carries the station's
    /// callsign (`"DISC"` + 6 bytes, 10 bytes total); the reflector's
    /// acknowledgement is `"DISC"` alone (4 bytes, `callsign: None`).
    Disc {
        /// `Some` when this packet carries a callsign (client-initiated),
        /// `None` for the reflector's bare disconnect acknowledgement.
        callsign: Option<[u8; 6]>,
    },
}

impl ControlPacket {
    /// Serializes this packet to its wire form.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            ControlPacket::Conn { callsign, module } => {
                let mut buf = Vec::with_capacity(11);
                buf.extend_from_slice(b"CONN");
                buf.extend_from_slice(callsign);
                buf.push(*module);
                buf
            }
            ControlPacket::Ackn => b"ACKN".to_vec(),
            ControlPacket::Nack => b"NACK".to_vec(),
            ControlPacket::Ping { callsign } => {
                let mut buf = Vec::with_capacity(10);
                buf.extend_from_slice(b"PING");
                buf.extend_from_slice(callsign);
                buf
            }
            ControlPacket::Pong { callsign } => {
                let mut buf = Vec::with_capacity(10);
                buf.extend_from_slice(b"PONG");
                buf.extend_from_slice(callsign);
                buf
            }
            ControlPacket::Disc { callsign } => {
                let mut buf = Vec::with_capacity(callsign.map_or(4, |_| 10));
                buf.extend_from_slice(b"DISC");
                if let Some(cs) = callsign {
                    buf.extend_from_slice(cs);
                }
                buf
            }
        }
    }

    /// Parses a control packet from `buf`, per the pinned sizes for each
    /// magic. Returns `None` for unrecognized magics or mismatched lengths.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<ControlPacket> {
        if buf.len() < 4 {
            return None;
        }
        let magic = &buf[0..4];
        match (magic, buf.len()) {
            (b"CONN", 11) => {
                let mut callsign = [0u8; 6];
                callsign.copy_from_slice(&buf[4..10]);
                Some(ControlPacket::Conn {
                    callsign,
                    module: buf[10],
                })
            }
            (b"ACKN", 4) => Some(ControlPacket::Ackn),
            (b"NACK", 4) => Some(ControlPacket::Nack),
            (b"PING", 10) => Some(ControlPacket::Ping {
                callsign: callsign_at(buf),
            }),
            (b"PONG", 10) => Some(ControlPacket::Pong {
                callsign: callsign_at(buf),
            }),
            (b"DISC", 10) => Some(ControlPacket::Disc {
                callsign: Some(callsign_at(buf)),
            }),
            (b"DISC", 4) => Some(ControlPacket::Disc { callsign: None }),
            _ => None,
        }
    }
}

/// Reads the 6-byte callsign starting at offset 4 (used for PING/PONG/DISC).
fn callsign_at(buf: &[u8]) -> [u8; 6] {
    let mut callsign = [0u8; 6];
    callsign.copy_from_slice(&buf[4..10]);
    callsign
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::encode_callsign;

    #[test]
    fn control_packet_sizes_match_mrefd() {
        let cs = encode_callsign("N0CALL").unwrap();
        assert_eq!(
            ControlPacket::Conn {
                callsign: cs,
                module: b'A'
            }
            .to_bytes()
            .len(),
            11
        );
        assert_eq!(ControlPacket::Ping { callsign: cs }.to_bytes().len(), 10);
        assert_eq!(ControlPacket::Ackn.to_bytes(), b"ACKN");
        assert_eq!(ControlPacket::parse(b"NACK"), Some(ControlPacket::Nack));
    }

    #[test]
    fn round_trips_conn_ping_pong_and_both_disc_forms() {
        let cs = encode_callsign("N0CALL").unwrap();
        let conn = ControlPacket::Conn {
            callsign: cs,
            module: b'A',
        };
        assert_eq!(ControlPacket::parse(&conn.to_bytes()), Some(conn));

        let keepalive_req = ControlPacket::Ping { callsign: cs };
        assert_eq!(
            ControlPacket::parse(&keepalive_req.to_bytes()),
            Some(keepalive_req)
        );

        let keepalive_reply = ControlPacket::Pong { callsign: cs };
        assert_eq!(
            ControlPacket::parse(&keepalive_reply.to_bytes()),
            Some(keepalive_reply)
        );

        let disc_with_cs = ControlPacket::Disc { callsign: Some(cs) };
        assert_eq!(
            ControlPacket::parse(&disc_with_cs.to_bytes()),
            Some(disc_with_cs)
        );

        let disc_bare = ControlPacket::Disc { callsign: None };
        assert_eq!(disc_bare.to_bytes(), b"DISC");
        assert_eq!(ControlPacket::parse(&disc_bare.to_bytes()), Some(disc_bare));
    }

    #[test]
    fn parse_rejects_unknown_magic_and_bad_lengths() {
        assert!(ControlPacket::parse(b"XXXX").is_none());
        assert!(ControlPacket::parse(b"ACK").is_none()); // too short
        assert!(ControlPacket::parse(b"ACKNX").is_none()); // wrong length for ACKN
    }
}
