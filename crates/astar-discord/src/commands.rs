// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The plain data that crosses the gateway boundary, and the validators that
//! stand between Discord and the engine.
//!
//! Nothing here knows about serenity, tokio or sockets: a slash command that
//! has reached this module is already a value, and a value is all the rule
//! engine in [`crate::commander`] ever sees. That split is what lets every
//! rule be tested under default features with no network at all.
//!
//! The single most load-bearing fact in this file is what [`DialTarget`]
//! *cannot* say. It has no host field and no port field, so there is no
//! spelling of a slash command that reaches the engine's `addr` override —
//! a Discord user cannot point this node at an arbitrary endpoint even if
//! they are approved, an admin, or an approver. The validators below narrow
//! it further: an `AllStar` target is digits, and digits are not an address.

/// One Discord user, as the gateway saw them at the moment of the event.
///
/// The name is the display name *then*, carried along so every list the
/// admin page shows and every line posted into the channel can name a human
/// instead of a bare snowflake. It is never compared — `id` is identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRef {
    /// The Discord snowflake.
    pub id: u64,
    /// The display name last seen for this user.
    pub name: String,
}

/// Where a `/dial` wants the node pointed.
///
/// Deliberately not an address. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialTarget {
    /// An `AllStar` node number, unvalidated as it arrived.
    Allstar {
        /// The node number as typed.
        node: String,
    },
    /// An M17 reflector and module, unvalidated as they arrived.
    M17 {
        /// The reflector name as typed.
        reflector: String,
        /// The module letter as typed.
        module: char,
    },
    /// A D-Star reflector and module, unvalidated as they arrived.
    Dstar {
        /// The reflector name as typed.
        reflector: String,
        /// The module letter as typed.
        module: char,
    },
}

/// Which slash command was invoked, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommandKind {
    /// `/register callsign:` — record a callsign and ask an approver for
    /// dial access.
    Request {
        /// The caller's amateur radio callsign, as typed; the Commander
        /// validates and normalises it ([`validate_callsign`]).
        callsign: String,
    },
    /// `/approve user:` — admin tier.
    Approve {
        /// Who is being approved.
        target: UserRef,
    },
    /// `/revoke user:` — admin tier.
    Revoke {
        /// Who is being revoked.
        target: UserRef,
    },
    /// `/dial …` — approved tier.
    Dial(DialTarget),
    /// `/hangup` — approved tier.
    Hangup,
    /// `/status` — anyone.
    Status,
    /// `/mic` (`on: true`) or `/mic state:off` (`on: false`).
    Mic {
        /// Whether the caller is taking the mic or giving it up.
        on: bool,
    },
    /// A click on the panel's PTT button.
    ///
    /// A Discord message button delivers one click and cannot report
    /// press-and-hold, so the button is a toggle: the Commander resolves
    /// this to `Mic { on: false }` when the clicker already holds the mic
    /// and `Mic { on: true }` otherwise, and every `/mic` rule and string
    /// then applies unchanged.
    MicToggle,
    /// `/ban user:` — admin tier.
    Ban {
        /// Who is being banned.
        target: UserRef,
    },
    /// `/unban user:` — admin tier.
    Unban {
        /// Who is being unbanned.
        target: UserRef,
    },
    /// `/unmic` — admin tier; takes the mic from whoever holds it.
    Unmic,
    /// `/bot state:on|off` — admin tier; the master switch.
    Bot {
        /// The new position of the switch.
        on: bool,
    },
    /// `/admin add user:` — approver tier only.
    AdminAdd {
        /// Who is being made an admin.
        target: UserRef,
    },
    /// `/admin remove user:` — approver tier only.
    AdminRemove {
        /// Who is losing admin status.
        target: UserRef,
    },
}

/// One invocation: who typed it, what they typed, and the interaction token
/// the ephemeral reply has to go back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotCommand {
    /// The caller.
    pub user: UserRef,
    /// What they asked for.
    pub kind: BotCommandKind,
    /// The interaction this command arrived on; every reply names it.
    pub interaction: u64,
}

/// One per voice-state update. `channel: None` = disconnected from voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEvent {
    /// Whose voice state changed.
    pub user: UserRef,
    /// The voice channel they are in now, or `None` if they left voice.
    pub channel: Option<u64>,
    /// Whether the guild has them server-muted (not self-mute, which is a
    /// different flag and none of this node's business).
    pub server_muted: bool,
    /// Whether this member is a bot — including this bridge itself.
    pub is_bot: bool,
}

