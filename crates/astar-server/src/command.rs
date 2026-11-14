// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Command-and-event vocabulary for the astar-server daemon.
//!
//! Commands flow **inbound** (JSON → daemon); replies and events flow
//! **outbound** (daemon → JSON). `ProvideSecret` is inbound-only and is
//! never serialised or echoed.

/// Inbound commands from a client (stdin / HTTP / TUI).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum NodeCommand {
    Dial {
        node: String,
    },
    Hangup,
    Answer,
    Reject,
    Key,
    Unkey,
    EnableInbound,
    DisableInbound,
    Register,
    Deregister,
    SetDevices {
        input: Option<String>,
        output: Option<String>,
    },
    /// Inbound only — credentials supplied by the user. Never serialised/echoed.
    ProvideSecret {
        username: String,
        secret: String,
    },
    Status,
    Shutdown,
    Announce {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        sample: Option<String>,
        #[serde(default)]
        destination: Option<String>,
        #[serde(default)]
        mixunder: Option<bool>,
        #[serde(default)]
        gain_db: Option<f32>,
    },
    IdNow,
    /// Node-to-node link control (iax-d829.1, `POST /link`) — the `AllStar`
    /// `ilink` surface. `connect` = transceive (two-way), `monitor` = RX-only,
    /// and `disconnect` tears the link to `node` down. `addr` optionally dials
    /// an EXPLICIT `host:port` (harness / localhost / NAT-hairpin), bypassing
    /// `AllStar` DNS; omitted means resolve `node` via DNS.
    Link {
        action: LinkAction,
        node: String,
        #[serde(default)]
        addr: Option<String>,
    },
    /// Re-wire the conference bridge live (iax-647d, `POST /bridge`). Omitted
    /// fields keep their current value, so a partial body is a partial update.
    SetBridge {
        /// `"handset"` | `"bridge"` | `"conference"` (case-insensitive).
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        mix_minus: Option<bool>,
        #[serde(default)]
        include_local_radio: Option<bool>,
    },
    /// Discord voice bridge control (`POST /discord`). Listen-only: the only
    /// actions are joining and leaving a channel. There is no arm action
    /// because there is no transmit path to arm.
    Discord {
        action: DiscordAction,
    },
    /// One action from the node's own Discord admin page (`POST
    /// /discord/admin`). `action` is one of `"approve"`, `"revoke"`,
    /// `"ban"`, `"unban"`, `"make_admin"`, `"remove_admin"` (each of which
    /// REQUIRES `user_id`), or `"release_mic"`, `"enable"`, `"disable"`,
    /// `"hangup"` (which ignore it).
    ///
    /// A free-form `String` rather than an enum on purpose: the mapping to
    /// `astar_discord::AdminAction` lives behind the `discord` feature, and
    /// this vocabulary has to deserialize identically in a build that has no
    /// such feature so the refusal can name it.
    ///
    /// The control port is already this node's root of trust — it can
    /// `POST /key` — so the page adds no authentication of its own. It must
    /// not be reached by binding `[control]` to a public address.
    DiscordAdmin {
        action: String,
        /// The Discord user id the action applies to, as a string. Optional
        /// so the four actions that take no target can omit it entirely;
        /// the six that need one are refused without it.
        #[serde(default)]
        user_id: Option<String>,
    },
    /// Read the Discord admin state (`GET /discord/state`) — everything the
    /// admin page renders, in one snapshot.
    DiscordState,
    /// Receive-only M17 reflector monitor (`POST /m17`). No address field
    /// exists: the node resolves `reflector` from operator-owned data only
    /// (`[m17.reflectors]`, then `[m17] directory`), so nothing that
    /// arrives from the wire can point the node at a host.
    M17 {
        action: M17Action,
        #[serde(default)]
        reflector: Option<String>,
        #[serde(default)]
        module: Option<char>,
        #[serde(default)]
        callsign: Option<String>,
    },
    /// The node's `EchoLink` leg (`POST /echolink`): dial a station into the
    /// conference, or hang up on one. `target` is a node number or a
    /// callsign as the directory lists it, and nothing else: no field here
    /// can name a host or an address, so a station is only ever reached
    /// through the directory the node logged in to. Refused unless
    /// `[echolink]` is configured.
    EchoLink {
        action: EchoLinkAction,
        #[serde(default)]
        target: Option<String>,
    },
    /// Mint and drop a WT token with the RUNNING portal credentials
    /// (`POST /settings/portal/test`). Carries nothing: the credentials it
    /// tests are the ones the station was built with, so the settings page
    /// can say "these work" after a restart without a password ever
    /// crossing the wire a second time.
    PortalTest,
    /// Restart the daemon by re-exec (`POST /settings/restart`). Refused
    /// while this node is on a net — calls, links, a joined Discord bridge
    /// — unless `disconnect` is set, which is the page's confirmed second
    /// click. The controller sets its restart flag and then does exactly
    /// what `Shutdown` does; `run_serve` and `main` turn the flag into an
    /// exec instead of an exit.
    Restart {
        #[serde(default)]
        disconnect: bool,
    },
}

