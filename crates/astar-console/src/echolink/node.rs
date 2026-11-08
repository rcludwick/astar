// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! astar-server's `EchoLink` leg: stations an admin dials, each one a peer
//! member of the node's conference.
//!
//! [`EchoLinkNodeLeg`] owns one [`EchoLinkLink`] (one thread, the two UDP
//! ports) and one entry per station called. Each station's audio ends are a
//! conference member from [`astar_iax::Manager::add_peer_member`]: what the
//! station says is summed into the net, and while someone else on the net
//! is talking the station hears the mix-minus, everyone but itself.
//!
//! # Sample rates
//!
//! GSM runs at 8 kHz and the conference at the pipeline rate, which is
//! 16 kHz on a node running slin16. Each member gets a [`RateBridge`] in each
//! direction when the two differ: one 20 ms frame in, one 20 ms frame out.
//!
//! # When packets leave
//!
//! The link thread sends on its own pass grid, not on the conference's
//! 20 ms tick. A pass blocks on the audio socket for at most 50 ms, then
//! drains every member's mix-minus channel in turn and sends each station
//! whatever full four-frame packets it has. So all the stations on the leg
//! are served in the same pass, one after another, and a station hears its
//! packets in small bursts that its own playout smooths.
//!
//! # Two stations behind one address
//!
//! In production the link matches a datagram to a station by IP alone,
//! because a NAT may rewrite the source port. Two stations behind one public
//! IP would then both match whichever was called first, so the second
//! station's audio would be credited to the first. Rather than mix them up,
//! the leg refuses to call a station whose IP another station on the leg
//! already has.
//!
//! # Talking
//!
//! [`EchoLinkNodeLeg::poll`] copies each station's "far end talking" flag
//! into [`astar_iax::Manager::set_peer_member_keyed`], which is what keys
//! the node's `AllStar` links and opens the other stations' receive sides on
//! the next `sync_link_keying`.

use std::io;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use astar_audio::MemberId;
use astar_echolink::PeerAddr;
use astar_echolink::directory::StationEntry;
use astar_echolink::qso::{CloseReason, QsoState};
use astar_iax::{Manager, PeerMemberHandle};
use astar_record::packet_tap::{PacketCallId, PacketSlot};

use super::EchoLinkNetwork;
use super::client::CallLink;
use super::link::{EchoLinkLink, PeerAudio, PeerShared};
use crate::ConsoleError;
use crate::voice_route::{RateBridge, SESSION_RATE};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One station on the leg, as the admin page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLinkPeerStatus {
    pub call: String,
    pub node: u32,
    pub link: CallLink,
    /// The station is talking (audio arrived within `talk_hold`).
    pub talking: bool,
    /// Whole seconds since the station answered; `None` until it has.
    pub up_secs: Option<u64>,
    /// Why the leg stopped sending the net to this station (the transmit
    /// cap), a plain sentence; cleared when the net next talks to it.
    pub notice: Option<String>,
}

/// A change on the leg that [`EchoLinkNodeLeg::poll`] reports once.
///
/// Edges report only what happens on its own. A station the admin hangs up
/// on with `echolink_node_disconnect`, or that `echolink_logout` hangs up
/// along with every other, leaves the leg at once and emits no `Ended`
/// edge: the caller already knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoLinkNodeEdge {
    /// The station answered.
    Connected(String),
    /// The contact is over and the station has left the conference.
    /// `reason` is a few plain words: "no answer" (five SDES went
    /// unanswered), "timed out" (no keepalive for the connection timeout),
    /// "hung up" (the far end sent a BYE), or "conference closed" (the
    /// node's topology no longer has a mix to be in).
    Ended { call: String, reason: String },
    /// The transmit cap stopped sending the net to this station after 10
    /// minutes of continuous talk; `notice` says so in a plain sentence.
    /// Sending resumes by itself after the net's next pause.
    TransmitCapped { call: String, notice: String },
}

/// One station called, and the conference member it plays into.
struct NodePeer {
    entry: StationEntry,
    /// The station's IP as the link calls it, for the shared-address check.
    ip: IpAddr,
    member: MemberId,
    shared: Arc<PeerShared>,
    connected_at: Option<Instant>,
    /// Transmit-cap trips already reported as edges.
    caps_seen: u32,
}

