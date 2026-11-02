// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What the slash commands ARE, and how one arrives as a value.
//!
//! Two halves of the same fact live here, and they live here together on
//! purpose:
//!
//! * [`COMMANDS`] is the registration table — every command, subcommand,
//!   option, choice and required flag, as plain data. The serenity layer
//!   turns it into `CreateCommand` builders; nothing else decides what
//!   Discord is told this bot accepts.
//! * [`parse_command`] is the reader — the same shapes, turned back into a
//!   [`BotCommandKind`].
//!
//! A registration table and a parser that disagree is the classic failure
//! of this layer: Discord happily offers an option the parser then throws
//! away, and the caller sees "I didn't understand that command" for a
//! command the bot itself advertised. `definitions_and_parser_agree` below
//! walks [`COMMANDS`], builds a well-formed invocation out of each
//! definition, and insists the parser accepts it — so the two can only
//! drift apart with a test failing.
//!
//! **There is no policy in this file.** Nothing here asks who the caller is
//! or whether they may do what they asked; that is the whole of
//! [`crate::commander`]. The one thing this layer decides — because it is
//! the only layer that can — is which guild an interaction came from
//! ([`is_ours`]): a `BotCommand` carries no guild, so a command from
//! somewhere else has to be dropped before it becomes one.
//!
//! Nothing in this file names serenity, which is what lets every rule below
//! be tested under default features with no network and no Discord types.

// Under default features nothing in this crate CALLS any of this: the
// serenity layer that does is behind the `discord` feature. It is compiled
// and tested anyway, which is the point — these rules must hold in a build
// with no Discord, no tokio and no network in it at all.
#![cfg_attr(not(feature = "discord"), allow(dead_code))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::commands::{BotCommandKind, DialTarget, UserRef, validate_module};

/// The guild lock, and the only place it is decided.
///
/// `configured` is the `guild_id` from `node.toml`; `got` is whatever the
/// event carried, which is `None` for a DM. Both are dropped without a
/// reply, a defer or a log line — a bot added to a second server must be
/// inert there, and an unanswered interaction is how Discord shows that.
///
/// A zero `configured` is nobody's guild: it is the "unset" spelling of a
/// snowflake, and matching it against a real event would be a wildcard in
/// the one place a wildcard must not exist.
#[must_use]
pub(crate) fn is_ours(configured: u64, got: Option<u64>) -> bool {
    configured != 0 && got == Some(configured)
}

/// The display name to carry alongside a snowflake, most specific first.
///
/// Per-guild nickname, then the account's display name, then the bare
/// username. An empty string counts as absent: Discord does not send one,
/// but a name rendered as nothing would be worse than falling through to
/// the next candidate, and the caller has no way to tell the difference.
#[must_use]
pub(crate) fn display_name(nick: Option<&str>, global: Option<&str>, username: &str) -> String {
    nick.filter(|s| !s.is_empty())
        .or(global.filter(|s| !s.is_empty()))
        .unwrap_or(username)
        .to_string()
}

/// One option value, as the parser sees it.
///
/// A deliberately tiny stand-in for serenity's `ResolvedValue`: the
/// serenity layer converts into this and then never appears again, so
/// every command shape can be parsed in a unit test without a Discord
/// value — which cannot be constructed at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OptVal {
    /// A string option.
    Str(String),
    /// A user option, already resolved to an id and a display name.
    User(UserRef),
    /// A subcommand, with its own options.
    Sub(Vec<(String, OptVal)>),
    /// Any option type this bot never registers. Kept rather than dropped
    /// so that an option of the WRONG type reads as present-and-wrong
    /// (which refuses) instead of absent (which may have a default).
    Other,
}

/// The kinds of option this bot registers. A deliberate subset of
/// Discord's: adding a kind here means teaching [`parse_command`] to read
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptKind {
    /// A subcommand, which carries options of its own.
    SubCommand,
    /// A free string.
    Str,
    /// A user picker, resolved by Discord before it reaches us.
    User,
}

