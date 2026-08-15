// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The inspector's two-level view: a Level-1 semantic timeline and a Level-2
//! bounded raw-frame ring. Fed by `CallEvents` and `TracedFrames`; read by the
//! harness for /timeline and /frames.

use std::collections::VecDeque;

use astar_iax::{CallEvent, TracedFrame};
use astar_iax_core::session::fsm::FailReason;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TimelineEvent {
    pub seq: u64,
    pub kind: String,
    pub detail: String,
}

pub struct Tracer {
    timeline: Vec<TimelineEvent>,
    next_event_seq: u64,
    frames: VecDeque<TracedFrame>,
    capacity: usize,
}

impl Tracer {
    #[must_use]
    pub fn new(frame_capacity: usize) -> Self {
        Self {
            timeline: Vec::new(),
            next_event_seq: 0,
            frames: VecDeque::new(),
            capacity: frame_capacity.max(1),
        }
    }

    fn push(&mut self, kind: &str, detail: String) {
        self.timeline.push(TimelineEvent {
            seq: self.next_event_seq,
            kind: kind.to_string(),
            detail,
        });
        self.next_event_seq += 1;
    }

    pub fn on_event(&mut self, ev: &CallEvent) {
        match ev {
            CallEvent::Ringing => self.push("Ringing", String::new()),
            CallEvent::Answered { format } => self.push("Answered", format!("codec {format:?}")),
            CallEvent::RemotePtt(true) => self.push("RemoteKey", String::new()),
            CallEvent::RemotePtt(false) => self.push("RemoteUnkey", String::new()),
            CallEvent::Text(s) => self.push("Text", s.clone()),
            CallEvent::ConnectionLost => self.push("ConnectionLost", String::new()),
            CallEvent::ConnectionRestored => self.push("ConnectionRestored", String::new()),
            CallEvent::Dtmf(d) => self.push("Dtmf", d.to_string()),
            CallEvent::Hangup { reason } => {
                let detail = match reason {
                    FailReason::RemoteHangup { cause } => format!(
                        "remote hangup — {}",
                        cause.clone().unwrap_or_else(|| "no cause".into())
                    ),
                    other => format!("{other:?}"),
                };
                self.push("Hangup", detail);
            }
            _ => {}
        }
    }

    pub fn on_frame(&mut self, tf: TracedFrame) {
        if self.frames.len() == self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(tf);
    }

    /// Push an out-of-band note (e.g. a token-mint error) onto the timeline.
    pub fn note(&mut self, kind: &str, detail: impl Into<String>) {
        self.push(kind, detail.into());
    }

    #[must_use]
    pub fn timeline_since(&self, seq: u64) -> Vec<TimelineEvent> {
        self.timeline
            .iter()
            .filter(|e| e.seq >= seq)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn frames_since(&self, seq: u64) -> Vec<TracedFrame> {
        self.frames
            .iter()
            .filter(|f| f.seq >= seq)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_iax::CallEvent;
    use astar_iax_core::session::fsm::FailReason;

    #[test]
    fn timeline_records_answered_and_hangup_in_order() {
        let mut t = Tracer::new(8);
        t.on_event(&CallEvent::Ringing);
        t.on_event(&CallEvent::Answered {
            format: astar_iax_core::VoiceFormat::G711U,
        });
        t.on_event(&CallEvent::Hangup {
            reason: FailReason::Aborted,
        });
        let kinds: Vec<_> = t.timeline_since(0).iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                "Ringing".to_string(),
                "Answered".to_string(),
                "Hangup".to_string()
            ]
        );
    }

    #[test]
    fn frame_ring_is_bounded_and_keeps_seq() {
        let mut t = Tracer::new(2);
        for seq in 0..5u64 {
            t.on_frame(traced(seq));
        }
        let frames = t.frames_since(0);
        assert_eq!(frames.len(), 2, "ring caps at capacity");
        assert_eq!(frames.first().unwrap().seq, 3);
        assert_eq!(frames.last().unwrap().seq, 4);
        assert_eq!(t.frames_since(4).len(), 1, "since filter works");
    }

    fn traced(seq: u64) -> astar_iax::TracedFrame {
        astar_iax::TracedFrame {
            seq,
            dir: astar_iax::Direction::In,
            at_ms: seq,
            summary: astar_iax_core::trace::FrameSummary {
                kind: "MiniVoice".into(),
                source_call: 1,
                dest_call: 0,
                oseqno: 0,
                iseqno: 0,
                timestamp: 0,
                is_voice: true,
                highlights: vec![],
            },
            raw: vec![],
        }
    }
}
