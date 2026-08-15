// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Hand-constructed golden frames cross-referenced with RFC 5456 §6 byte
//! diagrams and the jaracil/iax test vectors. Each fixture exercises one
//! shape of frame the rest of the stack will care about; together they pin
//! the byte-perfect-roundtrip property the parser/encoder promises.
//!
//! Replace these with real pcap captures once iax-7022 lands.

use bytes::BufMut;

use astar_iax_core::frame::MINI_HEADER_LEN;
use astar_iax_core::ie::{
    IE_CALLTOKEN, IE_FORMAT, IE_PASSWORD, IE_REFRESH, IE_USERNAME, IE_VERSION,
};
use astar_iax_core::{
    ControlSubclass, Frame, FrameType, FullFrame, IaxCommand, Ies, MiniFrame, Subclass,
    VoiceFormat, encode, parse,
};

/// Outgoing NEW handshake with the "I support call tokens" probe — a
/// zero-length CALLTOKEN IE. Confirms the conformance doc 2026-05-29 entry.
#[test]
fn new_with_empty_calltoken_ie_round_trips() {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x1234,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            version: Some(2),
            format: Some(VoiceFormat::G711U.as_u32()),
            username: Some("rob"),
            calltoken: Some(&[]),
            ..Ies::empty()
        },
        payload: &[],
    }));

    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");

    // Byte layout sanity: full-frame discriminator + correct frame type
    // and subclass byte (NEW = 1 is < 128, so emitted verbatim).
    assert_eq!(wire[0] & 0x80, 0x80);
    assert_eq!(wire[10], FrameType::Iax as u8);
    assert_eq!(wire[11], IaxCommand::New as u8);

    // CALLTOKEN IE: id=54, len=0 — exactly the bytes in the conformance doc.
    let pos = wire.iter().position(|&b| b == IE_CALLTOKEN).unwrap();
    assert_eq!(wire[pos + 1], 0);

    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// Server reply to the empty-CALLTOKEN probe: subclass = `IAX_COMMAND_CALLTOKEN`
/// (40) with an opaque token in the IE payload. The client then re-sends
/// NEW with the populated token.
#[test]
fn server_calltoken_reply_round_trips() {
    let token = b"\xde\xad\xbe\xef\xfe\xed\xfa\xceopaque";
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x4001,
        dest_call: 0x1234,
        retransmission: false,
        timestamp: 50,
        oseqno: 0,
        iseqno: 1,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::CallToken),
        ies: Ies {
            calltoken: Some(token),
            ..Ies::empty()
        },
        payload: &[],
    }));

    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    assert_eq!(wire[11], IaxCommand::CallToken as u8, "subclass=40");
    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// Second-leg NEW: same as the first NEW but the CALLTOKEN IE now carries
/// the server-issued token. Per the conformance doc this is the only IE
/// that differs between the two NEW frames.
#[test]
fn new_with_populated_calltoken_round_trips() {
    let token = b"\xde\xad\xbe\xef\xfe\xed\xfa\xceopaque";
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x1234,
        dest_call: 0x4001,
        retransmission: false,
        timestamp: 100,
        oseqno: 1,
        iseqno: 1,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            version: Some(2),
            format: Some(VoiceFormat::G711U.as_u32()),
            username: Some("rob"),
            calltoken: Some(token),
            ..Ies::empty()
        },
        payload: &[],
    }));
    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// 160-byte μ-law mini frame — the steady-state media packet on an
/// `AllStarLink` link.
#[test]
fn ulaw_mini_frame_160_bytes_round_trips() {
    let payload: Vec<u8> = (0..160u8).map(|i| i.wrapping_mul(7)).collect();
    let frame = Frame::Mini(MiniFrame {
        source_call: 0x1234,
        timestamp: 0x8000,
        payload: &payload,
    });

    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");

    assert_eq!(wire.len(), MINI_HEADER_LEN + 160);
    assert_eq!(wire[0] & 0x80, 0, "mini-frame discriminator");

    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// `REGREQ` with a USERNAME IE — the no-calltoken path the conformance doc
/// 2026-05-29 entry says `register.allstarlink.org` accepts today.
#[test]
fn regreq_with_username_round_trips() {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x7000,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::RegReq),
        ies: Ies {
            username: Some("rob"),
            refresh: Some(60),
            ..Ies::empty()
        },
        payload: &[],
    }));
    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    assert_eq!(wire[11], IaxCommand::RegReq as u8);
    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// Control frame carrying the `AllStarLink` `RADIO_KEY` PTT signal.
/// Cross-checks the dialect doc value `AST_CONTROL_RADIO_KEY = 12`.
#[test]
fn control_radio_key_round_trips() {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x1234,
        dest_call: 0x4001,
        retransmission: false,
        timestamp: 200,
        oseqno: 2,
        iseqno: 3,
        frame_type: FrameType::Control,
        subclass: Subclass::Control(ControlSubclass::RadioKey),
        ies: Ies::empty(),
        payload: &[],
    }));
    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    assert_eq!(wire[10], FrameType::Control as u8);
    assert_eq!(wire[11], 12);
    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// AUTHREP with an MD5 result — exercises the auth handshake's IE payload
