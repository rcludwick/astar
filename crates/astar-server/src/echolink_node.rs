// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The node's `EchoLink` leg as the controller sees it: what `[echolink]`
//! installed, the checks a dial passes before the station is asked, the
//! sentences each refusal says, and the `/status` card.
//!
//! Everything here is pure. The station calls live in `controller.rs`,
//! because the station is private to the controller (that is what keeps
//! `key_refusal` in front of every way to key it); this module only decides
//! and describes.
//!
//! # What a dial can name
//!
//! A dial names a station by node number or callsign, and only a station
//! the directory lists can be called. Nothing that arrives from the wire
//! can name a host or an address: the station's IP comes from the directory
//! the node logged in to.
//!
//! # No password anywhere
//!
//! The password is read from the [`crate::secrets::SecretProvider`] only at
//! the moment of a login and moved straight into the station. Nothing here
//! holds it, and no sentence below can carry it.

use astar_station::{
    CallLink, EchoLinkPeerStatus, EchoLinkStationInfo, LoginState, PresenceSnapshot,
};

use crate::command::{EchoLinkNodeStatus, EchoLinkPeerRow, NodeError};
use crate::config::EchoLinkCfg;

/// Refusal for every `POST /echolink` on a node with no `[echolink]`
/// section. `EchoLink` is dormant until the operator adds one.
pub(crate) const ECHOLINK_NOT_ENABLED: &str =
    "EchoLink isn't enabled on this node — its operator can add an [echolink] section.";

/// Refusal for a dial with no usable target.
pub(crate) const ECHOLINK_NEEDS_TARGET: &str = "A dial needs a node number or callsign.";

/// Refusal for a disconnect whose target is present but names nothing. Only
/// a disconnect with no target at all hangs up on everyone.
pub(crate) const ECHOLINK_DISCONNECT_NEEDS_TARGET: &str =
    "A disconnect needs a callsign; leave the target out to hang up on everyone.";

/// Refusal for a disconnect-everyone with nobody on the leg.
pub(crate) const ECHOLINK_NOBODY_ON: &str = "No EchoLink station is connected to this node.";

/// The longest target a dial or disconnect accepts. `EchoLink` callsigns are
/// at most a dozen characters and node numbers at most seven digits, so
/// anything longer is not a station, and it would only be echoed back in a
/// refusal.
const TARGET_MAX_CHARS: usize = 32;

/// `"EchoLink isn't logged in yet: <reason>"`.
pub(crate) fn not_logged_in(reason: &str) -> String {
    format!("EchoLink isn't logged in yet: {reason}")
}

/// Refusal for a dial whose station is not in the list in hand. The list
/// may be up to ten minutes old, so the pre-flight asks for a fresh one as
/// it refuses: a station that has only just logged in is there on the next
/// try.
pub(crate) const ECHOLINK_NOT_LISTED_YET: &str = "Not in the station list yet — refreshing…";

/// A disconnect for a station that is not on the leg.
pub(crate) fn not_on_node(target: &str) -> String {
    format!("No EchoLink station {target} is connected to this node.")
}

/// A dial for a station that is already on the leg.
pub(crate) fn already_on_node(target: &str) -> String {
    format!("EchoLink station {target} is already connected to this node.")
}

/// Why the node did not log in at start-up when no password is held. Names
/// the secret to set, never a value.
pub(crate) fn no_password(callsign: &str) -> String {
    format!("no password: set the echolink:{callsign} secret")
}

/// Why the node did not log in when the stored password holds a character
/// the directory line cannot carry. Names the secret, never its value.
pub(crate) fn unsendable_password(callsign: &str) -> String {
    format!(
        "the echolink:{callsign} secret has a character the directory can't take (a line \
         break, or a non-ASCII character such as ¬); set it again"
    )
}

/// The node's `EchoLink` state. Behind one short-held mutex on the
/// controller, taken only to clone or set a field, never across a station
/// call.
#[derive(Default)]
pub(crate) struct EchoLinkNodeState {
    /// `[echolink]`, validated at load. `None` ⇒ `EchoLink` is dormant.
    pub(crate) cfg: Option<EchoLinkCfg>,
    /// Why the last login attempt never started (no password, or the
    /// station refused its arguments). Cleared by the next attempt.
    pub(crate) setup_error: Option<String>,
}

/// The target as the station will be asked for it: trimmed, non-empty, no
/// longer than a station can be, with no whitespace or control character
/// inside.
///
/// # Errors
/// [`ECHOLINK_NEEDS_TARGET`] for anything else.
pub(crate) fn clean_target(target: Option<&str>) -> Result<String, NodeError> {
    let t = target.map(str::trim).unwrap_or_default();
    if t.is_empty()
        || t.chars().count() > TARGET_MAX_CHARS
        || t.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(refuse(ECHOLINK_NEEDS_TARGET.to_string()));
    }
    Ok(t.to_string())
}

