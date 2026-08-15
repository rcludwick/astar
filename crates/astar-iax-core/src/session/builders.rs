// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Outbound IAX2 frame constructors shared by the per-state handlers.
//! Moved verbatim from fsm.rs (iax-19ad); no behavioural change.

use super::call_no::CallNo;
use super::call_profile::CallProfile;
use super::codec_policy::CodecPolicy;
use super::fsm::{CodecMask, truncate_to_codepoint_boundary};
use crate::frame::OwnedFullFrame;
use crate::subclass::VoiceFormat;

pub(super) fn build_authrep(our_call: CallNo, peer_call: CallNo, md5_hex: &str) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    let ies = Ies {
        md5_result: Some(md5_hex),
        ..Ies::empty()
    };
    // MD5 hex is always 32 ASCII characters; encoding is infallible.
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::AuthRep),
        &ies,
    )
    .expect("AUTHREP carries only a 32-char MD5 hex digest")
}

// The low 16 bits of `ts` are the wire format for mini-frame timestamps
// (the high 16 are inherited from the last full voice frame on this call).
#[allow(clippy::cast_possible_truncation)]
pub(super) fn build_mini_voice(our_call: CallNo, ts: u32, payload: &[u8]) -> Vec<u8> {
    use crate::frame::{Frame, MiniFrame};

    let mini = MiniFrame {
        source_call: our_call.value(),
        timestamp: ts as u16,
        payload,
    };
    let mut bytes = Vec::with_capacity(4 + payload.len());
    // Mini frames carry no IEs, so encoding is infallible.
    crate::frame::encode(&Frame::Mini(mini), &mut bytes).expect("mini frame has no IEs");
    bytes
}

pub(super) fn build_dtmf_begin(
    our_call: CallNo,
    peer_call: CallNo,
    digit: char,
    ts_ms: u32,
) -> OwnedFullFrame {
    build_dtmf(
        our_call,
        peer_call,
        digit,
        ts_ms,
        crate::subclass::FrameType::DtmfBegin,
    )
}

pub(super) fn build_dtmf_end(
    our_call: CallNo,
    peer_call: CallNo,
    digit: char,
    ts_ms: u32,
) -> OwnedFullFrame {
    build_dtmf(
        our_call,
        peer_call,
        digit,
        ts_ms,
        crate::subclass::FrameType::DtmfEnd,
    )
}

/// Empty-IE full-frame builder shared by BEGIN and END. The DTMF subclass
/// is the ASCII digit (RFC 5456 §6.7), carried as `Subclass::Dtmf`. The
/// digit is validated upstream by `is_valid_dtmf_digit` before send.
fn build_dtmf(
    our_call: CallNo,
    peer_call: CallNo,
    digit: char,
    ts_ms: u32,
    kind: crate::subclass::FrameType,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;

    debug_assert!(
        crate::subclass::is_dtmf_digit(digit),
        "DTMF digit validated by is_valid_dtmf_digit before build_dtmf"
    );
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        ts_ms,
        0, // oseqno assigned by Reliability when enqueued
        0, // iseqno assigned by Reliability when enqueued
        kind,
        Subclass::Dtmf(digit),
        &Ies::empty(),
    )
    .expect("DTMF frame carries no IEs")
}

/// PTT-down control frame (`AST_CONTROL_RADIO_KEY = 12`). `AllStarLink` PTT
/// signalling — see docs/spec/allstar-dialect.md §2. Full Control frame, no
/// IEs/payload; `Reliability::enqueue` stamps seqnos + call numbers.
pub(super) fn build_radio_key(our_call: CallNo, peer_call: CallNo) -> OwnedFullFrame {
    build_control(
        our_call,
        peer_call,
        crate::subclass::ControlSubclass::RadioKey,
    )
}

/// PTT-up control frame (`AST_CONTROL_RADIO_UNKEY = 13`). See `build_radio_key`.
pub(super) fn build_radio_unkey(our_call: CallNo, peer_call: CallNo) -> OwnedFullFrame {
    build_control(
        our_call,
        peer_call,
        crate::subclass::ControlSubclass::RadioUnkey,
    )
}

/// Empty-IE Control-frame builder shared by the `RADIO_KEY`/`UNKEY` signals.
fn build_control(
    our_call: CallNo,
    peer_call: CallNo,
    sub: crate::subclass::ControlSubclass,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::FrameType;

    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0, // timestamp 0 for control; Reliability::enqueue preserves it
        0, // oseqno assigned by Reliability when enqueued
        0, // iseqno assigned by Reliability when enqueued
        FrameType::Control,
        Subclass::Control(sub),
        &Ies::empty(),
    )
    .expect("control frame carries no IEs")
}