/// `POST /discord` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscordAction {
    Join,
    Leave,
}

/// `POST /m17` actions. Two variants: nothing in this vocabulary can express
/// transmit or an address, which is what keeps "the node only listens to
/// M17" a property of the types rather than of a check somebody must
/// remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M17Action {
    /// Link to a reflector module and relay what it carries into the
    /// conference. Receive-only.
    Monitor,
    /// Tear the monitor down (DISC to the reflector, member withdrawn).
    Disconnect,
}

/// `POST /echolink` actions. Two variants, and neither can express a host
/// or an address: a dial names a station the directory lists, a disconnect
/// names a station already on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoLinkAction {
    /// Call a listed station and add it to the conference.
    Dial,
    /// Hang up on one station (by callsign), or on every station when no
    /// target is given.
    Disconnect,
}

/// Discord bridge state in the node snapshot. Credential-free by
/// construction: there is no field in which a token could appear.
// Five bools, and they are five independent facts about five different
// things — a section with a token, a voice session, audio actually
// arriving in it, a registered command set, a master switch. This is a
// wire DTO whose shape IS the JSON the
// status page reads; folding any pair into an enum would change that JSON
// to satisfy a lint about state machines, and there is no state machine
// here.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscordStatus {
    /// A `[discord]` section is present AND a token has been supplied.
    pub configured: bool,
    /// **A bridge object is installed** — the node believes it is joined to
    /// the voice channel and is handing conference audio to it.
    ///
    /// It says nothing about whether that audio is arriving: the one
    /// long-lived songbird track can die with the bridge, the gateway and
    /// the voice connection all still in place, which is exactly what the
    /// first bridge did for its entire life. [`Self::audio_ok`] is that
    /// second question, and both are needed to read this status.
    pub connected: bool,
    /// The configured channel, for display.
    pub channel_id: Option<String>,
    /// Frames played into the channel since the bridge was created.
    pub frames_sent: u64,
    /// Last failure, human-readable. Never contains the token. Prefers the
    /// rule engine's own last error when it has one — a failed state-file
    /// write or a mute Discord refused is the thing an operator needs to
    /// see, and it outlives the audio-path error that may sit beneath it.
    pub last_error: Option<String>,
    /// **Audio is actually reaching Discord**: a bridge is installed AND
    /// its last play did not fail (`Bridge::last_error().is_none()`).
    ///
    /// Only meaningful beside [`Self::connected`]: false-with-connected is
    /// the fault worth alarming on — the bridge is up and the channel is
    /// hearing nothing — while false-with-not-connected is just an idle
    /// node that has never joined. A bridge that has joined but not yet
    /// played a frame reads `true`, because nothing has failed.
    pub audio_ok: bool,
    /// Slash commands are live: `[discord] approvers` is non-empty AND the
    /// gateway is up. False on a node that configured approvers but whose
    /// bot has not connected (no token yet, Discord unreachable), which is
    /// exactly the state an operator needs to tell apart from "off".
    pub commands: bool,
    /// The master switch inside the rule engine (`/bot state:on|off`, or
    /// the admin page). `true` when commands are unconfigured, because an
    /// unconfigured node has not turned anything off.
    pub enabled: bool,
    /// Who holds the one open mic, as a snowflake string, or `None`.
    pub mic_holder: Option<String>,
    /// What a slash command pointed the node at, if anything.
    pub link: Option<DiscordLinkStatus>,
}

