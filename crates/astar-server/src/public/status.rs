// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public status: an **allowlisted projection** of [`NodeSnapshot`], not
//! a filtered copy of it.
//!
//! Every field of [`PublicStatus`] is built here by hand from named snapshot
//! fields, so a field added to the snapshot later reaches the public page
//! only if someone writes it into this file. `public_status_field_list_is_pinned`
//! fails the moment the wire shape changes. What is never in it: an IP
//! address, a port, a secret, config, Discord details, registration state.
//!
//! [`Activity`] turns successive snapshots into the page's feed (link up and
//! down, key and unkey) and today's totals.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use crate::command::NodeSnapshot;

/// How many feed entries the page shows.
pub const ACTIVITY_LEN: usize = 20;
/// A key shorter than this is a kerchunk: counted in today's totals, kept
/// out of the feed.
pub const KERCHUNK_MS: u64 = 500;

/// Whether the live audio streams can be listened to.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamAvailability {
    /// `[stream] enabled` is false (or the section is absent).
    Off,
    /// The tap is enrolled and audio is flowing.
    Live,
    /// Enabled, but no tap is possible in this mode (handset or parrot).
    Unavailable,
}

/// The page's whole view of the node. Field order is the wire order.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct PublicStatus {
    /// This node's number, when it has one.
    pub node: Option<String>,
    /// `"bridge" | "conference" | "parrot" | "handset"`.
    pub mode: &'static str,
    pub links: Vec<PublicLink>,
    pub talker: Option<PublicTalker>,
    /// Who spoke last, for the "Quiet" line.
    pub last_talker: Option<String>,
    /// Newest first, at most [`ACTIVITY_LEN`].
    pub activity: Vec<PublicEvent>,
    pub today: Today,
    pub uptime_secs: u64,
    pub listeners: u32,
    pub stream: StreamAvailability,
    /// The server was built with `stream-mp3`, so `/stream.mp3` exists.
    pub mp3: bool,
}

/// One linked node or reflector.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PublicLink {
    /// `"allstar"` or `"m17"`.
    pub network: &'static str,
    /// The node number, or the reflector and module (`"M17-USA C"`).
    pub name: String,
    /// `"in"`, `"out"`, `"monitor"` or `"receive"` (M17 is receive-only).
    pub direction: &'static str,
    pub up_secs: u64,
}

/// Who is talking now.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PublicTalker {
    /// A node number, or an M17 callsign. Text only on the page: an M17
    /// callsign is whatever the far end sent.
    pub name: String,
    /// The link it arrived on, where that differs from `name`.
    pub via: Option<String>,
    pub secs: u64,
}

/// One feed entry.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PublicEvent {
    pub at_unix: u64,
    pub text: String,
}

/// Today's totals, reset at midnight UTC.
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Today {
    pub talk_secs: u64,
    pub transmissions: u32,
    pub stations: u32,
}

/// An over in progress.
#[derive(Debug, Clone)]
struct Over {
    name: String,
    via: Option<String>,
    started_ms: u64,
    /// The "keyed" entry is in the feed: the over outlived a kerchunk.
    announced: bool,
}

/// The diff state between snapshots, the feed, and the day's counters.
#[derive(Debug)]
pub struct Activity {
    started: Instant,
    /// Up links by a stable key (`allstar:<call>` / `m17:<target>`) → name.
    links: HashMap<String, String>,
    /// Keyed talkers by the same keys (an M17 key adds the callsign).
    overs: HashMap<String, Over>,
    feed: VecDeque<PublicEvent>,
    last_talker: Option<String>,
    day: u64,
    talk_ms: u64,
    transmissions: u32,
    stations: BTreeSet<String>,
}

fn day_of(now_ms: u64) -> u64 {
    now_ms / 1000 / 86_400
}

/// `m:ss`, or `h:mm:ss` past the hour.
fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The name a link is shown under: its node number, or a neutral phrase
/// for an inbound link that gave none.
fn link_name(node: &str) -> String {
    if node.trim().is_empty() {
        "a connected node".to_string()
    } else {
        node.to_string()
    }
}