pub(super) fn build_hangup(
    our_call: CallNo,
    peer_call: CallNo,
    cause: Option<&str>,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    // `cause` is user/event-supplied; shorten by codepoint to stay
    // inside the 255-byte IE limit. iax-545b.
    let cause_trunc = cause.map(|c| truncate_to_codepoint_boundary(c, 255));
    let ies = Ies {
        cause: cause_trunc,
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Hangup),
        &ies,
    )
    .expect("cause was codepoint-truncated to ≤ 255 bytes")
}

/// Empty-IE IAX-frame builder shared by PING/PONG/LAGRQ/LAGRP (iax-a307).
/// `ts_ms` is ms-since-establishment for requests, or the request's echoed
/// timestamp for replies (RFC 5456 §6.7.2–§6.7.5). `Reliability::enqueue`
/// preserves the timestamp (it only stamps seqnos and call numbers).
fn build_iax_simple(
    our_call: CallNo,
    peer_call: CallNo,
    ts_ms: u32,
    cmd: crate::subclass::IaxCommand,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::FrameType;

    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        ts_ms,
        0, // oseqno assigned by Reliability when enqueued
        0, // iseqno assigned by Reliability when enqueued
        FrameType::Iax,
        Subclass::Iax(cmd),
        &Ies::empty(),
    )
    .expect("PING/PONG/LAGRQ/LAGRP carry no IEs")
}

pub(super) fn build_ping(our_call: CallNo, peer_call: CallNo, ts_ms: u32) -> OwnedFullFrame {
    build_iax_simple(
        our_call,
        peer_call,
        ts_ms,
        crate::subclass::IaxCommand::Ping,
    )
}

pub(super) fn build_pong(our_call: CallNo, peer_call: CallNo, ts_ms: u32) -> OwnedFullFrame {
    build_iax_simple(
        our_call,
        peer_call,
        ts_ms,
        crate::subclass::IaxCommand::Pong,
    )
}

pub(super) fn build_lagrq(our_call: CallNo, peer_call: CallNo, ts_ms: u32) -> OwnedFullFrame {
    build_iax_simple(
        our_call,
        peer_call,
        ts_ms,
        crate::subclass::IaxCommand::LagRq,
    )
}

pub(super) fn build_lagrp(our_call: CallNo, peer_call: CallNo, ts_ms: u32) -> OwnedFullFrame {
    build_iax_simple(
        our_call,
        peer_call,
        ts_ms,
        crate::subclass::IaxCommand::LagRp,
    )
}

/// Build a FULL Voice frame (RFC 5456 §6.4 / §8.x). The first outbound
/// voice frame of a call—and any frame where the codec or high-16-bits of the
/// timestamp change—MUST be a full frame so the peer can establish the context
/// that subsequent mini frames inherit. (iax-a116)
///
/// A full voice frame carries NO IEs; the audio bytes follow the 12-byte
/// header directly.
pub(super) fn build_full_voice(
    our_call: CallNo,
    peer_call: CallNo,
    format: VoiceFormat,
    ts: u32,
    payload: &[u8],
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::subclass::FrameType;

    // Voice frames carry raw codec samples, not IEs (carries_payload == true).
    // ie_bytes must be empty; payload holds the audio. Because OwnedFullFrame.ie_bytes
    // is pub(crate) we construct it directly here, inside the crate.
    OwnedFullFrame {
        source_call: our_call.value(),
        dest_call: peer_call.value(),
        retransmission: false,
        timestamp: ts,
        oseqno: 0, // stamped by Reliability::enqueue
        iseqno: 0, // stamped by Reliability::enqueue
        frame_type: FrameType::Voice,
        subclass: Subclass::Voice(format),
        ie_bytes: Vec::new(), // voice frames have no IEs
        payload: payload.to_vec(),
    }
}

/// Build a full TEXT frame ([`FrameType::Text`]) carrying `body` as the payload.
/// `app_rpt` link-control text (e.g. "!NEWKEY1!") rides on these. Reliable.
///
/// Text frames carry raw UTF-8 bytes directly after the 12-byte header (no
/// IEs). The subclass is `Raw(0)` — the wire subclass byte for a Text frame is
/// unused / zero (RFC 5456 §6.7; `app_rpt` does not assign it a meaning).
/// Mirrors `build_full_voice` in construction style.
pub(super) fn build_text(our_call: CallNo, peer_call: CallNo, body: &str) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::subclass::FrameType;

    OwnedFullFrame {
        source_call: our_call.value(),
        dest_call: peer_call.value(),
        retransmission: false,
        timestamp: 0,
        oseqno: 0, // stamped by Reliability::enqueue
        iseqno: 0, // stamped by Reliability::enqueue
        frame_type: FrameType::Text,
        subclass: Subclass::Raw(0),
        ie_bytes: Vec::new(), // text frames carry no IEs
        payload: body.as_bytes().to_vec(),
    }
}

