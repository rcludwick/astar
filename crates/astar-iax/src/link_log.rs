// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The link log: one `info` line per lifecycle event of every IAX2 call leg,
//! on the target `astar_iax::link`, so a link that comes up and drops leaves
//! a record of who ended it, why, and when.
//!
//! Each call's run-loop (outbound in `runtime.rs`, inbound in `listener.rs`)
//! owns one [`LinkLog`] and feeds it the same `AppEvent`s it turns into
//! `CallEvent`s, plus the HANGUP / REJECT frames it puts on the wire itself.
//! Lines:
//!
//! - `link starting` — the call arrived or was dialled: direction, the far
//!   address, the node number it claims (inbound) or that we dialled.
//! - `link codec settled` — the voice codec both sides agreed on.
//! - `link up` — answered; `setup_ms` is how long it took to get there.
//! - `link ended` — `ended_by` is `us`, `them` or `timeout`, `cause` is the
//!   CAUSE text from the HANGUP / REJECT (or what the engine knows about the
//!   end), and `duration_s` counts from `link up` (or from `link starting`
//!   for a call that never came up).
//! - a `warn` `short link` line when a link that came up ends under
//!   [`SHORT_LINK`] later — the symptom of a node dropping a link.
//!
//! Never a secret, password, challenge or call token: the fields are the
//! direction, the far address, the node number, our local call number, the
//! codec name, the cause text and times. Nothing is logged per audio frame.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use astar_iax_core::session::{AppEvent, FailReason};
use astar_iax_core::{IaxCommand, Ies, OwnedFullFrame, Subclass, VoiceFormat};

/// A link that ends sooner than this after coming up also logs a `warn`.
pub(crate) const SHORT_LINK: Duration = Duration::from_secs(10);

const TARGET: &str = "astar_iax::link";

/// Which way the call was set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkDirection {
    /// The far end called us.
    Inbound,
    /// We dialled the far end.
    Outbound,
}

impl LinkDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "in",
            Self::Outbound => "out",
        }
    }
}

/// Who ended a link, as the log spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndedBy {
    Us,
    Them,
    Timeout,
}

impl EndedBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Them => "them",
            Self::Timeout => "timeout",
        }
    }
}

/// One call leg's story in the log. Dropping it before the call's end was
/// seen still writes the `link ended` line, so a run-loop that stops for any
/// reason (shutdown, socket error) leaves a record.
pub(crate) struct LinkLog {
    direction: LinkDirection,
    peer: SocketAddr,
    node: String,
    call: u16,
    started: Instant,
    /// Whether `link starting` has been written: at once for a call we dial
    /// or one that needs no call token, otherwise once the caller echoes the
    /// token. Until then the far address may be spoofed, so nothing is
    /// written at `info`.
    admitted: bool,
    up_at: Option<Instant>,
    /// Set once we put our own HANGUP or REJECT on the wire, which is when
    /// we, not the far end, ended the call.
    local_end: bool,
    /// The cause text that request carried, if any.
    local_cause: Option<String>,
    ended: bool,
}

impl LinkLog {
    /// Open the log for one call leg. `node` is the node number the far end
    /// claims (inbound) or the one we dialled (outbound); empty reads as `?`.
    /// `admitted` writes `link starting` now; otherwise it waits for
    /// [`Self::admit`], and a leg that ends first writes one `debug` line.
    pub(crate) fn start(
        direction: LinkDirection,
        peer: SocketAddr,
        node: &str,
        call: u16,
        admitted: bool,
    ) -> Self {
        let node = if node.is_empty() {
            "?".to_string()
        } else {
            clean_word(node)
        };
        let mut log = Self {
            direction,
            peer,
            node,
            call,
            started: Instant::now(),
            admitted: false,
            up_at: None,
            local_end: false,
            local_cause: None,
            ended: false,
        };
        if admitted {
            log.admit();
        }
        log
    }

    /// The caller is past the call-token round (its source address is
    /// real): write `link starting`. Idempotent.
    pub(crate) fn admit(&mut self) {
        if self.admitted || self.ended {
            return;
        }
        self.admitted = true;
        self.started = Instant::now();
        tracing::info!(
            target: TARGET,
            direction = %self.direction.as_str(),
            peer = %self.peer,
            node = %self.node,
            call = self.call,
            "link starting"
        );
    }

    /// Feed every `AppEvent` the run-loop dispatches. Only `Connected` and
    /// `Disconnected` are looked at; everything else is ignored at the cost
    /// of one match.
    pub(crate) fn on_app_event(&mut self, ev: &AppEvent, negotiated: VoiceFormat) {
        match ev {
            AppEvent::Connected { .. } => self.on_up(negotiated),
            AppEvent::Disconnected { reason } => self.on_end(Some(reason)),
            _ => {}
        }
    }