impl NodePeer {
    fn qso(&self) -> QsoState {
        *lock(&self.shared.state)
    }

    fn talking(&self) -> bool {
        self.shared.talking.load(Ordering::Relaxed)
    }
}

/// The node's `EchoLink` leg: one link, any number of stations, each a
/// conference peer member.
pub struct EchoLinkNodeLeg {
    link: EchoLinkLink,
    my_call: String,
    location: String,
    net: EchoLinkNetwork,
    peers: Vec<NodePeer>,
    packet_slot: PacketSlot,
}

impl EchoLinkNodeLeg {
    /// Start the link thread for `my_call`. No port is open until the first
    /// station is called.
    pub(crate) fn start(my_call: &str, location: &str, net: &EchoLinkNetwork) -> io::Result<Self> {
        let packet_slot = PacketSlot::default();
        Ok(Self {
            link: EchoLinkLink::start(my_call, location, net, packet_slot.clone())?,
            my_call: my_call.to_owned(),
            location: location.to_owned(),
            net: net.clone(),
            peers: Vec::new(),
            packet_slot,
        })
    }

    /// Whether this leg was started for this callsign and location. A leg
    /// with no stations is cheap to replace when either changes.
    pub(crate) fn started_for(&self, my_call: &str, location: &str) -> bool {
        self.my_call == my_call && self.location == location
    }

    /// True when no station is on the leg (reaped ones are gone already).
    pub(crate) fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// True while any station is connecting or connected.
    pub(crate) fn active(&self) -> bool {
        self.peers
            .iter()
            .any(|p| !matches!(p.qso(), QsoState::Closed(_)))
    }

    pub(crate) fn has(&self, call: &str) -> bool {
        self.peers
            .iter()
            .any(|p| p.entry.call.eq_ignore_ascii_case(call))
    }

    /// Under by-IP matching, whether a station already on the leg has the
    /// IP `entry` would be called on.
    pub(crate) fn shares_address(&self, entry: &StationEntry) -> Option<IpAddr> {
        if !self.net.match_by_ip {
            return None;
        }
        let ip = (self.net.peer_addr)(entry).audio.ip();
        self.peers.iter().any(|p| p.ip == ip).then_some(ip)
    }

    /// Call `entry` with `member` as its audio ends. On failure the member
    /// is the caller's to withdraw.
    pub(crate) fn add(
        &mut self,
        entry: StationEntry,
        member: PeerMemberHandle,
    ) -> Result<(), ConsoleError> {
        let bridge = |from, to| {
            if from == to {
                Ok(None)
            } else {
                RateBridge::new(from, to).map(Some)
            }
        };
        let audio = PeerAudio::Member {
            to_conference: member.to_conference,
            from_conference: member.from_conference,
            rx_bridge: bridge(SESSION_RATE, member.sample_rate).map_err(ConsoleError::Audio)?,
            tx_bridge: bridge(member.sample_rate, SESSION_RATE).map_err(ConsoleError::Audio)?,
        };
        let ip = (self.net.peer_addr)(&entry).audio.ip();
        let shared = self.link.add(entry.clone(), audio).map_err(|e| {
            tracing::warn!(error = %e, "echolink: could not open the UDP ports");
            ConsoleError::EchoLink(super::client::BIND_FAILED.to_owned())
        })?;
        self.peers.push(NodePeer {
            entry,
            ip,
            member: member.id,
            shared,
            connected_at: None,
            caps_seen: 0,
        });
        Ok(())
    }

    /// Hang up on `call` (a BYE, on the link's next pass) and drop it from
    /// the leg. Returns its member for the caller to withdraw.
    pub(crate) fn remove(&mut self, call: &str) -> Option<MemberId> {
        let at = self
            .peers
            .iter()
            .position(|p| p.entry.call.eq_ignore_ascii_case(call))?;
        let peer = self.peers.remove(at);
        self.link.remove(&peer.entry.call);
        Some(peer.member)
    }

