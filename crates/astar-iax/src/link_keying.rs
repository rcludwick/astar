// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! When a link's received voice counts as the far node talking, by
//! `app_rpt`'s rule (nodebench finding 7).
//!
//! `app_rpt` opens every link with `!NEWKEY1!`. A peer that did that keys by
//! sending voice: its voice frames are its transmission, and it sends none
//! while unkeyed. A peer that is plainly an `app_rpt` node (it sends the link
//! TEXT traffic, `L ...`, `T ...`, `K ...`) but has not done the handshake
//! within `NEWKEYTIME` (2 s) is in the older mode: it may stream voice
//! continuously and marks its transmission with `RADIO_KEY` / `RADIO_UNKEY`.
//! On a monitor link (`ilink 2`) it never sends `RADIO_KEY` at all, which is
//! how a monitor link stays one-way. `app_rpt` ignores such a link's voice
//! while it is unkeyed, and so does this: the voice never reaches the bridge.
//!
//! Peers that are not `app_rpt` nodes (an IAX phone, astar's own clients, the
//! bench probe) send no link TEXT, so their voice always passes.

use std::time::{Duration, Instant};

use astar_iax_core::OwnedTextEvent;
use astar_iax_core::session::fsm::AppEvent;

/// `app_rpt`'s `NEWKEYTIME`: how long it waits for `!NEWKEY1!` back.
pub(crate) const NEWKEY_TIME: Duration = Duration::from_secs(2);

/// The `!NEWKEY1!` body, as the FSM surfaces it in `TextReceived`.
pub(crate) const NEWKEY: &str = "!NEWKEY1!";

/// `app_rpt`'s link-disconnect string.
pub(crate) const DISCONNECT: &str = "!!DISCONNECT!!";

/// How long a leg holds its HANGUP after `!!DISCONNECT!!` (nodebench finding
/// 3). `app_rpt` drops a link at once only when it has read
/// `!!DISCONNECT!!`; a link that just hangs up it holds for `DISC_TIME`
/// (10 s), expecting it back. `chan_iax2` discards a channel's queued frames
/// once a HANGUP is processed, so a TEXT and HANGUP sent together lose the
/// TEXT. `app_rpt`'s own `ilink 1` leaves 300-700 ms between them. The leg
/// sends HANGUP when this runs out, or not at all if the node hangs up
/// first, which it does once it has read the TEXT.
pub(crate) const DISCONNECT_GRACE: Duration = Duration::from_millis(250);

/// A leg's own hangup, held while the node reads `!!DISCONNECT!!`.
#[derive(Debug)]
pub(crate) struct PendingHangup {
    pub(crate) cause: Option<String>,
    pub(crate) due: Instant,
    /// The handle asked the runtime to stop: exit once the HANGUP is out.
    pub(crate) then_shutdown: bool,
}

/// Per-leg view of the far end's keying mode.
#[derive(Debug, Default)]
pub(crate) struct LinkKeying {
    active_at: Option<Instant>,
    app_rpt: bool,
    newkey: bool,
    /// Not a node-to-node link (a web-transceiver guest call): the far end
    /// is never treated as an `app_rpt` link peer.
    off: bool,
}

impl LinkKeying {
    /// For a call that is not a node-to-node link, such as a web-transceiver
    /// guest call: its voice always counts, and its hangup is a plain HANGUP.
    pub(crate) fn off() -> Self {
        Self {
            off: true,
            ..Self::default()
        }
    }

    /// Observe an FSM event on this leg.
    pub(crate) fn observe(&mut self, ev: &AppEvent, now: Instant) {
        if self.off {
            return;
        }
        match ev {
            AppEvent::Connected { .. } => {
                self.active_at.get_or_insert(now);
            }
            AppEvent::TextReceived(t) => match t {
                OwnedTextEvent::Raw(s) if s == NEWKEY => {
                    self.newkey = true;
                    self.app_rpt = true;
                }
                OwnedTextEvent::Raw(s) => {
                    if is_link_text(s) {
                        self.app_rpt = true;
                    }
                }
                OwnedTextEvent::KStatus { .. } => self.app_rpt = true,
            },
            _ => {}
        }
    }

    /// Whether the far end is an `app_rpt` node: it sent `!NEWKEY1!` or the
    /// link TEXT traffic. Such a peer is told `!!DISCONNECT!!` before HANGUP.
    pub(crate) fn is_app_rpt(&self) -> bool {
        self.app_rpt
    }

    /// Whether voice received now counts. `keyed` is the far end's
    /// `RADIO_KEY` state.
    pub(crate) fn rx_open(&self, keyed: bool, now: Instant) -> bool {
        if self.newkey || !self.app_rpt {
            return true;
        }
        match self.active_at {
            Some(t) if now.saturating_duration_since(t) >= NEWKEY_TIME => keyed,
            _ => true,
        }
    }
}

/// `app_rpt`'s periodic link TEXT: the link list (`L`), telemetry (`T`) and
/// key status (`K`), each a letter and a space or the bare letter.
fn is_link_text(s: &str) -> bool {
    let mut it = s.chars();
    matches!(
        (it.next(), it.next()),
        (Some('L' | 'T' | 'K'), None | Some(' '))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_iax_core::session::call_no::CallNo;

    fn connected() -> AppEvent {
        AppEvent::Connected {
            peer_call: CallNo::new(1).unwrap(),
        }
    }

    fn text(s: &str) -> AppEvent {
        AppEvent::TextReceived(OwnedTextEvent::Raw(s.into()))
    }

    #[test]
    fn a_peer_that_sends_no_link_text_is_always_heard() {
        let t0 = Instant::now();
        let mut k = LinkKeying::default();
        k.observe(&connected(), t0);
        assert!(k.rx_open(false, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn a_newkey_peer_keys_by_sending_voice() {
        let t0 = Instant::now();
        let mut k = LinkKeying::default();
        k.observe(&connected(), t0);
        k.observe(&text("L "), t0);
        k.observe(&text(NEWKEY), t0 + Duration::from_millis(30));
        assert!(k.rx_open(false, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn an_app_rpt_peer_without_newkey_needs_radio_key_after_newkeytime() {
        let t0 = Instant::now();
        let mut k = LinkKeying::default();
        k.observe(&connected(), t0);
        k.observe(&text("T 1999 CONNECTED,1999,1998"), t0);
        assert!(k.rx_open(false, t0 + Duration::from_secs(1)), "grace");
        let later = t0 + NEWKEY_TIME;
        assert!(!k.rx_open(false, later), "unkeyed voice is ignored");
        assert!(k.rx_open(true, later), "RADIO_KEY opens it");
    }

    #[test]
    fn a_call_that_is_not_a_link_ignores_link_text() {
        let t0 = Instant::now();
        let mut k = LinkKeying::off();
        k.observe(&connected(), t0);
        k.observe(&text("L "), t0);
        assert!(!k.is_app_rpt());
        assert!(k.rx_open(false, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn link_text_shapes() {
        assert!(is_link_text("L "));
        assert!(is_link_text("L 1999,T2000"));
        assert!(is_link_text("T 1999 COMPLETE"));
        assert!(is_link_text("K"));
        assert!(!is_link_text("Lima"));
        assert!(!is_link_text("hello"));
        assert!(!is_link_text(""));
    }
}
