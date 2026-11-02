// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Typed parsers for the Asterisk / `app_rpt` CLI tables nodebench reads.
//!
//! Formats come from the source (`app_rpt` `rpt_cli.c` / `rpt_link.c`,
//! Asterisk `chan_iax2.c`); see `fixtures/README.md`. Every parser is
//! tolerant of leading/trailing blank lines and of the `asterisk -rx`
//! wrapper adding nothing else, and fails loudly with the offending line
//! rather than silently returning an empty table.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

/// A parse failure: what was expected, and the line that broke it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub what: &'static str,
    pub line: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not parse {}: {:?}", self.what, self.line)
    }
}

impl std::error::Error for ParseError {}

fn err(what: &'static str, line: &str) -> ParseError {
    ParseError {
        what,
        line: line.to_string(),
    }
}

// ---- rpt nodes --------------------------------------------------------------

/// How `app_rpt` reports a link's mode in `rpt nodes` (`__mklinklist`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RptLinkMode {
    /// `T` — transceive.
    Transceive,
    /// `R` — monitor (receive only).
    Monitor,
    /// `L` — local monitor.
    LocalMonitor,
    /// `C` — not yet connected.
    Connecting,
}

impl RptLinkMode {
    fn from_char(c: char) -> Option<Self> {
        match c {
            'T' => Some(Self::Transceive),
            'R' => Some(Self::Monitor),
            'L' => Some(Self::LocalMonitor),
            'C' => Some(Self::Connecting),
            _ => None,
        }
    }
}

/// One entry of `rpt nodes <n>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RptNode {
    pub mode: RptLinkMode,
    pub node: String,
}

/// `rpt nodes <n>`: every node reachable through this one's links (it
/// includes nodes linked transitively, not only direct links — use
/// [`parse_rpt_lstats`] for direct links).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RptNodes {
    pub nodes: Vec<RptNode>,
}

impl RptNodes {
    /// The entry for `node`, if listed.
    #[must_use]
    pub fn get(&self, node: &str) -> Option<&RptNode> {
        self.nodes.iter().find(|n| n.node == node)
    }
}

/// Parse `rpt nodes <n>`.
pub fn parse_rpt_nodes(text: &str) -> Result<RptNodes, ParseError> {
    let mut lines = text.lines();
    // Find the banner; everything after it is the comma-separated list.
    if !lines.by_ref().any(|l| l.contains("CONNECTED NODES")) {
        return Err(err(
            "rpt nodes banner",
            text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
        ));
    }
    let mut out = RptNodes::default();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line == "<NONE>" {
            continue;
        }
        for tok in line.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let mut chars = tok.chars();
            let mode = chars
                .next()
                .and_then(RptLinkMode::from_char)
                .ok_or_else(|| err("rpt nodes entry mode", tok))?;
            let node = chars.as_str();
            if node.is_empty() {
                return Err(err("rpt nodes entry node", tok));
            }
            out.nodes.push(RptNode {
                mode,
                node: node.to_string(),
            });
        }
    }
    Ok(out)
}

// ---- rpt lstats -------------------------------------------------------------

/// Link direction from `rpt lstats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
}

/// Link state from `rpt lstats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkConnState {
    Established,
    Connecting,
}

/// One row of `rpt lstats <n>`: a direct link of this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RptLinkStat {
    pub node: String,
    /// `${IAXPEER(CURRENTCHANNEL)}`, or `None` for `(none)`.
    pub peer: Option<String>,
    pub reconnects: u32,
    pub direction: Direction,
    /// Connect time in milliseconds.
    pub connect_ms: u64,
    pub state: LinkConnState,
}

/// Parse `HH:MM:SS:mmm` (lstats/stats TX time) or `HH:MM:SS` (uptime) to ms.
fn parse_hms(s: &str) -> Option<u64> {
    let parts: Vec<u64> = s
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match parts.as_slice() {
        [h, m, sec, ms] => Some(((h * 60 + m) * 60 + sec) * 1000 + ms),
        [h, m, sec] => Some(((h * 60 + m) * 60 + sec) * 1000),
        _ => None,
    }
}