/// The links and current talkers a snapshot shows, keyed stably.
struct Seen {
    links: Vec<(String, String)>,
    talkers: Vec<(String, String, Option<String>)>,
}

fn seen(snap: &NodeSnapshot) -> Seen {
    let mut links = Vec::new();
    let mut talkers = Vec::new();
    for l in &snap.links {
        if l.state != astar_iax::LinkState::Up {
            continue;
        }
        let key = format!("allstar:{}", l.call);
        let name = link_name(&l.node);
        if l.rx_active {
            talkers.push((key.clone(), name.clone(), None));
        }
        links.push((key, name));
    }
    if let Some(m) = &snap.m17
        && m.state == "linked"
    {
        let key = format!("m17:{}", m.target);
        if m.receiving
            && let Some(call) = m.talker.as_deref().filter(|c| !c.trim().is_empty())
        {
            talkers.push((
                format!("{key}:{call}"),
                call.to_string(),
                Some(m.target.clone()),
            ));
        }
        links.push((key, m.target.clone()));
    }
    Seen { links, talkers }
}

impl Activity {
    #[must_use]
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            links: HashMap::new(),
            overs: HashMap::new(),
            feed: VecDeque::with_capacity(ACTIVITY_LEN),
            last_talker: None,
            day: 0,
            talk_ms: 0,
            transmissions: 0,
            stations: BTreeSet::new(),
        }
    }

    fn push(&mut self, at_ms: u64, text: String) {
        self.feed.push_front(PublicEvent {
            at_unix: at_ms / 1000,
            text,
        });
        self.feed.truncate(ACTIVITY_LEN);
    }

    fn roll_day(&mut self, now_ms: u64) {
        let day = day_of(now_ms);
        if day != self.day {
            self.day = day;
            self.talk_ms = 0;
            self.transmissions = 0;
            self.stations.clear();
        }
    }

    /// Feed one snapshot. `now_ms` is Unix time in milliseconds, injected so
    /// tests control the clock.
    pub fn observe(&mut self, snap: &NodeSnapshot, now_ms: u64) {
        self.roll_day(now_ms);
        let Seen { links, talkers } = seen(snap);

        // Links up and down.
        let now_links: HashMap<String, String> = links.into_iter().collect();
        let mut gone: Vec<(String, String)> = self
            .links
            .iter()
            .filter(|(k, _)| !now_links.contains_key(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        gone.sort();
        for (_, name) in gone {
            self.push(now_ms, format!("{name} unlinked"));
        }
        let mut new: Vec<(&String, &String)> = now_links
            .iter()
            .filter(|(k, _)| !self.links.contains_key(*k))
            .collect();
        new.sort();
        let new: Vec<String> = new.into_iter().map(|(_, v)| v.clone()).collect();
        for name in new {
            self.push(now_ms, format!("{name} linked"));
        }
        self.links = now_links;

        // Overs ending.
        let keyed: HashMap<String, (String, Option<String>)> =
            talkers.into_iter().map(|(k, n, v)| (k, (n, v))).collect();
        let mut ended: Vec<String> = self
            .overs
            .keys()
            .filter(|k| !keyed.contains_key(*k))
            .cloned()
            .collect();
        ended.sort();
        for key in ended {
            let over = self.overs.remove(&key).expect("listed above");
            let ms = now_ms.saturating_sub(over.started_ms);
            self.talk_ms += ms;
            self.transmissions += 1;
            self.stations.insert(over.name.clone());
            self.last_talker = Some(over.name.clone());
            if ms >= KERCHUNK_MS {
                if !over.announced {
                    self.push(over.started_ms, format!("{} keyed", over.name));
                }
                self.push(
                    now_ms,
                    format!("{} unkeyed · {}", over.name, duration_text(ms)),
                );
            }
        }
        // Overs starting, and overs outliving a kerchunk.
        for (key, (name, via)) in keyed {
            self.overs.entry(key).or_insert(Over {
                name,
                via,
                started_ms: now_ms,
                announced: false,
            });
        }
        let mut due: Vec<(u64, String)> = Vec::new();
        for over in self.overs.values_mut() {
            if !over.announced && now_ms.saturating_sub(over.started_ms) >= KERCHUNK_MS {
                over.announced = true;
                due.push((over.started_ms, over.name.clone()));
            }
        }
        due.sort();
        for (at, name) in due {
            self.push(at, format!("{name} keyed"));
        }
    }

    /// Build the public view from `snap` (the latest snapshot) and what has
    /// been observed. Reads only the fields named here.
    #[must_use]
    pub fn project(
        &self,
        snap: &NodeSnapshot,
        mode: &'static str,
        stream: StreamAvailability,
        listeners: u32,
        mp3: bool,
        now_ms: u64,
    ) -> PublicStatus {
        let mut links: Vec<PublicLink> = snap
            .links
            .iter()
            .filter(|l| l.state == astar_iax::LinkState::Up)
            .map(|l| PublicLink {
                network: "allstar",
                name: link_name(&l.node),
                direction: match l.mode {
                    astar_iax::LinkMode::Monitor | astar_iax::LinkMode::LocalMonitor => "monitor",
                    astar_iax::LinkMode::Transceive if l.addr.is_some() => "out",
                    astar_iax::LinkMode::Transceive => "in",
                },
                up_secs: l.up_secs,
            })
            .collect();
        if let Some(m) = &snap.m17
            && m.state == "linked"
        {
            links.push(PublicLink {
                network: "m17",
                name: m.target.clone(),
                direction: "receive",
                up_secs: m.up_secs.unwrap_or(0),
            });
        }
        let talker = self
            .overs
            .values()
            .min_by_key(|o| (o.started_ms, o.name.clone()))
            .map(|o| PublicTalker {
                name: o.name.clone(),
                via: o.via.clone(),
                secs: now_ms.saturating_sub(o.started_ms) / 1000,
            });
        let same_day = day_of(now_ms) == self.day;
        PublicStatus {
            node: snap.node_id.clone(),
            mode,
            links,
            talker,
            last_talker: self.last_talker.clone(),
            activity: self.feed.iter().cloned().collect(),
            today: if same_day {
                Today {
                    talk_secs: self.talk_ms / 1000,
                    transmissions: self.transmissions,
                    stations: u32::try_from(self.stations.len()).unwrap_or(u32::MAX),
                }
            } else {
                Today::default()
            },
            uptime_secs: self.started.elapsed().as_secs(),
            listeners,
            stream,
            mp3,
        }
    }
}

/// The wire name of a bridge mode.
#[must_use]
pub fn mode_name(mode: astar_iax::BridgeMode) -> &'static str {
    match mode {
        astar_iax::BridgeMode::Handset => "handset",
        astar_iax::BridgeMode::Bridge => "bridge",
        astar_iax::BridgeMode::Conference => "conference",
        astar_iax::BridgeMode::Parrot => "parrot",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::M17LinkStatus;
    use astar_iax::{LinkMode, LinkSnapshot, LinkState};

    const T0: u64 = 1_791_849_600_000; // 2026-10-02 00:00:00 UTC, in ms

    fn empty() -> NodeSnapshot {
        NodeSnapshot {
            node_id: Some("69586".into()),
            listening: true,
            registered: true,
            calls: vec![],
            links: vec![],
            discord: None,
            m17: None,
            m17_enabled: false,
            m17_available: false,
            m17_relay: None,
            echolink: None,
            echolink_enabled: false,
            public: None,
            nets: false,
        }
    }

    fn link(call: u64, node: &str, rx: bool, addr: Option<&str>) -> LinkSnapshot {
        LinkSnapshot {
            call,
            node: node.into(),
            mode: LinkMode::Transceive,
            state: LinkState::Up,
            keyed: false,
            rx_active: rx,
            up_secs: 61,
            addr: addr.map(str::to_string),
        }
    }

    fn with_links(links: Vec<LinkSnapshot>) -> NodeSnapshot {
        NodeSnapshot { links, ..empty() }
    }

    fn snapshot_with_link_addr(addr: &str) -> NodeSnapshot {
        let mut s = with_links(vec![link(7, "2560", true, Some(addr))]);
        s.m17 = Some(M17LinkStatus {
            reflector: "M17-USA".into(),
            module: "C".into(),
            target: "M17-USA C".into(),
            callsign: "AJ7HR".into(),
            state: "linked".into(),
            receiving: false,
            talker: None,
            talker_secs_ago: None,
            up_secs: Some(30),
            dropped_packets: 0,
            directory_generated: None,
        });
        s
    }

    fn sample_projection() -> PublicStatus {
        let snap = snapshot_with_link_addr("203.0.113.7:4569");
        let mut a = Activity::new(Instant::now());
        a.observe(&snap, T0);
        a.project(
            &snap,
            "bridge",
            StreamAvailability::Live,
            3,
            true,
            T0 + 1000,
        )
    }

    #[test]
    fn public_status_field_list_is_pinned() {
        let v = serde_json::to_value(sample_projection()).unwrap();
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "activity",
                "last_talker",
                "links",
                "listeners",
                "mode",
                "mp3",
                "node",
                "stream",
                "talker",
                "today",
                "uptime_secs"
            ]
        );
        let link = v["links"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(link, ["direction", "name", "network", "up_secs"]);
        let talker = v["talker"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(talker, ["name", "secs", "via"]);
        let today = v["today"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(today, ["stations", "talk_secs", "transmissions"]);
        let ev = v["activity"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(ev, ["at_unix", "text"]);
        assert_eq!(v["stream"], "live");
    }

    #[test]
    fn no_address_or_port_ever_appears() {
        let s = serde_json::to_string(&sample_projection()).unwrap();
        assert!(!s.contains("203.0.113.7") && !s.contains("4569"), "{s}");
        for bad in ["secret", "password", "token", "addr", "registered", "AJ7HR"] {
            assert!(!s.contains(bad), "{bad}: {s}");
        }
    }

    #[test]
    fn directions_and_m17_links_are_projected() {
        let mut snap = snapshot_with_link_addr("203.0.113.7:4569");
        snap.links.push(link(8, "", false, None));
        let mut mon = link(9, "2001", false, Some("x"));
        mon.mode = LinkMode::Monitor;
        snap.links.push(mon);
        let mut connecting = link(10, "3000", false, None);
        connecting.state = LinkState::Connecting;
        snap.links.push(connecting);
        let p = Activity::new(Instant::now()).project(
            &snap,
            "bridge",
            StreamAvailability::Off,
            0,
            false,
            T0,
        );
        let got: Vec<(&str, &str, &str)> = p
            .links
            .iter()
            .map(|l| (l.network, l.name.as_str(), l.direction))
            .collect();
        assert_eq!(
            got,
            [
                ("allstar", "2560", "out"),
                ("allstar", "a connected node", "in"),
                ("allstar", "2001", "monitor"),
                ("m17", "M17-USA C", "receive"),
            ]
        );
    }

    #[test]
    fn key_and_unkey_make_events_and_totals() {
        let mut a = Activity::new(Instant::now());
        a.observe(&with_links(vec![link(7, "2560", false, None)]), T0);
        a.observe(&with_links(vec![link(7, "2560", true, None)]), T0 + 1_000);
        let mid = with_links(vec![link(7, "2560", true, None)]);
        a.observe(&mid, T0 + 3_000);
        let p = a.project(
            &mid,
            "bridge",
            StreamAvailability::Live,
            0,
            true,
            T0 + 5_000,
        );
        assert_eq!(
            p.talker,
            Some(PublicTalker {
                name: "2560".into(),
                via: None,
                secs: 4
            })
        );
        a.observe(&with_links(vec![link(7, "2560", false, None)]), T0 + 23_000);
        let p = a.project(
            &mid,
            "bridge",
            StreamAvailability::Live,
            0,
            true,
            T0 + 23_000,
        );
        let texts: Vec<&str> = p.activity.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["2560 unkeyed · 0:22", "2560 keyed", "2560 linked"]);
        assert_eq!(p.activity[1].at_unix, (T0 + 1_000) / 1000);
        assert_eq!(
            p.today,
            Today {
                talk_secs: 22,
                transmissions: 1,
                stations: 1
            }
        );
        assert_eq!(p.last_talker.as_deref(), Some("2560"));
    }

    #[test]
    fn kerchunk_counts_but_is_not_in_the_feed() {
        let mut a = Activity::new(Instant::now());
        let idle = with_links(vec![link(7, "2560", false, None)]);
        a.observe(&idle, T0);
        a.observe(&with_links(vec![link(7, "2560", true, None)]), T0 + 1_000);
        a.observe(&idle, T0 + 1_300);
        let p = a.project(
            &idle,
            "bridge",
            StreamAvailability::Live,
            0,
            true,
            T0 + 2_000,
        );
        let texts: Vec<&str> = p.activity.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["2560 linked"]);
        assert_eq!(p.today.transmissions, 1);
        assert_eq!(p.today.stations, 1);
    }

    #[test]
    fn an_m17_talker_comes_via_its_reflector() {
        let mut snap = snapshot_with_link_addr("203.0.113.7:4569");
        snap.links.clear();
        let m = snap.m17.as_mut().unwrap();
        m.receiving = true;
        m.talker = Some("N0CALL".into());
        let mut a = Activity::new(Instant::now());
        a.observe(&snap, T0);
        let p = a.project(
            &snap,
            "bridge",
            StreamAvailability::Live,
            0,
            true,
            T0 + 2_000,
        );
        assert_eq!(
            p.talker,
            Some(PublicTalker {
                name: "N0CALL".into(),
                via: Some("M17-USA C".into()),
                secs: 2
            })
        );
    }

    #[test]
    fn activity_keeps_the_newest_twenty() {
        let mut a = Activity::new(Instant::now());
        let up = with_links(vec![link(7, "2560", false, None)]);
        let down = with_links(vec![]);
        for i in 0..15u64 {
            a.observe(&up, T0 + i * 2_000);
            a.observe(&down, T0 + i * 2_000 + 1_000);
        }
        let p = a.project(
            &down,
            "bridge",
            StreamAvailability::Off,
            0,
            false,
            T0 + 40_000,
        );
        assert_eq!(p.activity.len(), ACTIVITY_LEN);
        assert_eq!(p.activity[0].text, "2560 unlinked");
        assert_eq!(p.activity[0].at_unix, (T0 + 29_000) / 1000);
        assert!(p.activity.windows(2).all(|w| w[0].at_unix >= w[1].at_unix));
    }

    #[test]
    fn today_resets_at_midnight_utc() {
        let mut a = Activity::new(Instant::now());
        let idle = with_links(vec![link(7, "2560", false, None)]);
        let keyed = with_links(vec![link(7, "2560", true, None)]);
        let late = T0 + 86_400_000 - 10_000;
        a.observe(&idle, late);
        a.observe(&keyed, late + 1_000);
        a.observe(&idle, late + 3_000);
        let p = a.project(
            &idle,
            "bridge",
            StreamAvailability::Off,
            0,
            false,
            late + 4_000,
        );
        assert_eq!(p.today.transmissions, 1);
        // No snapshot since midnight: the projection still shows zeroes.
        let p = a.project(
            &idle,
            "bridge",
            StreamAvailability::Off,
            0,
            false,
            T0 + 86_400_000 + 5,
        );
        assert_eq!(p.today, Today::default());
        a.observe(&idle, T0 + 86_400_000 + 10);
        let p = a.project(
            &idle,
            "bridge",
            StreamAvailability::Off,
            0,
            false,
            T0 + 86_400_000 + 20,
        );
        assert_eq!(p.today, Today::default());
    }

    #[test]
    fn mode_names_are_the_config_spellings() {
        assert_eq!(mode_name(astar_iax::BridgeMode::Bridge), "bridge");
        assert_eq!(mode_name(astar_iax::BridgeMode::Handset), "handset");
        assert_eq!(mode_name(astar_iax::BridgeMode::Conference), "conference");
    }
}