/// The link the Discord rule engine believes it placed, flattened for the
/// wire.
///
/// Mirrors `astar_discord::LinkInfo` field for field rather than
/// re-exporting it, so `command.rs` — which is the daemon's whole outbound
/// vocabulary — keeps compiling, and keeps its shape, in a build with no
/// `discord` feature at all.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscordLinkStatus {
    /// The network's wire name: `"allstar"` or `"m17"`.
    pub network: String,
    /// What was dialled: an `AllStar` node number, or `"M17-USA C"`.
    pub target: String,
    /// The display name of whoever dialled it, or `"the node admin"`.
    pub by: String,
    /// The callsign presented to an M17 reflector; absent for `AllStar`.
    /// The dialer's Discord id is deliberately not mirrored here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Unix seconds when the dial was issued.
    pub since_unix: u64,
    /// Whether the target has actually appeared in the link roster.
    pub up: bool,
}

#[cfg(feature = "discord")]
impl From<&astar_discord::LinkInfo> for DiscordLinkStatus {
    fn from(l: &astar_discord::LinkInfo) -> Self {
        Self {
            network: l.network.as_str().to_string(),
            target: l.target.clone(),
            by: l.by.clone(),
            callsign: l.callsign.clone(),
            since_unix: l.since_unix,
            up: l.up,
        }
    }
}

/// Which `ilink`-style link operation a [`NodeCommand::Link`] requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkAction {
    /// `*3<node>` — full two-way link (transceive).
    Connect,
    /// `*2<node>` — receive-only link (monitor).
    Monitor,
    /// `*1<node>` — tear the link down.
    Disconnect,
}

/// Synchronous replies sent in response to a `NodeCommand`.
// `Snapshot` is the big variant and stays unboxed: one `NodeReply` exists
// per command, for as long as it takes to serialise it, so the size
// difference costs a few hundred bytes on one stack slot and boxing it
// would put a `Box` in every adapter's match arm. `DiscordState` IS boxed,
// because it is the newcomer and nothing yet depends on its shape.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum NodeReply {
    Ok,
    Snapshot(NodeSnapshot),
    /// Everything the Discord admin page renders. Carried as the rule
    /// engine's own type: it is already `Serialize`, and mirroring it here
    /// would be a second definition of the same nine lists to keep in step.
    ///
    /// Feature-gated because nothing without the `discord` feature can
    /// construct one — `DiscordState` there is refused before a reply
    /// exists. Credential-free: an `AdminView` carries user ids, display
    /// names and timers, and there is no field on it a token could reach.
    #[cfg(feature = "discord")]
    DiscordState(Box<astar_discord::AdminView>),
}