/// Parse `rpt lstats <n>`.
pub fn parse_rpt_lstats(text: &str) -> Result<Vec<RptLinkStat>, ParseError> {
    let mut rows = Vec::new();
    let mut seen_header = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("NODE") && t.contains("CONNECT STATE") {
            seen_header = true;
            continue;
        }
        if t.starts_with("----") {
            continue;
        }
        if !seen_header {
            return Err(err("rpt lstats header", line));
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let [node, peer, reconnects, dir, time, state] = f.as_slice() else {
            return Err(err("rpt lstats row", line));
        };
        rows.push(RptLinkStat {
            node: (*node).to_string(),
            peer: (*peer != "(none)").then(|| (*peer).to_string()),
            reconnects: reconnects
                .parse()
                .map_err(|_| err("rpt lstats reconnects", line))?,
            direction: match *dir {
                "IN" => Direction::In,
                "OUT" => Direction::Out,
                _ => return Err(err("rpt lstats direction", line)),
            },
            connect_ms: parse_hms(time).ok_or_else(|| err("rpt lstats connect time", line))?,
            state: match *state {
                "ESTABLISHED" => LinkConnState::Established,
                "CONNECTING" => LinkConnState::Connecting,
                _ => return Err(err("rpt lstats state", line)),
            },
        });
    }
    if !seen_header {
        return Err(err("rpt lstats header", text.trim()));
    }
    Ok(rows)
}

// ---- rpt stats --------------------------------------------------------------

/// `rpt stats <n>`: every `label....: value` pair, plus typed accessors for
/// the fields scenarios use.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RptStats {
    pub node: String,
    /// Raw label → value (label without the dot leader).
    pub fields: BTreeMap<String, String>,
    /// `Nodes currently connected to us`, split (empty for `<NONE>`).
    pub connected: Vec<String>,
}

const CONNECTED_LABEL: &str = "Nodes currently connected to us";

impl RptStats {
    #[must_use]
    pub fn get(&self, label: &str) -> Option<&str> {
        self.fields.get(label).map(String::as_str)
    }

    /// `Signal on input` — the node's receiver is keyed (COS).
    #[must_use]
    pub fn signal_on_input(&self) -> Option<bool> {
        match self.get("Signal on input")? {
            "YES" => Some(true),
            "NO" => Some(false),
            _ => None,
        }
    }

    #[must_use]
    pub fn keyups_total(&self) -> Option<u64> {
        self.get("Keyups since system initialization")?.parse().ok()
    }

    #[must_use]
    pub fn uptime_ms(&self) -> Option<u64> {
        parse_hms(self.get("Uptime")?)
    }

    #[must_use]
    pub fn tx_time_total_ms(&self) -> Option<u64> {
        parse_hms(self.get("TX time since system initialization")?)
    }
}

/// Parse `rpt stats <n>`.
pub fn parse_rpt_stats(text: &str) -> Result<RptStats, ParseError> {
    let mut out = RptStats::default();
    let mut last_label: Option<String> = None;
    let mut seen_banner = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix('*') {
            // "****** NODE 1999 STATISTICS ******"
            let words: Vec<&str> = rest.split_whitespace().collect();
            if let Some(i) = words.iter().position(|w| *w == "NODE")
                && let Some(n) = words.get(i + 1)
            {
                out.node = (*n).to_string();
            }
            seen_banner = true;
            continue;
        }
        // Continuation of a wrapped value: whitespace, then ": ".
        if line.starts_with(' ') {
            if let (Some(label), Some(v)) = (&last_label, line.trim_start().strip_prefix(':')) {
                let entry = out.fields.entry(label.clone()).or_default();
                let v = v.trim();
                if !v.is_empty() {
                    if !entry.is_empty() && !entry.ends_with(',') {
                        entry.push(',');
                    }
                    if !entry.is_empty() {
                        entry.push(' ');
                    }
                    entry.push_str(v);
                }
                continue;
            }
            return Err(err("rpt stats continuation", line));
        }
        let Some((label, value)) = line.split_once(": ").or_else(|| {
            // A value can be empty: "label....: " with the space trimmed.
            line.strip_suffix(':').map(|l| (l, ""))
        }) else {
            return Err(err("rpt stats field", line));
        };
        let label = label.trim_end_matches('.').trim().to_string();
        if label.is_empty() {
            return Err(err("rpt stats label", line));
        }
        out.fields.insert(label.clone(), value.trim().to_string());
        last_label = Some(label);
    }
    if !seen_banner {
        return Err(err("rpt stats banner", text.trim()));
    }
    if let Some(v) = out.fields.get(CONNECTED_LABEL) {
        out.connected = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "<NONE>")
            .map(str::to_string)
            .collect();
    }
    Ok(out)
}

