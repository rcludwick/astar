// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Property: `parse(encode(frame)) == frame` for every representable frame.
//!
//! Generators here cover the realistic shapes — full frames with arbitrary
//! IE combinations, mini frames with arbitrary payloads, and subclasses in
//! both the < 128 (verbatim) and >= 128 (compressed-power-of-two) ranges.
//! Shrinking is left to the proptest defaults.

use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

use astar_iax_core::{
    ControlSubclass, Frame, FrameType, FullFrame, IaxCommand, Ies, MiniFrame, Subclass,
    VoiceFormat, encode, parse,
};

fn iax_command() -> impl Strategy<Value = IaxCommand> {
    prop_oneof![
        Just(IaxCommand::New),
        Just(IaxCommand::Ping),
        Just(IaxCommand::Pong),
        Just(IaxCommand::Ack),
        Just(IaxCommand::Hangup),
        Just(IaxCommand::Reject),
        Just(IaxCommand::Accept),
        Just(IaxCommand::AuthReq),
        Just(IaxCommand::AuthRep),
        Just(IaxCommand::RegReq),
        Just(IaxCommand::RegAuth),
        Just(IaxCommand::RegAck),
        Just(IaxCommand::CallToken),
    ]
}

fn control_subclass() -> impl Strategy<Value = ControlSubclass> {
    prop_oneof![
        Just(ControlSubclass::Hangup),
        Just(ControlSubclass::Ring),
        Just(ControlSubclass::Ringing),
        Just(ControlSubclass::Answer),
        Just(ControlSubclass::Busy),
        Just(ControlSubclass::Progress),
        // PTT signals — included specifically so shrinking finds the
        // AllStarLink-relevant codepath fast.
        Just(ControlSubclass::RadioKey),
        Just(ControlSubclass::RadioUnkey),
    ]
}

fn voice_format() -> impl Strategy<Value = VoiceFormat> {
    prop_oneof![
        Just(VoiceFormat::G711U),
        Just(VoiceFormat::G711A),
        Just(VoiceFormat::Gsm),
        Just(VoiceFormat::Slin),
        // G722 = 1 << 12 = 4096; > 128 so the subclass byte is compressed.
        Just(VoiceFormat::G722),
        // Slin16 = 1 << 15 = 32768; also compressed.
        Just(VoiceFormat::Slin16),
    ]
}

fn subclass_strategy() -> impl Strategy<Value = (FrameType, Subclass)> {
    prop_oneof![
        iax_command().prop_map(|c| (FrameType::Iax, Subclass::Iax(c))),
        control_subclass().prop_map(|c| (FrameType::Control, Subclass::Control(c))),
        voice_format().prop_map(|c| (FrameType::Voice, Subclass::Voice(c))),
    ]
}

prop_compose! {
    fn arb_ies()(
        // Strings — bounded to keep the TLV length byte happy (< 256) and
        // ASCII to dodge UTF-8 normalisation games.
        username in option::of("[a-zA-Z0-9]{1,32}"),
        password in option::of("[a-zA-Z0-9]{1,32}"),
        called_number in option::of("[0-9*#]{1,32}"),
        challenge in option::of("[0-9a-f]{32}"),
        md5_result in option::of("[0-9a-f]{32}"),
        capability in option::of(any::<u32>()),
        format in option::of(any::<u32>()),
        version in option::of(any::<u16>()),
        refresh in option::of(any::<u16>()),
        authmethods in option::of(any::<u16>()),
        callno in option::of(any::<u16>()),
        causecode in option::of(any::<u8>()),
        autoanswer in any::<bool>(),
        calltoken in option::of(vec(any::<u8>(), 0..32)),
    ) -> OwnedIes {
        OwnedIes {
            username,
            password,
            called_number,
            challenge,
            md5_result,
            capability,
            format,
            version,
            refresh,
            authmethods,
            callno,
            causecode,
            autoanswer,
            calltoken,
        }
    }
}

#[derive(Debug, Clone)]
struct OwnedIes {
    username: Option<String>,
    password: Option<String>,
    called_number: Option<String>,
    challenge: Option<String>,
    md5_result: Option<String>,
    capability: Option<u32>,
    format: Option<u32>,
    version: Option<u16>,
    refresh: Option<u16>,
    authmethods: Option<u16>,
    callno: Option<u16>,
    causecode: Option<u8>,
    autoanswer: bool,
    calltoken: Option<Vec<u8>>,
}

impl OwnedIes {
    fn empty() -> Self {
        Self {
            username: None,
            password: None,
            called_number: None,
            challenge: None,
            md5_result: None,
            capability: None,
            format: None,
            version: None,
            refresh: None,
            authmethods: None,
            callno: None,
            causecode: None,
            autoanswer: false,
            calltoken: None,
        }
    }

