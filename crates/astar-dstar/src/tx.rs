// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! TX-side D-Star stream framing (research §3, §6): turns "key, transmit
//! these AMBE frames as CALLSIGN to reflector module M, unkey" into the
//! exact `DsvtPacket` sequence a reflector expects.
//!
//! Pure and no-I/O, like the rest of this crate's protocol modules
//! (`dsvt`/`header`/`slowdata`/`fsm`): no socket, no clock, no encoder.
//! Callers own pacing (send one [`TxStream::next_voice_frame`] every real
//! 20ms), the AMBE encoder (`ThumbDV` or otherwise), and the UDP socket.
//! [`DsvtPacket::encode`]'s doc comment already called out this framing as
//! reusable for TX (dsvt.rs) — this module is what actually drives it: a
//! small sequence counter plus the field-value/slow-data policy research
//! §3/§6 specify for a transmitter.
//!
//! ## What this module does NOT decide
//!
//! [`TxStream::header_packet`] takes a caller-supplied [`RfHeader`]
//! verbatim — this layer has no opinion on RPT1 ("local node") policy for
//! astar's no-physical-repeater case; that's a session/config-layer
//! decision left to whoever wires PTT up to this framing (see
//! [`general_call_header`] for the one convenience shortcut this module
//! does provide, for the common "general call" case research §4
//! describes).

use crate::dsvt::DsvtPacket;
use crate::header::RfHeader;
use crate::slowdata::slow_data_slot;

/// `UR` field value for a general ("CQCQCQ") call — research §4: the
/// standard target for general chat, used by every real client when not
/// addressing one specific station.
pub const CQCQCQ: [u8; 8] = *b"CQCQCQ  ";

/// D-Star's null (silence) AMBE codeword: the 9 voice bytes every reference
/// implementation transmits when it has no encoded audio to send —
/// `MMDVMHost`'s `DSTAR_NULL_FRAME_SYNC_BYTES` minus its trailing 3 slow-data
/// bytes (`55 2D 16`, which this crate already carries as
/// [`crate::slowdata::SYNC_PATTERN`]).
///
/// An all-zero payload is NOT silence to an AMBE decoder: its voice
/// parameters are all zero rather than the recognized null codeword, which
/// typically renders as a short click/noise burst. Use this wherever a
/// filler/substitute frame is needed (the bare terminating frame of a keying
/// with no captured audio, or a vocoder substitution for a lost response).
pub const NULL_AMBE: [u8; 9] = [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8];

/// Builds an [`RfHeader`] for the common "general call" case: flags all
/// zero (matches both this crate's existing test fixtures and the real
/// captured XLX header, research §8), `UR` fixed to [`CQCQCQ`], MY suffix
/// blank. `rpt2` (destination reflector + module, e.g. `*b"XRF757 A"`),
/// `rpt1` (this station's own "local node" identity — see module docs),
/// and `my` (the operator's own callsign) are already-formatted 8-byte
/// ASCII wire fields; the plain [`RfHeader`] struct literal remains
/// available directly for anything this shortcut doesn't cover (directed
/// calls, a non-blank suffix, non-zero flags).
#[must_use]
pub fn general_call_header(rpt2: [u8; 8], rpt1: [u8; 8], my: [u8; 8]) -> RfHeader {
    RfHeader {
        flags: [0x00, 0x00, 0x00],
        rpt2,
        rpt1,
        ur: CQCQCQ,
        my,
        suffix: *b"    ",
    }
}

/// The stream id used when the clock-derived one would be zero (or the clock
/// reports a time before the epoch). Any fixed non-zero value would do; this
/// one matches the pre-existing before-the-epoch fallback.
const FALLBACK_STREAM_ID: u16 = 0x5353;