// ---- iax2 show channels -----------------------------------------------------

/// One row of `iax2 show channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IaxChannel {
    /// Channel name (truncated by Asterisk to 20 chars), `None` for `(None)`.
    pub channel: Option<String>,
    pub peer: String,
    pub username: Option<String>,
    pub local_call: u32,
    pub remote_call: u32,
    pub tx_seq: u32,
    pub rx_seq: u32,
    pub lag_ms: i64,
    /// `-1` when the jitter buffer is off.
    pub jitter_ms: i64,
    pub jitbuf_ms: i64,
    /// Asterisk's format name: `ulaw`, `slin`, `slin16`, ...
    pub format: String,
    /// e.g. `Tx:NEW`.
    pub first_msg: String,
    /// e.g. `Rx:LAGRP` — the last IAX control message, handy for a drop.
    pub last_msg: String,
}

/// `iax2 show channels` with its footer count.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct IaxChannels {
    pub channels: Vec<IaxChannel>,
    /// From `N active IAX channel(s)`.
    pub active: usize,
}

fn pair(s: &str, line: &str) -> Result<(u32, u32), ParseError> {
    let (a, b) = s.split_once('/').ok_or_else(|| err("a/b pair", line))?;
    Ok((
        a.parse().map_err(|_| err("a/b pair number", line))?,
        b.parse().map_err(|_| err("a/b pair number", line))?,
    ))
}

fn ms(s: &str, line: &str) -> Result<i64, ParseError> {
    s.strip_suffix("ms")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| err("NNms value", line))
}

fn none_to_opt(s: &str) -> Option<String> {
    (s != "(None)").then(|| s.to_string())
}

/// Parse `iax2 show channels`.
pub fn parse_iax2_channels(text: &str) -> Result<IaxChannels, ParseError> {
    let mut out = IaxChannels::default();
    let mut seen_header = false;
    let mut seen_footer = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("Channel") && t.contains("Peer") {
            seen_header = true;
            continue;
        }
        if t.ends_with("active IAX channel") || t.ends_with("active IAX channels") {
            out.active = t
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| err("iax2 channels footer", line))?;
            seen_footer = true;
            continue;
        }
        if !seen_header {
            return Err(err("iax2 show channels header", line));
        }
        let f: Vec<&str> = t.split_whitespace().collect();
        let [ch, peer, user, ids, seqs, lag, jit, jb, fmt, first, last] = f.as_slice() else {
            return Err(err("iax2 show channels row", line));
        };
        let (local_call, remote_call) = pair(ids, line)?;
        let (tx_seq, rx_seq) = pair(seqs, line)?;
        out.channels.push(IaxChannel {
            channel: none_to_opt(ch),
            peer: (*peer).to_string(),
            username: none_to_opt(user),
            local_call,
            remote_call,
            tx_seq,
            rx_seq,
            lag_ms: ms(lag, line)?,
            jitter_ms: ms(jit, line)?,
            jitbuf_ms: ms(jb, line)?,
            format: (*fmt).to_string(),
            first_msg: (*first).to_string(),
            last_msg: (*last).to_string(),
        });
    }
    if !seen_header || !seen_footer {
        return Err(err("iax2 show channels header/footer", text.trim()));
    }
    Ok(out)
}

// ---- iax2 show peers --------------------------------------------------------

/// Qualify status of a peer (`peer_status` in `chan_iax2.c`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "ms", rename_all = "snake_case")]
pub enum PeerStatus {
    Ok(u32),
    Lagged(u32),
    Unreachable,
    Unknown,
    Unmonitored,
}

/// One row of `iax2 show peers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IaxPeer {
    pub name: String,
    pub username: Option<String>,
    /// `(null)` when a dynamic peer has not registered.
    pub host: String,
    pub dynamic: bool,
    pub mask: String,
    /// `None` when Asterisk prints `(null)` (a dynamic peer not registered).
    pub port: Option<u16>,
    pub trunk: bool,
    pub encrypted: bool,
    pub status: PeerStatus,
    pub description: String,
}

/// `iax2 show peers` with its summary line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct IaxPeers {
    pub peers: Vec<IaxPeer>,
    pub total: usize,
    pub online: usize,
    pub offline: usize,
    pub unmonitored: usize,
}