/// Whether the directory list in hand has `target`, matched the way the
/// station matches a dial: a number is a node number, anything else a
/// callsign, case-blind.
pub(crate) fn is_listed(stations: &[EchoLinkStationInfo], target: &str) -> bool {
    match target.parse::<u32>() {
        Ok(node) => stations.iter().any(|s| s.node == node),
        Err(_) => stations.iter().any(|s| s.call.eq_ignore_ascii_case(target)),
    }
}

/// The dial pre-flight, in its fixed order. Every refusal is one plain,
/// secret-free sentence, and every one happens before the station is asked:
///
/// 1. No `[echolink]` section.
/// 2. No usable target.
/// 3. Not logged in, with the reason.
/// 4. A station the list in hand does not have. `listed` reads the station
///    list, so it is a closure and runs only once 1–3 have passed; on a
///    miss `refresh` asks the directory for a fresh list (the engine's 30 s
///    floor keeps a run of misses from hammering it) and the refusal says
///    so.
///
/// Returns the cleaned target.
pub(crate) fn dial_preflight(
    cfg: Option<&EchoLinkCfg>,
    setup_error: Option<&str>,
    login: Option<&PresenceSnapshot>,
    target: Option<&str>,
    listed: impl FnOnce(&str) -> bool,
    refresh: impl FnOnce(),
) -> Result<String, NodeError> {
    if cfg.is_none() {
        return Err(refuse(ECHOLINK_NOT_ENABLED.to_string()));
    }
    let target = clean_target(target)?;
    if login.map(|l| l.state) != Some(LoginState::LoggedIn) {
        return Err(refuse(not_logged_in(&login_reason(setup_error, login))));
    }
    if !listed(&target) {
        refresh();
        return Err(refuse(ECHOLINK_NOT_LISTED_YET.to_string()));
    }
    Ok(target)
}

/// Why the node is not logged in, in a few words.
fn login_reason(setup_error: Option<&str>, login: Option<&PresenceSnapshot>) -> String {
    if let Some(e) = setup_error {
        return e.to_string();
    }
    match login {
        None => "logged out".to_string(),
        Some(l) => match l.state {
            LoginState::LoggingIn => "still logging in".to_string(),
            LoginState::Failed => l
                .error
                .clone()
                .unwrap_or_else(|| "the login failed".to_string()),
            LoginState::LoggedOut | LoginState::LoggedIn => "logged out".to_string(),
        },
    }
}

/// The `/status` word for a login state. `None` is a node that never
/// logged in, or has logged out.
fn login_label(state: Option<LoginState>) -> &'static str {
    match state {
        None | Some(LoginState::LoggedOut) => "logged_out",
        Some(LoginState::LoggingIn) => "logging_in",
        Some(LoginState::LoggedIn) => "logged_in",
        Some(LoginState::Failed) => "failed",
    }
}

/// One leg station, flattened for the wire.
fn peer_row(p: &EchoLinkPeerStatus) -> EchoLinkPeerRow {
    EchoLinkPeerRow {
        call: p.call.clone(),
        node: p.node,
        state: match p.link {
            CallLink::Connecting => "connecting",
            CallLink::Connected => "connected",
            CallLink::Closed => "closed",
        }
        .to_string(),
        talking: p.talking,
        up_secs: p.up_secs,
        notice: p.notice.clone(),
    }
}

/// The `/status` card for a configured node.
pub(crate) fn node_status(
    cfg: &EchoLinkCfg,
    setup_error: Option<&str>,
    login: Option<&PresenceSnapshot>,
    peers: &[EchoLinkPeerStatus],
) -> EchoLinkNodeStatus {
    EchoLinkNodeStatus {
        login: login_label(login.map(|l| l.state)).to_string(),
        callsign: cfg.callsign.clone(),
        error: setup_error
            .map(str::to_string)
            .or_else(|| login.and_then(|l| l.error.clone())),
        peers: peers.iter().map(peer_row).collect(),
    }
}

