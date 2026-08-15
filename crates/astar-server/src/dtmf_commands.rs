// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! DTMF `*`-command sequence assembly (iax-d254).
//!
//! Pure per-call assembler: digits in, `(LinkAction, node)` commands out.
//! Grammar (`AllStar` `ilink` defaults, docs/allstar-interop.md §5):
//! `*3<node>` connect (transceive), `*2<node>` monitor, `*1<node>`
//! disconnect. A sequence finalizes on `#` or on the inter-digit timeout;
//! a repeat `*` restarts; any non-digit aborts the pending sequence.
//! No I/O and no clock of its own — callers supply `Instant`s, so the
//! logic is fully unit-testable (the `id_due_and_advance` pattern).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::command::LinkAction;

/// One in-flight `*` sequence for a single call.
struct Pending {
    action: Option<LinkAction>,
    node: String,
    last_digit_at: Instant,
}

/// Per-call DTMF command assembler (iax-d254). See the module doc.
pub(crate) struct DtmfCommandAssembler {
    pending: HashMap<u64, Pending>,
    inter_digit_timeout: Duration,
}

impl DtmfCommandAssembler {
    pub fn new(inter_digit_timeout: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            inter_digit_timeout,
        }
    }

    /// Feed one digit from `call`. Returns a finalized `(action, node)` when
    /// this digit completes a command (`#` terminator with a non-empty node).
    pub fn push(&mut self, call: u64, digit: char, now: Instant) -> Option<(LinkAction, String)> {
        match digit {
            '*' => {
                // Start (or restart) a sequence for this call.
                self.pending.insert(
                    call,
                    Pending {
                        action: None,
                        node: String::new(),
                        last_digit_at: now,
                    },
                );
                None
            }
            '#' => {
                // Finalize: needs an action and a non-empty node number.
                let p = self.pending.remove(&call)?;
                match p.action {
                    Some(action) if !p.node.is_empty() => Some((action, p.node)),
                    _ => None,
                }
            }
            '0'..='9' => {
                let p = self.pending.get_mut(&call)?;
                p.last_digit_at = now;
                #[allow(clippy::single_match_else)]
                match p.action {
                    None => {
                        p.action = match digit {
                            '3' => Some(LinkAction::Connect),
                            '2' => Some(LinkAction::Monitor),
                            '1' => Some(LinkAction::Disconnect),
                            _ => {
                                // Unknown function code: abort the sequence.
                                self.pending.remove(&call);
                                return None;
                            }
                        };
                        None
                    }
                    Some(_) => {
                        p.node.push(digit);
                        None
                    }
                }
            }
            _ => {
                // A/B/C/D or anything else aborts the pending sequence.
                self.pending.remove(&call);
                None
            }
        }
    }

    /// Finalize every pending sequence whose inter-digit gap has elapsed.
    /// Sequences without an action or node are silently discarded.
    pub fn tick(&mut self, now: Instant) -> Vec<(LinkAction, String)> {
        let timeout = self.inter_digit_timeout;
        let due: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_digit_at) >= timeout)
            .map(|(&c, _)| c)
            .collect();
        let mut out = Vec::new();
        for call in due {
            if let Some(p) = self.pending.remove(&call)
                && let Some(action) = p.action
                && !p.node.is_empty()
            {
                out.push((action, p.node));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[allow(clippy::duration_suboptimal_units)]
    const T: Duration = Duration::from_millis(3000);

    fn feed(
        a: &mut DtmfCommandAssembler,
        call: u64,
        s: &str,
        now: Instant,
    ) -> Vec<(LinkAction, String)> {
        s.chars().filter_map(|c| a.push(call, c, now)).collect()
    }

    #[test]
    fn star3_node_hash_connects() {
        let mut a = DtmfCommandAssembler::new(T);
        let now = Instant::now();
        let got = feed(&mut a, 1, "*355553#", now);
        assert_eq!(got, vec![(LinkAction::Connect, "55553".to_string())]);
    }

    #[test]
    fn star2_and_star1_map_to_monitor_and_disconnect() {
        let mut a = DtmfCommandAssembler::new(T);
        let now = Instant::now();
        assert_eq!(
            feed(&mut a, 1, "*21999#", now),
            vec![(LinkAction::Monitor, "1999".to_string())]
        );
        assert_eq!(
            feed(&mut a, 1, "*11999#", now),
            vec![(LinkAction::Disconnect, "1999".to_string())]
        );
    }

    #[test]
    fn timeout_finalizes_without_hash() {
        let mut a = DtmfCommandAssembler::new(T);
        let t0 = Instant::now();
        assert!(
            feed(&mut a, 1, "*355553", t0).is_empty(),
            "not finalized yet"
        );
        assert!(a.tick(t0 + Duration::from_millis(2999)).is_empty());
        assert_eq!(
            a.tick(t0 + Duration::from_millis(3001)),
            vec![(LinkAction::Connect, "55553".to_string())]
        );
        assert!(
            a.tick(t0 + Duration::from_millis(9999)).is_empty(),
            "consumed"
        );
    }

    #[test]
    fn repeat_star_restarts_the_sequence() {
        let mut a = DtmfCommandAssembler::new(T);
        let now = Instant::now();
        assert_eq!(
            feed(&mut a, 1, "*35*11999#", now),
            vec![(LinkAction::Disconnect, "1999".to_string())]
        );
    }

    #[test]
    fn unknown_action_digit_discards() {
        let mut a = DtmfCommandAssembler::new(T);
        let t0 = Instant::now();
        assert!(feed(&mut a, 1, "*955553#", t0).is_empty());
        assert!(a.tick(t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn empty_node_number_discards() {
        let mut a = DtmfCommandAssembler::new(T);
        let t0 = Instant::now();
        assert!(
            feed(&mut a, 1, "*3#", t0).is_empty(),
            "# with no node = discard"
        );
        assert!(
            a.tick(t0 + Duration::from_secs(10)).is_empty(),
            "timeout with no node = discard"
        );
    }

    #[test]
    fn non_command_digits_outside_a_sequence_are_ignored() {
        let mut a = DtmfCommandAssembler::new(T);
        let t0 = Instant::now();
        assert!(feed(&mut a, 1, "5551999#", t0).is_empty());
        assert!(a.tick(t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn letters_abort_a_pending_sequence() {
        let mut a = DtmfCommandAssembler::new(T);
        let t0 = Instant::now();
        assert!(feed(&mut a, 1, "*355A553#", t0).is_empty());
        assert!(a.tick(t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn interleaved_calls_assemble_independently() {
        let mut a = DtmfCommandAssembler::new(T);
        let now = Instant::now();
        let mut got = Vec::new();
        // Digits from two calls interleaved digit-by-digit.
        for (call, d) in [
            (1, '*'),
            (2, '*'),
            (1, '3'),
            (2, '1'),
            (1, '5'),
            (2, '9'),
            (1, '5'),
            (2, '#'),
            (1, '#'),
        ] {
            if let Some(cmd) = a.push(call, d, now) {
                got.push((call, cmd));
            }
        }
        assert_eq!(
            got,
            vec![
                (2, (LinkAction::Disconnect, "9".to_string())),
                (1, (LinkAction::Connect, "55".to_string())),
            ]
        );
    }
}