    /// Hang up on every station, returning their members.
    pub(crate) fn remove_all(&mut self) -> Vec<MemberId> {
        let calls: Vec<String> = self.peers.iter().map(|p| p.entry.call.clone()).collect();
        calls.iter().filter_map(|c| self.remove(c)).collect()
    }

    /// Report each station's edges, push its talking flag into the
    /// conference, and reap the stations whose contact is over, withdrawing
    /// their members. A station whose member the conference has already
    /// dropped (a switch to Handset or Parrot, or no engine at all) is hung
    /// up and reaped as "conference closed".
    pub(crate) fn poll(&mut self, mut mgr: Option<&mut Manager>) -> Vec<EchoLinkNodeEdge> {
        let mut edges = Vec::new();
        let mut gone = Vec::new();
        for peer in &mut self.peers {
            let state = peer.qso();
            let enrolled = mgr.as_ref().is_some_and(|m| m.has_peer_member(peer.member));
            let reason = match state {
                QsoState::Closed(why) => Some(close_reason(why)),
                _ if !enrolled => Some("conference closed"),
                _ => None,
            };
            if let Some(reason) = reason {
                edges.push(EchoLinkNodeEdge::Ended {
                    call: peer.entry.call.clone(),
                    reason: reason.to_owned(),
                });
                gone.push(peer.entry.call.clone());
                continue;
            }
            if state == QsoState::Connected && peer.connected_at.is_none() {
                peer.connected_at = Some(Instant::now());
                edges.push(EchoLinkNodeEdge::Connected(peer.entry.call.clone()));
            }
            let caps = peer.shared.caps.load(Ordering::Relaxed);
            if caps != peer.caps_seen {
                peer.caps_seen = caps;
                edges.push(EchoLinkNodeEdge::TransmitCapped {
                    call: peer.entry.call.clone(),
                    notice: super::link::member_cap_notice(&peer.entry.call),
                });
            }
            if let Some(m) = mgr.as_deref_mut() {
                m.set_peer_member_keyed(peer.member, peer.talking());
            }
        }
        for call in gone {
            if let (Some(member), Some(m)) = (self.remove(&call), mgr.as_deref_mut()) {
                m.remove_peer_member(member);
            }
        }
        edges
    }

    /// Every station on the leg, in the order they were called.
    pub(crate) fn peers(&self) -> Vec<EchoLinkPeerStatus> {
        self.peers
            .iter()
            .map(|p| EchoLinkPeerStatus {
                call: p.entry.call.clone(),
                node: p.entry.node,
                link: match p.qso() {
                    QsoState::Connecting => CallLink::Connecting,
                    QsoState::Connected => CallLink::Connected,
                    QsoState::Closed(_) => CallLink::Closed,
                },
                talking: p.talking(),
                up_secs: p.connected_at.map(|t| t.elapsed().as_secs()),
                notice: lock(&p.shared.notice).clone(),
            })
            .collect()
    }

    /// The slot every station's packet tap offers to, one slot for the leg.
    /// Install a tap to log them.
    #[must_use]
    pub fn packet_slot(&self) -> PacketSlot {
        self.packet_slot.clone()
    }

    /// Each station's id in the packet log, in the order they were called.
    #[must_use]
    pub fn packet_calls(&self) -> Vec<PacketCallId> {
        self.peers.iter().map(|p| p.shared.packet_call).collect()
    }

    /// The link's bound audio and control addresses, or `None` while no
    /// station is on the leg. For tests, which aim an uninvited fake at the
    /// real ephemeral ports.
    #[doc(hidden)]
    #[must_use]
    pub fn local_addrs(&self) -> Option<PeerAddr> {
        self.link.local_addrs()
    }
}

fn close_reason(why: CloseReason) -> &'static str {
    match why {
        CloseReason::NoAnswer => "no answer",
        CloseReason::TimedOut => "timed out",
        CloseReason::ByeReceived => "hung up",
        // Only our own hang-up closes a contact this way, and that takes
        // the station off the leg before any poll can see it, so this
        // string never reaches an edge. It is here because the match must
        // be total.
        CloseReason::Hangup => "disconnected",
    }
}
