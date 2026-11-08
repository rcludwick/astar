// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The two voice-port datagrams: GSM audio packets and `oNDATA` text.

/// RTP-style header in front of the GSM frames.
pub const AUDIO_HEADER_LEN: usize = 12;
/// One GSM 06.10 frame.
pub const GSM_FRAME_LEN: usize = 33;
/// GSM frames per audio datagram (80 ms of speech).
pub const FRAMES_PER_PACKET: usize = 4;
/// Total length of an audio datagram.
pub const AUDIO_PACKET_LEN: usize = AUDIO_HEADER_LEN + GSM_FRAME_LEN * FRAMES_PER_PACKET;
/// Longest text body kept from an `oNDATA` datagram, in bytes.
pub const MAX_TEXT_BYTES: usize = 512;

const AUDIO_MARK: u8 = 0xC0;
const PAYLOAD_GSM: u8 = 0x03;
const ONDATA: &[u8] = b"oNDATA";

/// One audio datagram: a sequence number and four GSM frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u16,
    pub frames: [[u8; GSM_FRAME_LEN]; FRAMES_PER_PACKET],
}

impl AudioPacket {
    /// Header `C0 03 seq(be) 00*8`, then the four frames back to back.
    #[must_use]
    pub fn encode(&self) -> [u8; AUDIO_PACKET_LEN] {
        let mut out = [0u8; AUDIO_PACKET_LEN];
        out[0] = AUDIO_MARK;
        out[1] = PAYLOAD_GSM;
        out[2..4].copy_from_slice(&self.seq.to_be_bytes());
        for (i, f) in self.frames.iter().enumerate() {
            let at = AUDIO_HEADER_LEN + i * GSM_FRAME_LEN;
            out[at..at + GSM_FRAME_LEN].copy_from_slice(f);
        }
        out
    }
}

/// What a voice-port datagram turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datagram {
    Audio(AudioPacket),
    /// Station information text; newlines are restored from CR.
    Info(String),
    Chat {
        from: String,
        text: String,
    },
    /// Anything else: wrong length, Speex, unknown, malformed.
    Unsupported,
}

/// Decode text bytes lossily and cap the result at [`MAX_TEXT_BYTES`]
/// without splitting a character.
fn clean_text(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TEXT_BYTES * 4)]).into_owned();
    if s.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// Sort a received voice-port datagram.
#[must_use]
pub fn classify(bytes: &[u8]) -> Datagram {
    if bytes.first() == Some(&AUDIO_MARK) {
        if bytes.len() != AUDIO_PACKET_LEN || bytes[1] != PAYLOAD_GSM {
            return Datagram::Unsupported;
        }
        let mut frames = [[0u8; GSM_FRAME_LEN]; FRAMES_PER_PACKET];
        for (i, f) in frames.iter_mut().enumerate() {
            let at = AUDIO_HEADER_LEN + i * GSM_FRAME_LEN;
            f.copy_from_slice(&bytes[at..at + GSM_FRAME_LEN]);
        }
        return Datagram::Audio(AudioPacket {
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            frames,
        });
    }
    let Some(rest) = bytes.strip_prefix(ONDATA) else {
        return Datagram::Unsupported;
    };
    if let Some(info) = rest.strip_prefix(b"\r") {
        return Datagram::Info(clean_text(info).replace('\r', "\n"));
    }
    let Some(gt) = rest.iter().position(|&b| b == b'>') else {
        return Datagram::Unsupported;
    };
    let body = &rest[gt + 1..];
    let body = body.strip_suffix(b"\r\n").unwrap_or(body);
    Datagram::Chat {
        from: clean_text(&rest[..gt]),
        text: clean_text(body),
    }
}

/// True when `bytes` is a GSM audio datagram: the only voice-port datagram
/// worth recording.
#[must_use]
pub fn is_media(bytes: &[u8]) -> bool {
    bytes.len() == AUDIO_PACKET_LEN && bytes[0] == AUDIO_MARK && bytes[1] == PAYLOAD_GSM
}