fn refuse(message: String) -> NodeError {
    NodeError { message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EchoLinkCfg {
        EchoLinkCfg {
            callsign: "KX7ABC-L".into(),
            servers: vec!["127.0.0.1:5200".into()],
            location: "Denver".into(),
        }
    }

    fn login(state: LoginState, error: Option<&str>) -> PresenceSnapshot {
        PresenceSnapshot {
            state,
            callsign: "KX7ABC-L".into(),
            error: error.map(str::to_string),
            retrying: false,
            stations_age: None,
        }
    }

    fn station(call: &str, node: u32) -> EchoLinkStationInfo {
        EchoLinkStationInfo {
            call: call.into(),
            description: String::new(),
            node,
            kind: "conference",
            status: "online",
        }
    }

    #[test]
    fn the_preflight_refuses_in_its_fixed_order() {
        let up = login(LoginState::LoggedIn, None);
        let msg = |r: Result<String, NodeError>| r.unwrap_err().message;
        assert_eq!(
            msg(dial_preflight(
                None,
                None,
                Some(&up),
                Some("9999"),
                |_| true,
                || panic!("no refresh before the list is read")
            )),
            ECHOLINK_NOT_ENABLED
        );
        for bad in [
            None,
            Some(""),
            Some("   "),
            Some("W1 AW"),
            Some(&"9".repeat(33)[..]),
        ] {
            assert_eq!(
                msg(dial_preflight(
                    Some(&cfg()),
                    None,
                    Some(&up),
                    bad,
                    |_| true,
                    || {}
                )),
                ECHOLINK_NEEDS_TARGET,
                "{bad:?}"
            );
        }
        let mut refreshed = 0;
        assert_eq!(
            msg(dial_preflight(
                Some(&cfg()),
                None,
                Some(&up),
                Some("9999"),
                |_| false,
                || refreshed += 1
            )),
            "Not in the station list yet — refreshing…"
        );
        assert_eq!(refreshed, 1, "a miss asks for a fresh list");
        assert_eq!(
            dial_preflight(
                Some(&cfg()),
                None,
                Some(&up),
                Some(" 9999 "),
                |t| t == "9999",
                || refreshed += 1
            )
            .unwrap(),
            "9999"
        );
        assert_eq!(refreshed, 1, "a hit asks for nothing");
    }

    #[test]
    fn not_logged_in_names_the_reason() {
        let pw = no_password("KX7ABC-L");
        assert_eq!(pw, "no password: set the echolink:KX7ABC-L secret");
        let cases = [
            (
                Some(pw.as_str()),
                None,
                format!("EchoLink isn't logged in yet: {pw}"),
            ),
            (
                None,
                None,
                "EchoLink isn't logged in yet: logged out".to_string(),
            ),
            (
                None,
                Some(login(LoginState::LoggingIn, None)),
                "EchoLink isn't logged in yet: still logging in".to_string(),
            ),
            (
                None,
                Some(login(
                    LoginState::Failed,
                    Some("EchoLink said the password is wrong"),
                )),
                "EchoLink isn't logged in yet: EchoLink said the password is wrong".to_string(),
            ),
        ];
        for (setup, l, want) in cases {
            let got = dial_preflight(
                Some(&cfg()),
                setup,
                l.as_ref(),
                Some("9999"),
                |_| true,
                || {},
            )
            .unwrap_err()
            .message;
            assert_eq!(got, want);
        }
    }

    #[test]
    fn listing_matches_a_node_number_or_a_callsign_case_blind() {
        let list = [station("*ECHOTEST*", 9999), station("W1AW-R", 1234)];
        assert!(is_listed(&list, "9999"));
        assert!(is_listed(&list, "w1aw-r"));
        assert!(!is_listed(&list, "1235"));
        assert!(!is_listed(&list, "W1AW"));
    }

    #[test]
    fn the_card_carries_the_login_and_the_setup_error_first() {
        let peers = [EchoLinkPeerStatus {
            call: "*ECHOTEST*".into(),
            node: 9999,
            link: CallLink::Connected,
            talking: true,
            up_secs: Some(12),
            notice: Some("Stopped sending the net to *ECHOTEST*.".into()),
        }];
        let card = node_status(
            &cfg(),
            None,
            Some(&login(LoginState::LoggedIn, None)),
            &peers,
        );
        assert_eq!(
            card,
            EchoLinkNodeStatus {
                login: "logged_in".into(),
                callsign: "KX7ABC-L".into(),
                error: None,
                peers: vec![EchoLinkPeerRow {
                    call: "*ECHOTEST*".into(),
                    node: 9999,
                    state: "connected".into(),
                    talking: true,
                    up_secs: Some(12),
                    notice: Some("Stopped sending the net to *ECHOTEST*.".into()),
                }],
            }
        );
        let pw = no_password("KX7ABC-L");
        let card = node_status(&cfg(), Some(&pw), None, &[]);
        assert_eq!(card.login, "logged_out");
        assert_eq!(card.error.as_deref(), Some(pw.as_str()));
        let failed = login(LoginState::Failed, Some("no server answered"));
        let card = node_status(&cfg(), None, Some(&failed), &[]);
        assert_eq!(card.login, "failed");
        assert_eq!(card.error.as_deref(), Some("no server answered"));
    }
}