/// Asynchronous events pushed to the client.
// `Snapshot` dwarfs every other variant once it carries the M17 leg. One
// event exists per broadcast, cloned per subscriber and serialised once,
// so the size costs a few hundred bytes briefly; boxing `Snapshot` would
// change every match arm and the SSE path for no measurable gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NodeEvent {
    Snapshot(NodeSnapshot),
    IncomingCall {
        from: String,
    },
    Registered,
    RegisterFailed {
        reason: String,
    },
    Hangup {
        reason: String,
    },
    /// A voice/CW announcement has started. `kind` is a secret-free label:
    /// `"id"` for a periodic station-ID, `"event"` for an event-table trigger,
    /// `"command"` for a manual `Announce`/`IdNow` command, `"join"` for the
    /// greeting spoken to a newly joined inbound caller,
    /// `"link_connect"` / `"link_disconnect"` for an `AllStar` link dial or
    /// teardown, and `"m17_connect"` / `"m17_disconnect"` for an M17 monitor
    /// starting or ending. `"link_*"` and `"m17_*"` are sent only when the
    /// announcement reached at least one non-link member.
    AnnouncementStarted {
        kind: String,
    },
    /// A voice/CW announcement has finished (reserved for future use when the
    /// station surfaces a completion event).
    AnnouncementFinished {
        kind: String,
    },
    /// A node-to-node link lifecycle edge (iax-d829.1), secret-free. `kind` is
    /// `"connected"` / `"disconnected"` / `"keyed"`; `reason` is set only for
    /// disconnects, `keyed` only for key changes.
    Link {
        kind: String,
        node: String,
        call: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keyed: Option<bool>,
    },
    /// M17 monitor lifecycle edge, secret-free. `kind` is `"linking"`,
    /// `"connected"` or `"disconnected"`; `reason` is set only on
    /// `"disconnected"` and is one of the fixed end reasons ("hung up",
    /// "reflector refused the link", "node shutting down", …). Its own
    /// event rather than a synthetic `Link`: a `Link` names an IAX call,
    /// and faking one would mislead every roster check keyed on it.
    M17 {
        kind: String,
        reflector: String,
        module: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A DTMF digit received from a connected member (iax-2f5e), emitted for
    /// EVERY drained digit whether or not `[dtmf] enabled` maps it to a
    /// command — it is the only visibility operators have into whether a
    /// handset's tones are reaching the node at all. `command`, when set,
    /// reports the `*`-sequence this digit completed (e.g. `"connect 55553"`).
    Dtmf {
        call: u64,
        digit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
}

/// Point-in-time view of the daemon state.
// Four bools, and each is its own fact read by a different part of the
// status page: the listener, registration, whether `[m17]` is on, and
// whether a Codec 2 decoder loaded. Like `DiscordStatus` above, this is a
// wire DTO whose shape IS the JSON the page reads; folding any pair into an
// enum would change that JSON to satisfy a lint about state machines.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeSnapshot {
    /// This node's own ID (the register username, e.g. `"77777"`); `None`
    /// when registration is unconfigured. Page/UI title (iax-24e2).
    pub node_id: Option<String>,
    pub listening: bool,
    pub registered: bool,
    pub calls: Vec<astar_iax::CallSnapshot>,
    /// Live node-to-node links (iax-d829.1) — the `app_rpt` `RPT_LINKS`
    /// analogue, secret-free.
    pub links: Vec<astar_iax::LinkSnapshot>,
    /// Discord voice bridge (`[discord]`). `None` when the section is absent
    /// or the build has no `discord` feature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordStatus>,
    /// The node's receive-only M17 monitor leg, while one exists (from
    /// Monitor until it ends). `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m17: Option<M17LinkStatus>,
    /// An `[m17]` section is present with `enabled = true`. M17 is off
    /// without one, so this is what tells "off" from "broken".
    pub m17_enabled: bool,
    /// `Station::m17_available()`: a Codec 2 decoder can be loaded. With
    /// `m17_enabled` it drives the page's "M17: no codec" chip.
    pub m17_available: bool,
    /// Who the M17 monitor's audio reaches right now, and the warning that
    /// says so. Present exactly when `m17_enabled`: the monitor contributes
    /// to the conference, so whatever the reflector carries is relayed to the
    /// node's `AllStar` and `EchoLink` members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m17_relay: Option<crate::m17_relay::M17RelayStatus>,
    /// The node's `EchoLink` login and the stations on its leg. `Some`
    /// exactly when `[echolink]` is configured. Never carries the password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echolink: Option<EchoLinkNodeStatus>,
    /// An `[echolink]` section is present. `EchoLink` is dormant without
    /// one, so this is what tells "off" from "not logged in".
    pub echolink_enabled: bool,
    /// `[netrec]` is set, so `/nets/` answers: the status page shows its
    /// "Nets" link. Left out of the JSON when off, so a node without the
    /// table sends exactly what it always did.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub nets: bool,
    /// The public listener (`[web]`): hostnames, certificate expiry and the
    /// last ACME or DNS error. `None` without a public page. Admin-only:
    /// the public status is a separate projection that never includes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<crate::public::serve::PublicTlsStatus>,
}

/// The node's M17 monitor leg, flattened for the wire. Secret-free: M17 has
/// no password, and there is no field a node or portal secret could reach.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct M17LinkStatus {
    /// Uppercase reflector name, e.g. `"M17-USA"`.
    pub reflector: String,
    /// The module letter, e.g. `"C"`.
    pub module: String,
    /// `"{reflector} {module}"`, the label every surface prints.
    pub target: String,
    /// The callsign the reflector sees. Shown because the identity the node
    /// presented to a third party is exactly what an operator must be able
    /// to audit.
    pub callsign: String,
    /// `"linking"`, `"linked"` or `"failed"` (the last only until the pump
    /// reaps the leg).
    pub state: String,
    /// A stream packet arrived within the last 400 ms.
    pub receiving: bool,
    /// Last station heard. Attacker-supplied text: render as text only.
    pub talker: Option<String>,
    /// Seconds since `talker` was last heard.
    pub talker_secs_ago: Option<u64>,
    /// Seconds since ACKN. `None` while linking.
    pub up_secs: Option<u64>,
    /// Stream packets the monitor's flood gate dropped.
    pub dropped_packets: u64,
    /// The directory snapshot's date, so a stale bundled list is visible.
    pub directory_generated: Option<String>,
}

