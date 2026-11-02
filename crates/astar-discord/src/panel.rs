// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The PTT panel: what it says, and how often it is allowed to say it.
//!
//! The panel is one bot message in the bridged voice channel's own text
//! chat, carrying a PTT button and a Status button. The Commander redraws
//! it on every change of mic holder by calling `GatewayOps::panel`, which
//! happens on the node's 50 ms pump thread — so this file exists to make
//! sure a burst of holder changes becomes at most one Discord edit in
//! flight at a time, and exactly one more afterwards carrying the newest
//! state.
//!
//! Discord rate-limits message edits at roughly five per five seconds per
//! channel. A handover storm (`/mic`, another `/mic`, `/unmic`, a
//! time-out) can produce four holder changes in a second, and an edit per
//! change would spend the whole bucket and then queue behind serenity's
//! ratelimiter — leaving the panel showing something that stopped being
//! true seconds ago. Coalescing fixes both halves: fewer calls, and the
//! one call that does go out carries the LATEST state rather than the
//! oldest unsent one.
//!
//! Nothing in this file names serenity, so all of it is tested under
//! default features.

// Under default features nothing in this crate CALLS any of this: the
// serenity layer that does is behind the `discord` feature. It is compiled
// and tested anyway, which is the point — these rules must hold in a build
// with no Discord, no tokio and no network in it at all.
#![cfg_attr(not(feature = "discord"), allow(dead_code))]

/// What the panel should look like for a given mic holder.
///
/// Plain data so the wording and the button's urgency can be tested
/// without building a Discord message; the serenity layer maps
/// [`PanelView::ptt_danger`] onto a button style and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelView {
    /// The message body.
    pub content: String,
    /// The PTT button's label.
    pub ptt_label: &'static str,
    /// Whether the PTT button is the loud one. `true` exactly while
    /// somebody holds the mic.
    pub ptt_danger: bool,
}

/// The Status button's label. Always present, always the quiet style.
pub(crate) const STATUS_LABEL: &str = "Status";

/// Render the panel for a mic holder, or for nobody.
///
/// The holder's display name is interpolated as-is. It cannot ping
/// anybody: every message this bot posts carries an empty
/// `CreateAllowedMentions`, so even a display name of `@everyone` renders
/// as text.
#[must_use]
pub(crate) fn panel_view(talking: Option<&str>) -> PanelView {
    match talking {
        None => PanelView {
            content: "🎙 Push to talk — approved users can key the mic here.".to_string(),
            ptt_label: "🎙 PTT",
            ptt_danger: false,
        },
        Some(name) => PanelView {
            content: format!("🔴 {name} is talking"),
            ptt_label: "🔴 Unkey / take over",
            ptt_danger: true,
        },
    }
}

/// One panel redraw worth performing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelAction {
    /// Make the panel show this holder (or nobody).
    Render(Option<String>),
}

/// Either nothing, or one panel state.
///
/// A plain `Option<Option<String>>` would say the same thing — the outer
/// layer being "is there a state at all?" and the inner "does anybody hold
/// the mic?" — but at the cost of a reader having to work out which is
/// which every time. Two named variants and one named payload cost
/// nothing and say it once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Slot {
    /// The panel has never been rendered, or no redraw is in flight.
    #[default]
    Empty,
    /// One panel state: the mic holder's display name, or `None` for
    /// nobody.
    Holding(Option<String>),
}

impl Slot {
    fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Whether this slot holds exactly the state `want` describes. An
    /// empty slot matches nothing, which is what makes the very first
    /// redraw unconditional.
    fn matches(&self, want: Option<&str>) -> bool {
        matches!(self, Self::Holding(held) if held.as_deref() == want)
    }
}

/// The coalescing rule: what the panel shows, what it should show, and
/// whether a redraw is already in flight.
///
/// The contract is two calls. [`Self::want`] is made by the command loop
/// for every `panel` the Commander issues and answers "start a redraw?";
/// [`Self::done`] is made by the redraw task when its REST call returns
/// and answers "and another one?". Between them at most one redraw exists
/// at a time, and no redraw is ever started for a state that is already on
/// screen.
#[derive(Debug, Default)]
pub(crate) struct PanelState {
    /// What Discord is believed to be showing.
    shown: Slot,
    /// The newest state asked for.
    desired: Option<String>,
    /// The state the in-flight redraw is carrying, if there is one.
    rendering: Slot,
}

impl PanelState {
    /// Record a wanted state, and start a redraw if one is due.
    pub(crate) fn want(&mut self, talking: Option<String>) -> Option<PanelAction> {
        self.desired = talking;
        if !self.rendering.is_empty() {
            // Something is already in flight. It will ask again on the way
            // out, and it will ask with whatever `desired` says THEN — so
            // an intermediate state nobody ever saw is simply skipped.
            return None;
        }
        self.start()
    }

    /// Finish the in-flight redraw, and start the follow-up if the wanted
    /// state moved on while it was out.
    ///
    /// **A failed redraw counts as done.** The state it carried is
    /// recorded as shown even when Discord refused it, because the
    /// alternative is retrying the same call against the same missing
    /// permission forever. A panel edit is a status line: logged and
    /// dropped (see `post_status_text`). The next real holder change tries
    /// again.
    pub(crate) fn done(&mut self) -> Option<PanelAction> {
        let rendered = std::mem::take(&mut self.rendering);
        if !rendered.is_empty() {
            self.shown = rendered;
        }
        self.start()
    }

