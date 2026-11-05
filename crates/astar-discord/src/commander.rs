// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Every rule the Discord slash commands obey, as one network-free state
//! machine.
//!
//! [`Commander`] is the whole product. The serenity layer above it turns
//! gateway traffic into [`GatewayEvent`] values and pushes replies back
//! through [`GatewayOps`]; the node controller below it answers
//! [`NodeOps`]. Neither of them contains a single decision — if a rule is
//! anywhere but this file it can be changed without a test noticing.
//!
//! **The safety property this module exists to hold:** [`NodeOps`] has no
//! way to express a transceive link, a host or a port. Its two dial methods
//! are `dial_allstar_monitor` and `dial_m17_monitor`, both receive-only by
//! name and by contract. The first is reached only with a string that has
//! passed [`validate_allstar_node`]. The second is reached only with a name
//! that has passed [`validate_reflector`], an ASCII module letter and the
//! caller's own registered callsign. No slash command can key anything, and
//! no slash command can point the node at an address. Do not widen this
//! trait.
//!
//! The tiers, widest first: an **approver** is an id in `node.toml`, and is
//! config rather than state — never banned, revoked or demoted from inside
//! Discord. An **admin** is an approver or someone in the persisted admin
//! list. **Allowed** (the tier `/dial`, `/hangup` and `/mic` need) is
//! approver, admin or approved, and not banned.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::approvals::{Approvals, ApprovalsError, UserEntry};
use crate::commands::{
    AdminAction, BotCommand, BotCommandKind, DialTarget, GatewayEvent, MODULE_RULE, UserRef,
    VoiceEvent, validate_allstar_node, validate_callsign, validate_reflector,
};

/// How the rule engine speaks to Discord.
///
/// Three verbs and no getters: everything the engine knows it knows from
/// the events it was handed, which is what makes it replayable in a test.
pub trait GatewayOps {
    /// Answer the caller of one interaction. Always ephemeral.
    fn reply(&mut self, interaction: u64, text: &str);
    /// Post a public line into the voice channel's text chat. Pings nobody
    /// — the channel is the audit trail of who pointed the node where, not
    /// a notification firehose.
    fn announce(&mut self, text: &str);
    /// Server-mute (or unmute) one member.
    fn set_mute(&mut self, user: u64, mute: bool);
    /// Redraw the channel's PTT panel: the holder's display name, or
    /// `None` when nobody holds the mic.
    fn panel(&mut self, talking: Option<&str>);
}

/// How the rule engine speaks to the node.
///
/// Deliberately tiny, and deliberately monitor-only. See the module docs.
pub trait NodeOps {
    /// Whether the bridge is currently in the voice channel.
    fn bridge_joined(&self) -> bool;
    /// Every link the node holds, however it got there: the `AllStar` link
    /// roster, plus the node's M17 leg **only while it is linked**. A leg
    /// still waiting for its ACKN is left out. That is what lets `tick_link`
    /// say "Connected" at the moment the reflector accepts, with no state of
    /// its own.
    fn links(&self) -> Vec<LinkRef>;
    /// Dial one `AllStar` node, receive-only.
    ///
    /// # Errors
    /// The engine's own message, which is shown to the caller verbatim.
    fn dial_allstar_monitor(&mut self, node: &str) -> Result<(), String>;
    /// Listen to one M17 reflector module, presenting `callsign`.
    ///
    /// Receive-only by name and by contract. There is no host, no port and
    /// no transmit parameter, so no caller can aim the node at an address or
    /// put audio on the reflector. The node resolves the name from its own
    /// configuration.
    ///
    /// `Ok` carries the node's warning about where the reflector's audio
    /// goes: the monitor never sends voice to the reflector, but the node
    /// relays what it hears to every `AllStar` and `EchoLink` member, and a
    /// linked radio transmits it. The reply and the channel line both quote
    /// it, so nobody dials without being told.
    ///
    /// # Errors
    /// The node's own message, which is shown to the caller verbatim.
    fn dial_m17_monitor(
        &mut self,
        reflector: &str,
        module: char,
        callsign: &str,
    ) -> Result<String, String>;
    /// Hang up one link.
    ///
    /// # Errors
    /// The engine's own message, which is shown to the caller verbatim.
    fn hangup(&mut self, link: &LinkRef) -> Result<(), String>;
    /// Why the node's own leg for `link` ended, once it has. `None` while it
    /// is up, and for a network whose legs report no reason.
    fn link_end_reason(&self, link: &LinkRef) -> Option<String>;
    /// `(callsign, seconds ago)` of the newest station heard on `link`. The
    /// callsign is whatever the far end sent: plain text, never markup.
    fn last_heard(&self, link: &LinkRef) -> Option<(String, u64)>;
}

/// Which network a link is on.
///
/// `Display` is the name people read (`"AllStar"`, `"M17"`, `"D-Star"`) and
/// is what every announced line uses. [`Network::as_str`] and serde give the
/// lowercase wire name that `/status` and the admin page already speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// `AllStarLink`, over IAX2.
    Allstar,
    /// An M17 reflector module. On this node that is only ever a
    /// receive-only monitor.
    M17,
    /// A D-Star reflector module. Named so the vocabulary is complete; no
    /// dial reaches one yet.
    Dstar,
}

impl Network {
    /// The lowercase wire name: `"allstar"`, `"m17"` or `"dstar"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allstar => "allstar",
            Self::M17 => "m17",
            Self::Dstar => "dstar",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allstar => "AllStar",
            Self::M17 => "M17",
            Self::Dstar => "D-Star",
        })
    }
}

/// One link the node holds, as the rule engine names it.
///
/// `target` is what the network calls the far end: a node number for
/// `AllStar`, `"<reflector> <module>"` for M17. `Display` reads as the
/// network and then the target (`"AllStar 543260"`, `"M17 M17-USA C"`),
/// which is exactly how every announced line has always named an `AllStar`
/// link. That is what keeps those lines byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkRef {
    /// Which network.
    pub network: Network,
    /// The far end, as that network names it.
    pub target: String,
}

impl LinkRef {
    /// An `AllStar` node.
    #[must_use]
    pub fn allstar(node: &str) -> LinkRef {
        LinkRef {
            network: Network::Allstar,
            target: node.to_string(),
        }
    }

    /// One module of an M17 reflector; `target` is `"M17-USA C"`.
    #[must_use]
    pub fn m17(reflector: &str, module: char) -> LinkRef {
        LinkRef {
            network: Network::M17,
            target: format!("{reflector} {module}"),
        }
    }
}

impl std::fmt::Display for LinkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.network, self.target)
    }
}

/// The links as one phrase, grouped by network in first-seen order:
/// `"AllStar 1, 2"`, `"AllStar 543260, M17 M17-USA C"`.
///
/// Every line that names more than one link uses this one function:
/// `/hangup`'s reply and announce, and the hourly heartbeat. So they cannot
/// drift apart, and two `AllStar` links still read the way the old
/// comma-joined roster did.
#[must_use]
pub fn link_labels(links: &[LinkRef]) -> String {
    let mut groups: Vec<(Network, Vec<&str>)> = Vec::new();
    for l in links {
        match groups.iter_mut().find(|(n, _)| *n == l.network) {
            Some((_, targets)) => targets.push(&l.target),
            None => groups.push((l.network, vec![l.target.as_str()])),
        }
    }
    groups
        .iter()
        .map(|(n, targets)| format!("{n} {}", targets.join(", ")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the node is linked to, as the commander believes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LinkInfo {
    /// Which network. Serialises as its wire name (`"allstar"`, `"m17"`),
    /// so the admin page's JSON for an `AllStar` link is what it always was.
    pub network: Network,
    /// What was dialled: an `AllStar` node number, or `"M17-USA C"`.
    pub target: String,
    /// The display name of whoever dialled it, or `"the node admin"`.
    pub by: String,
    /// The Discord id of whoever dialled it. It is kept so that a revoke or
    /// ban can hang up the M17 leg that presents that person's callsign. It
    /// is never serialised: the page already shows the name.
    #[serde(skip_serializing)]
    pub by_id: Option<u64>,
    /// The callsign presented to the reflector. M17 only; an `AllStar`
    /// monitor presents none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Unix seconds when the dial was issued.
    pub since_unix: u64,
    /// Whether the target has actually appeared in the link roster.
    pub up: bool,
}

impl LinkInfo {
    /// The roster entry this record stands for, so the commander compares
    /// and names its own link exactly as the node reports its roster.
    fn link_ref(&self) -> LinkRef {
        LinkRef {
            network: self.network,
            target: self.target.clone(),
        }
    }
}

/// Everything the admin page renders, in one snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminView {
    /// The master switch.
    pub enabled: bool,
    /// Config approvers, read-only on the page. Strings because JavaScript
    /// cannot hold a snowflake as a number.
    pub approvers: Vec<String>,
    /// The persisted admin list.
    pub admins: Vec<UserEntry>,
    /// Requests waiting for an approver.
    pub pending: Vec<UserEntry>,
    /// Approved users.
    pub approved: Vec<UserEntry>,
    /// Banned users.
    pub banned: Vec<UserEntry>,
    /// Callsign by user id, for everyone who has ever `/register`ed one —
    /// whichever list they are on now, and approvers included.
    pub callsigns: BTreeMap<String, String>,
    /// Who is in the bridged voice channel right now.
    pub present: Vec<PresentUser>,
    /// Who holds the mic, if anyone.
    pub mic_holder: Option<String>,
    /// The current link, if any.
    pub link: Option<LinkInfo>,
    /// The last thing that went wrong — a failed state-file write, or a
    /// mute Discord would not let us make.
    pub last_error: Option<String>,
    /// The hard time-out timer, in seconds; 0 is off.
    pub mic_timeout_secs: u64,
    /// The silence unkey window, in seconds; 0 is off.
    pub mic_silence_secs: u64,
    /// Whether the holder is sending voice packets right now.
    pub talking: bool,
}

/// One member of the bridged voice channel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PresentUser {
    /// The snowflake, as a string.
    pub user_id: String,
    /// The display name last seen.
    pub name: String,
    /// Their server-mute flag as of the last voice event seen for them.
    pub server_muted: bool,
}

// ---------------------------------------------------------------------------
// The exact words. Every refusal is a constant so a test can name it and a
// reviewer can read the whole vocabulary in one screen.
// ---------------------------------------------------------------------------

const BOT_OFF: &str = "Discord commands are turned off by the node admin.";
const REQUEST_BANNED: &str = "You can't request access on this node.";
const ALREADY_APPROVED: &str = "You're already approved.";
const ALREADY_PENDING: &str = "Your request is already waiting for an approver.";
const NOT_ADMIN: &str = "Only this node's admins can do that.";
const ADMIN_PROTECTED: &str = "Only an approver or the node's admin page can act on an admin.";
const APPROVER_NOT_REVOCABLE: &str =
    "Approvers are set in the node's config and can't be revoked here.";
const APPROVER_NOT_BANNABLE: &str =
    "Approvers are set in the node's config and can't be banned here.";
const APPROVER_NOT_REMOVABLE: &str =
    "Approvers are set in the node's config and can't be removed here.";
const NOT_APPROVER: &str =
    "Only this node's approvers (set in its config) can make or remove admins.";
const NOT_APPROVED_DIAL: &str = "You're not approved to dial — use `/register`.";
/// `/hangup` is the same tier as `/dial` but not the same verb: telling
/// someone who is trying to STOP a link that they may not dial reads as a
/// non sequitur at the moment it matters most.
const NOT_APPROVED_LINK: &str = "You're not approved to control the link — use `/register`.";
const NOT_APPROVED_MIC: &str = "You're not approved — use `/register`.";
/// Rule 4's M17 refusal for a caller with no registered callsign. An M17
/// CONN carries a callsign and the reflector shows it, so a dial without one
/// has nothing honest to present. The node never invents one.
const NEED_CALLSIGN: &str = "M17 needs your callsign — run `/register callsign:<yours>` first.";
const DSTAR_NOT_WIRED: &str = "D-Star isn't wired into this node's conference yet.";

/// `/register` refuses a callsign change while the caller's own M17 leg is
/// up or dialling. The reflector already holds the old callsign from the
/// CONN, so changing the record under the leg would make every later line
/// ("as W1AW") describe an identity the reflector never saw.
const HANG_UP_M17_FIRST: &str = "Hang up your M17 link first.";
const BRIDGE_NOT_JOINED: &str = "The bridge isn't in the voice channel, so nobody would hear it.";
const NOTHING_LINKED: &str = "Nothing is linked.";
const JOIN_VOICE_FIRST: &str = "Join the voice channel first.";
const ALREADY_HAVE_MIC: &str = "You already have the mic.";
const NOT_THE_HOLDER: &str = "You don't have the mic.";
const NOBODY_HAS_MIC: &str = "Nobody has the mic.";
const MUTE_PERMISSION: &str = "I can't mute here — I'm missing the Mute Members permission.";

/// What the channel is told when a rejoin has fixed the audio feed.
///
/// Said only to a channel that was told it stopped — see the
/// `AudioRecovered` arm. The pair is what makes the outage message usable
/// more than once in a node's lifetime.
const AUDIO_BACK: &str = "The audio feed into this channel is back.";

/// What the channel is told when the audio feed into it dies.
///
/// Fixed words, and no reason text: songbird's reason names codecs and
/// sometimes a host error, which is operator information rather than
/// channel information — it goes to [`AdminView::last_error`] and `/status`
/// instead. Posted once per process, like [`MUTE_PERMISSION`], because the
/// cause does not change by being repeated and because one dead track
/// fires two events.
const AUDIO_STOPPED: &str =
    "The audio feed into this channel stopped — the node's operator needs to rejoin the bridge.";

/// Appended to the caller's reply when the action worked but could not be
/// written down. The action still stands for this process; the operator is
/// told plainly that a restart loses it rather than discovering it later.
const WRITE_FAILED: &str = " (couldn't write the state file — this won't survive a restart)";

/// How the admin page signs its actions in the channel, where a Discord
/// user would be mentioned. The page is the control port, which has no
/// snowflake to mention.
const ADMIN_ACTOR: &str = "the node admin";

/// The `by:` field recorded against a decision the admin page made.
const ADMIN_BY: &str = "admin";

/// Seconds after a dial with no answer before the commander gives up on it.
const NO_ANSWER_SECS: u64 = 30;

/// Repeater-style hard time-out timer, in seconds. 0 turns it off.
const DEFAULT_MIC_TIMEOUT_SECS: u64 = 180;

/// Silence unkey window, in seconds. 0 turns it off.
const DEFAULT_MIC_SILENCE_SECS: u64 = 5;

fn mention(id: u64) -> String {
    format!("<@{id}>")
}

/// Whichever of the two write-failure suffixes is non-empty, so a step that
/// makes two saves reports the failure once rather than twice.
fn either(a: &'static str, b: &'static str) -> &'static str {
    if a.is_empty() { b } else { a }
}

fn ago(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else {
        format!("{}m ago", secs / 60)
    }
}

// The announced lines live in free functions rather than inline `format!`s
// because the admin page produces the same lines with "the node admin" where
// a mention goes: one definition, two actors, no chance of the page's audit
// trail drifting out of step with Discord's.

fn approved_line(target: u64, actor: &str) -> String {
    format!("<@{target}> was approved by {actor}.")
}

fn revoked_line(target: u64, actor: &str, had_mic: bool) -> String {
    if had_mic {
        format!("<@{target}> was revoked by {actor} and no longer has the mic.")
    } else {
        format!("<@{target}> was revoked by {actor}.")
    }
}

fn banned_line(target: u64, actor: &str, had_mic: bool) -> String {
    if had_mic {
        format!("<@{target}> was banned by {actor} and no longer has the mic.")
    } else {
        format!("<@{target}> was banned by {actor}.")
    }
}

fn unbanned_line(target: u64, actor: &str) -> String {
    format!("<@{target}> was unbanned by {actor}.")
}

fn admin_added_line(target: u64, actor: &str) -> String {
    format!("<@{target}> is now an admin, added by {actor}.")
}

fn admin_removed_line(target: u64, actor: &str, had_mic: bool) -> String {
    if had_mic {
        format!("<@{target}> is no longer an admin, removed by {actor}, and no longer has the mic.")
    } else {
        format!("<@{target}> is no longer an admin, removed by {actor}.")
    }
}

fn took_mic_line(holder: u64, actor: &str) -> String {
    format!("{actor} took the mic from <@{holder}>")
}

fn switch_line(on: bool, actor: &str) -> String {
    let state = if on { "on" } else { "off" };
    format!("Discord commands were turned {state} by {actor}.")
}

/// `labels` is a [`link_labels`] phrase, which names the network itself.
fn hungup_line(labels: &str, actor: &str) -> String {
    format!("{actor} hung up {labels}.")
}

/// `label` is one [`LinkRef`] rendered by `Display`, e.g. `AllStar 44444`.
fn busy_line(label: &str, by: &str) -> String {
    format!("Already listening to {label} (dialled by {by}). `/hangup` first.")
}

/// The busy refusal for a link the commander did not place: someone dialled
/// it from the node's own control port, so there is no Discord name to give.
/// `/hangup` still works on it — hanging up is the safe direction.
fn busy_line_from_node(label: &str) -> String {
    format!("Already listening to {label} (dialled from the node). `/hangup` first.")
}

fn mic_taken_line(caller: u64, previous: Option<u64>) -> String {
    match previous {
        Some(p) => format!("<@{caller}> has the mic (was <@{p}>)"),
        None => format!("<@{caller}> has the mic"),
    }
}

/// Which commands the master switch stops.
///
/// Everything else keeps working while the switch is off, and that is not an
/// oversight: `/status` has to be able to say *why* nothing works, the admin
/// commands have to be able to turn it back on, and `/hangup` plus
/// `/mic state:off` are the two safe directions — a switch that trapped a
/// live link or a live mic would be worse than no switch.
fn master_switch_blocks(kind: &BotCommandKind) -> bool {
    matches!(
        kind,
        BotCommandKind::Request { .. } | BotCommandKind::Dial(_) | BotCommandKind::Mic { on: true }
    )
}

/// Who holds the mic, and everything the two timers need to know about
/// them.
///
/// `keyed_at` and `last_voice_unix` are deliberately separate values.
/// `last_voice_unix` moves whenever the holder goes quiet; `keyed_at` never
/// moves at all. That is the fail-safe: the talk/silence events come from
/// the reverse-engineered half of Discord voice and can stop arriving
/// without notice, so nothing derived from them may be allowed to push the
/// hard time-out timer back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MicHolder {
    user: u64,
    /// When they took the mic. Never updated.
    keyed_at: u64,
    /// When they were last known to be sending voice packets — set to
    /// `keyed_at` on key-up, so keying and saying nothing still unkeys.
    last_voice_unix: u64,
    /// Whether packets are arriving from them right now.
    talking: bool,
}

/// The rule engine. One per configured Discord channel.
#[derive(Debug)]
pub struct Commander {
    approvals: Approvals,
    approvers: Vec<u64>,
    channel: u64,
    /// Who is in the bridged voice channel, and their last-seen server-mute
    /// flag. In memory only: Discord tells us the whole truth on READY.
    present: BTreeMap<u64, (String, bool)>,
    /// The one open mic. In memory only — a restart means nobody holds it
    /// and the READY sweep mutes everyone.
    ///
    /// **Only [`Commander::set_holder`] writes this field.** Every path that
    /// can change who holds the mic goes through it, which is what
    /// guarantees the PTT panel is redrawn exactly when the holder changes
    /// and never otherwise.
    mic: Option<MicHolder>,
    link: Option<LinkInfo>,
    /// Ids already told once, this process, that they are muted here.
    noticed: BTreeSet<u64>,
    mute_failure_posted: bool,
    /// Whether the channel has been told the audio feed died AND not yet
    /// been told it came back. Once per OUTAGE, never per event — see the
    /// `AudioFailed` arm — and it is what licenses the recovery line: a
    /// channel that was never warned is not told the feed is back.
    audio_failure_posted: bool,
    /// Why the audio feed into Discord stopped, if it is stopped now.
    ///
    /// **Kept apart from `last_error` because only this one is ever
    /// cleared.** A rejoin fixes the audio track and nothing else, so
    /// wiping the shared field would take a mute Discord refused or a state
    /// file that could not be written down with it — faults that are still
    /// true and that nothing else will report again.
    ///
    /// Precedence, applied in [`Self::view`]: **while this is set it wins**.
    /// It means the channel is hearing nothing at all, which outranks any
    /// bookkeeping failure; and it is the one error with an end, so the
    /// other is not lost, only covered until the audio is back.
    audio_error: Option<String>,
    last_error: Option<String>,
    mic_timeout_secs: u64,
    mic_silence_secs: u64,
    /// Injectable so the 30-second no-answer rule is testable without
    /// sleeping. Production is [`wall_clock`].
    now: fn() -> u64,
}