/// `oNDATA`, CR, then the text with each newline turned into CR.
#[must_use]
pub fn info_datagram(text: &str) -> Vec<u8> {
    let mut out = ONDATA.to_vec();
    out.push(b'\r');
    out.extend_from_slice(clean_text(text.as_bytes()).replace('\n', "\r").as_bytes());
    out
}

/// `oNDATA`, callsign, `>`, text, CR LF.
#[must_use]
pub fn chat_datagram(callsign: &str, text: &str) -> Vec<u8> {
    let mut out = ONDATA.to_vec();
    out.extend_from_slice(callsign.as_bytes());
    out.push(b'>');
    out.extend_from_slice(clean_text(text.as_bytes()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

/// True when `seq` is ahead of `last`, counting around the 16-bit wrap
/// (a half-range window).
#[must_use]
pub fn seq_is_newer(seq: u16, last: u16) -> bool {
    let d = seq.wrapping_sub(last);
    d != 0 && d < 0x8000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AUDIO_PORT, CONTROL_PORT, DIRECTORY_PORT, PeerAddr, Timings};
    use std::net::Ipv4Addr;
    use std::time::Duration;

    #[test]
    fn timings_default_matches_the_spec_table() {
        let t = Timings::default();
        assert_eq!(t.status_refresh, Duration::from_secs(300));
        assert_eq!(t.list_refresh, Duration::from_secs(600));
        assert_eq!(t.list_min_interval, Duration::from_secs(30));
        assert_eq!(t.backoff_start, Duration::from_secs(20));
        assert_eq!(t.backoff_cap, Duration::from_secs(600));
        assert_eq!(t.directory_connect_timeout, Duration::from_secs(10));
        assert_eq!(t.directory_read_timeout, Duration::from_secs(30));
        assert_eq!(t.keepalive, Duration::from_secs(10));
        assert_eq!(t.give_up_after, 5);
        assert_eq!(t.connection_timeout, Duration::from_secs(50));
        assert_eq!(t.talk_hold, Duration::from_millis(320));
        assert_eq!(t.max_transmit, Duration::from_secs(600));
        assert_eq!(DIRECTORY_PORT, 5200);
    }

    #[test]
    fn fixed_peer_addr_uses_5198_and_5199() {
        let p = PeerAddr::fixed(Ipv4Addr::LOCALHOST);
        assert_eq!(p.audio.port(), AUDIO_PORT);
        assert_eq!(p.control.port(), CONTROL_PORT);
        assert_eq!((AUDIO_PORT, CONTROL_PORT), (5198, 5199));
        assert_eq!(p.audio.ip(), p.control.ip());
    }

    #[test]
    fn audio_header_is_c0_03_seq_then_eight_zeros() {
        let p = AudioPacket {
            seq: 0x1234,
            frames: [[0xD0; 33]; 4],
        }
        .encode();
        assert_eq!(&p[..12], &[0xC0, 0x03, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(p.len(), 144);
    }

    #[test]
    fn audio_round_trips_through_classify() {
        let mut frames = [[0u8; 33]; 4];
        for (i, f) in frames.iter_mut().enumerate() {
            for (j, b) in f.iter_mut().enumerate() {
                *b = u8::try_from(i * 33 + j).unwrap();
            }
        }
        let pkt = AudioPacket {
            seq: 0xFFFE,
            frames,
        };
        assert_eq!(classify(&pkt.encode()), Datagram::Audio(pkt));
    }

    #[test]
    fn a_short_audio_packet_is_unsupported() {
        let mut b = vec![0xC0, 0x03];
        b.extend_from_slice(&[0u8; 100]);
        assert_eq!(classify(&b), Datagram::Unsupported);
    }

    #[test]
    fn speex_payload_type_is_unsupported() {
        let mut b = AudioPacket {
            seq: 1,
            frames: [[0; 33]; 4],
        }
        .encode();
        b[1] = 0x96;
        assert_eq!(classify(&b), Datagram::Unsupported);
    }

    #[test]
    fn info_datagram_is_ondata_cr_then_text_with_newlines_as_cr() {
        assert_eq!(info_datagram("Denver\nCO"), b"oNDATA\rDenver\rCO");
    }

    #[test]
    fn chat_datagram_is_ondata_call_gt_text_crlf() {
        assert_eq!(chat_datagram("KX7ABC", "hi"), b"oNDATAKX7ABC>hi\r\n");
    }

    #[test]
    fn classify_parses_info_and_chat() {
        assert_eq!(
            classify(b"oNDATA\rDenver\rCO"),
            Datagram::Info("Denver\nCO".to_string())
        );
        assert_eq!(
            classify(b"oNDATAKX7ABC>hi\r\n"),
            Datagram::Chat {
                from: "KX7ABC".into(),
                text: "hi".into()
            }
        );
        assert_eq!(classify(b"oNDATAno separator"), Datagram::Unsupported);
        assert_eq!(classify(b""), Datagram::Unsupported);
        assert_eq!(classify(b"hello"), Datagram::Unsupported);
    }

    #[test]
    fn hostile_text_is_lossy_and_truncated_to_512_bytes() {
        let mut b = b"oNDATA\r".to_vec();
        b.extend_from_slice(&[0xFF; 2000]);
        let Datagram::Info(s) = classify(&b) else {
            panic!("not info")
        };
        assert!(s.len() <= MAX_TEXT_BYTES);
        assert!(s.contains('\u{FFFD}'));

        let mut c = b"oNDATAKX7ABC>".to_vec();
        c.extend(std::iter::repeat_n(b'a', 64 * 1024));
        let Datagram::Chat { text, .. } = classify(&c) else {
            panic!("not chat")
        };
        assert_eq!(text.len(), MAX_TEXT_BYTES);

        assert!(info_datagram(&"a".repeat(64 * 1024)).len() <= ONDATA.len() + 1 + MAX_TEXT_BYTES);
    }

    #[test]
    fn seq_window_wraps() {
        assert!(seq_is_newer(0, 65535));
        assert!(!seq_is_newer(65535, 0));
        assert!(!seq_is_newer(7, 7));
        assert!(seq_is_newer(8, 7));
    }

    #[test]
    fn clean_text_never_splits_a_multi_byte_character() {
        // 300 two-byte characters: byte 512 falls on a boundary, byte 511 does not.
        let input = "é".repeat(300);
        assert_eq!(input.len(), 600);
        let out = clean_text(input.as_bytes());
        assert!(out.len() <= MAX_TEXT_BYTES);
        assert_eq!(out.len() % 2, 0);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.chars().all(|c| c == 'é'));
        // Shifted by one ASCII byte, the cut lands mid-character and must step back.
        let shifted = format!("a{input}");
        let out = clean_text(shifted.as_bytes());
        assert_eq!(out.len(), 511);
        assert!(out[1..].chars().all(|c| c == 'é'));
    }

    fn audio_wire() -> Vec<u8> {
        AudioPacket {
            seq: 9,
            frames: [[0x5a; GSM_FRAME_LEN]; FRAMES_PER_PACKET],
        }
        .encode()
        .to_vec()
    }

    fn non_media_wires() -> Vec<Vec<u8>> {
        let mut speex = audio_wire();
        speex[1] = 0x96;
        let mut short = vec![0xC0, 0x03];
        short.extend_from_slice(&[0; 20]);
        let mut other_mark = audio_wire();
        other_mark[0] = 0x80;
        vec![
            other_mark,
            crate::rtcp::sdes("N0CALL", "Rob"),
            crate::rtcp::bye().to_vec(),
            info_datagram("hello"),
            chat_datagram("N0CALL", "hi"),
            short,
            speex,
        ]
    }

    #[test]
    fn is_media_accepts_voice() {
        assert!(is_media(&audio_wire()));
    }

    #[test]
    fn is_media_refuses_every_non_media_packet() {
        for w in non_media_wires() {
            assert!(!is_media(&w), "{w:02x?}");
        }
    }

    #[test]
    fn is_media_agrees_with_classify() {
        let mut all = non_media_wires();
        all.push(audio_wire());
        for w in all {
            assert_eq!(
                is_media(&w),
                matches!(classify(&w), Datagram::Audio(_)),
                "{w:02x?}"
            );
        }
    }

    #[test]
    fn is_media_never_panics_on_short_input() {
        for len in 0..=64 {
            for fill in [0x00u8, 0xff] {
                let _ = is_media(&vec![fill; len]);
            }
        }
    }
}