impl IaxPeers {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&IaxPeer> {
        self.peers.iter().find(|p| p.name == name)
    }
}

/// Split `OK (1 ms) description words` into the status and the rest.
fn split_status(rest: &str, line: &str) -> Result<(PeerStatus, String), ParseError> {
    let paren_ms = |r: &str| -> Result<(u32, String), ParseError> {
        let r = r.trim_start();
        let inner = r
            .strip_prefix('(')
            .ok_or_else(|| err("peer status ms", line))?;
        let (num, after) = inner
            .split_once(" ms)")
            .ok_or_else(|| err("peer status ms", line))?;
        Ok((
            num.trim()
                .parse()
                .map_err(|_| err("peer status ms", line))?,
            after.trim().to_string(),
        ))
    };
    if let Some(r) = rest.strip_prefix("OK") {
        let (n, d) = paren_ms(r)?;
        return Ok((PeerStatus::Ok(n), d));
    }
    if let Some(r) = rest.strip_prefix("LAGGED") {
        let (n, d) = paren_ms(r)?;
        return Ok((PeerStatus::Lagged(n), d));
    }
    let (word, desc) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let status = match word {
        "UNREACHABLE" => PeerStatus::Unreachable,
        "UNKNOWN" => PeerStatus::Unknown,
        "Unmonitored" => PeerStatus::Unmonitored,
        _ => return Err(err("peer status", line)),
    };
    Ok((status, desc.trim().to_string()))
}

