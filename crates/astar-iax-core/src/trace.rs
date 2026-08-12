// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Compact, serializable summaries of wire frames for the inspector
//! (astar-inspect). Pure functions over parsed frames; no behavioural
//! effect on the FSM.

use crate::frame::{Frame, FullFrame, Subclass};
use crate::subclass::FrameType;

/// A small human/agent-friendly summary of one IAX2 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FrameSummary {
    /// Coarse label, e.g. "Iax/New", "Control/Answer", "Voice/G711U", "`MiniVoice`".
    pub kind: String,
    pub source_call: u16,
    pub dest_call: u16,
    pub oseqno: u8,
    pub iseqno: u8,
    pub timestamp: u32,
    /// True for mini and full voice frames (so the UI can fold the audio flood).
    pub is_voice: bool,
    /// A few decoded highlights (CALLTOKEN present, CAUSE, TEXT body, codec).
    pub highlights: Vec<String>,
}

/// Summarize a parsed frame. Infallible.
#[must_use]
pub fn summarize(frame: &Frame<'_>) -> FrameSummary {
    match frame {
        Frame::Mini(m) => FrameSummary {
            kind: "MiniVoice".to_owned(),
            source_call: m.source_call,
            dest_call: 0,
            oseqno: 0,
            iseqno: 0,
            timestamp: u32::from(m.timestamp),
            is_voice: true,
            highlights: Vec::new(),
        },
        Frame::Full(f) => summarize_full(f),
    }
}

fn summarize_full(f: &FullFrame<'_>) -> FrameSummary {
    let kind = label_subclass(f.subclass);
    let is_voice = matches!(f.frame_type, FrameType::Voice);
    let mut highlights = Vec::new();

    // CALLTOKEN IE — presence is significant (anti-spoof handshake, RFC 5456 §8.6)
    if let Some(tok) = f.ies.calltoken {
        if tok.is_empty() {
            highlights.push("calltoken=<request>".to_owned());
        } else {
            highlights.push(format!("calltoken=<{} bytes>", tok.len()));
        }
    }

    // CAUSE IE — human-readable hangup reason
    if let Some(cause) = f.ies.cause {
        highlights.push(format!("cause={cause}"));
    }

    // CAUSECODE — numeric Q.931 cause code
    if let Some(code) = f.ies.causecode {
        highlights.push(format!("causecode={code}"));
    }

    // TEXT frame body — payload is raw UTF-8 text (RFC 5456 §6.4)
    if matches!(f.frame_type, FrameType::Text) && !f.payload.is_empty() {
        let text = String::from_utf8_lossy(f.payload);
        highlights.push(format!("text={text}"));
    }

    // Voice codec
    if is_voice && let Subclass::Voice(fmt) = f.subclass {
        highlights.push(format!("codec={fmt:?}"));
    }

    FrameSummary {
        kind,
        source_call: f.source_call,
        dest_call: f.dest_call,
        oseqno: f.oseqno,
        iseqno: f.iseqno,
        timestamp: f.timestamp,
        is_voice,
        highlights,
    }
}

fn label_subclass(sc: Subclass) -> String {
    match sc {
        Subclass::Iax(c) => format!("Iax/{c:?}"),
        Subclass::Control(c) => format!("Control/{c:?}"),
        Subclass::Voice(v) => format!("Voice/{v:?}"),
        Subclass::Dtmf(c) => format!("Dtmf/{c}"),
        Subclass::Raw(v) => format!("Raw({v})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::parse_lenient;

    #[test]
    fn summarize_full_iax_new_reports_type_and_seq() {
        let bytes: &[u8] = &[0x80, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 6, 1];
        let f = parse_lenient(bytes).expect("parses");
        let s = summarize(&f);
        assert_eq!(s.kind, "Iax/New");
        assert_eq!(s.source_call, 1);
        assert_eq!(s.dest_call, 0);
        assert_eq!(s.oseqno, 0);
        assert!(!s.is_voice);
    }

    #[test]
    fn summarize_mini_voice_is_flagged() {
        let bytes: &[u8] = &[0x00, 0x01, 0x00, 0x14, 0xff, 0xff];
        let f = parse_lenient(bytes).expect("parses");
        let s = summarize(&f);
        assert!(s.is_voice, "mini frames are voice");
        assert_eq!(s.kind, "MiniVoice");
        assert_eq!(s.source_call, 1);
    }
}