    fn as_ies(&self) -> Ies<'_> {
        Ies {
            username: self.username.as_deref(),
            password: self.password.as_deref(),
            called_number: self.called_number.as_deref(),
            challenge: self.challenge.as_deref(),
            md5_result: self.md5_result.as_deref(),
            capability: self.capability,
            format: self.format,
            version: self.version,
            refresh: self.refresh,
            authmethods: self.authmethods,
            callno: self.callno,
            causecode: self.causecode,
            autoanswer: self.autoanswer,
            calltoken: self.calltoken.as_deref(),
            ..Ies::empty()
        }
    }
}

#[derive(Debug, Clone)]
struct OwnedFullFixture {
    source_call: u16,
    dest_call: u16,
    retransmission: bool,
    timestamp: u32,
    oseqno: u8,
    iseqno: u8,
    frame_type: FrameType,
    subclass: Subclass,
    // Per RFC 5456 §6.4 exactly one of these is populated per frame type:
    // media frames (Voice/Video/Image) carry samples; everything else
    // carries IEs. The generator below honours that split so round-trip
    // holds.
    ies: OwnedIes,
    payload: Vec<u8>,
}

fn arb_full() -> impl Strategy<Value = OwnedFullFixture> {
    (
        any::<u16>(),
        any::<u16>(),
        any::<bool>(),
        any::<u32>(),
        any::<u8>(),
        any::<u8>(),
        subclass_strategy(),
        arb_ies(),
        vec(any::<u8>(), 0..200),
    )
        .prop_map(|(src, dst, rtx, ts, os, is, (ft, sc), ies, payload)| {
            // RFC 5456 §6.4: media frame types carry codec samples;
            // every other type carries an IE TLV stream. Zero out the
            // unused side so the fixture matches what the parser will
            // produce on the round trip.
            let carries_pl = matches!(ft, FrameType::Voice | FrameType::Video | FrameType::Image);
            let (ies, payload) = if carries_pl {
                (OwnedIes::empty(), payload)
            } else {
                (ies, Vec::new())
            };
            OwnedFullFixture {
                source_call: src & 0x7fff,
                dest_call: dst & 0x7fff,
                retransmission: rtx,
                timestamp: ts,
                oseqno: os,
                iseqno: is,
                frame_type: ft,
                subclass: sc,
                ies,
                payload,
            }
        })
}

prop_compose! {
    fn arb_mini()(
        source_call in any::<u16>(),
        timestamp in any::<u16>(),
        payload in vec(any::<u8>(), 0..400),
    ) -> (u16, u16, Vec<u8>) {
        (source_call & 0x7fff, timestamp, payload)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn full_frame_round_trips(fixture in arb_full()) {
        let frame = Frame::Full(Box::new(FullFrame {
            source_call: fixture.source_call,
            dest_call: fixture.dest_call,
            retransmission: fixture.retransmission,
            timestamp: fixture.timestamp,
            oseqno: fixture.oseqno,
            iseqno: fixture.iseqno,
            frame_type: fixture.frame_type,
            subclass: fixture.subclass,
            ies: fixture.ies.as_ies(),
            payload: &fixture.payload,
        }));
        let mut wire = Vec::new();
        encode(&frame, &mut wire).expect("test frame must encode");
        let parsed = parse(&wire).expect("parse must succeed for any encoded frame");
        prop_assert_eq!(parsed, frame);
    }

    #[test]
    fn mini_frame_round_trips((src, ts, payload) in arb_mini()) {
        let frame = Frame::Mini(MiniFrame {
            source_call: src,
            timestamp: ts,
            payload: &payload,
        });
        let mut wire = Vec::new();
        encode(&frame, &mut wire).expect("test frame must encode");
        let parsed = parse(&wire).unwrap();
        prop_assert_eq!(parsed, frame);
    }

    #[test]
    fn double_encode_is_byte_identical(fixture in arb_full()) {
        // Re-encoding a parsed frame produces the same bytes as the
        // original encoding — i.e. parse/encode are inverses *and* the
        // encoder is deterministic.
        let frame = Frame::Full(Box::new(FullFrame {
            source_call: fixture.source_call,
            dest_call: fixture.dest_call,
            retransmission: fixture.retransmission,
            timestamp: fixture.timestamp,
            oseqno: fixture.oseqno,
            iseqno: fixture.iseqno,
            frame_type: fixture.frame_type,
            subclass: fixture.subclass,
            ies: fixture.ies.as_ies(),
            payload: &fixture.payload,
        }));
        let mut wire1 = Vec::new();
        encode(&frame, &mut wire1).expect("test frame must encode");
        let parsed = parse(&wire1).unwrap();
        let mut wire2 = Vec::new();
        encode(&parsed, &mut wire2).expect("test frame must encode");
        prop_assert_eq!(wire1, wire2);
    }
}