/// One registered option.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionDef {
    /// Which kind of option.
    pub kind: OptKind,
    /// Its name, as typed after the command.
    pub name: &'static str,
    /// The one-line description Discord shows.
    pub description: &'static str,
    /// Whether Discord should insist on it. Never load-bearing: the node
    /// re-checks everything, because Discord's UI is not a security
    /// boundary.
    pub required: bool,
    /// Fixed choices, if the option is a closed set.
    pub choices: &'static [&'static str],
    /// Sub-options, for [`OptKind::SubCommand`].
    pub options: &'static [OptionDef],
}

/// One registered command.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandDef {
    /// The name, without the slash.
    pub name: &'static str,
    /// The one-line description Discord shows.
    pub description: &'static str,
    /// Its options or subcommands.
    pub options: &'static [OptionDef],
}

const fn plain(kind: OptKind, name: &'static str, description: &'static str) -> OptionDef {
    OptionDef {
        kind,
        name,
        description,
        required: true,
        choices: &[],
        options: &[],
    }
}

const fn choose(
    name: &'static str,
    description: &'static str,
    required: bool,
    choices: &'static [&'static str],
) -> OptionDef {
    OptionDef {
        kind: OptKind::Str,
        name,
        description,
        required,
        choices,
        options: &[],
    }
}

const fn subcommand(
    name: &'static str,
    description: &'static str,
    options: &'static [OptionDef],
) -> OptionDef {
    OptionDef {
        kind: OptKind::SubCommand,
        name,
        description,
        required: false,
        choices: &[],
        options,
    }
}

/// The `on`/`off` choice list, shared by `/mic` and `/bot`.
const ON_OFF: &[&str] = &["on", "off"];

const DIAL_ALLSTAR: &[OptionDef] = &[plain(
    OptKind::Str,
    "node",
    "The AllStar node number to listen to.",
)];

const DIAL_REFLECTOR: &[OptionDef] = &[
    plain(OptKind::Str, "reflector", "The reflector name."),
    plain(OptKind::Str, "module", "The module letter, A to Z."),
];

const DIAL_OPTIONS: &[OptionDef] = &[
    subcommand("allstar", "Listen to an AllStar node.", DIAL_ALLSTAR),
    subcommand(
        "m17",
        "Listen to an M17 reflector, receive-only.",
        DIAL_REFLECTOR,
    ),
    subcommand("dstar", "Listen to a D-Star reflector.", DIAL_REFLECTOR),
];

const ADMIN_USER: &[OptionDef] = &[plain(OptKind::User, "user", "Which user.")];

const ADMIN_OPTIONS: &[OptionDef] = &[
    subcommand("add", "Make someone an admin of this node.", ADMIN_USER),
    subcommand("remove", "Take away someone's admin status.", ADMIN_USER),
];

const USER_OPTION: &[OptionDef] = &[plain(OptKind::User, "user", "Which user.")];

const REGISTER_OPTIONS: &[OptionDef] = &[plain(
    OptKind::Str,
    "callsign",
    "Your amateur radio callsign, e.g. AJ7HR — it is how an M17 reflector sees you when you dial.",
)];

const MIC_OPTIONS: &[OptionDef] = &[choose(
    "state",
    "Take the mic (on) or give it up (off). Defaults to on.",
    false,
    ON_OFF,
)];

const BOT_OPTIONS: &[OptionDef] = &[choose(
    "state",
    "Turn this node's Discord commands on or off.",
    true,
    ON_OFF,
)];