/// Parse `iax2 show peers`.
pub fn parse_iax2_peers(text: &str) -> Result<IaxPeers, ParseError> {
    let mut out = IaxPeers::default();
    let mut seen_header = false;
    let mut seen_footer = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("Name/Username") {
            seen_header = true;
            continue;
        }
        if t.contains("iax2 peers [") {
            // "4 iax2 peers [2 online, 1 offline, 1 unmonitored]"
            let (head, bracket) = t
                .split_once('[')
                .ok_or_else(|| err("iax2 peers footer", line))?;
            let nums: Vec<usize> = std::iter::once(head.split_whitespace().next().unwrap_or(""))
                .chain(bracket.split(|c: char| !c.is_ascii_digit()))
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            let [total, online, offline, unmonitored] = nums.as_slice() else {
                return Err(err("iax2 peers footer", line));
            };
            out.total = *total;
            out.online = *online;
            out.offline = *offline;
            out.unmonitored = *unmonitored;
            seen_footer = true;
            continue;
        }
        if !seen_header {
            return Err(err("iax2 show peers header", line));
        }
        // name host (D|S) mask port [(T)] [(E)] status... [description...]
        let mut toks = t.split_whitespace();
        let (Some(name), Some(host), Some(ds), Some(mask), Some(port)) = (
            toks.next(),
            toks.next(),
            toks.next(),
            toks.next(),
            toks.next(),
        ) else {
            return Err(err("iax2 show peers row", line));
        };
        let dynamic = match ds {
            "(D)" => true,
            "(S)" => false,
            _ => return Err(err("iax2 show peers (D)/(S)", line)),
        };
        let rest: Vec<&str> = toks.collect();
        let mut i = 0;
        let trunk = rest.get(i) == Some(&"(T)");
        if trunk {
            i += 1;
        }
        let encrypted = rest.get(i) == Some(&"(E)");
        if encrypted {
            i += 1;
        }
        let tail = rest[i..].join(" ");
        let (status, description) = split_status(&tail, line)?;
        let (name, username) = match name.split_once('/') {
            Some((n, u)) => (n.to_string(), Some(u.to_string())),
            None => (name.to_string(), None),
        };
        out.peers.push(IaxPeer {
            name,
            username,
            host: host.to_string(),
            dynamic,
            mask: mask.to_string(),
            port: if port == "(null)" {
                None
            } else {
                Some(
                    port.parse()
                        .map_err(|_| err("iax2 show peers port", line))?,
                )
            },
            trunk,
            encrypted,
            status,
            description,
        });
    }
    if !seen_header || !seen_footer {
        return Err(err("iax2 show peers header/footer", text.trim()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODES_TWO: &str = include_str!("fixtures/rpt_nodes_two.txt");
    const NODES_NONE: &str = include_str!("fixtures/rpt_nodes_none.txt");
    const NODES_WRAP: &str = include_str!("fixtures/rpt_nodes_wrap.txt");
    const LSTATS: &str = include_str!("fixtures/rpt_lstats.txt");
    const LSTATS_EMPTY: &str = include_str!("fixtures/rpt_lstats_empty.txt");
    const STATS: &str = include_str!("fixtures/rpt_stats.txt");
    const CHANNELS: &str = include_str!("fixtures/iax2_channels.txt");
    const CHANNELS_NONE: &str = include_str!("fixtures/iax2_channels_none.txt");
    const PEERS: &str = include_str!("fixtures/iax2_peers.txt");

    // ---- live captures from the rig (ASL3 node 1999 linked to 1998) ----

    #[test]
    fn live_captures_parse() {
        let nodes = parse_rpt_nodes(include_str!("fixtures/live_rpt_nodes.txt")).unwrap();
        assert_eq!(nodes.get("1998").unwrap().mode, RptLinkMode::Transceive);

        let lstats = parse_rpt_lstats(include_str!("fixtures/live_rpt_lstats.txt")).unwrap();
        assert_eq!(lstats.len(), 1);
        assert_eq!(lstats[0].node, "1998");
        // Live: the peer is a bare address, and the ms field is not padded.
        assert_eq!(lstats[0].peer.as_deref(), Some("127.0.0.1"));
        assert_eq!(lstats[0].direction, Direction::Out);
        assert_eq!(lstats[0].state, LinkConnState::Established);
        assert!(lstats[0].connect_ms > 0);

        let stats = parse_rpt_stats(include_str!("fixtures/live_rpt_stats.txt")).unwrap();
        assert_eq!(stats.node, "1999");
        assert_eq!(stats.connected, vec!["1998"]);
        assert_eq!(stats.signal_on_input(), Some(false));

        let chans = parse_iax2_channels(include_str!("fixtures/live_iax2_channels.txt")).unwrap();
        assert_eq!(chans.active, 1);
        // Live: Asterisk truncates the channel name to 20 chars.
        assert_eq!(
            chans.channels[0].channel.as_deref(),
            Some("IAX2/127.0.0.1:4570-")
        );
        assert_eq!(chans.channels[0].username.as_deref(), Some("radio"));
        assert_eq!(chans.channels[0].format, "ulaw");
        assert_eq!(chans.channels[0].last_msg, "Rx:ACK");

        let peers = parse_iax2_peers(include_str!("fixtures/live_iax2_peers.txt")).unwrap();
        let peer = peers.get("1998").unwrap();
        assert!(peer.dynamic);
        assert_eq!(peer.host, "(null)");
        assert_eq!(peer.port, None);
        assert_eq!(peer.status, PeerStatus::Unmonitored);
        assert_eq!(peers.unmonitored, 1);
    }

    #[test]
    fn rpt_nodes_two() {
        let n = parse_rpt_nodes(NODES_TWO).unwrap();
        assert_eq!(n.nodes.len(), 2);
        assert_eq!(n.get("1998").unwrap().mode, RptLinkMode::Transceive);
        assert_eq!(n.get("2000").unwrap().mode, RptLinkMode::Monitor);
    }

    #[test]
    fn rpt_nodes_none_is_empty() {
        assert_eq!(
            parse_rpt_nodes(NODES_NONE).unwrap().nodes,
            [] as [crate::node::parse::RptNode; 0]
        );
    }

    #[test]
    fn rpt_nodes_wraps_after_eight() {
        let n = parse_rpt_nodes(NODES_WRAP).unwrap();
        assert_eq!(n.nodes.len(), 10);
        assert_eq!(n.get("1998").unwrap().mode, RptLinkMode::Connecting);
        assert_eq!(n.nodes[8].node, "3008");
    }

    #[test]
    fn rpt_nodes_rejects_garbage() {
        assert!(parse_rpt_nodes("No such command 'rpt nodes'").is_err());
        let bad = "************************* CONNECTED NODES *****\n\nX1998\n";
        assert!(parse_rpt_nodes(bad).is_err());
    }

    #[test]
    fn rpt_lstats_rows() {
        let rows = parse_rpt_lstats(LSTATS).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            RptLinkStat {
                node: "1998".into(),
                peer: Some("127.0.0.1:4570".into()),
                reconnects: 0,
                direction: Direction::Out,
                connect_ms: 62_345,
                state: LinkConnState::Established,
            }
        );
        assert_eq!(rows[1].peer, None);
        assert_eq!(rows[1].reconnects, 3);
        assert_eq!(rows[1].direction, Direction::In);
        assert_eq!(rows[1].state, LinkConnState::Connecting);
    }

    #[test]
    fn rpt_lstats_empty_table() {
        assert_eq!(
            parse_rpt_lstats(LSTATS_EMPTY).unwrap(),
            [] as [crate::node::parse::RptLinkStat; 0]
        );
        assert!(parse_rpt_lstats("").is_err(), "no header is an error");
    }

    #[test]
    fn rpt_stats_fields() {
        let s = parse_rpt_stats(STATS).unwrap();
        assert_eq!(s.node, "1999");
        assert_eq!(s.signal_on_input(), Some(false));
        assert_eq!(s.keyups_total(), Some(5));
        assert_eq!(s.uptime_ms(), Some(3_723_000));
        assert_eq!(s.tx_time_total_ms(), Some(60_005));
        assert_eq!(s.get("Last DTMF command executed"), Some("N/A"));
        assert_eq!(s.get("User functions"), Some("ENABLED"));
        assert_eq!(s.connected, vec!["1998", "2000", "2001", "2002", "2003"]);
    }

    #[test]
    fn rpt_stats_none_connected() {
        let text = "*** NODE 1999 STATISTICS ***\n\nSignal on input..........: YES\nNodes currently connected to us..................: <NONE>\n";
        let s = parse_rpt_stats(text).unwrap();
        assert_eq!(s.connected, [] as [std::string::String; 0]);
        assert_eq!(s.signal_on_input(), Some(true));
    }

    #[test]
    fn iax2_channels_rows() {
        let c = parse_iax2_channels(CHANNELS).unwrap();
        assert_eq!(c.active, 2);
        assert_eq!(c.channels.len(), 2);
        let a = &c.channels[0];
        assert_eq!(a.channel.as_deref(), Some("IAX2/1998-4321"));
        assert_eq!(a.username.as_deref(), Some("1998"));
        assert_eq!((a.local_call, a.remote_call), (4321, 1));
        assert_eq!((a.tx_seq, a.rx_seq), (12, 11));
        assert_eq!(a.lag_ms, 1);
        assert_eq!(a.jitter_ms, -1);
        assert_eq!(a.format, "ulaw");
        assert_eq!(a.first_msg, "Tx:NEW");
        assert_eq!(a.last_msg, "Rx:LAGRP");
        let b = &c.channels[1];
        assert_eq!(b.channel, None);
        assert_eq!(b.username, None);
        assert_eq!(b.format, "slin16");
    }

    #[test]
    fn iax2_channels_none() {
        let c = parse_iax2_channels(CHANNELS_NONE).unwrap();
        assert_eq!(c.active, 0);
        assert_eq!(c.channels, [] as [crate::node::parse::IaxChannel; 0]);
        assert!(parse_iax2_channels("No such command").is_err());
    }

    #[test]
    fn iax2_peers_rows() {
        let peers = parse_iax2_peers(PEERS).unwrap();
        assert_eq!(
            (peers.total, peers.online, peers.offline, peers.unmonitored),
            (4, 2, 1, 1)
        );
        assert_eq!(peers.peers.len(), 4);
        let srv = peers.get("1998").unwrap();
        assert_eq!(srv.host, "127.0.0.1");
        assert_eq!(srv.port, Some(4570));
        assert!(!srv.dynamic);
        assert_eq!(srv.status, PeerStatus::Ok(1));
        assert_eq!(srv.description, "astar-server");
        let radio = peers.get("radio").unwrap();
        assert_eq!(radio.username.as_deref(), Some("radio"));
        assert!(radio.dynamic);
        assert_eq!(radio.host, "(null)");
        assert_eq!(radio.status, PeerStatus::Unmonitored);
        assert_eq!(radio.description, "");
        let trunk = peers.get("trunky").unwrap();
        assert!(trunk.trunk && trunk.encrypted);
        assert_eq!(trunk.status, PeerStatus::Unreachable);
        let lag = peers.get("lagger").unwrap();
        assert_eq!(lag.status, PeerStatus::Lagged(2500));
        assert_eq!(lag.description, "slow one");
    }
}