/// Everything the gateway hands the rule engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// The gateway (re)connected: `voice` is the guild's whole voice state,
    /// which is the only chance to reconcile after a reconnect lost events.
    Ready {
        /// Every member currently in voice, anywhere in the guild.
        voice: Vec<VoiceEvent>,
    },
    /// A slash command was invoked.
    Command(BotCommand),
    /// One member's voice state changed.
    Voice(VoiceEvent),
    /// One user started or stopped sending voice packets.
    ///
    /// Edge-triggered, for any user; the Commander ignores every user but
    /// the mic holder. Discord has no "stopped speaking" event, so the
    /// gateway layer infers these from packet arrival — which is the
    /// reverse-engineered half of Discord voice and may stop working
    /// without notice. Nothing here may be the only thing standing between
    /// a forgotten toggle and a stuck open mic; see the hard time-out
    /// timer in `commander`.
    Talking {
        /// Whose voice packets started or stopped.
        user: u64,
        /// `true` on the first packet, `false` when they have gone quiet.
        talking: bool,
    },
    /// The audio track feeding the voice channel stopped.
    ///
    /// The bridge is one long-lived songbird track; if it errors or ends,
    /// nothing downstream notices by itself — the node keeps handing
    /// frames to a dead track, `/status` keeps saying `connected: true`,
    /// and the channel hears silence for as long as the bridge is up.
    /// This is that fact arriving as an event, so the rule engine can say
    /// so once and record why.
    ///
    /// The reason is songbird's, credential-free, and for the operator:
    /// it goes into the admin view, never into the channel.
    AudioFailed {
        /// Why the track stopped, in songbird's words.
        reason: String,
    },
    /// A join installed a working audio track after a recorded failure.
    ///
    /// Emitted only by a join that actually cleared one, so an ordinary
    /// join — every join on a healthy node — emits nothing. Without it the
    /// rule engine reports an outage that is over for the life of the
    /// process, and the next genuine outage is never announced, because
    /// the "already said it once" flag is still set from the first.
    AudioRecovered,
    /// A mute call was rejected by Discord — almost always a missing
    /// Mute Members permission.
    MuteFailed {
        /// Who could not be muted.
        user: u64,
        /// Discord's reason, credential-free.
        reason: String,
    },
}

/// An action taken on the node's own admin page, which is the root of trust:
/// it may act on admins, and it is never subject to the master switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAction {
    /// Approve this user id.
    Approve(u64),
    /// Revoke this user id.
    Revoke(u64),
    /// Ban this user id.
    Ban(u64),
    /// Unban this user id.
    Unban(u64),
    /// Make this user id an admin.
    MakeAdmin(u64),
    /// Remove this user id's admin status.
    RemoveAdmin(u64),
    /// Take the mic from whoever holds it.
    ReleaseMic,
    /// Turn the master switch on.
    Enable,
    /// Turn the master switch off.
    Disable,
    /// Hang up whatever is linked.
    Hangup,
}

/// Checks an `AllStar` node number: ASCII digits only, 1–7 long.
///
/// The digits-only rule is the second half of the no-addresses guarantee.
/// [`DialTarget`] cannot carry a host, and this makes sure the one string it
/// can carry is not a host either: `1.2.3.4:4569` fails here and never
/// reaches the engine.
///
/// # Errors
/// A message naming the rule that was broken, ready to be shown to the
/// caller as an ephemeral refusal.
pub fn validate_allstar_node(s: &str) -> Result<(), String> {
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("An AllStar node number is digits only — no host, no port.".to_string());
    }
    if s.is_empty() || s.len() > 7 {
        return Err("An AllStar node number is 1 to 7 digits.".to_string());
    }
    Ok(())
}

/// Checks a reflector name: 3–8 characters of ASCII alphanumerics or `-`,
/// after trimming. Returns it uppercased, which is how every reflector
/// network spells its own names.
///
/// # Errors
/// A message naming the rule that was broken.
pub fn validate_reflector(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("A reflector name is letters, digits and '-' only.".to_string());
    }
    if trimmed.len() < 3 || trimmed.len() > 8 {
        return Err("A reflector name is 3 to 8 characters.".to_string());
    }
    Ok(trimmed.to_ascii_uppercase())
}

/// The one refusal a bad module gets, shared with the rule engine — which
/// checks a `char` directly and so cannot call [`validate_module`].
pub(crate) const MODULE_RULE: &str = "A module is a single letter, A to Z.";

/// Checks a module: exactly one ASCII letter. Returns it uppercased.
///
/// # Errors
/// A message naming the rule that was broken.
pub fn validate_module(s: &str) -> Result<char, String> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
        _ => Err(MODULE_RULE.to_string()),
    }
}

pub(crate) const CALLSIGN_RULE: &str =
    "A callsign is 3 to 8 letters and digits, with at least one of each — like AJ7HR.";