/// Every command this bot registers.
///
/// **All of them are visible to everyone.** There is deliberately no
/// `default_member_permissions` anywhere: the tiers live in
/// [`crate::commander`], which refuses ephemerally and says why. Hiding a
/// command in Discord's UI would make the node's answer depend on a
/// server's permission state, which is exactly the delegation the design
/// forbids — and would leave an admin who had been demoted in the node
/// still seeing the command.
pub(crate) const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "register",
        description: "Register your callsign and ask this node's approvers for permission to dial.",
        options: REGISTER_OPTIONS,
    },
    CommandDef {
        name: "status",
        description: "What this node is linked to right now.",
        options: &[],
    },
    CommandDef {
        name: "dial",
        description: "Point this node at a network, receive-only.",
        options: DIAL_OPTIONS,
    },
    CommandDef {
        name: "hangup",
        description: "Disconnect whatever this node is linked to.",
        options: &[],
    },
    CommandDef {
        name: "mic",
        description: "Take the mic, or give it up.",
        options: MIC_OPTIONS,
    },
    CommandDef {
        name: "approve",
        description: "Let someone dial this node.",
        options: USER_OPTION,
    },
    CommandDef {
        name: "revoke",
        description: "Take away someone's permission to dial.",
        options: USER_OPTION,
    },
    CommandDef {
        name: "ban",
        description: "Stop someone using this node at all.",
        options: USER_OPTION,
    },
    CommandDef {
        name: "unban",
        description: "Lift a ban.",
        options: USER_OPTION,
    },
    CommandDef {
        name: "unmic",
        description: "Take the mic away from whoever has it.",
        options: &[],
    },
    CommandDef {
        name: "bot",
        description: "Turn this node's Discord commands on or off.",
        options: BOT_OPTIONS,
    },
    CommandDef {
        name: "admin",
        description: "Manage this node's admins (approvers only).",
        options: ADMIN_OPTIONS,
    },
];

/// The panel's PTT button.
pub(crate) const PTT_BUTTON_ID: &str = "astar:ptt";
/// The panel's Status button.
pub(crate) const STATUS_BUTTON_ID: &str = "astar:status";

/// A button click, as a command.
///
/// The click is translated into the same [`BotCommandKind`] a typed
/// command produces, so every rule and every refusal string applies to it
/// unchanged — the Commander cannot tell the difference, and that is the
/// point. An id we did not put on a button is `None`.
#[must_use]
pub(crate) fn parse_custom_id(custom_id: &str) -> Option<BotCommandKind> {
    match custom_id {
        PTT_BUTTON_ID => Some(BotCommandKind::MicToggle),
        STATUS_BUTTON_ID => Some(BotCommandKind::Status),
        _ => None,
    }
}

/// The module char a malformed module option becomes.
///
/// `DialTarget::module` is a `char`, so a module option of "AB" or "" has
/// no faithful representation — but answering "I didn't understand that
/// command" for a typo in ONE option would be a worse answer than the one
/// the Commander already has for it. `'?'` is not `is_ascii_alphabetic`,
/// which is precisely the condition the Commander refuses with the module
/// rule, so a malformed module reaches the caller as "A module is a single
/// letter, A to Z." — the rule it broke. The decision of what to do about
/// it still lives in exactly one place.
const BAD_MODULE: char = '?';

fn get<'a>(opts: &'a [(String, OptVal)], name: &str) -> Option<&'a OptVal> {
    opts.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// The string value of an option, or `None` when it is absent OR present