/// The pure half of [`generate_stream_id`]: truncate a nanosecond field to 16
/// bits, mapping the ONE forbidden value (zero) onto [`FALLBACK_STREAM_ID`].
///
/// Zero is not merely unlucky, it is rejected on the wire: `xlxd`/`new-xlxd`'s
/// `CReflector::OpenStream` explicitly refuses to open a stream whose id is
/// null, so a transmission sent under id 0 is discarded — header and every
/// voice frame — with no error visible to the transmitting client. Plain
/// truncation hits it whenever `subsec_nanos()` is an exact multiple of
/// 65536: ~1 keying in 65536 on a nanosecond-resolution clock, and ~1 in 8192
/// on a microsecond-granularity one (where the reachable zeros are the
/// multiples of `8_192_000` ns).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
fn stream_id_from_nanos(nanos: u32) -> u16 {
    match nanos as u16 {
        0 => FALLBACK_STREAM_ID,
        id => id,
    }
}

/// Generates a fresh, guaranteed NON-ZERO 16-bit stream id from the low bits
/// of the current wall-clock time.
///
/// A stream id is a network correlation id, not a security token, so
/// nanosecond-resolution wall-clock entropy is more than sufficient; this
/// mirrors the idiom `astar-asl3`'s `resolve.rs` already uses for a DNS
/// query id. Falls back to a fixed constant if the clock somehow reports a
/// time before the epoch (practically unreachable on any real system, but
/// avoids ever unwrapping in a TX path), and — see
/// [`stream_id_from_nanos`] — never returns 0, which real reflectors reject.
/// Kept separate from [`TxStream::new`] (which takes the id as a plain
/// argument) specifically so callers — and every test in this module — can
/// inject a fixed id instead of depending on the wall clock.
#[must_use]
pub fn generate_stream_id() -> u16 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(FALLBACK_STREAM_ID, |d| {
            stream_id_from_nanos(d.subsec_nanos())
        })
}

/// Builds the `RPT1`/`RPT2` pair for a transmission addressed at
/// `reflector_callsign`'s module `module` (e.g. `("XRF757", b'A')` →
/// `rpt1 = "XRF757 G"`, `rpt2 = "XRF757 A"`).
///
/// Why these values: the one piece of real traffic this project owns — the
/// captured XLX458 header pinned as a fixture in `dsvt.rs` — carries
/// `RPT2 = "XRF458 A"` (destination reflector + module) and `RPT1 = "BM3104 B"`
/// (the departure repeater/hotspot), and `DroidStar`/`DUDE-Star` fill the same
/// two fields for a softclient with no physical repeater chain (reflector
/// callsign + module, reflector callsign + `G` for the gateway). It also
/// matters beyond cosmetics: `xlxd`'s `DPlus` path gates on
/// `IsValidModule(rpt2.GetModule())`, so a blank `RPT2` drops every REF-family
/// transmission at the reflector with no error at the client.
///
/// The callsign is uppercased, stripped of whitespace and truncated to 7
/// characters (byte 7 is always the module/`G` slot, matching the real
/// capture's `"XRF458 A"` shape).
#[must_use]
pub fn repeater_fields(reflector_callsign: &str, module: u8) -> ([u8; 8], [u8; 8]) {
    let mut base = [b' '; 8];
    for (i, c) in reflector_callsign
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .take(7)
        .enumerate()
    {
        base[i] = c.to_ascii_uppercase();
    }
    let mut rpt1 = base;
    rpt1[7] = b'G';
    let mut rpt2 = base;
    rpt2[7] = module.to_ascii_uppercase();
    (rpt1, rpt2)
}

/// Sequences one D-Star transmission: the header packet sent once on key,
/// then one [`DsvtPacket::Voice`] per 20ms frame, wrapping its sequence
/// counter at [`crate::dsvt::SYNC_INTERVAL`] and filling the slow-data slot
/// per research §6 ([`slow_data_slot`]).
///
/// Holds only the stream id and the next frame's sequence number — no
/// clock, no socket, no encoder state. A caller starts one of these per
/// transmission (one per key/unkey cycle); reusing a `TxStream` across
/// multiple keyings would replay the same stream id, which a real
/// transmitter must not do (get a fresh id from [`generate_stream_id`] and
/// build a new `TxStream` for each transmission instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxStream {
    stream_id: u16,
    seq: u8,
}