    /// Give up the in-flight redraw WITHOUT recording what it carried as
    /// shown, so the next [`Self::done`] or [`Self::want`] renders it
    /// again.
    ///
    /// Two callers, one mechanism:
    ///
    /// * **A redraw that never finished.** `done` is the only thing that
    ///   clears `rendering`, and it is called by the redraw task when its
    ///   REST call returns. A panic inside that task is contained by tokio
    ///   in its `JoinHandle` and never unwinds into anybody — so without
    ///   this, `rendering` stays occupied forever and every later `want`
    ///   answers `None`: the panel silently stops updating for the life of
    ///   the process.
    /// * **A panel somebody deleted.** An edit that comes back 404 means
    ///   the message is gone. Recording its state as shown would leave the
    ///   panel missing until the next holder change, which on an idle node
    ///   is a long time with no button; abandoning it instead makes the
    ///   follow-up pass render again — and, since the message id has been
    ///   forgotten by then, that pass POSTS a fresh panel rather than
    ///   editing a corpse. Exactly one retry: a post does not 404, so the
    ///   second pass records itself as shown either way.
    pub(crate) fn abandon(&mut self) {
        self.rendering = Slot::Empty;
    }

    fn start(&mut self) -> Option<PanelAction> {
        if self.shown.matches(self.desired.as_deref()) {
            return None;
        }
        self.rendering = Slot::Holding(self.desired.clone());
        Some(PanelAction::Render(self.desired.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::{PanelAction, PanelState, panel_view};

    fn render(name: Option<&str>) -> PanelAction {
        PanelAction::Render(name.map(ToString::to_string))
    }

    #[test]
    fn an_idle_panel_invites_and_a_keyed_one_names_the_holder() {
        let idle = panel_view(None);
        assert_eq!(
            idle.content,
            "🎙 Push to talk — approved users can key the mic here."
        );
        assert_eq!(idle.ptt_label, "🎙 PTT");
        assert!(!idle.ptt_danger);

        let keyed = panel_view(Some("kd0xyz"));
        assert_eq!(keyed.content, "🔴 kd0xyz is talking");
        assert_eq!(keyed.ptt_label, "🔴 Unkey / take over");
        assert!(keyed.ptt_danger);
    }

    #[test]
    fn the_first_want_always_renders() {
        // Nothing has been posted, so even "nobody has the mic" is news.
        let mut p = PanelState::default();
        assert_eq!(p.want(None), Some(render(None)));
    }

    #[test]
    fn a_want_for_what_is_already_shown_does_nothing() {
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        assert_eq!(p.done(), None);
        assert_eq!(
            p.want(Some("a".into())),
            None,
            "the panel already says this"
        );
    }

    #[test]
    fn two_wants_during_one_redraw_produce_exactly_one_follow_up_with_the_last() {
        // The whole reason this type exists: three holder changes inside
        // one REST round trip must cost two edits, not three, and the
        // second must carry the NEWEST holder.
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        assert_eq!(p.want(Some("b".into())), None, "one redraw at a time");
        assert_eq!(p.want(Some("c".into())), None, "one redraw at a time");

        assert_eq!(
            p.done(),
            Some(render(Some("c"))),
            "the follow-up carries the last"
        );
        assert_eq!(p.done(), None, "and there is nothing left to do");
    }

    #[test]
    fn a_want_that_returns_to_what_is_on_screen_during_a_redraw_costs_nothing_more() {
        // a → b → a while the redraw for `a` is out: when it lands, the
        // screen already says what is wanted, so no second call is made.
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        assert_eq!(p.want(Some("b".into())), None);
        assert_eq!(p.want(Some("a".into())), None);
        assert_eq!(p.done(), None, "the redraw already carried `a`");
    }

    #[test]
    fn an_abandoned_redraw_is_rendered_again_rather_than_counted_as_shown() {
        // The 404 case: the edit failed because somebody deleted the
        // panel, so what it carried is NOT on screen. Recording it as shown
        // — which is what `done` does for every other failure, on purpose —
        // would leave the channel with no panel until the next holder
        // change.
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        p.abandon();
        assert_eq!(
            p.done(),
            Some(render(Some("a"))),
            "the state it carried never reached the screen, so it is owed again"
        );
        // And the retry, having landed, is recorded normally: exactly one.
        assert_eq!(p.done(), None);
    }

    #[test]
    fn a_redraw_that_never_finished_does_not_wedge_the_panel() {
        // A panic inside the redraw task is contained by tokio and never
        // reaches `done`. Without the abandon, `rendering` stays occupied
        // and every later `want` answers `None` — the panel stops updating
        // for the life of the process, silently.
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        assert_eq!(p.want(Some("b".into())), None, "one redraw at a time");
        p.abandon();
        assert_eq!(
            p.want(Some("b".into())),
            Some(render(Some("b"))),
            "the panel must be drawable again after a redraw that never returned"
        );
    }

    #[test]
    fn done_without_a_redraw_in_flight_is_harmless() {
        // Not expected, but `done` must not invent work: the panel has
        // never been rendered and nothing is wanted beyond that.
        let mut p = PanelState::default();
        assert_eq!(p.want(None), Some(render(None)));
        assert_eq!(p.done(), None);
        assert_eq!(p.done(), None);
    }

    #[test]
    fn the_mic_going_idle_is_a_redraw_like_any_other() {
        let mut p = PanelState::default();
        assert_eq!(p.want(Some("a".into())), Some(render(Some("a"))));
        assert_eq!(p.done(), None);
        assert_eq!(p.want(None), Some(render(None)));
        assert_eq!(p.done(), None);
    }
}