/// with some other type. A wrong type must never read as a missing option:
/// `/mic` treats absent as "take the mic", and a bogus `state` must not
/// inherit that.
fn str_of<'a>(opts: &'a [(String, OptVal)], name: &str) -> Option<&'a str> {
    match get(opts, name) {
        Some(OptVal::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn user_of(opts: &[(String, OptVal)], name: &str) -> Option<UserRef> {
    match get(opts, name) {
        Some(OptVal::User(u)) => Some(u.clone()),
        _ => None,
    }
}

/// The single subcommand of a command that has subcommands.
///
/// Discord sends exactly one; anything else is a shape this bot did not
/// register and does not read.
fn only_sub(opts: &[(String, OptVal)]) -> Option<(&str, &[(String, OptVal)])> {
    match opts {
        [(name, OptVal::Sub(inner))] => Some((name.as_str(), inner.as_slice())),
        _ => None,
    }
}

fn on_off(s: &str) -> Option<bool> {
    match s {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn module_of(opts: &[(String, OptVal)]) -> Option<char> {
    let raw = str_of(opts, "module")?;
    Some(validate_module(raw).unwrap_or(BAD_MODULE))
}

/// One invocation, as a value — or `None` when the shape is not one this
/// bot registered.
///
/// `None` is answered with "I didn't understand that command." and emits
/// nothing: an unparsed invocation never becomes a [`BotCommandKind`], so
/// the Commander is never asked to rule on a shape nobody designed.
#[must_use]
pub(crate) fn parse_command(name: &str, opts: &[(String, OptVal)]) -> Option<BotCommandKind> {
    match name {
        "register" => Some(BotCommandKind::Request {
            callsign: str_of(opts, "callsign")?.to_string(),
        }),
        "status" => Some(BotCommandKind::Status),
        "hangup" => Some(BotCommandKind::Hangup),
        "unmic" => Some(BotCommandKind::Unmic),
        "approve" => user_of(opts, "user").map(|target| BotCommandKind::Approve { target }),
        "revoke" => user_of(opts, "user").map(|target| BotCommandKind::Revoke { target }),
        "ban" => user_of(opts, "user").map(|target| BotCommandKind::Ban { target }),
        "unban" => user_of(opts, "user").map(|target| BotCommandKind::Unban { target }),
        // The only command with a defaulted option: bare `/mic` takes the
        // mic. `get` rather than `str_of` so a wrong-typed `state` refuses
        // instead of falling into that default.
        "mic" => match get(opts, "state") {
            None => Some(BotCommandKind::Mic { on: true }),
            Some(OptVal::Str(s)) => on_off(s).map(|on| BotCommandKind::Mic { on }),
            Some(_) => None,
        },
        "bot" => on_off(str_of(opts, "state")?).map(|on| BotCommandKind::Bot { on }),
        "dial" => match only_sub(opts)? {
            ("allstar", o) => Some(BotCommandKind::Dial(DialTarget::Allstar {
                node: str_of(o, "node")?.to_string(),
            })),
            ("m17", o) => Some(BotCommandKind::Dial(DialTarget::M17 {
                reflector: str_of(o, "reflector")?.to_string(),
                module: module_of(o)?,
            })),
            ("dstar", o) => Some(BotCommandKind::Dial(DialTarget::Dstar {
                reflector: str_of(o, "reflector")?.to_string(),
                module: module_of(o)?,
            })),
            _ => None,
        },
        "admin" => match only_sub(opts)? {
            ("add", o) => Some(BotCommandKind::AdminAdd {
                target: user_of(o, "user")?,
            }),
            ("remove", o) => Some(BotCommandKind::AdminRemove {
                target: user_of(o, "user")?,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// How long a deferred interaction is worth remembering.
///
/// Discord's interaction token is valid for 15 minutes. An entry older
/// than this can no longer be answered, so keeping it would only be a leak
/// — of memory, and of a token — with nothing to spend it on.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(14 * 60);

/// Drop every pending interaction older than `ttl`.
///
/// Called on every insert rather than on a timer: the map only grows when
/// an interaction arrives, so that is the only moment it can need
/// trimming, and it costs nothing to sweep a map that holds a handful of
/// entries.
pub(crate) fn prune_expired<T>(map: &mut HashMap<u64, (Instant, T)>, now: Instant, ttl: Duration) {
    map.retain(|_, (at, _)| now.saturating_duration_since(*at) < ttl);
}

#[cfg(test)]
mod tests {
    use super::{
        COMMANDS, CommandDef, OptKind, OptVal, OptionDef, PENDING_TTL, display_name, is_ours,
        parse_command, parse_custom_id, prune_expired,
    };
    use crate::commands::{BotCommandKind, DialTarget, UserRef};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn user(id: u64) -> UserRef {
        UserRef {
            id,
            name: "kd0xyz".to_string(),
        }
    }

    fn opts(pairs: &[(&str, OptVal)]) -> Vec<(String, OptVal)> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_string(), v.clone()))
            .collect()
    }

    fn sub(name: &str, inner: &[(&str, OptVal)]) -> Vec<(String, OptVal)> {
        vec![(name.to_string(), OptVal::Sub(opts(inner)))]
    }

    // ---- the guild lock ------------------------------------------------

    #[test]
    fn only_the_configured_guild_is_ours() {
        assert!(is_ours(42, Some(42)));
        assert!(!is_ours(42, Some(43)));
    }

    #[test]
    fn a_dm_is_never_ours() {
        // `None` is how a DM arrives. It must not match anything, least of
        // all an unset configured guild.
        assert!(!is_ours(42, None));
        assert!(!is_ours(0, None));
    }

    #[test]
    fn an_unset_configured_guild_matches_nothing() {
        // Zero is the "unset" spelling of a snowflake, not a wildcard.
        assert!(!is_ours(0, Some(42)));
        assert!(!is_ours(0, Some(0)));
    }

    // ---- display names -------------------------------------------------

    #[test]
    fn the_nickname_wins_then_the_global_name_then_the_username() {
        assert_eq!(display_name(Some("Rob"), Some("robl"), "rludwick"), "Rob");
        assert_eq!(display_name(None, Some("robl"), "rludwick"), "robl");
        assert_eq!(display_name(None, None, "rludwick"), "rludwick");
    }

    #[test]
    fn an_empty_name_falls_through_rather_than_rendering_as_nothing() {
        assert_eq!(display_name(Some(""), Some("robl"), "rludwick"), "robl");
        assert_eq!(display_name(Some(""), Some(""), "rludwick"), "rludwick");
    }

    // ---- the buttons ---------------------------------------------------

    #[test]
    fn the_two_panel_buttons_are_commands_and_nothing_else_is() {
        assert_eq!(
            parse_custom_id("astar:ptt"),
            Some(BotCommandKind::MicToggle)
        );
        assert_eq!(
            parse_custom_id("astar:status"),
            Some(BotCommandKind::Status)
        );
        assert_eq!(parse_custom_id("astar:key"), None);
        assert_eq!(parse_custom_id(""), None);
        assert_eq!(parse_custom_id("ptt"), None);
    }

    // ---- parsing, one command shape at a time --------------------------

    #[test]
    fn the_commands_that_take_nothing() {
        assert_eq!(
            parse_command(
                "register",
                &opts(&[("callsign", OptVal::Str("aj7hr".into()))])
            ),
            Some(BotCommandKind::Request {
                callsign: "aj7hr".to_string()
            }),
            "the parser passes the callsign through as typed; the Commander normalises it"
        );
        assert_eq!(
            parse_command("register", &[]),
            None,
            "the callsign is mandatory"
        );
        assert_eq!(
            parse_command("request", &[]),
            None,
            "/request is gone, not an alias"
        );
        assert_eq!(parse_command("status", &[]), Some(BotCommandKind::Status));
        assert_eq!(parse_command("hangup", &[]), Some(BotCommandKind::Hangup));
        assert_eq!(parse_command("unmic", &[]), Some(BotCommandKind::Unmic));
    }

    #[test]
    fn the_commands_that_take_a_user() {
        let o = opts(&[("user", OptVal::User(user(7)))]);
        assert_eq!(
            parse_command("approve", &o),
            Some(BotCommandKind::Approve { target: user(7) })
        );
        assert_eq!(
            parse_command("revoke", &o),
            Some(BotCommandKind::Revoke { target: user(7) })
        );
        assert_eq!(
            parse_command("ban", &o),
            Some(BotCommandKind::Ban { target: user(7) })
        );
        assert_eq!(
            parse_command("unban", &o),
            Some(BotCommandKind::Unban { target: user(7) })
        );
    }

    #[test]
    fn a_missing_required_option_is_not_a_command() {
        assert_eq!(parse_command("approve", &[]), None);
        assert_eq!(parse_command("revoke", &[]), None);
        assert_eq!(parse_command("ban", &[]), None);
        assert_eq!(parse_command("unban", &[]), None);
        assert_eq!(parse_command("bot", &[]), None);
    }

    #[test]
    fn an_option_of_the_wrong_type_is_not_a_command() {
        // Present, but a string where a user belongs. This must refuse, not
        // read as absent.
        let wrong = opts(&[("user", OptVal::Str("7".to_string()))]);
        assert_eq!(parse_command("approve", &wrong), None);
        assert_eq!(parse_command("ban", &wrong), None);
        let other = opts(&[("user", OptVal::Other)]);
        assert_eq!(parse_command("revoke", &other), None);
    }

    #[test]
    fn bare_mic_takes_the_mic() {
        assert_eq!(
            parse_command("mic", &[]),
            Some(BotCommandKind::Mic { on: true })
        );
    }

    #[test]
    fn mic_state_says_which_way() {
        assert_eq!(
            parse_command("mic", &opts(&[("state", OptVal::Str("on".into()))])),
            Some(BotCommandKind::Mic { on: true })
        );
        assert_eq!(
            parse_command("mic", &opts(&[("state", OptVal::Str("off".into()))])),
            Some(BotCommandKind::Mic { on: false })
        );
    }

    #[test]
    fn a_mic_state_that_is_neither_on_nor_off_is_refused_not_defaulted() {
        // The bug this guards: reading a bogus `state` as "no option" would
        // silently key the mic for `/mic state:whatever`.
        assert_eq!(
            parse_command("mic", &opts(&[("state", OptVal::Str("maybe".into()))])),
            None
        );
        assert_eq!(
            parse_command("mic", &opts(&[("state", OptVal::User(user(7)))])),
            None
        );
    }

    #[test]
    fn the_master_switch_needs_a_state() {
        assert_eq!(
            parse_command("bot", &opts(&[("state", OptVal::Str("on".into()))])),
            Some(BotCommandKind::Bot { on: true })
        );
        assert_eq!(
            parse_command("bot", &opts(&[("state", OptVal::Str("off".into()))])),
            Some(BotCommandKind::Bot { on: false })
        );
        assert_eq!(
            parse_command("bot", &opts(&[("state", OptVal::Str("yes".into()))])),
            None
        );
    }

    #[test]
    fn dial_allstar_carries_the_node_number_verbatim() {
        // Verbatim on purpose: validation is the Commander's, and its
        // refusal names the rule. The parser must not quietly "fix" it.
        assert_eq!(
            parse_command(
                "dial",
                &sub("allstar", &[("node", OptVal::Str("44444".into()))])
            ),
            Some(BotCommandKind::Dial(DialTarget::Allstar {
                node: "44444".to_string()
            }))
        );
        assert_eq!(
            parse_command(
                "dial",
                &sub("allstar", &[("node", OptVal::Str("1.2.3.4:4569".into()))])
            ),
            Some(BotCommandKind::Dial(DialTarget::Allstar {
                node: "1.2.3.4:4569".to_string()
            }))
        );
    }

    #[test]
    fn dial_m17_and_dstar_carry_a_reflector_and_a_module() {
        assert_eq!(
            parse_command(
                "dial",
                &sub(
                    "m17",
                    &[
                        ("reflector", OptVal::Str("kcwide".into())),
                        ("module", OptVal::Str("a".into())),
                    ]
                )
            ),
            Some(BotCommandKind::Dial(DialTarget::M17 {
                reflector: "kcwide".to_string(),
                module: 'A',
            }))
        );
        assert_eq!(
            parse_command(
                "dial",
                &sub(
                    "dstar",
                    &[
                        ("reflector", OptVal::Str("xrf234".into())),
                        ("module", OptVal::Str("B".into())),
                    ]
                )
            ),
            Some(BotCommandKind::Dial(DialTarget::Dstar {
                reflector: "xrf234".to_string(),
                module: 'B',
            }))
        );
    }

    #[test]
    fn a_malformed_module_becomes_a_char_the_commander_refuses() {
        // Not `None`: "I didn't understand that command" would bury a typo
        // in one option. `'?'` is not `is_ascii_alphabetic`, which is the
        // Commander's own test, so the caller is told the module rule.
        for bad in ["AB", "", "1", " "] {
            assert_eq!(
                parse_command(
                    "dial",
                    &sub(
                        "m17",
                        &[
                            ("reflector", OptVal::Str("kcwide".into())),
                            ("module", OptVal::Str(bad.into())),
                        ]
                    )
                ),
                Some(BotCommandKind::Dial(DialTarget::M17 {
                    reflector: "kcwide".to_string(),
                    module: '?',
                })),
                "module {bad:?} must reach the Commander as a refusable char"
            );
        }
        assert!(
            !'?'.is_ascii_alphabetic(),
            "the sentinel only works while the Commander's own check refuses it"
        );
    }

    #[test]
    fn admin_add_and_remove_carry_the_target() {
        assert_eq!(
            parse_command("admin", &sub("add", &[("user", OptVal::User(user(9)))])),
            Some(BotCommandKind::AdminAdd { target: user(9) })
        );
        assert_eq!(
            parse_command("admin", &sub("remove", &[("user", OptVal::User(user(9)))])),
            Some(BotCommandKind::AdminRemove { target: user(9) })
        );
    }

    #[test]
    fn an_unknown_subcommand_is_not_a_command() {
        assert_eq!(
            parse_command("dial", &sub("dmr", &[("node", OptVal::Str("1".into()))])),
            None
        );
        assert_eq!(
            parse_command("admin", &sub("promote", &[("user", OptVal::User(user(9)))])),
            None
        );
        // A subcommand missing ITS required option.
        assert_eq!(parse_command("dial", &sub("allstar", &[])), None);
        assert_eq!(parse_command("admin", &sub("add", &[])), None);
    }

    #[test]
    fn a_command_that_needs_a_subcommand_and_got_none_is_not_a_command() {
        assert_eq!(parse_command("dial", &[]), None);
        assert_eq!(parse_command("admin", &[]), None);
        // Two subcommands is a shape Discord never sends and this bot never
        // registered.
        let mut two = sub("add", &[("user", OptVal::User(user(1)))]);
        two.extend(sub("remove", &[("user", OptVal::User(user(2)))]));
        assert_eq!(parse_command("admin", &two), None);
    }

    #[test]
    fn an_unknown_command_is_not_a_command() {
        assert_eq!(parse_command("key", &[]), None);
        assert_eq!(parse_command("", &[]), None);
        assert_eq!(parse_command("Status", &[]), None, "names are lowercase");
    }

    // ---- registration and parsing cannot drift apart -------------------

    /// A value of the right shape for one registered option.
    ///
    /// `"A"` serves for every free string here: it is a single letter, so
    /// it satisfies the module option, and `node`/`reflector` are carried
    /// verbatim by the parser, which validates nothing. An option with
    /// choices uses its own first choice — which is what makes a choice
    /// list nobody can parse (say `state: yes|no`) fail this test.
    fn well_formed(def: &OptionDef) -> OptVal {
        match def.kind {
            OptKind::SubCommand => OptVal::Sub(
                def.options
                    .iter()
                    .map(|o| (o.name.to_string(), well_formed(o)))
                    .collect(),
            ),
            OptKind::Str => OptVal::Str(
                def.choices
                    .first()
                    .map_or_else(|| "A".to_string(), |c| (*c).to_string()),
            ),
            OptKind::User => OptVal::User(user(1234)),
        }
    }

    /// Every invocation a command's own definition permits: one per
    /// subcommand, or a single one with all its options filled in.
    fn invocations(def: &CommandDef) -> Vec<Vec<(String, OptVal)>> {
        if def.options.iter().any(|o| o.kind == OptKind::SubCommand) {
            def.options
                .iter()
                .map(|s| vec![(s.name.to_string(), well_formed(s))])
                .collect()
        } else {
            vec![
                def.options
                    .iter()
                    .map(|o| (o.name.to_string(), well_formed(o)))
                    .collect(),
            ]
        }
    }

    #[test]
    fn definitions_and_parser_agree() {
        // The test that stops the two halves of this file drifting: a
        // command registered with Discord that the parser then throws away
        // shows up to the caller as "I didn't understand that command" for
        // something the bot itself offered them.
        for def in COMMANDS {
            for inv in invocations(def) {
                assert!(
                    parse_command(def.name, &inv).is_some(),
                    "registered command /{} does not parse from its own definition: {inv:?}",
                    def.name
                );
            }
        }
    }

    #[test]
    fn every_registered_command_reaches_a_distinct_shape() {
        // Guards the other direction of the same drift: two definitions
        // that parse to the SAME kind would mean one of them is dead, and
        // a `COMMANDS` entry nothing can produce is worse than no entry.
        let mut seen: Vec<BotCommandKind> = Vec::new();
        for def in COMMANDS {
            for inv in invocations(def) {
                let kind = parse_command(def.name, &inv)
                    .unwrap_or_else(|| panic!("/{} must parse", def.name));
                assert!(
                    !seen.contains(&kind),
                    "/{} parses to {kind:?}, which another command already produces",
                    def.name
                );
                seen.push(kind);
            }
        }
        // 12 commands, but `dial` and `admin` contribute one shape per
        // subcommand: 10 + 3 + 2.
        assert_eq!(seen.len(), 15);
    }

    #[test]
    fn the_registration_table_is_something_discord_would_accept() {
        // Discord's own limits, checked here rather than discovered as a
        // 400 on a live bot at start-up: names are lowercase and short,
        // descriptions are 1..=100 characters.
        fn check_option(def: &OptionDef) {
            assert!(
                def.name.len() <= 32 && def.name == def.name.to_lowercase(),
                "option name {:?} is not a legal Discord option name",
                def.name
            );
            assert!(
                (1..=100).contains(&def.description.chars().count()),
                "option {:?}'s description is not 1..=100 characters",
                def.name
            );
            for sub in def.options {
                check_option(sub);
            }
        }
        for def in COMMANDS {
            assert!(
                def.name.len() <= 32 && def.name == def.name.to_lowercase(),
                "command name {:?} is not a legal Discord command name",
                def.name
            );
            assert!(
                (1..=100).contains(&def.description.chars().count()),
                "/{}'s description is not 1..=100 characters",
                def.name
            );
            for opt in def.options {
                check_option(opt);
            }
        }
    }

    #[test]
    fn the_m17_wording_says_receive_only_and_what_the_callsign_is_for() {
        let dial = COMMANDS
            .iter()
            .find(|d| d.name == "dial")
            .expect("/dial is registered");
        let m17 = dial
            .options
            .iter()
            .find(|o| o.name == "m17")
            .expect("/dial has an m17 subcommand");
        assert_eq!(m17.description, "Listen to an M17 reflector, receive-only.");
        let register = COMMANDS
            .iter()
            .find(|d| d.name == "register")
            .expect("/register is registered");
        assert_eq!(
            register
                .options
                .iter()
                .map(|o| o.description)
                .collect::<Vec<_>>(),
            vec![
                "Your amateur radio callsign, e.g. AJ7HR — it is how an M17 reflector sees you \
                 when you dial."
            ]
        );
    }

    // ---- the pending map -----------------------------------------------

    #[test]
    fn prune_drops_only_expired() {
        // The base is in the FUTURE of `Instant::now()` so the arithmetic
        // is addition only: subtracting from a monotonic clock can
        // underflow on a machine that has just booted.
        let base = Instant::now();
        let mut map: HashMap<u64, (Instant, &str)> = HashMap::new();
        map.insert(1, (base, "the oldest"));
        map.insert(2, (base + Duration::from_secs(60), "a minute later"));
        map.insert(3, (base + Duration::from_secs(13 * 60), "still answerable"));

        // "Now" is 14 minutes after the oldest entry, which is exactly the
        // TTL: 1 is expired, 2 is 13 minutes old, 3 is one minute old.
        prune_expired(&mut map, base + PENDING_TTL, PENDING_TTL);

        let mut left: Vec<u64> = map.keys().copied().collect();
        left.sort_unstable();
        assert_eq!(left, vec![2, 3]);
        assert_eq!(map.get(&2).map(|(_, v)| *v), Some("a minute later"));
    }

    #[test]
    fn prune_keeps_everything_when_nothing_has_expired() {
        let base = Instant::now();
        let mut map: HashMap<u64, (Instant, u8)> = HashMap::new();
        map.insert(1, (base, 1));
        map.insert(2, (base, 2));

        prune_expired(&mut map, base + Duration::from_secs(60), PENDING_TTL);

        assert_eq!(map.len(), 2);
    }

    #[test]
    fn prune_drops_everything_once_every_token_is_dead() {
        let base = Instant::now();
        let mut map: HashMap<u64, (Instant, u8)> = HashMap::new();
        map.insert(1, (base, 1));
        map.insert(2, (base + Duration::from_secs(30), 2));

        prune_expired(&mut map, base + Duration::from_secs(20 * 60), PENDING_TTL);

        assert!(map.is_empty());
    }
}