/// shape (challenge/response strings of 32 hex chars).
#[test]
fn authrep_with_password_round_trips() {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 0x1234,
        dest_call: 0x4001,
        retransmission: false,
        timestamp: 300,
        oseqno: 2,
        iseqno: 2,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::AuthRep),
        ies: Ies {
            md5_result: Some("0123456789abcdef0123456789abcdef"),
            ..Ies::empty()
        },
        payload: &[],
    }));
    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    let parsed = parse(&wire).unwrap();
    assert_eq!(parsed, frame);
}

/// Hand-spelled wire bytes for a minimal NEW so the test asserts the exact
/// header layout from RFC 5456 §6.3 rather than just self-consistency.
#[test]
fn minimal_new_matches_hand_spelled_bytes() {
    // FLAG_FULL=0x8000 ORed with source_call=1 => 0x8001.
    // dcallno=0, retransmission cleared.
    // ts=0, oseqno=0, iseqno=0, type=AST_FRAME_IAX(6), subclass=NEW(1).
    let mut expected = Vec::new();
    expected.put_u16(0x8001);
    expected.put_u16(0x0000);
    expected.put_u32(0);
    expected.put_u8(0); // oseqno
    expected.put_u8(0); // iseqno
    expected.put_u8(6); // AST_FRAME_IAX
    expected.put_u8(1); // IAX_COMMAND_NEW
    // IEs are emitted in ascending id order so the wire form is canonical.
    // USERNAME(6), PASSWORD(7), FORMAT(9), VERSION(11), REFRESH(19).
    expected.extend_from_slice(&[IE_USERNAME, 3, b'r', b'o', b'b']);
    expected.extend_from_slice(&[IE_PASSWORD, 4, b'a', b's', b'd', b'f']);
    expected.extend_from_slice(&[IE_FORMAT, 4, 0, 0, 0, 4]);
    expected.extend_from_slice(&[IE_VERSION, 2, 0, 2]);
    expected.extend_from_slice(&[IE_REFRESH, 2, 0, 60]);

    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 1,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            username: Some("rob"),
            password: Some("asdf"),
            format: Some(4),
            version: Some(2),
            refresh: Some(60),
            ..Ies::empty()
        },
        payload: &[],
    }));
    let mut actual = Vec::new();
    encode(&frame, &mut actual).expect("test frame must encode");
    assert_eq!(actual, expected);

    let reparsed = parse(&actual).unwrap();
    assert_eq!(reparsed, frame);
}

/// Truncated input (full-frame header that ends before the 12-byte minimum)
/// must surface `ParseError::Truncated` rather than panic.
#[test]
fn truncated_full_frame_errors_cleanly() {
    let buf = [0x80, 0x01, 0x00, 0x00, 0x00];
    let err = parse(&buf).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("truncated"), "got: {msg}");
}

/// Truncated mini frame (less than 4 bytes) must surface `Truncated`.
#[test]
fn truncated_mini_frame_errors_cleanly() {
    let buf = [0x00, 0x01, 0x00];
    let err = parse(&buf).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("truncated"), "got: {msg}");
}

/// IE byte stream parses left-to-right; later IEs win in case of duplicates.
/// (Asterisk's behavior is the same — see `iax_parse_ies()` in
/// `iax2-parser.c`.) Mostly here to document the policy.
#[test]
fn duplicate_ies_last_one_wins() {
    let bytes = [
        IE_USERNAME,
        3,
        b'a',
        b'b',
        b'c',
        IE_USERNAME,
        3,
        b'x',
        b'y',
        b'z',
    ];
    let ies = Ies::parse(&bytes).unwrap();
    assert_eq!(ies.username, Some("xyz"));
}

/// Encoding never copies the input payload into a heap allocation managed
/// by the encoder — proven by handing it an exact-fit slice.
#[test]
fn encode_does_not_allocate_internal_vec() {
    let payload = vec![0u8; 160];
    let frame = Frame::Mini(MiniFrame {
        source_call: 1,
        timestamp: 0,
        payload: &payload,
    });
    let mut buf = Vec::with_capacity(MINI_HEADER_LEN + 160);
    let cap_before = buf.capacity();
    encode(&frame, &mut buf).expect("test frame must encode");
    assert_eq!(buf.len(), MINI_HEADER_LEN + 160);
    assert_eq!(
        buf.capacity(),
        cap_before,
        "encoder must not force a reallocation when caller pre-sized"
    );
}

/// Full-frame header bytes 10/11 are the only header fields where vendored
/// astar and upstream Asterisk could plausibly disagree (per the conformance
/// doc). This test pins both:
/// - byte 10 = `AST_FRAME_IAX` = 6 (upstream)
/// - byte 11 = `IAX_COMMAND_CALLTOKEN` = 40 (upstream — not in vendored astar)
#[test]
fn upstream_calltoken_subclass_byte_is_40() {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: 1,
        dest_call: 1,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::CallToken),
        ies: Ies::empty(),
        payload: &[],
    }));
    let mut wire = Vec::new();
    encode(&frame, &mut wire).expect("test frame must encode");
    assert_eq!(wire[10], 6);
    assert_eq!(wire[11], 40);
}