/// Checks a callsign and returns it uppercased.
///
/// 3 to 8 ASCII letters and digits after trimming, with at least one letter
/// and at least one digit. That admits every ITU-format callsign and fits
/// both places it will be written: D-Star's 8-character MY field and M17's
/// 9-character source address. No `/` or `-` suffixes: the callsign is an
/// identity to put on the air, not a free-text label, and a suffix belongs
/// to the transmission, not to the person.
///
/// # Errors
/// [`CALLSIGN_RULE`], whichever part was broken.
pub fn validate_callsign(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    let ok = trimmed.bytes().all(|b| b.is_ascii_alphanumeric())
        && (3..=8).contains(&trimmed.len())
        && trimmed.bytes().any(|b| b.is_ascii_alphabetic())
        && trimmed.bytes().any(|b| b.is_ascii_digit());
    if ok {
        Ok(trimmed.to_ascii_uppercase())
    } else {
        Err(CALLSIGN_RULE.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CALLSIGN_RULE, validate_allstar_node, validate_callsign, validate_module,
        validate_reflector,
    };

    #[test]
    fn callsign_is_trimmed_and_uppercased() {
        assert_eq!(validate_callsign(" aj7hr "), Ok("AJ7HR".to_string()));
        assert_eq!(validate_callsign("W1AW"), Ok("W1AW".to_string()));
        // The bounds: 3 and 8 are in.
        assert_eq!(validate_callsign("K1A"), Ok("K1A".to_string()));
        assert_eq!(validate_callsign("VK2ABCDE"), Ok("VK2ABCDE".to_string()));
    }

    #[test]
    fn callsign_refuses_each_rule_separately() {
        let rule = Err(CALLSIGN_RULE.to_string());
        // Too short, too long.
        assert_eq!(validate_callsign("K1"), rule);
        assert_eq!(validate_callsign("VK2ABCDEF"), rule);
        // No digit; no letter.
        assert_eq!(validate_callsign("ABCD"), rule);
        assert_eq!(validate_callsign("1234"), rule);
        // Anything that is not a letter or digit, including the suffixes
        // people type by habit and an inner space.
        assert_eq!(validate_callsign("AJ7HR/P"), rule);
        assert_eq!(validate_callsign("AJ7HR-9"), rule);
        assert_eq!(validate_callsign("AJ7 HR"), rule);
        assert_eq!(validate_callsign(""), rule);
    }

    #[test]
    fn allstar_node_accepts_one_to_seven_digits() {
        assert_eq!(validate_allstar_node("1"), Ok(()));
        assert_eq!(validate_allstar_node("44444"), Ok(()));
        assert_eq!(validate_allstar_node("1234567"), Ok(()));
    }

    #[test]
    fn allstar_node_refuses_anything_that_is_not_digits() {
        // The refusal names the rule, and the rule is the one that keeps an
        // address out of the engine.
        assert_eq!(
            validate_allstar_node("1.2.3.4:4569"),
            Err("An AllStar node number is digits only — no host, no port.".to_string())
        );
        assert_eq!(
            validate_allstar_node("12a4"),
            Err("An AllStar node number is digits only — no host, no port.".to_string())
        );
        assert_eq!(
            validate_allstar_node(" 123"),
            Err("An AllStar node number is digits only — no host, no port.".to_string())
        );
        // The four that separate `is_ascii_digit` from `is_numeric` and
        // from a sloppy "parses as a number" check. Full-width digits are
        // numeric, and `char::is_numeric` would let them through.
        for sneaky in ["\u{ff11}\u{ff12}\u{ff13}", "+123", "-1", "123 "] {
            assert_eq!(
                validate_allstar_node(sneaky),
                Err("An AllStar node number is digits only — no host, no port.".to_string()),
                "{sneaky:?} must be refused as not-digits"
            );
        }
    }

    #[test]
    fn allstar_node_refuses_empty_and_too_long() {
        assert_eq!(
            validate_allstar_node(""),
            Err("An AllStar node number is 1 to 7 digits.".to_string())
        );
        assert_eq!(
            validate_allstar_node("12345678"),
            Err("An AllStar node number is 1 to 7 digits.".to_string())
        );
    }

    #[test]
    fn reflector_is_trimmed_and_uppercased() {
        assert_eq!(validate_reflector("  kcwide "), Ok("KCWIDE".to_string()));
        assert_eq!(validate_reflector("xrf-234"), Ok("XRF-234".to_string()));
    }

    #[test]
    fn reflector_refuses_bad_characters_and_bad_lengths() {
        assert_eq!(
            validate_reflector("kc_wide"),
            Err("A reflector name is letters, digits and '-' only.".to_string())
        );
        assert_eq!(
            validate_reflector("ab"),
            Err("A reflector name is 3 to 8 characters.".to_string())
        );
        assert_eq!(
            validate_reflector("abcdefghi"),
            Err("A reflector name is 3 to 8 characters.".to_string())
        );
    }

    #[test]
    fn module_is_one_letter_uppercased() {
        assert_eq!(validate_module("a"), Ok('A'));
        assert_eq!(validate_module("B"), Ok('B'));
        assert_eq!(
            validate_module("AB"),
            Err("A module is a single letter, A to Z.".to_string())
        );
        assert_eq!(
            validate_module("1"),
            Err("A module is a single letter, A to Z.".to_string())
        );
        assert_eq!(
            validate_module(""),
            Err("A module is a single letter, A to Z.".to_string())
        );
    }
}
