// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Where the M17 monitor's audio goes, in words an operator reads.
//!
//! The monitor never sends voice to the reflector. It does join the node's
//! conference as a contributing member (`Manager::add_source_member`), so
//! what the reflector carries is mixed into what the conference sends its
//! members, and a linked AllStar node or an EchoLink station with a radio
//! puts it on the air. That is the behaviour, on purpose. This module is the
//! warning about it: one sentence every surface shows (the admin page, the
//! settings page, `POST /m17`, Discord's `/dial m17`, the log), plus a count
//! of who is connected right now.
//!
//! Who is counted:
//!
//! * every IAX2 leg on the node that is **not** one of its own links. Those
//!   are the nodes and handsets that dialled in, and the conference sends
//!   each of them the whole mix all the time;
//! * every link this node dialled in **transceive** (`*3`). The conference
//!   sends it the mix whenever another member is talking, and the
//!   reflector's audio is part of that mix. A monitor-only link (`*2`) is
//!   never sent anything, so it is left out;
//! * every EchoLink station on the node's leg that has not closed. Like a
//!   transceive link, it is sent the mix while another member talks.
//!
//! The counts are a snapshot of one moment. Anything that connects later
//! hears the reflector too, which the warning says.

use astar_iax::{CallSnapshot, CallSnapshotState, LinkMode, LinkSnapshot};

use crate::command::EchoLinkNodeStatus;

/// The warning itself: what the monitor sends, what it relays, and what to
/// do about it. Shown verbatim by `app.js` and `settings.js` (a test pins
/// that they carry this exact text), quoted in `POST /m17` and Discord.
pub const M17_RELAY_WARNING: &str = "The node never sends voice to an M17 reflector, but it relays what it hears there to every AllStar node and EchoLink station connected to it, and a connected node with a radio puts that on the air. Link only reflectors whose traffic you would rebroadcast, and mind your licence's rules on retransmission.";

/// How many connected members the monitor's audio reaches right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct RelayCounts {
    /// IAX2 legs: nodes and handsets that dialled in, and links this node
    /// dialled in transceive.
    pub allstar: usize,
    /// `EchoLink` stations on the node's leg.
    pub echolink: usize,
}

impl RelayCounts {
    /// Count from raw facts. `calls` is every pooled IAX2 call as
    /// `(raw call id, still up)`; `links` is the link roster as
    /// `(raw call id, mode)`; `echolink` is each `EchoLink` station's state
    /// string.
    #[must_use]
    pub fn count<'a>(
        calls: impl IntoIterator<Item = (u64, bool)>,
        links: impl IntoIterator<Item = (u64, LinkMode)>,
        echolink: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let links: Vec<(u64, LinkMode)> = links.into_iter().collect();
        let inbound = calls
            .into_iter()
            .filter(|(id, up)| *up && !links.iter().any(|(l, _)| l == id))
            .count();
        let transceive = links
            .iter()
            .filter(|(_, mode)| mode.is_transmit_capable())
            .count();
        let echolink = echolink.into_iter().filter(|s| *s != "closed").count();
        Self {
            allstar: inbound + transceive,
            echolink,
        }
    }

    /// Count from the parts of a `NodeSnapshot`.
    #[must_use]
    pub fn from_parts(
        calls: &[CallSnapshot],
        links: &[LinkSnapshot],
        echolink: Option<&EchoLinkNodeStatus>,
    ) -> Self {
        Self::count(
            calls
                .iter()
                .map(|c| (c.id.as_raw(), c.state != CallSnapshotState::Hungup)),
            links.iter().map(|l| (l.call, l.mode)),
            echolink
                .into_iter()
                .flat_map(|e| e.peers.iter().map(|p| p.state.as_str())),
        )
    }

    /// Nobody is connected.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.allstar == 0 && self.echolink == 0
    }

    /// The second sentence: who that is right now.
    #[must_use]
    pub fn sentence(self) -> String {
        if self.is_empty() {
            return "Nothing is connected right now, and anything that connects while the monitor is up hears it too.".to_string();
        }
        let mut parts = Vec::new();
        if self.allstar > 0 {
            parts.push(plural(
                self.allstar,
                "AllStar connection",
                "AllStar connections",
            ));
        }
        if self.echolink > 0 {
            parts.push(plural(
                self.echolink,
                "EchoLink station",
                "EchoLink stations",
            ));
        }
        format!(
            "Right now that is {}, and any of them with a radio will transmit what this reflector carries.",
            parts.join(" and ")
        )
    }

    /// The whole warning: [`M17_RELAY_WARNING`] and then [`Self::sentence`].
    #[must_use]
    pub fn warning(self) -> String {
        format!("{M17_RELAY_WARNING} {}", self.sentence())
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `/status`'s `m17_relay`: the counts and the whole warning, so the pages
/// render the node's own sentence rather than building one of their own.
/// Present only while M17 is enabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct M17RelayStatus {
    /// Who is connected now.
    pub relays_to: RelayCounts,
    /// [`RelayCounts::warning`].
    pub warning: String,
}

impl From<RelayCounts> for M17RelayStatus {
    fn from(c: RelayCounts) -> Self {
        Self {
            relays_to: c,
            warning: c.warning(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_legs_and_transceive_links_count_and_monitor_links_do_not() {
        // Calls 1-3 dialled in; 10 and 11 are links this node dialled (10 in
        // transceive, 11 monitor-only); 4 has hung up.
        let c = RelayCounts::count(
            [
                (1, true),
                (2, true),
                (3, true),
                (4, false),
                (10, true),
                (11, true),
            ],
            [(10, LinkMode::Transceive), (11, LinkMode::Monitor)],
            ["connected", "connecting", "closed"],
        );
        assert_eq!(
            c,
            RelayCounts {
                allstar: 4,
                echolink: 2
            }
        );
    }

    #[test]
    fn a_local_monitor_link_is_never_sent_the_mix() {
        let c = RelayCounts::count([(7, true)], [(7, LinkMode::LocalMonitor)], []);
        assert!(c.is_empty(), "{c:?}");
    }

    #[test]
    fn the_sentence_names_the_counts_with_plurals() {
        let s = |allstar, echolink| RelayCounts { allstar, echolink }.sentence();
        assert_eq!(
            s(3, 1),
            "Right now that is 3 AllStar connections and 1 EchoLink station, and any of them with a radio will transmit what this reflector carries."
        );
        assert_eq!(
            s(1, 0),
            "Right now that is 1 AllStar connection, and any of them with a radio will transmit what this reflector carries."
        );
        assert_eq!(
            s(0, 2),
            "Right now that is 2 EchoLink stations, and any of them with a radio will transmit what this reflector carries."
        );
        assert!(s(0, 0).starts_with("Nothing is connected right now"));
    }

    #[test]
    fn the_warning_says_receive_only_relayed_and_on_the_air() {
        let w = RelayCounts::default().warning();
        assert!(w.starts_with(M17_RELAY_WARNING));
        for needle in [
            "never sends voice",
            "every AllStar node and EchoLink station",
            "on the air",
            "rebroadcast",
            "licence",
        ] {
            assert!(w.contains(needle), "{needle:?} missing from {w:?}");
        }
    }
}