/// Server-side AUTHREQ (RFC 5456 §6.2 / §8.14). Advertises MD5 only; the
/// `challenge` is hex-encoded `OsRng` bytes the inbound FSM generated and
/// stored. No RSA, no PLAINTEXT in AUTHMETHODS (design §Auth, RSA rejected).
pub(super) fn build_authreq(
    our_call: CallNo,
    peer_call: CallNo,
    challenge: &str,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};
    let challenge_trunc = truncate_to_codepoint_boundary(challenge, 255);
    let ies = Ies {
        authmethods: Some(super::auth::AuthMethods::MD5.bits()),
        challenge: Some(challenge_trunc),
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::AuthReq),
        &ies,
    )
    .expect("AUTHREQ challenge is hex and codepoint-truncated to <= 255 bytes")
}

/// ACCEPT carrying the chosen codec as a single FORMAT IE (design decision 2).
pub(super) fn build_accept(
    our_call: CallNo,
    peer_call: CallNo,
    format: VoiceFormat,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};
    let ies = Ies {
        format: Some(format.as_u32()),
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Accept),
        &ies,
    )
    .expect("ACCEPT carries a single FORMAT IE")
}

/// ANSWER is a Control(4) frame with no IEs (design decision 1, pcap frame 10).
pub(super) fn build_answer(our_call: CallNo, peer_call: CallNo) -> OwnedFullFrame {
    build_control(
        our_call,
        peer_call,
        crate::subclass::ControlSubclass::Answer,
    )
}

/// CALLTOKEN (subclass 40) carrying opaque per-call token bytes (RFC 5456 §8.6).
pub(super) fn build_calltoken(our_call: CallNo, peer_call: CallNo, token: &[u8]) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};
    debug_assert!(
        token.len() <= crate::ie::MAX_CALLTOKEN_LEN,
        "token fits IE limit"
    );
    let ies = Ies {
        calltoken: Some(token),
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::CallToken),
        &ies,
    )
    .expect("CALLTOKEN payload is bounded by OsRng length we control")
}

/// REJECT with an optional CAUSE IE (codepoint-truncated to the wire limit).
pub(super) fn build_reject(
    our_call: CallNo,
    peer_call: CallNo,
    cause: Option<&str>,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};
    let cause_trunc = cause.map(|c| truncate_to_codepoint_boundary(c, 255));
    let ies = Ies {
        cause: cause_trunc,
        ..Ies::empty()
    };
    OwnedFullFrame::with_ies(
        our_call.value(),
        peer_call.value(),
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Reject),
        &ies,
    )
    .expect("REJECT cause was codepoint-truncated to <= 255 bytes")
}

pub(super) fn build_new(
    our_call: CallNo,
    dest_call: u16,
    dest: &str,
    username: &str,
    capabilities: CodecMask,
    token: Option<&[u8]>,
    profile: &CallProfile,
) -> OwnedFullFrame {
    use crate::frame::Subclass;
    use crate::ie::Ies;
    use crate::subclass::{FrameType, IaxCommand};

    // `dest` and `username` come from caller config — shorten by codepoint
    // to stay inside the 255-byte IE limit. `token` originated in a peer's
    // `CALLTOKEN` IE so it's already wire-bounded; debug-assert anyway.
    let dest_trunc = truncate_to_codepoint_boundary(dest, 255);
    let username_trunc = truncate_to_codepoint_boundary(username, 255);
    // iax-3fca: `CALLING_*` IEs come from the profile (web-transceiver path);
    // `None` ⇒ the IE is omitted. Codepoint-truncate like the other strings.
    let calling_number = profile
        .calling_number
        .as_deref()
        .map(|s| truncate_to_codepoint_boundary(s, 255));
    let calling_name = profile
        .calling_name
        .as_deref()
        .map(|s| truncate_to_codepoint_boundary(s, 255));
    let token_bytes = token.unwrap_or(&[]);
    debug_assert!(
        token_bytes.len() <= 255,
        "CALLTOKEN from peer must fit a u8 length prefix"
    );
    let ies = Ies {
        called_number: Some(dest_trunc),
        calling_number,
        calling_name,
        username: Some(username_trunc),
        // iax-3fca: the web-transceiver guest path omits `CAPABILITY` — but
        // only while the advertised set is the legacy µ-law preference. With
        // no CAPABILITY IE the peer takes FORMAT alone as our whole
        // capability, so any richer policy (slin/slin16) would be
        // take-it-or-leave-it: nodes that don't allow the preferred format
        // REJECT "Unable to negotiate codec" instead of falling back
        // (iax-866f, live-verified vs HamVoIP node 55553). Any non-default
        // policy therefore ships the mask even in the WT shape.
        capability: if profile.send_capability || profile.codec_policy != CodecPolicy::UlawOnly {
            Some(capabilities.get())
        } else {
            None
        },
        format: Some(profile.codec_policy.preferred().as_u32()),
        version: Some(2),
        calltoken: Some(token_bytes),
        ..Ies::empty()
    };
    // iax-e402: `dest_call` is 0 for the very first NEW (we don't know the
    // peer's scallno yet) and the peer's chosen scallno on the post-`CALLTOKEN`
    // resent NEW (RFC 5456 §8.6.1).
    OwnedFullFrame::with_ies(
        our_call.value(),
        dest_call,
        false,
        0,
        0,
        0,
        FrameType::Iax,
        Subclass::Iax(IaxCommand::New),
        &ies,
    )
    .expect("NEW IE strings were codepoint-truncated to ≤ 255 bytes")
}