impl TxStream {
    /// Starts sequencing a new transmission under `stream_id`. Use
    /// [`generate_stream_id`] to obtain one in production (it never returns
    /// the one id real reflectors reject, see
    /// [`stream_id_from_nanos`]); tests pass a fixed literal for determinism.
    #[must_use]
    pub fn new(stream_id: u16) -> TxStream {
        debug_assert!(
            stream_id != 0,
            "stream id 0 is rejected outright by real reflectors (xlxd's OpenStream); use \
             generate_stream_id()"
        );
        TxStream { stream_id, seq: 0 }
    }

    /// This stream's id (the same value threaded through every packet this
    /// `TxStream` produces).
    #[must_use]
    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    /// The sequence number [`TxStream::next_voice_frame`] will assign to
    /// the *next* frame it builds (`0..SYNC_INTERVAL`).
    #[must_use]
    pub fn next_seq(&self) -> u8 {
        self.seq
    }

    /// Builds the RF-header ("configuration") packet: send this exactly
    /// once, first, on key — before any voice frame. Does not touch this
    /// `TxStream`'s sequence counter, which starts (and, after a full
    /// superframe, wraps back to) 0 regardless of when the header is sent.
    #[must_use]
    pub fn header_packet(&self, header: RfHeader) -> DsvtPacket {
        DsvtPacket::Header {
            stream_id: self.stream_id,
            header,
        }
    }

    /// Builds the next voice frame carrying `ambe` (9 bytes from the
    /// encoder, opaque per research §5 — copied through verbatim). Advances
    /// the internal sequence counter, wrapping `0..SYNC_INTERVAL`, and
    /// fills the slow-data slot: [`slow_data_slot`] emits the fixed sync
    /// pattern on the first frame of every superframe, scrambled null
    /// filler otherwise.
    ///
    /// Pass `end = true` on exactly the transmission's last frame — this
    /// sets the `0x40` end-of-stream bit (research §3). D-Star's DSVT
    /// framing has no separate empty terminator packet (unlike `DPlus`'s
    /// truncated-AMBE tail convention, out of scope here): the EOT bit
    /// rides on the last real voice frame, so a caller unkeying mid-frame
    /// must still route its final chunk of audio (or a caller-supplied
    /// silence/filler AMBE frame, if none is available) through this method
    /// with `end = true` rather than simply stopping — see
    /// [`TxStream::unkey`] for that common case.
    #[must_use]
    pub fn next_voice_frame(&mut self, ambe: [u8; 9], end: bool) -> DsvtPacket {
        let seq = self.seq;
        let slow_data = slow_data_slot(seq);
        self.seq = (self.seq + 1) % crate::dsvt::SYNC_INTERVAL;
        DsvtPacket::Voice {
            stream_id: self.stream_id,
            seq,
            end,
            ambe,
            slow_data,
        }
    }

