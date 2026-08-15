// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Frame tracing for the inspector. The runtime emits a `TracedFrame` for every
//! wire frame in/out when an observer channel is installed (zero cost otherwise).

use astar_iax_core::trace::FrameSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone)]
pub struct TracedFrame {
    pub seq: u64,
    pub dir: Direction,
    /// Milliseconds since the call runtime started.
    pub at_ms: u64,
    pub summary: FrameSummary,
    pub raw: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traced_frame_records_direction_and_seq() {
        let tf = TracedFrame {
            seq: 7,
            dir: Direction::In,
            at_ms: 12,
            summary: astar_iax_core::trace::FrameSummary {
                kind: "Iax/New".into(),
                source_call: 1,
                dest_call: 0,
                oseqno: 0,
                iseqno: 0,
                timestamp: 0,
                is_voice: false,
                highlights: vec![],
            },
            raw: vec![0x80, 0x01],
        };
        assert_eq!(tf.seq, 7);
        assert_eq!(tf.dir, Direction::In);
        assert_eq!(tf.raw.len(), 2);
    }
}