/// The node's `EchoLink` card on `/status`. Secret-free: the password is
/// never read into anything that reaches this struct.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EchoLinkNodeStatus {
    /// Where the directory login stands: `"logged_out"`, `"logging_in"`,
    /// `"logged_in"` or `"failed"`.
    pub login: String,
    /// The node's `EchoLink` callsign, `-L` or `-R` suffixed.
    pub callsign: String,
    /// Why the node is not logged in, for a person to read: a missing
    /// password, or the directory's refusal. `None` when nothing is wrong.
    pub error: Option<String>,
    /// Every station on the node's leg, in the order they were dialled.
    pub peers: Vec<EchoLinkPeerRow>,
}

/// One station on the node's `EchoLink` leg.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EchoLinkPeerRow {
    /// The station's callsign as the directory lists it. Directory-supplied
    /// text: render as text only.
    pub call: String,
    /// The station's node number.
    pub node: u32,
    /// `"connecting"`, `"connected"` or `"closed"` (the last only until the
    /// pump reaps it).
    pub state: String,
    /// The station is talking.
    pub talking: bool,
    /// Seconds since the station answered. `None` while connecting.
    pub up_secs: Option<u64>,
    /// Why the node stopped sending the net to this station (the 10-minute
    /// transmit cap), a plain sentence; `None` otherwise. Contains the
    /// directory-supplied callsign: render as text only.
    pub notice: Option<String>,
}