/// Unix seconds, or 0 if the system clock is before the epoch. A clock that
/// absurd makes "how long has this link been up" meaningless, not the node
/// unusable, so it is not worth an error path.
fn wall_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Commander {
    /// A commander over `approvals`, with `approvers` from `node.toml` and
    /// `channel` the bridged voice channel's id.
    #[must_use]
    pub fn new(approvals: Approvals, approvers: Vec<u64>, channel: u64) -> Self {
        let mut me = Self {
            approvals,
            approvers,
            channel,
            present: BTreeMap::new(),
            mic: None,
            link: None,
            noticed: BTreeSet::new(),
            mute_failure_posted: false,
            audio_failure_posted: false,
            audio_error: None,
            last_error: None,
            mic_timeout_secs: DEFAULT_MIC_TIMEOUT_SECS,
            mic_silence_secs: DEFAULT_MIC_SILENCE_SECS,
            now: wall_clock,
        };
        me.normalise_hand_edits();
        me
    }

    /// Reconcile a state file with the tier table it is supposed to encode,
    /// once, at start-up.
    ///
    /// **No code path in this crate can produce either shape.**
    /// [`Approvals::ban`] strips approved, pending and admin in the same
    /// call, and a ban on an approver is refused before it is recorded. But
    /// the file is documented as plain JSON with nothing secret in it, so it
    /// will be hand-edited, and both hand edits fail in the dangerous
    /// direction:
    ///
    /// * an approver under `banned` — `is_allowed` is false, so `/dial` is
    ///   refused to the one person `node.toml` says may always have it,
    ///   while every approver power still works. `node.toml` is the
    ///   operator's own file and wins: the ban is dropped.
    /// * an id in both `admins` (or `approved`) and `banned` — the ban is
    ///   meant to be unconditional and instead leaves a banned user running
    ///   admin commands. The ban wins: the tiers are dropped.
    ///
    /// Each removal is logged with the id, because silently rewriting
    /// somebody's state file is worse than the inconsistency. The cleaned
    /// state is saved only when something actually changed — each mutator
    /// skips the write for a no-op — and a save that fails is treated the
    /// way every other site treats one: recorded in `last_error`, carry on.
    fn normalise_hand_edits(&mut self) {
        // Approvers first: dropping their ban may be what makes the second
        // pass leave their admin entry alone.
        let banned_approvers: Vec<u64> = self
            .approvers
            .iter()
            .copied()
            .filter(|id| self.approvals.is_banned(*id))
            .collect();
        for id in banned_approvers {
            eprintln!(
                "astar-discord: state file: {id} is an approver in node.toml and was also in \
                 `banned`; dropping the ban — an approver cannot be banned"
            );
            let r = self.approvals.unban(id);
            let _ = self.noting_write(r);
        }

        // Then the reverse: whatever is still banned holds no tier.
        let banned_ids: Vec<u64> = self
            .approvals
            .state()
            .banned
            .iter()
            // A `user_id` this crate did not write may be anything at all.
            // One that is not a snowflake matches no live user, so there is
            // nothing to reconcile it against and it is left alone.
            .filter_map(|e| e.user_id.parse::<u64>().ok())
            .collect();
        for id in banned_ids {
            if self.approvals.is_admin(id) {
                eprintln!(
                    "astar-discord: state file: {id} is in both `admins` and `banned`; \
                     dropping the admin entry — a ban is unconditional"
                );
                let r = self.approvals.remove_admin(id);
                let _ = self.noting_write(r);
            }
            if self.approvals.is_approved(id) {
                eprintln!(
                    "astar-discord: state file: {id} is in both `approved` and `banned`; \
                     dropping the approval — a ban is unconditional"
                );
                let r = self.approvals.revoke(id);
                let _ = self.noting_write(r);
            }
        }
    }

    /// The gateway this commander was talking to has been replaced by a new
    /// one. Forget the audio fault that belonged to the old one, and
    /// nothing else.
    ///
    /// **Why the fault cannot clear itself.** `GatewayEvent::AudioRecovered`
    /// is emitted by a join that clears a failure recorded on the SAME
    /// gateway's `AudioHealth`. A gateway whose runtime thread died is not
    /// rejoined, it is replaced: the new one's `TrackSupervisor` starts with
    /// fresh health and has nothing to clear, so it emits no recovery and
    /// never will. Without this, `audio_error` and `audio_failure_posted`
    /// stick for the life of the process — `/status` reports a dead feed on
    /// a bridge that is working, and the next real outage is never announced
    /// because the "said once" flag is still set.
    ///
    /// Approvals, admins, bans, the mic and the link are deliberately
    /// untouched: none of them belongs to a gateway. Nothing is announced
    /// either — a replacement is an operational event, and the channel is
    /// told about the join that follows it.
    pub fn gateway_replaced(&mut self) {
        self.audio_error = None;
        self.audio_failure_posted = false;
    }

    /// The hard time-out timer, in seconds. 0 turns it off.
    pub fn set_mic_timeout(&mut self, secs: u64) {
        self.mic_timeout_secs = secs;
    }

    /// The silence unkey window, in seconds. 0 turns it off.
    pub fn set_mic_silence(&mut self, secs: u64) {
        self.mic_silence_secs = secs;
    }

    /// Replaces the clock. Exists for tests: the no-answer rule and every
    /// `since_unix` read through this, so a test can move time without
    /// sleeping.
    #[must_use]
    pub fn with_clock(mut self, now: fn() -> u64) -> Self {
        self.now = now;
        self
    }

    // -- tiers ------------------------------------------------------------

    fn is_approver(&self, id: u64) -> bool {
        self.approvers.contains(&id)
    }

    /// Approver or persisted admin. An approver is always an admin; that
    /// containment is the whole point of the tier table.
    fn is_admin(&self, id: u64) -> bool {
        self.is_approver(id) || self.approvals.is_admin(id)
    }

    /// The tier `/dial`, `/hangup` and `/mic` need. An approver can never be
    /// banned (the ban is refused before it is recorded), so an approver is
    /// always allowed.
    fn is_allowed(&self, id: u64) -> bool {
        (self.is_admin(id) || self.approvals.is_approved(id)) && !self.approvals.is_banned(id)
    }

    // -- small shared steps ----------------------------------------------

    /// Records a failed state-file write and returns the suffix the caller's
    /// reply gains. The action itself has already happened and stays
    /// happened — [`Approvals`] applies in memory first — so this is the one
    /// place that turns "could not persist" into words, and every mutator
    /// goes through it.
    fn noting_write<T>(&mut self, r: Result<T, ApprovalsError>) -> &'static str {
        match r {
            Ok(_) => "",
            Err(e) => {
                self.last_error = Some(e.to_string());
                WRITE_FAILED
            }
        }
    }

    fn entry(&self, u: &UserRef, by: String) -> UserEntry {
        UserEntry {
            user_id: u.id.to_string(),
            name: u.name.clone(),
            by,
            at_unix: (self.now)(),
        }
    }

    fn entry_for_id(&self, id: u64, by: String) -> UserEntry {
        UserEntry {
            user_id: id.to_string(),
            name: self.display_name(id),
            by,
            at_unix: (self.now)(),
        }
    }

    /// The name to show for an id: whoever is in the channel now, else the
    /// name recorded when they were approved, banned, queued or made an
    /// admin, else the bare snowflake. The page never shows a raw id if
    /// astar has ever heard this person's name.
    fn display_name(&self, id: u64) -> String {
        if let Some((name, _)) = self.present.get(&id) {
            return name.clone();
        }
        let key = id.to_string();
        let state = self.approvals.state();
        for list in [
            &state.approved,
            &state.admins,
            &state.pending,
            &state.banned,
        ] {
            if let Some(e) = list.iter().find(|e| e.user_id == key) {
                return e.name.clone();
            }
        }
        key
    }

    /// Who holds the mic, if anyone.
    fn holder(&self) -> Option<u64> {
        self.mic.map(|m| m.user)
    }

    /// The one writer of `self.mic`.
    ///
    /// Redraws the PTT panel if and only if the holder actually changed, and
    /// restarts both timers for a new holder. Funnelling every path — take,
    /// release, handover, `/unmic`, revoke, ban, the admin page, the master
    /// switch, leaving the channel, a moderator's mute, `Ready`, and both
    /// timers — through one method is what makes "the panel is redrawn on
    /// every holder change and never otherwise" a property of the code
    /// rather than of thirteen call sites remembering.
    fn set_holder(&mut self, new: Option<u64>, gw: &mut dyn GatewayOps) {
        if self.holder() == new {
            return;
        }
        let now = (self.now)();
        self.mic = new.map(|user| MicHolder {
            user,
            keyed_at: now,
            last_voice_unix: now,
            talking: false,
        });
        let label = new.map(|user| self.display_name(user));
        gw.panel(label.as_deref());
    }

    /// Issues a mute and keeps our own picture of the channel in step with
    /// it.
    ///
    /// Discord will echo this back as a voice event in a moment, but
    /// [`AdminView`] is read on demand and must not show a stale flag for the
    /// person who just took the mic. Updating optimistically here costs
    /// nothing: when the echo arrives it agrees with what we already recorded
    /// and produces no ops (rules 11 and 12).
    fn set_mute(&mut self, gw: &mut dyn GatewayOps, id: u64, mute: bool) {
        gw.set_mute(id, mute);
        if let Some((_, server_muted)) = self.present.get_mut(&id) {
            *server_muted = mute;
        }
    }

    /// Mutes whoever holds the mic and clears the holder. Returns who it
    /// was (if anyone) and the write-failure suffix.
    fn take_mic_from_holder(&mut self, gw: &mut dyn GatewayOps) -> (Option<u64>, &'static str) {
        let Some(holder) = self.holder() else {
            return (None, "");
        };
        self.set_holder(None, gw);
        self.set_mute(gw, holder, true);
        let r = self.approvals.set_muted_by_us(holder, true);
        (Some(holder), self.noting_write(r))
    }

    /// What `/hangup` should act on: the roster if the node has one,
    /// otherwise the link we dialled but have not yet seen appear. For M17
    /// that is a leg still waiting for its ACKN.
    fn hangup_targets(&self, node: &dyn NodeOps) -> Vec<LinkRef> {
        let roster = node.links();
        if roster.is_empty() {
            self.link.iter().map(LinkInfo::link_ref).collect()
        } else {
            roster
        }
    }

    /// Rule 4's shared gate once a target is valid. The bridge must be in
    /// the channel, and nothing may be linked, by us or by the node, on any
    /// network. One link at a time is what keeps the channel's audio
    /// attributable.
    fn dial_is_clear(&self, it: u64, gw: &mut dyn GatewayOps, node: &dyn NodeOps) -> bool {
        if !node.bridge_joined() {
            gw.reply(it, BRIDGE_NOT_JOINED);
            return false;
        }
        if let Some(l) = &self.link {
            gw.reply(it, &busy_line(&l.link_ref().to_string(), &l.by));
            return false;
        }
        if let Some(existing) = node.links().first() {
            gw.reply(it, &busy_line_from_node(&existing.to_string()));
            return false;
        }
        true
    }

    /// Hangs up the M17 leg `target` dialled, if the commander holds one,
    /// and returns the label to announce.
    ///
    /// Revoke and ban call this because an M17 reflector sees the dialler's
    /// own callsign on the CONN. Once a person is revoked or banned, the
    /// node must stop presenting their identity. Three kinds of link stay
    /// up: an `AllStar` monitor, which carries no identity of theirs; a leg
    /// someone else dialled; and one with no Discord dialler (`by_id` is
    /// `None`).
    ///
    /// A refused hang-up keeps `link`, because the leg really is still up.
    /// The refusal lands in `last_error` only. The revoke or ban has already
    /// taken effect, and that is what the channel is told.
    fn hang_up_m17_leg_of(&mut self, target: u64, node: &mut dyn NodeOps) -> Option<String> {
        let link_ref = self
            .link
            .as_ref()
            .filter(|l| l.network == Network::M17 && l.by_id == Some(target))?
            .link_ref();
        let label = link_ref.to_string();
        match node.hangup(&link_ref) {
            Ok(()) => {
                self.link = None;
                Some(label)
            }
            Err(e) => {
                self.last_error = Some(format!("couldn't hang up {label}: {e}"));
                None
            }
        }
    }

    fn status_line(&self, node: &dyn NodeOps) -> String {
        let mut line = match &self.link {
            Some(l) => {
                let since = ago((self.now)().saturating_sub(l.since_unix));
                let link = l.link_ref();
                // " as AJ7HR" only where a callsign was presented, which is
                // M17. An AllStar monitor presents none, so its line is
                // unchanged.
                let as_call = l
                    .callsign
                    .as_ref()
                    .map_or_else(String::new, |c| format!(" as {c}"));
                if l.up {
                    let mut s =
                        format!("Linked to {link}{as_call} — dialled by {}, {since}.", l.by);
                    if l.network == Network::M17
                        && let Some((call, secs)) = node.last_heard(&link)
                    {
                        let _ = write!(s, " Last heard {call} {}.", ago(secs));
                    }
                    s
                } else {
                    format!("Dialling {link}{as_call} — asked by {}, {since}.", l.by)
                }
            }
            None => "Not linked.".to_string(),
        };
        match self.holder() {
            Some(m) => {
                let who = self.display_name(m);
                let _ = write!(line, " {who} has the mic.");
            }
            None => line.push_str(" Nobody has the mic."),
        }
        if !self.approvals.state().enabled {
            line.push_str(" Discord commands are off.");
        }
        line
    }

    // -- the entry points -------------------------------------------------

    /// Handle one gateway event.
    pub fn handle(&mut self, ev: GatewayEvent, gw: &mut dyn GatewayOps, node: &mut dyn NodeOps) {
        match ev {
            GatewayEvent::Ready { voice } => {
                // A reconnect means every voice event since the disconnect
                // was missed, so the in-memory picture is rebuilt from
                // Discord's rather than patched. The mic goes with it:
                // nobody holds it across a reconnect, and the sweep below
                // re-mutes the channel.
                //
                // **`Ready` always draws the panel exactly once**, and that
                // is the one place the idle panel can come from. On a fresh
                // start nobody holds the mic, so `set_holder(None)` is a
                // no-op and redraws nothing — which left the PTT button
                // message never posted at all until somebody happened to key
                // and unkey the mic by slash command. `Ready` is the one
                // event guaranteed to arrive, so the draw is owed here. It
                // is conditional rather than unconditional because
                // `set_holder` has already drawn it when there WAS a holder,
                // and two edits for one event is a wasted REST call the
                // channel sees as a flicker.
                let had_holder = self.holder().is_some();
                self.set_holder(None, gw);
                if !had_holder {
                    gw.panel(None);
                }
                self.present.clear();
                for v in &voice {
                    self.voice(v, gw);
                }
            }
            GatewayEvent::Command(mut cmd) => {
                // A Discord message button delivers one click and cannot
                // report press-and-hold, so the PTT button is a toggle. It
                // is resolved here, before any rule sees it, so the button
                // and `/mic` are the same command with the same refusals,
                // the same announcements and the same tests.
                if matches!(cmd.kind, BotCommandKind::MicToggle) {
                    cmd.kind = BotCommandKind::Mic {
                        on: self.holder() != Some(cmd.user.id),
                    };
                }
                self.command(cmd, gw, node);
            }
            GatewayEvent::Voice(v) => self.voice(&v, gw),
            GatewayEvent::Talking { user, talking } => {
                let now = (self.now)();
                if let Some(m) = self.mic.as_mut().filter(|m| m.user == user) {
                    m.talking = talking;
                    // Only the falling edge moves the silence clock. While
                    // packets are arriving there is no silence to measure,
                    // and `keyed_at` is untouched either way.
                    if !talking {
                        m.last_voice_unix = now;
                    }
                }
            }
            GatewayEvent::AudioFailed { reason } => {
                // The newest reason every time: the operator looking at
                // `/status` wants what happened last, not what happened
                // first. Into `audio_error`, not `last_error`, because this
                // is the one error a later event can END.
                self.audio_error = Some(format!("the audio feed into Discord stopped: {reason}"));
                // Announced once per process, like the mute failure above
                // and for a stronger version of the same reason: one dead
                // track fires both `TrackEvent::Error` and
                // `TrackEvent::End`, a rejoin that fails can fire more,
                // and none of it changes what the channel needs to know.
                if !self.audio_failure_posted {
                    self.audio_failure_posted = true;
                    gw.announce(AUDIO_STOPPED);
                }
            }
            GatewayEvent::AudioRecovered => {
                // Emitted only by a join that cleared a recorded failure,
                // so arriving at all means the audio really was broken and
                // really is fixed.
                self.audio_error = None;
                // Only a channel that was told it stopped is told it is
                // back — and clearing the flag is what lets the NEXT outage
                // be announced. Without that pair, the first outage in a
                // node's life was the only one anybody heard about.
                if self.audio_failure_posted {
                    self.audio_failure_posted = false;
                    gw.announce(AUDIO_BACK);
                }
            }
            GatewayEvent::MuteFailed { user, reason } => {
                self.last_error = Some(format!("couldn't mute {user}: {reason}"));
                // Posted once per process, not once per join: the cause is
                // a missing permission, which does not change between
                // joins, and a busy channel would otherwise repeat this
                // line for every arrival.
                if !self.mute_failure_posted {
                    self.mute_failure_posted = true;
                    gw.announce(MUTE_PERMISSION);
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)] // flat command match — splitting hurts readability
    fn command(&mut self, cmd: BotCommand, gw: &mut dyn GatewayOps, node: &mut dyn NodeOps) {
        let BotCommand {
            user: caller,
            kind,
            interaction: it,
        } = cmd;

        if !self.approvals.state().enabled && master_switch_blocks(&kind) {
            gw.reply(it, BOT_OFF);
            return;
        }

        match kind {
            // Rule 2.
            BotCommandKind::Request { callsign } => {
                if self.approvals.is_banned(caller.id) {
                    gw.reply(it, REQUEST_BANNED);
                    return;
                }
                let callsign = match validate_callsign(&callsign) {
                    Ok(c) => c,
                    Err(e) => {
                        gw.reply(it, &e);
                        return;
                    }
                };
                // One callsign, one member. A reflector knows the dialler only
                // by the callsign on the CONN, so recording someone else's
                // would let this caller present their identity. Checked after
                // normalising, so " aj7hr " is the same claim as "AJ7HR".
                if self
                    .approvals
                    .callsign_holder(&callsign)
                    .is_some_and(|holder| holder != caller.id)
                {
                    gw.reply(
                        it,
                        &format!(
                            "{callsign} is already registered to another member — ask an \
                             approver."
                        ),
                    );
                    return;
                }
                let old = self.approvals.callsign(caller.id).map(str::to_owned);
                let changed = old.as_deref() != Some(callsign.as_str());
                if changed
                    && self
                        .link
                        .as_ref()
                        .is_some_and(|l| l.network == Network::M17 && l.by_id == Some(caller.id))
                {
                    gw.reply(it, HANG_UP_M17_FIRST);
                    return;
                }
                // The callsign is recorded for anyone not banned and not
                // refused above, whatever list they are on: it is how an
                // already-approved user — or
                // an approver, who is on no list — gets one, and how a typo
                // is fixed. Only the request half depends on the tier.
                let r = self.approvals.set_callsign(caller.id, callsign.clone());
                let callsign_suffix = self.noting_write(r);
                let recorded = format!("Callsign {callsign} recorded.");
                if self.is_allowed(caller.id) {
                    gw.reply(
                        it,
                        &format!("{recorded} {ALREADY_APPROVED}{callsign_suffix}"),
                    );
                    // A change of identity is public, because the dial announce
                    // ("as AJ7HR") is the channel's record of who the node
                    // presented. A first registration or a repeat is not a
                    // change. A pending member's change is not announced
                    // either: an approver reads the new callsign on the page
                    // before approving.
                    if let Some(old) = old.as_deref().filter(|_| changed) {
                        let id = caller.id;
                        gw.announce(&format!(
                            "<@{id}> changed their callsign from {old} to {callsign}."
                        ));
                    }
                } else if self.approvals.is_pending(caller.id) {
                    // Deliberately no second announce: a user who runs
                    // `/register` twice must not ping the channel twice.
                    gw.reply(
                        it,
                        &format!("{recorded} {ALREADY_PENDING}{callsign_suffix}"),
                    );
                } else {
                    let entry = self.entry(&caller, String::new());
                    let r = self.approvals.add_pending(entry);
                    let suffix = either(callsign_suffix, self.noting_write(r));
                    gw.reply(it, &format!("Request sent as {callsign}.{suffix}"));
                    let id = caller.id;
                    gw.announce(&format!(
                        "<@{id}> ({callsign}) is asking for dial access — an approver can \
                         `/approve` them."
                    ));
                }
            }

            // Rule 3 (approve half).
            BotCommandKind::Approve { target } => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                if self.approvals.is_banned(target.id) {
                    let name = &target.name;
                    gw.reply(
                        it,
                        &format!(
                            "{name} is banned on this node; unban them from the node's admin \
                             page first."
                        ),
                    );
                    return;
                }
                let entry = self.entry(&target, caller.id.to_string());
                let r = self.approvals.approve(entry);
                let suffix = self.noting_write(r);
                let name = &target.name;
                // No unmute: approval is permission to take the mic, not
                // the mic. Taking it stays a deliberate act.
                gw.reply(it, &format!("Approved {name}.{suffix}"));
                gw.announce(&approved_line(target.id, &mention(caller.id)));
            }

            // Rule 3 (revoke half).
            BotCommandKind::Revoke { target } => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                if self.is_approver(target.id) {
                    gw.reply(it, APPROVER_NOT_REVOCABLE);
                    return;
                }
                if self.approvals.is_admin(target.id) && !self.is_approver(caller.id) {
                    gw.reply(it, ADMIN_PROTECTED);
                    return;
                }
                let r = self.approvals.revoke(target.id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_approved = !matches!(&r, Ok(false));
                let suffix = self.noting_write(r);
                let name = &target.name;
                if !was_approved {
                    gw.reply(it, &format!("{name} isn't approved."));
                    return;
                }
                let had_mic = self.holder() == Some(target.id);
                let mic_suffix = if had_mic {
                    self.take_mic_from_holder(gw).1
                } else {
                    ""
                };
                let suffix = either(suffix, mic_suffix);
                let hung_up = self.hang_up_m17_leg_of(target.id, node);
                let actor = mention(caller.id);
                gw.reply(it, &format!("Revoked {name}.{suffix}"));
                gw.announce(&revoked_line(target.id, &actor, had_mic));
                if let Some(label) = hung_up {
                    gw.announce(&hungup_line(&label, &actor));
                }
            }

            // Rule 4.
            BotCommandKind::Dial(target) => {
                if !self.is_allowed(caller.id) {
                    gw.reply(it, NOT_APPROVED_DIAL);
                    return;
                }
                match target {
                    DialTarget::Allstar { node: number } => {
                        if let Err(e) = validate_allstar_node(&number) {
                            gw.reply(it, &e);
                            return;
                        }
                        if !self.dial_is_clear(it, gw, node) {
                            return;
                        }
                        // A dial the engine refused leaves `link` at `None`:
                        // the commander must not believe in a link that was
                        // never made.
                        if let Err(e) = node.dial_allstar_monitor(&number) {
                            gw.reply(it, &format!("Couldn't dial: {e}"));
                            return;
                        }
                        self.link = Some(LinkInfo {
                            network: Network::Allstar,
                            target: number.clone(),
                            by: caller.name.clone(),
                            by_id: Some(caller.id),
                            callsign: None,
                            since_unix: (self.now)(),
                            up: false,
                        });
                        gw.reply(it, &format!("Dialling AllStar {number} (receive-only)…"));
                        let id = caller.id;
                        gw.announce(&format!("<@{id}> → AllStar {number} (receive-only)"));
                    }
                    DialTarget::M17 { reflector, module } => {
                        let Some((reflector, module)) =
                            validated_reflector(&reflector, module, it, gw)
                        else {
                            return;
                        };
                        // After the shape check, so a typo is named as a
                        // typo, and before the bridge check, because the
                        // callsign is the thing this caller can fix.
                        let Some(callsign) = self.approvals.callsign(caller.id).map(str::to_string)
                        else {
                            gw.reply(it, NEED_CALLSIGN);
                            return;
                        };
                        if !self.dial_is_clear(it, gw, node) {
                            return;
                        }
                        let warning = match node.dial_m17_monitor(&reflector, module, &callsign) {
                            Ok(w) => w,
                            Err(e) => {
                                gw.reply(it, &format!("Couldn't dial: {e}"));
                                return;
                            }
                        };
                        let target = LinkRef::m17(&reflector, module).target;
                        gw.reply(
                            it,
                            &format!(
                                "Dialling M17 {target} as {callsign} (receive-only)…\n{warning}"
                            ),
                        );
                        let id = caller.id;
                        // The callsign goes into the public line on purpose:
                        // it is the identity the reflector now shows, and
                        // this line is the only record of who presented it.
                        // The warning goes in too: everyone in the channel
                        // should know the node now relays this reflector.
                        gw.announce(&format!(
                            "<@{id}> → M17 {target} (receive-only, as {callsign})\n{warning}"
                        ));
                        self.link = Some(LinkInfo {
                            network: Network::M17,
                            target,
                            by: caller.name.clone(),
                            by_id: Some(caller.id),
                            callsign: Some(callsign),
                            since_unix: (self.now)(),
                            up: false,
                        });
                    }
                    DialTarget::Dstar { reflector, module } => {
                        // Validated first, so a typo is reported as a typo
                        // rather than hidden behind "not wired yet".
                        if validated_reflector(&reflector, module, it, gw).is_some() {
                            gw.reply(it, DSTAR_NOT_WIRED);
                        }
                    }
                }
            }

            // Rule 5.
            BotCommandKind::Hangup => {
                if !self.is_allowed(caller.id) {
                    gw.reply(it, NOT_APPROVED_LINK);
                    return;
                }
                let targets = self.hangup_targets(node);
                if targets.is_empty() {
                    gw.reply(it, NOTHING_LINKED);
                    return;
                }
                for t in &targets {
                    // A hangup that failed leaves the link up, so the
                    // commander must keep believing it is up: clearing
                    // `link` here would hide a live link from `/status` and
                    // from the busy check.
                    if let Err(e) = node.hangup(t) {
                        gw.reply(it, &format!("Couldn't hang up: {e}"));
                        return;
                    }
                }
                self.link = None;
                let labels = link_labels(&targets);
                gw.reply(it, &format!("Hung up {labels}."));
                gw.announce(&hungup_line(&labels, &mention(caller.id)));
            }

            // Rule 6.
            BotCommandKind::Status => {
                let line = self.status_line(node);
                gw.reply(it, &line);
            }

            // Rule 7.
            BotCommandKind::Mic { on: true } => {
                if !self.is_allowed(caller.id) {
                    gw.reply(it, NOT_APPROVED_MIC);
                    return;
                }
                if !self.present.contains_key(&caller.id) {
                    gw.reply(it, JOIN_VOICE_FIRST);
                    return;
                }
                if self.holder() == Some(caller.id) {
                    gw.reply(it, ALREADY_HAVE_MIC);
                    return;
                }
                let previous = self.holder();
                // ORDER IS LOAD-BEARING, twice over.
                //
                // The holder is set BEFORE any mute call because our own
                // unmute comes straight back as a voice event with
                // `server_muted == false`; if that event arrived while
                // `self.mic` still named the old holder, rule 9 would see an
                // unmuted non-holder and mute the caller again, one command
                // after unmuting them.
                //
                // The caller's unmute is issued BEFORE the previous holder's
                // mute so the channel never has two open mics, not even for
                // the width of one API call — it may briefly have none.
                self.set_holder(Some(caller.id), gw);
                self.set_mute(gw, caller.id, false);
                let r = self.approvals.set_muted_by_us(caller.id, false);
                let mut suffix = self.noting_write(r);
                if let Some(p) = previous {
                    self.set_mute(gw, p, true);
                    let r = self.approvals.set_muted_by_us(p, true);
                    let s = self.noting_write(r);
                    suffix = either(suffix, s);
                }
                gw.reply(it, &format!("You have the mic.{suffix}"));
                gw.announce(&mic_taken_line(caller.id, previous));
            }

            // Resolved to `Mic { .. }` in `handle` before it ever reaches
            // here; matching it again would be a second place for the
            // toggle's meaning to live.
            BotCommandKind::MicToggle => {}

            // Rule 8.
            BotCommandKind::Mic { on: false } => {
                if self.holder() != Some(caller.id) {
                    gw.reply(it, NOT_THE_HOLDER);
                    return;
                }
                let (_, suffix) = self.take_mic_from_holder(gw);
                gw.reply(it, &format!("Mic released.{suffix}"));
                let id = caller.id;
                gw.announce(&format!("<@{id}> released the mic"));
            }

            // Rule 8a (ban half).
            BotCommandKind::Ban { target } => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                if self.is_approver(target.id) {
                    gw.reply(it, APPROVER_NOT_BANNABLE);
                    return;
                }
                if self.approvals.is_admin(target.id) && !self.is_approver(caller.id) {
                    gw.reply(it, ADMIN_PROTECTED);
                    return;
                }
                let entry = self.entry(&target, caller.id.to_string());
                let r = self.approvals.ban(entry);
                let suffix = self.noting_write(r);
                let had_mic = self.holder() == Some(target.id);
                let mic_suffix = if had_mic {
                    self.take_mic_from_holder(gw).1
                } else {
                    ""
                };
                let suffix = either(suffix, mic_suffix);
                let hung_up = self.hang_up_m17_leg_of(target.id, node);
                let actor = mention(caller.id);
                let name = &target.name;
                gw.reply(it, &format!("Banned {name}.{suffix}"));
                gw.announce(&banned_line(target.id, &actor, had_mic));
                if let Some(label) = hung_up {
                    gw.announce(&hungup_line(&label, &actor));
                }
            }

            // Rule 8a (unban half).
            BotCommandKind::Unban { target } => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                if self.is_approver(target.id) {
                    gw.reply(it, APPROVER_NOT_BANNABLE);
                    return;
                }
                let r = self.approvals.unban(target.id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_banned = !matches!(&r, Ok(false));
                let suffix = self.noting_write(r);
                let name = &target.name;
                if !was_banned {
                    gw.reply(it, &format!("{name} isn't banned."));
                    return;
                }
                gw.reply(it, &format!("Unbanned {name}.{suffix}"));
                gw.announce(&unbanned_line(target.id, &mention(caller.id)));
            }

            // Rule 8b.
            BotCommandKind::Unmic => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                let Some(holder) = self.holder() else {
                    gw.reply(it, NOBODY_HAS_MIC);
                    return;
                };
                // An admin may always put their own mic down; the
                // admin-protection rule is about acting on *other* admins.
                if holder != caller.id && self.is_admin(holder) && !self.is_approver(caller.id) {
                    gw.reply(it, ADMIN_PROTECTED);
                    return;
                }
                let (_, suffix) = self.take_mic_from_holder(gw);
                let who = self.display_name(holder);
                gw.reply(it, &format!("Took the mic from {who}.{suffix}"));
                gw.announce(&took_mic_line(holder, &mention(caller.id)));
            }

            // Rule 8c.
            BotCommandKind::Bot { on } => {
                if !self.is_admin(caller.id) {
                    gw.reply(it, NOT_ADMIN);
                    return;
                }
                let r = self.approvals.set_enabled(on);
                let suffix = self.noting_write(r);
                let mic_suffix = if on {
                    ""
                } else {
                    // Turning the switch off closes the open mic: leaving
                    // someone unmuted while nobody can take the mic from
                    // them with `/mic` would outlast the switch.
                    self.take_mic_from_holder(gw).1
                };
                let suffix = either(suffix, mic_suffix);
                let state = if on { "on" } else { "off" };
                gw.reply(it, &format!("Discord commands are now {state}.{suffix}"));
                gw.announce(&switch_line(on, &mention(caller.id)));
            }

            // Rule 8d (add half).
            BotCommandKind::AdminAdd { target } => {
                if !self.is_approver(caller.id) {
                    gw.reply(it, NOT_APPROVER);
                    return;
                }
                if self.approvals.is_banned(target.id) {
                    let name = &target.name;
                    gw.reply(
                        it,
                        &format!(
                            "{name} is banned on this node; unban them from the node's admin \
                             page first."
                        ),
                    );
                    return;
                }
                let entry = self.entry(&target, caller.id.to_string());
                let r = self.approvals.add_admin(entry);
                let suffix = self.noting_write(r);
                let name = &target.name;
                gw.reply(it, &format!("{name} is now an admin.{suffix}"));
                gw.announce(&admin_added_line(target.id, &mention(caller.id)));
            }

            // Rule 8d (remove half).
            BotCommandKind::AdminRemove { target } => {
                if !self.is_approver(caller.id) {
                    gw.reply(it, NOT_APPROVER);
                    return;
                }
                if self.is_approver(target.id) {
                    gw.reply(it, APPROVER_NOT_REMOVABLE);
                    return;
                }
                let r = self.approvals.remove_admin(target.id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_admin = !matches!(&r, Ok(false));
                let suffix = self.noting_write(r);
                let name = &target.name;
                if !was_admin {
                    gw.reply(it, &format!("{name} isn't an admin."));
                    return;
                }
                // A demotion can leave the holder with no tier at all, and
                // an open mic is a tier — `/mic` is for approved users.
                // Checked AFTER the removal, so `is_allowed` reads the new
                // truth: an admin who is also separately approved keeps it,
                // an admin-only holder does not. Without this the mic stays
                // open until a timer fires, and with `mic_timeout_secs = 0`
                // and `mic_silence_secs = 0` — both documented — no timer
                // ever does.
                let had_mic = self.holder() == Some(target.id) && !self.is_allowed(target.id);
                let mic_suffix = if had_mic {
                    self.take_mic_from_holder(gw).1
                } else {
                    ""
                };
                let suffix = either(suffix, mic_suffix);
                gw.reply(it, &format!("{name} is no longer an admin.{suffix}"));
                gw.announce(&admin_removed_line(target.id, &mention(caller.id), had_mic));
            }
        }
    }

    /// Rules 9–15: one member's voice state.
    fn voice(&mut self, v: &VoiceEvent, gw: &mut dyn GatewayOps) {
        // Including this bridge itself: our own mute events come back
        // through the gateway, and a bot that reacted to them would fight
        // itself.
        if v.is_bot {
            return;
        }
        let id = v.user.id;

        if v.channel == Some(self.channel) {
            self.present
                .insert(id, (v.user.name.clone(), v.server_muted));

            if self.holder() == Some(id) {
                // Rule 11. An unmuted holder is the echo of our own unmute —
                // nothing to do. A *muted* holder means a moderator took the
                // mic away by hand; the node yields rather than fighting
                // them, and does not re-issue a mute it did not decide on.
                if v.server_muted {
                    self.set_holder(None, gw);
                }
                return;
            }

            // Rules 9, 10 and 12. An already-muted non-holder produces no
            // ops at all, which is both the moderator's-mute rule (never
            // recorded, so never lifted) and the no-self-loop rule: our own
            // mute arrives here as `server_muted: true` and matches nothing.
            if !v.server_muted {
                self.set_mute(gw, id, true);
                let r = self.approvals.set_muted_by_us(id, true);
                let _ = self.noting_write(r);
                // `insert` returns false if it was already there, which is
                // exactly "once per user per process".
                if self.noticed.insert(id) {
                    gw.announce(&format!(
                        "<@{id}> you're muted here — approved users can take the mic with \
                         `/mic`; otherwise `/register`."
                    ));
                }
            }
            return;
        }

        // Out of our channel, either way: they are not present and they do
        // not hold the mic.
        self.present.remove(&id);
        if self.holder() == Some(id) {
            self.set_holder(None, gw);
        }

        if v.channel.is_some() {
            // Rule 13: visible somewhere else, so the mute we placed can be
            // lifted — and must be, because a server mute is guild-wide.
            if self.approvals.muted_by_us(id) {
                let r = self.approvals.set_muted_by_us(id, false);
                let _ = self.noting_write(r);
                // Only if it is actually still on: if a moderator already
                // lifted it, issuing an unmute would be noise.
                if v.server_muted {
                    self.set_mute(gw, id, false);
                }
            }
        }
        // Rule 14: gone from voice entirely. Discord will not let a bot
        // change the voice state of someone not in voice, so the mark stays
        // recorded and is lifted the next time they appear somewhere.
    }

    /// Called every pump tick: runs the two mic timers, then reconciles
    /// `link` with `node.links()`.
    pub fn tick(&mut self, gw: &mut dyn GatewayOps, node: &dyn NodeOps) {
        self.tick_mic(gw);
        self.tick_link(gw, node);
    }

    /// The hard time-out timer and the silence unkey.
    ///
    /// A toggle can be forgotten, and on an armed bridge a forgotten toggle
    /// is a stuck transmitter — so the mic gets a repeater-style TOT. The
    /// silence unkey is the thing that makes a toggle *feel* like PTT, but
    /// it is a convenience layered on top and is never the only thing
    /// holding the door: it is checked second, it can only shorten how long
    /// the mic stays open, and it reads `last_voice_unix`, which the hard
    /// timer does not look at. A holder whose talk events stop arriving
    /// mid-transmission keeps `talking: true` forever and is still released
    /// on `keyed_at + mic_timeout_secs`.
    fn tick_mic(&mut self, gw: &mut dyn GatewayOps) {
        let Some(m) = self.mic else {
            return;
        };
        let now = (self.now)();
        // Checked first, and it wins a tie: when both are due the channel
        // should hear the rule that actually protects it.
        let timed_out =
            self.mic_timeout_secs != 0 && now.saturating_sub(m.keyed_at) >= self.mic_timeout_secs;
        let silent = !timed_out
            && self.mic_silence_secs != 0
            && !m.talking
            && now.saturating_sub(m.last_voice_unix) >= self.mic_silence_secs;
        if !timed_out && !silent {
            return;
        }
        // Same release as `/mic state:off`: cleared, muted, recorded as ours
        // to lift, and the panel redrawn.
        let (_, _suffix) = self.take_mic_from_holder(gw);
        let user = m.user;
        if timed_out {
            gw.announce(&format!("⏱ <@{user}> timed out"));
        } else {
            gw.announce(&format!("<@{user}> unkeyed (silence)"));
        }
    }

    /// Called every pump tick: reconciles `link` with `node.links()`.
    ///
    /// The dial is asynchronous — the engine answers "asked", not
    /// "connected" — so this is the only place that can tell the channel a
    /// link actually came up, went away, or never answered at all.
    fn tick_link(&mut self, gw: &mut dyn GatewayOps, node: &dyn NodeOps) {
        let Some(link) = self.link.clone() else {
            return;
        };
        let r = link.link_ref();
        let in_roster = node.links().contains(&r);

        if in_roster {
            if !link.up {
                if let Some(l) = self.link.as_mut() {
                    l.up = true;
                }
                gw.announce(&format!("Connected to {r}."));
            }
        } else if let Some(reason) = node.link_end_reason(&r) {
            // The node knows why its own leg ended (an M17 NACK, a DISC, or
            // keepalive silence). Say so now, with the reason, rather than a
            // bare "ended" or thirty seconds of "did not answer" for a
            // refusal that has already happened.
            self.link = None;
            gw.announce(&format!("The link to {r} has ended: {reason}."));
        } else if link.up {
            self.link = None;
            gw.announce(&format!("The link to {r} has ended."));
        } else if (self.now)().saturating_sub(link.since_unix) >= NO_ANSWER_SECS {
            self.link = None;
            gw.announce(&format!("{r} did not answer."));
        }
    }

    /// Best-effort: unmute every `muted_by_us` member still in the channel.
    ///
    /// The marks are deliberately **not** cleared. A server mute is applied
    /// by Discord, not by us, and this call cannot know whether the API
    /// request survived the shutdown. Leaving the record in place means the
    /// worst case is one redundant unmute the next time that person appears
    /// in a voice channel (rule 13) rather than someone left muted
    /// guild-wide with nothing remembering why.
    pub fn shutdown(&mut self, gw: &mut dyn GatewayOps) {
        for id in self.present.keys().copied().collect::<Vec<_>>() {
            if self.approvals.muted_by_us(id) {
                self.set_mute(gw, id, false);
            }
        }
    }

    /// The snapshot the admin page renders.
    #[must_use]
    pub fn view(&self) -> AdminView {
        let state = self.approvals.state();
        AdminView {
            enabled: state.enabled,
            approvers: self.approvers.iter().map(ToString::to_string).collect(),
            admins: state.admins.clone(),
            pending: state.pending.clone(),
            approved: state.approved.clone(),
            banned: state.banned.clone(),
            callsigns: state.callsigns.clone(),
            present: self
                .present
                .iter()
                .map(|(id, (name, muted))| PresentUser {
                    user_id: id.to_string(),
                    name: name.clone(),
                    server_muted: *muted,
                })
                .collect(),
            mic_holder: self.holder().map(|m| m.to_string()),
            mic_timeout_secs: self.mic_timeout_secs,
            mic_silence_secs: self.mic_silence_secs,
            talking: self.mic.is_some_and(|m| m.talking),
            link: self.link.clone(),
            // The audio fault wins while it stands; see `audio_error`.
            last_error: self.audio_error.clone().or_else(|| self.last_error.clone()),
        }
    }

    /// One action from the node's own admin page.
    ///
    /// The page is the control port, which is already the node's root of
    /// trust — it can key the transmitter. So it is not subject to the
    /// master switch and it *may* act on admins, which no admin inside
    /// Discord can. Approvers stay config-only even here.
    ///
    /// # Errors
    /// A human-readable sentence for the page to show when the action does
    /// not apply — the user is an approver, or is not in the state the
    /// action assumes. A state-file write that fails is **not** an error:
    /// the action took effect, and [`AdminView::last_error`] carries it.
    #[allow(clippy::too_many_lines)]
    // flat action match — splitting hurts readability
    // `a` is a plain tag from the HTTP layer and the brief pins this
    // signature; taking it by reference would buy nothing.
    #[allow(clippy::needless_pass_by_value)]
    pub fn admin(
        &mut self,
        a: AdminAction,
        gw: &mut dyn GatewayOps,
        node: &mut dyn NodeOps,
    ) -> Result<(), String> {
        match a {
            AdminAction::Approve(id) => {
                let name = self.display_name(id);
                if self.approvals.is_banned(id) {
                    return Err(format!("{name} is banned on this node; unban them first"));
                }
                let entry = self.entry_for_id(id, ADMIN_BY.to_string());
                let r = self.approvals.approve(entry);
                let _ = self.noting_write(r);
                gw.announce(&approved_line(id, ADMIN_ACTOR));
            }
            AdminAction::Revoke(id) => {
                let name = self.display_name(id);
                if self.is_approver(id) {
                    return Err(format!("{name} is an approver in node.toml"));
                }
                let r = self.approvals.revoke(id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_approved = !matches!(&r, Ok(false));
                let _ = self.noting_write(r);
                if !was_approved {
                    return Err(format!("{name} is not approved"));
                }
                let had_mic = self.holder() == Some(id);
                if had_mic {
                    self.take_mic_from_holder(gw);
                }
                let hung_up = self.hang_up_m17_leg_of(id, node);
                gw.announce(&revoked_line(id, ADMIN_ACTOR, had_mic));
                if let Some(label) = hung_up {
                    gw.announce(&hungup_line(&label, ADMIN_ACTOR));
                }
            }
            AdminAction::Ban(id) => {
                let name = self.display_name(id);
                if self.is_approver(id) {
                    return Err(format!("{name} is an approver in node.toml"));
                }
                let entry = self.entry_for_id(id, ADMIN_BY.to_string());
                let r = self.approvals.ban(entry);
                let _ = self.noting_write(r);
                let had_mic = self.holder() == Some(id);
                if had_mic {
                    self.take_mic_from_holder(gw);
                }
                let hung_up = self.hang_up_m17_leg_of(id, node);
                gw.announce(&banned_line(id, ADMIN_ACTOR, had_mic));
                if let Some(label) = hung_up {
                    gw.announce(&hungup_line(&label, ADMIN_ACTOR));
                }
            }
            AdminAction::Unban(id) => {
                let name = self.display_name(id);
                let r = self.approvals.unban(id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_banned = !matches!(&r, Ok(false));
                let _ = self.noting_write(r);
                if !was_banned {
                    return Err(format!("{name} is not banned"));
                }
                gw.announce(&unbanned_line(id, ADMIN_ACTOR));
            }
            AdminAction::MakeAdmin(id) => {
                let name = self.display_name(id);
                if self.approvals.is_banned(id) {
                    return Err(format!("{name} is banned on this node; unban them first"));
                }
                let entry = self.entry_for_id(id, ADMIN_BY.to_string());
                let r = self.approvals.add_admin(entry);
                let _ = self.noting_write(r);
                gw.announce(&admin_added_line(id, ADMIN_ACTOR));
            }
            AdminAction::RemoveAdmin(id) => {
                let name = self.display_name(id);
                if self.is_approver(id) {
                    return Err(format!("{name} is an approver in node.toml"));
                }
                let r = self.approvals.remove_admin(id);
                // `Approvals` removes in memory FIRST and saves second, so `Err` means
                // the removal happened and only the writing-down failed.
                // `Ok(false)` — and only that — means there was nothing to
                // remove.
                let was_admin = !matches!(&r, Ok(false));
                let _ = self.noting_write(r);
                if !was_admin {
                    return Err(format!("{name} is not an admin"));
                }
                // Same rule as the slash path, and for the same reason: a
                // holder the demotion leaves with no tier keeps no mic.
                let had_mic = self.holder() == Some(id) && !self.is_allowed(id);
                if had_mic {
                    self.take_mic_from_holder(gw);
                }
                gw.announce(&admin_removed_line(id, ADMIN_ACTOR, had_mic));
            }
            AdminAction::ReleaseMic => {
                let Some(holder) = self.holder() else {
                    return Err("nobody has the mic".to_string());
                };
                self.take_mic_from_holder(gw);
                gw.announce(&took_mic_line(holder, ADMIN_ACTOR));
            }
            AdminAction::Enable | AdminAction::Disable => {
                let on = matches!(a, AdminAction::Enable);
                let r = self.approvals.set_enabled(on);
                let _ = self.noting_write(r);
                if !on {
                    self.take_mic_from_holder(gw);
                }
                gw.announce(&switch_line(on, ADMIN_ACTOR));
            }
            AdminAction::Hangup => {
                let targets = self.hangup_targets(node);
                if targets.is_empty() {
                    return Err("nothing is linked".to_string());
                }
                for t in &targets {
                    node.hangup(t)
                        .map_err(|e| format!("couldn't hang up: {e}"))?;
                }
                self.link = None;
                gw.announce(&hungup_line(&link_labels(&targets), ADMIN_ACTOR));
            }
        }
        Ok(())
    }
}

/// Rule 4's reflector check, shared by `/dial m17` and `/dial dstar`: a name
/// that passes [`validate_reflector`] (returned uppercased) and an ASCII
/// module letter (returned uppercased). `None` means the caller has already
/// been told why, and the dial must not proceed.
fn validated_reflector(
    reflector: &str,
    module: char,
    it: u64,
    gw: &mut dyn GatewayOps,
) -> Option<(String, char)> {
    let name = match validate_reflector(reflector) {
        Ok(n) => n,
        Err(e) => {
            gw.reply(it, &e);
            return None;
        }
    };
    // Checked directly rather than through `validate_module`: a `char` can
    // never fail that function's "exactly one" arm, so routing through it
    // would look like a test covered a branch it cannot reach.
    // `validate_module` stays for the serenity layer, which parses a string
    // option.
    if !module.is_ascii_alphabetic() {
        gw.reply(it, MODULE_RULE);
        return None;
    }
    Some((name, module.to_ascii_uppercase()))
}

/// A [`GatewayOps`] that records instead of speaking, so a test can assert
/// on the exact vectors — which pins ordering, and proves nothing else
/// happened. Exported like `RecordingLink` is, for the tasks above this one.
#[derive(Debug, Default)]
pub struct RecordingOps {
    /// Every ephemeral reply, in order, as `(interaction, text)`.
    pub replies: Vec<(u64, String)>,
    /// Every public line, in order.
    pub announced: Vec<String>,
    /// Every mute call, in order, as `(user, mute)`.
    pub mutes: Vec<(u64, bool)>,
    /// Every PTT-panel redraw, in order.
    pub panels: Vec<Option<String>>,
}

impl GatewayOps for RecordingOps {
    fn reply(&mut self, interaction: u64, text: &str) {
        self.replies.push((interaction, text.to_string()));
    }

    fn announce(&mut self, text: &str) {
        self.announced.push(text.to_string());
    }

    fn set_mute(&mut self, user: u64, mute: bool) {
        self.mutes.push((user, mute));
    }

    fn panel(&mut self, talking: Option<&str>) {
        self.panels.push(talking.map(ToString::to_string));
    }
}

/// What `TestNode::dial_m17_monitor` answers on success, standing in for
/// the node's relay warning.
pub const TEST_RELAY_WARNING: &str = "Relay warning: linked radios transmit this reflector.";

/// A [`NodeOps`] with no node behind it.
#[derive(Debug, Default)]
pub struct TestNode {
    /// What `bridge_joined` answers.
    pub joined: bool,
    /// What `links` answers; a test moves this by hand to simulate a link
    /// coming up or going away.
    pub roster: Vec<LinkRef>,
    /// Every string that reached `dial_allstar_monitor`, in order. The
    /// safety test asserts this is empty.
    pub dialled: Vec<String>,
    /// Every `(reflector, module, callsign)` that reached
    /// `dial_m17_monitor`, in order.
    pub dialled_m17: Vec<(String, char, String)>,
    /// Every link that reached `hangup`, in order.
    pub hung_up: Vec<LinkRef>,
    /// When set, both dial verbs answer `Err` with this.
    pub fail_dial: Option<String>,
    /// When set, `hangup` answers `Err` with this.
    pub fail_hangup: Option<String>,
    /// What `link_end_reason` answers, for any link.
    pub end_reason: Option<String>,
    /// What `last_heard` answers, for any link.
    pub heard: Option<(String, u64)>,
}

impl NodeOps for TestNode {
    fn bridge_joined(&self) -> bool {
        self.joined
    }

    fn links(&self) -> Vec<LinkRef> {
        self.roster.clone()
    }

    fn dial_allstar_monitor(&mut self, node: &str) -> Result<(), String> {
        // Recorded before the failure branch on purpose: "did this string
        // reach the node" is the question the safety test asks, and a dial
        // the engine then refused still reached it.
        self.dialled.push(node.to_string());
        match &self.fail_dial {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn dial_m17_monitor(
        &mut self,
        reflector: &str,
        module: char,
        callsign: &str,
    ) -> Result<String, String> {
        // Recorded first, for the same reason as above.
        self.dialled_m17
            .push((reflector.to_string(), module, callsign.to_string()));
        match &self.fail_dial {
            Some(e) => Err(e.clone()),
            None => Ok(TEST_RELAY_WARNING.to_string()),
        }
    }

    fn hangup(&mut self, link: &LinkRef) -> Result<(), String> {
        self.hung_up.push(link.clone());
        match &self.fail_hangup {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn link_end_reason(&self, _link: &LinkRef) -> Option<String> {
        self.end_reason.clone()
    }

    fn last_heard(&self, _link: &LinkRef) -> Option<(String, u64)> {
        self.heard.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_PROTECTED, ALREADY_APPROVED, ALREADY_HAVE_MIC, ALREADY_PENDING,
        APPROVER_NOT_BANNABLE, APPROVER_NOT_REMOVABLE, APPROVER_NOT_REVOCABLE, AUDIO_BACK,
        AUDIO_STOPPED, BOT_OFF, BRIDGE_NOT_JOINED, Commander, DSTAR_NOT_WIRED, JOIN_VOICE_FIRST,
        LinkInfo, LinkRef, MUTE_PERMISSION, NEED_CALLSIGN, NOBODY_HAS_MIC, NOT_ADMIN,
        NOT_APPROVED_DIAL, NOT_APPROVED_LINK, NOT_APPROVED_MIC, NOT_APPROVER, NOT_THE_HOLDER,
        NOTHING_LINKED, Network, REQUEST_BANNED, RecordingOps, TEST_RELAY_WARNING, TestNode,
        WRITE_FAILED, link_labels,
    };
    use crate::approvals::{Approvals, UserEntry};
    use crate::commands::{
        AdminAction, BotCommand, BotCommandKind, CALLSIGN_RULE, DialTarget, GatewayEvent, UserRef,
        VoiceEvent,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    const APPROVER: u64 = 100;
    const ADMIN: u64 = 200;
    const APPROVED: u64 = 300;
    const OTHER: u64 = 400;
    const THIRD: u64 = 500;
    const CHANNEL: u64 = 77;
    const ELSEWHERE: u64 = 88;
    /// The interaction id every helper-driven command arrives on.
    const IT: u64 = 1;

    /// A clock that never moves, so `since_unix` and "how long ago" are exact
    /// values a test can assert rather than wall-clock noise.
    fn fixed_clock() -> u64 {
        1_000
    }

    /// Owned by `tick_gives_up_after_thirty_seconds_with_no_answer` alone —
    /// a `fn() -> u64` cannot capture, so a movable test clock has to be a
    /// static, and a static shared between parallel tests would race.
    static NO_ANSWER_CLOCK: AtomicU64 = AtomicU64::new(1_000);

    fn no_answer_clock() -> u64 {
        NO_ANSWER_CLOCK.load(Ordering::SeqCst)
    }

    /// Owned by `rule6_status_counts_in_minutes_once_a_minute_has_passed`
    /// alone, for the same reason.
    static STATUS_CLOCK: AtomicU64 = AtomicU64::new(5_000);

    fn status_clock() -> u64 {
        STATUS_CLOCK.load(Ordering::SeqCst)
    }

    fn user(id: u64) -> UserRef {
        UserRef {
            id,
            name: format!("u{id}"),
        }
    }

    fn entry(id: u64) -> UserEntry {
        UserEntry {
            user_id: id.to_string(),
            name: format!("u{id}"),
            by: String::new(),
            at_unix: 0,
        }
    }

    fn commander_with(approved: &[u64], admins: &[u64], banned: &[u64]) -> Commander {
        let mut a = Approvals::in_memory();
        for id in approved {
            a.approve(entry(*id))
                .expect("an in-memory save cannot fail");
        }
        for id in admins {
            a.add_admin(entry(*id))
                .expect("an in-memory save cannot fail");
        }
        for id in banned {
            a.ban(entry(*id)).expect("an in-memory save cannot fail");
        }
        Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock)
    }

    fn commander() -> Commander {
        commander_with(&[], &[], &[])
    }

    fn run(
        c: &mut Commander,
        gw: &mut RecordingOps,
        node: &mut TestNode,
        caller: u64,
        kind: BotCommandKind,
    ) {
        c.handle(
            GatewayEvent::Command(BotCommand {
                user: user(caller),
                kind,
                interaction: IT,
            }),
            gw,
            node,
        );
    }

    fn voice(c: &mut Commander, gw: &mut RecordingOps, id: u64, channel: Option<u64>, muted: bool) {
        c.handle(
            GatewayEvent::Voice(VoiceEvent {
                user: user(id),
                channel,
                server_muted: muted,
                is_bot: false,
            }),
            gw,
            &mut TestNode::default(),
        );
    }

    /// Joins someone already server-muted, which is what the READY sweep
    /// leaves everyone as. Rules 10 and 12 make that a no-op, so a test can
    /// populate the channel without polluting the recorder.
    fn join_muted(c: &mut Commander, id: u64) {
        let mut quiet = RecordingOps::default();
        voice(c, &mut quiet, id, Some(CHANNEL), true);
        assert!(
            quiet.mutes.is_empty() && quiet.announced.is_empty(),
            "joining already-muted must be silent, got {quiet:?}"
        );
    }

    /// Everything a refusal must leave alone, as one comparable string.
    fn fingerprint(c: &Commander) -> String {
        let v = c.view();
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            v.enabled, v.admins, v.approved, v.banned, v.pending, v.callsigns, v.mic_holder, v.link
        )
    }

    /// A refusal is `reply` and nothing else: no public line, no mute, no
    /// panel redraw, no node traffic, no state change.
    fn refused(c: &Commander, gw: &RecordingOps, node: &TestNode, before: &str, text: &str) {
        assert_eq!(gw.replies, vec![(IT, text.to_string())]);
        assert!(
            gw.announced.is_empty(),
            "a refusal must announce nothing, got {:?}",
            gw.announced
        );
        assert!(
            gw.mutes.is_empty(),
            "a refusal must mute nobody, got {:?}",
            gw.mutes
        );
        assert!(
            node.dialled.is_empty(),
            "a refusal must dial nothing, got {:?}",
            node.dialled
        );
        assert!(
            node.dialled_m17.is_empty(),
            "a refusal must dial no reflector, got {:?}",
            node.dialled_m17
        );
        assert!(
            node.hung_up.is_empty(),
            "a refusal must hang up nothing, got {:?}",
            node.hung_up
        );
        assert!(
            gw.panels.is_empty(),
            "a refusal must not redraw the PTT panel, got {:?}",
            gw.panels
        );
        assert_eq!(fingerprint(c), before, "a refusal must not change state");
    }

    fn register(callsign: &str) -> BotCommandKind {
        BotCommandKind::Request {
            callsign: callsign.to_string(),
        }
    }

    fn dial(node: &str) -> BotCommandKind {
        BotCommandKind::Dial(DialTarget::Allstar {
            node: node.to_string(),
        })
    }

    // -- Rule 1: the master switch ---------------------------------------

    #[test]
    fn rule1_master_switch_off_refuses_request_dial_and_mic() {
        for kind in [
            register("AJ7HR"),
            dial("44444"),
            BotCommandKind::Mic { on: true },
        ] {
            let mut a = Approvals::in_memory();
            a.approve(entry(APPROVED))
                .expect("an in-memory save cannot fail");
            a.set_enabled(false).expect("an in-memory save cannot fail");
            let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock);
            join_muted(&mut c, APPROVED);
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            let mut node = TestNode {
                joined: true,
                ..TestNode::default()
            };
            run(&mut c, &mut gw, &mut node, APPROVED, kind);
            refused(&c, &gw, &node, &before, BOT_OFF);
        }
    }

    #[test]
    fn rule1_master_switch_off_still_allows_status_hangup_approve_and_mic_off() {
        let mut a = Approvals::in_memory();
        a.approve(entry(APPROVED))
            .expect("an in-memory save cannot fail");
        a.set_enabled(false).expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Approve {
                target: user(OTHER),
            },
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: false },
        );

        assert_eq!(
            gw.replies,
            vec![
                (
                    IT,
                    "Not linked. Nobody has the mic. Discord commands are off.".to_string()
                ),
                (IT, NOTHING_LINKED.to_string()),
                (IT, "Approved u400.".to_string()),
                (IT, NOT_THE_HOLDER.to_string()),
            ]
        );
        assert!(
            c.view().approved.iter().any(|e| e.user_id == "400"),
            "the approval landed even with the switch off"
        );
    }

    // -- Rule 2: /register ------------------------------------------------

    #[test]
    fn rule2_register_from_a_banned_user_is_refused_and_records_nothing() {
        let mut c = commander_with(&[], &[], &[OTHER]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, register("AJ7HR"));
        // `fingerprint` covers the callsign map: a banned user's callsign
        // is not kept either.
        refused(&c, &gw, &node, &before, REQUEST_BANNED);
    }

    #[test]
    fn rule2_register_refuses_a_bad_callsign_and_records_nothing() {
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, register("AJ7HR/P"));
        refused(&c, &gw, &node, &before, CALLSIGN_RULE);
        assert!(
            c.view().pending.is_empty(),
            "a refused callsign must not queue a request either"
        );
    }

    #[test]
    fn rule2_register_from_an_approved_user_records_the_callsign_only() {
        // Approved users — and approvers, who are approved by config rather
        // than by state — predate the callsign or may need to fix one, so
        // `/register` is how they set it. Nothing is queued and the channel
        // is not pinged; the callsign is normalised on the way in.
        for (c, who) in [
            (commander_with(&[APPROVED], &[], &[]), APPROVED),
            (commander(), APPROVER),
        ] {
            let mut c = c;
            let mut gw = RecordingOps::default();
            let mut node = TestNode::default();
            run(&mut c, &mut gw, &mut node, who, register(" aj7hr "));
            assert_eq!(
                gw.replies,
                vec![(IT, format!("Callsign AJ7HR recorded. {ALREADY_APPROVED}"))]
            );
            assert!(gw.announced.is_empty(), "no ping for a callsign update");
            let view = c.view();
            assert_eq!(
                view.callsigns.get(&who.to_string()).map(String::as_str),
                Some("AJ7HR")
            );
            assert!(view.pending.is_empty(), "nothing to approve");
        }
    }

    #[test]
    fn rule2_register_queues_and_announces_once_and_a_repeat_updates_the_callsign() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        run(&mut c, &mut gw, &mut node, OTHER, register("aj7hr"));
        assert_eq!(gw.replies, vec![(IT, "Request sent as AJ7HR.".to_string())]);
        assert_eq!(
            gw.announced,
            vec![
                "<@400> (AJ7HR) is asking for dial access — an approver can `/approve` them."
                    .to_string()
            ]
        );
        let view = c.view();
        let pending: Vec<&str> = view.pending.iter().map(|e| e.user_id.as_str()).collect();
        assert_eq!(pending, vec!["400"]);
        assert_eq!(view.callsigns.get("400").map(String::as_str), Some("AJ7HR"));

        // A second /register must not ping the channel again — but it does
        // take the new callsign, because that is how a typo gets fixed
        // while the request is still waiting.
        run(&mut c, &mut gw, &mut node, OTHER, register("W1AW"));
        assert_eq!(
            gw.replies,
            vec![
                (IT, "Request sent as AJ7HR.".to_string()),
                (IT, format!("Callsign W1AW recorded. {ALREADY_PENDING}")),
            ]
        );
        assert_eq!(
            gw.announced.len(),
            1,
            "one ping per person, not per attempt"
        );
        let view = c.view();
        assert_eq!(view.callsigns.get("400").map(String::as_str), Some("W1AW"));
        assert_eq!(view.pending.len(), 1, "still one pending entry");
    }

    #[test]
    fn a_callsign_outlives_approval_and_revocation() {
        // The callsign is a fact about the person, not about the list they
        // are on: approving them must not lose it, and neither must
        // revoking them — they will be approved again with the same call.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, register("AJ7HR"));
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Approve {
                target: user(OTHER),
            },
        );
        assert!(c.view().approved.iter().any(|e| e.user_id == "400"));
        assert_eq!(
            c.view().callsigns.get("400").map(String::as_str),
            Some("AJ7HR")
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Revoke {
                target: user(OTHER),
            },
        );
        assert!(!c.view().approved.iter().any(|e| e.user_id == "400"));
        assert_eq!(
            c.view().callsigns.get("400").map(String::as_str),
            Some("AJ7HR")
        );
    }

    // -- Rule 3: /approve and /revoke -------------------------------------

    #[test]
    fn rule3_approve_or_revoke_by_a_non_admin_is_refused() {
        for kind in [
            BotCommandKind::Approve {
                target: user(THIRD),
            },
            BotCommandKind::Revoke {
                target: user(THIRD),
            },
        ] {
            let mut c = commander_with(&[APPROVED, THIRD], &[], &[]);
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            let mut node = TestNode::default();
            run(&mut c, &mut gw, &mut node, APPROVED, kind);
            refused(&c, &gw, &node, &before, NOT_ADMIN);
        }
    }

    #[test]
    fn rule3_approve_of_a_banned_target_is_refused() {
        let mut c = commander_with(&[], &[ADMIN], &[OTHER]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Approve {
                target: user(OTHER),
            },
        );
        refused(
            &c,
            &gw,
            &node,
            &before,
            "u400 is banned on this node; unban them from the node's admin page first.",
        );
    }

    #[test]
    fn rule3_approve_records_announces_and_does_not_unmute() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Approve {
                target: user(OTHER),
            },
        );
        assert_eq!(gw.replies, vec![(IT, "Approved u400.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@400> was approved by <@200>.".to_string()]
        );
        // Approval is permission to take the mic, not the mic.
        assert!(
            gw.mutes.is_empty(),
            "approving must not unmute, got {:?}",
            gw.mutes
        );
        assert!(c.view().approved.iter().any(|e| e.user_id == "400"));
    }

    #[test]
    fn rule3_revoke_of_an_approver_is_refused() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Revoke {
                target: user(APPROVER),
            },
        );
        refused(&c, &gw, &node, &before, APPROVER_NOT_REVOCABLE);
    }

    #[test]
    fn rule3_revoke_of_an_admin_by_a_plain_admin_is_refused() {
        let mut c = commander_with(&[THIRD], &[ADMIN, THIRD], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Revoke {
                target: user(THIRD),
            },
        );
        refused(&c, &gw, &node, &before, ADMIN_PROTECTED);
    }

    #[test]
    fn rule3_revoke_takes_the_mic_from_the_holder() {
        let mut c = commander_with(&[APPROVED], &[ADMIN], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Revoke {
                target: user(APPROVED),
            },
        );
        assert_eq!(gw.replies, vec![(IT, "Revoked u300.".to_string())]);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.announced,
            vec!["<@300> was revoked by <@200> and no longer has the mic.".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);
        assert_eq!(c.view().approved, [] as [crate::approvals::UserEntry; 0]);
    }

    // -- Rule 4: /dial ----------------------------------------------------

    #[test]
    fn rule4_dial_by_an_unapproved_user_is_refused() {
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, OTHER, dial("44444"));
        refused(&c, &gw, &node, &before, NOT_APPROVED_DIAL);
    }

    #[test]
    fn dial_never_reaches_the_node_with_anything_but_a_validated_node() {
        // The whole reason this module exists: an address, an empty string,
        // an over-long number or anything with a non-digit in it must be
        // stopped here, in the rule engine, not by the engine's own parser.
        for bad in [
            "1.2.3.4:4569",
            "",
            "12345678",
            "12a4",
            " 123",
            // Full-width digits are `is_numeric` but not `is_ascii_digit`,
            // and the last three are what a lazy "parses as a number" check
            // would wave through.
            "\u{ff11}\u{ff12}\u{ff13}",
            "+123",
            "-1",
            "123 ",
        ] {
            let mut c = commander_with(&[APPROVED], &[], &[]);
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            let mut node = TestNode {
                joined: true,
                ..TestNode::default()
            };
            run(&mut c, &mut gw, &mut node, APPROVED, dial(bad));
            assert!(
                node.dialled.is_empty(),
                "{bad:?} reached the node: {:?}",
                node.dialled
            );
            assert_eq!(
                gw.replies.len(),
                1,
                "{bad:?} should produce exactly one refusal, got {:?}",
                gw.replies
            );
            assert!(
                gw.announced.is_empty(),
                "{bad:?} must not be announced, got {:?}",
                gw.announced
            );
            assert_eq!(fingerprint(&c), before, "{bad:?} must not change state");
            assert!(c.view().link.is_none(), "{bad:?} must not record a link");
        }
    }

    /// A commander with `APPROVED` approved and holding `callsign`, as
    /// `/register` leaves them.
    fn commander_with_callsign(callsign: &str) -> Commander {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut TestNode::default(),
            APPROVED,
            register(callsign),
        );
        c
    }

    fn dial_m17(reflector: &str, module: char) -> BotCommandKind {
        BotCommandKind::Dial(DialTarget::M17 {
            reflector: reflector.to_string(),
            module,
        })
    }

    fn joined() -> TestNode {
        TestNode {
            joined: true,
            ..TestNode::default()
        }
    }

    #[test]
    fn rule4_dstar_still_refuses_honestly() {
        // No callsign on purpose: D-Star's refusal is not hidden behind
        // M17's callsign rule.
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Dial(DialTarget::Dstar {
                reflector: "xrf234".to_string(),
                module: 'b',
            }),
        );
        refused(&c, &gw, &node, &before, DSTAR_NOT_WIRED);
    }

    #[test]
    fn rule4_m17_without_a_callsign_is_told_to_register() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        refused(&c, &gw, &node, &before, NEED_CALLSIGN);
        assert_eq!(
            NEED_CALLSIGN,
            "M17 needs your callsign — run `/register callsign:<yours>` first."
        );
    }

    #[test]
    fn rule4_m17_checks_approval_then_shape_then_callsign_then_the_bridge() {
        // Not approved and no callsign: the approval refusal keeps first place.
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(&mut c, &mut gw, &mut node, OTHER, dial_m17("M17-USA", 'C'));
        refused(&c, &gw, &node, &before, NOT_APPROVED_DIAL);

        // Approved, no callsign, a typo: the typo is named as a typo.
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            dial_m17("kc_wide", 'a'),
        );
        refused(
            &c,
            &gw,
            &node,
            &before,
            "A reflector name is letters, digits and '-' only.",
        );

        // Approved, no callsign, bridge not in the channel: the callsign
        // comes first, because it is the thing the caller can fix.
        let mut gw = RecordingOps::default();
        let mut away = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut away,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        refused(&c, &gw, &away, &before, NEED_CALLSIGN);

        // With a callsign, the bridge rule applies as it does to AllStar.
        let mut c = commander_with_callsign("AJ7HR");
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut away = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut away,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        refused(&c, &gw, &away, &before, BRIDGE_NOT_JOINED);
    }

    #[test]
    fn rule4_m17_dial_presents_the_callers_callsign_and_announces_it() {
        let mut c = commander_with_callsign("aj7hr");
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            dial_m17("m17-usa", 'c'),
        );
        assert_eq!(
            node.dialled_m17,
            vec![("M17-USA".to_string(), 'C', "AJ7HR".to_string())],
            "the node gets the validated, uppercased name and module and the registered callsign"
        );
        assert!(
            node.dialled.is_empty(),
            "an M17 dial never reaches the AllStar verb, got {:?}",
            node.dialled
        );
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                format!("Dialling M17 M17-USA C as AJ7HR (receive-only)…\n{TEST_RELAY_WARNING}")
            )],
            "the caller is told, in the reply, what the node's warning says"
        );
        assert_eq!(
            gw.announced,
            vec![format!(
                "<@300> → M17 M17-USA C (receive-only, as AJ7HR)\n{TEST_RELAY_WARNING}"
            )],
            "and so is the whole channel"
        );
        assert_eq!(
            c.view().link,
            Some(LinkInfo {
                network: Network::M17,
                target: "M17-USA C".to_string(),
                by: "u300".to_string(),
                by_id: Some(APPROVED),
                callsign: Some("AJ7HR".to_string()),
                since_unix: 1_000,
                up: false,
            })
        );
    }

    #[test]
    fn rule4_an_m17_dial_the_node_refuses_leaves_no_link() {
        let mut c = commander_with_callsign("AJ7HR");
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            fail_dial: Some(
                "M17 isn't enabled on this node — its operator can add an `[m17]` section."
                    .to_string(),
            ),
            ..TestNode::default()
        };
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        assert_eq!(
            node.dialled_m17,
            vec![("M17-USA".to_string(), 'C', "AJ7HR".to_string())]
        );
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Couldn't dial: M17 isn't enabled on this node — its operator can add an \
                 `[m17]` section."
                    .to_string()
            )]
        );
        assert!(
            gw.announced.is_empty(),
            "a refused dial must not be announced, got {:?}",
            gw.announced
        );
        assert!(
            c.view().link.is_none(),
            "a refused dial must record no link"
        );
    }

    #[test]
    fn rule4_an_m17_link_and_an_allstar_link_are_busy_for_each_other() {
        // The commander's AllStar link blocks an M17 dial.
        let mut c = commander_with_callsign("AJ7HR");
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut joined(),
            APPROVED,
            dial("44444"),
        );
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        refused(
            &c,
            &gw,
            &node,
            &before,
            "Already listening to AllStar 44444 (dialled by u300). `/hangup` first.",
        );

        // The commander's M17 link blocks an AllStar dial.
        let mut c = commander_with_callsign("AJ7HR");
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut joined(),
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = joined();
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        refused(
            &c,
            &gw,
            &node,
            &before,
            "Already listening to M17 M17-USA C (dialled by u300). `/hangup` first.",
        );

        // An M17 leg the node holds from its control port blocks both.
        let mut c = commander_with_callsign("AJ7HR");
        let before = fingerprint(&c);
        for kind in [dial("44444"), dial_m17("M17-EU", 'A')] {
            let mut gw = RecordingOps::default();
            let mut node = TestNode {
                joined: true,
                roster: vec![LinkRef::m17("M17-USA", 'C')],
                ..TestNode::default()
            };
            run(&mut c, &mut gw, &mut node, APPROVED, kind);
            refused(
                &c,
                &gw,
                &node,
                &before,
                "Already listening to M17 M17-USA C (dialled from the node). `/hangup` first.",
            );
        }
    }

    #[test]
    fn rule5_hangup_of_an_m17_leg_still_linking_uses_the_commanders_own_link() {
        // Not in the roster yet (no ACKN), so `/hangup` falls back to the
        // link the commander placed, and a slow CONN can still be stopped.
        let mut c = commander_with_callsign("AJ7HR");
        let mut node = joined();
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        assert_eq!(node.hung_up, vec![LinkRef::m17("M17-USA", 'C')]);
        assert_eq!(gw.replies, vec![(IT, "Hung up M17 M17-USA C.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@300> hung up M17 M17-USA C.".to_string()]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn admin_page_hangup_of_a_linked_m17_leg_names_it() {
        let mut c = commander_with_callsign("AJ7HR");
        let mut node = joined();
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        node.roster = vec![LinkRef::m17("M17-USA", 'C')];
        let mut gw = RecordingOps::default();
        c.admin(AdminAction::Hangup, &mut gw, &mut node)
            .expect("the page may hang up");
        assert_eq!(node.hung_up, vec![LinkRef::m17("M17-USA", 'C')]);
        assert_eq!(
            gw.announced,
            vec!["the node admin hung up M17 M17-USA C.".to_string()]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn rule4_a_bad_reflector_or_module_is_reported_as_a_typo() {
        // Validation runs first, so a typo is named as a typo rather than
        // hidden behind the callsign rule (M17) or the not-wired refusal
        // (D-Star).
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Dial(DialTarget::M17 {
                reflector: "kc_wide".to_string(),
                module: 'a',
            }),
        );
        refused(
            &c,
            &gw,
            &node,
            &before,
            "A reflector name is letters, digits and '-' only.",
        );

        // A digit is not a module, and neither is a letter Discord let
        // through from some other alphabet.
        for module in ['7', '\u{3b1}'] {
            let mut gw = RecordingOps::default();
            run(
                &mut c,
                &mut gw,
                &mut node,
                APPROVED,
                BotCommandKind::Dial(DialTarget::Dstar {
                    reflector: "xrf234".to_string(),
                    module,
                }),
            );
            refused(
                &c,
                &gw,
                &node,
                &before,
                "A module is a single letter, A to Z.",
            );
        }
    }

    #[test]
    fn rule4_dial_requires_the_bridge_in_the_channel() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        refused(&c, &gw, &node, &before, BRIDGE_NOT_JOINED);
    }

    #[test]
    fn rule4_a_second_dial_names_the_dialler() {
        let mut c = commander_with(&[APPROVED, THIRD], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));

        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, THIRD, dial("55555"));
        refused(
            &c,
            &gw,
            &node,
            &before,
            "Already listening to AllStar 44444 (dialled by u300). `/hangup` first.",
        );
    }

    #[test]
    fn rule4_a_link_the_commander_did_not_dial_says_dialled_from_the_node() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            roster: vec![LinkRef::allstar("12345")],
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        refused(
            &c,
            &gw,
            &node,
            &before,
            "Already listening to AllStar 12345 (dialled from the node). `/hangup` first.",
        );
    }

    #[test]
    fn rule4_a_dial_the_engine_refuses_leaves_no_link() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            fail_dial: Some("no route to that node".to_string()),
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        assert_eq!(node.dialled, vec!["44444".to_string()]);
        assert_eq!(
            gw.replies,
            vec![(IT, "Couldn't dial: no route to that node".to_string())]
        );
        assert!(
            gw.announced.is_empty(),
            "a failed dial must not be announced, got {:?}",
            gw.announced
        );
        assert!(c.view().link.is_none(), "a failed dial must record no link");
    }

    #[test]
    fn rule4_dial_records_the_link_and_announces() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        assert_eq!(node.dialled, vec!["44444".to_string()]);
        assert_eq!(
            gw.replies,
            vec![(IT, "Dialling AllStar 44444 (receive-only)…".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["<@300> → AllStar 44444 (receive-only)".to_string()]
        );
        assert_eq!(
            c.view().link,
            Some(LinkInfo {
                network: Network::Allstar,
                target: "44444".to_string(),
                by: "u300".to_string(),
                by_id: Some(APPROVED),
                callsign: None,
                since_unix: 1_000,
                up: false,
            })
        );
    }

    // -- Rule 5: /hangup --------------------------------------------------

    #[test]
    fn rule5_hangup_by_an_unapproved_user_is_refused() {
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            roster: vec![LinkRef::allstar("44444")],
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Hangup);
        // Not the /dial line: someone trying to STOP a link must not be told
        // they are not approved to dial.
        refused(&c, &gw, &node, &before, NOT_APPROVED_LINK);
    }

    #[test]
    fn rule5_hangup_with_nothing_linked_says_so() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        refused(&c, &gw, &node, &before, NOTHING_LINKED);
    }

    #[test]
    fn rule5_hangup_clears_the_link_and_announces() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        node.roster = vec![LinkRef::allstar("44444")];

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        assert_eq!(node.hung_up, vec![LinkRef::allstar("44444")]);
        assert_eq!(gw.replies, vec![(IT, "Hung up AllStar 44444.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@300> hung up AllStar 44444.".to_string()]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn rule5_hangup_works_on_a_link_the_commander_did_not_dial() {
        // Hanging up is the safe direction, so it is not restricted to
        // whoever dialled — and the node's own control port counts as
        // somebody else.
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            roster: vec![LinkRef::allstar("12345")],
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        assert_eq!(node.hung_up, vec![LinkRef::allstar("12345")]);
        assert_eq!(
            gw.announced,
            vec!["<@300> hung up AllStar 12345.".to_string()]
        );
    }

    #[test]
    fn rule5_a_hangup_the_engine_refuses_keeps_the_link() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        node.roster = vec![LinkRef::allstar("44444")];
        node.fail_hangup = Some("the link is busy".to_string());

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
        assert_eq!(
            gw.replies,
            vec![(IT, "Couldn't hang up: the link is busy".to_string())]
        );
        assert!(
            gw.announced.is_empty(),
            "a failed hangup must not be announced, got {:?}",
            gw.announced
        );
        assert!(
            c.view().link.is_some(),
            "a link that would not hang up is still up"
        );
    }

    // -- Rule 6: /status --------------------------------------------------

    #[test]
    fn rule6_status_with_nothing_linked() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(IT, "Not linked. Nobody has the mic.".to_string())]
        );
    }

    #[test]
    fn rule6_status_reports_the_link_and_the_mic_holder() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Dialling AllStar 44444 — asked by u300, 0s ago. u300 has the mic.".to_string()
            )]
        );
    }

    // -- Rule 7: /mic -----------------------------------------------------

    #[test]
    fn rule7_mic_by_an_unapproved_user_is_refused() {
        let mut c = commander();
        join_muted(&mut c, OTHER);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            OTHER,
            BotCommandKind::Mic { on: true },
        );
        refused(&c, &gw, &node, &before, NOT_APPROVED_MIC);
    }

    #[test]
    fn rule7_mic_from_outside_the_channel_is_refused() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        refused(&c, &gw, &node, &before, JOIN_VOICE_FIRST);
    }

    #[test]
    fn rule7_mic_when_you_already_hold_it_is_a_no_op() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        let before = fingerprint(&c);

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        refused(&c, &gw, &node, &before, ALREADY_HAVE_MIC);
    }

    #[test]
    fn rule7_mic_handover_unmutes_the_new_holder_before_muting_the_old() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVER);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(gw.mutes, vec![(APPROVER, false)]);
        assert_eq!(gw.announced, vec!["<@100> has the mic".to_string()]);

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        // The order is the rule: the new holder is opened before the old
        // one is closed, so the channel is never doubly open.
        assert_eq!(gw.mutes, vec![(APPROVED, false), (APPROVER, true)]);
        assert_eq!(gw.replies, vec![(IT, "You have the mic.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@300> has the mic (was <@100>)".to_string()]
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    // -- Rule 8: /mic state:off -------------------------------------------

    #[test]
    fn rule8_mic_off_by_a_non_holder_is_refused() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: false },
        );
        refused(&c, &gw, &node, &before, NOT_THE_HOLDER);
    }

    #[test]
    fn rule8_mic_off_mutes_the_holder_and_clears_the_mic() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: false },
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.replies, vec![(IT, "Mic released.".to_string())]);
        assert_eq!(gw.announced, vec!["<@300> released the mic".to_string()]);
        assert_eq!(c.view().mic_holder, None);
    }

    // -- Rule 8a: /ban and /unban -----------------------------------------

    #[test]
    fn rule8a_ban_by_a_non_admin_is_refused() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Ban {
                target: user(OTHER),
            },
        );
        refused(&c, &gw, &node, &before, NOT_ADMIN);
    }

    #[test]
    fn rule8a_ban_or_unban_of_an_approver_is_refused() {
        for kind in [
            BotCommandKind::Ban {
                target: user(APPROVER),
            },
            BotCommandKind::Unban {
                target: user(APPROVER),
            },
        ] {
            let mut c = commander_with(&[], &[ADMIN], &[]);
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            let mut node = TestNode::default();
            run(&mut c, &mut gw, &mut node, ADMIN, kind);
            refused(&c, &gw, &node, &before, APPROVER_NOT_BANNABLE);
        }
    }

    #[test]
    fn rule8a_ban_of_an_admin_by_a_plain_admin_is_refused() {
        let mut c = commander_with(&[], &[ADMIN, THIRD], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Ban {
                target: user(THIRD),
            },
        );
        refused(&c, &gw, &node, &before, ADMIN_PROTECTED);
    }

    #[test]
    fn rule8a_ban_strips_approval_and_admin_and_takes_the_mic() {
        let mut c = commander_with(&[APPROVED], &[APPROVED], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Ban {
                target: user(APPROVED),
            },
        );
        assert_eq!(gw.replies, vec![(IT, "Banned u300.".to_string())]);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.announced,
            vec!["<@300> was banned by <@100> and no longer has the mic.".to_string()]
        );
        let v = c.view();
        assert_eq!(v.mic_holder, None);
        assert!(v.approved.is_empty(), "a ban revokes");
        assert!(v.admins.is_empty(), "a ban demotes");
        assert_eq!(
            v.banned
                .iter()
                .map(|e| e.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["300"]
        );
    }

    #[test]
    fn rule8a_unban_of_someone_not_banned_says_so() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Unban {
                target: user(OTHER),
            },
        );
        refused(&c, &gw, &node, &before, "u400 isn't banned.");
    }

    #[test]
    fn rule8a_unban_lifts_the_ban_and_announces() {
        let mut c = commander_with(&[], &[ADMIN], &[OTHER]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Unban {
                target: user(OTHER),
            },
        );
        assert_eq!(gw.replies, vec![(IT, "Unbanned u400.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@400> was unbanned by <@200>.".to_string()]
        );
        assert_eq!(c.view().banned, [] as [crate::approvals::UserEntry; 0]);
    }

    // -- Rule 8b: /unmic ---------------------------------------------------

    #[test]
    fn rule8b_unmic_by_a_non_admin_is_refused() {
        let mut c = commander_with(&[APPROVED, THIRD], &[], &[]);
        join_muted(&mut c, THIRD);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: true },
        );
        let before = fingerprint(&c);

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Unmic);
        refused(&c, &gw, &node, &before, NOT_ADMIN);
    }

    #[test]
    fn rule8b_unmic_with_nobody_holding_says_so() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, ADMIN, BotCommandKind::Unmic);
        refused(&c, &gw, &node, &before, NOBODY_HAS_MIC);
    }

    #[test]
    fn rule8b_unmic_of_an_admin_holder_by_a_plain_admin_is_refused() {
        let mut c = commander_with(&[], &[ADMIN, THIRD], &[]);
        join_muted(&mut c, THIRD);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: true },
        );
        let before = fingerprint(&c);

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, ADMIN, BotCommandKind::Unmic);
        refused(&c, &gw, &node, &before, ADMIN_PROTECTED);
    }

    #[test]
    fn rule8b_unmic_takes_the_mic_and_announces() {
        let mut c = commander_with(&[APPROVED], &[ADMIN], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, ADMIN, BotCommandKind::Unmic);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.replies,
            vec![(IT, "Took the mic from u300.".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["<@200> took the mic from <@300>".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);
    }

    // -- Rule 8c: /bot ----------------------------------------------------

    #[test]
    fn rule8c_bot_by_a_non_admin_is_refused() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Bot { on: false },
        );
        refused(&c, &gw, &node, &before, NOT_ADMIN);
    }

    #[test]
    fn rule8c_bot_off_takes_the_mic() {
        let mut c = commander_with(&[APPROVED], &[ADMIN], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Bot { on: false },
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.replies,
            vec![(IT, "Discord commands are now off.".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["Discord commands were turned off by <@200>.".to_string()]
        );
        let v = c.view();
        assert!(!v.enabled);
        assert_eq!(v.mic_holder, None);
    }

    #[test]
    fn rule8c_bot_on_works_while_the_switch_is_off() {
        // Rule 1 must not be able to trap the node with its own switch.
        let mut a = Approvals::in_memory();
        a.set_enabled(false).expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Bot { on: true },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, "Discord commands are now on.".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["Discord commands were turned on by <@100>.".to_string()]
        );
        assert!(c.view().enabled);
    }

    // -- Rule 8d / 8e: /admin add and /admin remove ------------------------

    #[test]
    fn rule8d_admin_add_by_a_plain_admin_is_refused() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::AdminAdd {
                target: user(THIRD),
            },
        );
        refused(&c, &gw, &node, &before, NOT_APPROVER);
    }

    #[test]
    fn rule8e_an_admin_cannot_escalate_a_third_user_to_admin() {
        // The refusal text is the least of it: the assertion that matters is
        // that the admin list did not move.
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let admins_before: Vec<String> =
            c.view().admins.iter().map(|e| e.user_id.clone()).collect();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::AdminAdd {
                target: user(THIRD),
            },
        );
        let admins_after: Vec<String> = c.view().admins.iter().map(|e| e.user_id.clone()).collect();
        assert_eq!(admins_before, vec!["200".to_string()]);
        assert_eq!(
            admins_after, admins_before,
            "an admin must not be able to mint another admin"
        );
        assert!(
            !c.view().admins.iter().any(|e| e.user_id == "500"),
            "the third user must not have become an admin"
        );
    }

    #[test]
    fn rule8d_admin_add_of_a_banned_user_is_refused() {
        let mut c = commander_with(&[], &[], &[OTHER]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminAdd {
                target: user(OTHER),
            },
        );
        refused(
            &c,
            &gw,
            &node,
            &before,
            "u400 is banned on this node; unban them from the node's admin page first.",
        );
    }

    #[test]
    fn rule8d_admin_add_makes_an_admin_and_announces() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminAdd {
                target: user(THIRD),
            },
        );
        assert_eq!(gw.replies, vec![(IT, "u500 is now an admin.".to_string())]);
        assert_eq!(
            gw.announced,
            vec!["<@500> is now an admin, added by <@100>.".to_string()]
        );
        // An admin is allowed by tier, without being separately approved.
        assert!(c.view().approved.is_empty(), "no implicit approval");
        assert!(c.view().admins.iter().any(|e| e.user_id == "500"));
    }

    #[test]
    fn rule8d_admin_remove_of_an_approver_is_refused() {
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(APPROVER),
            },
        );
        refused(&c, &gw, &node, &before, APPROVER_NOT_REMOVABLE);
    }

    #[test]
    fn rule8d_admin_remove_of_a_non_admin_says_so() {
        let mut c = commander();
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(THIRD),
            },
        );
        refused(&c, &gw, &node, &before, "u500 isn't an admin.");
    }

    #[test]
    fn rule8d_admin_remove_demotes_and_announces() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(ADMIN),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, "u200 is no longer an admin.".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["<@200> is no longer an admin, removed by <@100>.".to_string()]
        );
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
    }

    /// A demoted admin who was never separately approved stops being
    /// `is_allowed` the moment the demotion lands — and used to keep the
    /// open mic anyway, until a timer fired. With `mic_timeout_secs = 0`
    /// and `mic_silence_secs = 0`, both documented settings, no timer ever
    /// fires: the mic would stay open indefinitely for somebody with no
    /// tier left. Same bug class as the revoke case above.
    #[test]
    fn rule8d_admin_remove_takes_the_mic_from_a_holder_who_has_no_tier_left() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        // Both timers off, so nothing but this rule can close the mic.
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        join_muted(&mut c, ADMIN);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("200".to_string()));

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(ADMIN),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, "u200 is no longer an admin.".to_string())]
        );
        assert_eq!(gw.mutes, vec![(ADMIN, true)]);
        assert_eq!(
            gw.announced,
            vec![
                "<@200> is no longer an admin, removed by <@100>, and no longer has the mic."
                    .to_string()
            ]
        );
        assert_eq!(gw.panels, vec![None], "the PTT panel shows nobody holding");
        assert_eq!(c.view().mic_holder, None);
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
    }

    /// The other half of the same rule: demotion takes the ADMIN tier, not
    /// the mic. Someone who is separately approved is still allowed to hold
    /// it, and yanking it would be a second punishment nobody asked for.
    #[test]
    fn rule8d_admin_remove_leaves_the_mic_with_a_holder_who_is_also_approved() {
        let mut c = commander_with(&[ADMIN], &[ADMIN], &[]);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        join_muted(&mut c, ADMIN);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("200".to_string()));

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(ADMIN),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, "u200 is no longer an admin.".to_string())]
        );
        assert!(gw.mutes.is_empty(), "an approved holder keeps the mic");
        assert_eq!(
            gw.announced,
            vec!["<@200> is no longer an admin, removed by <@100>.".to_string()]
        );
        assert!(gw.panels.is_empty(), "and the panel has nothing to redraw");
        assert_eq!(c.view().mic_holder, Some("200".to_string()));
    }

    #[test]
    fn rule3_revoke_of_a_never_approved_user_says_so() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Revoke {
                target: user(THIRD),
            },
        );
        // No public line: a revoke that revoked nothing does not belong in
        // the channel's audit trail.
        refused(&c, &gw, &node, &before, "u500 isn't approved.");
    }

    #[test]
    fn rule8b_an_admin_may_unmic_their_own_mic() {
        // The admin-protection rule is about acting on OTHER admins. An
        // admin who cannot put their own mic down would be stuck with it.
        let mut c = commander_with(&[], &[ADMIN], &[]);
        join_muted(&mut c, ADMIN);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("200".to_string()));

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, ADMIN, BotCommandKind::Unmic);
        assert_eq!(gw.mutes, vec![(ADMIN, true)]);
        assert_eq!(
            gw.replies,
            vec![(IT, "Took the mic from u200.".to_string())]
        );
        assert_eq!(
            gw.announced,
            vec!["<@200> took the mic from <@200>".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);
    }

    #[test]
    fn rule8b_a_plain_admin_cannot_unmic_an_approver() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        join_muted(&mut c, APPROVER);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Mic { on: true },
        );
        let before = fingerprint(&c);
        assert_eq!(c.view().mic_holder, Some("100".to_string()));

        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, ADMIN, BotCommandKind::Unmic);
        refused(&c, &gw, &node, &before, ADMIN_PROTECTED);
        assert_eq!(
            c.view().mic_holder,
            Some("100".to_string()),
            "the approver still holds it"
        );
    }

    #[test]
    fn rule6_status_counts_in_minutes_once_a_minute_has_passed() {
        STATUS_CLOCK.store(5_000, Ordering::SeqCst);
        let mut a = Approvals::in_memory();
        a.approve(entry(APPROVED))
            .expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(status_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        node.roster = vec![LinkRef::allstar("44444")];
        c.tick(&mut gw, &node);

        STATUS_CLOCK.store(5_125, Ordering::SeqCst);
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Linked to AllStar 44444 — dialled by u300, 2m ago. Nobody has the mic."
                    .to_string()
            )]
        );
    }

    // -- Rules 9–15: voice events ------------------------------------------

    #[test]
    fn rule9_an_unmuted_newcomer_is_muted_and_told_once() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        assert_eq!(gw.mutes, vec![(OTHER, true)]);
        assert_eq!(
            gw.announced,
            vec![
                "<@400> you're muted here — approved users can take the mic with `/mic`; \
                 otherwise `/register`."
                    .to_string()
            ]
        );

        // A second arrival is muted again but not announced again.
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        assert_eq!(gw.mutes, vec![(OTHER, true), (OTHER, true)]);
        assert_eq!(gw.announced.len(), 1, "the notice is once per user");
    }

    #[test]
    fn rule10_a_moderators_existing_mute_is_never_recorded() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), true);
        assert!(
            gw.mutes.is_empty(),
            "an already-muted arrival needs no mute, got {:?}",
            gw.mutes
        );
        assert!(
            gw.announced.is_empty(),
            "an already-muted arrival is not announced, got {:?}",
            gw.announced
        );
        // Not ours, so we must never lift it: moving to another channel
        // leaves it alone.
        voice(&mut c, &mut gw, OTHER, Some(ELSEWHERE), true);
        assert!(
            gw.mutes.is_empty(),
            "a mute we did not place must never be lifted, got {:?}",
            gw.mutes
        );
    }

    #[test]
    fn rule11_the_holders_own_unmute_echo_does_nothing() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(gw.mutes, vec![(APPROVED, false)]);

        // Our own unmute comes back as an event. It must change nothing —
        // otherwise rule 9 would re-mute the person we just unmuted.
        voice(&mut c, &mut gw, APPROVED, Some(CHANNEL), false);
        assert_eq!(gw.mutes, vec![(APPROVED, false)]);
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn rule11_a_moderator_muting_the_holder_takes_the_mic() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, APPROVED, Some(CHANNEL), true);
        assert_eq!(c.view().mic_holder, None, "the node yields to a moderator");
        assert!(
            gw.mutes.is_empty(),
            "we do not re-issue a mute we did not decide on, got {:?}",
            gw.mutes
        );
        assert!(
            gw.announced.is_empty(),
            "a moderator's mute is not our announcement to make, got {:?}",
            gw.announced
        );
    }

    #[test]
    fn rule12_our_own_mute_event_produces_no_further_ops() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        assert_eq!(gw.mutes, vec![(OTHER, true)]);

        // Discord echoes our mute back as `server_muted: true`, which
        // matches no rule — this is what stops the bot fighting itself.
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), true);
        assert_eq!(gw.mutes, vec![(OTHER, true)], "no self-loop");
        assert_eq!(gw.announced.len(), 1);
    }

    #[test]
    fn rule13_appearing_in_another_channel_lifts_our_mute() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        assert_eq!(gw.mutes, vec![(OTHER, true)]);

        voice(&mut c, &mut gw, OTHER, Some(ELSEWHERE), true);
        assert_eq!(gw.mutes, vec![(OTHER, true), (OTHER, false)]);
        assert!(
            c.view().present.is_empty(),
            "they are not in our channel any more"
        );

        // The mark is gone, so a second appearance elsewhere lifts nothing.
        voice(&mut c, &mut gw, OTHER, Some(ELSEWHERE), true);
        assert_eq!(gw.mutes, vec![(OTHER, true), (OTHER, false)]);
    }

    #[test]
    fn rule13_another_channel_does_not_unmute_when_the_mute_is_already_gone() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        let mut gw = RecordingOps::default();
        // A moderator lifted it before we saw them elsewhere.
        voice(&mut c, &mut gw, OTHER, Some(ELSEWHERE), false);
        assert!(
            gw.mutes.is_empty(),
            "no unmute is needed when it is already off, got {:?}",
            gw.mutes
        );
    }

    #[test]
    fn rule13_the_holder_moving_to_another_channel_clears_the_mic() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, APPROVED, Some(ELSEWHERE), false);
        assert_eq!(c.view().mic_holder, None);
        assert!(
            gw.mutes.is_empty(),
            "the holder is unmuted already, nothing to lift, got {:?}",
            gw.mutes
        );
    }

    #[test]
    fn rule14_disconnecting_keeps_the_mark_and_clears_the_mic() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        // Someone we muted, and separately the mic holder.
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false);
        join_muted(&mut c, APPROVED);
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, None, true);
        voice(&mut c, &mut gw, APPROVED, None, false);
        assert!(
            gw.mutes.is_empty(),
            "a bot cannot change the voice state of someone not in voice, got {:?}",
            gw.mutes
        );
        assert_eq!(c.view().mic_holder, None);
        assert!(c.view().present.is_empty());

        // The mark survived: coming back elsewhere still lifts it.
        voice(&mut c, &mut gw, OTHER, Some(ELSEWHERE), true);
        assert_eq!(gw.mutes, vec![(OTHER, false)]);
    }

    #[test]
    fn rule15_ready_rebuilds_presence_clears_the_mic_and_sweeps() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let mut gw = RecordingOps::default();
        c.handle(
            GatewayEvent::Ready {
                voice: vec![
                    VoiceEvent {
                        user: user(APPROVED),
                        channel: Some(CHANNEL),
                        server_muted: false,
                        is_bot: false,
                    },
                    VoiceEvent {
                        user: user(OTHER),
                        channel: Some(CHANNEL),
                        server_muted: true,
                        is_bot: false,
                    },
                    VoiceEvent {
                        user: user(THIRD),
                        channel: Some(ELSEWHERE),
                        server_muted: false,
                        is_bot: false,
                    },
                    VoiceEvent {
                        user: UserRef {
                            id: 999,
                            name: "shazbot".to_string(),
                        },
                        channel: Some(CHANNEL),
                        server_muted: false,
                        is_bot: true,
                    },
                ],
            },
            &mut gw,
            &mut node,
        );

        // Nobody holds the mic across a reconnect, the unmuted human in our
        // channel is re-muted, the already-muted one is left alone, the
        // member elsewhere is not present, and the bot is ignored entirely.
        assert_eq!(c.view().mic_holder, None);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        let present: Vec<String> = c.view().present.iter().map(|p| p.user_id.clone()).collect();
        assert_eq!(present, vec!["300".to_string(), "400".to_string()]);
    }

    #[test]
    fn rule16_mute_failed_posts_once_and_records_the_error() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        for _ in 0..3 {
            c.handle(
                GatewayEvent::MuteFailed {
                    user: OTHER,
                    reason: "Missing Permissions".to_string(),
                },
                &mut gw,
                &mut node,
            );
        }
        assert_eq!(gw.announced, vec![MUTE_PERMISSION.to_string()]);
        assert_eq!(
            c.view().last_error,
            Some("couldn't mute 400: Missing Permissions".to_string())
        );
    }

    #[test]
    fn a_replaced_gateway_clears_the_audio_fault_that_nothing_else_can() {
        // `AudioRecovered` is emitted by a JOIN that clears a recorded
        // failure — on the gateway that recorded it. A gateway that DIED is
        // replaced wholesale, and the new one's `TrackSupervisor` starts
        // with fresh `AudioHealth` and nothing to clear, so no
        // `AudioRecovered` is ever emitted again: without this, the fault
        // and the "already posted" flag stick for the life of the process,
        // `/status` shows a dead feed forever, and the NEXT real outage is
        // never announced.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.handle(
            GatewayEvent::AudioFailed {
                reason: "the audio track ended".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(gw.announced, vec![AUDIO_STOPPED.to_string()]);
        assert!(c.view().last_error.is_some());

        c.gateway_replaced();

        assert!(
            c.view().last_error.is_none(),
            "a gateway that no longer exists cannot still have a dead track: {:?}",
            c.view().last_error
        );
        // And the channel can be told about the next one.
        let mut gw = RecordingOps::default();
        c.handle(
            GatewayEvent::AudioFailed {
                reason: "the audio track ended".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(
            gw.announced,
            vec![AUDIO_STOPPED.to_string()],
            "the once-per-process flag is reset with the gateway it belonged to"
        );
    }

    #[test]
    fn an_audio_failure_is_announced_once_and_every_reason_reaches_last_error() {
        // The track feeding this channel died. The channel is told once,
        // in fixed words: the reason is songbird's and belongs in
        // `/status`, not in a public message, and a track that errors
        // fires BOTH `Error` and `End` — two events for one death — so a
        // per-event announcement would double every time.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        c.handle(
            GatewayEvent::AudioFailed {
                reason: "runtime error while playing track: parsing formats/codecs".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(gw.announced, vec![AUDIO_STOPPED.to_string()]);
        assert_eq!(
            c.view().last_error,
            Some(
                "the audio feed into Discord stopped: runtime error while playing track: \
                 parsing formats/codecs"
                    .to_string()
            )
        );

        c.handle(
            GatewayEvent::AudioFailed {
                reason: "the audio track ended".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(
            gw.announced,
            vec![AUDIO_STOPPED.to_string()],
            "the second failure must not announce again"
        );
        assert_eq!(
            c.view().last_error,
            Some("the audio feed into Discord stopped: the audio track ended".to_string()),
            "but the operator's view always carries the newest reason"
        );
    }

    #[test]
    fn the_announced_audio_failure_carries_no_reason_text() {
        // The line the channel sees is fixed. songbird's reason names
        // codecs, files and sometimes a host error; none of that belongs
        // in a room full of people, and the operator has `/status`.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.handle(
            GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(gw.announced.len(), 1);
        assert!(
            !gw.announced[0].contains("no compatible track found"),
            "the reason must not reach the channel: {}",
            gw.announced[0]
        );
    }

    #[test]
    fn the_audio_feed_stopping_and_coming_back_announces_each_transition() {
        // The exact vector: a node whose track dies, is rejoined, and dies
        // again must say so three times. Before recovery existed the
        // "already said it" flag was set for the life of the process, so
        // the SECOND outage was silent — the one case where a channel full
        // of people is hearing nothing and nobody has been told.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        c.handle(
            GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string(),
            },
            &mut gw,
            &mut node,
        );
        c.handle(GatewayEvent::AudioRecovered, &mut gw, &mut node);
        c.handle(
            GatewayEvent::AudioFailed {
                reason: "the audio track ended".to_string(),
            },
            &mut gw,
            &mut node,
        );

        assert_eq!(
            gw.announced,
            vec![
                AUDIO_STOPPED.to_string(),
                AUDIO_BACK.to_string(),
                AUDIO_STOPPED.to_string(),
            ]
        );
    }

    #[test]
    fn a_recovery_the_channel_was_never_warned_about_announces_nothing() {
        // Every successful join that cleared a failure emits this, and a
        // failure can be recorded without ever having been announced (the
        // announcement is once per outage, the flag is what tracks it).
        // Saying "the audio feed is back" to a channel that was never told
        // it stopped is noise.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        c.handle(GatewayEvent::AudioRecovered, &mut gw, &mut node);

        assert_eq!(gw.announced, Vec::<String>::new());
        assert_eq!(c.view().last_error, None);
    }

    #[test]
    fn a_recovery_clears_the_audio_error_and_nothing_else() {
        // `last_error` is one field on the wire and two facts underneath.
        // A mute Discord refused, or a state file that could not be
        // written, has nothing to do with the audio track and must survive
        // a rejoin that fixes the audio.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        c.handle(
            GatewayEvent::MuteFailed {
                user: OTHER,
                reason: "Missing Permissions".to_string(),
            },
            &mut gw,
            &mut node,
        );
        c.handle(
            GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert_eq!(
            c.view().last_error,
            Some("the audio feed into Discord stopped: no compatible track found".to_string()),
            "the audio fault wins while it stands: the channel is hearing nothing"
        );

        c.handle(GatewayEvent::AudioRecovered, &mut gw, &mut node);

        assert_eq!(
            c.view().last_error,
            Some("couldn't mute 400: Missing Permissions".to_string()),
            "the unrelated error must come back into view, not be wiped"
        );
        assert_eq!(
            gw.announced,
            vec![
                MUTE_PERMISSION.to_string(),
                AUDIO_STOPPED.to_string(),
                AUDIO_BACK.to_string(),
            ]
        );
    }

    #[test]
    fn a_recovery_leaves_no_error_at_all_when_audio_was_the_only_one() {
        // The ordinary case, and the one an operator looks at: `/status`
        // must stop reporting an outage that is over.
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();

        c.handle(
            GatewayEvent::AudioFailed {
                reason: "no compatible track found".to_string(),
            },
            &mut gw,
            &mut node,
        );
        assert!(c.view().last_error.is_some());

        c.handle(GatewayEvent::AudioRecovered, &mut gw, &mut node);

        assert_eq!(c.view().last_error, None);
    }

    // -- The admin page ----------------------------------------------------

    #[test]
    fn admin_page_announces_as_the_node_admin() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.admin(AdminAction::Approve(OTHER), &mut gw, &mut node)
            .expect("approving an unknown user is allowed");
        assert_eq!(
            gw.announced,
            vec!["<@400> was approved by the node admin.".to_string()]
        );
        assert!(
            gw.replies.is_empty(),
            "the page has no interaction to reply to, got {:?}",
            gw.replies
        );
        let approved = c.view().approved;
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].by, "admin");
    }

    #[test]
    fn admin_page_may_act_on_an_admin() {
        // The page is the root of trust, so it can do what no admin inside
        // Discord can.
        let mut c = commander_with(&[THIRD], &[THIRD], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.admin(AdminAction::RemoveAdmin(THIRD), &mut gw, &mut node)
            .expect("the page may demote an admin");
        c.admin(AdminAction::Revoke(THIRD), &mut gw, &mut node)
            .expect("the page may revoke");
        assert_eq!(
            gw.announced,
            vec![
                "<@500> is no longer an admin, removed by the node admin.".to_string(),
                "<@500> was revoked by the node admin.".to_string(),
            ]
        );
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
        assert_eq!(c.view().approved, [] as [crate::approvals::UserEntry; 0]);
    }

    /// The page's half of the demotion rule — same vectors, "the node
    /// admin" where a mention goes.
    #[test]
    fn admin_page_remove_admin_takes_the_mic_unless_they_are_also_approved() {
        let mut c = commander_with(&[], &[ADMIN], &[]);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        join_muted(&mut c, ADMIN);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        c.admin(AdminAction::RemoveAdmin(ADMIN), &mut gw, &mut node)
            .expect("the page may demote an admin");
        assert_eq!(gw.mutes, vec![(ADMIN, true)]);
        assert_eq!(
            gw.announced,
            vec![
                "<@200> is no longer an admin, removed by the node admin, and no longer has \
                 the mic."
                    .to_string()
            ]
        );
        assert_eq!(gw.panels, vec![None]);
        assert_eq!(c.view().mic_holder, None);

        // And the approved case: still allowed, so still holding.
        let mut c = commander_with(&[THIRD], &[THIRD], &[]);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        join_muted(&mut c, THIRD);
        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: true },
        );
        let mut gw = RecordingOps::default();
        c.admin(AdminAction::RemoveAdmin(THIRD), &mut gw, &mut node)
            .expect("the page may demote an admin");
        assert_eq!(gw.mutes, [] as [(u64, bool); 0]);
        assert_eq!(
            gw.announced,
            vec!["<@500> is no longer an admin, removed by the node admin.".to_string()]
        );
        assert_eq!(c.view().mic_holder, Some("500".to_string()));
    }

    #[test]
    fn admin_page_cannot_touch_an_approver() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        for action in [
            AdminAction::Ban(APPROVER),
            AdminAction::Revoke(APPROVER),
            AdminAction::RemoveAdmin(APPROVER),
        ] {
            let e = c
                .admin(action, &mut gw, &mut node)
                .expect_err("an approver is config, not state");
            assert_eq!(e, "100 is an approver in node.toml");
        }
        assert!(
            gw.announced.is_empty(),
            "a refused page action announces nothing, got {:?}",
            gw.announced
        );
    }

    #[test]
    fn admin_page_errors_on_unknown_state() {
        let mut c = commander_with(&[], &[], &[OTHER]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        // Nobody has ever named user 500 to this node, so the page falls
        // back to the bare snowflake rather than inventing a name.
        assert_eq!(
            c.admin(AdminAction::Unban(THIRD), &mut gw, &mut node),
            Err("500 is not banned".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::Revoke(THIRD), &mut gw, &mut node),
            Err("500 is not approved".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::RemoveAdmin(THIRD), &mut gw, &mut node),
            Err("500 is not an admin".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::ReleaseMic, &mut gw, &mut node),
            Err("nobody has the mic".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::Hangup, &mut gw, &mut node),
            Err("nothing is linked".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::Approve(OTHER), &mut gw, &mut node),
            Err("u400 is banned on this node; unban them first".to_string())
        );
        assert_eq!(
            c.admin(AdminAction::MakeAdmin(OTHER), &mut gw, &mut node),
            Err("u400 is banned on this node; unban them first".to_string())
        );
        assert!(
            gw.announced.is_empty(),
            "no refused page action announces, got {:?}",
            gw.announced
        );
    }

    #[test]
    fn admin_page_ban_takes_the_mic_and_disable_closes_it() {
        let mut c = commander_with(&[APPROVED, THIRD], &[], &[]);
        join_muted(&mut c, APPROVED);
        join_muted(&mut c, THIRD);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        c.admin(AdminAction::Ban(APPROVED), &mut gw, &mut node)
            .expect("the page may ban");
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.announced,
            vec!["<@300> was banned by the node admin and no longer has the mic.".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);

        // Now someone else takes it, and Disable closes it again.
        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: true },
        );
        let mut gw = RecordingOps::default();
        c.admin(AdminAction::Disable, &mut gw, &mut node)
            .expect("the page may disable");
        assert_eq!(gw.mutes, vec![(THIRD, true)]);
        assert_eq!(
            gw.announced,
            vec!["Discord commands were turned off by the node admin.".to_string()]
        );
        let v = c.view();
        assert!(!v.enabled);
        assert_eq!(v.mic_holder, None);
    }

    #[test]
    fn admin_page_release_mic_and_hangup() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        node.roster = vec![LinkRef::allstar("44444")];

        let mut gw = RecordingOps::default();
        c.admin(AdminAction::ReleaseMic, &mut gw, &mut node)
            .expect("the page may take the mic");
        c.admin(AdminAction::Hangup, &mut gw, &mut node)
            .expect("the page may hang up");
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(node.hung_up, vec![LinkRef::allstar("44444")]);
        assert_eq!(
            gw.announced,
            vec![
                "the node admin took the mic from <@300>".to_string(),
                "the node admin hung up AllStar 44444.".to_string(),
            ]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn admin_page_enable_turns_the_switch_back_on() {
        let mut a = Approvals::in_memory();
        a.set_enabled(false).expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.admin(AdminAction::Enable, &mut gw, &mut node)
            .expect("the page may enable");
        assert_eq!(
            gw.announced,
            vec!["Discord commands were turned on by the node admin.".to_string()]
        );
        assert!(c.view().enabled);
    }

    // -- A hand-edited state file ------------------------------------------

    /// Ids on both sides of the tier table, which no code path here can
    /// produce (`ban` strips approved/pending/admins, and an approver's ban
    /// is refused before it is recorded) — but a text editor can, and the
    /// file is documented as plain JSON with nothing secret in it.
    ///
    /// Built in the order that survives: `ban` first, then the grants,
    /// because `ban` is the mutator that does the stripping.
    fn hand_edited(banned: &[u64], then_admins: &[u64], then_approved: &[u64]) -> Approvals {
        let mut a = Approvals::in_memory();
        for id in banned {
            a.ban(entry(*id)).expect("an in-memory save cannot fail");
        }
        for id in then_admins {
            a.add_admin(entry(*id))
                .expect("an in-memory save cannot fail");
        }
        for id in then_approved {
            a.approve(entry(*id))
                .expect("an in-memory save cannot fail");
        }
        a
    }

    fn ids_of(list: &[UserEntry]) -> Vec<String> {
        list.iter().map(|e| e.user_id.clone()).collect()
    }

    #[test]
    fn a_banned_approver_in_the_state_file_is_unbanned_at_startup() {
        // An approver's id under `banned` makes `is_allowed` false while
        // they keep every approver power, so `/dial` is refused to the one
        // person `node.toml` says may always have it. node.toml wins: it is
        // the operator's own file and the state file is not.
        let c = Commander::new(hand_edited(&[APPROVER], &[], &[]), vec![APPROVER], CHANNEL);
        let v = c.view();
        assert!(
            v.banned.is_empty(),
            "the ban on an approver is dropped, not merely ignored: {:?}",
            ids_of(&v.banned)
        );
        assert_eq!(v.admins, [] as [crate::approvals::UserEntry; 0]);
        assert_eq!(v.approved, [] as [crate::approvals::UserEntry; 0]);
        assert_eq!(v.pending, [] as [crate::approvals::UserEntry; 0]);
    }

    #[test]
    fn a_banned_id_in_the_state_file_keeps_no_admin_or_approved_tier() {
        // The other shape: banned AND an admin AND approved. A ban is
        // unconditional — `Approvals::ban` strips both for exactly this
        // reason — so a hand edit that leaves them standing gives a banned
        // user working admin commands.
        let c = Commander::new(
            hand_edited(&[OTHER], &[OTHER], &[OTHER]),
            vec![APPROVER],
            CHANNEL,
        );
        let v = c.view();
        assert_eq!(
            ids_of(&v.banned),
            vec!["400".to_string()],
            "the ban stands: it is the tier that wins"
        );
        assert!(
            v.admins.is_empty(),
            "a banned id is nobody's admin: {:?}",
            ids_of(&v.admins)
        );
        assert!(
            v.approved.is_empty(),
            "and is not approved either: {:?}",
            ids_of(&v.approved)
        );
        assert_eq!(v.pending, [] as [crate::approvals::UserEntry; 0]);
    }

    #[test]
    fn a_consistent_state_file_is_left_exactly_as_it_was() {
        // The normalisation must be a no-op on every file the node itself
        // writes — including the ordinary case of an admin who is also
        // approved, which is not a contradiction at all.
        let c = Commander::new(
            hand_edited(&[OTHER], &[ADMIN], &[ADMIN, APPROVED]),
            vec![APPROVER],
            CHANNEL,
        );
        let v = c.view();
        assert_eq!(ids_of(&v.banned), vec!["400".to_string()]);
        assert_eq!(ids_of(&v.admins), vec!["200".to_string()]);
        assert_eq!(
            ids_of(&v.approved),
            vec!["200".to_string(), "300".to_string()]
        );
        assert!(
            v.last_error.is_none(),
            "nothing changed, so nothing was written and nothing could fail: {:?}",
            v.last_error
        );
    }

    // -- Persistence failure -----------------------------------------------

    /// An `Approvals` whose file lives in a directory that does not exist:
    /// `load` succeeds (a missing file is empty state) and every later save
    /// fails, which is exactly the shape of a state file on a full or
    /// read-only disk.
    fn unwritable_approvals() -> Approvals {
        let path = std::env::temp_dir()
            .join(format!(
                "astar-commander-{}-no-such-dir",
                std::process::id()
            ))
            .join("state.json");
        Approvals::load(&path).expect("a missing file loads as empty state")
    }

    #[test]
    fn a_failed_state_file_write_still_applies_and_says_so() {
        let mut c =
            Commander::new(unwritable_approvals(), vec![APPROVER], CHANNEL).with_clock(fixed_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, register("AJ7HR"));

        // Two writes failed (the callsign and the pending entry); the
        // caller is told once.
        assert_eq!(
            gw.replies,
            vec![(IT, format!("Request sent as AJ7HR.{WRITE_FAILED}"))]
        );
        // The action still stands, and the channel is still told.
        assert_eq!(
            gw.announced,
            vec![
                "<@400> (AJ7HR) is asking for dial access — an approver can `/approve` them."
                    .to_string()
            ]
        );
        assert!(c.view().pending.iter().any(|e| e.user_id == "400"));
        let err = c
            .view()
            .last_error
            .expect("a failed write must be recorded as the last error");
        assert!(
            err.starts_with("could not save the approvals file:"),
            "unexpected last_error: {err}"
        );
    }

    /// The same unwritable state file, pre-loaded with a decision or two.
    /// Each setup call fails its own save and is applied in memory anyway,
    /// which is exactly the situation a node on a full disk is already in
    /// by the time someone types `/revoke`.
    fn unwritable_commander(approved: &[u64], admins: &[u64], banned: &[u64]) -> Commander {
        let mut a = unwritable_approvals();
        for id in approved {
            let _ = a.approve(entry(*id));
        }
        for id in admins {
            let _ = a.add_admin(entry(*id));
        }
        for id in banned {
            let _ = a.ban(entry(*id));
        }
        Commander::new(a, vec![APPROVER], CHANNEL).with_clock(fixed_clock)
    }

    #[test]
    fn a_failed_write_on_revoke_still_takes_the_mic_and_says_so() {
        // `Approvals::revoke` removes first and saves second, so `Err` means
        // the removal HAPPENED. Reading it as "they were never approved"
        // would leave a revoked user holding an open mic.
        let mut c = unwritable_commander(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::Revoke {
                target: user(APPROVED),
            },
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.replies,
            vec![(IT, format!("Revoked u300.{WRITE_FAILED}"))]
        );
        assert_eq!(
            gw.announced,
            vec!["<@300> was revoked by <@100> and no longer has the mic.".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);
        assert!(
            c.view().approved.is_empty(),
            "the in-memory revoke stands even though the save failed"
        );
        assert!(c.view().last_error.is_some());
    }

    #[test]
    fn a_failed_write_on_unban_still_lifts_the_ban() {
        let mut c = unwritable_commander(&[], &[ADMIN], &[OTHER]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Unban {
                target: user(OTHER),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, format!("Unbanned u400.{WRITE_FAILED}"))]
        );
        assert_eq!(
            gw.announced,
            vec!["<@400> was unbanned by <@200>.".to_string()]
        );
        assert_eq!(c.view().banned, [] as [crate::approvals::UserEntry; 0]);
        assert!(c.view().last_error.is_some());
    }

    #[test]
    fn a_failed_write_on_admin_remove_still_demotes() {
        let mut c = unwritable_commander(&[], &[ADMIN], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(ADMIN),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, format!("u200 is no longer an admin.{WRITE_FAILED}"))]
        );
        assert_eq!(
            gw.announced,
            vec!["<@200> is no longer an admin, removed by <@100>.".to_string()]
        );
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
        assert!(c.view().last_error.is_some());
    }

    #[test]
    fn a_failed_write_on_admin_remove_still_takes_the_mic_and_says_so() {
        // `Approvals::remove_admin` removes first and saves second, so `Err`
        // means the demotion HAPPENED. Reading it as "they were never an
        // admin" would leave somebody with no tier at all holding an open
        // mic on a node whose disk is full.
        let mut c = unwritable_commander(&[], &[ADMIN], &[]);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        join_muted(&mut c, ADMIN);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            ADMIN,
            BotCommandKind::Mic { on: true },
        );
        assert_eq!(c.view().mic_holder, Some("200".to_string()));

        let mut gw = RecordingOps::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVER,
            BotCommandKind::AdminRemove {
                target: user(ADMIN),
            },
        );
        assert_eq!(
            gw.replies,
            vec![(IT, format!("u200 is no longer an admin.{WRITE_FAILED}"))],
            "the suffix is said once, not once per failed save"
        );
        assert_eq!(gw.mutes, vec![(ADMIN, true)]);
        assert_eq!(
            gw.announced,
            vec![
                "<@200> is no longer an admin, removed by <@100>, and no longer has the mic."
                    .to_string()
            ]
        );
        assert_eq!(c.view().mic_holder, None);
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
        assert!(c.view().last_error.is_some());
    }

    #[test]
    fn admin_page_revoke_survives_a_failed_write() {
        // A write that failed is not a refusal: the page must be told the
        // action happened, with `last_error` carrying the bad news.
        let mut c = unwritable_commander(&[APPROVED], &[], &[]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let mut gw = RecordingOps::default();
        assert_eq!(
            c.admin(AdminAction::Revoke(APPROVED), &mut gw, &mut node),
            Ok(())
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(
            gw.announced,
            vec!["<@300> was revoked by the node admin and no longer has the mic.".to_string()]
        );
        assert_eq!(c.view().mic_holder, None);
        assert!(c.view().last_error.is_some());
    }

    #[test]
    fn admin_page_unban_and_remove_admin_survive_a_failed_write() {
        let mut c = unwritable_commander(&[], &[ADMIN], &[OTHER]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        assert_eq!(
            c.admin(AdminAction::Unban(OTHER), &mut gw, &mut node),
            Ok(())
        );
        assert_eq!(
            c.admin(AdminAction::RemoveAdmin(ADMIN), &mut gw, &mut node),
            Ok(())
        );
        assert_eq!(
            gw.announced,
            vec![
                "<@400> was unbanned by the node admin.".to_string(),
                "<@200> is no longer an admin, removed by the node admin.".to_string(),
            ]
        );
        assert_eq!(c.view().banned, [] as [crate::approvals::UserEntry; 0]);
        assert_eq!(c.view().admins, [] as [crate::approvals::UserEntry; 0]);
        assert!(c.view().last_error.is_some());
    }

    // -- tick ---------------------------------------------------------------

    #[test]
    fn tick_announces_connected_once() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));

        let mut gw = RecordingOps::default();
        c.tick(&mut gw, &node);
        assert!(
            gw.announced.is_empty(),
            "nothing is up yet, got {:?}",
            gw.announced
        );

        node.roster = vec![LinkRef::allstar("44444")];
        c.tick(&mut gw, &node);
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec!["Connected to AllStar 44444.".to_string()],
            "exactly one Connected line"
        );
        assert_eq!(c.view().link.map(|l| l.up), Some(true));
    }

    #[test]
    fn tick_announces_the_end_of_a_link_that_was_up() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
        node.roster = vec![LinkRef::allstar("44444")];

        let mut gw = RecordingOps::default();
        c.tick(&mut gw, &node);
        node.roster.clear();
        c.tick(&mut gw, &node);
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec![
                "Connected to AllStar 44444.".to_string(),
                "The link to AllStar 44444 has ended.".to_string(),
            ]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn tick_gives_up_after_thirty_seconds_with_no_answer() {
        NO_ANSWER_CLOCK.store(1_000, Ordering::SeqCst);
        let mut a = Approvals::in_memory();
        a.approve(entry(APPROVED))
            .expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(no_answer_clock);
        let mut gw = RecordingOps::default();
        let mut node = TestNode {
            joined: true,
            ..TestNode::default()
        };
        run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));

        let mut gw = RecordingOps::default();
        NO_ANSWER_CLOCK.store(1_029, Ordering::SeqCst);
        c.tick(&mut gw, &node);
        assert!(
            gw.announced.is_empty(),
            "29 seconds is not yet a timeout, got {:?}",
            gw.announced
        );
        assert!(c.view().link.is_some());

        NO_ANSWER_CLOCK.store(1_030, Ordering::SeqCst);
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec!["AllStar 44444 did not answer.".to_string()]
        );
        assert!(c.view().link.is_none());

        // And the giving-up is once: there is nothing left to give up on.
        c.tick(&mut gw, &node);
        assert_eq!(gw.announced.len(), 1);
    }

    // -- Task 2b: the PTT toggle, the time-out timer, the silence unkey ----

    /// The timers need a clock that moves, and a `fn() -> u64` cannot
    /// capture, so each test that moves time owns its own static. These are
    /// handed out one per test; sharing one would race under the default
    /// parallel harness.
    macro_rules! test_clock {
        ($name:ident, $fn_name:ident) => {
            static $name: AtomicU64 = AtomicU64::new(0);
            fn $fn_name() -> u64 {
                $name.load(Ordering::SeqCst)
            }
        };
    }

    test_clock!(CLOCK_HARD, clock_hard);
    test_clock!(CLOCK_HARD_OFF, clock_hard_off);
    test_clock!(CLOCK_HANDOVER, clock_handover);
    test_clock!(CLOCK_SECOND_TICK, clock_second_tick);
    test_clock!(CLOCK_NEVER_TALKS, clock_never_talks);
    test_clock!(CLOCK_TALKING, clock_talking);
    test_clock!(CLOCK_EDGE, clock_edge);
    test_clock!(CLOCK_CANCEL, clock_cancel);
    test_clock!(CLOCK_SILENCE_OFF, clock_silence_off);
    test_clock!(CLOCK_FAILSAFE, clock_failsafe);
    test_clock!(CLOCK_BOTH_OFF, clock_both_off);
    test_clock!(CLOCK_TIE, clock_tie);
    test_clock!(CLOCK_PANEL, clock_panel);

    /// A commander with `APPROVED` and `THIRD` approved and both already in
    /// the channel (joined muted, so nothing is recorded), on the given
    /// clock, which starts at 0.
    fn mic_commander(clock: &'static AtomicU64, now: fn() -> u64) -> Commander {
        clock.store(0, Ordering::SeqCst);
        let mut a = Approvals::in_memory();
        for id in [APPROVED, THIRD] {
            a.approve(entry(id)).expect("an in-memory save cannot fail");
        }
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(now);
        join_muted(&mut c, APPROVED);
        join_muted(&mut c, THIRD);
        c
    }

    /// Keys the mic for `who`, discarding the recorder: every timer test
    /// starts from a keyed mic and asserts only on what happens next.
    fn key_up(c: &mut Commander, who: u64) {
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(c, &mut gw, &mut node, who, BotCommandKind::Mic { on: true });
        assert_eq!(
            c.view().mic_holder,
            Some(who.to_string()),
            "setup: {who} should hold the mic"
        );
    }

    fn talking(c: &mut Commander, gw: &mut RecordingOps, user: u64, talking: bool) {
        c.handle(
            GatewayEvent::Talking { user, talking },
            gw,
            &mut TestNode::default(),
        );
    }

    /// Ticks at `at`, returning what the commander did.
    fn tick_at(c: &mut Commander, clock: &'static AtomicU64, at: u64) -> RecordingOps {
        clock.store(at, Ordering::SeqCst);
        let mut gw = RecordingOps::default();
        c.tick(&mut gw, &TestNode::default());
        gw
    }

    // -- the toggle --------------------------------------------------------

    #[test]
    fn toggle_by_the_holder_releases_the_mic() {
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::MicToggle,
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.replies, vec![(IT, "Mic released.".to_string())]);
        assert_eq!(gw.announced, vec!["<@300> released the mic".to_string()]);
        assert_eq!(gw.panels, vec![None]);
        assert_eq!(c.view().mic_holder, None);
    }

    #[test]
    fn toggle_by_another_approved_user_moves_the_mic() {
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, THIRD, BotCommandKind::MicToggle);
        assert_eq!(gw.mutes, vec![(THIRD, false), (APPROVED, true)]);
        assert_eq!(
            gw.announced,
            vec!["<@500> has the mic (was <@300>)".to_string()]
        );
        assert_eq!(gw.panels, vec![Some("u500".to_string())]);
        assert_eq!(c.view().mic_holder, Some("500".to_string()));
    }

    #[test]
    fn toggle_by_an_unapproved_user_refuses_exactly_as_mic_does() {
        let mut c = commander();
        join_muted(&mut c, OTHER);
        let before = fingerprint(&c);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::MicToggle);
        refused(&c, &gw, &node, &before, NOT_APPROVED_MIC);
        assert!(
            gw.panels.is_empty(),
            "a refused toggle must not redraw the panel, got {:?}",
            gw.panels
        );
    }

    // -- the hard time-out timer --------------------------------------------

    #[test]
    fn the_hard_timer_fires_at_the_timeout_and_not_before() {
        let mut c = mic_commander(&CLOCK_HARD, clock_hard);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_HARD, 179);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "179 s is not yet the timeout, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let gw = tick_at(&mut c, &CLOCK_HARD, 180);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.announced, vec!["⏱ <@300> timed out".to_string()]);
        assert_eq!(gw.panels, vec![None]);
        assert_eq!(c.view().mic_holder, None);
    }

    #[test]
    fn the_hard_timer_does_not_fire_when_it_is_off() {
        let mut c = mic_commander(&CLOCK_HARD_OFF, clock_hard_off);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_HARD_OFF, 100_000);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "a timeout of 0 never fires, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn a_handover_restarts_both_clocks_for_the_new_holder() {
        let mut c = mic_commander(&CLOCK_HANDOVER, clock_handover);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        CLOCK_HANDOVER.store(100, Ordering::SeqCst);
        key_up(&mut c, THIRD);

        // The old holder's deadline, which must mean nothing now.
        let gw = tick_at(&mut c, &CLOCK_HANDOVER, 180);
        assert!(
            gw.announced.is_empty(),
            "the new holder keyed at 100 s and is not due at 180 s, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("500".to_string()));

        let gw = tick_at(&mut c, &CLOCK_HANDOVER, 279);
        assert!(gw.announced.is_empty(), "not due until 280 s, got {gw:?}");

        let gw = tick_at(&mut c, &CLOCK_HANDOVER, 280);
        assert_eq!(gw.mutes, vec![(THIRD, true)]);
        assert_eq!(gw.announced, vec!["⏱ <@500> timed out".to_string()]);
        assert_eq!(gw.panels, vec![None]);
    }

    #[test]
    fn a_second_tick_after_a_timer_release_does_nothing() {
        let mut c = mic_commander(&CLOCK_SECOND_TICK, clock_second_tick);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_SECOND_TICK, 200);
        assert_eq!(gw.panels, vec![None], "released once");

        let gw = tick_at(&mut c, &CLOCK_SECOND_TICK, 400);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "nothing is left to release, got {gw:?}"
        );
    }

    // -- the silence unkey ---------------------------------------------------

    #[test]
    fn a_holder_who_never_talks_unkeys_after_the_silence_window() {
        // The clock starts at key-up, so keying and saying nothing unkeys
        // too — that is the whole "it feels like PTT" point.
        let mut c = mic_commander(&CLOCK_NEVER_TALKS, clock_never_talks);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_NEVER_TALKS, 4);
        assert!(
            gw.announced.is_empty(),
            "4 s is inside the 5 s window, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let gw = tick_at(&mut c, &CLOCK_NEVER_TALKS, 5);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.announced, vec!["<@300> unkeyed (silence)".to_string()]);
        assert_eq!(gw.panels, vec![None]);
        assert_eq!(c.view().mic_holder, None);
    }

    #[test]
    fn a_talking_holder_is_not_unkeyed_by_the_silence_window() {
        let mut c = mic_commander(&CLOCK_TALKING, clock_talking);
        c.set_mic_timeout(0);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        talking(&mut c, &mut gw, APPROVED, true);
        assert!(
            gw.mutes.is_empty() && gw.panels.is_empty(),
            "a talk event is not itself an op, got {gw:?}"
        );
        assert!(c.view().talking, "the page shows them talking");

        let gw = tick_at(&mut c, &CLOCK_TALKING, 60);
        assert!(
            gw.announced.is_empty(),
            "someone who is talking is not silent, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn silence_is_measured_from_the_silent_edge_not_from_key_up() {
        let mut c = mic_commander(&CLOCK_EDGE, clock_edge);
        c.set_mic_timeout(0);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        CLOCK_EDGE.store(10, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, true);
        CLOCK_EDGE.store(30, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, false);
        assert!(!c.view().talking);

        // 5 s after key-up, and 5 s after the talking edge, are both long
        // past — only the SILENT edge counts.
        let gw = tick_at(&mut c, &CLOCK_EDGE, 34);
        assert!(gw.announced.is_empty(), "not yet 5 s silent, got {gw:?}");
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let gw = tick_at(&mut c, &CLOCK_EDGE, 35);
        assert_eq!(gw.announced, vec!["<@300> unkeyed (silence)".to_string()]);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
    }

    #[test]
    fn talking_again_inside_the_window_cancels_the_unkey() {
        let mut c = mic_commander(&CLOCK_CANCEL, clock_cancel);
        c.set_mic_timeout(0);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        CLOCK_CANCEL.store(3, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, true);

        let gw = tick_at(&mut c, &CLOCK_CANCEL, 100);
        assert!(gw.announced.is_empty(), "still talking, got {gw:?}");
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn a_talk_event_for_anyone_but_the_holder_changes_nothing() {
        let mut c = mic_commander(&CLOCK_TALKING, clock_talking);
        c.set_mic_timeout(0);
        key_up(&mut c, APPROVED);
        let before = fingerprint(&c);

        let mut gw = RecordingOps::default();
        talking(&mut c, &mut gw, THIRD, true);
        talking(&mut c, &mut gw, THIRD, false);
        talking(&mut c, &mut gw, OTHER, true);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "a bystander's packets are none of our business, got {gw:?}"
        );
        assert_eq!(fingerprint(&c), before);
        assert!(
            !c.view().talking,
            "the holder is not talking just because someone else is"
        );
    }

    #[test]
    fn a_talk_event_with_nobody_holding_the_mic_changes_nothing() {
        let mut c = mic_commander(&CLOCK_TALKING, clock_talking);
        let before = fingerprint(&c);

        let mut gw = RecordingOps::default();
        talking(&mut c, &mut gw, APPROVED, true);
        talking(&mut c, &mut gw, APPROVED, false);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "nobody is keyed, got {gw:?}"
        );
        assert_eq!(fingerprint(&c), before);
        assert!(!c.view().talking);
    }

    #[test]
    fn silence_off_still_leaves_the_hard_timer() {
        let mut c = mic_commander(&CLOCK_SILENCE_OFF, clock_silence_off);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_SILENCE_OFF, 100);
        assert!(
            gw.announced.is_empty(),
            "silence is off, so 100 s of it means nothing, got {gw:?}"
        );

        let gw = tick_at(&mut c, &CLOCK_SILENCE_OFF, 180);
        assert_eq!(gw.announced, vec!["⏱ <@300> timed out".to_string()]);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
    }

    #[test]
    fn the_hard_timer_backstops_a_holder_whose_talk_events_stopped() {
        // THE FAIL-SAFE. Receive is the reverse-engineered half of Discord
        // voice: a holder marked talking whose events then stop must still
        // be unkeyed, and nothing in the silence logic may extend the hard
        // timer by so much as a second.
        let mut c = mic_commander(&CLOCK_FAILSAFE, clock_failsafe);
        key_up(&mut c, APPROVED);

        // Normal speech is bursts, so the silence clock keeps being pushed
        // forward — here all the way to 100 s, long after key-up. The hard
        // timer must not move with it: it counts from key-up and from
        // nothing else.
        let mut gw = RecordingOps::default();
        CLOCK_FAILSAFE.store(1, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, true);
        CLOCK_FAILSAFE.store(100, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, false);
        CLOCK_FAILSAFE.store(101, Ordering::SeqCst);
        talking(&mut c, &mut gw, APPROVED, true);
        // ... and now the gateway's receive half dies. No further events,
        // so `talking` stays true for ever and the silence rule can never
        // fire again. Only the hard timer is left.
        assert!(c.view().talking, "setup: stuck marked as talking");

        let gw = tick_at(&mut c, &CLOCK_FAILSAFE, 179);
        assert!(
            gw.announced.is_empty(),
            "the hard timer is not early either, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));

        let gw = tick_at(&mut c, &CLOCK_FAILSAFE, 180);
        assert_eq!(
            gw.announced,
            vec!["⏱ <@300> timed out".to_string()],
            "a stuck talking flag must not hold the mic open"
        );
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.panels, vec![None]);
        assert_eq!(c.view().mic_holder, None);
    }

    #[test]
    fn both_timers_off_means_nothing_ever_fires() {
        let mut c = mic_commander(&CLOCK_BOTH_OFF, clock_both_off);
        c.set_mic_timeout(0);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_BOTH_OFF, 1_000_000);
        assert!(
            gw.mutes.is_empty() && gw.announced.is_empty() && gw.panels.is_empty(),
            "both timers are off, got {gw:?}"
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn the_hard_timer_wins_a_tie_with_the_silence_timer() {
        // Both are due on the same tick. The channel is told the truth
        // about which rule fired, and the holder is released exactly once.
        let mut c = mic_commander(&CLOCK_TIE, clock_tie);
        c.set_mic_timeout(10);
        c.set_mic_silence(10);
        key_up(&mut c, APPROVED);

        let gw = tick_at(&mut c, &CLOCK_TIE, 10);
        assert_eq!(gw.announced, vec!["⏱ <@300> timed out".to_string()]);
        assert_eq!(gw.mutes, vec![(APPROVED, true)]);
        assert_eq!(gw.panels, vec![None]);
    }

    // -- the panel ----------------------------------------------------------

    #[test]
    fn panel_follows_the_mic_through_take_handover_and_release() {
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: true },
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: false },
        );
        assert_eq!(
            gw.panels,
            vec![Some("u300".to_string()), Some("u500".to_string()), None]
        );
    }

    #[test]
    fn panel_is_not_redrawn_when_the_holder_did_not_change() {
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        key_up(&mut c, APPROVED);

        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        // The holder asking again, someone unapproved asking, and someone
        // approved but not in the channel asking: three refusals, no redraw.
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            OTHER,
            BotCommandKind::Mic { on: true },
        );
        run(
            &mut c,
            &mut gw,
            &mut node,
            THIRD,
            BotCommandKind::Mic { on: false },
        );
        assert_eq!(
            gw.replies,
            vec![
                (IT, ALREADY_HAVE_MIC.to_string()),
                (IT, NOT_APPROVED_MIC.to_string()),
                (IT, NOT_THE_HOLDER.to_string()),
            ]
        );
        assert!(
            gw.panels.is_empty(),
            "nothing changed hands, got {:?}",
            gw.panels
        );
        assert_eq!(c.view().mic_holder, Some("300".to_string()));
    }

    #[test]
    fn panel_is_cleared_when_an_admin_or_the_page_takes_the_mic() {
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        key_up(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(&mut c, &mut gw, &mut node, APPROVER, BotCommandKind::Unmic);
        assert_eq!(gw.panels, vec![None]);

        key_up(&mut c, THIRD);
        let mut gw = RecordingOps::default();
        c.admin(AdminAction::ReleaseMic, &mut gw, &mut node)
            .expect("the page may take the mic");
        assert_eq!(gw.panels, vec![None]);
    }

    #[test]
    fn panel_is_cleared_when_the_holder_is_revoked_banned_or_the_switch_goes_off() {
        for kind in [
            BotCommandKind::Revoke {
                target: user(APPROVED),
            },
            BotCommandKind::Ban {
                target: user(APPROVED),
            },
            BotCommandKind::Bot { on: false },
        ] {
            let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
            key_up(&mut c, APPROVED);
            let mut gw = RecordingOps::default();
            let mut node = TestNode::default();
            run(&mut c, &mut gw, &mut node, APPROVER, kind);
            assert_eq!(gw.panels, vec![None]);
            assert_eq!(c.view().mic_holder, None);
        }
    }

    #[test]
    fn panel_is_cleared_when_the_holder_leaves_or_a_moderator_mutes_them() {
        // Three more paths into the same place: another channel, gone from
        // voice entirely, and a moderator's Server Mute (rule 11).
        for (channel, muted) in [
            (Some(ELSEWHERE), false),
            (None, false),
            (Some(CHANNEL), true),
        ] {
            let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
            key_up(&mut c, APPROVED);
            let mut gw = RecordingOps::default();
            voice(&mut c, &mut gw, APPROVED, channel, muted);
            assert_eq!(
                gw.panels,
                vec![None],
                "channel {channel:?} muted {muted} should clear the panel"
            );
            assert_eq!(c.view().mic_holder, None);
        }
    }

    #[test]
    fn ready_while_someone_holds_the_mic_draws_the_idle_panel_exactly_once() {
        // The holder DID change — `set_holder(None)` clears it and redraws
        // — so `Ready` must not then post a second, identical panel. This
        // is the half that catches an unconditional `gw.panel(None)` bolted
        // on next to `set_holder`: that would give `[None, None]`, two REST
        // edits for one event.
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        key_up(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.handle(GatewayEvent::Ready { voice: vec![] }, &mut gw, &mut node);
        assert_eq!(gw.panels, vec![None]);
    }

    #[test]
    fn ready_with_nobody_holding_the_mic_still_posts_the_idle_panel() {
        // The gap this closes: on a FRESH start nobody holds the mic, so
        // `set_holder(None)` is a no-op and nothing else ever called
        // `panel()` — the button message was never posted at all, and the
        // only way to make one appear was for somebody to key and unkey the
        // mic with a slash command first. `Ready` is the one event that is
        // guaranteed to happen, so it is where the panel is owed.
        //
        // Catches the old `set_holder(None, gw)`-and-nothing-else path,
        // which leaves `panels` empty here.
        let mut c = mic_commander(&CLOCK_PANEL, clock_panel);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.handle(GatewayEvent::Ready { voice: vec![] }, &mut gw, &mut node);
        assert_eq!(gw.panels, vec![None]);

        // And a second Ready posts exactly one more: a reconnect re-posts
        // the panel rather than assuming the old message survived, but it
        // does not post two.
        let mut gw = RecordingOps::default();
        c.handle(GatewayEvent::Ready { voice: vec![] }, &mut gw, &mut node);
        assert_eq!(gw.panels, vec![None]);
    }

    #[test]
    fn view_reports_the_timer_settings_and_whether_the_holder_is_talking() {
        let mut c = mic_commander(&CLOCK_TALKING, clock_talking);
        let v = c.view();
        assert_eq!(v.mic_timeout_secs, 180, "the repeater-style default");
        assert_eq!(v.mic_silence_secs, 5, "the PTT-feel default");
        assert!(!v.talking, "nobody is keyed");

        c.set_mic_timeout(90);
        c.set_mic_silence(0);
        key_up(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        talking(&mut c, &mut gw, APPROVED, true);
        let v = c.view();
        assert_eq!(v.mic_timeout_secs, 90);
        assert_eq!(v.mic_silence_secs, 0);
        assert!(v.talking, "the holder is sending packets");

        talking(&mut c, &mut gw, APPROVED, false);
        assert!(!c.view().talking, "and now they are not");
    }

    // -- shutdown and view --------------------------------------------------

    #[test]
    fn shutdown_unmutes_only_those_we_muted() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        voice(&mut c, &mut gw, OTHER, Some(CHANNEL), false); // ours
        voice(&mut c, &mut gw, THIRD, Some(CHANNEL), true); // a moderator's

        let mut gw = RecordingOps::default();
        c.shutdown(&mut gw);
        assert_eq!(
            gw.mutes,
            vec![(OTHER, false)],
            "only the mute we placed is lifted"
        );
    }

    #[test]
    fn view_reports_every_id_as_a_string() {
        // JavaScript cannot hold a snowflake as a number, so nothing in the
        // view may be a u64.
        let mut c = commander_with(&[APPROVED], &[ADMIN], &[OTHER]);
        join_muted(&mut c, APPROVED);
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: true },
        );

        let v = c.view();
        assert_eq!(v.approvers, vec!["100".to_string()]);
        assert_eq!(v.mic_holder, Some("300".to_string()));
        assert_eq!(
            v.admins
                .iter()
                .map(|e| e.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["200"]
        );
        assert_eq!(
            v.banned
                .iter()
                .map(|e| e.user_id.as_str())
                .collect::<Vec<_>>(),
            vec!["400"]
        );
        assert_eq!(v.present.len(), 1);
        assert_eq!(v.present[0].user_id, "300");
        assert_eq!(v.present[0].name, "u300");
        // The flag tracks what we did, not just what Discord last told us:
        // they joined muted, and taking the mic unmuted them, so the page
        // must already say so rather than waiting for the echo.
        assert!(
            !v.present[0].server_muted,
            "the mic holder is not muted any more"
        );

        // And back the other way, so neither value is fixed by construction.
        run(
            &mut c,
            &mut gw,
            &mut node,
            APPROVED,
            BotCommandKind::Mic { on: false },
        );
        assert!(
            c.view().present[0].server_muted,
            "giving the mic up mutes them again"
        );
    }

    #[test]
    fn the_reply_goes_back_to_the_interaction_it_came_from() {
        let mut c = commander();
        let mut gw = RecordingOps::default();
        let mut node = TestNode::default();
        c.handle(
            GatewayEvent::Command(BotCommand {
                user: user(OTHER),
                kind: BotCommandKind::Status,
                interaction: 42,
            }),
            &mut gw,
            &mut node,
        );
        assert_eq!(
            gw.replies,
            vec![(42, "Not linked. Nobody has the mic.".to_string())]
        );
    }

    // -- Networks and link names --------------------------------------------

    #[test]
    fn a_network_has_a_display_name_and_a_wire_name() {
        assert_eq!(Network::Allstar.to_string(), "AllStar");
        assert_eq!(Network::M17.to_string(), "M17");
        assert_eq!(Network::Dstar.to_string(), "D-Star");
        assert_eq!(
            [Network::Allstar, Network::M17, Network::Dstar].map(Network::as_str),
            ["allstar", "m17", "dstar"]
        );
        assert_eq!(
            serde_json::to_string(&[Network::Allstar, Network::M17, Network::Dstar])
                .expect("a Network serialises"),
            r#"["allstar","m17","dstar"]"#,
            "serde speaks the wire name, which is what the pages already read"
        );
    }

    #[test]
    fn a_link_ref_reads_as_its_network_then_its_target() {
        assert_eq!(LinkRef::allstar("543260").to_string(), "AllStar 543260");
        assert_eq!(LinkRef::m17("M17-USA", 'C').to_string(), "M17 M17-USA C");
        assert_eq!(
            LinkRef::m17("M17-USA", 'C'),
            LinkRef {
                network: Network::M17,
                target: "M17-USA C".to_string(),
            }
        );
    }

    #[test]
    fn link_labels_group_by_network_in_first_seen_order() {
        assert_eq!(link_labels(&[]), "");
        assert_eq!(
            link_labels(&[LinkRef::allstar("1"), LinkRef::allstar("2")]),
            "AllStar 1, 2",
            "two AllStar links read exactly as the old joined roster did"
        );
        assert_eq!(
            link_labels(&[LinkRef::allstar("543260"), LinkRef::m17("M17-USA", 'C')]),
            "AllStar 543260, M17 M17-USA C"
        );
        assert_eq!(
            link_labels(&[
                LinkRef::allstar("1"),
                LinkRef::m17("M17-USA", 'C'),
                LinkRef::allstar("2"),
            ]),
            "AllStar 1, 2, M17 M17-USA C",
            "a network's targets stay together, in the order the network first appeared"
        );
        assert_eq!(
            link_labels(&[LinkRef::m17("M17-USA", 'C'), LinkRef::allstar("1")]),
            "M17 M17-USA C, AllStar 1"
        );
    }

    #[test]
    fn link_info_serialises_the_wire_network_and_never_the_dialers_id() {
        let allstar = LinkInfo {
            network: Network::Allstar,
            target: "44444".to_string(),
            by: "u300".to_string(),
            by_id: Some(APPROVED),
            callsign: None,
            since_unix: 1_000,
            up: false,
        };
        assert_eq!(
            serde_json::to_string(&allstar).expect("a LinkInfo serialises"),
            r#"{"network":"allstar","target":"44444","by":"u300","since_unix":1000,"up":false}"#,
            "an AllStar link's admin-page JSON is exactly what it was before"
        );
        let m17 = LinkInfo {
            network: Network::M17,
            target: "M17-USA C".to_string(),
            by: "u300".to_string(),
            by_id: Some(APPROVED),
            callsign: Some("AJ7HR".to_string()),
            since_unix: 1_000,
            up: true,
        };
        assert_eq!(
            serde_json::to_string(&m17).expect("a LinkInfo serialises"),
            r#"{"network":"m17","target":"M17-USA C","by":"u300","callsign":"AJ7HR","since_unix":1000,"up":true}"#
        );
    }

    // -- M17: tick and status ---------------------------------------------

    test_clock!(CLOCK_M17_NO_ANSWER, clock_m17_no_answer);

    /// `APPROVED`, holding callsign AJ7HR, has dialled M17-USA C on a
    /// commander running on `now`. The node reports nothing yet.
    fn m17_dialled(now: fn() -> u64) -> (Commander, TestNode) {
        let mut a = Approvals::in_memory();
        a.approve(entry(APPROVED))
            .expect("an in-memory save cannot fail");
        let mut c = Commander::new(a, vec![APPROVER], CHANNEL).with_clock(now);
        let mut node = joined();
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut node,
            APPROVED,
            register("AJ7HR"),
        );
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut node,
            APPROVED,
            dial_m17("M17-USA", 'C'),
        );
        assert_eq!(
            node.dialled_m17.len(),
            1,
            "the fixture's dial reached the node"
        );
        (c, node)
    }

    #[test]
    fn tick_announces_an_m17_leg_connected_once_and_its_plain_end() {
        let (mut c, mut node) = m17_dialled(fixed_clock);
        let mut gw = RecordingOps::default();
        c.tick(&mut gw, &node);
        assert!(
            gw.announced.is_empty(),
            "linking is not linked, got {:?}",
            gw.announced
        );
        node.roster = vec![LinkRef::m17("M17-USA", 'C')];
        c.tick(&mut gw, &node);
        c.tick(&mut gw, &node);
        node.roster.clear();
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec![
                "Connected to M17 M17-USA C.".to_string(),
                "The link to M17 M17-USA C has ended.".to_string(),
            ]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn tick_ends_an_m17_leg_with_the_nodes_reason_at_once() {
        // Refused before it ever came up: said now, with the reason, not
        // thirty seconds later as "did not answer".
        let (mut c, mut node) = m17_dialled(fixed_clock);
        node.end_reason = Some("reflector refused the link".to_string());
        let mut gw = RecordingOps::default();
        c.tick(&mut gw, &node);
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec!["The link to M17 M17-USA C has ended: reflector refused the link.".to_string()],
            "exactly once, with the reason"
        );
        assert!(c.view().link.is_none());

        // Up, then dropped by the reflector: the reason wins over the bare
        // "has ended".
        let (mut c, mut node) = m17_dialled(fixed_clock);
        let mut gw = RecordingOps::default();
        node.roster = vec![LinkRef::m17("M17-USA", 'C')];
        c.tick(&mut gw, &node);
        node.roster.clear();
        node.end_reason = Some("reflector sent DISC".to_string());
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec![
                "Connected to M17 M17-USA C.".to_string(),
                "The link to M17 M17-USA C has ended: reflector sent DISC.".to_string(),
            ]
        );
    }

    #[test]
    fn tick_gives_up_on_an_m17_leg_that_never_answers() {
        CLOCK_M17_NO_ANSWER.store(0, Ordering::SeqCst);
        let (mut c, node) = m17_dialled(clock_m17_no_answer);
        let mut gw = RecordingOps::default();
        CLOCK_M17_NO_ANSWER.store(29, Ordering::SeqCst);
        c.tick(&mut gw, &node);
        assert!(
            gw.announced.is_empty(),
            "29 seconds is not yet a timeout, got {:?}",
            gw.announced
        );
        CLOCK_M17_NO_ANSWER.store(30, Ordering::SeqCst);
        c.tick(&mut gw, &node);
        assert_eq!(
            gw.announced,
            vec!["M17 M17-USA C did not answer.".to_string()]
        );
        assert!(c.view().link.is_none());
    }

    #[test]
    fn rule6_status_names_the_m17_leg_its_callsign_and_who_was_last_heard() {
        let (mut c, mut node) = m17_dialled(fixed_clock);
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Dialling M17 M17-USA C as AJ7HR — asked by u300, 0s ago. Nobody has the mic."
                    .to_string()
            )]
        );

        node.roster = vec![LinkRef::m17("M17-USA", 'C')];
        c.tick(&mut RecordingOps::default(), &node);
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Linked to M17 M17-USA C as AJ7HR — dialled by u300, 0s ago. Nobody has the mic."
                    .to_string()
            )]
        );

        node.heard = Some(("KC1ABC".to_string(), 40));
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Linked to M17 M17-USA C as AJ7HR — dialled by u300, 0s ago. Last heard KC1ABC \
                 40s ago. Nobody has the mic."
                    .to_string()
            )]
        );
    }

    #[test]
    fn rule6_an_allstar_status_line_never_grows_a_callsign_or_last_heard() {
        let mut c = commander_with(&[APPROVED], &[], &[]);
        let mut node = TestNode {
            joined: true,
            heard: Some(("KC1ABC".to_string(), 40)),
            ..TestNode::default()
        };
        run(
            &mut c,
            &mut RecordingOps::default(),
            &mut node,
            APPROVED,
            dial("44444"),
        );
        node.roster = vec![LinkRef::allstar("44444")];
        c.tick(&mut RecordingOps::default(), &node);
        let mut gw = RecordingOps::default();
        run(&mut c, &mut gw, &mut node, OTHER, BotCommandKind::Status);
        assert_eq!(
            gw.replies,
            vec![(
                IT,
                "Linked to AllStar 44444 — dialled by u300, 0s ago. Nobody has the mic."
                    .to_string()
            )]
        );
    }

    /// `/register` policing (one callsign per member, no change under your
    /// own live M17 leg, a public change line) and revoke/ban hanging up the
    /// target's own M17 leg. The reflector sees the dialler's callsign, so
    /// these rules are about whose identity the node presents.
    mod callsign_policing {
        use super::{
            ADMIN, APPROVED, APPROVER, IT, OTHER, THIRD, commander_with, dial, fingerprint,
            refused, register, run, user,
        };
        use crate::commander::{
            ALREADY_APPROVED, APPROVER_NOT_REVOCABLE, Commander, LinkRef, Network, RecordingOps,
            TestNode,
        };
        use crate::commands::{AdminAction, BotCommandKind, DialTarget};

        const DUPLICATE: &str = "AJ7HR is already registered to another member — ask an approver.";
        const HANG_UP_FIRST: &str = "Hang up your M17 link first.";

        fn dial_m17() -> BotCommandKind {
            BotCommandKind::Dial(DialTarget::M17 {
                reflector: "M17-USA".to_string(),
                module: 'C',
            })
        }

        fn m17_usa_c() -> LinkRef {
            LinkRef::m17("M17-USA", 'C')
        }

        /// `who` registers `callsign` (uppercase) and dials M17-USA C with the
        /// bridge in the channel. ADMIN is a plain admin. The node's roster
        /// is empty, so the leg is still dialling (`up == false`).
        fn dialled_m17(approved: &[u64], who: u64, callsign: &str) -> (Commander, TestNode) {
            let mut c = commander_with(approved, &[ADMIN], &[]);
            let mut node = TestNode {
                joined: true,
                ..TestNode::default()
            };
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, who, register(callsign));
            run(&mut c, &mut gw, &mut node, who, dial_m17());
            assert_eq!(
                node.dialled_m17,
                vec![("M17-USA".to_string(), 'C', callsign.to_string())],
                "setup: the M17 dial must have reached the node"
            );
            let link = c.view().link.expect("setup: the dial records a link");
            assert_eq!(
                (link.network, link.by_id, link.up),
                (Network::M17, Some(who), false)
            );
            // The setup's own dial is not traffic a later refusal made.
            node.dialled_m17.clear();
            (c, node)
        }

        /// Brings the leg up the way production does: the roster shows it,
        /// and the next tick sees it there.
        fn bring_up(c: &mut Commander, node: &mut TestNode) {
            node.roster = vec![m17_usa_c()];
            let mut gw = RecordingOps::default();
            c.tick(&mut gw, node);
            assert_eq!(
                gw.announced,
                vec!["Connected to M17 M17-USA C.".to_string()]
            );
        }

        // -- /register ---------------------------------------------------

        #[test]
        fn a_callsign_held_by_another_member_is_refused_and_nothing_changes() {
            let mut c = commander_with(&[APPROVED, OTHER], &[], &[]);
            let mut node = TestNode::default();
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, OTHER, register("AJ7HR"));
            let before = fingerprint(&c);

            // An approved member: normalised first, so " aj7hr " is the same claim.
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register(" aj7hr "));
            refused(&c, &gw, &node, &before, DUPLICATE);

            // A stranger: refused before being queued, so nothing is pending
            // and the channel is not pinged.
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, THIRD, register("AJ7HR"));
            refused(&c, &gw, &node, &before, DUPLICATE);

            let v = c.view();
            assert_eq!(v.callsigns.get("300"), None);
            assert_eq!(v.callsigns.get("500"), None);
            assert_eq!(v.callsigns.get("400").map(String::as_str), Some("AJ7HR"));
        }

        #[test]
        fn re_registering_your_own_callsign_is_not_a_duplicate_or_a_change() {
            let mut c = commander_with(&[APPROVED], &[], &[]);
            let mut node = TestNode::default();
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("AJ7HR"));
            run(&mut c, &mut gw, &mut node, APPROVED, register("aj7hr"));
            assert_eq!(
                gw.replies,
                vec![
                    (IT, format!("Callsign AJ7HR recorded. {ALREADY_APPROVED}")),
                    (IT, format!("Callsign AJ7HR recorded. {ALREADY_APPROVED}")),
                ]
            );
            assert_eq!(gw.announced, Vec::<String>::new());
        }

        #[test]
        fn an_approved_members_callsign_change_is_announced_exactly() {
            let mut c = commander_with(&[APPROVED], &[], &[]);
            let mut node = TestNode::default();
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("AJ7HR"));
            assert_eq!(
                gw.announced,
                Vec::<String>::new(),
                "a first registration is not a change"
            );

            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("w1aw"));
            assert_eq!(
                gw.replies,
                vec![(IT, format!("Callsign W1AW recorded. {ALREADY_APPROVED}"))]
            );
            assert_eq!(
                gw.announced,
                vec!["<@300> changed their callsign from AJ7HR to W1AW.".to_string()]
            );
            assert_eq!(
                c.view().callsigns.get("300").map(String::as_str),
                Some("W1AW")
            );
        }

        #[test]
        fn a_change_is_refused_while_your_own_m17_leg_is_live_and_allowed_after_hangup() {
            let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");

            // Still dialling already counts: the reflector has the CONN.
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("W1AW"));
            refused(&c, &gw, &node, &before, HANG_UP_FIRST);

            bring_up(&mut c, &mut node);
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("W1AW"));
            refused(&c, &gw, &node, &before, HANG_UP_FIRST);

            // The same callsign again is not a change, so it is not refused.
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("AJ7HR"));
            assert_eq!(
                gw.replies,
                vec![(IT, format!("Callsign AJ7HR recorded. {ALREADY_APPROVED}"))]
            );
            assert_eq!(gw.announced, Vec::<String>::new());

            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, BotCommandKind::Hangup);
            assert_eq!(node.hung_up, vec![m17_usa_c()]);
            node.roster.clear();

            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("W1AW"));
            assert_eq!(
                gw.replies,
                vec![(IT, format!("Callsign W1AW recorded. {ALREADY_APPROVED}"))]
            );
            assert_eq!(
                gw.announced,
                vec!["<@300> changed their callsign from AJ7HR to W1AW.".to_string()]
            );
        }

        #[test]
        fn someone_elses_m17_leg_does_not_block_a_change() {
            let (mut c, mut node) = dialled_m17(&[APPROVED, OTHER], APPROVED, "AJ7HR");
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, OTHER, register("K1ABC"));
            run(&mut c, &mut gw, &mut node, OTHER, register("W1AW"));
            assert_eq!(
                gw.replies,
                vec![
                    (IT, format!("Callsign K1ABC recorded. {ALREADY_APPROVED}")),
                    (IT, format!("Callsign W1AW recorded. {ALREADY_APPROVED}")),
                ]
            );
            assert_eq!(
                gw.announced,
                vec!["<@400> changed their callsign from K1ABC to W1AW.".to_string()]
            );
        }

        #[test]
        fn an_allstar_link_you_dialled_does_not_block_a_change() {
            let mut c = commander_with(&[APPROVED], &[], &[]);
            let mut node = TestNode {
                joined: true,
                ..TestNode::default()
            };
            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("AJ7HR"));
            run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
            assert_eq!(node.dialled, vec!["44444".to_string()]);

            let mut gw = RecordingOps::default();
            run(&mut c, &mut gw, &mut node, APPROVED, register("W1AW"));
            assert_eq!(
                gw.replies,
                vec![(IT, format!("Callsign W1AW recorded. {ALREADY_APPROVED}"))]
            );
            assert_eq!(
                gw.announced,
                vec!["<@300> changed their callsign from AJ7HR to W1AW.".to_string()]
            );
        }

        // -- revoke and ban hang up the target's own M17 leg --------------

        #[test]
        fn revoking_the_dialler_hangs_up_their_m17_leg_after_the_revoked_line() {
            for up in [false, true] {
                let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");
                if up {
                    bring_up(&mut c, &mut node);
                }
                let mut gw = RecordingOps::default();
                run(
                    &mut c,
                    &mut gw,
                    &mut node,
                    ADMIN,
                    BotCommandKind::Revoke {
                        target: user(APPROVED),
                    },
                );
                assert_eq!(node.hung_up, vec![m17_usa_c()], "up={up}");
                assert_eq!(
                    gw.replies,
                    vec![(IT, "Revoked u300.".to_string())],
                    "up={up}"
                );
                assert_eq!(
                    gw.announced,
                    vec![
                        "<@300> was revoked by <@200>.".to_string(),
                        "<@200> hung up M17 M17-USA C.".to_string(),
                    ],
                    "up={up}"
                );
                let v = c.view();
                assert_eq!(v.link, None, "up={up}");
                assert_eq!(v.last_error, None, "up={up}");
            }
        }

        #[test]
        fn banning_the_dialler_hangs_up_their_m17_leg_after_the_banned_line() {
            for up in [false, true] {
                let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");
                if up {
                    bring_up(&mut c, &mut node);
                }
                let mut gw = RecordingOps::default();
                run(
                    &mut c,
                    &mut gw,
                    &mut node,
                    ADMIN,
                    BotCommandKind::Ban {
                        target: user(APPROVED),
                    },
                );
                assert_eq!(node.hung_up, vec![m17_usa_c()], "up={up}");
                assert_eq!(
                    gw.replies,
                    vec![(IT, "Banned u300.".to_string())],
                    "up={up}"
                );
                assert_eq!(
                    gw.announced,
                    vec![
                        "<@300> was banned by <@200>.".to_string(),
                        "<@200> hung up M17 M17-USA C.".to_string(),
                    ],
                    "up={up}"
                );
                assert_eq!(c.view().link, None, "up={up}");
            }
        }

        #[test]
        fn the_admin_page_revoke_and_ban_hang_up_the_dialler_too() {
            for (action, line) in [
                (
                    AdminAction::Revoke(APPROVED),
                    "<@300> was revoked by the node admin.",
                ),
                (
                    AdminAction::Ban(APPROVED),
                    "<@300> was banned by the node admin.",
                ),
            ] {
                for up in [false, true] {
                    let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");
                    if up {
                        bring_up(&mut c, &mut node);
                    }
                    let mut gw = RecordingOps::default();
                    c.admin(action.clone(), &mut gw, &mut node)
                        .expect("the page may revoke and ban");
                    assert_eq!(node.hung_up, vec![m17_usa_c()], "{line} up={up}");
                    assert_eq!(
                        gw.announced,
                        vec![
                            line.to_string(),
                            "the node admin hung up M17 M17-USA C.".to_string(),
                        ],
                        "up={up}"
                    );
                    assert_eq!(c.view().link, None, "{line} up={up}");
                }
            }
        }

        #[test]
        fn revoking_or_banning_someone_else_leaves_the_m17_leg_up() {
            // OTHER is approved, so the revoke really happens. Only the
            // `by_id` comparison keeps the leg up.
            for slash in [true, false] {
                for ban in [false, true] {
                    let (mut c, mut node) = dialled_m17(&[APPROVED, OTHER], APPROVED, "AJ7HR");
                    bring_up(&mut c, &mut node);
                    let mut gw = RecordingOps::default();
                    let actor = if slash { "<@200>" } else { "the node admin" };
                    match (slash, ban) {
                        (true, false) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Revoke {
                                target: user(OTHER),
                            },
                        ),
                        (true, true) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Ban {
                                target: user(OTHER),
                            },
                        ),
                        (false, false) => c
                            .admin(AdminAction::Revoke(OTHER), &mut gw, &mut node)
                            .expect("the page may revoke"),
                        (false, true) => c
                            .admin(AdminAction::Ban(OTHER), &mut gw, &mut node)
                            .expect("the page may ban"),
                    }
                    let verb = if ban { "banned" } else { "revoked" };
                    assert_eq!(
                        node.hung_up,
                        Vec::<LinkRef>::new(),
                        "slash={slash} ban={ban}"
                    );
                    assert_eq!(
                        gw.announced,
                        vec![format!("<@400> was {verb} by {actor}.")],
                        "slash={slash} ban={ban}"
                    );
                    let link = c.view().link.expect("the dialler's leg stays up");
                    assert_eq!(
                        (link.network, link.by_id, link.up),
                        (Network::M17, Some(APPROVED), true)
                    );
                }
            }
        }

        #[test]
        fn revoking_or_banning_the_dialler_of_an_allstar_link_hangs_nothing_up() {
            // An AllStar monitor carries no identity of the dialler's, so it
            // outlives their access, exactly as it did before M17.
            for slash in [true, false] {
                for ban in [false, true] {
                    let mut c = commander_with(&[APPROVED], &[ADMIN], &[]);
                    let mut node = TestNode {
                        joined: true,
                        ..TestNode::default()
                    };
                    let mut gw = RecordingOps::default();
                    run(&mut c, &mut gw, &mut node, APPROVED, dial("44444"));
                    node.roster = vec![LinkRef::allstar("44444")];
                    c.tick(&mut gw, &node);

                    let mut gw = RecordingOps::default();
                    let actor = if slash { "<@200>" } else { "the node admin" };
                    match (slash, ban) {
                        (true, false) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Revoke {
                                target: user(APPROVED),
                            },
                        ),
                        (true, true) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Ban {
                                target: user(APPROVED),
                            },
                        ),
                        (false, false) => c
                            .admin(AdminAction::Revoke(APPROVED), &mut gw, &mut node)
                            .expect("the page may revoke"),
                        (false, true) => c
                            .admin(AdminAction::Ban(APPROVED), &mut gw, &mut node)
                            .expect("the page may ban"),
                    }
                    let verb = if ban { "banned" } else { "revoked" };
                    assert_eq!(
                        node.hung_up,
                        Vec::<LinkRef>::new(),
                        "slash={slash} ban={ban}"
                    );
                    assert_eq!(
                        gw.announced,
                        vec![format!("<@300> was {verb} by {actor}.")],
                        "slash={slash} ban={ban}"
                    );
                    let link = c.view().link.expect("the AllStar link stays up");
                    assert_eq!(
                        (link.network, link.target.as_str(), link.up),
                        (Network::Allstar, "44444", true)
                    );
                }
            }
        }

        #[test]
        fn a_refused_hang_up_on_revoke_or_ban_goes_to_last_error_only_and_keeps_the_link() {
            // All four paths: slash /revoke and /ban, and the admin page's
            // Revoke and Ban. The page still answers Ok: the revoke or ban
            // itself succeeded, and the page reads the failure from view().
            for slash in [true, false] {
                for ban in [false, true] {
                    let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");
                    bring_up(&mut c, &mut node);
                    node.fail_hangup = Some("engine busy".to_string());
                    let mut gw = RecordingOps::default();
                    let actor = if slash { "<@200>" } else { "the node admin" };
                    match (slash, ban) {
                        (true, false) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Revoke {
                                target: user(APPROVED),
                            },
                        ),
                        (true, true) => run(
                            &mut c,
                            &mut gw,
                            &mut node,
                            ADMIN,
                            BotCommandKind::Ban {
                                target: user(APPROVED),
                            },
                        ),
                        (false, false) => assert_eq!(
                            c.admin(AdminAction::Revoke(APPROVED), &mut gw, &mut node),
                            Ok(()),
                            "the page's revoke succeeds even when the hang-up is refused"
                        ),
                        (false, true) => assert_eq!(
                            c.admin(AdminAction::Ban(APPROVED), &mut gw, &mut node),
                            Ok(()),
                            "the page's ban succeeds even when the hang-up is refused"
                        ),
                    }
                    let verb = if ban { "banned" } else { "revoked" };
                    // TestNode records the attempt before it refuses it.
                    assert_eq!(node.hung_up, vec![m17_usa_c()], "slash={slash} ban={ban}");
                    if slash {
                        let reply = if ban { "Banned u300." } else { "Revoked u300." };
                        assert_eq!(
                            gw.replies,
                            vec![(IT, reply.to_string())],
                            "slash={slash} ban={ban}"
                        );
                    }
                    assert_eq!(
                        gw.announced,
                        vec![format!("<@300> was {verb} by {actor}.")],
                        "only the revoked/banned line is announced: slash={slash} ban={ban}"
                    );
                    let v = c.view();
                    assert_eq!(
                        v.last_error,
                        Some("couldn't hang up M17 M17-USA C: engine busy".to_string()),
                        "slash={slash} ban={ban}"
                    );
                    let link = v.link.expect("a refused hang-up leaves the leg up");
                    assert_eq!(
                        (link.network, link.up),
                        (Network::M17, true),
                        "slash={slash} ban={ban}"
                    );
                }
            }
        }

        #[test]
        fn turning_the_bot_off_leaves_the_m17_leg_up() {
            // Switching the bot off closes the mic; it is not a revoke, so the
            // dialler's identity stays on the reflector until someone hangs up.
            for slash in [true, false] {
                let (mut c, mut node) = dialled_m17(&[APPROVED], APPROVED, "AJ7HR");
                bring_up(&mut c, &mut node);
                let mut gw = RecordingOps::default();
                if slash {
                    run(
                        &mut c,
                        &mut gw,
                        &mut node,
                        ADMIN,
                        BotCommandKind::Bot { on: false },
                    );
                } else {
                    c.admin(AdminAction::Disable, &mut gw, &mut node)
                        .expect("the page may switch the bot off");
                }
                assert_eq!(node.hung_up, Vec::<LinkRef>::new(), "slash={slash}");
                let link = c.view().link.expect("the bot switch leaves the leg up");
                assert_eq!(
                    (link.network, link.by_id, link.up),
                    (Network::M17, Some(APPROVED), true),
                    "slash={slash}"
                );
            }
        }

        #[test]
        fn an_approver_is_never_hung_up_by_a_refused_revoke() {
            // A refused revoke has no side effects, the hang-up included.
            let (mut c, mut node) = dialled_m17(&[], APPROVER, "K1ABC");
            let before = fingerprint(&c);
            let mut gw = RecordingOps::default();
            run(
                &mut c,
                &mut gw,
                &mut node,
                ADMIN,
                BotCommandKind::Revoke {
                    target: user(APPROVER),
                },
            );
            refused(&c, &gw, &node, &before, APPROVER_NOT_REVOCABLE);
        }
    }
}