    /// Builds the transmission's terminating frame: exactly
    /// [`TxStream::next_voice_frame`] with `end = true`. Call this on
    /// unkey with whatever AMBE payload covers the last 20ms of audio (or
    /// an all-zero/silence AMBE frame if the encoder has nothing left) —
    /// unkey must always emit a frame with the EOT bit set so a receiver
    /// doesn't consider the stream still open (see this crate/project's
    /// on-air-safety convention: an unterminated stream is a defect, not
    /// just an inconvenience).
    #[must_use]
    pub fn unkey(&mut self, ambe: [u8; 9]) -> DsvtPacket {
        self.next_voice_frame(ambe, true)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::dsvt::SYNC_INTERVAL;
    use crate::slowdata::{SCRAMBLE, SYNC_PATTERN};

    fn ambe(fill: u8) -> [u8; 9] {
        [fill; 9]
    }

    fn sample_header() -> RfHeader {
        general_call_header(*b"XRF757 A", *b"AJ7HR   ", *b"AJ7HR   ")
    }

    #[test]
    fn general_call_header_fills_documented_fields() {
        let h = general_call_header(*b"XRF757 A", *b"AJ7HR   ", *b"W7VRY   ");
        assert_eq!(h.flags, [0x00, 0x00, 0x00]);
        assert_eq!(&h.rpt2, b"XRF757 A");
        assert_eq!(&h.rpt1, b"AJ7HR   ");
        assert_eq!(&h.ur, b"CQCQCQ  ");
        assert_eq!(&h.my, b"W7VRY   ");
        assert_eq!(&h.suffix, b"    ");
    }

    #[test]
    fn header_packet_carries_a_correct_crc_and_round_trips_strictly() {
        let stream = TxStream::new(0x1234);
        let header = sample_header();
        let packet = stream.header_packet(header);
        let DsvtPacket::Header {
            stream_id,
            header: packed_header,
        } = packet
        else {
            panic!("expected a header packet");
        };
        assert_eq!(stream_id, 0x1234);
        assert_eq!(packed_header, header);

        // Round-trip through the wire encoding, and decode STRICTLY (not
        // _lenient): research §8 pins strict CRC verification as the path
        // for locally generated/TX headers, precisely because a
        // transmitter (unlike a reflector) must compute a correct one.
        let encoded = packet.encode();
        let parsed = DsvtPacket::parse(&encoded).expect("a TX header must parse");
        assert_eq!(parsed, packet);
        let DsvtPacket::Header { header: wire, .. } = parsed else {
            unreachable!()
        };
        // decode_lenient round-trips the exact bytes; the real assertion
        // that matters is that a strict decode of the same 41 raw bytes
        // succeeds (the CRC is actually correct, not just present).
        let raw = wire.encode();
        RfHeader::decode(&raw).expect("TX-emitted header must have a correct CRC");
    }

    #[test]
    fn header_packet_does_not_advance_the_sequence_counter() {
        let stream = TxStream::new(1);
        assert_eq!(stream.next_seq(), 0);
        let _ = stream.header_packet(sample_header());
        assert_eq!(stream.next_seq(), 0);
    }

    #[test]
    fn first_voice_frame_is_seq_zero_and_carries_the_sync_pattern() {
        let mut stream = TxStream::new(42);
        let packet = stream.next_voice_frame(ambe(0xAB), false);
        assert_eq!(
            packet,
            DsvtPacket::Voice {
                stream_id: 42,
                seq: 0,
                end: false,
                ambe: ambe(0xAB),
                slow_data: SYNC_PATTERN,
            }
        );
        assert_eq!(stream.next_seq(), 1);
    }

    #[test]
    fn sequence_wraps_at_sync_interval_and_stream_id_is_stable() {
        let mut stream = TxStream::new(0xBEEF);
        // Drive through more than two full superframes.
        let total = usize::from(SYNC_INTERVAL) * 2 + 5;
        let mut seqs = Vec::with_capacity(total);
        for i in 0..total {
            let packet = stream.next_voice_frame(ambe(i as u8), false);
            let DsvtPacket::Voice { stream_id, seq, .. } = packet else {
                panic!("expected a voice packet");
            };
            assert_eq!(stream_id, 0xBEEF, "stream id must never change mid-stream");
            seqs.push(seq);
        }
        let expected: Vec<u8> = (0..total).map(|i| (i % 21) as u8).collect();
        assert_eq!(seqs, expected);
    }

    #[test]
    fn sync_pattern_appears_exactly_at_every_superframe_boundary() {
        let mut stream = TxStream::new(7);
        let total = usize::from(SYNC_INTERVAL) * 3;
        for i in 0..total {
            let packet = stream.next_voice_frame(ambe(0), false);
            let DsvtPacket::Voice { seq, slow_data, .. } = packet else {
                panic!("expected a voice packet");
            };
            if i % 21 == 0 {
                assert_eq!(seq, 0);
                assert_eq!(slow_data, SYNC_PATTERN, "frame {i} should be sync");
            } else {
                assert_ne!(slow_data, SYNC_PATTERN, "frame {i} should not be sync");
            }
        }
    }

    #[test]
    fn non_sync_frames_carry_the_scrambled_null_filler() {
        let mut stream = TxStream::new(1);
        let _ = stream.next_voice_frame(ambe(0), false); // consume seq 0 (sync)
        for expected_seq in 1..SYNC_INTERVAL {
            let packet = stream.next_voice_frame(ambe(0), false);
            let DsvtPacket::Voice { seq, slow_data, .. } = packet else {
                panic!("expected a voice packet");
            };
            assert_eq!(seq, expected_seq);
            // Scrambled all-zero filler is exactly SCRAMBLE (XOR against 0).
            assert_eq!(slow_data, SCRAMBLE, "seq {expected_seq}");
        }
    }

    #[test]
    fn end_of_stream_bit_is_set_only_on_the_unkey_frame() {
        let mut stream = TxStream::new(99);
        for _ in 0..5 {
            let packet = stream.next_voice_frame(ambe(0), false);
            let DsvtPacket::Voice { end, .. } = packet else {
                panic!("expected a voice packet");
            };
            assert!(!end);
        }
        let last = stream.unkey(ambe(0xFF));
        let DsvtPacket::Voice {
            end, seq, ambe: a, ..
        } = last
        else {
            panic!("expected a voice packet");
        };
        assert!(end);
        assert_eq!(seq, 5); // sixth frame overall
        assert_eq!(a, ambe(0xFF));

        // Confirm the EOT bit actually lands on the wire (offset 14, 0x40).
        let encoded = last.encode();
        assert_eq!(encoded[14], 5 | 0x40);
    }

    #[test]
    fn unkey_across_a_superframe_boundary_sets_eot_and_correct_seq() {
        // Key, run a full superframe plus a few frames, then unkey — the
        // exact off-by-one spot where SYNC_INTERVAL wraparound bugs hide.
        let mut stream = TxStream::new(5);
        for _ in 0..(usize::from(SYNC_INTERVAL) + 3) {
            let _ = stream.next_voice_frame(ambe(0), false);
        }
        assert_eq!(stream.next_seq(), 3);
        let last = stream.unkey(ambe(1));
        let DsvtPacket::Voice { seq, end, .. } = last else {
            panic!("expected a voice packet");
        };
        assert_eq!(seq, 3);
        assert!(end);
    }

    #[test]
    fn a_full_transmission_round_trips_through_dsvt_parse() {
        // End-to-end: key, one full superframe of voice, unkey — parse
        // every emitted packet back and check the invariants a receiver
        // would actually rely on.
        let stream_id = 0x0102;
        let mut stream = TxStream::new(stream_id);
        let header = sample_header();

        let header_packet = stream.header_packet(header);
        let header_encoded = header_packet.encode();
        let parsed_header = DsvtPacket::parse(&header_encoded).expect("header must parse");
        assert_eq!(parsed_header, header_packet);

        let mut voice_packets = Vec::new();
        for i in 0..usize::from(SYNC_INTERVAL) {
            let end = i == usize::from(SYNC_INTERVAL) - 1;
            let packet = if end {
                stream.unkey(ambe(i as u8))
            } else {
                stream.next_voice_frame(ambe(i as u8), false)
            };
            let encoded = packet.encode();
            let parsed = DsvtPacket::parse(&encoded).expect("voice frame must parse");
            assert_eq!(parsed, packet);
            voice_packets.push(parsed);
        }

        // Every voice packet shares the header's stream id.
        for (i, packet) in voice_packets.iter().enumerate() {
            let DsvtPacket::Voice {
                stream_id: sid,
                seq,
                end,
                ..
            } = *packet
            else {
                panic!("expected a voice packet");
            };
            assert_eq!(sid, stream_id);
            assert_eq!(seq, i as u8);
            assert_eq!(end, i == usize::from(SYNC_INTERVAL) - 1);
        }
    }

    #[test]
    fn generate_stream_id_does_not_panic_and_returns_a_value() {
        // Can't assert a fixed value (real wall clock), but this exercises
        // the code path and confirms it's usable directly as a TxStream id.
        let id = generate_stream_id();
        assert_ne!(id, 0, "a generated stream id must never be zero");
        let mut stream = TxStream::new(id);
        assert_eq!(stream.stream_id(), id);
        let _ = stream.next_voice_frame(ambe(0), false);
    }

    /// Regression: plain `subsec_nanos() as u16` returns 0 whenever the
    /// nanosecond field is an exact multiple of 65536, and a stream id of 0
    /// is refused outright by xlxd's `CReflector::OpenStream` — the whole
    /// transmission is discarded at the reflector with no error visible to
    /// the operator. Every nanosecond value that truncates to zero must be
    /// mapped away.
    #[test]
    fn a_stream_id_is_never_zero_for_any_nanosecond_value() {
        assert_ne!(stream_id_from_nanos(0), 0);
        assert_ne!(stream_id_from_nanos(65_536), 0);
        // The microsecond-granularity clock case: multiples of 8_192_000 ns.
        assert_ne!(stream_id_from_nanos(8_192_000), 0);
        assert_ne!(stream_id_from_nanos(999_997_440), 0);
        // Exhaustive over the whole 16-bit truncation space.
        for low in 0..=u32::from(u16::MAX) {
            assert_ne!(stream_id_from_nanos(low), 0, "nanos = {low}");
        }
        // Non-zero values are passed through untouched (entropy preserved).
        assert_eq!(stream_id_from_nanos(0x1234), 0x1234);
        assert_eq!(stream_id_from_nanos(65_536 + 7), 7);
    }

    #[test]
    fn repeater_fields_name_the_destination_reflector_and_module() {
        let (rpt1, rpt2) = repeater_fields("XRF757", b'A');
        // Matches the real captured XLX458 header's shape (`"XRF458 A"`):
        // callsign left-aligned, module in byte 7.
        assert_eq!(&rpt2, b"XRF757 A");
        assert_eq!(&rpt1, b"XRF757 G");
        assert_eq!(rpt2[7], b'A', "xlxd gates REF traffic on RPT2's module");
    }

    #[test]
    fn repeater_fields_normalize_case_padding_and_overlong_callsigns() {
        let (rpt1, rpt2) = repeater_fields("xlx458", b'b');
        assert_eq!(&rpt2, b"XLX458 B");
        assert_eq!(&rpt1, b"XLX458 G");
        // Whitespace stripped, and never more than 7 callsign characters so
        // byte 7 always stays the module slot.
        let (_, rpt2) = repeater_fields(" xrf 757 ", b'C');
        assert_eq!(&rpt2, b"XRF757 C");
        let (rpt1, rpt2) = repeater_fields("ABCDEFGHIJ", b'D');
        assert_eq!(&rpt2, b"ABCDEFGD");
        assert_eq!(&rpt1, b"ABCDEFGG");
    }

    #[test]
    fn null_ambe_is_the_reference_implementations_silence_codeword() {
        // MMDVMHost's DSTAR_NULL_FRAME_SYNC_BYTES, minus its trailing
        // slow-data sync bytes (which live in `slowdata::SYNC_PATTERN`).
        assert_eq!(
            NULL_AMBE,
            [0x9E, 0x8D, 0x32, 0x88, 0x26, 0x1A, 0x3F, 0x61, 0xE8]
        );
        assert_ne!(NULL_AMBE, [0u8; 9], "all-zero is NOT D-Star's null frame");
    }

    #[test]
    fn distinct_tx_streams_can_use_distinct_injected_ids() {
        // The "injectable so tests are deterministic" requirement: two
        // fixed, caller-chosen ids produce independently sequenced,
        // non-colliding streams.
        let mut a = TxStream::new(1);
        let mut b = TxStream::new(2);
        let pa = a.next_voice_frame(ambe(0), false);
        let pb = b.next_voice_frame(ambe(0), false);
        let DsvtPacket::Voice { stream_id: sa, .. } = pa else {
            unreachable!()
        };
        let DsvtPacket::Voice { stream_id: sb, .. } = pb else {
            unreachable!()
        };
        assert_ne!(sa, sb);
    }
}