#[cfg(test)]
mod inbound_builder_tests {
    use super::*;
    use crate::frame::{Frame, OwnedFrame, Subclass, encode, parse};
    use crate::ie::Ies;
    use crate::subclass::{ControlSubclass, FrameType, IaxCommand};

    fn our() -> CallNo {
        CallNo::new(16379).unwrap()
    }
    fn peer() -> CallNo {
        CallNo::new(13885).unwrap()
    }

    #[test]
    fn build_authreq_carries_challenge_and_md5_authmethods() {
        let f = build_authreq(our(), peer(), "c0ffeebabe");
        assert_eq!(f.frame_type, FrameType::Iax);
        assert_eq!(f.subclass, Subclass::Iax(IaxCommand::AuthReq));
        let ies = Ies::parse(&f.ie_bytes).unwrap();
        assert_eq!(ies.challenge, Some("c0ffeebabe"));
        assert_eq!(ies.authmethods, Some(2), "advertise MD5 only");
    }

    #[test]
    fn build_accept_carries_chosen_format_ie() {
        let f = build_accept(our(), peer(), VoiceFormat::G711U);
        assert_eq!(f.subclass, Subclass::Iax(IaxCommand::Accept));
        let ies = Ies::parse(&f.ie_bytes).unwrap();
        assert_eq!(ies.format, Some(VoiceFormat::G711U.as_u32()));
        assert!(
            ies.capability.is_none(),
            "ACCEPT sends FORMAT, not CAPABILITY"
        );
    }

    #[test]
    fn build_answer_is_control_answer_no_ies() {
        let f = build_answer(our(), peer());
        assert_eq!(f.frame_type, FrameType::Control);
        assert_eq!(f.subclass, Subclass::Control(ControlSubclass::Answer));
        assert_eq!(Ies::parse(&f.ie_bytes).unwrap(), Ies::empty());
    }

    #[test]
    fn build_calltoken_carries_token_bytes() {
        let tok = b"0123456789abcdef";
        let f = build_calltoken(our(), peer(), tok);
        assert_eq!(f.subclass, Subclass::Iax(IaxCommand::CallToken));
        let ies = Ies::parse(&f.ie_bytes).unwrap();
        assert_eq!(ies.calltoken, Some(&tok[..]));
    }

    #[test]
    fn build_reject_carries_cause_truncated() {
        let f = build_reject(our(), peer(), Some("Authentication failed"));
        assert_eq!(f.subclass, Subclass::Iax(IaxCommand::Reject));
        let ies = Ies::parse(&f.ie_bytes).unwrap();
        assert_eq!(ies.cause, Some("Authentication failed"));
    }

    #[test]
    fn build_accept_round_trips_on_the_wire() {
        let f = build_accept(our(), peer(), VoiceFormat::G711U);
        let owned = OwnedFrame::Full(f.clone());
        let borrowed = owned.as_frame().expect("re-borrow");
        let mut bytes = Vec::new();
        encode(&borrowed, &mut bytes).expect("encode");
        let parsed = parse(&bytes).expect("parse");
        let Frame::Full(p) = parsed else {
            panic!("full")
        };
        assert_eq!(p.subclass, Subclass::Iax(IaxCommand::Accept));
    }
}