/// Error payload sent to the client — never contains credentials.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_deserializes_from_json() {
        let c: NodeCommand = serde_json::from_str(r#"{"cmd":"dial","node":"55553"}"#).unwrap();
        assert!(matches!(c, NodeCommand::Dial { node } if node == "55553"));
        let s: NodeCommand = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert!(matches!(s, NodeCommand::Status));
    }

    /// The whole M17 vocabulary is two verbs. Nothing that parses can ask
    /// for transmit, for an address, or for anything but monitoring.
    #[test]
    fn m17_command_has_monitor_and_disconnect_and_nothing_else() {
        let c: NodeCommand = serde_json::from_str(
            r#"{"cmd":"m17","action":"monitor","reflector":"M17-USA","module":"C","callsign":"AJ7HR"}"#,
        )
        .unwrap();
        let NodeCommand::M17 {
            action,
            reflector,
            module,
            callsign,
        } = c
        else {
            panic!("`cmd: m17` must parse as NodeCommand::M17");
        };
        assert_eq!(action, M17Action::Monitor);
        assert_eq!(reflector.as_deref(), Some("M17-USA"));
        assert_eq!(module, Some('C'));
        assert_eq!(callsign.as_deref(), Some("AJ7HR"));

        let d: NodeCommand =
            serde_json::from_str(r#"{"cmd":"m17","action":"disconnect"}"#).unwrap();
        assert!(
            matches!(
                d,
                NodeCommand::M17 {
                    action: M17Action::Disconnect,
                    reflector: None,
                    module: None,
                    callsign: None,
                }
            ),
            "{d:?}"
        );

        for verb in ["transmit", "connect", "key", "arm"] {
            let body = format!(
                r#"{{"cmd":"m17","action":"{verb}","reflector":"M17-USA","module":"C","callsign":"AJ7HR"}}"#
            );
            assert!(
                serde_json::from_str::<NodeCommand>(&body).is_err(),
                "{verb:?} must not parse: the M17 vocabulary is monitor and disconnect only"
            );
        }
    }

    #[test]
    fn m17_event_serializes_exactly() {
        let linking = NodeEvent::M17 {
            kind: "linking".into(),
            reflector: "M17-USA".into(),
            module: "C".into(),
            reason: None,
        };
        assert_eq!(
            serde_json::to_string(&linking).unwrap(),
            r#"{"event":"m17","kind":"linking","reflector":"M17-USA","module":"C"}"#
        );
        let ended = NodeEvent::M17 {
            kind: "disconnected".into(),
            reflector: "M17-USA".into(),
            module: "C".into(),
            reason: Some("reflector refused the link".into()),
        };
        assert_eq!(
            serde_json::to_string(&ended).unwrap(),
            r#"{"event":"m17","kind":"disconnected","reflector":"M17-USA","module":"C","reason":"reflector refused the link"}"#
        );
    }

    #[test]
    fn m17_link_status_serializes_in_contract_order() {
        let s = M17LinkStatus {
            reflector: "M17-USA".into(),
            module: "C".into(),
            target: "M17-USA C".into(),
            callsign: "AJ7HR".into(),
            state: "linked".into(),
            receiving: false,
            talker: Some("KC1ABC".into()),
            talker_secs_ago: Some(3),
            up_secs: Some(125),
            dropped_packets: 0,
            directory_generated: Some("2026-09-15".into()),
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"reflector":"M17-USA","module":"C","target":"M17-USA C","callsign":"AJ7HR","state":"linked","receiving":false,"talker":"KC1ABC","talker_secs_ago":3,"up_secs":125,"dropped_packets":0,"directory_generated":"2026-09-15"}"#
        );
    }

    /// With no leg, `m17` is omitted and both flags are still on the wire:
    /// the status page needs `m17_enabled && !m17_available` to show its
    /// "no codec" chip.
    #[test]
    fn snapshot_without_an_m17_leg_serializes_exactly() {
        let snap = NodeSnapshot {
            node_id: None,
            listening: true,
            registered: false,
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
        };
        assert_eq!(
            serde_json::to_string(&snap).unwrap(),
            r#"{"node_id":null,"listening":true,"registered":false,"calls":[],"links":[],"m17_enabled":false,"m17_available":false,"echolink_enabled":false}"#
        );
    }

    #[test]
    fn dtmf_event_serializes_secret_free_with_optional_command() {
        let bare = NodeEvent::Dtmf {
            call: 7,
            digit: "5".into(),
            command: None,
        };
        let j = serde_json::to_string(&bare).unwrap();
        assert!(j.contains(r#""event":"dtmf""#), "{j}");
        assert!(j.contains(r#""digit":"5""#), "{j}");
        assert!(!j.contains("command"), "omitted when absent: {j}");

        let done = NodeEvent::Dtmf {
            call: 7,
            digit: "#".into(),
            command: Some("connect 55553".into()),
        };
        let j = serde_json::to_string(&done).unwrap();
        assert!(j.contains(r#""command":"connect 55553""#), "{j}");
        for banned in ["secret", "password", "token"] {
            assert!(!j.contains(banned), "{banned} must not appear: {j}");
        }
    }

    #[test]
    fn link_command_deserializes_with_action_and_optional_addr() {
        let c: NodeCommand =
            serde_json::from_str(r#"{"cmd":"link","action":"connect","node":"55553"}"#).unwrap();
        assert!(
            matches!(c, NodeCommand::Link { action: LinkAction::Connect, ref node, addr: None } if node == "55553")
        );
        let m: NodeCommand =
            serde_json::from_str(r#"{"cmd":"link","action":"monitor","node":"42"}"#).unwrap();
        assert!(matches!(
            m,
            NodeCommand::Link {
                action: LinkAction::Monitor,
                ..
            }
        ));
        let d: NodeCommand = serde_json::from_str(
            r#"{"cmd":"link","action":"disconnect","node":"42","addr":"127.0.0.1:4569"}"#,
        )
        .unwrap();
        assert!(matches!(
            d,
            NodeCommand::Link {
                action: LinkAction::Disconnect,
                addr: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn snapshot_with_links_serializes_secret_free() {
        let snap = NodeSnapshot {
            node_id: None,
            listening: true,
            registered: false,
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
        };
        let j = serde_json::to_string(&snap).unwrap();
        assert!(j.contains("\"links\":[]"), "links field present: {j}");
    }

    #[test]
    fn announce_command_deserializes() {
        let c: NodeCommand = serde_json::from_str(
            r#"{"cmd":"announce","text":"net in five minutes","destination":"to_air"}"#,
        )
        .unwrap();
        assert!(matches!(c, NodeCommand::Announce { text: Some(_), .. }));
        let c2: NodeCommand = serde_json::from_str(r#"{"cmd":"id_now"}"#).unwrap();
        assert!(matches!(c2, NodeCommand::IdNow));
    }

    #[test]
    fn discord_status_serialises_without_credentials() {
        let s = DiscordStatus {
            configured: true,
            connected: false,
            channel_id: Some("222".into()),
            frames_sent: 0,
            last_error: Some("voice link failed".into()),
            audio_ok: false,
            commands: true,
            enabled: true,
            mic_holder: Some("333".into()),
            link: Some(DiscordLinkStatus {
                network: "allstar".into(),
                target: "55553".into(),
                by: "kd0xyz".into(),
                callsign: None,
                since_unix: 1_789_851_851,
                up: true,
            }),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains(r#""configured":true"#), "{j}");
        assert!(
            j.contains(r#""audio_ok":false"#),
            "the page needs the audio fact on the wire, not only `connected`: {j}"
        );
        assert!(j.contains(r#""commands":true"#), "{j}");
        assert!(j.contains(r#""mic_holder":"333""#), "{j}");
        assert!(j.contains(r#""target":"55553""#), "the link is nested: {j}");
        assert!(!j.to_lowercase().contains("token"), "no token key: {j}");
    }

    #[test]
    fn discord_link_status_names_an_m17_callsign_and_omits_an_absent_one() {
        let m17 = DiscordLinkStatus {
            network: "m17".into(),
            target: "M17-USA C".into(),
            by: "kd0xyz".into(),
            callsign: Some("AJ7HR".into()),
            since_unix: 1_789_851_851,
            up: true,
        };
        assert_eq!(
            serde_json::to_string(&m17).unwrap(),
            r#"{"network":"m17","target":"M17-USA C","by":"kd0xyz","callsign":"AJ7HR","since_unix":1789851851,"up":true}"#
        );
        let allstar = DiscordLinkStatus {
            network: "allstar".into(),
            target: "55553".into(),
            by: "kd0xyz".into(),
            callsign: None,
            since_unix: 1_789_851_851,
            up: true,
        };
        assert_eq!(
            serde_json::to_string(&allstar).unwrap(),
            r#"{"network":"allstar","target":"55553","by":"kd0xyz","since_unix":1789851851,"up":true}"#,
            "an AllStar link's /status JSON is exactly what it was"
        );
    }

    /// `network` is the wire name, not `Display`: the page's
    /// `networkName("m17")` matches on it, and `"AllStar"` would render as an
    /// unknown network.
    #[cfg(feature = "discord")]
    #[test]
    fn discord_link_status_takes_the_wire_network_and_the_callsign_from_link_info() {
        let l = astar_discord::LinkInfo {
            network: astar_discord::Network::M17,
            target: "M17-USA C".into(),
            by: "u300".into(),
            by_id: Some(300),
            callsign: Some("AJ7HR".into()),
            since_unix: 1_000,
            up: false,
        };
        assert_eq!(
            serde_json::to_string(&DiscordLinkStatus::from(&l)).unwrap(),
            r#"{"network":"m17","target":"M17-USA C","by":"u300","callsign":"AJ7HR","since_unix":1000,"up":false}"#,
            "the dialer's Discord id never reaches /status"
        );
        let a = astar_discord::LinkInfo {
            network: astar_discord::Network::Allstar,
            target: "55553".into(),
            by: "u300".into(),
            by_id: Some(300),
            callsign: None,
            since_unix: 1_000,
            up: true,
        };
        assert_eq!(DiscordLinkStatus::from(&a).network, "allstar");
    }

    #[test]
    fn snapshot_serializes_and_is_secret_free() {
        let snap = NodeSnapshot {
            node_id: None,
            listening: true,
            registered: false,
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
        };
        let j = serde_json::to_string(&snap).unwrap();
        assert!(j.contains("\"listening\":true"));
        for bad in ["secret", "password", "token"] {
            assert!(
                !j.contains(bad),
                "serialised snapshot contains forbidden word: {bad}"
            );
        }
    }

    #[test]
    fn portal_test_and_restart_commands_deserialize() {
        let p: NodeCommand = serde_json::from_str(r#"{"cmd":"portal_test"}"#).unwrap();
        assert!(matches!(p, NodeCommand::PortalTest));
        // `disconnect` defaults off: the page's first click is a request,
        // and only the confirmed second one carries the override.
        let bare: NodeCommand = serde_json::from_str(r#"{"cmd":"restart"}"#).unwrap();
        assert!(matches!(bare, NodeCommand::Restart { disconnect: false }));
        let forced: NodeCommand =
            serde_json::from_str(r#"{"cmd":"restart","disconnect":true}"#).unwrap();
        assert!(matches!(forced, NodeCommand::Restart { disconnect: true }));
        assert_eq!(
            format!("{:?}", NodeCommand::Restart { disconnect: true }),
            "Restart { disconnect: true }",
            "neither new variant has a field a credential could sit in"
        );
    }
}