    /// Feed every reliable frame the run-loop sends. A HANGUP or REJECT of
    /// our own means we are the side ending the call; its CAUSE IE is the
    /// cause we gave.
    pub(crate) fn on_send(&mut self, frame: &OwnedFullFrame) {
        if self.local_end
            || !matches!(
                frame.subclass,
                Subclass::Iax(IaxCommand::Hangup | IaxCommand::Reject)
            )
        {
            return;
        }
        self.local_end = true;
        self.local_cause = Ies::parse(frame.ie_bytes())
            .ok()
            .and_then(|ies| ies.cause.map(clean));
    }

    /// We are starting the teardown with `!!DISCONNECT!!` and holding our
    /// HANGUP behind it (`link_keying::DISCONNECT_GRACE`). The end is ours
    /// from now on, even when the node's HANGUP answers before ours goes:
    /// only a HANGUP that comes while we had sent nothing toward ending the
    /// link is theirs.
    pub(crate) fn on_local_hangup(&mut self, cause: Option<&str>) {
        if self.local_end {
            return;
        }
        self.local_end = true;
        self.local_cause = cause.map(clean);
    }

    fn on_up(&mut self, negotiated: VoiceFormat) {
        if self.up_at.is_some() || self.ended {
            return;
        }
        let now = Instant::now();
        self.up_at = Some(now);
        tracing::info!(
            target: TARGET,
            direction = %self.direction.as_str(),
            peer = %self.peer,
            node = %self.node,
            call = self.call,
            codec = %codec_name(negotiated),
            "link codec settled"
        );
        tracing::info!(
            target: TARGET,
            direction = %self.direction.as_str(),
            peer = %self.peer,
            node = %self.node,
            call = self.call,
            setup_ms = u64::try_from(now.duration_since(self.started).as_millis())
                .unwrap_or(u64::MAX),
            "link up"
        );
    }

    fn on_end(&mut self, reason: Option<&FailReason>) {
        if self.ended {
            return;
        }
        self.ended = true;
        let (ended_by, cause) = classify(reason, self.local_end, self.local_cause.as_deref());
        let now = Instant::now();
        let lasted = now.duration_since(self.up_at.unwrap_or(self.started));
        let duration_s = format!("{:.1}", lasted.as_secs_f64());
        if !self.admitted {
            // Never past the call-token round: the source may be spoofed, so
            // one line, at debug, and no warning.
            tracing::debug!(
                target: TARGET,
                direction = %self.direction.as_str(),
                peer = %self.peer,
                node = %self.node,
                call = self.call,
                ended_by = %ended_by.as_str(),
                cause = cause.as_str(),
                duration_s = %duration_s,
                "call ended before the call-token round"
            );
            return;
        }
        tracing::info!(
            target: TARGET,
            direction = %self.direction.as_str(),
            peer = %self.peer,
            node = %self.node,
            call = self.call,
            ended_by = %ended_by.as_str(),
            cause = cause.as_str(),
            duration_s = %duration_s,
            was_up = self.up_at.is_some(),
            "link ended"
        );
        if is_short(self.up_at.is_some(), lasted) {
            tracing::warn!(
                target: TARGET,
                direction = %self.direction.as_str(),
                peer = %self.peer,
                node = %self.node,
                call = self.call,
                ended_by = %ended_by.as_str(),
                duration_s = %duration_s,
                "short link: it ended less than 10 s after coming up"
            );
        }
    }
}

impl Drop for LinkLog {
    fn drop(&mut self) {
        self.on_end(None);
    }
}

/// The longest peer-supplied text the log keeps, in characters.
pub(crate) const MAX_PEER_TEXT: usize = 64;

/// Make text the far end chose safe to log: every control character (a
/// newline would forge a log line, ESC repaints a terminal) becomes `?`, and
/// the text stops at [`MAX_PEER_TEXT`] characters. A NEW's calling number or
/// a HANGUP's cause can be up to 255 bytes of anything.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .take(MAX_PEER_TEXT)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// [`clean`] for a field logged bare (`node=`): whitespace becomes `?` too,
/// so a caller cannot append fake `key=value` pairs to the line.
pub(crate) fn clean_word(text: &str) -> String {
    clean(text)
        .chars()
        .map(|c| if c.is_whitespace() { '?' } else { c })
        .collect()
}

/// Whether a link's end earns the `short link` warning: it came up, and it
/// lasted less than [`SHORT_LINK`].
pub(crate) fn is_short(was_up: bool, lasted: Duration) -> bool {
    was_up && lasted < SHORT_LINK
}

/// Who ended the call and the cause text to log.
///
/// Our own end request comes first: once we hung up or rejected, the end is
/// ours whatever the FSM reports next. Otherwise the reason decides: a
/// time-out is `timeout`, a local abort or a protocol fault is `us`, and a
/// HANGUP, REJECT or INVAL from the far end is `them`. With no reason at all
/// (the run-loop stopped before any end event) it is `us`.
pub(crate) fn classify(
    reason: Option<&FailReason>,
    local_end: bool,
    local_cause: Option<&str>,
) -> (EndedBy, String) {
    if local_end {
        let text = clean(
            local_cause
                .filter(|c| !c.is_empty())
                .unwrap_or("hung up locally"),
        );
        return (EndedBy::Us, text);
    }
    match reason {
        None => (EndedBy::Us, "call stopped".to_string()),
        Some(r @ FailReason::Timeout { .. }) => (EndedBy::Timeout, r.to_string()),
        Some(r @ (FailReason::Aborted | FailReason::InvalidTransition { .. })) => {
            (EndedBy::Us, r.to_string())
        }
        Some(
            FailReason::RemoteHangup { cause: Some(c) } | FailReason::Rejected { cause: Some(c) },
        ) if !c.is_empty() => (EndedBy::Them, clean(c)),
        Some(r) => (EndedBy::Them, r.to_string()),
    }
}

/// The codec's Asterisk name, the one a node operator reads in `iax2 show
/// channels`.
pub(crate) fn codec_name(f: VoiceFormat) -> &'static str {
    match f {
        VoiceFormat::G723_1 => "g723",
        VoiceFormat::Gsm => "gsm",
        VoiceFormat::G711U => "ulaw",
        VoiceFormat::G711A => "alaw",
        VoiceFormat::G726 | VoiceFormat::G726Aal2 => "g726",
        VoiceFormat::Adpcm => "adpcm",
        VoiceFormat::Slin => "slin",
        VoiceFormat::Lpc10 => "lpc10",
        VoiceFormat::G729 => "g729",
        VoiceFormat::Speex => "speex",
        VoiceFormat::Ilbc => "ilbc",
        VoiceFormat::G722 => "g722",
        VoiceFormat::Siren7 => "siren7",
        VoiceFormat::Siren14 => "siren14",
        VoiceFormat::Slin16 => "slin16",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_end_request_wins_over_the_reason() {
        let (by, cause) = classify(Some(&FailReason::Aborted), true, Some("bye"));
        assert_eq!((by, cause.as_str()), (EndedBy::Us, "bye"));
        let (by, cause) = classify(None, true, None);
        assert_eq!((by, cause.as_str()), (EndedBy::Us, "hung up locally"));
    }

    #[test]
    fn the_reason_decides_when_we_asked_for_nothing() {
        let (by, cause) = classify(
            Some(&FailReason::RemoteHangup {
                cause: Some("Normal Clearing".into()),
            }),
            false,
            None,
        );
        assert_eq!((by, cause.as_str()), (EndedBy::Them, "Normal Clearing"));
        let (by, cause) = classify(
            Some(&FailReason::Timeout { in_state: "Active" }),
            false,
            None,
        );
        assert_eq!(by, EndedBy::Timeout);
        assert_eq!(cause, "peer timed out (in Active)");
        assert_eq!(
            classify(Some(&FailReason::PeerInval), false, None).0,
            EndedBy::Them
        );
        assert_eq!(
            classify(Some(&FailReason::Rejected { cause: None }), false, None),
            (EndedBy::Them, "rejected".to_string())
        );
        assert_eq!(
            classify(Some(&FailReason::Aborted), false, None).0,
            EndedBy::Us
        );
        assert_eq!(
            classify(None, false, None),
            (EndedBy::Us, "call stopped".into())
        );
    }

    #[test]
    fn only_a_link_that_came_up_and_lasted_under_ten_seconds_is_short() {
        assert!(is_short(true, Duration::from_millis(2_400)));
        assert!(is_short(true, Duration::from_millis(9_999)));
        assert!(!is_short(true, Duration::from_secs(10)));
        assert!(!is_short(true, Duration::from_secs(3_600)));
        assert!(!is_short(false, Duration::from_millis(100)), "never up");
    }

    #[test]
    fn peer_text_loses_control_characters_and_stops_at_64() {
        assert_eq!(clean("Normal Clearing"), "Normal Clearing");
        assert_eq!(clean("a\nb\u{1b}[2Jc\0"), "a?b?[2Jc?");
        assert_eq!(clean(&"x".repeat(300)).chars().count(), 64);
        assert_eq!(clean_word("1999\nWARN fake"), "1999?WARN?fake");
        assert_eq!(clean_word("1999"), "1999");
    }

    #[test]
    fn codecs_read_as_asterisk_names() {
        assert_eq!(codec_name(VoiceFormat::G711U), "ulaw");
        assert_eq!(codec_name(VoiceFormat::Slin16), "slin16");
        assert_eq!(codec_name(VoiceFormat::Gsm), "gsm");
    }
}
